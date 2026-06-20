// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for the document/RAG path: ingest chunks and embeds a document,
//! search returns the chunk that lexically matches the query, ingestion is idempotent
//! on custom_id, and search is scoped to the owning actor.

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
async fn ingest_and_search_documents() {
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

    let opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let pool = connect(opts).await;
    run_migrations(&pool).await.expect("migrations");

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('doc') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    // A document long enough to split into several chunks, with a unique phrase that
    // appears in exactly one chunk so the lexical signal pins the right one. The mock
    // embedder is hash-based, so retrieval here rides on the deterministic tsvector.
    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(40);
    let content = format!("{filler} The mitochondria powerhouse note is unique here. {filler}");

    let res = engine
        .ingest_document(tenant, "u", &tags, Some("Cell Biology"), Some("file://cells"), &content, Some("doc1"), &serde_json::json!({}))
        .await
        .unwrap();
    assert!(!res.idempotent_skip, "first ingest is not a skip");
    assert!(res.chunk_ids.len() > 1, "the document split into multiple chunks");

    // The chunk carrying the unique phrase ranks first for that query.
    let hits = engine.search_documents(tenant, "u", &tags, "mitochondria powerhouse", 5).await.unwrap();
    assert!(!hits.is_empty(), "search returns chunks");
    assert!(
        hits[0].content.contains("mitochondria"),
        "the lexically matching chunk ranks first, got {:?}",
        hits[0].content
    );
    assert_eq!(hits[0].document_id, res.document_id, "hit points back to the document");

    // Idempotent on custom_id: a repeat writes no new document or chunks.
    let again = engine
        .ingest_document(tenant, "u", &tags, Some("Cell Biology"), None, &content, Some("doc1"), &serde_json::json!({}))
        .await
        .unwrap();
    assert!(again.idempotent_skip, "repeat ingest is a skip");
    assert!(again.chunk_ids.is_empty(), "no new chunks on a skip");
    let after = engine.search_documents(tenant, "u", &tags, "mitochondria powerhouse", 50).await.unwrap();
    let matching = after.iter().filter(|h| h.content.contains("mitochondria")).count();
    assert_eq!(matching, 1, "the unique phrase was not duplicated by the repeat ingest");

    // Search is scoped to the owning actor.
    let other = engine.search_documents(tenant, "v", &tags, "mitochondria powerhouse", 5).await.unwrap();
    assert!(other.is_empty(), "another actor sees none of u's chunks");

    // Empty content is rejected, not stored as a chunk-less document.
    assert!(
        engine.ingest_document(tenant, "u", &tags, None, None, "   \n ", Some("empty"), &serde_json::json!({})).await.is_err(),
        "empty content is rejected"
    );
}
