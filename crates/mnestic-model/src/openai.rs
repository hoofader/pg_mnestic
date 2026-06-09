// SPDX-License-Identifier: Apache-2.0

//! Cloud OpenAI provider impls (embeddings + extraction). Gated behind the `openai`
//! feature so the default build (and tests) make no network calls. Traits keep a
//! local backend a drop-in later.

use async_trait::async_trait;
use mnestic_core::{Candidate, Ctx, Embedder, Error, Extractor, Result};
use serde::Deserialize;

use crate::extract_schema::{
    ensure_success, http_client, into_candidate, Extraction, EXTRACT_SYSTEM_PROMPT,
};

const DEFAULT_BASE: &str = "https://api.openai.com/v1";

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
            .map_err(|e| Error::Provider(e.to_string()))?;
        let resp = ensure_success(resp).await?;
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
            .map_err(|e| Error::Provider(e.to_string()))?;
        let resp = ensure_success(resp).await?;
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
        let extraction: Extraction =
            serde_json::from_str(&raw).map_err(|e| Error::Serde(e.to_string()))?;
        Ok(extraction.memories.into_iter().map(into_candidate).collect())
    }
}
