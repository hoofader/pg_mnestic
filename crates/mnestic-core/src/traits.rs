// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{Candidate, Scored};

/// Context handed to extraction (actor, container, temporal hints). Phase 0
/// keeps it minimal; resolution-quality fields (ontology, prior facts) land in
/// Phase 1.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub actor_id: String,
    pub container_tags: Vec<String>,
}

#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

#[async_trait]
pub trait Extractor: Send + Sync {
    async fn extract(&self, text: &str, ctx: &Ctx) -> Result<Vec<Candidate>>;
}

#[async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<Scored>>;
}
