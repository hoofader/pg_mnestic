// SPDX-License-Identifier: AGPL-3.0-only

//! The `/v4/memories` save endpoint. Maps the supermemory wire fields onto an engine
//! `add`, scoping the actor and container tags from the `containerTag`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{resolve_container_tag, AppState};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMemoryRequest {
    pub content: String,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub custom_id: Option<String>,
    /// supermemory's `dreaming` mode (doc 04 §3): `instant` (default) extracts synchronously;
    /// `dynamic` enqueues and a worker extracts out of band, so the call returns fast.
    #[serde(default)]
    pub dreaming: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMemoryResponse {
    /// The source row id (the unit a later `forget` targets by custom_id).
    pub id: String,
    /// Echoed back so the caller sees the tag it sent (doc 04 §2 round-trip).
    pub container_tag: String,
    /// "saved" (sync), "queued" (dreaming: dynamic, extraction deferred to the worker), or
    /// "skipped" (an idempotent repeat of the same custom_id).
    pub status: String,
}

pub async fn add_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<AddMemoryResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;

    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("content is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);

    let dynamic = req
        .dreaming
        .as_deref()
        .map(|d| d.eq_ignore_ascii_case("dynamic"))
        .unwrap_or(false);
    if dynamic {
        // Dynamic ingestion enqueues the raw source for out-of-band extraction, which does not
        // yet carry the request's metadata, so it is dropped on this path for now.
        let enq = state
            .engine
            .enqueue(tenant, &actor_id, &container_tags, &req.content, "conversation", req.custom_id.as_deref())
            .await?;
        let status = if enq.queued { "queued" } else { "skipped" };
        return Ok(Json(AddMemoryResponse {
            id: enq.source_id.to_string(),
            container_tag: tag,
            status: status.to_string(),
        }));
    }

    let meta = normalize_metadata(req.metadata);
    let result = state
        .engine
        .add_at(
            tenant,
            &actor_id,
            &container_tags,
            &req.content,
            "conversation",
            req.custom_id.as_deref(),
            None,
            &meta,
        )
        .await?;

    let status = if result.idempotent_skip { "skipped" } else { "saved" };
    Ok(Json(AddMemoryResponse {
        id: result.source_id.to_string(),
        container_tag: tag,
        status: status.to_string(),
    }))
}

// The SDK's `client.memories.forget` -> DELETE /v4/memories. `id` targets one memory; `content`
// forgets by extracted key when no id is known. `reason` is recorded on the tombstone.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetRequest {
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetResponse {
    /// The forgotten memory id (the first removed, for a content-based forget).
    pub id: String,
    pub forgotten: bool,
}

pub async fn forget_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ForgetRequest>,
) -> Result<Json<ForgetResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags: _ } = parse_container_tag(&tag);

    if let Some(id_str) = req.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let id = uuid::Uuid::parse_str(id_str)
            .map_err(|_| ApiError::BadRequest("id is not a valid memory id".into()))?;
        let forgotten = state.engine.forget_by_id(tenant, &actor_id, id, req.reason.as_deref()).await?;
        return Ok(Json(ForgetResponse { id: id_str.to_string(), forgotten }));
    }

    if let Some(content) = req.content.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let ids = state.engine.forget_by_content(tenant, &actor_id, content).await?;
        let id = ids.first().map(|u| u.to_string()).unwrap_or_default();
        return Ok(Json(ForgetResponse { id, forgotten: !ids.is_empty() }));
    }

    Err(ApiError::BadRequest("forget requires id or content".into()))
}

// The SDK's `client.memories.updateMemory` -> PATCH /v4/memories. A versioned edit:
// `newContent` becomes a new memory version that supersedes the prior, identified by `id`.
// `content` is part of the wire contract but unused here (we identify by id alone).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMemoryRequest {
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    pub new_content: String,
    #[serde(default)]
    pub id: Option<String>,
    // Part of the SDK wire contract, but we identify the target by `id` alone, so the
    // accepted value is never read. Kept so a client sending it gets a 200, not a 422.
    #[serde(default)]
    #[allow(dead_code)]
    pub content: Option<String>,
    #[serde(default)]
    pub forget_after: Option<String>,
    #[serde(default)]
    pub forget_reason: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub temporal_context: Option<TemporalContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemporalContext {
    #[serde(default)]
    pub document_date: Option<String>,
    #[serde(default)]
    pub event_date: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMemoryResponse {
    pub id: String,
    pub created_at: String,
    pub forget_after: Option<String>,
    pub forget_reason: Option<String>,
    pub memory: String,
    pub parent_memory_id: Option<String>,
    pub root_memory_id: Option<String>,
    pub version: i32,
}

pub async fn update_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpdateMemoryRequest>,
) -> Result<Json<UpdateMemoryResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags: _ } = parse_container_tag(&tag);

    let new_content = req.new_content.trim();
    if new_content.is_empty() {
        return Err(ApiError::BadRequest("newContent is empty".into()));
    }

    let id_str = req
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::BadRequest("id is required".into()))?;
    let id = uuid::Uuid::parse_str(id_str)
        .map_err(|_| ApiError::BadRequest("id is not a valid memory id".into()))?;

    let forget_after = parse_rfc3339_opt(req.forget_after.as_deref(), "forgetAfter")?;
    let (document_date, event_date) = match req.temporal_context {
        Some(tc) => {
            let doc = parse_rfc3339_opt(tc.document_date.as_deref(), "temporalContext.documentDate")?;
            // The schema's event_date is a single timestamptz; take the first element.
            let first = tc.event_date.as_ref().and_then(|v| v.first()).map(String::as_str);
            let evt = parse_rfc3339_opt(first, "temporalContext.eventDate")?;
            (doc, evt)
        }
        None => (None, None),
    };

    let metadata = normalize_metadata(req.metadata);

    let updated = state
        .engine
        .update_memory(
            tenant,
            &actor_id,
            id,
            new_content,
            forget_after,
            req.forget_reason.as_deref(),
            &metadata,
            document_date,
            event_date,
        )
        .await?;

    let Some(v) = updated else {
        return Err(ApiError::NotFound);
    };
    Ok(Json(UpdateMemoryResponse {
        id: v.id.to_string(),
        created_at: v.created_at.to_rfc3339(),
        forget_after: v.forget_after.map(|t| t.to_rfc3339()),
        forget_reason: v.forget_reason,
        memory: new_content.to_string(),
        parent_memory_id: Some(v.parent_memory_id.to_string()),
        root_memory_id: Some(v.root_memory_id.to_string()),
        version: v.version,
    }))
}

/// Normalize the wire `metadata` to the value the store binds: an absent field or an explicit
/// null becomes the empty object, matching the column's NOT NULL default.
pub(crate) fn normalize_metadata(metadata: Option<serde_json::Value>) -> serde_json::Value {
    match metadata {
        Some(serde_json::Value::Null) | None => serde_json::json!({}),
        Some(v) => v,
    }
}

/// Parse an optional RFC3339 timestamp, treating an explicit JSON null (already mapped to
/// None by the caller) and an absent field alike. A present but unparseable value is a 400,
/// named so the caller knows which field tripped.
fn parse_rfc3339_opt(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, ApiError> {
    match raw {
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| Some(t.with_timezone(&chrono::Utc)))
            .map_err(|_| ApiError::BadRequest(format!("{field} is not a valid RFC3339 timestamp"))),
        None => Ok(None),
    }
}

#[derive(Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRequest {
    pub conversation_id: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationResponse {
    pub conversation_id: String,
    pub id: String,
    pub status: String,
}

pub async fn ingest_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConversationRequest>,
) -> Result<Json<ConversationResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);

    // Ingest the whole conversation in one extraction so the model sees the dialogue
    // context (evidence often spans a user+assistant pair), matching the eval's
    // per-session ingest. conversationId is the idempotency key: re-posting it, even a
    // grown thread, is skipped rather than appended, so a retry never duplicates;
    // re-ingest under a new conversationId.
    let text = req
        .messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(ApiError::BadRequest("messages have no content".into()));
    }

    let meta = normalize_metadata(req.metadata);
    let result = state
        .engine
        .add_at(
            tenant,
            &actor_id,
            &container_tags,
            &text,
            "conversation",
            Some(&req.conversation_id),
            None,
            &meta,
        )
        .await?;

    let status = if result.idempotent_skip { "skipped" } else { "ingested" };
    Ok(Json(ConversationResponse {
        conversation_id: req.conversation_id,
        id: result.source_id.to_string(),
        status: status.to_string(),
    }))
}
