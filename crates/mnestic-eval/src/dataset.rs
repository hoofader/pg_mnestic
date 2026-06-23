// SPDX-License-Identifier: MIT

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
    /// LongMemEval `question_type`, used to pick the per-type judge prompt. None for
    /// datasets that do not carry it (the standard correctness judge is used).
    #[serde(default)]
    pub question_type: Option<String>,
    /// True for LongMemEval abstention questions (question_id ends in `_abs`): the
    /// judge checks whether the model correctly declined, not whether it matched gold.
    #[serde(default)]
    pub abstention: bool,
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
/// The harness replicates LongMemEval's per-type judge prompts (standard, temporal
/// off-by-one tolerance, knowledge-update "accept the newer answer", preference
/// rubric) and scores abstention questions with the unanswerable-question prompt, so
/// the output tracks the published methodology and abstention is its own breakdown
/// bucket. Two deviations remain: the judge model is Claude, not the gpt-4o used
/// upstream, and recall recency ranks by ingest time (supersession orders by event
/// time). Treat the MemScore as close-but-not-official.
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
