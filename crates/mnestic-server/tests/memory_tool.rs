// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for /v4/memory: save then content-based forget. Uses a structured
//! extractor so the forget content resolves to a (subject, attribute) key that matches
//! what save stored, which is what content-based forget keys on.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Result, Temporal};
use mnestic_engine::Engine;
use mnestic_model::MockEmbedder;
use mnestic_server::{app, AppState};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tower::ServiceExt;
use uuid::Uuid;

/// Emits a single (user, employer) triple. A "forget" command carries no value (a
/// key-wipe), so it resolves to the same key as the saved fact and removes it.
struct EmployerExtractor;

#[async_trait]
impl Extractor for EmployerExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        let value = if text.contains("forget") { None } else { Some(text.to_string()) };
        Ok(vec![Candidate {
            content: text.to_string(),
            subject: Some("user".to_string()),
            attribute: Some("employer".to_string()),
            value,
            single_valued: true,
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

fn post(token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v4/memory")
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
async fn memory_tool_save_then_forget_by_content() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('mt') RETURNING id")
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
    let extractor: Arc<dyn Extractor> = Arc::new(EmployerExtractor);
    let engine = Arc::new(Engine::new(Store::new(pool.clone()), embedder, extractor));
    let state = AppState { engine: engine.clone(), limiter: mnestic_server::RateLimiter::disabled() };

    // Save an employer fact under actor user:7.
    let resp = app(state.clone())
        .oneshot(post("sk-test", r#"{"action":"save","content":"I work at Acme","containerTag":"user:7"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "saved");
    assert!(
        !engine.recall(tenant, "user:7", "Acme", 10).await.unwrap().is_empty(),
        "the employer fact is recallable after save"
    );

    // Content-based forget removes the employer fact.
    let resp = app(state.clone())
        .oneshot(post("sk-test", r#"{"action":"forget","content":"forget where I work","containerTag":"user:7"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "forgotten");
    assert_eq!(j["forgotten"].as_array().unwrap().len(), 1, "one memory tombstoned, ids returned");
    assert!(
        engine.recall(tenant, "user:7", "Acme", 10).await.unwrap().is_empty(),
        "the employer fact is gone after forget"
    );

    // An unknown action is a 400.
    let resp = app(state)
        .oneshot(post("sk-test", r#"{"action":"nope","content":"x","containerTag":"user:7"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
