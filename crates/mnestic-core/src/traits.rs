// SPDX-License-Identifier: MIT

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{Candidate, RelationEdge, Scored};

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

/// Classifies how a new memory relates to existing same-subject memories, for the
/// `extends`/`derives` graph edges. Returns only the candidates that ARE related (by
/// their slice index); unrelated candidates are omitted. Runs post-commit and
/// best-effort, so an error here never fails the write that produced `memory`.
#[async_trait]
pub trait RelationClassifier: Send + Sync {
    async fn classify(&self, memory: &str, candidates: &[String]) -> Result<Vec<RelationEdge>>;
}
