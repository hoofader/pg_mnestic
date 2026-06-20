// SPDX-License-Identifier: Apache-2.0

//! The document/RAG endpoints: `POST /v3/documents` (ingest) and `POST /v3/search`
//! (chunk search). Same auth and containerTag scoping as the v4 memory endpoints;
//! documents are stored as chunks, not run through memory extraction.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::ChunkHit;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{clamp_limit, resolve_container_tag, AppState};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestRequest {
    pub content: String,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub custom_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    /// The document id (doc 04 §4 `{ id, status }`). Null on an idempotent skip, where
    /// the prior ingest's document id is not re-resolved.
    pub id: Option<String>,
    pub status: String,
    pub chunks: usize,
}

pub async fn ingest_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("content is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);

    let result = state
        .engine
        .ingest_document(
            tenant,
            &actor_id,
            &container_tags,
            req.title.as_deref(),
            req.uri.as_deref(),
            &req.content,
            req.custom_id.as_deref(),
        )
        .await?;

    let (id, status) = if result.idempotent_skip {
        (None, "skipped")
    } else {
        (Some(result.document_id.to_string()), "ingested")
    };
    Ok(Json(IngestResponse { id, status: status.to_string(), chunks: result.chunk_ids.len() }))
}

// The supermemory SDK groups matches per document (sdk-ts SearchDocumentsResponse.Result):
// each document carries its matching `chunks`, the best `score`, and document fields.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocChunk {
    pub content: String,
    /// All returned chunks are matches, so each is relevant; kept for SDK shape parity.
    pub is_relevant: bool,
    pub score: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentResult {
    pub document_id: String,
    pub chunks: Vec<DocChunk>,
    /// The document's best chunk score.
    pub score: f64,
    pub title: Option<String>,
    /// Documents carry no distinct type here; the field is present and nullable per the SDK.
    #[serde(rename = "type")]
    pub doc_type: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: Option<String>,
    /// Documents are immutable reference text, so updatedAt mirrors createdAt.
    pub updated_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocSearchRequest {
    pub q: String,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocSearchResponse {
    pub container_tag: String,
    pub results: Vec<DocumentResult>,
    pub timing: u64,
    pub total: usize,
}

/// Group score-ordered chunk hits into documents, preserving the order in which each
/// document first appears (so the best-scoring document leads).
fn group_by_document(hits: Vec<ChunkHit>) -> Vec<DocumentResult> {
    let mut order: Vec<String> = Vec::new();
    let mut by_doc: std::collections::HashMap<String, DocumentResult> = std::collections::HashMap::new();
    for h in hits {
        let doc_id = h.document_id.to_string();
        let created = h.document_created_at.map(|t| t.to_rfc3339());
        let entry = by_doc.entry(doc_id.clone()).or_insert_with(|| {
            order.push(doc_id.clone());
            DocumentResult {
                document_id: doc_id.clone(),
                chunks: Vec::new(),
                score: h.score,
                title: h.document_title.clone(),
                doc_type: None,
                metadata: h.document_metadata.clone(),
                created_at: created.clone(),
                updated_at: created.clone(),
            }
        });
        entry.score = entry.score.max(h.score);
        entry.chunks.push(DocChunk { content: h.content, is_relevant: true, score: h.score });
    }
    order.into_iter().filter_map(|id| by_doc.remove(&id)).collect()
}

pub async fn search_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DocSearchRequest>,
) -> Result<Json<DocSearchResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.q.trim().is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    // `limit` bounds documents, but several top chunks can collapse into one document, so
    // over-fetch chunks and truncate to `limit` documents after grouping. Bounded so a large
    // limit can't fan the chunk scan out without a ceiling.
    let doc_limit = clamp_limit(req.limit) as usize;
    let chunk_budget = (doc_limit as i64).saturating_mul(8).clamp(50, 200);
    let started = std::time::Instant::now();
    let hits = state
        .engine
        .search_documents(tenant, &actor_id, &container_tags, &req.q, chunk_budget)
        .await?;
    let timing = started.elapsed().as_millis() as u64;
    let mut results = group_by_document(hits);
    results.truncate(doc_limit);
    let total = results.len();
    Ok(Json(DocSearchResponse { container_tag: tag, results, timing, total }))
}
