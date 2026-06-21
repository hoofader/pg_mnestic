// SPDX-License-Identifier: AGPL-3.0-only

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
    // sdk-ts SearchMemoriesResponse: top-level timing/total, per-result metadata object.
    assert!(j["timing"].is_number(), "search carries timing");
    assert!(j["total"].is_number(), "search carries total");
    assert!(results.iter().all(|r| r["metadata"].is_object()), "each result carries a metadata object");
    // Memory hits carry `memory` and never a `chunk` key (it is skipped when None).
    assert!(results.iter().all(|r| r.get("chunk").is_none()), "memory hits omit chunk");

    // Seed a document for the same actor so the searchMode/hybrid paths have a chunk to find.
    engine
        .ingest_document(
            tenant,
            "user:99",
            &["org:7".to_string()],
            Some("Doc"),
            None,
            "a note about climbing knots and belaying",
            Some("dq"),
            &serde_json::json!({}),
        )
        .await
        .unwrap();

    // searchMode "documents" returns chunk hits: each entry carries a `chunk` string and null memory.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"documents"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(!results.is_empty(), "documents mode returns at least one chunk");
    assert!(
        results.iter().all(|r| r["chunk"].is_string() && r["memory"].is_null()),
        "documents mode entries carry a chunk string and null memory, got {results:?}"
    );

    // searchMode "memories" (explicit) matches the default: memory set, no chunk key.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"memories"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["memory"].as_str() == Some("the user loves climbing")),
        "memories mode returns the memory"
    );
    assert!(results.iter().all(|r| r.get("chunk").is_none()), "memories mode omits chunk");

    // searchMode "hybrid" returns both kinds: at least one memory and at least one chunk.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"hybrid"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["memory"].is_string()),
        "hybrid returns at least one memory hit, got {results:?}"
    );
    assert!(
        results.iter().any(|r| r["chunk"].is_string()),
        "hybrid returns at least one chunk hit, got {results:?}"
    );

    // Seed a second memory that is lexically irrelevant to "climbing". In RECALL_SQL a memory whose
    // text matches the tsquery gets both a vec and a lex RRF contribution, while one that does not
    // match the tsquery is dropped from the lex CTE and keeps only the vec contribution. So the
    // matching memory scores roughly twice the non-matching one (mock embeddings are a hash, so the
    // vec ranks are noise but bounded; the lex doubling dominates). A relative threshold of 0.9 thus
    // deterministically keeps the strong hit and drops the weak one.
    engine
        .add(tenant, "user:99", &["org:7".to_string()], "the user dislikes loud music", "conversation", None)
        .await
        .unwrap();

    // Without a threshold, both memories come back.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let memories: Vec<&str> =
        j["results"].as_array().unwrap().iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "strong hit present without threshold");
    assert!(memories.contains(&"the user dislikes loud music"), "weak hit present without threshold");

    // A high threshold keeps the strong hit and drops the weak one.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","threshold":0.9}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let memories: Vec<&str> =
        j["results"].as_array().unwrap().iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "threshold keeps the strong hit");
    assert!(!memories.contains(&"the user dislikes loud music"), "threshold drops the weak hit, got {memories:?}");

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
    // sdk-ts ProfileResponse: profile.static / profile.dynamic, recall under searchResults.
    assert!(j["profile"]["static"].is_array(), "profile.static present");
    assert!(j["profile"]["dynamic"].is_array(), "profile.dynamic present");
    assert!(j["searchResults"]["timing"].is_number(), "searchResults.timing present");
    assert!(j["searchResults"]["total"].is_number(), "searchResults.total present");
    let rel: Vec<&str> = j["searchResults"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["memory"].as_str())
        .collect();
    assert!(rel.contains(&"the user loves climbing"), "profile query returns relevant memory");

    // Without a query there is no searchResults block (the SDK types it optional).
    let resp = app(state.clone())
        .oneshot(post("/v4/profile", "sk-test", r#"{"containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(j["profile"]["static"].is_array(), "profile body present without a query");
    assert!(j["searchResults"].is_null(), "no searchResults block without a query");

    // Read endpoints also require auth.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "nope", r#"{"q":"x","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Seed two memories under one actor with different metadata; the `filters` tree retains
    // only the matching one. Both share the term "hiking" so recall returns both candidates.
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking with the infra team",
            "conversation",
            None,
            None,
            &serde_json::json!({"team": "infra"}),
        )
        .await
        .unwrap();
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking with the sales team",
            "conversation",
            None,
            None,
            &serde_json::json!({"team": "sales"}),
        )
        .await
        .unwrap();

    // An AND filter on team=infra keeps only the infra memory.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"infra"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    let teams: Vec<&str> =
        results.iter().filter_map(|r| r["metadata"]["team"].as_str()).collect();
    assert_eq!(teams, vec!["infra"], "filter keeps only the infra memory, got {teams:?}");

    // `negate` flips the same predicate, keeping only the non-infra memory.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"infra","negate":true}]}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let teams: Vec<&str> = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["metadata"]["team"].as_str())
        .collect();
    assert_eq!(teams, vec!["sales"], "negated filter keeps only the non-infra memory, got {teams:?}");

    // A filter that matches nothing returns an empty result set, not an error.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"nobody"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["results"].as_array().unwrap().len(), 0, "non-matching filter returns empty");
    assert_eq!(j["total"], 0, "total reflects the filtered set");

    // The same filter applies to /v4/profile's recall results.
    let resp = app(state)
        .oneshot(post(
            "/v4/profile",
            "sk-test",
            r#"{"containerTag":"org:9:user:fil","q":"hiking","filters":{"AND":[{"key":"team","value":"infra"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let teams: Vec<&str> = j["searchResults"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["metadata"]["team"].as_str())
        .collect();
    assert_eq!(teams, vec!["infra"], "profile filter keeps only the infra memory, got {teams:?}");
}
