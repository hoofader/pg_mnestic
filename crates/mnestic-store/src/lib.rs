// SPDX-License-Identifier: Apache-2.0

//! Postgres access over sqlx. Runtime query functions only (no compile-time
//! macros), so the build needs no DATABASE_URL.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, sqlx::Error>;

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
