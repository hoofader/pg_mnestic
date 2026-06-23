// SPDX-License-Identifier: MIT

//! Dockerized test for POST /v4/memories over the full stack: bearer auth via the
//! api_key table, containerTag scoping, and a real engine (mock providers) writing to
//! Postgres. Driven with tower::oneshot, so no port bind and no network providers.

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

fn post(token: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v4/memories")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn delete_req(token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri("/v4/memories")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn patch_req(token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri("/v4/memories")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn search_req(token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v4/search")
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
async fn add_memory_endpoint() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('srv') RETURNING id")
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

    // No token and a wrong token are both rejected.
    let resp = app(state.clone())
        .oneshot(post(None, r#"{"content":"x","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "missing token");
    let resp = app(state.clone())
        .oneshot(post(Some("nope"), r#"{"content":"x","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "wrong token");

    // A valid save: containerTag org:7:user:99 scopes to actor user:99.
    let resp = app(state.clone())
        .oneshot(post(
            Some("sk-test"),
            r#"{"content":"the user loves climbing","containerTag":"org:7:user:99","customId":"m1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "saved");
    assert_eq!(j["containerTag"], "org:7:user:99", "echoes the caller's tag");

    let hits = engine.recall(tenant, "user:99", "climbing", 10).await.unwrap();
    assert!(
        hits.iter().any(|h| h.content.as_deref() == Some("the user loves climbing")),
        "memory stored under the parsed actor"
    );

    // DELETE /v4/memories forgets one memory by id (the SDK's client.memories.forget).
    let hit_id = hits
        .iter()
        .find(|h| h.content.as_deref() == Some("the user loves climbing"))
        .unwrap()
        .id
        .to_string();
    let resp = app(state.clone())
        .oneshot(delete_req(
            "sk-test",
            &format!(r#"{{"containerTag":"org:7:user:99","id":"{hit_id}","reason":"test"}}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["id"], hit_id, "echoes the forgotten id");
    assert!(j["forgotten"].as_bool().unwrap(), "expected forgotten:true");
    let after = engine.recall(tenant, "user:99", "climbing", 10).await.unwrap();
    assert!(
        !after.iter().any(|h| h.content.as_deref() == Some("the user loves climbing")),
        "forgotten memory should not appear in recall"
    );
    // Forget with neither id nor content is a 400.
    let resp = app(state.clone())
        .oneshot(delete_req("sk-test", r#"{"containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "forget needs id or content");

    // Same customId is an idempotent skip.
    let resp = app(state.clone())
        .oneshot(post(
            Some("sk-test"),
            r#"{"content":"the user loves climbing","containerTag":"org:7:user:99","customId":"m1"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert_eq!(j["status"], "skipped", "repeat customId is skipped");

    // PATCH /v4/memories updates a memory as a new version (the SDK's updateMemory). Add a
    // fresh memory, find its id via recall, then patch it.
    let resp = app(state.clone())
        .oneshot(post(
            Some("sk-test"),
            r#"{"content":"the user loves hiking","containerTag":"org:7:user:99","customId":"p1"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hits = engine.recall(tenant, "user:99", "hiking", 10).await.unwrap();
    let prior_id = hits
        .iter()
        .find(|h| h.content.as_deref() == Some("the user loves hiking"))
        .expect("the added memory is recallable")
        .id
        .to_string();

    let resp = app(state.clone())
        .oneshot(patch_req(
            "sk-test",
            &format!(
                r#"{{"containerTag":"org:7:user:99","id":"{prior_id}","newContent":"the user now prefers bouldering","metadata":{{"k":"v"}}}}"#
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["version"], 2, "a first edit is version 2");
    assert_eq!(j["parentMemoryId"], prior_id, "parent is the prior row");
    assert!(j["rootMemoryId"].is_string(), "rootMemoryId is a string");
    assert_eq!(j["memory"], "the user now prefers bouldering");
    assert!(j["createdAt"].is_string(), "createdAt is a string");

    // The edit carries the memory's class forward (an edit must not demote a static or typed
    // memory to a default dynamic fact). The test role is superuser, so it reads past RLS.
    let new_id = j["id"].as_str().unwrap();
    let class = |id: &str| {
        let p = pool.clone();
        let id = id.to_string();
        async move {
            sqlx::query_as::<_, (bool, String, f32)>(
                "SELECT is_static, mem_type, confidence FROM mnestic_memory WHERE id = $1::uuid",
            )
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap()
        }
    };
    assert_eq!(class(&prior_id).await, class(new_id).await, "edit preserves the memory class");

    // The new content is recallable and the prior is superseded out of recall.
    let after = engine.recall(tenant, "user:99", "bouldering", 10).await.unwrap();
    assert!(
        after.iter().any(|h| h.content.as_deref() == Some("the user now prefers bouldering")),
        "the new version is recallable"
    );
    assert!(
        !after.iter().any(|h| h.content.as_deref() == Some("the user loves hiking")),
        "the superseded prior should not appear in recall"
    );

    // PATCH with no id is a 400; patching a random (unknown) id is a 404.
    let resp = app(state.clone())
        .oneshot(patch_req(
            "sk-test",
            r#"{"containerTag":"org:7:user:99","newContent":"x"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "patch needs an id");
    let resp = app(state.clone())
        .oneshot(patch_req(
            "sk-test",
            &format!(
                r#"{{"containerTag":"org:7:user:99","id":"{}","newContent":"x"}}"#,
                Uuid::new_v4()
            ),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unknown id is a 404");

    // Plural containerTags is accepted as the same scope shape. The save carries `metadata`,
    // which is stored on the resulting rows and returned by /v4/search.
    let resp = app(state.clone())
        .oneshot(post(
            Some("sk-test"),
            r#"{"content":"the user enjoys jazz","containerTags":["user:99"],"customId":"m2","metadata":{"team":"infra"}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["status"], "saved");
    assert_eq!(j["containerTag"], "user:99", "echoes the resolved tag");

    // /v4/search recalls the seeded memory and surfaces the metadata stored on its row.
    let resp = app(state.clone())
        .oneshot(search_req(
            "sk-test",
            r#"{"q":"jazz","containerTag":"user:99"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["memory"] == "the user enjoys jazz")
        .expect("the seeded memory is recalled");
    assert_eq!(hit["metadata"]["team"], "infra", "search returns the stored metadata");

    // Empty content is a 400.
    let resp = app(state)
        .oneshot(post(Some("sk-test"), r#"{"content":"   ","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
