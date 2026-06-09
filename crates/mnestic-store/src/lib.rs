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
    pub async fn insert_source_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        kind: &str,
        content: &str,
        custom_id: Option<&str>,
    ) -> Result<Option<Uuid>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO mnestic_source \
               (tenant_id, actor_id, container_tags, kind, raw, custom_id) \
             VALUES ($1, $2, $3, $4, jsonb_build_object('text', $5::text), $6) \
             ON CONFLICT (tenant_id, custom_id) DO NOTHING \
             RETURNING id",
        )
        .bind(tenant_id)
        .bind(actor_id)
        .bind(container_tags)
        .bind(kind)
        .bind(content)
        .bind(custom_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(id)
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

    /// Hybrid recall over the actor's latest active memories: vector similarity and
    /// lexical (tsvector) relevance fused with reciprocal-rank fusion, then weighted
    /// by recency decay and confidence (LLD §5.4). Superseded, non-latest, and
    /// time-expired rows are excluded. This uses the tsvector floor; the pg_search
    /// BM25 path swaps the lexical CTE where the extension is available.
    pub async fn recall_memories(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        query_embedding: &[f32],
        query_text: &str,
        limit: i64,
    ) -> Result<Vec<RecallHit>> {
        let qvec = vec_literal(query_embedding);
        let mut tx = self.begin_tenant(tenant_id).await?;
        let rows = sqlx::query(RECALL_SQL)
            .bind(tenant_id)
            .bind(qvec)
            .bind(query_text)
            .bind(actor_id)
            .bind(limit)
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
            })
            .collect())
    }
}

// The filters live inside each signal's inner subquery, and each does
// `ORDER BY <distance|rank> LIMIT k` so the HNSW and GIN indexes drive the top-k
// pull (a wrapping CTE or a window over the full set would force a scan plus sort).
// Ranks are then assigned over just those k rows. Per-signal fan-out is at least the
// caller's limit. Final ordering is fully determined by (score, recency, id), so
// results are stable across calls. At large per-actor volumes the filtered HNSW scan
// may need hnsw.ef_search or iterative-scan tuning. This is the tsvector floor; the
// pg_search BM25 path swaps the lex subquery. It scopes by tenant and actor only;
// a container_tags filter is a later increment.
const RECALL_SQL: &str = "\
WITH vec AS ( \
  SELECT id, row_number() OVER (ORDER BY dist) AS rnk FROM ( \
    SELECT id, embedding <=> $2::halfvec AS dist FROM mnestic_memory \
    WHERE tenant_id = $1 AND actor_id = $4 AND is_latest AND status = 'active' \
      AND (forget_after IS NULL OR forget_after > now()) AND embedding IS NOT NULL \
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
    ORDER BY lr DESC LIMIT greatest(50, $5) \
  ) t \
), \
fused AS ( \
  SELECT id, SUM(1.0 / (60 + rnk)) AS rrf \
  FROM (SELECT id, rnk FROM vec UNION ALL SELECT id, rnk FROM lex) u \
  GROUP BY id \
) \
SELECT m.id, m.content, m.subject, m.attribute, m.value, m.confidence, \
       lower(m.recorded_time) AS recorded_at, \
       (f.rrf \
        * exp(-extract(epoch FROM (now() - lower(m.recorded_time))) / 2592000.0) \
        * (0.5 + 0.5 * m.confidence))::float8 AS score \
FROM fused f JOIN mnestic_memory m ON m.id = f.id \
ORDER BY score DESC, recorded_at DESC NULLS LAST, m.id \
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
