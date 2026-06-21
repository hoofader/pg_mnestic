// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for the container_tags recall filter: the same actor holds
//! memories under different containers, and a scoped recall returns only the
//! memories carrying all requested tags, while an unscoped recall returns both.

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
async fn container_filter_scopes_recall() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('ct') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);

    // Same actor, same query word ("fact"), different containers.
    engine
        .add(tenant, "u", &["proj-a".to_string()], "alpha fact", "conversation", None)
        .await
        .unwrap();
    engine
        .add(tenant, "u", &["proj-b".to_string()], "beta fact", "conversation", None)
        .await
        .unwrap();

    let contents = |hits: &[mnestic_engine::RecallHit]| -> Vec<String> {
        hits.iter().filter_map(|h| h.content.clone()).collect()
    };

    // Unscoped: both memories are in scope.
    let all = engine.recall(tenant, "u", "fact", 10).await.unwrap();
    let mut all = contents(&all);
    all.sort();
    assert_eq!(all, vec!["alpha fact", "beta fact"], "no filter returns both");

    // Scoped to proj-a: only the proj-a memory.
    let a = engine
        .recall_scoped(tenant, "u", &["proj-a".to_string()], "fact", 10)
        .await
        .unwrap();
    assert_eq!(contents(&a), vec!["alpha fact"], "proj-a filter returns only proj-a");

    // Scoped to proj-b: only the proj-b memory.
    let b = engine
        .recall_scoped(tenant, "u", &["proj-b".to_string()], "fact", 10)
        .await
        .unwrap();
    assert_eq!(contents(&b), vec!["beta fact"], "proj-b filter returns only proj-b");

    // Containment: a tag the memory does not carry returns nothing.
    let none = engine
        .recall_scoped(tenant, "u", &["proj-a".to_string(), "proj-b".to_string()], "fact", 10)
        .await
        .unwrap();
    assert!(none.is_empty(), "requiring both tags matches neither memory");
}
