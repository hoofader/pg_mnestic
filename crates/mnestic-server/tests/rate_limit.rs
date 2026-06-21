// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for per-key rate limiting: a key over its bucket gets 429, a different key
//! has its own budget, unauthenticated /health is never limited, and a bad token is 401 (not
//! 429, so it never consumes a bucket).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_server::{app, AppState, RateLimiter};
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

async fn status_of(state: AppState, req: Request<Body>) -> StatusCode {
    app(state).oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn per_key_rate_limit() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rl') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");
    // Two distinct keys for the same tenant, to show the limit is per key, not per tenant.
    for tok in ["sk-a", "sk-b"] {
        sqlx::query("INSERT INTO mnestic_api_key (token_sha256, tenant_id) VALUES (digest($1, 'sha256'), $2)")
            .bind(tok)
            .bind(tenant)
            .execute(&pool)
            .await
            .expect("api key");
    }

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Arc::new(Engine::new(Store::new(pool.clone()), embedder, extractor));
    // Capacity 2: the third request on a key with no time to refill is rejected.
    let state = AppState { engine, limiter: RateLimiter::per_minute(2) };

    // sk-a: two pass, the third is 429.
    assert_eq!(status_of(state.clone(), get("/v3/session", Some("sk-a"))).await, StatusCode::OK);
    assert_eq!(status_of(state.clone(), get("/v3/session", Some("sk-a"))).await, StatusCode::OK);
    assert_eq!(
        status_of(state.clone(), get("/v3/session", Some("sk-a"))).await,
        StatusCode::TOO_MANY_REQUESTS,
        "third request on the same key is limited"
    );

    // sk-b has its own bucket and is unaffected by sk-a being exhausted.
    assert_eq!(
        status_of(state.clone(), get("/v3/session", Some("sk-b"))).await,
        StatusCode::OK,
        "a different key has its own budget"
    );

    // /health is unauthenticated and never rate-limited.
    assert_eq!(status_of(state.clone(), get("/health", None)).await, StatusCode::OK);

    // A bad token is 401 (rejected before the limiter), so it never consumes a bucket.
    assert_eq!(
        status_of(state, get("/v3/session", Some("sk-bogus"))).await,
        StatusCode::UNAUTHORIZED,
    );
}
