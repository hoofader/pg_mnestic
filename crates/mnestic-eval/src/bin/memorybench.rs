// SPDX-License-Identifier: AGPL-3.0-only

//! Run the Mnestic eval over a normalized dataset, against a live Postgres and real
//! providers. Build with `--features real`.
//!
//! Env: ANTHROPIC_API_KEY (extraction, answer, judge, rewrite, rerank), OPENAI_API_KEY
//! (embeddings), DATABASE_URL (Postgres with the migrations applied or applyable). Arg
//! 1: path to the dataset JSON. Real runs cost money (Opus 4.8 is $5/$25 per 1M tokens).
//!
//! A/B over recall modes: the dataset is ingested ONCE (the expensive extraction phase),
//! then every mode in MEMORYBENCH_MODES is evaluated against the same stored memory, so
//! the only thing that varies is recall. Modes are `off`, `rewrite`, `rerank`, `both`
//! (default `off,rewrite,rerank,both`). Narrow it (e.g. `MEMORYBENCH_MODES=off,both`) to
//! cut the query-phase cost; ingestion is paid once regardless. The answer and judge
//! calls run once per question per mode, so N modes multiply the whole query phase by N;
//! each non-`off` mode adds a Claude rewrite and/or rerank call on top.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_eval::providers::{AnthropicAnswerer, AnthropicJudge, AnthropicReranker, AnthropicRewriter};
use mnestic_eval::{dataset, evaluate_cases, ingest_cases, EngineBackend};
use mnestic_model::{AnthropicExtractor, OpenAiEmbedder};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgPool, PgPoolOptions};
use uuid::Uuid;

/// Recall fan-out per question. Override with MEMORYBENCH_RECALL_LIMIT.
const DEFAULT_RECALL_LIMIT: i64 = 10;

/// The four recall configurations the eval can compare.
const DEFAULT_MODES: &str = "off,rewrite,rerank,both";

struct Mode {
    name: &'static str,
    rewrite: bool,
    rerank: bool,
}

/// Parse a comma-separated mode list, rejecting unknown names up front so a paid run
/// never starts with a typo'd configuration. Empty segments (a trailing comma, stray
/// whitespace) are skipped; a list with no valid modes is an error.
fn parse_modes(s: &str) -> Result<Vec<Mode>> {
    let modes: Vec<Mode> = s
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| match m {
            "off" => Ok(Mode { name: "off", rewrite: false, rerank: false }),
            "rewrite" => Ok(Mode { name: "rewrite", rewrite: true, rerank: false }),
            "rerank" => Ok(Mode { name: "rerank", rewrite: false, rerank: true }),
            "both" => Ok(Mode { name: "both", rewrite: true, rerank: true }),
            other => Err(anyhow!("unknown mode {other:?} (use off|rewrite|rerank|both)")),
        })
        .collect::<Result<_>>()?;
    if modes.is_empty() {
        return Err(anyhow!("MEMORYBENCH_MODES has no modes (use off|rewrite|rerank|both)"));
    }
    Ok(modes)
}

/// Build an engine over the shared pool for one mode. Ingestion already happened, so
/// only the recall providers (rewriter/reranker) distinguish the modes; the extractor
/// is unused on the query path but the engine still requires one.
fn engine_for(
    mode: &Mode,
    pool: &PgPool,
    embedder: &Arc<dyn Embedder>,
    extractor: &Arc<dyn Extractor>,
    anthropic_key: &str,
) -> Engine {
    let mut e = Engine::new(Store::new(pool.clone()), embedder.clone(), extractor.clone());
    if mode.rewrite {
        e = e.with_rewriter(Arc::new(AnthropicRewriter::new(anthropic_key)));
    }
    if mode.rerank {
        e = e.with_reranker(Arc::new(AnthropicReranker::new(anthropic_key)));
    }
    e
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
    // Validate modes before any DB work, so a typo fails before a run tenant is created.
    let modes = parse_modes(
        std::env::var("MEMORYBENCH_MODES").as_deref().unwrap_or(DEFAULT_MODES),
    )?;

    let cases = dataset::load(&PathBuf::from(&path))?;
    eprintln!("loaded {} cases from {path}", cases.len());

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&dsn)
        .await
        .context("connecting to DATABASE_URL")?;
    run_migrations(&pool).await.context("running migrations")?;

    // A fresh tenant per run so repeated runs do not accumulate memories into the same
    // actors (which would drift recall and the score across runs).
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
    let answerer = AnthropicAnswerer::new(&anthropic_key);
    let judge = AnthropicJudge::new(&anthropic_key);

    // Ingest once with a plain engine (recall providers do not affect ingestion).
    let mode_names: Vec<&str> = modes.iter().map(|m| m.name).collect();
    eprintln!("ingesting once, then evaluating modes: {}", mode_names.join(", "));
    let base = Engine::new(Store::new(pool.clone()), embedder.clone(), extractor.clone());
    let base_backend = EngineBackend::new(Arc::new(base), tenant, "pg_mnestic");
    let ingest = ingest_cases(&base_backend, &cases).await;
    if !ingest.errors.is_empty() {
        eprintln!("{} case(s) failed to ingest (their questions are skipped):", ingest.errors.len());
        for e in &ingest.errors {
            eprintln!("  {e}");
        }
    }

    // Evaluate each mode against the same stored memory.
    let mut summary: Vec<(&str, f64)> = Vec::new();
    for mode in &modes {
        let engine = engine_for(mode, &pool, &embedder, &extractor, &anthropic_key);
        let backend = EngineBackend::new(Arc::new(engine), tenant, mode.name);
        let report =
            evaluate_cases(&backend, &answerer, &judge, recall_limit, &cases, &ingest.failed)
                .await;
        let s = &report.score;
        println!(
            "=== mode {} (rewrite={} rerank={}) ===",
            mode.name, mode.rewrite, mode.rerank
        );
        if !report.errors.is_empty() {
            eprintln!("  {} question(s) errored (not scored):", report.errors.len());
            for e in &report.errors {
                eprintln!("    {e}");
            }
        }
        println!(
            "MemScore: n={} accuracy={:.3} avg_query_ms={:.1} avg_recalled_ctx_tokens~{:.0}",
            s.n, s.accuracy, s.avg_query_latency_ms, s.avg_recalled_context_tokens
        );
        for t in &s.per_type {
            println!("  {:<28} n={:<4} accuracy={:.3}", t.question_type, t.n, t.accuracy);
        }
        summary.push((mode.name, s.accuracy));
    }

    let line = summary
        .iter()
        .map(|(name, acc)| format!("{name}={acc:.3}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!("accuracy by mode: {line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes_reads_names_and_flags() {
        let m = parse_modes("off, rewrite,rerank , both").unwrap();
        let got: Vec<(&str, bool, bool)> = m.iter().map(|x| (x.name, x.rewrite, x.rerank)).collect();
        assert_eq!(
            got,
            vec![
                ("off", false, false),
                ("rewrite", true, false),
                ("rerank", false, true),
                ("both", true, true),
            ]
        );
    }

    #[test]
    fn parse_modes_skips_empty_segments() {
        assert_eq!(parse_modes("off,").unwrap().len(), 1);
    }

    #[test]
    fn parse_modes_rejects_unknown_and_empty() {
        assert!(parse_modes("off,nope").is_err());
        assert!(parse_modes("").is_err());
        assert!(parse_modes("  ,  ").is_err());
    }
}
