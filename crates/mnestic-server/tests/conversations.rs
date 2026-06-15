// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for POST /v4/conversations: ingest a multi-message conversation once,
//! recall the stored memory, and re-post the same conversationId as an idempotent skip.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_server::{app, AppState};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tower::ServiceExt;
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

fn post(token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v4/conversations")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ingest_conversation_endpoint() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('conv') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");
    sqlx::query("INSERT INTO mnestic_api_key (token_sha256, tenant_id) VALUES (digest($1, 'sha256'), $2)")
        .bind("sk-test")
        .bind(tenant)
        .execute(&pool)
        .await
        .expect("api key");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Arc::new(Engine::new(Store::new(pool.clone()), embedder, extractor));
    let state = AppState { engine: engine.clone() };

    let conv = r#"{"conversationId":"c-1","containerTag":"user:3",
                   "messages":[{"role":"user","content":"I am training for a marathon"},
                               {"role":"assistant","content":"Nice, what is your target time?"}]}"#;

    let resp = app(state.clone()).oneshot(post("sk-test", conv)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "ingested");
    assert_eq!(j["conversationId"], "c-1");

    // The conversation content is recallable under the parsed actor.
    let hits = engine.recall(tenant, "user:3", "marathon", 10).await.unwrap();
    assert!(!hits.is_empty(), "conversation memory recallable");

    // Re-posting the same conversationId is an idempotent skip.
    let resp = app(state.clone()).oneshot(post("sk-test", conv)).await.unwrap();
    assert_eq!(body_json(resp).await["status"], "skipped");

    // A grown thread under the same id is also skipped (documented idempotency contract).
    let grown = r#"{"conversationId":"c-1","containerTag":"user:3",
                    "messages":[{"role":"user","content":"I am training for a marathon"},
                                {"role":"assistant","content":"Nice, what is your target time?"},
                                {"role":"user","content":"Sub four hours"}]}"#;
    let resp = app(state.clone()).oneshot(post("sk-test", grown)).await.unwrap();
    assert_eq!(body_json(resp).await["status"], "skipped", "growth under same id is skipped");

    // Empty messages is a 400.
    let resp = app(state)
        .oneshot(post("sk-test", r#"{"conversationId":"c-2","containerTag":"user:3","messages":[]}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
