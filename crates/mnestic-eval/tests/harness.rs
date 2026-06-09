// SPDX-License-Identifier: Apache-2.0

//! Proves the eval harness wiring end to end on real Postgres with mock providers:
//! ingest a case, recall, answer, judge, and compute a MemScore. Real-model runs go
//! through the `memorybench` binary (the `real` feature) and are not exercised here.

use std::sync::Arc;
use std::time::Duration;

use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_eval::mock::{EchoAnswerer, SubstringJudge};
use mnestic_eval::{run_eval, Case, Qa, Turn};
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
async fn harness_ingests_recalls_answers_and_scores() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('eval') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);

    let cases = vec![Case {
        id: "c1".to_string(),
        sessions: vec![vec![Turn {
            role: "user".to_string(),
            content: "The user lives in San Francisco.".to_string(),
        }]],
        questions: vec![Qa {
            question: "Where does the user live?".to_string(),
            answer: "San Francisco".to_string(),
        }],
    }];

    let report = run_eval(&engine, tenant, &EchoAnswerer, &SubstringJudge, 10, &cases).await;

    assert!(report.errors.is_empty(), "unexpected errors: {:?}", report.errors);
    assert_eq!(report.score.n, 1);
    assert!(
        report.results[0].correct,
        "the SF memory should be recalled and graded correct"
    );
    assert!((report.score.accuracy - 1.0).abs() < 1e-9);
}
