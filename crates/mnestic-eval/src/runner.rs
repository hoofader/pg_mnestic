// SPDX-License-Identifier: AGPL-3.0-only

use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use mnestic_engine::Engine;
use uuid::Uuid;

use crate::dataset::{Case, Qa};
use crate::score::{MemScore, QuestionResult};

/// Produces an answer to a question given the recalled memory context.
#[async_trait]
pub trait Answerer: Send + Sync {
    async fn answer(&self, question: &str, context: &[String]) -> Result<String>;
}

/// Grades a predicted answer. `category` is the question type (for per-type judge
/// prompts) and `abstention` flags an unanswerable question (the judge then checks
/// whether the model correctly declined rather than matching `gold`).
#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(
        &self,
        question: &str,
        gold: &str,
        predicted: &str,
        category: Option<&str>,
        abstention: bool,
    ) -> Result<bool>;
}

pub struct RunReport {
    pub results: Vec<QuestionResult>,
    /// Per-case / per-question failures (a provider 4xx/5xx, a timeout). Captured so
    /// one transient blip does not discard a long, paid run; these are NOT counted
    /// in `score` (the operator decides whether to re-run the failed items).
    pub errors: Vec<String>,
    pub score: MemScore,
}

/// Outcome of the ingest phase: any per-case ingest errors and the ids of cases that
/// failed (so the evaluate phase can skip their questions).
pub struct IngestOutcome {
    pub errors: Vec<String>,
    pub failed: HashSet<String>,
}

/// Ingest every case's sessions into memory. This is the expensive, mode-independent
/// phase (one extraction call per session); run it once, then evaluate the same stored
/// memory under several recall modes with `evaluate_cases`.
pub async fn ingest_cases(engine: &Engine, tenant_id: Uuid, cases: &[Case]) -> IngestOutcome {
    let mut errors = Vec::new();
    let mut failed = HashSet::new();
    for case in cases {
        let actor = format!("case:{}", case.id);
        if let Err(e) = ingest_case(engine, tenant_id, &actor, case).await {
            errors.push(format!("case {}: ingest failed: {e:#}", case.id));
            failed.insert(case.id.clone());
        }
    }
    IngestOutcome { errors, failed }
}

/// Answer and grade every question against already-ingested memory. The `engine`
/// carries the recall mode (its optional rewriter/reranker), so calling this with
/// engines that differ only in those, over the same tenant, measures their effect on
/// identical memory. Cases in `failed` (ingest failed) are skipped, not scored.
pub async fn evaluate_cases(
    engine: &Engine,
    tenant_id: Uuid,
    answerer: &dyn Answerer,
    judge: &dyn Judge,
    recall_limit: i64,
    cases: &[Case],
    failed: &HashSet<String>,
) -> RunReport {
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for case in cases {
        if failed.contains(&case.id) {
            continue;
        }
        let actor = format!("case:{}", case.id);
        for qa in &case.questions {
            match score_question(engine, tenant_id, &actor, recall_limit, answerer, judge, qa).await
            {
                Ok(result) => results.push(result),
                Err(e) => errors.push(format!("case {} q {:?}: {e:#}", case.id, qa.question)),
            }
        }
    }
    let score = MemScore::from_results(&results);
    RunReport {
        results,
        errors,
        score,
    }
}

/// Ingest each case's sessions into memory, then answer its questions from recall and
/// grade them, in one pass with the engine's configured recall mode. A failure
/// ingesting a case skips that case's questions; a failure on a single question is
/// recorded and the run continues. The returned report always reflects the work that
/// succeeded, so a mid-run error never loses progress.
pub async fn run_eval(
    engine: &Engine,
    tenant_id: Uuid,
    answerer: &dyn Answerer,
    judge: &dyn Judge,
    recall_limit: i64,
    cases: &[Case],
) -> RunReport {
    let ingest = ingest_cases(engine, tenant_id, cases).await;
    let mut report =
        evaluate_cases(engine, tenant_id, answerer, judge, recall_limit, cases, &ingest.failed).await;
    // Surface ingest failures alongside the per-question ones, ingest first.
    let mut errors = ingest.errors;
    errors.append(&mut report.errors);
    report.errors = errors;
    report
}

async fn ingest_case(engine: &Engine, tenant_id: Uuid, actor: &str, case: &Case) -> Result<()> {
    let tags: Vec<String> = Vec::new();
    for session in &case.sessions {
        // Ingest the whole session in one extraction call, not turn-by-turn: it gives
        // the extractor the dialogue context (evidence often spans a user+assistant
        // pair) and avoids one Opus call per turn on a large haystack. The session
        // date flows in as `as_of`, so a fact's valid_from is when it was said. This
        // drives supersession event-ordering; recall recency still uses ingest time.
        let text = session
            .turns
            .iter()
            .map(|t| format!("{}: {}", t.role, t.content))
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        engine
            .add_at(tenant_id, actor, &tags, &text, "conversation", None, session.date, &serde_json::json!({}))
            .await?;
    }
    Ok(())
}

async fn score_question(
    engine: &Engine,
    tenant_id: Uuid,
    actor: &str,
    recall_limit: i64,
    answerer: &dyn Answerer,
    judge: &dyn Judge,
    qa: &Qa,
) -> Result<QuestionResult> {
    let start = std::time::Instant::now();
    let hits = engine.recall(tenant_id, actor, &qa.question, recall_limit).await?;
    let query_latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    let context: Vec<String> = hits.iter().filter_map(|h| h.content.clone()).collect();
    let recalled_context_tokens = approx_tokens(&context);
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
    })
}

/// Rough token estimate (~4 chars/token). Real token counts need the provider's
/// counter; this is a stable proxy for the cost dimension across runs.
fn approx_tokens(context: &[String]) -> usize {
    context.iter().map(|c| c.chars().count()).sum::<usize>() / 4
}
