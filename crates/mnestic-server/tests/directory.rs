// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for /v3/session, /v3/projects, and the MCP whoAmI/listProjects tools.

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

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
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
async fn session_and_projects() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('acme-user') RETURNING id")
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

    // Seed a memory carrying a container tag so projects has something to list.
    engine
        .add(tenant, "user:5", &["org:9".to_string()], "a fact", "conversation", None)
        .await
        .unwrap();

    // /v3/session returns the tenant's external id as userId.
    let resp = app(state.clone()).oneshot(get("/v3/session", Some("sk-test"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["userId"], "acme-user");

    // /v3/session requires auth.
    let resp = app(state.clone()).oneshot(get("/v3/session", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // /v3/projects lists the container tags in use.
    let resp = app(state.clone()).oneshot(get("/v3/projects", Some("sk-test"))).await.unwrap();
    let j = body_json(resp).await;
    let tags: Vec<&str> = j.as_array().unwrap().iter().filter_map(|t| t.as_str()).collect();
    assert!(tags.contains(&"org:9"), "projects lists the tag, got {tags:?}");

    // MCP whoAmI and listProjects mirror them.
    let resp = app(state.clone())
        .oneshot(post("/mcp", "sk-test", r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoAmI","arguments":{}}}"#))
        .await
        .unwrap();
    let text = body_json(resp).await["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("acme-user"), "whoAmI returns the user, got {text}");

    let resp = app(state)
        .oneshot(post("/mcp", "sk-test", r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"listProjects","arguments":{}}}"#))
        .await
        .unwrap();
    let text = body_json(resp).await["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(text.contains("org:9"), "listProjects returns the tag, got {text}");
}
