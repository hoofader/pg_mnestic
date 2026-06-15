// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for the MCP endpoint (POST /mcp): the JSON-RPC handshake, tools/list,
//! and tools/call for memory(save) + recall, driven with tower::oneshot + mock engine.

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

fn rpc(token: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn mcp_handshake_and_tools() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('mcp') RETURNING id")
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

    // No token is rejected.
    let resp = app(state.clone())
        .oneshot(rpc(None, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // initialize echoes the requested protocol version and advertises tools.
    let resp = app(state.clone())
        .oneshot(rpc(
            Some("sk-test"),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["result"]["protocolVersion"], "2025-03-26");
    assert!(j["result"]["capabilities"]["tools"].is_object());

    // An unsupported version is not parroted; the server answers with its own.
    let resp = app(state.clone())
        .oneshot(rpc(
            Some("sk-test"),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_ne!(j["result"]["protocolVersion"], "1999-01-01", "unsupported version not echoed");

    // A batch (array) is rejected, not silently accepted.
    let resp = app(state.clone())
        .oneshot(rpc(Some("sk-test"), r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j["error"]["code"], -32600, "batch rejected");

    // notifications/initialized (no id) gets 202, no body.
    let resp = app(state.clone())
        .oneshot(rpc(Some("sk-test"), r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // tools/list names memory and recall.
    let resp = app(state.clone())
        .oneshot(rpc(Some("sk-test"), r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let names: Vec<&str> = j["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"memory") && names.contains(&"recall"), "tools listed, got {names:?}");

    // tools/call memory save, then recall finds it.
    let resp = app(state.clone())
        .oneshot(rpc(
            Some("sk-test"),
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory","arguments":{"content":"the user loves climbing","containerTag":"user:9"}}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let text = j["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("saved"), "memory saved, got {text:?}");

    let resp = app(state.clone())
        .oneshot(rpc(
            Some("sk-test"),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recall","arguments":{"query":"climbing","containerTag":"user:9"}}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let text = j["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("climbing"), "recall returns the memory, got {text}");
    assert!(text.contains("profile"), "includeProfile defaults true");

    // memory-graph lists the actor's documents.
    engine
        .ingest_document(tenant, "user:9", &[], Some("Notes"), None, "some reference document content", Some("d1"))
        .await
        .unwrap();
    let resp = app(state.clone())
        .oneshot(rpc(
            Some("sk-test"),
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory-graph","arguments":{"containerTag":"user:9"}}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert!(j["result"]["content"][0]["text"].as_str().unwrap().contains("document"), "summary text present");
    let sc = &j["result"]["structuredContent"];
    assert_eq!(sc["totalCount"], 1, "structuredContent totalCount");
    let titles: Vec<&str> = sc["documents"].as_array().unwrap().iter().filter_map(|d| d["title"].as_str()).collect();
    assert!(titles.contains(&"Notes"), "document title in structuredContent, got {titles:?}");

    // Unknown method is a JSON-RPC error.
    let resp = app(state)
        .oneshot(rpc(Some("sk-test"), r#"{"jsonrpc":"2.0","id":5,"method":"bogus/method"}"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j["error"]["code"], -32601, "method not found");
}
