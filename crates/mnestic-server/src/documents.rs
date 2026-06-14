// SPDX-License-Identifier: Apache-2.0

//! The document/RAG endpoints: `POST /v3/documents` (ingest) and `POST /v3/search`
//! (chunk search). Same auth and containerTag scoping as the v4 memory endpoints;
//! documents are stored as chunks, not run through memory extraction.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::ChunkHit;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate;
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
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentHit {
    pub id: String,
    pub document_id: String,
    /// The chunk text (doc 04 field map: the document path returns `chunk`, not `memory`).
    pub chunk: String,
    pub similarity: f64,
}

fn to_hit(h: ChunkHit) -> DocumentHit {
    DocumentHit {
        id: h.id.to_string(),
        document_id: h.document_id.to_string(),
        chunk: h.content,
        similarity: h.score,
    }
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
    pub results: Vec<DocumentHit>,
}

pub async fn search_documents(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DocSearchRequest>,
) -> Result<Json<DocSearchResponse>, ApiError> {
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
    if req.q.trim().is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let hits = state
        .engine
        .search_documents(tenant, &actor_id, &container_tags, &req.q, clamp_limit(req.limit))
        .await?;
    Ok(Json(DocSearchResponse {
        container_tag: tag,
        results: hits.into_iter().map(to_hit).collect(),
    }))
}
