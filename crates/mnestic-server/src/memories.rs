// SPDX-License-Identifier: Apache-2.0

//! The `/v4/memories` save endpoint. Maps the supermemory wire fields onto an engine
//! `add`, scoping the actor and container tags from the `containerTag`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate;
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMemoryResponse {
    /// The source row id (the unit a later `forget` targets by custom_id).
    pub id: String,
    /// Echoed back so the caller sees the tag it sent (doc 04 §2 round-trip).
    pub container_tag: String,
    /// "saved" or "skipped" (an idempotent repeat of the same custom_id).
    pub status: String,
}

pub async fn add_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AddMemoryRequest>,
) -> Result<Json<AddMemoryResponse>, ApiError> {
    let pool = state.engine.store().pool();
    let tenant = authenticate(pool, &headers).await?;

    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("content is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);

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
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
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
