// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for query-scoped profile: profile_query returns the cached
//! profile plus the query-relevant memories, and a blank query returns the profile
//! with no recall.

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
async fn profile_query_returns_profile_and_relevant() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('pq') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    engine.add(tenant, "u", &tags, "the user likes hiking", "conversation", None).await.unwrap();
    engine.add(tenant, "u", &tags, "the user drives a sedan", "conversation", None).await.unwrap();

    // A query returns the profile and the relevant memory.
    let ctx = engine.profile_query(tenant, "u", &tags, "hiking", 5).await.unwrap();
    assert!(!ctx.profile.dynamic_ctx.is_empty(), "profile is populated");
    assert!(
        ctx.relevant.iter().any(|h| h.content.as_deref() == Some("the user likes hiking")),
        "the hiking memory is recalled for the hiking query"
    );

    // A blank query returns the profile with no recall.
    let bare = engine.profile_query(tenant, "u", &tags, "   ", 5).await.unwrap();
    assert!(!bare.profile.dynamic_ctx.is_empty(), "profile still returned for a blank query");
    assert!(bare.relevant.is_empty(), "no recall runs for a blank query");
}
