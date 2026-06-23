// SPDX-License-Identifier: MIT

//! The document/RAG endpoints: `POST /v3/documents` (ingest) and `POST /v3/search`
//! (chunk search). Same auth and containerTag scoping as the v4 memory endpoints.
//! `taskType` routes the ingest (doc 04 §3): the default `memory` runs extraction so the
//! save is recallable via `/v4/search`; `superrag` keeps the chunk path that `/v3/search`
//! reads.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::ChunkHit;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::filter::{to_meta_filter, FilterNode};
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
    /// Routes the ingest (doc 04 §3). Absent or `memory` (the default) runs memory extraction so
    /// the save is recallable via `/v4/search`; `superrag` takes the chunk path that `/v3/search`
    /// reads. Lenient like `searchMode`: an unknown value falls back to the extraction default.
    #[serde(default)]
    pub task_type: Option<String>,
    // supermemory's `entityContext` (doc 04 §3) is accepted and ignored: we don't declare it, so
    // serde drops it (no `deny_unknown_fields`) rather than rejecting a body that carries it.
    /// Caller key-value metadata, stored on the document and returned by `/v3/search`.
    /// Stored as-is; the supermemory shape (string/number/boolean/string[]) is not enforced.
    #[serde(default = "empty_object")]
    pub metadata: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    /// The id of what was ingested: the source id on the memory path, the document id on the
    /// superrag path. The SDK types the add response `id` as a string, and it is present on both a
    /// fresh ingest and an idempotent skip (resolved from the prior source/document on a skip).
    pub id: Option<String>,
    pub status: String,
    /// Zero on the memory path (extraction produces memories, not chunks).
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

    // Only `superrag` takes the chunk path; everything else (absent, `memory`, or an unknown
    // value) extracts, so the primary shell's `client.add` is recallable via `/v4/search`.
    let superrag = req.task_type.as_deref() == Some("superrag");
    // `title`/`uri` are document (RAG) fields. Reject them off the superrag path rather than drop
    // them silently, so a caller that needs them learns they must use `taskType: superrag`.
    if !superrag && (req.title.is_some() || req.uri.is_some()) {
        return Err(ApiError::BadRequest("title and uri require taskType=superrag".into()));
    }

    if superrag {
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

        let status = if result.idempotent_skip { "skipped" } else { "ingested" };
        return Ok(Json(IngestResponse {
            id: Some(result.document_id.to_string()),
            status: status.to_string(),
            chunks: result.chunk_ids.len(),
        }));
    }

    // The default path extracts in-request: this calls the model synchronously (latency and
    // token cost), and unlike `/v4/memories` there is no `dreaming: dynamic` async option here
    // yet. `kind = "document"` (the source CHECK allows it) so provenance and the `custom_id`
    // idempotency key reflect that this came in over `/v3/documents`.
    let result = state
        .engine
        .add_at(
            tenant,
            &actor_id,
            &container_tags,
            &req.content,
            "document",
            req.custom_id.as_deref(),
            None,
            &req.metadata,
        )
        .await?;

    let status = if result.idempotent_skip { "skipped" } else { "ingested" };
    Ok(Json(IngestResponse {
        id: Some(result.source_id.to_string()),
        status: status.to_string(),
        chunks: 0,
    }))
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

/// `filters`, when present, is pushed into the chunk-search SQL over the document's `metadata`,
/// so it is exact (the same machinery the memory recall path uses). Chunks are still over-fetched
/// and grouped, because several top chunks can collapse into one document.
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
    let meta_filter = req.filters.as_ref().map(to_meta_filter);
    let started = std::time::Instant::now();
    let hits = state
        .engine
        .search_documents(tenant, &actor_id, &container_tags, &req.q, chunk_budget, meta_filter.as_ref())
        .await?;
    let timing = started.elapsed().as_millis() as u64;
    let mut results = group_by_document(hits);
    results.truncate(doc_limit);
    let total = results.len();
    Ok(Json(DocSearchResponse { container_tag: tag, results, timing, total }))
}
