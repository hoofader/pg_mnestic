// SPDX-License-Identifier: Apache-2.0

//! Cloud-default OpenAI provider impls. Gated behind the `openai` feature so the
//! default build (and tests) make no network calls. Traits keep a local backend
//! a drop-in later.

use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{
    Candidate, Ctx, Embedder, Error, Extractor, MemType, Result, Temporal,
};
use serde::Deserialize;

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

// A request timeout matters because extraction/embedding run inside the engine's
// open transaction; a hung connection would otherwise pin a pooled connection.
// TODO(phase1): embedding batching and retry with backoff.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client builds from a static config")
}

pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiEmbedder {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let resp = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Provider(e.to_string()))?;
        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }
}

pub struct OpenAiExtractor {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiExtractor {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: http_client(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

/// Wire shape of one extracted memory (LLD §5.1). Deserialized from the model's
/// JSON, then mapped onto the domain `Candidate`.
#[derive(Deserialize)]
struct RawMemory {
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
struct Extraction {
    memories: Vec<RawMemory>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

fn parse_mem_type(s: &str) -> MemType {
    match s {
        "preference" => MemType::Preference,
        "episode" => MemType::Episode,
        _ => MemType::Fact,
    }
}

#[async_trait]
impl Extractor for OpenAiExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        let body = serde_json::json!({
            "model": self.model,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": EXTRACT_SYSTEM_PROMPT },
                { "role": "user", "content": text },
            ],
        });
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Provider(e.to_string()))?;
        let chat: ChatResponse = resp
            .json()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        let raw = chat
            .choices
            .first()
            .ok_or_else(|| Error::Extraction("empty choices".into()))?
            .message
            .content
            .clone();
        let extraction: Extraction = serde_json::from_str(&raw)
            .map_err(|e| Error::Serde(e.to_string()))?;
        Ok(extraction.memories.into_iter().map(into_candidate).collect())
    }
}

fn into_candidate(m: RawMemory) -> Candidate {
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

const EXTRACT_SYSTEM_PROMPT: &str = "Extract entity-centric memories from the user text. \
Return only JSON: { \"memories\": [ { \"content\": string, \"subject\": string|null, \
\"attribute\": string|null, \"value\": string|null, \"single_valued\": bool, \
\"mem_type\": \"fact\"|\"preference\"|\"episode\", \"confidence\": number, \
\"is_static\": bool } ] }.";
