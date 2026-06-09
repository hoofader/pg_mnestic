# Mnestic: Low-Level Design

> **Document 3 of 4** · Status: Draft v0.2 · Date: 2026-06-07
> Companion documents: `01-high-level-plan.md`, `02-architecture.md`, `04-compatibility.md`

This document is implementation-facing. DDL, policies, and pseudocode here are a **design reference**, not final migrations. The vector dimension `1536` is templated at install (the embedding-provider decision in HLP §9); larger models and Matryoshka truncation are supported by changing one template value.

---

## 1. Extensions & prerequisites

```sql
CREATE EXTENSION IF NOT EXISTS vector;       -- pgvector: embeddings + HNSW (+ halfvec)
CREATE EXTENSION IF NOT EXISTS pgcrypto;     -- gen_random_uuid(), encryption
CREATE EXTENSION IF NOT EXISTS btree_gist;   -- required for EXCLUDE on (text =, range &&)
-- Optional, for true BM25 hybrid search (ParadeDB). Falls back to tsvector if absent.
-- CREATE EXTENSION IF NOT EXISTS pg_search;
```

Embeddings are stored as `halfvec(:dim)` by default (half the storage, negligible recall loss at these dimensions); deployments that need full precision template `vector(:dim)`. `:dim` defaults to 1536.

## 2. Schema

### 2.1 Tenants
```sql
CREATE TABLE mnestic_tenant (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  external_id text NOT NULL UNIQUE,           -- caller's own tenant key (resolved from the API key)
  created_at  timestamptz NOT NULL DEFAULT now()
);
```

### 2.2 Sources (raw, append-only audit trail)
```sql
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
```

### 2.3 Documents & chunks (RAG side)
```sql
CREATE TABLE mnestic_document (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
  source_id      uuid REFERENCES mnestic_source(id),
  container_tags text[] NOT NULL DEFAULT '{}',
  title          text,
  uri            text,
  metadata       jsonb NOT NULL DEFAULT '{}',
  created_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE mnestic_chunk (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  tenant_id      uuid NOT NULL REFERENCES mnestic_tenant(id),
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
```

### 2.4 Memories (content-primary hybrid, the core table)
```sql
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

  -- supersession & lifecycle
  supersedes_id  uuid REFERENCES mnestic_memory(id),
  is_latest      boolean NOT NULL DEFAULT true,
  forget_after   timestamptz,                     -- wire `forgetAfter`
  forget_reason  text,                            -- wire `forgetReason`
  status         text NOT NULL DEFAULT 'active'
                 CHECK (status IN ('active','superseded','expired','forgotten')),

  created_at     timestamptz NOT NULL DEFAULT now(),

  -- single-valued attributes only: no two active facts over overlapping valid time.
  -- Multi-valued facts (languages, skills, interests) set single_valued=false and coexist.
  CONSTRAINT no_overlap_single_valued EXCLUDE USING gist (
    tenant_id WITH =, actor_id WITH =, subject WITH =, attribute WITH =, valid_time WITH &&
  ) WHERE (status = 'active' AND single_valued
           AND subject IS NOT NULL AND attribute IS NOT NULL),

  CONSTRAINT content_present CHECK (content IS NOT NULL OR content_enc IS NOT NULL),
  UNIQUE (tenant_id, custom_id)
);
CREATE INDEX ON mnestic_memory USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX ON mnestic_memory USING gin (content_tsv);
CREATE INDEX ON mnestic_memory (tenant_id, actor_id) WHERE is_latest AND status = 'active';
CREATE INDEX ON mnestic_memory (forget_after) WHERE status = 'active' AND forget_after IS NOT NULL;
CREATE INDEX ON mnestic_memory (tenant_id, subject, attribute) WHERE single_valued AND is_latest;
```

Two design points worth stating plainly:

- **Content is canonical, structure is optional.** Real extracted memories are usually natural-language statements ("user is migrating from NYC to SF"), not clean triples. We store `content` always and the `(subject, attribute, value)` triple only when extraction produces a confident one. This is what makes the model both wire-compatible (maps to supermemory's `content`/`isStatic`/`forgetAfter`) and able to represent multi-valued facts.
- **The EXCLUDE is scoped, not global.** The original design forbade two active rows for any `(subject, attribute)`, which breaks multi-valued attributes. Here the constraint applies only when `single_valued` is true, so the database still refuses to hold two contradictory single-valued facts (location, employer, current city) while additive facts coexist. Supersession for everything else is an explicit `supersedes_id` + `is_latest` chain the engine maintains.

### 2.5 Profiles (precomputed, hot read path)
```sql
CREATE TABLE mnestic_profile (
  tenant_id    uuid NOT NULL REFERENCES mnestic_tenant(id),
  actor_id     text NOT NULL,
  static_facts jsonb NOT NULL DEFAULT '[]',    -- durable, high-confidence (is_static or confidence>=θ)
  dynamic_ctx  jsonb NOT NULL DEFAULT '[]',    -- recent activity window
  refreshed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, actor_id)
);
```

## 3. Row-Level Security

Applied to every tenant-scoped table. The engine sets `mnestic.tenant_id` per transaction via `SET LOCAL` from the resolved API key; policies enforce isolation regardless of application correctness.

```sql
ALTER TABLE mnestic_memory   ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_source   ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_document ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_chunk    ENABLE ROW LEVEL SECURITY;
ALTER TABLE mnestic_profile  ENABLE ROW LEVEL SECURITY;

-- Pattern, repeated per table:
CREATE POLICY tenant_isolation ON mnestic_memory
  USING      (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = nullif(current_setting('mnestic.tenant_id', true), '')::uuid);
```

Engine usage per request:
```sql
BEGIN;
SET LOCAL mnestic.tenant_id = '....';   -- the resolved tenant uuid
-- ... reads/writes scoped automatically ...
COMMIT;
```

> Run the application as a role **without** `BYPASSRLS` (a superuser bypasses RLS entirely, so isolation must be tested as a non-superuser). The `true` second arg to `current_setting` avoids an error when the GUC was never defined. The `nullif(..., '')` matters on pooled connections: once any prior transaction has run `SET LOCAL mnestic.tenant_id`, the GUC lingers as `''` after that transaction ends, and a bare `''::uuid` raises `22P02` instead of matching no rows. `nullif` maps both the never-defined and the emptied cases to `NULL`, which matches no rows (fail-closed).

## 4. Encryption design

- **What is encrypted:** sensitive `content` (memory) and `raw` (source) payloads, stored in `*_enc bytea`, cleartext columns left NULL.
- **What is not:** embeddings (needed for search), and the `subject`/`attribute` resolution keys (needed for supersession). This is a deliberate, documented trade-off; embeddings can leak information about source text. Lexical search over encrypted `content` is not possible, so an encrypted memory is recalled by vector and by its plaintext structured keys only.
- **Scheme:** envelope encryption. A per-tenant data encryption key (DEK) is wrapped by a key in an external KMS. The engine fetches/caches the DEK, encrypts before write, decrypts after read. Keys are never stored in Postgres.
- **In-DB fallback:** for low-sensitivity deployments without a KMS, `pgcrypto`'s `pgp_sym_encrypt`/`pgp_sym_decrypt` with a session-supplied key is acceptable but weaker (the key transits the DB session).

## 5. Pipeline algorithms

### 5.1 Extraction
A single LLM call converts raw text into candidate memories. The model returns only JSON, validated against this contract:

```json
{
  "memories": [
    {
      "content": "User lives in San Francisco.",
      "subject": "user",
      "attribute": "location",
      "value": "San Francisco",
      "single_valued": true,
      "mem_type": "fact",
      "confidence": 0.9,
      "is_static": false,
      "temporal": { "kind": "as_of", "timestamp": "2026-06-07T00:00:00Z" },
      "forget_after": null
    }
  ]
}
```

- `content` is required and canonical. `subject`/`attribute`/`value`/`single_valued` are optional; the engine fills them only when present and confident.
- `mem_type` ∈ `fact` | `preference` | `episode` (preferences strengthen with repetition; episodes decay unless significant).
- `temporal.kind` ∈ `as_of` | `range` | `none`; drives how `valid_time` is set. `forget_after` handles ephemeral facts ("has an exam tomorrow").
- The engine embeds `content` (or the rendered triple when present) for retrieval, and validates/repairs malformed JSON before proceeding.
- **Attribute resolution (Phase 1):** before resolution, normalize `attribute` against a small per-tenant ontology and by embedding similarity, so "location", "current city", and "lives in" collapse to one key. This is where supersession quality actually comes from; without it, contradictions never trigger. The naive single-call extractor is the Phase 0 floor, not the parity design.

### 5.2 Resolution & supersession
For each extracted candidate `c` (within the tenant transaction):

```text
matches = find_latest_memories(
    actor_id = c.actor_id,
    subject  = normalize(c.subject),
    attribute= normalize(c.attribute),
    semantic = c.embedding,           # vector match for the unstructured case
)

if exact duplicate (same normalized value, overlapping valid_time):
    bump confidence on existing; skip insert                      # dedup

elif c.single_valued and a latest match has a different value:
    # contradiction over a single-valued attribute -> supersede, order-correct
    let prior = the latest match
    # split valid_time by event order, not arrival order:
    if c.valid_from > lower(prior.valid_time):
        UPDATE prior SET valid_time = tstzrange(lower(prior.valid_time), c.valid_from)
    else:
        # late-arriving fact about an earlier period: close prior's lower bound instead
        UPDATE prior SET valid_time = tstzrange(c.valid_until, upper(prior.valid_time))
    UPDATE prior SET status='superseded', is_latest=false,
                     recorded_time = tstzrange(lower(prior.recorded_time), now());
    INSERT new memory (..., supersedes_id=prior.id, is_latest=true,
                       valid_time from c.temporal);

else:
    # additive (multi-valued) or brand-new: just record it
    INSERT new memory (..., is_latest=true, valid_time from c.temporal);

if c.forget_after is set:
    set forget_after on the new row.
```

A periodic (or lazy, at read time) sweep flips rows to `status='expired'` where `forget_after <= now()`. For single-valued attributes the `EXCLUDE` constraint is the backstop: any logic error that would create overlapping active facts fails loudly at write time. Out-of-order arrival is handled by splitting `valid_time` on event order rather than assuming the new fact is the most recent, which is the whole reason the model is bitemporal.

### 5.3 Profile maintenance (incremental)
After resolution for an actor, update only from the rows touched in this transaction:
- `static_facts`: latest, durable (`is_static` or `confidence >= θ_static`, non-ephemeral) facts, capped and ordered by confidence.
- `dynamic_ctx`: recent facts within a rolling window (last N items or last D days), ordered by `recorded_time` desc.
- Upsert `mnestic_profile`. A full rebuild from scratch is an out-of-band job, so the write path stays a bounded incremental update and the read path is a single indexed lookup (~50 ms target).

### 5.4 Hybrid recall + ranking
One query fuses semantic and lexical results via Reciprocal Rank Fusion (RRF), reranks, then applies a recency/confidence decay. The lexical CTE shown uses `tsvector` (the portable floor); on `pg_search` it becomes a BM25 query via the `@@@` operator and a BM25 index, which is a different operator and ranking, not a drop-in text swap.

```sql
WITH params AS (
  SELECT $1::halfvec AS qvec, $2::text AS qtext, 60 AS k   -- k = RRF constant
),
vec AS (
  SELECT m.id,
         row_number() OVER (ORDER BY m.embedding <=> p.qvec) AS rnk
  FROM mnestic_memory m, params p
  WHERE m.is_latest AND m.status = 'active'
    AND (m.forget_after IS NULL OR m.forget_after > now())
  ORDER BY m.embedding <=> p.qvec
  LIMIT 50
),
lex AS (   -- tsvector floor; replace with pg_search BM25 (@@@) when available
  SELECT m.id,
         row_number() OVER (
           ORDER BY ts_rank(m.content_tsv, plainto_tsquery('english', p.qtext)) DESC
         ) AS rnk
  FROM mnestic_memory m, params p
  WHERE m.is_latest AND m.status = 'active'
    AND m.content_tsv @@ plainto_tsquery('english', p.qtext)
  LIMIT 50
),
fused AS (
  SELECT id, SUM(1.0 / (k + rnk)) AS rrf
  FROM (SELECT id, rnk FROM vec UNION ALL SELECT id, rnk FROM lex) u, params
  GROUP BY id
)
SELECT m.*,
       f.rrf
       * exp(-extract(epoch FROM (now() - lower(m.recorded_time))) / 2592000.0)  -- 30-day decay
       * (0.5 + 0.5 * m.confidence)                                              -- confidence weight
       AS prelim_score
FROM fused f
JOIN mnestic_memory m ON m.id = f.id
ORDER BY prelim_score DESC
LIMIT 50;   -- candidate set handed to the cross-encoder reranker
```

Pipeline around the SQL (Phase 1):
1. Optional **query rewrite/expansion** before retrieval (e.g. "auth" -> "auth login oauth jwt").
2. The fused SQL above returns a candidate set (top ~50).
3. A **cross-encoder reranker** (swappable model behind a trait) reorders the candidates; this is the step that closes the gap to the recall bar. `rerank` is a per-call toggle, mirroring the wire API.
4. Apply the recency/confidence decay and return the top `limit`.

`search_mode`:
- `memories`: query `mnestic_memory` only.
- `documents`: query `mnestic_chunk` only.
- `hybrid` (default): union both before fusion, so one call returns personalized facts and knowledge-base chunks.

## 6. Rust core module layout

```
mnestic/                      # workspace
├── crates/
│   ├── mnestic-core/         # domain types, provider traits, pure resolution (decide)
│   ├── mnestic-engine/       # orchestration: add (ingest/extract/resolve), profile, recall
│   ├── mnestic-store/        # Postgres access (sqlx), queries, migrations
│   ├── mnestic-model/        # LLM + embedding + reranker provider traits + impls
│   ├── mnestic-py/           # PyO3 bindings  -> `mnestic` wheel
│   ├── mnestic-node/         # napi-rs bindings -> `mnestic` npm pkg
│   ├── mnestic-mcp/          # MCP server (supermemory-shaped tools)
│   ├── mnestic-compat/       # Phase 2: supermemory-wire REST subset (see 04-compatibility.md)
│   └── pg_mnestic/           # OPTIONAL pgrx extension (accelerators)
└── migrations/               # SQL migrations (the DDL above, dimension templated)
```

Key traits (provider-agnostic):
```rust
trait Embedder  { async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>; }
trait Extractor { async fn extract(&self, text: &str, ctx: &Ctx) -> Result<Vec<Candidate>>; }
trait Reranker  { async fn rerank(&self, q: &str, cands: &[Cand]) -> Result<Vec<Scored>>; }
trait Store     { /* typed query methods over Postgres */ }
```

## 7. SDK API contracts

### 7.1 Python (flagship)
```python
class Mnestic:
    def __init__(self, dsn: str, *, embedder: str = "openai:text-embedding-3-small",
                 extractor: str = "openai:gpt-4o-mini",
                 reranker: str | None = None) -> None: ...

    def add(self, content: str, *, container_tag: str, actor_id: str = "default",
            kind: str = "conversation", custom_id: str | None = None) -> AddResult: ...

    def search(self, q: str, *, container_tag: str, actor_id: str = "default",
               mode: Literal["hybrid","memories","documents"] = "hybrid",
               rerank: bool = False, limit: int = 10) -> list[SearchHit]: ...

    def profile(self, *, container_tag: str, actor_id: str = "default",
                q: str | None = None) -> Profile: ...     # static + dynamic (+ optional search)

    class documents:
        def upload(self, file: Path, *, container_tag: str) -> Document: ...
        def list(self, *, container_tag: str) -> list[Document]: ...

    class settings:
        def update(self, **kwargs) -> None: ...           # extraction/chunking/ontology config
```

The native surface is Mnestic's own. The Phase 2 compat layer (`04-compatibility.md`) maps the `supermemory` SDK calls onto these, so the shells need no Mnestic-specific code.

### 7.2 TypeScript (fast-follow, same shape)
```ts
const m = new Mnestic({ dsn });
await m.add({ content, containerTag, actorId, customId });
const hits = await m.search({ q, containerTag, mode: "hybrid", rerank: true });
const { static: s, dynamic: d } = await m.profile({ containerTag, q });
```

### 7.3 Return types (abridged)
```python
@dataclass
class SearchHit:
    id: str; content: str
    subject: str | None; attribute: str | None; value: str | None
    score: float; confidence: float; recorded_at: datetime; source_id: str | None

@dataclass
class Profile:
    static: list[str]      # durable facts
    dynamic: list[str]     # recent context
    search_results: list[SearchHit] | None
```

## 8. MCP server tool contracts

The MCP server speaks supermemory tool names so existing MCP clients work unchanged (full schemas in `04-compatibility.md` §1):

| Tool | Input | Behavior |
|---|---|---|
| `memory` | `{ content, action: "save"\|"forget", containerTag? }` | Save (full pipeline) or forget. |
| `recall` | `{ query, includeProfile?, containerTag? }` | Hybrid search; returns ranked memories, optionally the profile. |
| `listProjects` | `{ refresh? }` | List container tags. |
| `whoAmI` | `{}` | Returns `userId`, `email`, `name`, session info. |
| `memory-graph` | `{ containerTag? }` | Optional; summary + `structuredContent`. |

Resources `supermemory://profile` and `supermemory://projects`, and a `context` prompt (a prompt, not a tool). Auth: bearer `sm_...` validated via `GET /v3/session`. Returns use the MCP `{ content: [{ type:"text", text }], isError? }` shape.

## 9. pg_mnestic extension surface (optional, Phase 3)

Pushes hot logic into the database, callable as SQL functions:
```sql
mnestic_rank(query_embedding halfvec, query_text text, mode text, lim int) RETURNS SETOF ...
mnestic_resolve(candidate jsonb) RETURNS uuid     -- in-DB supersession in one call
mnestic_decay_sweep() RETURNS int                 -- batch expire, returns rows affected
```
Each has a pure-SQL/engine-side equivalent, so the extension is never mandatory.

## 10. Concurrency, idempotency, errors

- **Atomicity:** the entire write path runs in one transaction; partial failures roll back. No external system to reconcile.
- **Idempotency:** `add` accepts an optional `custom_id` (wire `customId`, unique per tenant) so retries are safe; a repeat resolves to the existing row.
- **Concurrent supersession:** for single-valued attributes the `EXCLUDE` constraint serializes conflicting active writes; on violation the engine retries resolution against the now-current state. Additive writes do not contend.
- **Extraction failures:** malformed LLM JSON is repaired, or the item is parked in `source` with `needs_extraction = true` for retry. Raw is never lost.
- **Fail-closed RLS:** an unset tenant GUC matches no rows.

## 11. Testing & evaluation

- **Unit/integration:** spin up ephemeral Postgres (testcontainers) per suite; assert DDL invariants, RLS isolation (a second tenant must see zero rows), and supersession correctness (point-in-time `valid_time` queries, including out-of-order arrival).
- **Property tests:** random fact streams must never violate the single-valued no-overlap invariant; multi-valued facts must coexist (a regression guard for the original EXCLUDE bug).
- **Eval harness (Phase 1, first-class):** run LongMemEval / LoCoMo / ConvoMem through supermemory's open `memorybench`, tracking MemScore (accuracy / latency / context tokens) in CI over time, compared against mem0/supermemory on the same harness. This is the instrument that lets us claim recall parity honestly.

---

*End of design set. Review order suggestion: `01-high-level-plan.md` -> `02-architecture.md` -> `03-low-level-design.md` -> `04-compatibility.md`.*
