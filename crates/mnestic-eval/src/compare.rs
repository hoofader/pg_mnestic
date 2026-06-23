// SPDX-License-Identifier: AGPL-3.0-only

//! Head-to-head comparison: run the SAME benchmark through several `MemoryBackend`s
//! and report both the quantitative `MemScore` per backend and a qualitative
//! per-question side-by-side. Pointed at pg_mnestic and supermemory over one wire, it
//! is apples-to-apples: identical cases, identical answerer and judge, only the memory
//! engine differs.

use std::fmt::Write as _;

use crate::backend::MemoryBackend;
use crate::dataset::Case;
use crate::runner::{ingest_cases, Answerer, Judge, RunReport};
use crate::score::QuestionResult;

/// One backend's outcome for a single question.
pub struct BackendAnswer {
    pub backend: String,
    pub correct: bool,
    pub recalled: Vec<String>,
    pub predicted: String,
}

/// One question, side by side across every backend that scored it.
pub struct ComparisonRow {
    pub case_id: String,
    pub question: String,
    pub gold: String,
    pub category: Option<String>,
    pub abstention: bool,
    pub answers: Vec<BackendAnswer>,
}

/// The full comparison: per-backend `MemScore` (in each `RunReport`), the aligned
/// per-question rows, and any alignment gaps (a backend that did not score a question
/// the others did) recorded as errors rather than silently dropped or misaligned.
pub struct ComparisonReport {
    pub per_backend: Vec<(String, RunReport)>,
    pub rows: Vec<ComparisonRow>,
    pub errors: Vec<String>,
}

impl ComparisonReport {
    /// Rows where the backends do not all agree on `correct`. This is the interesting
    /// subset a human reads first: where one engine got it and another did not.
    pub fn disagreements(&self) -> Vec<&ComparisonRow> {
        self.rows
            .iter()
            .filter(|row| {
                let mut answers = row.answers.iter();
                match answers.next() {
                    Some(first) => answers.any(|a| a.correct != first.correct),
                    None => false,
                }
            })
            .collect()
    }
}

/// Run `cases` through every backend and assemble the comparison. Each backend gets a
/// full `run_eval`-equivalent pass (so its `MemScore` is computed exactly as a solo
/// run would); the per-question results are then zipped per (case_id, question index).
/// A row is built only when EVERY backend produced a result for that question, so the
/// columns never misalign; a question any backend missed is counted as an error.
pub async fn compare(
    backends: &[&dyn MemoryBackend],
    answerer: &dyn Answerer,
    judge: &dyn Judge,
    recall_limit: i64,
    cases: &[Case],
) -> ComparisonReport {
    let mut per_backend: Vec<(String, RunReport)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // Key each result by (case_id, question index) so columns line up even when a
    // backend skips a case (its ingest failed) and produces fewer results. Question
    // text alone is not a key: a case may repeat a question.
    type Key = (String, usize);
    let mut by_backend: Vec<Vec<(Key, QuestionResult)>> = Vec::new();

    for backend in backends {
        let ingest = ingest_cases(*backend, cases).await;
        for e in &ingest.errors {
            errors.push(format!("[{}] {e}", backend.name()));
        }

        let mut results = Vec::new();
        let mut keyed: Vec<(Key, QuestionResult)> = Vec::new();
        let mut report_errors = Vec::new();
        for case in cases {
            if ingest.failed.contains(&case.id) {
                continue;
            }
            let actor = format!("case:{}", case.id);
            for (qi, qa) in case.questions.iter().enumerate() {
                match score_one(*backend, &actor, recall_limit, answerer, judge, qa).await {
                    Ok(result) => {
                        keyed.push(((case.id.clone(), qi), result.clone()));
                        results.push(result);
                    }
                    Err(e) => {
                        report_errors.push(format!("case {} q {:?}: {e:#}", case.id, qa.question));
                    }
                }
            }
        }
        for e in &report_errors {
            errors.push(format!("[{}] {e}", backend.name()));
        }
        let score = crate::score::MemScore::from_results(&results);
        per_backend.push((
            backend.name().to_string(),
            RunReport {
                results,
                errors: report_errors,
                score,
            },
        ));
        by_backend.push(keyed);
    }

    let rows = assemble_rows(cases, backends, &by_backend, &mut errors);
    ComparisonReport {
        per_backend,
        rows,
        errors,
    }
}

/// Walk the canonical (case, question) order and build a row only when every backend
/// has a result under that key. A missing key means a backend skipped or errored the
/// question; that is recorded as an error so the count is honest, not misaligned.
fn assemble_rows(
    cases: &[Case],
    backends: &[&dyn MemoryBackend],
    by_backend: &[Vec<((String, usize), QuestionResult)>],
    errors: &mut Vec<String>,
) -> Vec<ComparisonRow> {
    let mut rows = Vec::new();
    for case in cases {
        for (qi, qa) in case.questions.iter().enumerate() {
            let key = (case.id.clone(), qi);
            let mut answers = Vec::with_capacity(backends.len());
            let mut complete = true;
            for (b, backend) in backends.iter().enumerate() {
                match by_backend[b].iter().find(|(k, _)| *k == key) {
                    Some((_, r)) => answers.push(BackendAnswer {
                        backend: backend.name().to_string(),
                        correct: r.correct,
                        recalled: r.recalled.clone(),
                        predicted: r.predicted.clone(),
                    }),
                    None => {
                        complete = false;
                        errors.push(format!(
                            "[{}] missing result for case {} q {:?}; row dropped",
                            backend.name(),
                            case.id,
                            qa.question
                        ));
                    }
                }
            }
            if complete && !answers.is_empty() {
                rows.push(ComparisonRow {
                    case_id: case.id.clone(),
                    question: qa.question.clone(),
                    gold: qa.answer.clone(),
                    category: qa.question_type.clone(),
                    abstention: qa.abstention,
                    answers,
                });
            }
        }
    }
    rows
}

/// One question: recall, answer, judge, recording the qualitative payload. Mirrors the
/// runner's `score_question`, duplicated here so `compare` can key results as it goes.
async fn score_one(
    backend: &dyn MemoryBackend,
    actor: &str,
    recall_limit: i64,
    answerer: &dyn Answerer,
    judge: &dyn Judge,
    qa: &crate::dataset::Qa,
) -> anyhow::Result<QuestionResult> {
    let start = std::time::Instant::now();
    let context = backend.recall(actor, &qa.question, recall_limit).await?;
    let query_latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let recalled_context_tokens = context.iter().map(|c| c.chars().count()).sum::<usize>() / 4;
    let predicted = answerer.answer(&qa.question, &context).await?;
    let correct = judge
        .judge(
            &qa.question,
            &qa.answer,
            &predicted,
            qa.question_type.as_deref(),
            qa.abstention,
        )
        .await?;
    Ok(QuestionResult {
        correct,
        query_latency_ms,
        recalled_context_tokens,
        category: qa.question_type.clone(),
        abstention: qa.abstention,
        question: qa.question.clone(),
        gold: qa.answer.clone(),
        predicted,
        recalled: context,
    })
}

/// First line of a recalled snippet, capped, for a compact preview in the report.
fn preview(recalled: &[String]) -> String {
    if recalled.is_empty() {
        return "(none)".to_string();
    }
    let first = recalled[0].lines().next().unwrap_or("");
    let snip: String = first.chars().take(80).collect();
    let more = if recalled.len() > 1 {
        format!(" (+{} more)", recalled.len() - 1)
    } else {
        String::new()
    };
    format!("{snip}{more}")
}

/// Render the comparison as plain markdown: a per-backend summary table, a per-category
/// accuracy block, the disagreements (where the read is most useful), then the full
/// per-question dump.
pub fn render_markdown(report: &ComparisonReport) -> String {
    let mut out = String::new();
    out.push_str("# Backend comparison\n\n");

    // Summary table: one row per backend.
    out.push_str("## Summary\n\n");
    out.push_str("| backend | n | accuracy | avg latency (ms) | avg ctx tokens |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    for (name, rep) in &report.per_backend {
        let s = &rep.score;
        let _ = writeln!(
            out,
            "| {} | {} | {:.3} | {:.1} | {:.0} |",
            name, s.n, s.accuracy, s.avg_query_latency_ms, s.avg_recalled_context_tokens
        );
    }
    out.push('\n');

    // Per-category accuracy, one block per backend.
    out.push_str("## Per-category accuracy\n\n");
    for (name, rep) in &report.per_backend {
        let _ = writeln!(out, "### {name}\n");
        if rep.score.per_type.is_empty() {
            out.push_str("(no typed questions)\n\n");
            continue;
        }
        out.push_str("| category | n | accuracy |\n");
        out.push_str("| --- | --- | --- |\n");
        for t in &rep.score.per_type {
            let _ = writeln!(out, "| {} | {} | {:.3} |", t.question_type, t.n, t.accuracy);
        }
        out.push('\n');
    }

    // Disagreements: the rows where backends split on correctness.
    let disagreements = report.disagreements();
    let _ = writeln!(out, "## Disagreements ({})\n", disagreements.len());
    if disagreements.is_empty() {
        out.push_str("(none: the backends agreed on every scored question)\n\n");
    } else {
        for row in &disagreements {
            render_row(&mut out, row);
        }
    }

    // Full per-question dump for the complete read.
    out.push_str("## All questions\n\n");
    for row in &report.rows {
        render_row(&mut out, row);
    }

    if !report.errors.is_empty() {
        let _ = writeln!(out, "## Errors ({})\n", report.errors.len());
        for e in &report.errors {
            let _ = writeln!(out, "- {e}");
        }
        out.push('\n');
    }

    out
}

fn render_row(out: &mut String, row: &ComparisonRow) {
    let cat = row.category.as_deref().unwrap_or("untyped");
    let abs = if row.abstention { " [abstention]" } else { "" };
    let _ = writeln!(out, "### `{}` ({cat}{abs})\n", row.case_id);
    let _ = writeln!(out, "- **Q:** {}", row.question);
    let _ = writeln!(out, "- **Gold:** {}", row.gold);
    for a in &row.answers {
        let mark = if a.correct { "[correct]" } else { "[wrong]" };
        let _ = writeln!(out, "- **{}** {mark}", a.backend);
        let _ = writeln!(out, "  - answer: {}", a.predicted);
        let _ = writeln!(out, "  - recalled: {}", preview(&a.recalled));
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::dataset::{Qa, Session, Turn};
    use crate::mock::{EchoAnswerer, SubstringJudge};

    /// In-memory backend over a per-actor list of joined session texts. `exact`
    /// switches recall between substring-of-query matching (strict) and any-word
    /// matching (loose), so two instances DISAGREE on at least one question.
    struct MockBackend {
        name: String,
        exact: bool,
        store: Mutex<HashMap<String, Vec<String>>>,
    }

    impl MockBackend {
        fn new(name: &str, exact: bool) -> Self {
            Self {
                name: name.to_string(),
                exact,
                store: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryBackend for MockBackend {
        fn name(&self) -> &str {
            &self.name
        }

        async fn ingest_case(&self, actor: &str, case: &Case) -> anyhow::Result<()> {
            let mut store = self.store.lock().unwrap();
            let bucket = store.entry(actor.to_string()).or_default();
            for session in &case.sessions {
                let text = session
                    .turns
                    .iter()
                    .map(|t| format!("{}: {}", t.role, t.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    bucket.push(text);
                }
            }
            Ok(())
        }

        async fn recall(&self, actor: &str, query: &str, limit: i64) -> anyhow::Result<Vec<String>> {
            let store = self.store.lock().unwrap();
            let bucket = match store.get(actor) {
                Some(b) => b,
                None => return Ok(Vec::new()),
            };
            let ql = query.to_lowercase();
            let words: Vec<&str> = ql.split_whitespace().collect();
            let mut out = Vec::new();
            for text in bucket {
                let tl = text.to_lowercase();
                let hit = if self.exact {
                    // Strict: the whole query must appear verbatim.
                    tl.contains(&ql)
                } else {
                    // Loose: any query word matches.
                    words.iter().any(|w| tl.contains(w))
                };
                if hit {
                    out.push(text.clone());
                }
                if out.len() as i64 >= limit {
                    break;
                }
            }
            Ok(out)
        }
    }

    fn cases() -> Vec<Case> {
        let session = |content: &str| Session {
            date: None,
            turns: vec![Turn {
                role: "user".to_string(),
                content: content.to_string(),
            }],
        };
        vec![
            Case {
                id: "loc".to_string(),
                sessions: vec![session("I live in Berlin and work as a chef.")],
                questions: vec![Qa {
                    question: "Berlin".to_string(),
                    answer: "Berlin".to_string(),
                    question_type: Some("single-session-user".to_string()),
                    abstention: false,
                }],
            },
            // The loose backend recalls on "city" -> "live in Berlin"; the strict one
            // needs the verbatim query, recalls nothing, and gets it wrong. A forced
            // disagreement.
            Case {
                id: "city".to_string(),
                sessions: vec![session("I live in Berlin.")],
                questions: vec![Qa {
                    question: "city live".to_string(),
                    answer: "Berlin".to_string(),
                    question_type: Some("single-session-user".to_string()),
                    abstention: false,
                }],
            },
        ]
    }

    #[tokio::test]
    async fn compares_two_backends_and_finds_a_disagreement() {
        let loose = MockBackend::new("loose", false);
        let strict = MockBackend::new("strict", true);
        let backends: Vec<&dyn MemoryBackend> = vec![&loose, &strict];
        let cases = cases();

        let report = compare(&backends, &EchoAnswerer, &SubstringJudge, 10, &cases).await;

        // Both backends scored, each with a MemScore.
        assert_eq!(report.per_backend.len(), 2);
        for (_, rep) in &report.per_backend {
            assert!(rep.score.n > 0, "each backend should score questions");
        }
        assert!(!report.rows.is_empty(), "rows should be populated");

        // The "city live" question splits the backends.
        let disagreements = report.disagreements();
        assert!(
            !disagreements.is_empty(),
            "the loose/strict pair must disagree on at least one question"
        );

        let md = render_markdown(&report);
        assert!(md.contains("loose"), "markdown names the loose backend");
        assert!(md.contains("strict"), "markdown names the strict backend");
        assert!(md.contains("Disagreement"), "markdown has a Disagreements section");
    }
}
