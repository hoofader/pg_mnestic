// SPDX-License-Identifier: Apache-2.0

//! Run the Mnestic eval over a normalized dataset, against a live Postgres and real
//! providers. Build with `--features real`.
//!
//! Env: ANTHROPIC_API_KEY (extraction, answer, judge, rewrite, rerank), OPENAI_API_KEY
//! (embeddings), DATABASE_URL (Postgres with the migrations applied or applyable). Arg
//! 1: path to the dataset JSON. Real runs cost money (Opus 4.8 is $5/$25 per 1M tokens).
//!
//! Recall quality: a Claude query-rewriter and an LLM-as-reranker run per question by
//! default (two extra Opus calls per question, one each; the reranker prompt also
//! carries the candidate pool). Set MEMORYBENCH_REWRITE=0 / MEMORYBENCH_RERANK=0 to
//! disable either and measure its lift or cut cost.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_eval::dataset;
use mnestic_eval::providers::{AnthropicAnswerer, AnthropicJudge, AnthropicReranker, AnthropicRewriter};
use mnestic_eval::run_eval;
use mnestic_model::{AnthropicExtractor, OpenAiEmbedder};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Recall fan-out per question. Override with MEMORYBENCH_RECALL_LIMIT.
const DEFAULT_RECALL_LIMIT: i64 = 10;

/// Parse a 0/1/true/false toggle, defaulting when unset. A typo'd value is an error
/// rather than a silent default, so a paid run never runs the wrong configuration.
fn env_flag(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(anyhow!("{name} must be a boolean (0/1/true/false), got {other:?}")),
        },
        Err(_) => Ok(default),
    }
}

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
    // Validate the recall toggles before any DB work, so a typo'd flag fails before
    // a run tenant is created (an orphan row otherwise).
    let use_rewrite = env_flag("MEMORYBENCH_REWRITE", true)?;
    let use_rerank = env_flag("MEMORYBENCH_RERANK", true)?;

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
    let mut engine = Engine::new(Store::new(pool), embedder, extractor);
    if use_rewrite {
        engine = engine.with_rewriter(Arc::new(AnthropicRewriter::new(&anthropic_key)));
    }
    if use_rerank {
        engine = engine.with_reranker(Arc::new(AnthropicReranker::new(&anthropic_key)));
    }
    eprintln!("recall: limit={recall_limit} rewrite={use_rewrite} rerank={use_rerank}");

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
    for t in &s.per_type {
        println!("  {:<28} n={:<4} accuracy={:.3}", t.question_type, t.n, t.accuracy);
    }
    Ok(())
}
