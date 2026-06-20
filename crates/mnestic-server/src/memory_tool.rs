// SPDX-License-Identifier: Apache-2.0

//! The `/v4/memory` tool endpoint (doc 04 §3): one call with `action` save or forget.
//! Save is the same path as `/v4/memories`; forget is content-based (the engine extracts
//! the facts the text describes and tombstones the matching memories).

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{resolve_container_tag, AppState};

fn default_action() -> String {
    "save".to_string()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryToolRequest {
    #[serde(default = "default_action")]
    pub action: String,
    pub content: String,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub custom_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryToolResponse {
    pub action: String,
    pub status: String,
    /// The source id, on a save.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The ids a forget tombstoned, so the caller can see exactly what was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgotten: Option<Vec<String>>,
}

pub async fn memory_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<MemoryToolRequest>,
) -> Result<Json<MemoryToolResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.content.trim().is_empty() {
        return Err(ApiError::BadRequest("content is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);

    match req.action.as_str() {
        "save" => {
            let meta = crate::memories::normalize_metadata(req.metadata);
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
            Ok(Json(MemoryToolResponse {
                action: req.action,
                status: status.to_string(),
                id: Some(result.source_id.to_string()),
                forgotten: None,
            }))
        }
        "forget" => {
            if req.custom_id.is_some() {
                return Err(ApiError::BadRequest("customId is not used by forget".into()));
            }
            let ids = state.engine.forget_by_content(tenant, &actor_id, &req.content).await?;
            Ok(Json(MemoryToolResponse {
                action: req.action,
                status: "forgotten".to_string(),
                id: None,
                forgotten: Some(ids.iter().map(|id| id.to_string()).collect()),
            }))
        }
        other => Err(ApiError::BadRequest(format!(
            "unknown action {other:?} (use save or forget)"
        ))),
    }
}
