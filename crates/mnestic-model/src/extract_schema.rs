// SPDX-License-Identifier: MIT

//! JSON contract shared by the cloud extractor providers (OpenAI, Anthropic): the
//! wire shape the model returns, the mapping onto the domain `Candidate`, the
//! extraction prompt, and a shared HTTP client. Kept in one place so the providers
//! do not drift.

use std::time::Duration;

use mnestic_core::{Candidate, Error, MemType, Result, Temporal};
use serde::Deserialize;

/// A request timeout bounds a hung connection. Extraction and embedding run before the
/// write transaction opens, so a slow call or a retry backoff only delays the caller,
/// it does not pin a pooled connection.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client builds from a static config")
}

/// Send a request with bounded exponential backoff, rebuilding it each attempt via
/// `build`. Retries transient failures (a network/timeout send error, or 429/5xx/529)
/// so a blip mid-ingest does not abort the caller; a 4xx other than 429 is returned at
/// once via `ensure_success`. The raw HTTP path has no SDK retry, so this is it.
pub(crate) async fn send_with_retry<F>(build: F) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    const MAX_ATTEMPTS: u32 = 4;
    for attempt in 0..MAX_ATTEMPTS {
        let last = attempt + 1 == MAX_ATTEMPTS;
        match build().send().await {
            Ok(resp) => {
                let transient = matches!(resp.status().as_u16(), 408 | 429 | 500 | 502 | 503 | 529);
                if transient && !last {
                    backoff(attempt).await;
                    continue;
                }
                return ensure_success(resp).await;
            }
            // Only a timeout or failure to connect is worth retrying; a DNS, TLS, or
            // connection-refused error will not heal in a few seconds, so surface it now.
            Err(e) if !last && (e.is_timeout() || e.is_connect()) => backoff(attempt).await,
            Err(e) => return Err(Error::Provider(e.to_string())),
        }
    }
    unreachable!("loop returns on the last attempt")
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
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

pub(crate) const EXTRACT_SYSTEM_PROMPT: &str = "Extract memories from the conversation. \
Capture facts the user states about themselves, and salient information the assistant \
gave the user that may be asked about later (recommendations, plans, schedules, lists, \
figures, decisions). Record each as a self-contained `content` statement; skip \
greetings and filler. \
Return only JSON: { \"memories\": [ { \"content\": string, \"subject\": string|null, \
\"attribute\": string|null, \"value\": string|null, \"single_valued\": bool, \
\"mem_type\": \"fact\"|\"preference\"|\"episode\", \"confidence\": number, \
\"is_static\": bool } ] }. \
For a fact that holds one current value at a time (who the user works for, where they \
live, their job title, relationship status), set single_valued true and fill subject, \
attribute, and value, using a short lowercase attribute key such as \"employer\", \
\"location\", or \"role\". Attributes that can hold several values at once (languages, \
skills, hobbies, pets) stay single_valued false, even with words like \"now\" or \
\"also\". When the text reports a change (\"now\", \"moved to\", \"switched to\", \"no \
longer\", \"left X and joined Y\") to a single-valued fact, record the NEW current value \
as that triple, not just a note that it changed; the older value is replaced \
automatically.";

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

#[cfg(test)]
mod tests {
    use super::*;
    use mnestic_core::Ontology;

    #[test]
    fn prompt_steers_to_convergent_canonical_keys() {
        // The single-valued example keys named in the prompt must be the ontology's
        // canonical form AND converge with the surface forms the model uses elsewhere,
        // or an update lands under a different key and never supersedes the prior fact.
        let onto = Ontology::starter();
        for (key, surface) in [("employer", "works at"), ("location", "lives in"), ("role", "job title")] {
            assert!(EXTRACT_SYSTEM_PROMPT.contains(key), "prompt should name the {key:?} key");
            assert_eq!(onto.canonical_attribute(key), key, "{key:?} must be canonical");
            assert_eq!(onto.canonical_attribute(surface), key, "{surface:?} must converge to {key:?}");
        }
    }
}
