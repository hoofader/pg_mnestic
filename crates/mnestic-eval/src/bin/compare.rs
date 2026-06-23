// SPDX-License-Identifier: AGPL-3.0-only

//! Head-to-head: run one benchmark through two memory engines over the supermemory
//! wire and print a side-by-side. Both backends are HTTP, so the only variable is the
//! engine behind the URL. Build with `--features real`.
//!
//! Env:
//!   MNESTIC_COMPARE_A_NAME / _URL / _KEY   first backend (included only if _URL set)
//!   MNESTIC_COMPARE_B_NAME / _URL / _KEY   second backend (included only if _URL set)
//!   ANTHROPIC_API_KEY                       answerer + judge (Claude)
//!   MNESTIC_EVAL_RECALL_LIMIT               recall fan-out per question (default 10)
//! Arg 1: path to a normalized dataset (the scenarios fixture or a converted LongMemEval json).
//!
//! Example: A = a pg_mnestic server + tenant key, B = supermemory.
//!
//!   export MNESTIC_COMPARE_A_NAME=pg_mnestic
//!   export MNESTIC_COMPARE_A_URL=http://localhost:8080
//!   export MNESTIC_COMPARE_A_KEY=<tenant-key>
//!   export MNESTIC_COMPARE_B_NAME=supermemory
//!   export MNESTIC_COMPARE_B_URL=https://api.supermemory.ai
//!   export MNESTIC_COMPARE_B_KEY=<supermemory-key>
//!   export ANTHROPIC_API_KEY=<key>
//!   cargo run -p mnestic-eval --features real --bin compare -- \
//!     crates/mnestic-eval/fixtures/scenarios.json

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use mnestic_eval::providers::{AnthropicAnswerer, AnthropicJudge};
use mnestic_eval::{compare, dataset, render_markdown, HttpBackend, MemoryBackend};

const DEFAULT_RECALL_LIMIT: i64 = 10;

/// Read one backend's env triple. A backend is included only if its URL is set, so the
/// operator can run with just one (a degenerate one-sided run) or both.
fn backend_from_env(prefix: &str, client: &reqwest::Client) -> Option<HttpBackend> {
    let url = std::env::var(format!("{prefix}_URL")).ok()?;
    let name = std::env::var(format!("{prefix}_NAME")).unwrap_or_else(|_| prefix.to_string());
    let key = std::env::var(format!("{prefix}_KEY")).unwrap_or_default();
    Some(HttpBackend::new(client.clone(), url, key, name))
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let path = std::env::args()
        .nth(1)
        .context("usage: compare <dataset.json>")?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
    let recall_limit: i64 = match std::env::var("MNESTIC_EVAL_RECALL_LIMIT") {
        Ok(v) => v.parse().context("MNESTIC_EVAL_RECALL_LIMIT must be an integer")?,
        Err(_) => DEFAULT_RECALL_LIMIT,
    };

    let cases = dataset::load(&PathBuf::from(&path))?;
    eprintln!("loaded {} cases from {path}", cases.len());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("building the http client")?;
    let mut owned: Vec<HttpBackend> = Vec::new();
    if let Some(b) = backend_from_env("MNESTIC_COMPARE_A", &client) {
        owned.push(b);
    }
    if let Some(b) = backend_from_env("MNESTIC_COMPARE_B", &client) {
        owned.push(b);
    }
    if owned.is_empty() {
        anyhow::bail!("set at least MNESTIC_COMPARE_A_URL (and a KEY); B is optional");
    }
    let backends: Vec<&dyn MemoryBackend> = owned.iter().map(|b| b as &dyn MemoryBackend).collect();

    let answerer = AnthropicAnswerer::new(&anthropic_key);
    let judge = AnthropicJudge::new(&anthropic_key);

    let report = compare(&backends, &answerer, &judge, recall_limit, &cases).await;

    print!("{}", render_markdown(&report));

    if !report.errors.is_empty() {
        eprintln!("\n{} error(s):", report.errors.len());
        for e in &report.errors {
            eprintln!("  {e}");
        }
    }

    // A backend that scored zero questions means the run is not comparable (bad URL,
    // auth, or every case failed to ingest). Fail loudly so a broken setup is not read
    // as a real result.
    let any_empty = report.per_backend.iter().any(|(_, rep)| rep.score.n == 0);
    if any_empty {
        eprintln!("a backend scored zero questions; treating the run as failed");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}
