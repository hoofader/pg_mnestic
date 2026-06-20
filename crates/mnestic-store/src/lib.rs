// SPDX-License-Identifier: Apache-2.0

//! Postgres access over sqlx. Runtime query functions only (no compile-time
//! macros), so the build needs no DATABASE_URL.

use chrono::{DateTime, Utc};
use mnestic_core::ExistingMatch;
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// Embedding dimension the schema's `halfvec` columns are built for. The single
/// client-side source of truth pending the templated-dimension work; callers
/// validate against it before binding a vector, so a wrong-dim model fails with a
/// clear error rather than an opaque cast error deep in a query.
pub const EMBEDDING_DIM: usize = 1536;

// Path is relative to CARGO_MANIFEST_DIR (this crate), so up two levels to the
// workspace `migrations/` dir.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn run_migrations(pool: &PgPool) -> std::result::Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

/// Fields needed to insert a memory in the harness. Content-primary; the triple
/// is optional and only the structured path needs subject/attribute/value.
pub struct NewMemory<'a> {
    pub actor_id: &'a str,
    pub content: &'a str,
    pub subject: Option<&'a str>,
    pub attribute: Option<&'a str>,
    pub value: Option<&'a str>,
    pub single_valued: bool,
}

#[derive(Debug, Clone)]
pub struct LatestRow {
    pub id: Uuid,
    pub value: Option<String>,
}

/// An actor's precomputed profile: durable facts plus a recent-context window.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub static_facts: Vec<String>,
    pub dynamic_ctx: Vec<String>,
    pub refreshed_at: Option<DateTime<Utc>>,
}

/// A source claimed for out-of-band extraction: the raw content plus the scope it was
/// enqueued under, so the worker can run the same pipeline the sync path runs inline.
/// `claimed_at` is the lease stamp; the worker presents it back at mark time to prove it
/// still holds the claim (a reclaim by another worker would have overwritten it).
#[derive(Debug, Clone)]
pub struct PendingSource {
    pub id: Uuid,
    pub actor_id: String,
    pub container_tags: Vec<String>,
    pub content: String,
    pub claimed_at: DateTime<Utc>,
}

/// Inputs to hybrid recall, bundled so `recall_memories` stays under the argument-count lint.
pub struct RecallParams<'a> {
    pub tenant_id: Uuid,
    pub actor_id: &'a str,
    pub query_embedding: &'a [f32],
    pub query_text: &'a str,
    pub container_tags: &'a [String],
    pub limit: i64,
    /// Reference instant for recency decay. None means now(); a past value answers "as of
    /// then" (a fact whose event time is near `as_of` ranks as recent).
    pub as_of: Option<DateTime<Utc>>,
}

/// One ranked memory returned by hybrid recall.
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub id: Uuid,
    pub content: Option<String>,
    pub subject: Option<String>,
    pub attribute: Option<String>,
    pub value: Option<String>,
    pub confidence: f32,
    pub recorded_at: Option<DateTime<Utc>>,
    pub score: f64,
    pub metadata: serde_json::Value,
}

/// Fields for one document chunk insert. Bundled so the insert stays under the
/// argument-count lint and the call site reads as named fields.
pub struct NewChunk<'a> {
    pub tenant_id: Uuid,
    pub actor_id: &'a str,
    pub document_id: Uuid,
    pub container_tags: &'a [String],
    pub ord: i32,
    pub content: &'a str,
    pub embedding: &'a [f32],
}

/// One ranked document chunk returned by `search_chunks`, carrying its parent
/// document's fields so the caller can group chunks back into documents.
#[derive(Debug, Clone)]
pub struct ChunkHit {
    pub id: Uuid,
    pub document_id: Uuid,
    pub ord: i32,
    pub content: String,
    pub score: f64,
    pub document_title: Option<String>,
    pub document_metadata: serde_json::Value,
    pub document_created_at: Option<DateTime<Utc>>,
}

/// A fully specified memory row for the engine's write path. Unlike `NewMemory`,
/// it carries the embedding, temporal bounds, and supersession lineage.
pub struct NewMemoryFull<'a> {
    pub tenant_id: Uuid,
    pub actor_id: &'a str,
    pub container_tags: &'a [String],
    pub content: &'a str,
    pub subject: Option<&'a str>,
    pub attribute: Option<&'a str>,
    pub value: Option<&'a str>,
    pub single_valued: bool,
    pub confidence: f32,
    pub is_static: bool,
    pub mem_type: &'a str,
    pub embedding: Option<&'a [f32]>,
    pub source_id: Option<Uuid>,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub is_latest: bool,
    pub supersedes_id: Option<Uuid>,
    pub forget_after: Option<DateTime<Utc>>,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Insert a memory under the given tenant. The GUC is set with SET LOCAL so
    /// RLS scopes the write to that tenant, all in one tx.
    pub async fn insert_memory(&self, tenant_id: Uuid, m: &NewMemory<'_>) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, tenant_id).await?;
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO mnestic_memory \
               (tenant_id, actor_id, content, subject, attribute, value, single_valued) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(tenant_id)
        .bind(m.actor_id)
        .bind(m.content)
        .bind(m.subject)
        .bind(m.attribute)
        .bind(m.value)
        .bind(m.single_valued)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Count memories visible under the given tenant GUC. With the GUC unset the
    /// policy matches no rows (fail-closed), so callers can assert isolation.
    pub async fn count_visible_memories(&self, tenant_id: Option<Uuid>) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        if let Some(t) = tenant_id {
            set_tenant(&mut tx, t).await?;
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mnestic_memory")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(count)
    }

    /// Fetch the latest active single-valued row for (actor, subject, attribute).
    pub async fn latest_single_valued(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Option<LatestRow>> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, tenant_id).await?;
        let row = sqlx::query(
            "SELECT id, value FROM mnestic_memory \
             WHERE actor_id = $1 AND subject = $2 AND attribute = $3 \
               AND single_valued AND is_latest AND status = 'active'",
        )
        .bind(actor_id)
        .bind(subject)
        .bind(attribute)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.map(|r| LatestRow {
            id: r.get("id"),
            value: r.get("value"),
        }))
    }

    /// Supersede a prior single-valued fact with a new value, in one tx:
    /// close the prior valid_time, mark it superseded/is_latest=false, and
    /// insert the new row with supersedes_id and is_latest=true (LLD §5.2).
    pub async fn supersede_single_valued(
        &self,
        tenant_id: Uuid,
        prior_id: Uuid,
        m: &NewMemory<'_>,
        at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, tenant_id).await?;

        // Only close the prior when the cutover is strictly after its validity start.
        // Otherwise tstzrange(lower, $2) would be empty and silently lose history; in
        // that degenerate (out-of-order) case the prior stays active and the insert
        // below trips the EXCLUDE loudly. Proper event-order splitting is Phase 1.
        sqlx::query(
            "UPDATE mnestic_memory SET \
               valid_time = tstzrange(lower(valid_time), $2), \
               recorded_time = tstzrange(lower(recorded_time), now()), \
               status = 'superseded', is_latest = false \
             WHERE id = $1 AND lower(valid_time) < $2",
        )
        .bind(prior_id)
        .bind(at)
        .execute(&mut *tx)
        .await?;

        let new_id: Uuid = sqlx::query_scalar(
            "INSERT INTO mnestic_memory \
               (tenant_id, actor_id, content, subject, attribute, value, single_valued, \
                supersedes_id, is_latest, valid_time) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, true, tstzrange($9, NULL)) \
             RETURNING id",
        )
        .bind(tenant_id)
        .bind(m.actor_id)
        .bind(m.content)
        .bind(m.subject)
        .bind(m.attribute)
        .bind(m.value)
        .bind(m.single_valued)
        .bind(prior_id)
        .bind(at)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(new_id)
    }

    /// Begin a transaction with the tenant GUC already set, so the engine can run a
    /// multi-step write (source + resolved memories) as one atomic unit.
    pub async fn begin_tenant(
        &self,
        tenant_id: Uuid,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
        let mut tx = self.pool.begin().await?;
        set_tenant(&mut tx, tenant_id).await?;
        Ok(tx)
    }

    /// Persist the raw item to the audit trail. Returns None when a row with this
    /// (tenant, custom_id) already exists, so the caller can treat the add as an
    /// idempotent no-op instead of re-running the pipeline.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_source_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        kind: &str,
        content: &str,
        custom_id: Option<&str>,
        // true for the async path: the row is enqueued and a worker extracts it later.
        needs_extraction: bool,
    ) -> Result<Option<Uuid>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO mnestic_source \
               (tenant_id, actor_id, container_tags, kind, raw, custom_id, needs_extraction) \
             VALUES ($1, $2, $3, $4, jsonb_build_object('text', $5::text), $6, $7) \
             ON CONFLICT (tenant_id, custom_id) DO NOTHING \
             RETURNING id",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(container_tags)
        .bind(kind)
        .bind(content)
        .bind(custom_id)
        .bind(needs_extraction)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Enqueue a source for out-of-band extraction (the `dreaming: dynamic` path): persist the
    /// raw content with `needs_extraction = true` and return without running the model. None
    /// means this (tenant, custom_id) was already ingested, so the caller treats it as a skip.
    pub async fn enqueue_source(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        kind: &str,
        content: &str,
        custom_id: Option<&str>,
    ) -> Result<Option<Uuid>> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let id =
            Self::insert_source_tx(&mut tx, tenant_id, actor_id, container_tags, kind, content, custom_id, true)
                .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Lease-claim one pending source for a tenant: stamp `claimed_at` so concurrent workers
    /// skip it, and return its content for extraction. A claim older than `lease_secs` is
    /// considered abandoned and reclaimable. `FOR UPDATE SKIP LOCKED` lets parallel workers
    /// claim distinct rows without blocking. Returns None when nothing is pending.
    pub async fn claim_pending_source(
        &self,
        tenant_id: Uuid,
        lease_secs: i64,
    ) -> Result<Option<PendingSource>> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let row = sqlx::query(
            "UPDATE mnestic_source SET claimed_at = now() \
             WHERE id = ( \
               SELECT id FROM mnestic_source \
               WHERE tenant_id = $1 AND needs_extraction \
                 AND (claimed_at IS NULL OR claimed_at < now() - make_interval(secs => $2)) \
               ORDER BY ingested_at \
               FOR UPDATE SKIP LOCKED \
               LIMIT 1 \
             ) \
             RETURNING id, actor_id, container_tags, raw->>'text' AS content, claimed_at",
        )
        .bind(tenant_id)
        .bind(lease_secs as f64)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.map(|r| PendingSource {
            id: r.get("id"),
            actor_id: r.get("actor_id"),
            container_tags: r.get("container_tags"),
            content: r.get::<Option<String>, _>("content").unwrap_or_default(),
            claimed_at: r.get("claimed_at"),
        }))
    }

    /// Clear the extraction flag once a claimed source has been processed, but only if this
    /// worker still holds the claim: the `claimed_at = $3` guard fails if another worker
    /// reclaimed the source after its lease lapsed mid-extraction. Returns false in that case
    /// so the caller rolls back its duplicate writes and lets the reclaiming worker win.
    pub async fn mark_source_extracted_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        source_id: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE mnestic_source SET needs_extraction = false, claimed_at = NULL \
             WHERE tenant_id = $1 AND id = $2 AND claimed_at = $3",
        )
        .bind(tenant_id)
        .bind(source_id)
        .bind(claimed_at)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// All tenant ids, off RLS like the other registry reads. A worker iterates these to find
    /// pending work, since the per-tenant `needs_extraction` rows are not visible across the
    /// RLS boundary.
    pub async fn list_tenant_ids(&self) -> Result<Vec<Uuid>> {
        let ids = sqlx::query_scalar("SELECT id FROM mnestic_tenant")
            .fetch_all(&self.pool)
            .await?;
        Ok(ids)
    }

    /// Resolve an existing source id by its caller-supplied custom_id.
    pub async fn source_id_by_custom_id_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        custom_id: &str,
    ) -> Result<Option<Uuid>> {
        let id = sqlx::query_scalar(
            "SELECT id FROM mnestic_source WHERE tenant_id = $1 AND custom_id = $2",
        )
        .bind(tenant_id)
        .bind(custom_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Latest active rows for (tenant, actor, subject, attribute), each with its
    /// validity start so the engine can order supersession in event time. The
    /// explicit tenant_id is defense-in-depth beyond RLS and lets the partial index
    /// on (tenant_id, actor_id, subject, attribute) drive the lookup.
    pub async fn latest_matches_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Vec<ExistingMatch>> {
        let rows = sqlx::query(
            "SELECT id, value, single_valued, lower(valid_time) AS valid_from \
             FROM mnestic_memory \
             WHERE tenant_id = $1 AND actor_id = $2 AND subject = $3 AND attribute = $4 \
               AND is_latest AND status = 'active'",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(subject)
        .bind(attribute)
        .fetch_all(&mut **tx)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ExistingMatch {
                id: r.get::<Uuid, _>("id").to_string(),
                value: r.get("value"),
                single_valued: r.get("single_valued"),
                valid_from: r.get("valid_from"),
            })
            .collect())
    }

    /// All known validity-interval starts for (tenant, actor, subject, attribute),
    /// across every status, so the engine can place a late-arriving fact without
    /// overlapping an existing segment (active or superseded).
    pub async fn segment_starts_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        subject: &str,
        attribute: &str,
    ) -> Result<Vec<DateTime<Utc>>> {
        let rows = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "SELECT lower(valid_time) FROM mnestic_memory \
             WHERE tenant_id = $1 AND actor_id = $2 AND subject = $3 AND attribute = $4",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(subject)
        .bind(attribute)
        .fetch_all(&mut **tx)
        .await?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Insert a fully specified memory row and return its id.
    pub async fn insert_memory_full_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        m: &NewMemoryFull<'_>,
    ) -> Result<Uuid> {
        let embedding = m.embedding.map(vec_literal);
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO mnestic_memory \
               (tenant_id, actor_id, container_tags, content, subject, attribute, value, \
                single_valued, confidence, is_static, mem_type, embedding, source_id, \
                valid_time, is_latest, supersedes_id, forget_after) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::halfvec, $13, \
                     tstzrange($14, $15), $16, $17, $18) \
             RETURNING id",
        )
        .bind(m.tenant_id)
        .bind(m.actor_id)
        .bind(m.container_tags)
        .bind(m.content)
        .bind(m.subject)
        .bind(m.attribute)
        .bind(m.value)
        .bind(m.single_valued)
        .bind(m.confidence)
        .bind(m.is_static)
        .bind(m.mem_type)
        .bind(embedding)
        .bind(m.source_id)
        .bind(m.valid_from)
        .bind(m.valid_until)
        .bind(m.is_latest)
        .bind(m.supersedes_id)
        .bind(m.forget_after)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Close a prior row's validity at `at` and mark it superseded. Fires only when
    /// `at` is strictly after the row's validity start, so it never collapses the
    /// interval to empty; the out-of-order case is handled by the engine.
    pub async fn close_prior_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        prior_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<u64> {
        let done = sqlx::query(
            "UPDATE mnestic_memory SET \
               valid_time = tstzrange(lower(valid_time), $3), \
               recorded_time = tstzrange(lower(recorded_time), now()), \
               status = 'superseded', is_latest = false \
             WHERE tenant_id = $1 AND id = $2 AND lower(valid_time) < $3",
        )
        .bind(tenant_id)
        .bind(prior_id)
        .bind(at)
        .execute(&mut **tx)
        .await?;
        Ok(done.rows_affected())
    }

    /// Raise a row's confidence (capped at 1.0) when an identical fact recurs.
    pub async fn bump_confidence_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        id: Uuid,
        delta: f32,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE mnestic_memory SET confidence = least(1.0, confidence + $3) \
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(delta)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Tombstone the active memories an earlier `add` produced under `custom_id`, for
    /// this actor, inside the caller's tx; returns the forgotten ids. Only system time
    /// (`recorded_time`) is closed; `valid_time` is left intact, since forgetting is a
    /// belief change, not a claim that the fact was never valid. Superseded priors stay
    /// history (forgetting the newer fact does not resurrect the one it replaced), and
    /// the kept `valid_time` still partitions the axis, so a later out-of-order insert
    /// for the same attribute is placed around the forgotten interval, not over it.
    /// The CASE on `recorded_time` closes the system-time range only when that yields a
    /// non-empty interval, so a backward clock never collapses it to empty.
    pub async fn forget_source_memories_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        custom_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<Uuid>> {
        let ids = sqlx::query_scalar::<_, Uuid>(
            "UPDATE mnestic_memory m SET status = 'forgotten', is_latest = false, \
                    forget_reason = $4, \
                    recorded_time = CASE WHEN now() > lower(recorded_time) \
                                         THEN tstzrange(lower(recorded_time), now()) \
                                         ELSE recorded_time END \
             FROM mnestic_source s \
             WHERE m.source_id = s.id AND s.custom_id = $3 \
               AND m.tenant_id = $1 AND s.tenant_id = $1 \
               AND m.actor_id = $2 AND m.status = 'active' \
             RETURNING m.id",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(custom_id)
        .bind(reason)
        .fetch_all(&mut **tx)
        .await?;
        Ok(ids)
    }

    /// Tombstone a single memory by id (content-based forget targets ids found by key
    /// match). Same bitemporal close as the source variant: system time closed, event
    /// time kept. Returns rows affected (0 if it was not active). Caller's tx.
    pub async fn forget_memory_by_id_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        id: Uuid,
        reason: Option<&str>,
    ) -> Result<u64> {
        let done = sqlx::query(
            "UPDATE mnestic_memory SET status = 'forgotten', is_latest = false, \
                    forget_reason = $3, \
                    recorded_time = CASE WHEN now() > lower(recorded_time) \
                                         THEN tstzrange(lower(recorded_time), now()) \
                                         ELSE recorded_time END \
             WHERE tenant_id = $1 AND id = $2 AND status = 'active'",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(reason)
        .execute(&mut **tx)
        .await?;
        Ok(done.rows_affected())
    }

    /// Hybrid recall over the actor's latest active memories: vector similarity and
    /// lexical (tsvector) relevance fused with reciprocal-rank fusion, then weighted
    /// by recency decay and confidence (LLD §5.4). Recency is an exponential decay on event
    /// time (`valid_time`'s start, so a backfilled fact ages from when it was true, not from
    /// when it was ingested) relative to `as_of`, with a 30-day time constant (half-life
    /// about 21 days). A fact whose event is still ahead of `as_of` is clamped to maximum
    /// recency rather than excluded, so for a past `as_of` not-yet-valid facts surface at
    /// full recency (filtering them out instead is a future option). Superseded, non-latest,
    /// and time-expired rows are excluded. This uses the tsvector floor; the pg_search BM25
    /// path swaps the lexical CTE where the extension is available.
    pub async fn recall_memories(&self, p: RecallParams<'_>) -> Result<Vec<RecallHit>> {
        let RecallParams {
            tenant_id,
            actor_id,
            query_embedding,
            query_text,
            container_tags,
            limit,
            as_of,
        } = p;
        let qvec = vec_literal(query_embedding);
        let mut tx = self.begin_tenant(tenant_id).await?;
        // The container filter is a residual predicate on the HNSW top-k, so without
        // iterative scan a selective filter could return fewer than `limit` in-scope
        // rows while matching rows sit deeper in the index. relaxed_order keeps walking
        // until enough match; final ranking re-sorts anyway, so the looser order is fine.
        if !container_tags.is_empty() {
            sqlx::query("SET LOCAL hnsw.iterative_scan = 'relaxed_order'")
                .execute(&mut *tx)
                .await?;
        }
        let rows = sqlx::query(RECALL_SQL)
            .bind(tenant_id)
            .bind(qvec)
            .bind(query_text)
            .bind(actor_id)
            .bind(limit)
            .bind(container_tags)
            .bind(as_of)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|r| RecallHit {
                id: r.get("id"),
                content: r.get("content"),
                subject: r.get("subject"),
                attribute: r.get("attribute"),
                value: r.get("value"),
                confidence: r.get("confidence"),
                recorded_at: r.get("recorded_at"),
                score: r.get("score"),
                // NOT NULL in schema; tolerate a NULL from a future join/backfill rather
                // than panic on the recall path. A genuine type mismatch still panics.
                metadata: r
                    .get::<Option<serde_json::Value>, _>("metadata")
                    .unwrap_or_else(|| serde_json::json!({})),
            })
            .collect())
    }

    /// Insert a document row (provenance + metadata) in the caller's tx and return its
    /// id. Chunks are inserted separately with `insert_chunk_tx`.
    pub async fn insert_document_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        source_id: Option<Uuid>,
        container_tags: &[String],
        title: Option<&str>,
        uri: Option<&str>,
    ) -> Result<Uuid> {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO mnestic_document \
               (tenant_id, actor_id, source_id, container_tags, title, uri) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(source_id)
        .bind(container_tags)
        .bind(title)
        .bind(uri)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Insert one chunk of a document in the caller's tx. `content_tsv` is a generated
    /// column, so only `content` and `embedding` are supplied; returns the chunk id.
    pub async fn insert_chunk_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        c: &NewChunk<'_>,
    ) -> Result<Uuid> {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO mnestic_chunk \
               (tenant_id, actor_id, document_id, container_tags, ord, content, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::halfvec) RETURNING id",
        )
        .bind(c.tenant_id)
        .bind(c.actor_id)
        .bind(c.document_id)
        .bind(c.container_tags)
        .bind(c.ord)
        .bind(c.content)
        .bind(vec_literal(c.embedding))
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    /// Hybrid search over an actor's document chunks: vector and lexical fused with RRF,
    /// scoped by tenant, actor, and an optional container_tags filter. Chunks are
    /// immutable reference text, so there is no recency/confidence weighting or
    /// supersession; the score is the fused rank alone.
    pub async fn search_chunks(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        query_embedding: &[f32],
        query_text: &str,
        container_tags: &[String],
        limit: i64,
    ) -> Result<Vec<ChunkHit>> {
        let qvec = vec_literal(query_embedding);
        let mut tx = self.begin_tenant(tenant_id).await?;
        if !container_tags.is_empty() {
            sqlx::query("SET LOCAL hnsw.iterative_scan = 'relaxed_order'")
                .execute(&mut *tx)
                .await?;
        }
        let rows = sqlx::query(CHUNK_SEARCH_SQL)
            .bind(tenant_id)
            .bind(qvec)
            .bind(query_text)
            .bind(actor_id)
            .bind(limit)
            .bind(container_tags)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|r| ChunkHit {
                id: r.get("id"),
                document_id: r.get("document_id"),
                ord: r.get("ord"),
                content: r.get("content"),
                score: r.get("score"),
                document_title: r.get("document_title"),
                document_metadata: r
                    .get::<Option<serde_json::Value>, _>("document_metadata")
                    .unwrap_or_else(|| serde_json::json!({})),
                document_created_at: r.get("document_created_at"),
            })
            .collect())
    }

    /// Recompute and upsert the actor's profile from current latest memories. A
    /// bounded recompute (top static facts plus a recent window) run inside the write
    /// transaction; an out-of-band debounced refresh is a scaling option.
    pub async fn refresh_profile_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        static_confidence: f32,
        static_limit: i64,
        dynamic_limit: i64,
    ) -> Result<()> {
        sqlx::query(PROFILE_REFRESH_SQL)
            .bind(tenant_id)
            .bind(actor_id)
            .bind(static_confidence)
            .bind(static_limit)
            .bind(dynamic_limit)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Read the cached profile for an actor, if one has been built.
    pub async fn get_profile(&self, tenant_id: Uuid, actor_id: &str) -> Result<Option<Profile>> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let row = sqlx::query(
            "SELECT array(SELECT jsonb_array_elements_text(static_facts)) AS s, \
                    array(SELECT jsonb_array_elements_text(dynamic_ctx)) AS d, \
                    refreshed_at \
             FROM mnestic_profile WHERE tenant_id = $1 AND actor_id = $2",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.map(|r| Profile {
            static_facts: r.get("s"),
            dynamic_ctx: r.get("d"),
            refreshed_at: Some(r.get("refreshed_at")),
        }))
    }

    /// The tenant's external id (the supermemory userId). Read off RLS like the api_key
    /// bootstrap, since mnestic_tenant is the registry, not tenant-scoped data.
    pub async fn tenant_external_id(&self, tenant_id: Uuid) -> Result<Option<String>> {
        let id = sqlx::query_scalar("SELECT external_id FROM mnestic_tenant WHERE id = $1")
            .bind(tenant_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(id)
    }

    /// Distinct container tags across the tenant's memories and chunks (the supermemory
    /// "projects"). RLS-scoped, so it runs in a tenant tx.
    pub async fn list_container_tags(&self, tenant_id: Uuid) -> Result<Vec<String>> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT tag FROM ( \
                SELECT unnest(container_tags) AS tag FROM mnestic_memory \
                  WHERE tenant_id = $1 AND is_latest AND status = 'active' \
                UNION \
                SELECT unnest(container_tags) AS tag FROM mnestic_chunk WHERE tenant_id = $1 \
             ) s WHERE tag <> '' ORDER BY tag",
        )
        .bind(tenant_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(tags)
    }

    /// An actor's documents (id, title) for the memory-graph view, newest first.
    pub async fn list_documents(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
    ) -> Result<Vec<(Uuid, Option<String>)>> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let rows = sqlx::query(
            "SELECT id, title FROM mnestic_document \
             WHERE tenant_id = $1 AND actor_id = $2 ORDER BY created_at DESC",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|r| (r.get("id"), r.get("title"))).collect())
    }

    /// Permanently delete every row belonging to one actor within a tenant: memories,
    /// chunks, documents, sources, and the cached profile. This is the GDPR right-to-erasure
    /// path, distinct from `forget` (a soft tombstone). It runs in one transaction so a
    /// failure leaves the actor's data intact rather than half-deleted.
    pub async fn purge_actor(&self, tenant_id: Uuid, actor_id: &str) -> Result<PurgeCounts> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        // supersedes_id is a self-referential NO ACTION FK. Supersession is always within one
        // (tenant, actor), so every reference among the actor's rows points inside the set we
        // are deleting; nulling them first lets the bulk delete run without a 23503. If a
        // future feature ever lets supersession cross actors, this would fail closed (roll
        // back the whole purge), not corrupt data.
        sqlx::query("UPDATE mnestic_memory SET supersedes_id = NULL WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?;
        // Delete referencing rows before the rows they reference: memories and chunks point at
        // sources/documents, so sources go last.
        let memories = sqlx::query("DELETE FROM mnestic_memory WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let chunks = sqlx::query("DELETE FROM mnestic_chunk WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let documents = sqlx::query("DELETE FROM mnestic_document WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let sources = sqlx::query("DELETE FROM mnestic_source WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let profile = sqlx::query("DELETE FROM mnestic_profile WHERE tenant_id = $1 AND actor_id = $2")
            .bind(tenant_id)
            .bind(actor_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(PurgeCounts { memories, chunks, documents, sources, profile })
    }

    /// Export everything held for one actor as a pretty-printed JSON document (the GDPR
    /// right-to-access/portability path). Postgres assembles the document so the wire format
    /// stays in lockstep with the schema. The opaque `embedding`/`content_tsv` retrieval
    /// columns are dropped; the natural-language content and metadata are what the subject
    /// gets. RLS-scoped to the tenant; `actor_id` is the subject filter.
    pub async fn export_actor(&self, tenant_id: Uuid, actor_id: &str) -> Result<String> {
        let mut tx = self.begin_tenant(tenant_id).await?;
        let json: String = sqlx::query_scalar(
            "SELECT jsonb_pretty(jsonb_build_object( \
               'tenant_id', $1::uuid, \
               'actor_id', $2::text, \
               'exported_at', now(), \
               'memories', COALESCE((SELECT jsonb_agg((to_jsonb(m) - 'embedding' - 'content_tsv') \
                                       ORDER BY m.created_at) \
                                     FROM mnestic_memory m \
                                     WHERE m.tenant_id = $1 AND m.actor_id = $2), '[]'::jsonb), \
               'documents', COALESCE((SELECT jsonb_agg(to_jsonb(d) ORDER BY d.created_at) \
                                      FROM mnestic_document d \
                                      WHERE d.tenant_id = $1 AND d.actor_id = $2), '[]'::jsonb), \
               'chunks', COALESCE((SELECT jsonb_agg((to_jsonb(c) - 'embedding' - 'content_tsv') \
                                     ORDER BY c.document_id, c.ord) \
                                   FROM mnestic_chunk c \
                                   WHERE c.tenant_id = $1 AND c.actor_id = $2), '[]'::jsonb), \
               'sources', COALESCE((SELECT jsonb_agg(to_jsonb(s) ORDER BY s.ingested_at) \
                                    FROM mnestic_source s \
                                    WHERE s.tenant_id = $1 AND s.actor_id = $2), '[]'::jsonb), \
               'profile', (SELECT to_jsonb(p) FROM mnestic_profile p \
                           WHERE p.tenant_id = $1 AND p.actor_id = $2) \
             ))::text",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json)
    }
}

/// Rows removed by `purge_actor`, per table, for the operator's audit log.
#[derive(Debug, Clone, Default)]
pub struct PurgeCounts {
    pub memories: u64,
    pub chunks: u64,
    pub documents: u64,
    pub sources: u64,
    pub profile: u64,
}

// Static facts are durable and high-confidence (is_static or confidence >= the
// threshold) and never ephemeral; dynamic context is the most recent window
// regardless of confidence. A structured row renders as \"attribute: value\", an
// unstructured one as its content. jsonb_agg orders the array so the cached profile
// is deterministic.
const PROFILE_REFRESH_SQL: &str = "\
INSERT INTO mnestic_profile (tenant_id, actor_id, static_facts, dynamic_ctx, refreshed_at) \
SELECT $1, $2, \
  COALESCE((SELECT jsonb_agg(txt ORDER BY conf DESC, rt DESC, id DESC) \
              FILTER (WHERE txt IS NOT NULL) FROM ( \
     SELECT CASE WHEN attribute IS NOT NULL \
                 THEN attribute || COALESCE(': ' || value, '') ELSE content END AS txt, \
            confidence AS conf, lower(recorded_time) AS rt, id \
     FROM mnestic_memory \
     WHERE tenant_id = $1 AND actor_id = $2 AND is_latest AND status = 'active' \
       AND forget_after IS NULL AND (is_static OR confidence >= $3) \
     ORDER BY confidence DESC, lower(recorded_time) DESC, id DESC LIMIT $4) s), '[]'::jsonb), \
  COALESCE((SELECT jsonb_agg(txt ORDER BY rt DESC, id DESC) \
              FILTER (WHERE txt IS NOT NULL) FROM ( \
     SELECT CASE WHEN attribute IS NOT NULL \
                 THEN attribute || COALESCE(': ' || value, '') ELSE content END AS txt, \
            lower(recorded_time) AS rt, id \
     FROM mnestic_memory \
     WHERE tenant_id = $1 AND actor_id = $2 AND is_latest AND status = 'active' \
       AND (forget_after IS NULL OR forget_after > now()) \
     ORDER BY lower(recorded_time) DESC, id DESC LIMIT $5) d), '[]'::jsonb), \
  now() \
ON CONFLICT (tenant_id, actor_id) DO UPDATE \
  SET static_facts = EXCLUDED.static_facts, dynamic_ctx = EXCLUDED.dynamic_ctx, \
      refreshed_at = EXCLUDED.refreshed_at";

// The filters live inside each signal's inner subquery, and each does
// `ORDER BY <distance|rank> LIMIT k` so the HNSW and GIN indexes drive the top-k
// pull (a wrapping CTE or a window over the full set would force a scan plus sort).
// Ranks are then assigned over just those k rows. Per-signal fan-out is at least the
// caller's limit before filtering; a container filter shrinks it after the index walk,
// which is why recall_memories turns on hnsw.iterative_scan when one is set. Final
// ordering is fully determined by (score, recency, id), so results are stable across
// calls. This is the tsvector floor; the pg_search BM25 path swaps the lex subquery. It
// scopes by tenant and actor, plus an optional container_tags filter ($6, array
// containment, all-of by design: any-of is a union of all-of at a higher layer). An
// empty array imposes no filter.
const RECALL_SQL: &str = "\
WITH vec AS ( \
  SELECT id, row_number() OVER (ORDER BY dist) AS rnk FROM ( \
    SELECT id, embedding <=> $2::halfvec AS dist FROM mnestic_memory \
    WHERE tenant_id = $1 AND actor_id = $4 AND is_latest AND status = 'active' \
      AND (forget_after IS NULL OR forget_after > now()) AND embedding IS NOT NULL \
      AND (cardinality($6::text[]) = 0 OR container_tags @> $6::text[]) \
    ORDER BY embedding <=> $2::halfvec LIMIT greatest(50, $5) \
  ) t \
), \
lex AS ( \
  SELECT id, row_number() OVER (ORDER BY lr DESC) AS rnk FROM ( \
    SELECT id, ts_rank(content_tsv, plainto_tsquery('english', $3)) AS lr \
    FROM mnestic_memory \
    WHERE tenant_id = $1 AND actor_id = $4 AND is_latest AND status = 'active' \
      AND (forget_after IS NULL OR forget_after > now()) \
      AND content_tsv @@ plainto_tsquery('english', $3) \
      AND (cardinality($6::text[]) = 0 OR container_tags @> $6::text[]) \
    ORDER BY lr DESC LIMIT greatest(50, $5) \
  ) t \
), \
fused AS ( \
  SELECT id, SUM(1.0 / (60 + rnk)) AS rrf \
  FROM (SELECT id, rnk FROM vec UNION ALL SELECT id, rnk FROM lex) u \
  GROUP BY id \
) \
SELECT m.id, m.content, m.subject, m.attribute, m.value, m.confidence, m.metadata, \
       lower(m.recorded_time) AS recorded_at, \
       (f.rrf \
        * exp(-greatest(0, extract(epoch FROM (coalesce($7, now()) - lower(m.valid_time)))) / 2592000.0) \
        * (0.5 + 0.5 * m.confidence))::float8 AS score \
FROM fused f JOIN mnestic_memory m ON m.id = f.id \
ORDER BY score DESC, recorded_at DESC NULLS LAST, m.id \
LIMIT $5";

// Document-chunk search. Same hybrid shape as RECALL_SQL (per-signal top-k inside the
// index-driven subqueries, fused by RRF), but chunks are immutable reference text, so
// there is no is_latest/status/recency/confidence. Scoped by tenant, actor, and the
// optional container_tags filter ($6). Binds match recall_memories: limit is $5.
const CHUNK_SEARCH_SQL: &str = "\
WITH vec AS ( \
  SELECT id, row_number() OVER (ORDER BY dist) AS rnk FROM ( \
    SELECT id, embedding <=> $2::halfvec AS dist FROM mnestic_chunk \
    WHERE tenant_id = $1 AND actor_id = $4 AND embedding IS NOT NULL \
      AND (cardinality($6::text[]) = 0 OR container_tags @> $6::text[]) \
    ORDER BY embedding <=> $2::halfvec LIMIT greatest(50, $5) \
  ) t \
), \
lex AS ( \
  SELECT id, row_number() OVER (ORDER BY lr DESC) AS rnk FROM ( \
    SELECT id, ts_rank(content_tsv, plainto_tsquery('english', $3)) AS lr \
    FROM mnestic_chunk \
    WHERE tenant_id = $1 AND actor_id = $4 \
      AND content_tsv @@ plainto_tsquery('english', $3) \
      AND (cardinality($6::text[]) = 0 OR container_tags @> $6::text[]) \
    ORDER BY lr DESC LIMIT greatest(50, $5) \
  ) t \
), \
fused AS ( \
  SELECT id, SUM(1.0 / (60 + rnk)) AS rrf \
  FROM (SELECT id, rnk FROM vec UNION ALL SELECT id, rnk FROM lex) u \
  GROUP BY id \
) \
SELECT c.id, c.document_id, c.ord, c.content, f.rrf::float8 AS score, \
       d.title AS document_title, d.metadata AS document_metadata, d.created_at AS document_created_at \
FROM fused f JOIN mnestic_chunk c ON c.id = f.id \
JOIN mnestic_document d ON d.id = c.document_id \
ORDER BY score DESC, c.id \
LIMIT $5";

/// Render an embedding as a pgvector text literal (`[a,b,c]`) for a `::halfvec`
/// cast, so the write path needs no dedicated vector-binding dependency.
fn vec_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // pgvector rejects NaN/Inf, and a single bad component would abort the
        // write. Coerce non-finite values to 0 so a misbehaving embedder degrades
        // one dimension instead of dropping the whole memory.
        let x = if x.is_finite() { *x } else { 0.0 };
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

async fn set_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
) -> Result<()> {
    // set_config is the bindable form of SET LOCAL (is_local = true), so the tenant
    // value is a bound parameter and never interpolated into SQL text.
    sqlx::query("SELECT set_config('mnestic.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::MIGRATOR;

    // The assertion below pins the SHA-384 of every shipped migration as it was embedded at
    // build time. sqlx re-checks that same checksum against _sqlx_migrations on every start,
    // so an in-place edit of a file a database already ran makes run_migrations refuse to
    // boot. This test turns that runtime failure into a compile-time tripwire: once a
    // migration ships, append its (version, sha384) here and make changes in a NEW file.
    // See MIGRATIONS.md.
    const FROZEN: &[(i64, &str)] = &[
        (
            1,
            "36e381dad2f9d73367beb12d8f045dbed9d3c2a8aadf9241404e04d53c22d3532e138d675fab1b39e7c3eda100f2a2b4",
        ),
        (
            2,
            "9cfe123f3469bdc2a878125c22f3d8ae2a712b2fd8568b3000dd224bf570fb3eb6f6a9bfafeef0286a2577fc269dce9c",
        ),
        (
            3,
            "1e6096be8b4b7bbc1d14a2c45e763379dfe35d95d0402b6998aadeb3744e9ed5ac9d4cf68f3457dc558da9d7e9784919",
        ),
    ];

    #[test]
    fn shipped_migrations_are_frozen() {
        for &(version, expected) in FROZEN {
            let m = MIGRATOR
                .iter()
                .find(|m| m.version == version)
                .unwrap_or_else(|| panic!("migration {version:04} present in the embedded set"));
            let hex: String = m.checksum.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(
                hex, expected,
                "migration {version:04} changed; shipped migrations are append-only (see MIGRATIONS.md)"
            );
        }
    }
}
