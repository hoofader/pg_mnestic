// SPDX-License-Identifier: MIT

//! The memory backend the runner drives. One trait, two transports: in-process
//! `EngineBackend` and, behind `real`, the supermemory wire `HttpBackend`. The same
//! HTTP client pointed at two base URLs drives both pg_mnestic's server and
//! `api.supermemory.ai`, so the comparison is apples-to-apples over one protocol.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use mnestic_engine::Engine;
use uuid::Uuid;

use crate::dataset::Case;

/// Join a session's turns the way the engine path ingests them, so the HTTP path
/// sends the identical text and the only variable between backends is the engine.
pub(crate) fn join_session(turns: &[crate::dataset::Turn]) -> String {
    turns
        .iter()
        .map(|t| format!("{}: {}", t.role, t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A memory engine the eval can ingest into and recall from. `actor` namespaces one
/// case's memories (the runner uses `case:<id>`); on the wire it is the `containerTag`.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn ingest_case(&self, actor: &str, case: &Case) -> Result<()>;
    async fn recall(&self, actor: &str, query: &str, limit: i64) -> Result<Vec<String>>;
}

/// The in-process engine path. Carries the tenant and recall mode (its optional
/// rewriter/reranker), so two `EngineBackend`s differing only in those measure their
/// effect on identical memory.
pub struct EngineBackend {
    engine: Arc<Engine>,
    tenant_id: Uuid,
    name: String,
}

impl EngineBackend {
    pub fn new(engine: Arc<Engine>, tenant_id: Uuid, name: impl Into<String>) -> Self {
        Self {
            engine,
            tenant_id,
            name: name.into(),
        }
    }
}

#[async_trait]
impl MemoryBackend for EngineBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn ingest_case(&self, actor: &str, case: &Case) -> Result<()> {
        let tags: Vec<String> = Vec::new();
        for session in &case.sessions {
            // One extraction call per session, not per turn: it gives the extractor the
            // dialogue context (evidence often spans a user+assistant pair) and avoids one
            // Opus call per turn. The session date flows in as `as_of`, so a fact's
            // valid_from is when it was said; this drives supersession event-ordering.
            let text = join_session(&session.turns);
            if text.is_empty() {
                continue;
            }
            self.engine
                .add_at(
                    self.tenant_id,
                    actor,
                    &tags,
                    &text,
                    "conversation",
                    None,
                    session.date,
                    &serde_json::json!({}),
                )
                .await?;
        }
        Ok(())
    }

    async fn recall(&self, actor: &str, query: &str, limit: i64) -> Result<Vec<String>> {
        let hits = self.engine.recall(self.tenant_id, actor, query, limit).await?;
        Ok(hits.iter().filter_map(|h| h.content.clone()).collect())
    }
}

#[cfg(feature = "real")]
mod http {
    use super::*;

    /// The supermemory wire path. The same client pointed at pg_mnestic's server or
    /// `api.supermemory.ai` drives either; the comparison treats both HTTP backends
    /// identically so it stays fair.
    pub struct HttpBackend {
        client: reqwest::Client,
        base_url: String,
        api_key: String,
        name: String,
    }

    impl HttpBackend {
        pub fn new(
            client: reqwest::Client,
            base_url: impl Into<String>,
            api_key: impl Into<String>,
            name: impl Into<String>,
        ) -> Self {
            let base_url = base_url.into();
            // Trim a trailing slash so `{base}/v3/documents` never doubles up.
            let base_url = base_url.trim_end_matches('/').to_string();
            Self {
                client,
                base_url,
                api_key: api_key.into(),
                name: name.into(),
            }
        }
    }

    #[async_trait]
    impl MemoryBackend for HttpBackend {
        fn name(&self) -> &str {
            &self.name
        }

        async fn ingest_case(&self, actor: &str, case: &Case) -> Result<()> {
            for session in &case.sessions {
                // Same join as the engine path, so both ingest identical text. The wire
                // `add` has no event-time field, so a session's date is NOT sent: temporal
                // fidelity is reduced, but symmetrically across both HTTP backends, so the
                // comparison stays fair.
                let content = join_session(&session.turns);
                if content.is_empty() {
                    continue;
                }
                let url = format!("{}/v3/documents", self.base_url);
                let body = serde_json::json!({ "content": content, "containerTag": actor });
                let resp = self
                    .client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .json(&body)
                    .send()
                    .await?;
                let status = resp.status();
                if !status.is_success() {
                    let detail = resp.text().await.unwrap_or_default();
                    anyhow::bail!("ingest {status}: {detail}");
                }
            }
            Ok(())
        }

        async fn recall(&self, actor: &str, query: &str, limit: i64) -> Result<Vec<String>> {
            let url = format!("{}/v4/search", self.base_url);
            let body = serde_json::json!({ "q": query, "containerTag": actor, "limit": limit });
            let resp = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                anyhow::bail!("recall {status}: {detail}");
            }
            let value: serde_json::Value = resp.json().await?;
            let results = value["results"].as_array().cloned().unwrap_or_default();
            // `memory` is the recalled text; skip nulls rather than surfacing "null".
            Ok(results
                .iter()
                .filter_map(|r| r["memory"].as_str().map(str::to_string))
                .collect())
        }
    }
}

#[cfg(feature = "real")]
pub use http::HttpBackend;
