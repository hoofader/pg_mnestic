// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized profile test against pgvector/pgvector:pg16. Checks that the profile
//! refreshed on write keeps durable, high-confidence facts as static and the recent
//! window as dynamic, and excludes low-confidence and ephemeral facts from static.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Temporal};
use mnestic_engine::Engine;
use mnestic_model::MockEmbedder;
use mnestic_store::Store;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

struct ScriptExtractor {
    batches: Mutex<VecDeque<Vec<Candidate>>>,
}

#[async_trait]
impl Extractor for ScriptExtractor {
    async fn extract(&self, _text: &str, _ctx: &Ctx) -> mnestic_core::Result<Vec<Candidate>> {
        Ok(self.batches.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().expect("rfc3339 timestamp")
}

fn structured(content: &str, attribute: &str, value: &str, confidence: f32) -> Candidate {
    Candidate {
        content: content.to_string(),
        subject: Some("user".to_string()),
        attribute: Some(attribute.to_string()),
        value: Some(value.to_string()),
        single_valued: true,
        mem_type: MemType::Fact,
        confidence,
        is_static: false,
        temporal: Temporal::None,
        forget_after: None,
    }
}

fn note(content: &str, confidence: f32) -> Candidate {
    Candidate {
        content: content.to_string(),
        subject: None,
        attribute: None,
        value: None,
        single_valued: false,
        mem_type: MemType::Fact,
        confidence,
        is_static: false,
        temporal: Temporal::None,
        forget_after: None,
    }
}

fn ephemeral(content: &str, confidence: f32, forget_after: DateTime<Utc>) -> Candidate {
    Candidate {
        mem_type: MemType::Episode,
        forget_after: Some(forget_after),
        ..note(content, confidence)
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

#[tokio::test]
async fn profile_separates_static_and_dynamic() {
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

    let su_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let su_pool = connect(su_opts).await;
    mnestic_store::run_migrations(&su_pool).await.expect("migrations");

    for stmt in [
        "CREATE ROLE mnestic_app LOGIN PASSWORD 'app' NOBYPASSRLS",
        "GRANT USAGE ON SCHEMA public TO mnestic_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO mnestic_app",
    ] {
        sqlx::query(stmt).execute(&su_pool).await.unwrap_or_else(|e| panic!("setup [{stmt}]: {e}"));
    }

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('A') RETURNING id")
            .fetch_one(&su_pool)
            .await
            .expect("tenant");

    let app_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("mnestic_app")
        .password("app")
        .database("postgres");
    let app_pool = connect(app_opts).await;

    let extractor = ScriptExtractor {
        batches: Mutex::new(
            vec![
                vec![structured("user lives in SF", "location", "SF", 0.9)],
                vec![note("user is currently reading a mystery novel", 0.5)],
                vec![ephemeral("user has a flight on Friday", 0.7, ts("2027-01-01T00:00:00Z"))],
                // High confidence (would qualify as static), but ephemeral: only the
                // forget_after filter can keep it out of static.
                vec![ephemeral("user is on vacation until next month", 0.95, ts("2027-06-01T00:00:00Z"))],
            ]
            .into(),
        ),
    };

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(extractor);
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    for _ in 0..4 {
        engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    }

    let p = engine.profile(tenant, "user").await.unwrap();

    // High-confidence structured fact is durable, rendered as "attribute: value".
    assert!(p.static_facts.iter().any(|f| f == "location: SF"), "static: {:?}", p.static_facts);
    // Low-confidence fact is not durable (excluded by the confidence bar).
    assert!(!p.static_facts.iter().any(|f| f.contains("mystery")), "static had low-confidence fact");
    // High-confidence ephemeral fact is excluded only by the forget_after filter.
    assert!(
        !p.static_facts.iter().any(|f| f.contains("vacation")),
        "static must exclude an ephemeral fact via forget_after, even at high confidence"
    );

    // Recent context holds the latest items regardless of confidence or ephemerality.
    assert!(p.dynamic_ctx.iter().any(|f| f.contains("mystery")), "dynamic: {:?}", p.dynamic_ctx);
    assert!(p.dynamic_ctx.iter().any(|f| f.contains("vacation")), "dynamic should hold the live ephemeral fact");
    assert!(p.refreshed_at.is_some());
}
