// SPDX-License-Identifier: Apache-2.0

//! JSON contract shared by the cloud extractor providers (OpenAI, Anthropic): the
//! wire shape the model returns, the mapping onto the domain `Candidate`, the
//! extraction prompt, and a shared HTTP client. Kept in one place so the providers
//! do not drift.

use std::time::Duration;

use mnestic_core::{Candidate, Error, MemType, Result, Temporal};
use serde::Deserialize;

/// A request timeout matters because extraction runs inside the engine's open
/// transaction; a hung connection would otherwise pin a pooled connection.
/// TODO(phase1): retry with backoff that distinguishes 429/529 from 4xx.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client builds from a static config")
}

/// Fold a non-2xx response into a provider error that carries the status AND the
/// body. The body is where a provider explains a 400 (bad schema, rejected param),
/// which `error_for_status` would discard.
pub(crate) async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Provider(format!("provider returned {status}: {body}")))
}

/// Wire shape of one extracted memory (LLD §5.1), mapped onto the domain `Candidate`.
#[derive(Deserialize)]
pub(crate) struct RawMemory {
    content: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    attribute: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    single_valued: bool,
    #[serde(default = "default_mem_type")]
    mem_type: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    is_static: bool,
}

fn default_mem_type() -> String {
    "fact".to_string()
}

fn default_confidence() -> f32 {
    0.5
}

#[derive(Deserialize)]
pub(crate) struct Extraction {
    pub(crate) memories: Vec<RawMemory>,
}

fn parse_mem_type(s: &str) -> MemType {
    match s {
        "preference" => MemType::Preference,
        "episode" => MemType::Episode,
        _ => MemType::Fact,
    }
}

pub(crate) fn into_candidate(m: RawMemory) -> Candidate {
    Candidate {
        content: m.content,
        subject: m.subject,
        attribute: m.attribute,
        value: m.value,
        single_valued: m.single_valued,
        mem_type: parse_mem_type(&m.mem_type),
        confidence: m.confidence,
        is_static: m.is_static,
        // Phase 1 wires temporal and forget extraction; the prompt does not ask for
        // them yet, so the bitemporal columns stay at their defaults for now.
        temporal: Temporal::None,
        forget_after: None,
    }
}

pub(crate) const EXTRACT_SYSTEM_PROMPT: &str = "Extract entity-centric memories from the user text. \
Return only JSON: { \"memories\": [ { \"content\": string, \"subject\": string|null, \
\"attribute\": string|null, \"value\": string|null, \"single_valued\": bool, \
\"mem_type\": \"fact\"|\"preference\"|\"episode\", \"confidence\": number, \
\"is_static\": bool } ] }.";

/// JSON Schema for the extraction output, for providers that constrain output to a
/// schema (the Anthropic Messages API `output_config.format`). Structured output
/// requires every property to be listed in `required` and `additionalProperties` to
/// be false; nullable fields use a `[type, "null"]` union.
#[cfg(feature = "anthropic")]
pub(crate) fn extraction_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "memories": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "content": { "type": "string" },
                        "subject": { "type": ["string", "null"] },
                        "attribute": { "type": ["string", "null"] },
                        "value": { "type": ["string", "null"] },
                        "single_valued": { "type": "boolean" },
                        "mem_type": { "type": "string", "enum": ["fact", "preference", "episode"] },
                        "confidence": { "type": "number" },
                        "is_static": { "type": "boolean" }
                    },
                    "required": [
                        "content", "subject", "attribute", "value",
                        "single_valued", "mem_type", "confidence", "is_static"
                    ]
                }
            }
        },
        "required": ["memories"]
    })
}
