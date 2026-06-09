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

/// Reorders candidates by relevance to the query, returning `Scored` carrying each
/// candidate's input `index`. May return a prefix (a top-k reranker); the caller is
/// expected to keep any omitted candidates after the reranked ones.
#[async_trait]
pub trait Reranker: Send + Sync {
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<Scored>>;
}

/// Rewrites or expands a query before retrieval (e.g. "auth" -> "auth login oauth
/// jwt"), to raise lexical and vector recall. Reranking still scores against the
/// user's original query.
#[async_trait]
pub trait QueryRewriter: Send + Sync {
    async fn rewrite(&self, query: &str) -> Result<String>;
}
