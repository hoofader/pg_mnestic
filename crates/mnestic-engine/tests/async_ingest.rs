// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for the async (dreaming: dynamic) path: enqueue defers extraction, a
//! worker pass extracts and persists, idempotency holds, and the lease prevents two workers
//! from claiming the same source. Runs as a non-BYPASSRLS role so the queue works under RLS.

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

#[tokio::test]
async fn enqueue_then_worker_extracts() {
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

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(OneFact);
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);
    let store = Store::new(app_pool.clone());
    let tags: Vec<String> = Vec::new();

    // Enqueue defers extraction: the source exists but no memory yet.
    let enq = engine.enqueue(tenant, "u", &tags, "raw conversation text", "conversation", Some("c1")).await.unwrap();
    assert!(enq.queued, "first enqueue is queued");
    assert!(
        engine.recall(tenant, "u", "sailing", 10).await.unwrap().is_empty(),
        "nothing recallable before the worker runs"
    );

    // A worker pass extracts the pending source and the memory becomes recallable.
    let n = engine.process_pending(tenant, 300, 10).await.unwrap();
    assert_eq!(n, 1, "one source processed");
    assert!(
        !engine.recall(tenant, "u", "sailing", 10).await.unwrap().is_empty(),
        "memory recallable after extraction"
    );

    // Re-enqueuing the same custom_id is an idempotent skip, and there is nothing left to do.
    let again = engine.enqueue(tenant, "u", &tags, "raw conversation text", "conversation", Some("c1")).await.unwrap();
    assert!(!again.queued, "duplicate custom_id is not re-queued");
    assert_eq!(engine.process_pending(tenant, 300, 10).await.unwrap(), 0, "no pending work remains");

    // The lease keeps two workers off the same source: a fresh claim leases it, and an
    // immediate second claim (lease not expired) sees nothing.
    engine.enqueue(tenant, "u", &tags, "another text", "conversation", Some("c2")).await.unwrap();
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
