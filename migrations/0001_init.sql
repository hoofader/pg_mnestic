-- SPDX-License-Identifier: Apache-2.0

-- sqlx runs each migration inside its own transaction, so there is no explicit
-- BEGIN/COMMIT here (an inner BEGIN would commit sqlx's surrounding transaction early).

-- Extensions (LLD §1).
CREATE EXTENSION IF NOT EXISTS vector;       -- pgvector: embeddings + HNSW (+ halfvec)
CREATE EXTENSION IF NOT EXISTS pgcrypto;     -- gen_random_uuid(), encryption
CREATE EXTENSION IF NOT EXISTS btree_gist;   -- required for EXCLUDE on (text =, range &&)

-- Tenants (LLD §2.1).
CREATE TABLE mnestic_tenant (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  external_id text NOT NULL UNIQUE,           -- caller's own tenant key (resolved from the API key)
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- Sources: raw, append-only audit trail (LLD §2.2).
CREATE TABLE mnestic_source (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id       text NOT NULL,               -- who/what this is about
  container_tags text[] NOT NULL DEFAULT '{}',
  kind           text NOT NULL                -- 'conversation' | 'document' | 'connector'
                 CHECK (kind IN ('conversation','document','connector')),
  raw            jsonb,                        -- cleartext payload (when not sensitive)
  raw_enc        bytea,                        -- envelope-encrypted payload (when sensitive)
  custom_id      text,                         -- caller idempotency key (wire `customId`)
  needs_extraction boolean NOT NULL DEFAULT false,  -- set when extraction is deferred (async mode)
  ingested_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, custom_id)
);
CREATE INDEX ON mnestic_source (tenant_id, actor_id, ingested_at DESC);
CREATE INDEX ON mnestic_source (tenant_id) WHERE needs_extraction;

-- Documents & chunks: RAG side (LLD §2.3). actor_id is denormalized onto both so
-- document search scopes by actor the same way memory recall does, without a join back
-- to the source; a supermemory containerTag resolves to an actor as the primary scope.
CREATE TABLE mnestic_document (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id       text NOT NULL,
  source_id      uuid REFERENCES mnestic_source(id),
  container_tags text[] NOT NULL DEFAULT '{}',
  title          text,
  uri            text,
  metadata       jsonb NOT NULL DEFAULT '{}',
  created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON mnestic_document (tenant_id, actor_id);

CREATE TABLE mnestic_chunk (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id       text NOT NULL,
  document_id    uuid NOT NULL REFERENCES mnestic_document(id) ON DELETE CASCADE,
  container_tags text[] NOT NULL DEFAULT '{}',
  ord            int  NOT NULL,
  content        text NOT NULL,
  content_tsv    tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
  embedding      halfvec(1536),               -- :dim templated at install
  created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON mnestic_chunk USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX ON mnestic_chunk USING gin (content_tsv);
CREATE INDEX ON mnestic_chunk (tenant_id, document_id, ord);
CREATE INDEX ON mnestic_chunk (tenant_id, actor_id);
CREATE INDEX ON mnestic_chunk USING gin (container_tags);

-- Memories: content-primary hybrid, the core table (LLD §2.4).
CREATE TABLE mnestic_memory (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id       text NOT NULL,
  container_tags text[] NOT NULL DEFAULT '{}',

  -- canonical memory: entity-centric natural language (maps to wire `content`)
  content        text,                          -- cleartext (when not sensitive)
  content_enc    bytea,                          -- envelope-encrypted (when sensitive)

  -- optional structured view, populated only when extraction yields a clean triple
  subject        text,
  attribute      text,
  value          text,
  single_valued  boolean NOT NULL DEFAULT false, -- gates the EXCLUDE below

  confidence     real NOT NULL DEFAULT 0.5 CHECK (confidence >= 0 AND confidence <= 1),
  is_static      boolean NOT NULL DEFAULT false, -- durable trait (wire `isStatic`)
  mem_type       text NOT NULL DEFAULT 'fact'    -- 'fact' | 'preference' | 'episode'
                 CHECK (mem_type IN ('fact','preference','episode')),
  metadata       jsonb NOT NULL DEFAULT '{}',

  -- retrieval signals
  embedding      halfvec(1536),                  -- :dim templated at install
  content_tsv    tsvector,                       -- maintained by engine from rendered text

  source_id      uuid REFERENCES mnestic_source(id),
  custom_id      text,                            -- caller dedup/idempotency (wire `customId`)

  -- bitemporal model (both axes are ranges so out-of-order arrival is correct)
  valid_time     tstzrange NOT NULL DEFAULT tstzrange(now(), NULL),  -- truth in the world
  recorded_time  tstzrange NOT NULL DEFAULT tstzrange(now(), NULL),  -- when the system held this belief
  document_date  timestamptz,                     -- wire temporalContext.documentDate
  event_date     timestamptz,                     -- wire temporalContext.eventDate

  -- supersession & lifecycle. The source/supersedes FKs are NO ACTION (not CASCADE):
  -- this is an append-only audit trail, so deleting a referenced row should fail
  -- rather than erase history.
  supersedes_id  uuid REFERENCES mnestic_memory(id),
  is_latest      boolean NOT NULL DEFAULT true,
  forget_after   timestamptz,                     -- wire `forgetAfter`
  forget_reason  text,                            -- wire `forgetReason`
  status         text NOT NULL DEFAULT 'active'
                 CHECK (status IN ('active','superseded','expired','forgotten')),

  created_at     timestamptz NOT NULL DEFAULT now(),

  -- single-valued attributes only: no two active facts over overlapping valid time.
  -- Multi-valued facts (languages, skills, interests) set single_valued=false and coexist.
  -- The key omits `value` on purpose: any two active single-valued rows for the same
  -- (subject, attribute) conflict, contradiction or not. So the resolve path must dedup an
  -- identical value BEFORE insert (LLD §5.2); an un-deduped repeat raises 23P01, not a no-op.
  CONSTRAINT no_overlap_single_valued EXCLUDE USING gist (
    tenant_id WITH =, actor_id WITH =, subject WITH =, attribute WITH =, valid_time WITH &&
  ) WHERE (status = 'active' AND single_valued
           AND subject IS NOT NULL AND attribute IS NOT NULL),

  -- `single_valued` is meaningless without a triple, and a NULL triple would slip past the
  -- partial EXCLUDE above, so the flag and the triple must travel together.
  CONSTRAINT single_valued_needs_triple
    CHECK (NOT single_valued OR (subject IS NOT NULL AND attribute IS NOT NULL)),

  CONSTRAINT content_present CHECK (content IS NOT NULL OR content_enc IS NOT NULL),
  UNIQUE (tenant_id, custom_id)
);
CREATE INDEX ON mnestic_memory USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX ON mnestic_memory USING gin (content_tsv);
CREATE INDEX ON mnestic_memory (tenant_id, actor_id) WHERE is_latest AND status = 'active';
CREATE INDEX ON mnestic_memory (forget_after) WHERE status = 'active' AND forget_after IS NOT NULL;
CREATE INDEX ON mnestic_memory (tenant_id, actor_id, subject, attribute) WHERE single_valued AND is_latest;
-- Container scoping on the recall path is `container_tags @> $tags`. GIN stores nothing for
-- the common empty-array row, so this is near-free until containers are actually used.
CREATE INDEX ON mnestic_memory USING gin (container_tags) WHERE is_latest AND status = 'active';

-- Keep memory.content_tsv populated so the §5.4 lexical CTE never silently returns zero
-- rows. The engine may render a richer form (subject attribute: value) and set content_tsv
-- itself; when it leaves it NULL we derive it from content as the floor.
CREATE FUNCTION mnestic_memory_tsv() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.content_tsv IS NULL THEN
    NEW.content_tsv := to_tsvector('english', coalesce(NEW.content, ''));
  END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER mnestic_memory_tsv_trg
  BEFORE INSERT OR UPDATE ON mnestic_memory
  FOR EACH ROW EXECUTE FUNCTION mnestic_memory_tsv();

-- Profiles: precomputed, hot read path (LLD §2.5).
CREATE TABLE mnestic_profile (
  tenant_id    uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id     text NOT NULL,
  static_facts jsonb NOT NULL DEFAULT '[]',    -- durable, high-confidence (is_static or confidence>=θ)
  dynamic_ctx  jsonb NOT NULL DEFAULT '[]',    -- recent activity window
  refreshed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, actor_id)
);

-- Row-Level Security (LLD §3).
-- The engine sets `mnestic.tenant_id` per transaction via SET LOCAL; policies enforce
-- isolation regardless of application correctness. The `true` second arg to current_setting
-- avoids errors when the GUC was never defined. A pooled connection that ran SET LOCAL in a
-- prior tx can leave the GUC defined as '' afterward, so nullif maps both cases to NULL, which
-- matches no rows (fail-closed) instead of raising 22P02 on ''::uuid.
-- FORCE makes the table owner subject to RLS too, so tests do not silently pass as owner.
ALTER TABLE mnestic_memory   ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_memory   FORCE ROW LEVEL SECURITY;
ALTER TABLE mnestic_source   ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_source   FORCE ROW LEVEL SECURITY;
ALTER TABLE mnestic_document ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_document FORCE ROW LEVEL SECURITY;
ALTER TABLE mnestic_chunk    ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_chunk    FORCE ROW LEVEL SECURITY;
ALTER TABLE mnestic_profile  ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_profile  FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON mnestic_memory
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON mnestic_source
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON mnestic_document
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON mnestic_chunk
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);

CREATE POLICY tenant_isolation ON mnestic_profile
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);
