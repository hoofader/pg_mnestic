// SPDX-License-Identifier: Apache-2.0

//! Run the Mnestic eval over a normalized dataset, against a live Postgres and real
//! providers. Build with `--features real`.
//!
//! Env: ANTHROPIC_API_KEY (extraction, answer, judge), OPENAI_API_KEY (embeddings),
//! DATABASE_URL (Postgres with the migrations applied or applyable). Arg 1: path to
//! the dataset JSON. Real runs cost money (Opus 4.8 is $5/$25 per 1M tokens).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_eval::dataset;
use mnestic_eval::providers::{AnthropicAnswerer, AnthropicJudge};
use mnestic_eval::run_eval;
use mnestic_model::{AnthropicExtractor, OpenAiEmbedder};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Recall fan-out per question. Override with MEMORYBENCH_RECALL_LIMIT.
const DEFAULT_RECALL_LIMIT: i64 = 10;

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: memorybench <dataset.json>")?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
    let openai_key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
    let dsn = std::env::var("DATABASE_URL").context("DATABASE_URL not set")?;
    let recall_limit: i64 = match std::env::var("MEMORYBENCH_RECALL_LIMIT") {
        Ok(v) => v.parse().context("MEMORYBENCH_RECALL_LIMIT must be an integer")?,
        Err(_) => DEFAULT_RECALL_LIMIT,
    };

    let cases = dataset::load(&PathBuf::from(&path))?;
    eprintln!("loaded {} cases from {path}", cases.len());

    // The run loop is sequential, so a small pool is enough.
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&dsn)
        .await
        .context("connecting to DATABASE_URL")?;
    run_migrations(&pool).await.context("running migrations")?;

    // A fresh tenant per run so repeated runs do not accumulate memories into the
    // same actors (which would drift recall and the score across runs).
    let external_id = format!("memorybench-{}", Uuid::new_v4());
    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ($1) RETURNING id")
            .bind(&external_id)
            .fetch_one(&pool)
            .await
            .context("creating the run tenant")?;

    let embedder: Arc<dyn Embedder> =
        Arc::new(OpenAiEmbedder::new(openai_key, "text-embedding-3-small"));
    let extractor: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::new(&anthropic_key));
    let engine = Engine::new(Store::new(pool), embedder, extractor);

    let answerer = AnthropicAnswerer::new(&anthropic_key);
    let judge = AnthropicJudge::new(&anthropic_key);

    let report = run_eval(&engine, tenant, &answerer, &judge, recall_limit, &cases).await;
    let s = &report.score;
    if !report.errors.is_empty() {
        eprintln!("{} item(s) errored (not scored):", report.errors.len());
        for e in &report.errors {
            eprintln!("  {e}");
        }
    }
    println!(
        "MemScore: n={} accuracy={:.3} avg_query_ms={:.1} avg_recalled_ctx_tokens~{:.0}",
        s.n, s.accuracy, s.avg_query_latency_ms, s.avg_recalled_context_tokens
    );
    Ok(())
}
