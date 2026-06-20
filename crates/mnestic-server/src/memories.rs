// SPDX-License-Identifier: Apache-2.0

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

    let result = state
        .engine
        .add(
            tenant,
            &actor_id,
            &container_tags,
            &req.content,
            "conversation",
            req.custom_id.as_deref(),
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
    // re-ingest under a new conversationId. The wire `metadata` is accepted and ignored.
    let text = req
        .messages
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(ApiError::BadRequest("messages have no content".into()));
    }

    let result = state
        .engine
        .add(tenant, &actor_id, &container_tags, &text, "conversation", Some(&req.conversation_id))
        .await?;

    let status = if result.idempotent_skip { "skipped" } else { "ingested" };
    Ok(Json(ConversationResponse {
        conversation_id: req.conversation_id,
        id: result.source_id.to_string(),
        status: status.to_string(),
    }))
}
