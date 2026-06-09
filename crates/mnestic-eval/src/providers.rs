// SPDX-License-Identifier: Apache-2.0

//! Claude-backed providers for real benchmark runs: the answerer and judge that
//! score a run, plus the recall-quality rewriter and reranker the engine calls
//! during retrieval. Raw HTTP against the Messages API (no official Rust SDK).
//! Opus 4.8 by default; no `temperature` or `budget_tokens` (rejected on 4.8).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use mnestic_core::{Error as CoreError, QueryRewriter, Reranker, Result as CoreResult, Scored};
use serde_json::Value;

use crate::runner::{Answerer, Judge};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-opus-4-8";
const MAX_ATTEMPTS: u32 = 4;

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client builds from a static config")
}

/// One non-streaming completion, with bounded retry on 429/529 (the SDK retries
/// these; raw HTTP must do it itself). `format`, when set, constrains output to a
/// JSON schema. A refusal or `max_tokens` stop is an error, not silently parsed.
async fn complete(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    format: Option<Value>,
) -> Result<String> {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": [ { "role": "user", "content": user } ],
    });
    if !system.is_empty() {
        body["system"] = serde_json::json!(system);
    }
    if let Some(schema) = format {
        body["output_config"] = serde_json::json!({
            "format": { "type": "json_schema", "schema": schema }
        });
    }

    for attempt in 0..MAX_ATTEMPTS {
        let resp = client
            .post(MESSAGES_URL)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .context("anthropic request")?;
        let status = resp.status();

        if status.is_success() {
            let value: Value = resp.json().await.context("anthropic response json")?;
            match value["stop_reason"].as_str() {
                Some("refusal") => return Err(anyhow!("anthropic refused the request")),
                Some("max_tokens") => return Err(anyhow!("anthropic output truncated at max_tokens")),
                _ => {}
            }
            let text = value["content"]
                .as_array()
                .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
                .and_then(|b| b["text"].as_str())
                .ok_or_else(|| anyhow!("no text block in anthropic response"))?;
            return Ok(text.to_string());
        }

        let retryable = matches!(status.as_u16(), 429 | 529);
        if retryable && attempt + 1 < MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
            continue;
        }
        let detail = resp.text().await.unwrap_or_default();
        return Err(anyhow!("anthropic {status}: {detail}"));
    }
    Err(anyhow!("anthropic retries exhausted"))
}

pub struct AnthropicAnswerer {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicAnswerer {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[async_trait]
impl Answerer for AnthropicAnswerer {
    async fn answer(&self, question: &str, context: &[String]) -> Result<String> {
        let system = "Answer the question using ONLY the memory context provided. \
                      Be concise. If the context does not contain the answer, say you do not know.";
        let user = format!(
            "Memory context:\n- {}\n\nQuestion: {}",
            context.join("\n- "),
            question
        );
        complete(&self.client, &self.api_key, &self.model, system, &user, 512, None).await
    }
}

pub struct AnthropicJudge {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicJudge {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[async_trait]
impl Judge for AnthropicJudge {
    async fn judge(
        &self,
        question: &str,
        gold: &str,
        predicted: &str,
        category: Option<&str>,
        abstention: bool,
    ) -> Result<bool> {
        // Replicate LongMemEval's per-type judge prompt and its `'yes' in response`
        // parse, so the grade matches the published methodology. Two deviations,
        // documented: the judge model is Claude (not gpt-4o), and max_tokens is 64
        // (not 10) so Opus is not truncated before it emits the verdict.
        let prompt = judge_prompt(category, abstention, question, gold, predicted);
        let verdict = complete(&self.client, &self.api_key, &self.model, "", &prompt, 64, None).await?;
        Ok(verdict.to_lowercase().contains("yes"))
    }
}

/// The verbatim LongMemEval judge prompts, selected by abstention then question type.
/// Unknown/None types use the standard correctness prompt.
fn judge_prompt(category: Option<&str>, abstention: bool, question: &str, gold: &str, predicted: &str) -> String {
    if abstention {
        return format!(
            "I will give you an unanswerable question, an explanation, and a response from a model. \
             Please answer yes if the model correctly identifies the question as unanswerable. The \
             model could say that the information is incomplete, or some other information is given \
             but the asked information is not.\n\nQuestion: {question}\n\nExplanation: {gold}\n\n\
             Model Response: {predicted}\n\nDoes the model correctly identify the question as \
             unanswerable? Answer yes or no only."
        );
    }
    if category == Some("single-session-preference") {
        return format!(
            "I will give you a question, a rubric for desired personalized response, and a response \
             from a model. Please answer yes if the response satisfies the desired response. \
             Otherwise, answer no. The model does not need to reflect all the points in the rubric. \
             The response is correct as long as it recalls and utilizes the user's personal \
             information correctly.\n\nQuestion: {question}\n\nRubric: {gold}\n\nModel Response: \
             {predicted}\n\nIs the model response correct? Answer yes or no only."
        );
    }
    let base = "I will give you a question, a correct answer, and a response from a model. Please \
                answer yes if the response contains the correct answer. Otherwise, answer no. If the \
                response is equivalent to the correct answer or contains all the intermediate steps \
                to get the correct answer, you should also answer yes. If the response only contains \
                a subset of the information required by the answer, answer no.";
    let intro = match category {
        Some("temporal-reasoning") => format!(
            "{base} In addition, do not penalize off-by-one errors for the number of days. If the \
             question asks for the number of days/weeks/months, etc., and the model makes off-by-one \
             errors (e.g., predicting 19 days when the answer is 18), the model's response is still \
             correct."
        ),
        Some("knowledge-update") => "I will give you a question, a correct answer, and a response \
            from a model. Please answer yes if the response contains the correct answer. Otherwise, \
            answer no. If the response contains some previous information along with an updated \
            answer, the response should be considered as correct as long as the updated answer is \
            the required answer."
            .to_string(),
        // None (non-LongMemEval datasets) and any unknown type fall back to the
        // standard prompt. Upstream raises on an unknown type; softened here so a new
        // dataset still runs, trading a loud failure for a possibly-wrong prompt.
        _ => base.to_string(),
    };
    format!(
        "{intro}\n\nQuestion: {question}\n\nCorrect Answer: {gold}\n\nModel Response: {predicted}\n\n\
         Is the model response correct? Answer yes or no only."
    )
}

/// Expands a question into a retrieval query before recall, to widen lexical and
/// vector matching. The engine reranks the retrieved pool against the original
/// question, so expansion costs in-pool ordering (which rerank repairs), not the
/// final order. Its net effect on recall is what MEMORYBENCH_REWRITE measures.
pub struct AnthropicRewriter {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicRewriter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[async_trait]
impl QueryRewriter for AnthropicRewriter {
    async fn rewrite(&self, query: &str) -> CoreResult<String> {
        let system = "Rewrite the user's question into a search query for a personal \
                      memory store. Keep the original words, and add the concrete entities, \
                      dates, and synonyms it implies so keyword and vector search match more \
                      rows. Return only the rewritten query, with no preamble or quotes.";
        let expanded = complete(&self.client, &self.api_key, &self.model, system, query, 256, None)
            .await
            .map_err(|e| CoreError::Provider(e.to_string()))?;
        let expanded = expanded.trim();
        // A blank completion would erase the query and tank recall; a 4xx/timeout
        // already propagated above. Degrade to the original rather than search on "".
        if expanded.is_empty() {
            return Ok(query.to_string());
        }
        Ok(expanded.to_string())
    }
}

/// LLM-as-reranker: scores the candidate pool against the original question and
/// returns the candidates in relevance order. A top-k or partial ranking is fine;
/// the engine keeps any indices this omits, after the ranked ones.
pub struct AnthropicReranker {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl AnthropicReranker {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
}

#[derive(serde::Deserialize)]
struct Ranking {
    ranking: Vec<i64>,
}

/// Constrain the reranker output to a list of candidate indices, so parsing the
/// response cannot fail on prose or a wrapped explanation.
fn ranking_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "ranking": { "type": "array", "items": { "type": "integer" } }
        },
        "required": ["ranking"]
    })
}

/// Map the model's index ranking onto `Scored`, dropping out-of-range or repeated
/// indices. Score descends with rank, so a score-sorting consumer keeps the model's
/// order (today the engine orders by sequence and ignores the score).
fn score_ranking(ranking: &[i64], candidates: &[String]) -> Vec<Scored> {
    let n = candidates.len();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(ranking.len());
    for &idx in ranking {
        if idx < 0 {
            continue;
        }
        let i = idx as usize;
        if i < n && seen.insert(i) {
            out.push(Scored {
                index: i,
                content: candidates[i].clone(),
                score: (n - out.len()) as f32,
            });
        }
    }
    out
}

#[async_trait]
impl Reranker for AnthropicReranker {
    async fn rerank(&self, query: &str, candidates: &[String]) -> CoreResult<Vec<Scored>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let system = "Rank candidate memories by how well each one answers the question. \
                      Return JSON {\"ranking\": [i, ...]} listing the candidate indices from \
                      most to least relevant. Include every index exactly once.";
        let listed = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| format!("[{i}] {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        let user = format!("Question: {query}\n\nCandidates:\n{listed}");
        // Size the output cap to the pool: a truncated ranking is a max_tokens stop,
        // which complete() turns into a hard error that would drop the question from
        // the scored set. Structured output keeps the body to JSON indices only.
        let max_tokens = 1024u32.max(candidates.len() as u32 * 8);
        let raw = complete(
            &self.client,
            &self.api_key,
            &self.model,
            system,
            &user,
            max_tokens,
            Some(ranking_schema()),
        )
        .await
        .map_err(|e| CoreError::Provider(e.to_string()))?;
        let ranking: Ranking =
            serde_json::from_str(&raw).map_err(|e| CoreError::Serde(e.to_string()))?;
        Ok(score_ranking(&ranking.ranking, candidates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_ranking_drops_bad_indices_and_orders_by_position() {
        let candidates: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        // 2 then 0, with an out-of-range (9) and a repeat (2) that must be dropped.
        let scored = score_ranking(&[2, 0, 9, 2], &candidates);
        let order: Vec<usize> = scored.iter().map(|s| s.index).collect();
        assert_eq!(order, vec![2, 0]);
        assert_eq!(scored[0].content, "c");
        assert!(scored[0].score > scored[1].score, "earlier rank scores higher");
    }

    #[test]
    fn score_ranking_empty_when_nothing_valid() {
        let candidates: Vec<String> = vec!["a".into()];
        assert!(score_ranking(&[-1, 5], &candidates).is_empty());
    }
}
