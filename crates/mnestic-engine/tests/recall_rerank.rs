// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for the recall reranker and query-rewriter hooks. Uses scripted
//! mocks: an alphabetical reranker (so its ordering is distinct from the SQL order)
//! and a fixed rewriter (so its effect on retrieval is observable).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Embedder, Extractor, QueryRewriter, Reranker, Result, Scored};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Orders candidates alphabetically by content, so the final order is the reranker's,
/// not the SQL ranking's.
struct AlphabeticalReranker;

#[async_trait]
impl Reranker for AlphabeticalReranker {
    async fn rerank(&self, _query: &str, candidates: &[String]) -> Result<Vec<Scored>> {
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by(|&a, &b| candidates[a].cmp(&candidates[b]));
        let n = candidates.len();
        Ok(order
            .into_iter()
            .enumerate()
            .map(|(rank, i)| Scored {
                index: i,
                content: candidates[i].clone(),
                score: (n - rank) as f32,
            })
            .collect())
    }
}

/// Rewrites any query to a fixed string.
struct FixedRewriter(String);

#[async_trait]
impl QueryRewriter for FixedRewriter {
    async fn rewrite(&self, _query: &str) -> Result<String> {
        Ok(self.0.clone())
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
async fn reranker_orders_results_and_rewriter_feeds_retrieval() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rk') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let tags: Vec<String> = Vec::new();
    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);

    // Reranker: the final order is alphabetical regardless of how SQL ranked the pool.
    let ranked = Engine::new(Store::new(pool.clone()), embedder.clone(), extractor.clone())
        .with_reranker(Arc::new(AlphabeticalReranker));
    for content in ["gamma fact", "alpha fact", "beta fact"] {
        ranked.add(tenant, "ranked", &tags, content, "conversation", None).await.unwrap();
    }
    let order: Vec<String> = ranked
        .recall(tenant, "ranked", "fact", 10)
        .await
        .unwrap()
        .iter()
        .filter_map(|h| h.content.clone())
        .collect();
    assert_eq!(order, vec!["alpha fact", "beta fact", "gamma fact"], "reranker controls order");

    // Rewriter: the result is decided by a deterministic lexical match. The raw query
    // "raw" matches the decoy memory; the rewriter rewrites it to "zebra", which
    // matches the other memory. Comparing the same actor with and without the rewriter
    // proves the rewrite drives retrieval (not a hash coincidence).
    let plain = Engine::new(Store::new(pool.clone()), embedder.clone(), extractor.clone());
    for content in ["the raw signal", "the zebra pattern"] {
        plain.add(tenant, "rewritten", &tags, content, "conversation", None).await.unwrap();
    }
    let without = plain.recall(tenant, "rewritten", "raw", 10).await.unwrap();
    assert_eq!(
        without.first().and_then(|h| h.content.clone()).as_deref(),
        Some("the raw signal"),
        "without the rewriter, the raw query matches the decoy memory"
    );

    let rewritten = Engine::new(Store::new(pool.clone()), embedder.clone(), extractor.clone())
        .with_rewriter(Arc::new(FixedRewriter("zebra".to_string())));
    let with = rewritten.recall(tenant, "rewritten", "raw", 10).await.unwrap();
    assert_eq!(
        with.first().and_then(|h| h.content.clone()).as_deref(),
        Some("the zebra pattern"),
        "the rewriter redirects retrieval to the zebra memory"
    );
}
