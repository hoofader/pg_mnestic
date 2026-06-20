// SPDX-License-Identifier: Apache-2.0

//! Dockerized test for the forget path: tombstoning by custom_id drops the memory
//! from recall and the cached profile, is scoped to the owning actor, and is
//! idempotent.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use mnestic_core::{Candidate, Ctx, Embedder, Extractor, MemType, Result, Temporal};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

/// Emits one single-valued (user, city) triple whose value is the input text, so
/// successive adds with different values supersede each other.
struct CityExtractor;

#[async_trait]
impl Extractor for CityExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            content: format!("city is {text}"),
            subject: Some("user".to_string()),
            attribute: Some("city".to_string()),
            value: Some(text.to_string()),
            single_valued: true,
            mem_type: MemType::Fact,
            confidence: 0.9,
            is_static: false,
            temporal: Temporal::None,
            forget_after: None,
        }])
    }
}

async fn pgvector() -> (testcontainers::ContainerAsync<GenericImage>, PgPool) {
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
    (container, pool)
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
async fn forget_tombstones_by_custom_id() {
    let (_container, pool) = pgvector().await;

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('fg') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);

    let tags: Vec<String> = Vec::new();
    engine.add(tenant, "u", &tags, "alpha fact", "conversation", Some("c1")).await.unwrap();
    engine.add(tenant, "u", &tags, "beta fact", "conversation", Some("c2")).await.unwrap();

    let recall = |q: &'static str| {
        let e = &engine;
        async move {
            let mut got: Vec<String> = e
                .recall(tenant, "u", q, 10)
                .await
                .unwrap()
                .iter()
                .filter_map(|h| h.content.clone())
                .collect();
            got.sort();
            got
        }
    };

    assert_eq!(recall("fact").await, vec!["alpha fact", "beta fact"], "both present before forget");

    // Wrong actor: forgetting c1 as "v" tombstones nothing.
    assert!(
        engine.forget(tenant, "v", "c1", None).await.unwrap().is_empty(),
        "forget is scoped to the owning actor"
    );
    assert_eq!(recall("fact").await, vec!["alpha fact", "beta fact"], "wrong-actor forget is a no-op");

    // Forget c1 for the owning actor: only alpha goes.
    let forgotten = engine.forget(tenant, "u", "c1", Some("user asked")).await.unwrap();
    assert_eq!(forgotten.len(), 1, "one memory forgotten");
    assert_eq!(recall("fact").await, vec!["beta fact"], "alpha is gone, beta stays");

    // The cached profile no longer carries the forgotten fact.
    let profile = engine.profile(tenant, "u").await.unwrap();
    assert!(
        !profile.dynamic_ctx.iter().any(|c| c.contains("alpha")),
        "forgotten fact left the profile"
    );
    assert!(
        profile.dynamic_ctx.iter().any(|c| c.contains("beta")),
        "the surviving fact stays in the profile"
    );

    // Idempotent: forgetting an already-forgotten / unknown custom_id yields nothing.
    assert!(engine.forget(tenant, "u", "c1", None).await.unwrap().is_empty(), "second forget is a no-op");
    assert!(engine.forget(tenant, "u", "nope", None).await.unwrap().is_empty(), "unknown custom_id is a no-op");
}

#[tokio::test]
async fn forget_does_not_resurrect_superseded_prior() {
    let (_container, pool) = pgvector().await;

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('fg2') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(CityExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    // v1 (NYC) then a later v2 (SF) that supersedes it, ordered by event time.
    let t1 = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    let t2 = Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).unwrap();
    engine.add_at(tenant, "u", &tags, "NYC", "conversation", Some("v1"), Some(t1), &serde_json::json!({})).await.unwrap();
    let r = engine.add_at(tenant, "u", &tags, "SF", "conversation", Some("v2"), Some(t2), &serde_json::json!({})).await.unwrap();
    assert_eq!(r.superseded.len(), 1, "v2 supersedes v1");

    let cities = |q: &'static str| {
        let e = &engine;
        async move {
            e.recall(tenant, "u", q, 10)
                .await
                .unwrap()
                .iter()
                .filter_map(|h| h.value.clone())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(cities("city").await, vec!["SF"], "only the latest city is recalled");

    // Forgetting the latest (v2) must not bring the superseded NYC back.
    let forgotten = engine.forget(tenant, "u", "v2", None).await.unwrap();
    assert_eq!(forgotten.len(), 1, "v2 forgotten");
    assert!(cities("city").await.is_empty(), "superseded NYC is not resurrected");
}

/// Emits a (user, employer) triple whose value is the input text; an empty input gives
/// a keyless candidate (a key-wipe). Lets a test drive value-aware content forget.
struct EmployerValueExtractor;

#[async_trait]
impl Extractor for EmployerValueExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        let value = if text.trim().is_empty() { None } else { Some(text.to_string()) };
        Ok(vec![Candidate {
            content: format!("employer {text}"),
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

#[tokio::test]
async fn forget_by_content_is_value_aware() {
    let (_container, pool) = pgvector().await;

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('fc') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(EmployerValueExtractor);
    let engine = Engine::new(Store::new(pool), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    let has_employer = |q: &'static str| {
        let e = &engine;
        async move { !e.recall(tenant, "u", q, 10).await.unwrap().is_empty() }
    };

    engine.add(tenant, "u", &tags, "Acme", "conversation", None).await.unwrap();

    // A different value must not delete the current fact.
    let gone = engine.forget_by_content(tenant, "u", "Globex").await.unwrap();
    assert!(gone.is_empty(), "value mismatch tombstones nothing");
    assert!(has_employer("Acme").await, "the Acme fact survives a Globex forget");

    // The matching value deletes it.
    let gone = engine.forget_by_content(tenant, "u", "Acme").await.unwrap();
    assert_eq!(gone.len(), 1, "matching value is forgotten");
    assert!(!has_employer("Acme").await);

    // A keyless forget (no value) wipes the key's latest row.
    engine.add(tenant, "u", &tags, "Paris", "conversation", None).await.unwrap();
    let gone = engine.forget_by_content(tenant, "u", "").await.unwrap();
    assert_eq!(gone.len(), 1, "keyless forget wipes the latest for the key");
    assert!(!has_employer("Paris").await);
}
