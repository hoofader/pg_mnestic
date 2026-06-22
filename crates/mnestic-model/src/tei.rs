// SPDX-License-Identifier: AGPL-3.0-only

//! Self-hosted reranker over a HuggingFace Text Embeddings Inference (TEI) sidecar.
//! Gated behind the `rerank` feature so the default build pulls no HTTP stack. Recall
//! reorders its candidate pool against the user's query without the text leaving the
//! operator's infrastructure.

use std::time::Duration;

use async_trait::async_trait;
use mnestic_core::{Error, Reranker, Result, Scored};
use serde::Deserialize;

pub struct TeiReranker {
    client: reqwest::Client,
    base_url: String,
}

impl TeiReranker {
    pub fn new(base_url: impl Into<String>) -> Self {
        // Trim a trailing slash so the joined `{base_url}/rerank` never doubles up.
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds from a static config");
        Self { client, base_url }
    }
}

/// One entry of TEI's rerank response. `text` is requested off (not sent), so it is
/// not parsed; the content is mapped back from the input candidates by `index`.
#[derive(Deserialize)]
struct RankEntry {
    index: usize,
    score: f32,
}

#[async_trait]
impl Reranker for TeiReranker {
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<Scored>> {
        // TEI rejects an empty `texts`; an empty pool also has nothing to reorder.
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let body = serde_json::json!({ "query": query, "texts": candidates });
        let resp = self
            .client
            .post(format!("{}/rerank", self.base_url))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            // The body is where TEI explains a rejection; keep it, matching the cloud providers.
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("reranker returned {status}: {text}")));
        }
        let ranked: Vec<RankEntry> = resp.json().await.map_err(|e| Error::Provider(e.to_string()))?;
        // TEI sorts descending by score; preserve that order. Drop an index the server
        // returned that is out of range rather than panic on a bad payload.
        Ok(ranked
            .into_iter()
            .filter(|e| e.index < candidates.len())
            .map(|e| Scored {
                index: e.index,
                content: candidates[e.index].clone(),
                score: e.score,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[tokio::test]
    async fn maps_response_into_scored_in_returned_order() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rerank"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = req.body_json().unwrap();
                assert_eq!(body["query"], "q", "the query is sent verbatim");
                assert_eq!(
                    body["texts"],
                    serde_json::json!(["first", "second", "third"]),
                    "the candidate texts are sent in order"
                );
                ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    { "index": 2, "score": 0.9 },
                    { "index": 0, "score": 0.5 }
                ]))
            })
            .mount(&server)
            .await;

        let reranker = TeiReranker::new(server.uri());
        let candidates =
            vec!["first".to_string(), "second".to_string(), "third".to_string()];
        let out = reranker.rerank("q", &candidates).await.unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].index, 2);
        assert_eq!(out[0].content, "third");
        assert_eq!(out[0].score, 0.9);
        assert_eq!(out[1].index, 0);
        assert_eq!(out[1].content, "first");
        assert_eq!(out[1].score, 0.5);
    }

    #[tokio::test]
    async fn empty_candidates_skip_the_call() {
        // No mock mounted, so any HTTP call would fail; an empty pool must short-circuit.
        let reranker = TeiReranker::new("http://127.0.0.1:1");
        let out = reranker.rerank("q", &[]).await.unwrap();
        assert!(out.is_empty());
    }
}
