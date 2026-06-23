// SPDX-License-Identifier: MIT

//! Dockerized test for the GDPR paths: export_actor returns a subject's data as JSON, and
//! purge_actor hard-deletes it across every table without touching another actor.

use std::sync::Arc;
use std::time::Duration;

use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

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
async fn export_then_purge_actor() {
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
    let opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let pool = connect(opts).await;
    run_migrations(&pool).await.expect("migrations");

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('gdpr') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool.clone()), embedder, extractor);
    let store = Store::new(pool.clone());

    // Two actors with data, so we can prove the purge is scoped to one of them.
    engine.add(tenant, "user:1", &[], "I live in Lisbon and love climbing", "conversation", None)
        .await
        .expect("add user:1 memory");
    engine
        .ingest_document(tenant, "user:1", &[], Some("Notes"), None, "a reference document for user one", Some("d1"), &serde_json::json!({}))
        .await
        .expect("ingest user:1 document");
    engine.add(tenant, "user:2", &[], "I work at Globex", "conversation", None)
        .await
        .expect("add user:2 memory");

    // Export carries the subject's content before erasure.
    let export = store.export_actor(tenant, "user:1").await.expect("export user:1");
    let doc: serde_json::Value = serde_json::from_str(&export).expect("export is json");
    assert_eq!(doc["actor_id"], "user:1");
    assert!(!doc["memories"].as_array().unwrap().is_empty(), "export has memories: {export}");
    assert_eq!(doc["documents"].as_array().unwrap().len(), 1, "export has the document");
    assert!(!doc["chunks"].as_array().unwrap().is_empty(), "export has chunks before purge");
    assert!(!doc["sources"].as_array().unwrap().is_empty(), "export has sources");
    // The export carries the actual ingested text, not just row counts.
    assert!(export.contains("Lisbon"), "export carries the memory content: {export}");
    assert!(export.contains("reference document"), "export carries the document content");

    // Erase user:1.
    let counts = store.purge_actor(tenant, "user:1").await.expect("purge user:1");
    assert!(counts.memories >= 1, "memories deleted, got {counts:?}");
    assert_eq!(counts.documents, 1, "document deleted");
    assert!(counts.sources >= 1, "sources deleted");

    // user:1 is gone from every table; the export is now empty.
    let after = store.export_actor(tenant, "user:1").await.expect("export after purge");
    let after_doc: serde_json::Value = serde_json::from_str(&after).expect("json");
    assert!(after_doc["memories"].as_array().unwrap().is_empty(), "no memories left");
    assert!(after_doc["documents"].as_array().unwrap().is_empty(), "no documents left");
    assert!(after_doc["chunks"].as_array().unwrap().is_empty(), "no chunks left");
    assert!(after_doc["sources"].as_array().unwrap().is_empty(), "no sources left");
    assert!(after_doc["profile"].is_null(), "no profile left");

    // user:2 is untouched (tenant-scoped erasure must not over-delete).
    let other = store.export_actor(tenant, "user:2").await.expect("export user:2");
    let other_doc: serde_json::Value = serde_json::from_str(&other).expect("json");
    assert!(!other_doc["memories"].as_array().unwrap().is_empty(), "user:2 memory survives");

    // Re-purging an already-erased actor is a harmless no-op (all counts zero).
    let again = store.purge_actor(tenant, "user:1").await.expect("re-purge");
    assert_eq!(again.memories, 0);
    assert_eq!(again.sources, 0);
}
