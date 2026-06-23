// SPDX-License-Identifier: MIT

//! Dockerized test for the post-commit relation pass: with a classifier configured,
//! adding a second same-subject memory yields an `extends` edge to the first. The
//! `MockRelationClassifier` is deterministic (it marks the first candidate as an
//! `extends`), and the extractor below pins a stable subject so the two memories group.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Result, Temporal};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockRelationClassifier};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Emits one multi-valued (subject "user") memory whose content is the input text, so
/// successive adds share a subject and coexist (no supersession).
struct SameSubjectExtractor;

#[async_trait]
impl Extractor for SameSubjectExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            content: text.to_string(),
            subject: Some("user".to_string()),
            attribute: None,
            value: None,
            single_valued: false,
            mem_type: MemType::Fact,
            confidence: 0.9,
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
async fn second_same_subject_add_creates_extends_edge() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rel') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(SameSubjectExtractor);
    let engine = Engine::new(Store::new(pool.clone()), embedder, extractor)
        .with_classifier(Arc::new(MockRelationClassifier));

    // First add has no same-subject neighbor, so no edge forms.
    let first = engine
        .add(tenant, "user", &[], "user adopted a dog", "conversation", None)
        .await
        .expect("first add");
    let first_id = first.inserted[0];
    assert!(
        engine.store().relation_edges_for(tenant, "user", first_id).await.unwrap().is_empty(),
        "no edge after the first memory"
    );

    // Second add finds the first as a same-subject neighbor; the mock classifier marks it
    // as an `extends`, so the second memory extends the first.
    let second = engine
        .add(tenant, "user", &[], "user has a pet", "conversation", None)
        .await
        .expect("second add");
    let second_id = second.inserted[0];

    let edges = engine.store().relation_edges_for(tenant, "user", second_id).await.unwrap();
    assert_eq!(edges.len(), 1, "the second memory has one relation edge, got {edges:?}");
    assert_eq!(edges[0].relation, "extends");
    assert!(edges[0].outgoing, "the second memory extends FROM the first (outgoing)");
    assert_eq!(edges[0].neighbor_content.as_deref(), Some("user adopted a dog"));
}

#[tokio::test]
async fn worker_classifies_relations_on_async_ingest() {
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

    let tenant: Uuid = sqlx::query_scalar(
        "INSERT INTO mnestic_tenant (external_id) VALUES ('relasync') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(SameSubjectExtractor);
    let engine = Engine::new(Store::new(pool.clone()), embedder, extractor)
        .with_classifier(Arc::new(MockRelationClassifier));

    let tags: Vec<String> = Vec::new();
    let meta = serde_json::json!({});

    // Enqueue and drain the first source. The worker extracts and commits it; with no
    // same-subject neighbor yet, no edge forms.
    engine
        .enqueue(tenant, "user", &tags, "user adopted a dog", "conversation", Some("c1"), &meta)
        .await
        .expect("enqueue 1");
    assert_eq!(engine.process_pending(tenant, 300, 10).await.unwrap(), 1, "first source drained");

    let first_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM mnestic_memory WHERE tenant_id=$1 AND actor_id='user' \
         AND content='user adopted a dog'",
    )
    .bind(tenant)
    .fetch_one(&pool)
    .await
    .expect("first memory id");
    assert!(
        engine.store().relation_edges_for(tenant, "user", first_id).await.unwrap().is_empty(),
        "no edge after the first async memory"
    );

    // Enqueue and drain the second. The worker classifies it post-commit against the first,
    // exactly as the sync path does, so the async memory also gets an edge.
    engine
        .enqueue(tenant, "user", &tags, "user has a pet", "conversation", Some("c2"), &meta)
        .await
        .expect("enqueue 2");
    assert_eq!(engine.process_pending(tenant, 300, 10).await.unwrap(), 1, "second source drained");

    let edges = engine.store().relation_edges_for(tenant, "user", first_id).await.unwrap();
    assert_eq!(edges.len(), 1, "the worker created a relation edge, got {edges:?}");
    assert_eq!(edges[0].relation, "extends");
    assert!(!edges[0].outgoing, "the first memory is extended BY the second (incoming)");
    assert_eq!(edges[0].neighbor_content.as_deref(), Some("user has a pet"));
}
