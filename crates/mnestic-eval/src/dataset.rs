// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Qa {
    pub question: String,
    pub answer: String,
}

/// A dated conversation session. `date` is when it happened (event time); the runner
/// passes it as the default `valid_from` so supersession and as-of queries order by
/// event time rather than ingest time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub date: Option<DateTime<Utc>>,
    pub turns: Vec<Turn>,
}

/// One benchmark item: prior dated sessions to ingest, then questions to answer from
/// memory. Mnestic's normalized shape; the LongMemEval converter (`longmemeval`)
/// produces it. The harness consumes the normalized form so the run loop stays
/// independent of each benchmark's on-disk schema.
///
/// Comparability caveats vs published LongMemEval numbers: the harness grades with a
/// generic correctness judge, not LongMemEval's per-type judge prompts (temporal
/// off-by-one tolerance, knowledge-update "accept the newer answer", preference
/// rubric), and abstention questions are not scored. So treat the output as a
/// Mnestic-internal MemScore, not an official LongMemEval score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub sessions: Vec<Session>,
    pub questions: Vec<Qa>,
}

/// Load a normalized dataset (a JSON array of `Case`).
pub fn load(path: &Path) -> Result<Vec<Case>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cases: Vec<Case> = serde_json::from_str(&text).context("parsing dataset JSON")?;
    Ok(cases)
}
