// SPDX-License-Identifier: MIT

//! Dockerized test for the async (dreaming: dynamic) path: enqueue defers extraction, a
//! worker pass extracts and persists, idempotency holds, and the lease prevents two workers
//! from claiming the same source. Runs as a non-BYPASSRLS role so the queue works under RLS.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Temporal};
use mnestic_engine::Engine;
use mnestic_model::MockEmbedder;
use mnestic_store::Store;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Always extracts one unstructured fact, so a claimed source yields a recallable memory.
struct OneFact;

#[async_trait]
impl Extractor for OneFact {
    async fn extract(&self, _text: &str, _ctx: &Ctx) -> mnestic_core::Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            content: "the user enjoys sailing".to_string(),
            subject: None,
            attribute: None,
            value: None,
            single_valued: false,
            mem_type: MemType::Fact,
            confidence: 0.8,
            is_static: false,
            temporal: Temporal::None,
            forget_after: None,
        }])
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

/// Extractor that fails its first call and succeeds after, so a worker batch contains one
/// poison source and one good one.
struct FailFirst {
    calls: AtomicU32,
}

#[async_trait]
impl Extractor for FailFirst {
    async fn extract(&self, _text: &str, _ctx: &Ctx) -> mnestic_core::Result<Vec<Candidate>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(mnestic_core::Error::Extraction("boom".into()));
        }
        Ok(vec![Candidate {
            content: "the user enjoys sailing".to_string(),
            subject: None,
            attribute: None,
            value: None,
            single_valued: false,
            mem_type: MemType::Fact,
            confidence: 0.8,
            is_static: false,
            temporal: Temporal::None,
            forget_after: None,
        }])
    }
}

/// Start pgvector, migrate, create a non-BYPASSRLS app role and a tenant. Returns the
/// container (kept alive by the caller), the app-role pool, and the tenant id.
async fn setup() -> (testcontainers::ContainerAsync<GenericImage>, PgPool, Uuid) {
    let container = GenericImage::new("mnestic-pg", "dev")
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
    (container, app_pool, tenant)
}

#[tokio::test]
async fn enqueue_then_worker_extracts() {
    let (_container, app_pool, tenant) = setup().await;

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(OneFact);
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);
    let store = Store::new(app_pool.clone());
    let tags: Vec<String> = Vec::new();

    // Enqueue defers extraction: the source exists but no memory yet. The metadata rides on the
    // source so the worker can tag the memory it extracts.
    let meta = serde_json::json!({"team": "infra"});
    let enq = engine.enqueue(tenant, "u", &tags, "raw conversation text about sailing", "conversation", Some("c1"), &meta).await.unwrap();
    assert!(enq.queued, "first enqueue is queued");
    assert!(
        engine.recall(tenant, "u", "sailing", 10).await.unwrap().is_empty(),
        "nothing recallable before the worker runs"
    );

    // A worker pass extracts the pending source and the memory becomes recallable.
    let n = engine.process_pending(tenant, 300, 10).await.unwrap();
    assert_eq!(n, 1, "one source processed");
    let hits = engine.recall(tenant, "u", "sailing", 10).await.unwrap();
    assert!(!hits.is_empty(), "memory recallable after extraction");
    // The metadata the request enqueued round-trips through the worker onto the memory.
    assert_eq!(
        hits[0].metadata,
        serde_json::json!({"team": "infra"}),
        "the worker tagged the extracted memory with the enqueued metadata"
    );

    // Re-enqueuing the same custom_id is an idempotent skip, and there is nothing left to do.
    let again = engine.enqueue(tenant, "u", &tags, "raw conversation text about sailing", "conversation", Some("c1"), &meta).await.unwrap();
    assert!(!again.queued, "duplicate custom_id is not re-queued");
    assert_eq!(engine.process_pending(tenant, 300, 10).await.unwrap(), 0, "no pending work remains");

    // The lease keeps two workers off the same source: a fresh claim leases it, and an
    // immediate second claim (lease not expired) sees nothing.
    engine.enqueue(tenant, "u", &tags, "another text", "conversation", Some("c2"), &serde_json::json!({})).await.unwrap();
    let claim_a = store.claim_pending_source(tenant, 300).await.unwrap().expect("c2 claimable");
    assert!(
        store.claim_pending_source(tenant, 300).await.unwrap().is_none(),
        "a leased source is not claimed again until the lease lapses"
    );

    // Once the lease lapses (modeled with a zero lease), another worker reclaims the same
    // source with a fresh stamp.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let claim_b = store.claim_pending_source(tenant, 0).await.unwrap().expect("c2 reclaimable after lease");
    assert_eq!(claim_a.id, claim_b.id, "same source reclaimed");
    assert_ne!(claim_a.claimed_at, claim_b.claimed_at, "reclaim restamps the lease");

    // The superseded claim (worker A) cannot mark the source done; only the current holder
    // (worker B) can. This is what stops a slow worker from double-writing after a reclaim.
    let mut tx = store.begin_tenant(tenant).await.unwrap();
    let stale = Store::mark_source_extracted_tx(&mut tx, tenant, claim_a.id, claim_a.claimed_at).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!stale, "a superseded claim cannot mark the source extracted");

    let mut tx = store.begin_tenant(tenant).await.unwrap();
    let current = Store::mark_source_extracted_tx(&mut tx, tenant, claim_b.id, claim_b.claimed_at).await.unwrap();
    tx.commit().await.unwrap();
    assert!(current, "the current claim holder marks it extracted");
    assert!(
        store.claim_pending_source(tenant, 0).await.unwrap().is_none(),
        "a marked-done source is no longer pending"
    );
}

#[tokio::test]
async fn one_poison_source_does_not_block_the_batch() {
    let (_container, app_pool, tenant) = setup().await;

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(FailFirst { calls: AtomicU32::new(0) });
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);
    let store = Store::new(app_pool);
    let tags: Vec<String> = Vec::new();

    // Two queued sources; extraction fails on the first claimed one (oldest), succeeds on the
    // next. The batch keeps going past the failure and commits the good one.
    engine.enqueue(tenant, "u", &tags, "first", "conversation", Some("p1"), &serde_json::json!({})).await.unwrap();
    engine.enqueue(tenant, "u", &tags, "second", "conversation", Some("p2"), &serde_json::json!({})).await.unwrap();
    let processed = engine.process_pending(tenant, 300, 10).await.unwrap();
    assert_eq!(processed, 1, "the good source committed despite the poison one");
    assert!(
        !engine.recall(tenant, "u", "sailing", 10).await.unwrap().is_empty(),
        "the good source is recallable"
    );

    // The failed source stayed leased for the batch (so it could not head-of-line), and is
    // still pending for a later retry once its lease lapses.
    assert!(
        store.claim_pending_source(tenant, 0).await.unwrap().is_some(),
        "the poison source remains pending after its lease lapses"
    );
}
