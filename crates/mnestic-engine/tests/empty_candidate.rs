// SPDX-License-Identifier: Apache-2.0

//! Dockerized test: empty-content candidates from the extractor must not abort ingest
//! or store an empty memory. A real embedder rejects an empty string in a batch; the
//! engine drops empty candidates before embedding. Three scenarios share one container:
//! blank+real, all-blank, and an embedder that errors on a blank input (the production
//! failure mode, reproduced without network).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Candidate, Ctx, Embedder, Error, Extractor, MemType, Result, Temporal};
use mnestic_engine::Engine;
use mnestic_model::MockEmbedder;
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

fn candidate(content: &str) -> Candidate {
    Candidate {
        content: content.to_string(),
        subject: None,
        attribute: None,
        value: None,
        single_valued: false,
        mem_type: MemType::Fact,
        confidence: 0.5,
        is_static: false,
        temporal: Temporal::None,
        forget_after: None,
    }
}

/// Returns a fixed candidate list, set per scenario.
struct ScriptedExtractor(Vec<&'static str>);

#[async_trait]
impl Extractor for ScriptedExtractor {
    async fn extract(&self, _text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        Ok(self.0.iter().map(|c| candidate(c)).collect())
    }
}

/// Mimics a provider (OpenAI) that rejects an empty string in a batch. Non-empty input
/// delegates to the mock so inserts still get valid embeddings.
struct RejectsEmptyEmbedder;

#[async_trait]
impl Embedder for RejectsEmptyEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.iter().any(|t| t.trim().is_empty()) {
            return Err(Error::Provider("input cannot be an empty string".into()));
        }
        MockEmbedder.embed(texts).await
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
async fn empty_candidates_are_dropped_before_embedding() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('ec') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let mock: Arc<dyn Embedder> = Arc::new(MockEmbedder);

    // Blank + real: only the real candidate is stored, ingest does not fail.
    let blank_then_real: Arc<dyn Extractor> =
        Arc::new(ScriptedExtractor(vec!["   ", "the user enjoys sailing"]));
    let e = Engine::new(Store::new(pool.clone()), mock.clone(), blank_then_real);
    let res = e.add(tenant, "mix", &[], "x", "conversation", None).await.expect("ingest ok");
    assert_eq!(res.inserted.len(), 1, "only the non-empty candidate is stored");
    let hits = e.recall(tenant, "mix", "sailing", 10).await.unwrap();
    let contents: Vec<String> = hits.iter().filter_map(|h| h.content.clone()).collect();
    assert_eq!(contents, vec!["the user enjoys sailing"], "no empty memory was stored");

    // All-blank: a clean no-op, not an error (the empty-batch branch).
    let all_blank: Arc<dyn Extractor> = Arc::new(ScriptedExtractor(vec!["  ", "\n\t"]));
    let e = Engine::new(Store::new(pool.clone()), mock.clone(), all_blank);
    let res = e.add(tenant, "blank", &[], "x", "conversation", None).await.expect("ingest ok");
    assert!(res.inserted.is_empty(), "no memories from an all-blank extraction");
    assert!(e.recall(tenant, "blank", "anything", 10).await.unwrap().is_empty());

    // An embedder that errors on a blank input still succeeds, because the engine drops
    // the blank before embedding. Guards against moving the filter after embed.
    let blank_then_real: Arc<dyn Extractor> =
        Arc::new(ScriptedExtractor(vec!["", "the user plays chess"]));
    let strict: Arc<dyn Embedder> = Arc::new(RejectsEmptyEmbedder);
    let e = Engine::new(Store::new(pool.clone()), strict, blank_then_real);
    let res = e.add(tenant, "strict", &[], "x", "conversation", None).await.expect("ingest ok");
    assert_eq!(res.inserted.len(), 1, "the blank never reached the strict embedder");
}
