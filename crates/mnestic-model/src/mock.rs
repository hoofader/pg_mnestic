// SPDX-License-Identifier: Apache-2.0

//! Deterministic, network-free provider impls. Always built so the default test
//! path stays offline.

use async_trait::async_trait;
use mnestic_core::{
    Candidate, Ctx, Embedder, Extractor, MemType, QueryRewriter, Reranker, Result, Scored, Temporal,
};

pub const MOCK_DIM: usize = 1536;

/// Maps text to a fixed-dim vector via a cheap rolling hash, so the same input
/// always yields the same embedding without a model.
pub struct MockEmbedder;

fn hash_to_vec(text: &str, dim: usize) -> Vec<f32> {
    let mut h: u64 = 1469598103934665603; // FNV-1a offset basis
    let mut out = Vec::with_capacity(dim);
    for i in 0..dim {
        for b in text.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(1099511628211);
        }
        h ^= i as u64;
        h = h.wrapping_mul(1099511628211);
        // Map into [-1, 1].
        out.push((h % 2000) as f32 / 1000.0 - 1.0);
    }
    out
}

#[async_trait]
impl Embedder for MockEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_to_vec(t, MOCK_DIM)).collect())
    }
}

/// Returns a single trivial candidate echoing the input text.
pub struct MockExtractor;

#[async_trait]
impl Extractor for MockExtractor {
    async fn extract(&self, text: &str, _ctx: &Ctx) -> Result<Vec<Candidate>> {
        Ok(vec![Candidate {
            content: text.to_string(),
            subject: None,
            attribute: None,
            value: None,
            single_valued: false,
            mem_type: MemType::Fact,
            confidence: 0.5,
            is_static: false,
            temporal: Temporal::None,
            forget_after: None,
        }])
    }
}

/// Identity reranker: preserves input order with a descending score so callers
/// can exercise the rerank path without a model.
pub struct MockReranker;

#[async_trait]
impl Reranker for MockReranker {
    async fn rerank(&self, _query: &str, candidates: &[String]) -> Result<Vec<Scored>> {
        let n = candidates.len();
        Ok(candidates
            .iter()
            .enumerate()
            .map(|(i, c)| Scored {
                index: i,
                content: c.clone(),
                score: (n - i) as f32,
            })
            .collect())
    }
}

/// Identity query rewriter: returns the query unchanged.
pub struct MockRewriter;

#[async_trait]
impl QueryRewriter for MockRewriter {
    async fn rewrite(&self, query: &str) -> Result<String> {
        Ok(query.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embed_is_deterministic_and_fixed_dim() {
        let e = MockEmbedder;
        let a = e.embed(&["hello".to_string()]).await.unwrap();
        let b = e.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a[0].len(), MOCK_DIM);
    }

    #[tokio::test]
    async fn extract_returns_one_candidate() {
        let x = MockExtractor;
        let out = x.extract("user lives in SF", &Ctx::default()).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "user lives in SF");
    }

    #[tokio::test]
    async fn rerank_preserves_order() {
        let r = MockReranker;
        let out = r
            .rerank("q", &["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(out[0].index, 0);
        assert_eq!(out[0].content, "a");
        assert!(out[0].score > out[1].score);
    }
}
