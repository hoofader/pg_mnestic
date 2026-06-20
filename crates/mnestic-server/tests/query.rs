// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for /v4/search and /v4/profile: ingest under an actor via the
//! engine, then drive the read endpoints with tower::oneshot and a mock engine.

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

fn post(uri: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
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
async fn search_and_profile_endpoints() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('q') RETURNING id")
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
    let state = AppState { engine: engine.clone(), limiter: mnestic_server::RateLimiter::disabled() };

    // Seed memory under actor user:99 (the actor a containerTag of org:7:user:99 maps to).
    engine
        .add(tenant, "user:99", &["org:7".to_string()], "the user loves climbing", "conversation", None)
        .await
        .unwrap();

    // Search finds the seeded memory.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["containerTag"], "org:7:user:99", "echoes the tag");
    let results = j["results"].as_array().unwrap();
    let memories: Vec<&str> = results.iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "search returns the memory, got {memories:?}");
    assert!(results.iter().all(|r| r["similarity"].is_number()), "each result carries a similarity score");
    assert!(results.iter().all(|r| r["updatedAt"].is_string()), "each result carries updatedAt");

    // A malformed containerTag is rejected at the edge.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"x","containerTag":"bad tag!"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "invalid containerTag rejected");

    // Plural containerTags (single-element) is accepted as the same scope.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTags":["org:7:user:99"]}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "plural containerTags accepted");

    // An empty query is rejected, and a bad limit is clamped (not a 500).
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"  ","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "empty q rejected");
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99","limit":-5}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "negative limit is clamped, not a 500");

    // Profile returns the actor's profile; a query also returns relevant memories.
    let resp = app(state.clone())
        .oneshot(post("/v4/profile", "sk-test", r#"{"containerTag":"org:7:user:99","q":"climbing"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(j["profile"]["dynamicCtx"].is_array(), "profile body present");
    let rel: Vec<&str> = j["results"].as_array().unwrap().iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(rel.contains(&"the user loves climbing"), "profile query returns relevant memory");

    // Read endpoints also require auth.
    let resp = app(state)
        .oneshot(post("/v4/search", "nope", r#"{"q":"x","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
