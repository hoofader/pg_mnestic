// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for the async REST path: POST /v4/memories with dreaming: dynamic returns
//! "queued" without extracting, and a worker pass (process_pending) makes it recallable.

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
        .uri("/v4/memories")
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
async fn dreaming_dynamic_defers_extraction() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('dream') RETURNING id")
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

    // dreaming: dynamic returns queued and does not extract inline.
    let resp = app(state.clone())
        .oneshot(post(
            "sk-test",
            r#"{"content":"the user loves climbing","containerTag":"user:7","dreaming":"dynamic","customId":"d1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "queued");
    assert!(
        engine.recall(tenant, "user:7", "climbing", 10).await.unwrap().is_empty(),
        "not recallable before the worker runs"
    );

    // A worker pass extracts the queued source; the memory becomes recallable.
    assert_eq!(engine.process_pending(tenant, 300, 16).await.unwrap(), 1, "one queued source processed");
    assert!(
        !engine.recall(tenant, "user:7", "climbing", 10).await.unwrap().is_empty(),
        "recallable after the worker pass"
    );

    // Re-posting the same customId in dynamic mode is an idempotent skip.
    let resp = app(state)
        .oneshot(post(
            "sk-test",
            r#"{"content":"the user loves climbing","containerTag":"user:7","dreaming":"dynamic","customId":"d1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["status"], "skipped", "duplicate customId not re-queued");
}
