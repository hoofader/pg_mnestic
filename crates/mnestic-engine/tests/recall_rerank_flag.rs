// SPDX-License-Identifier: MIT

//! Dockerized test that the per-request `rerank` flag gates reranking. A reranker that
//! reverses the SQL order makes its effect observable: with `rerank=true` recall returns
//! the reranker's order, with `rerank=false` it returns the engine's own order even though
//! a reranker is configured.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Embedder, Extractor, Reranker, Result, Scored};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Reverses the candidate order, so the reranked result is distinguishable from any
/// engine ordering of two distinct candidates.
struct ReversingReranker;

#[async_trait]
impl Reranker for ReversingReranker {
    async fn rerank(&self, _query: &str, candidates: &[String]) -> Result<Vec<Scored>> {
        let n = candidates.len();
        Ok((0..n)
            .rev()
            .enumerate()
            .map(|(rank, i)| Scored {
                index: i,
                content: candidates[i].clone(),
                score: (n - rank) as f32,
            })
            .collect())
    }
}

/// Always errors, standing in for a down or flaky reranker sidecar.
struct FailingReranker;

#[async_trait]
impl Reranker for FailingReranker {
    async fn rerank(&self, _query: &str, _candidates: &[String]) -> Result<Vec<Scored>> {
        Err(mnestic_core::Error::Provider("reranker unavailable".into()))
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
async fn rerank_flag_gates_reranking() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rf') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor)
        .with_reranker(Arc::new(ReversingReranker));

    let tags: Vec<String> = Vec::new();
    for content in ["alpha fact", "beta fact"] {
        engine.add(tenant, "u", &tags, content, "conversation", None).await.unwrap();
    }

    let contents = |hits: &[mnestic_engine::RecallHit]| -> Vec<String> {
        hits.iter().filter_map(|h| h.content.clone()).collect()
    };

    // The engine's own order, with reranking off, is the baseline to reverse against.
    let off = engine
        .recall_scoped(tenant, "u", &tags, "fact", 10, None, false, false)
        .await
        .unwrap();
    let off = contents(&off);
    assert_eq!(off.len(), 2, "both memories recalled with rerank off");

    // With reranking on, the reversing reranker flips that order.
    let on = engine
        .recall_scoped(tenant, "u", &tags, "fact", 10, None, false, true)
        .await
        .unwrap();
    let on = contents(&on);
    assert_eq!(on.len(), 2, "both memories recalled with rerank on");

    let mut reversed_off = off.clone();
    reversed_off.reverse();
    assert_eq!(on, reversed_off, "rerank=true applies the reranker, reversing the engine order");
    assert_ne!(on, off, "the two flag values yield different orders");
}

#[tokio::test]
async fn reranker_failure_falls_back_to_retrieval_order() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rfail') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor)
        .with_reranker(Arc::new(FailingReranker));

    let tags: Vec<String> = Vec::new();
    for content in ["alpha fact", "beta fact"] {
        engine.add(tenant, "u", &tags, content, "conversation", None).await.unwrap();
    }

    // A reranker error must not fail recall: it degrades to the retrieval order, still
    // returning the memories rather than propagating the error.
    let hits = engine
        .recall_scoped(tenant, "u", &tags, "fact", 10, None, false, true)
        .await
        .expect("recall succeeds despite the reranker failing");
    let contents: Vec<String> = hits.iter().filter_map(|h| h.content.clone()).collect();
    assert_eq!(contents.len(), 2, "both memories are returned on the fallback path");
}
