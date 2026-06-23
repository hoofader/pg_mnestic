// SPDX-License-Identifier: MIT

//! Cloud Anthropic (Claude) extractor. Gated behind the `anthropic` feature so the
//! default build makes no network calls. Anthropic has no embeddings endpoint, so
//! this provides an extractor only; pair it with a separate embedder.
//!
//! Rust has no official Anthropic SDK, so this calls the Messages API over raw HTTP
//! (`POST /v1/messages`). Output is constrained to the extraction schema via
//! `output_config.format`. Opus 4.8 rejects `temperature`/`budget_tokens`, so they
//! are not sent. Adaptive thinking is deliberately left off (the field is omitted):
//! extraction is schema-bound, so off-thinking keeps it cheaper and lower-latency.

use async_trait::async_trait;
use mnestic_core::{Candidate, Ctx, Error, Extractor, Result};
use serde::Deserialize;

use crate::extract_schema::{
    extraction_json_schema, http_client, into_candidate, send_with_retry, Extraction,
    EXTRACT_SYSTEM_PROMPT,
};

const DEFAULT_BASE: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-opus-4-8";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicExtractor {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: u32,
}

impl AnthropicExtractor {
    /// Defaults to Claude Opus 4.8; override with `with_model`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: DEFAULT_MODEL.to_string(),
            base_url: DEFAULT_BASE.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
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

    /// Raise the output cap for inputs that yield many memories (a `max_tokens` stop
    /// truncates the JSON and surfaces as a clear extraction error otherwise).
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: AnthropicUsage,
}

/// Token accounting from a Messages API response. Defaults to zero so a missing field never
/// fails the parse; the metric just reads zero.
#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[async_trait]
impl Extractor for AnthropicExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": EXTRACT_SYSTEM_PROMPT,
            "messages": [ { "role": "user", "content": text } ],
            "output_config": {
                "format": { "type": "json_schema", "schema": extraction_json_schema() }
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
        tracing::info!(
            target: "mnestic::tokens",
            provider = "anthropic",
            model = %self.model,
            op = "extract",
            input_tokens = message.usage.input_tokens,
            output_tokens = message.usage.output_tokens,
            "token usage"
        );
        // A refusal or a max_tokens truncation does not produce schema JSON; report it
        // plainly instead of letting it fall through to a confusing parse error.
        match message.stop_reason.as_deref() {
            Some("refusal") => return Err(Error::Extraction("model refused the request".into())),
            Some("max_tokens") => {
                return Err(Error::Extraction("output truncated at max_tokens".into()))
            }
            _ => {}
        }
        // Structured output lands in a text block; thinking blocks (if ever enabled)
        // precede it, so take the first text block rather than content[0].
        let raw = message
            .content
            .iter()
            .find(|b| b.kind == "text")
            .map(|b| b.text.clone())
            .ok_or_else(|| Error::Extraction("no text block in response".into()))?;
        let extraction: Extraction =
            serde_json::from_str(&raw).map_err(|e| Error::Serde(e.to_string()))?;
        Ok(extraction.memories.into_iter().map(into_candidate).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mnestic_core::Ctx;

    // Live, paid, non-deterministic: hits the real Messages API, so it is ignored by
    // default. Run with ANTHROPIC_API_KEY set:
    //   cargo test -p mnestic-model --features anthropic -- --ignored
    // Guards EXTRACT_SYSTEM_PROMPT: update phrasing must yield a single-valued triple
    // whose attribute canonicalizes to the prior fact's key, so the engine supersedes
    // the old value instead of keeping it.
    #[tokio::test]
    #[ignore]
    async fn update_phrasing_yields_single_valued_new_value() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let extractor = AnthropicExtractor::new(key);
        let cands = extractor
            .extract(
                "I left Acme and now work at Globex as a staff engineer.",
                &Ctx::default(),
            )
            .await
            .expect("extraction");
        let onto = mnestic_core::Ontology::starter();
        assert!(
            cands.iter().any(|c| c.single_valued
                && c.attribute.as_deref().is_some_and(|a| onto.canonical_attribute(a) == "employer")
                && c.value.as_deref().is_some_and(|v| v.to_lowercase().contains("globex"))),
            "expected a single-valued employer=Globex triple, got {cands:?}"
        );
    }

    // Live, paid, ignored (see above). Guards that salient assistant-provided content
    // is captured, not just user facts, so single-session-assistant questions have
    // something to recall.
    #[tokio::test]
    #[ignore]
    async fn assistant_provided_fact_is_extracted() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let extractor = AnthropicExtractor::new(key);
        let cands = extractor
            .extract(
                "user: Can you set the Sunday shift for the social media team?\n\
                 assistant: Done. Admon is assigned to the 8 am - 4 pm Day Shift on Sundays.",
                &Ctx::default(),
            )
            .await
            .expect("extraction");
        assert!(
            cands.iter().any(|c| c.content.to_lowercase().contains("admon")),
            "expected the assistant-provided shift fact to be captured, got {cands:?}"
        );
    }
}
