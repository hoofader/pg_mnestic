// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for /v3/documents and /v3/search: ingest a document over HTTP, then
//! find a chunk of it via document search. Driven with tower::oneshot + mock engine.

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
async fn ingest_and_search_documents_endpoints() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('doc') RETURNING id")
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
    let state = AppState { engine, limiter: mnestic_server::RateLimiter::disabled() };

    // Ingest a document with a unique phrase under containerTag user:42.
    let resp = app(state.clone())
        .oneshot(post(
            "/v3/documents",
            "sk-test",
            r#"{"content":"The mitochondria powerhouse note is unique here.","containerTag":"user:42","title":"Cells","customId":"d1","metadata":{"source":"wiki","page":7}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "ingested");
    assert!(j["id"].is_string(), "new document id returned");
    assert!(j["chunks"].as_u64().unwrap() >= 1, "at least one chunk");

    // Document search finds a chunk carrying the phrase (lexical match drives it).
    let resp = app(state.clone())
        .oneshot(post("/v3/search", "sk-test", r#"{"q":"mitochondria powerhouse","containerTag":"user:42"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["containerTag"], "user:42");
    // sdk-ts SearchDocumentsResponse: results grouped per document, each with chunks[].
    assert!(j["timing"].is_number() && j["total"].is_number(), "v3 search carries timing/total");
    let hits = j["results"].as_array().unwrap();
    assert!(
        hits.iter().any(|h| h["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["content"].as_str().is_some_and(|m| m.contains("mitochondria")))),
        "document search returns the matching chunk, got {hits:?}"
    );
    // The metadata sent at ingest round-trips on the document result.
    let doc = hits.iter().find(|h| h["metadata"]["source"] == "wiki").expect("doc carries metadata");
    assert_eq!(doc["metadata"]["page"], 7, "metadata values preserved");
    assert!(hits.iter().all(|h| {
        h["documentId"].is_string()
            && h["score"].is_number()
            && h["metadata"].is_object()
            && h["chunks"].as_array().unwrap().iter().all(|c| c["isRelevant"].is_boolean())
    }));

    // Idempotent re-ingest is skipped.
    let resp = app(state.clone())
        .oneshot(post(
            "/v3/documents",
            "sk-test",
            r#"{"content":"The mitochondria powerhouse note is unique here.","containerTag":"user:42","customId":"d1"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j["status"], "skipped");
    assert!(j["id"].is_null(), "skip returns no new document id");

    // Empty content is a 400, and auth is required.
    let resp = app(state.clone())
        .oneshot(post("/v3/documents", "sk-test", r#"{"content":"  ","containerTag":"user:42"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app(state.clone())
        .oneshot(post("/v3/search", "nope", r#"{"q":"x","containerTag":"user:42"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Ingest two documents sharing a search term but carrying different metadata, so the
    // `filters` tree can retain one and drop the other.
    let resp = app(state.clone())
        .oneshot(post(
            "/v3/documents",
            "sk-test",
            r#"{"content":"quantum entanglement primer for the alpha team","containerTag":"user:42","customId":"qa","metadata":{"team":"alpha"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app(state.clone())
        .oneshot(post(
            "/v3/documents",
            "sk-test",
            r#"{"content":"quantum entanglement primer for the beta team","containerTag":"user:42","customId":"qb","metadata":{"team":"beta"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Filtering on team=alpha returns only the alpha document.
    let resp = app(state.clone())
        .oneshot(post(
            "/v3/search",
            "sk-test",
            r#"{"q":"quantum entanglement","containerTag":"user:42","filters":{"AND":[{"key":"team","value":"alpha"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let teams: Vec<&str> = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["metadata"]["team"].as_str())
        .collect();
    assert_eq!(teams, vec!["alpha"], "doc filter keeps only the alpha document, got {teams:?}");

    // A non-matching filter returns no documents.
    let resp = app(state)
        .oneshot(post(
            "/v3/search",
            "sk-test",
            r#"{"q":"quantum entanglement","containerTag":"user:42","filters":{"AND":[{"key":"team","value":"gamma"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["results"].as_array().unwrap().len(), 0, "non-matching doc filter returns empty");
}
