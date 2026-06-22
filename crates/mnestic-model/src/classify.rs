// SPDX-License-Identifier: AGPL-3.0-only

//! Cloud Anthropic (Claude) relation classifier. Gated behind the `anthropic` feature
//! so the default build makes no network calls. Given a new memory and a numbered list
//! of same-subject candidates, it returns the `extends`/`derives` edges. The engine
//! runs it post-commit and best-effort, so a slow or failed call only skips enrichment.
//!
//! Same raw-HTTP Messages API path as `AnthropicExtractor`: structured output via
//! `output_config.format`, no `temperature`/`budget_tokens` (rejected on Opus 4.8),
//! retries via the shared `send_with_retry`.

use async_trait::async_trait;
use mnestic_core::{Error, Relation, RelationClassifier, RelationEdge, Result};
use serde::Deserialize;

use crate::extract_schema::{http_client, send_with_retry};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-opus-4-8";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 1024;

pub struct AnthropicRelationClassifier {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl AnthropicRelationClassifier {
    /// Defaults to Claude Opus 4.8; override with `with_model`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE.to_string(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Edges {
    edges: Vec<RawEdge>,
}

#[derive(Deserialize)]
struct RawEdge {
    index: i64,
    relation: String,
}

const SYSTEM_PROMPT: &str = "You are given a NEW memory and a numbered list of candidate \
existing memories about the same subject. For each candidate the NEW memory extends or \
derives from, return its index and the relation. Use \"extends\" when the new memory adds \
detail to the candidate, and \"derives\" when the new memory is inferred from the \
candidate. Omit candidates that are unrelated, contradictory, or merely similar. \
Return only JSON: {\"edges\": [{\"index\": <i>, \"relation\": \"extends\"|\"derives\"}]}.";

/// Constrain the output to a list of `{index, relation}` so parsing cannot fail on prose.
fn edges_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "edges": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "index": { "type": "integer" },
                        "relation": { "type": "string", "enum": ["extends", "derives"] }
                    },
                    "required": ["index", "relation"]
                }
            }
        },
        "required": ["edges"]
    })
}

/// Map the model's edges onto `RelationEdge`, dropping out-of-range indices and any
/// relation string outside the two known tokens.
fn into_edges(raw: Vec<RawEdge>, n: usize) -> Vec<RelationEdge> {
    raw.into_iter()
        .filter_map(|e| {
            if e.index < 0 || e.index as usize >= n {
                return None;
            }
            let relation = match e.relation.as_str() {
                "extends" => Relation::Extends,
                "derives" => Relation::Derives,
                _ => return None,
            };
            Some(RelationEdge { index: e.index as usize, relation })
        })
        .collect()
}

#[async_trait]
impl RelationClassifier for AnthropicRelationClassifier {
    async fn classify(&self, memory: &str, candidates: &[String]) -> Result<Vec<RelationEdge>> {
        // No candidates means no possible edges, so skip the call entirely.
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let listed = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| format!("[{i}] {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        let user = format!("New memory: {memory}\n\nCandidates:\n{listed}");
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": DEFAULT_MAX_TOKENS,
            "system": SYSTEM_PROMPT,
            "messages": [ { "role": "user", "content": user } ],
            "output_config": {
                "format": { "type": "json_schema", "schema": edges_schema() }
            }
        });
        let resp = send_with_retry(|| {
            self.client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body)
        })
        .await?;
        let message: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        match message.stop_reason.as_deref() {
            Some("refusal") => return Err(Error::Provider("model refused the request".into())),
            Some("max_tokens") => return Err(Error::Provider("output truncated at max_tokens".into())),
            _ => {}
        }
        let raw = message
            .content
            .iter()
            .find(|b| b.kind == "text")
            .map(|b| b.text.clone())
            .ok_or_else(|| Error::Provider("no text block in response".into()))?;
        let parsed: Edges =
            serde_json::from_str(&raw).map_err(|e| Error::Serde(e.to_string()))?;
        Ok(into_edges(parsed.edges, candidates.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_edges_drops_out_of_range_and_unknown_relations() {
        let raw = vec![
            RawEdge { index: 0, relation: "extends".into() },
            RawEdge { index: 5, relation: "extends".into() }, // out of range
            RawEdge { index: 1, relation: "guesses".into() }, // unknown relation
            RawEdge { index: -1, relation: "derives".into() }, // negative
            RawEdge { index: 2, relation: "derives".into() },
        ];
        let edges = into_edges(raw, 3);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0], RelationEdge { index: 0, relation: Relation::Extends });
        assert_eq!(edges[1], RelationEdge { index: 2, relation: Relation::Derives });
    }
}
