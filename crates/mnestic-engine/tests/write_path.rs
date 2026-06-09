// SPDX-License-Identifier: Apache-2.0

//! Dockerized write-path test against pgvector/pgvector:pg16. Exercises Engine::add
//! end to end as a non-superuser, covering supersession in event-time order,
//! out-of-order (late-arriving) inserts, multi-valued coexistence, and dedup.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Temporal};
use mnestic_engine::Engine;
use mnestic_model::MockEmbedder;
use mnestic_store::Store;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Returns its scripted candidate batches in order, one batch per `extract` call,
/// so a test drives exactly what each `add` resolves.
struct ScriptExtractor {
    batches: Mutex<VecDeque<Vec<Candidate>>>,
}

impl ScriptExtractor {
    fn new(batches: Vec<Vec<Candidate>>) -> Self {
        Self {
            batches: Mutex::new(batches.into()),
        }
    }
}

#[async_trait]
impl Extractor for ScriptExtractor {
    async fn extract(&self, _text: &str, _ctx: &Ctx) -> mnestic_core::Result<Vec<Candidate>> {
        Ok(self.batches.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().expect("rfc3339 timestamp")
}

fn fact(content: &str, attribute: &str, value: &str, single_valued: bool, at: Option<DateTime<Utc>>) -> Candidate {
    base(content, attribute, value, single_valued, match at {
        Some(t) => Temporal::AsOf { timestamp: t },
        None => Temporal::None,
    })
}

fn fact_range(content: &str, attribute: &str, value: &str, single_valued: bool, from: DateTime<Utc>, to: DateTime<Utc>) -> Candidate {
    base(content, attribute, value, single_valued, Temporal::Range { from: Some(from), to: Some(to) })
}

fn base(content: &str, attribute: &str, value: &str, single_valued: bool, temporal: Temporal) -> Candidate {
    Candidate {
        content: content.to_string(),
        subject: Some("user".to_string()),
        attribute: Some(attribute.to_string()),
        value: Some(value.to_string()),
        single_valued,
        mem_type: MemType::Fact,
        confidence: 0.8,
        is_static: false,
        temporal,
        forget_after: None,
    }
}

async fn connect(opts: PgConnectOptions) -> PgPool {
    let mut last_err = None;
    for _ in 0..30 {
        match PgPoolOptions::new().max_connections(5).connect_with(opts.clone()).await {
            Ok(pool) => return pool,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to postgres: {last_err:?}");
}

/// Count rows under tenant A's RLS scope for the given predicate.
async fn count(pool: &PgPool, tenant: Uuid, predicate: &str) -> i64 {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('mnestic.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let sql = format!("SELECT count(*) FROM mnestic_memory WHERE {predicate}");
    let n: i64 = sqlx::query_scalar(&sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn latest_location(pool: &PgPool, tenant: Uuid, actor: &str) -> Option<String> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('mnestic.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let v: Option<String> = sqlx::query_scalar(
        "SELECT value FROM mnestic_memory \
         WHERE actor_id = $1 AND attribute = 'location' AND single_valued \
           AND is_latest AND status = 'active'",
    )
    .bind(actor)
    .fetch_optional(&mut *tx)
    .await
    .unwrap()
    .flatten();
    tx.commit().await.unwrap();
    v
}

#[tokio::test]
async fn write_path_resolution_and_temporal_order() {
    let container = GenericImage::new("pgvector/pgvector", "pg16")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await
        .expect("start pgvector container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432.tcp()).await.expect("mapped port");

    let su_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let su_pool = connect(su_opts).await;
    mnestic_store::run_migrations(&su_pool).await.expect("migrations");

    for stmt in [
        "CREATE ROLE mnestic_app LOGIN PASSWORD 'app' NOBYPASSRLS",
        "GRANT USAGE ON SCHEMA public TO mnestic_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO mnestic_app",
    ] {
        sqlx::query(stmt).execute(&su_pool).await.unwrap_or_else(|e| panic!("setup [{stmt}]: {e}"));
    }

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('A') RETURNING id")
            .fetch_one(&su_pool)
            .await
            .expect("tenant");

    let app_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("mnestic_app")
        .password("app")
        .database("postgres");
    let app_pool = connect(app_opts).await;

    let t0 = ts("2025-01-01T00:00:00Z");
    let t1 = ts("2026-01-01T00:00:00Z");
    let t2 = ts("2026-06-01T00:00:00Z");
    let t3 = ts("2026-12-01T00:00:00Z");

    let extractor = ScriptExtractor::new(vec![
        vec![fact("user lives in NYC", "location", "NYC", true, Some(t1))],
        vec![fact("user lives in SF", "location", "SF", true, Some(t2))],
        vec![fact("user once lived in LA", "location", "LA", true, Some(t0))],
        vec![
            fact("user speaks English", "language", "English", false, None),
            fact("user speaks French", "language", "French", false, None),
        ],
        vec![fact("user lives in SF", "location", "SF", true, Some(t3))],
        vec![fact("user uses Rust", "tool", "Rust", false, None)],
        vec![fact("user uses Rust", "tool", "Rust", false, None)],
        vec![fact_range("user role is staff", "role", "staff", true, t2, t1)],
        vec![fact("user lives in NYC", "lives in", "NYC", true, Some(t1))],
        vec![fact("user current city is SF", "current city", "SF", true, Some(t2))],
        vec![fact("just some noise", "?", "x", true, None)],
    ]);

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(extractor);
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);

    let tags: Vec<String> = Vec::new();

    // 1. First fact inserts cleanly.
    let r = engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(latest_location(&app_pool, tenant, "user").await.as_deref(), Some("NYC"));

    // 2. A newer contradicting fact supersedes the prior.
    let r = engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.superseded.len(), 1, "NYC should be superseded");
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(latest_location(&app_pool, tenant, "user").await.as_deref(), Some("SF"));
    assert_eq!(
        count(&app_pool, tenant, "subject='user' AND attribute='location' AND is_latest AND status='active'").await,
        1
    );

    // 3. An older fact arriving late is recorded as history, not promoted to latest.
    let r = engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    assert!(r.superseded.is_empty(), "a late, older fact must not supersede the current latest");
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(latest_location(&app_pool, tenant, "user").await.as_deref(), Some("SF"));
    assert_eq!(
        count(&app_pool, tenant, "subject='user' AND attribute='location'").await,
        3,
        "NYC (superseded), SF (latest), LA (history)"
    );
    assert_eq!(
        count(&app_pool, tenant, "value='LA' AND NOT is_latest AND status='active'").await,
        1
    );
    // As-of read: exactly one location is valid at 2026-03, proving the late row was
    // placed without overlapping the intervening segment.
    assert_eq!(
        count(
            &app_pool,
            tenant,
            "subject='user' AND attribute='location' \
             AND valid_time @> '2026-03-01T00:00:00Z'::timestamptz",
        )
        .await,
        1
    );

    // 4. Multi-valued facts coexist (regression guard for a global EXCLUDE).
    let r = engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.inserted.len(), 2);
    assert_eq!(
        count(&app_pool, tenant, "attribute='language' AND is_latest AND status='active'").await,
        2
    );

    // 5. An identical fact dedups instead of inserting a new row.
    let r = engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.deduped.len(), 1, "repeating the current value should dedup");
    assert!(r.inserted.is_empty());
    assert_eq!(
        count(&app_pool, tenant, "subject='user' AND attribute='location'").await,
        3,
        "dedup adds no row"
    );

    // 6. Idempotency: re-adding the same custom_id skips the pipeline.
    let r = engine
        .add(tenant, "user", &tags, "ignored", "conversation", Some("evt-1"))
        .await
        .unwrap();
    assert_eq!(r.inserted.len(), 1);
    assert!(!r.idempotent_skip);
    let r = engine
        .add(tenant, "user", &tags, "ignored", "conversation", Some("evt-1"))
        .await
        .unwrap();
    assert!(r.idempotent_skip, "second add with the same custom_id is a no-op");
    assert!(r.inserted.is_empty());
    assert_eq!(count(&app_pool, tenant, "attribute='tool'").await, 1);

    // 7. A garbled range (to < from) is sanitized to open-ended, not aborted.
    let r = engine
        .add(tenant, "user", &tags, "ignored", "conversation", None)
        .await
        .unwrap();
    assert_eq!(r.inserted.len(), 1, "inverted range must not abort the write");
    assert_eq!(
        count(&app_pool, tenant, "attribute='role' AND upper_inf(valid_time)").await,
        1,
        "sanitized range is open-ended"
    );

    // 8. Attribute normalization: "lives in" then "current city" (different surface
    // forms) collapse to the canonical "location" key, so the second supersedes the
    // first. Uses actor user2 so it does not collide with user's location facts.
    let r = engine.add(tenant, "user2", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.inserted.len(), 1);
    let r = engine.add(tenant, "user2", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.superseded.len(), 1, "current city should supersede lives in via the canonical key");
    assert_eq!(latest_location(&app_pool, tenant, "user2").await.as_deref(), Some("SF"));
    assert_eq!(
        count(&app_pool, tenant, "actor_id='user2' AND attribute IN ('lives in','current city')").await,
        0,
        "surface attributes are stored canonically, not verbatim"
    );

    // 9. An attribute that normalizes to empty (punctuation only) is stored as
    // unstructured content with a NULL key, not an empty-string triple, and the
    // single-valued flag is dropped so the write does not abort.
    let r = engine.add(tenant, "user3", &tags, "ignored", "conversation", None).await.unwrap();
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(
        count(&app_pool, tenant, "actor_id='user3' AND attribute IS NULL AND NOT single_valued").await,
        1,
        "a punctuation-only attribute is dropped, not stored empty"
    );
}
