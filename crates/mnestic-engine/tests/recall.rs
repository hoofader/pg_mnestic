// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized recall test against pgvector/pgvector:pg16. Populates memories via
//! Engine::add, then checks hybrid recall ranks the relevant memory first and
//! excludes superseded and time-expired rows.

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

fn mem(content: &str) -> Candidate {
    Candidate {
        content: content.to_string(),
        subject: None,
        attribute: None,
        value: None,
        single_valued: false,
        mem_type: MemType::Fact,
        confidence: 0.8,
        is_static: false,
        temporal: Temporal::None,
        forget_after: None,
    }
}

fn loc(value: &str, at: DateTime<Utc>) -> Candidate {
    Candidate {
        content: format!("user lives in {value}"),
        subject: Some("user".to_string()),
        attribute: Some("location".to_string()),
        value: Some(value.to_string()),
        single_valued: true,
        mem_type: MemType::Fact,
        confidence: 0.9,
        is_static: false,
        temporal: Temporal::AsOf { timestamp: at },
        forget_after: None,
    }
}

fn expiring(content: &str, forget_after: DateTime<Utc>) -> Candidate {
    Candidate {
        content: content.to_string(),
        forget_after: Some(forget_after),
        mem_type: MemType::Episode,
        ..mem(content)
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
async fn recall_ranks_and_filters() {
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

    let past = ts("2025-01-01T00:00:00Z");
    let t1 = ts("2026-01-01T00:00:00Z");
    let t2 = ts("2026-06-01T00:00:00Z");

    let extractor = ScriptExtractor {
        batches: Mutex::new(
            vec![
                vec![mem("user prefers tea over coffee")],
                vec![mem("user enjoys hiking in the mountains")],
                vec![loc("Berlin", t1)],
                vec![loc("Paris", t2)],
                vec![expiring("user has a dentist appointment tomorrow", past)],
            ]
            .into(),
        ),
    };

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(extractor);
    let engine = Engine::new(Store::new(app_pool.clone()), embedder, extractor);
    let tags: Vec<String> = Vec::new();

    for _ in 0..5 {
        engine.add(tenant, "user", &tags, "ignored", "conversation", None).await.unwrap();
    }

    // Exact match ranks first (vector distance 0 plus lexical hit).
    let hits = engine.recall(tenant, "user", "user prefers tea over coffee", 10).await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].content.as_deref(), Some("user prefers tea over coffee"));

    // Superseded rows are excluded: the latest location is Paris, not Berlin.
    let hits = engine.recall(tenant, "user", "lives in", 10).await.unwrap();
    let contents: Vec<String> = hits.iter().filter_map(|h| h.content.clone()).collect();
    assert!(contents.iter().any(|c| c.contains("Paris")), "latest location should surface");
    assert!(!contents.iter().any(|c| c.contains("Berlin")), "superseded location must not surface");

    // Time-expired rows are excluded even though their status is still active.
    let hits = engine.recall(tenant, "user", "dentist appointment", 10).await.unwrap();
    let contents: Vec<String> = hits.iter().filter_map(|h| h.content.clone()).collect();
    assert!(!contents.iter().any(|c| c.contains("dentist")), "expired memory must not surface");

    // The vector signal contributes independently of lexical match: a query whose
    // terms appear in no memory still returns nearest neighbors via the vector CTE.
    let hits = engine.recall(tenant, "user", "zzqqx unrelated gibberish", 10).await.unwrap();
    assert!(!hits.is_empty(), "vector recall returns neighbors with no lexical overlap");
}
