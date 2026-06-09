// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Qa {
    pub question: String,
    pub answer: String,
}

/// One benchmark item: prior conversation sessions to ingest, then questions to
/// answer from memory. This is Mnestic's normalized shape. Adapters that convert
/// LongMemEval-S and LoCoMo into this shape are a follow-up; the harness consumes
/// the normalized form so the run loop stays independent of each benchmark's schema.
///
/// Limitation: turns carry no timestamp, and ingest defaults each memory's
/// `valid_from` to write time. So temporal-reasoning question types (LongMemEval's
/// "when did X" / multi-session ordering) cannot be scored faithfully yet. Threading
/// per-session timestamps into `valid_time` is the prerequisite, and until then these
/// numbers are not directly comparable to published LongMemEval accuracy.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub sessions: Vec<Vec<Turn>>,
    pub questions: Vec<Qa>,
}

/// Load a normalized dataset (a JSON array of `Case`).
pub fn load(path: &Path) -> Result<Vec<Case>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cases: Vec<Case> = serde_json::from_str(&text).context("parsing dataset JSON")?;
    Ok(cases)
}
