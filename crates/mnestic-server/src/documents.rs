// SPDX-License-Identifier: AGPL-3.0-only

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
use crate::filter::{matches, FilterNode};
use crate::{clamp_limit, resolve_container_tag, AppState};

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

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
    /// Caller key-value metadata, stored on the document and returned by `/v3/search`.
    /// Stored as-is; the supermemory shape (string/number/boolean/string[]) is not enforced.
    #[serde(default = "empty_object")]
    pub metadata: serde_json::Value,
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
    Json(mut req): Json<IngestRequest>,
) -> Result<Json<IngestResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("content is empty".into()));
    }
    // An explicit `metadata: null` bypasses the serde default; map it to {} so it never
    // reaches the NOT NULL column as a SQL NULL.
    if req.metadata.is_null() {
        req.metadata = empty_object();
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
            &req.metadata,
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
    /// Optional metadata-filter tree (sdk-ts `SearchDocumentsParams.filters`), matched against
    /// each document's `metadata`.
    #[serde(default)]
    pub filters: Option<FilterNode>,
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

/// `filters`, when present, is applied in Rust to each grouped document's `metadata` over the
/// retrieved chunk pool, not pushed into the SQL. At extreme scale a document whose chunks all
/// rank below the over-fetch budget will not be seen, so filtering is best-effort against the
/// top candidates.
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
    // A filter retains only matching documents, so the grouped pool must be larger or filtering
    // starves the result. Raise the ceiling to 400 chunks when a filter is present.
    let chunk_ceiling = if req.filters.is_some() { 400 } else { 200 };
    let chunk_budget = (doc_limit as i64).saturating_mul(8).clamp(50, chunk_ceiling);
    let started = std::time::Instant::now();
    let hits = state
        .engine
        .search_documents(tenant, &actor_id, &container_tags, &req.q, chunk_budget)
        .await?;
    let timing = started.elapsed().as_millis() as u64;
    let mut results = group_by_document(hits);
    if let Some(filter) = &req.filters {
        results.retain(|d| matches(filter, &d.metadata));
    }
    results.truncate(doc_limit);
    let total = results.len();
    Ok(Json(DocSearchResponse { container_tag: tag, results, timing, total }))
}
