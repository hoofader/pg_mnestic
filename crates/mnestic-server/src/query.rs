// SPDX-License-Identifier: Apache-2.0

//! The read endpoints: `/v4/search` (recall) and `/v4/profile` (profile plus
//! query-relevant memories). Both scope the actor and container tags from the
//! `containerTag` and echo it back.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::RecallHit;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{resolve_container_tag, AppState};

/// Recall fan-out when the caller does not specify one, and the cap on what it may ask
/// for, so a client value cannot push a giant query or a negative SQL `LIMIT`.
const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    /// Cleartext content, or null for an encrypted-at-rest row.
    pub memory: Option<String>,
    /// The fused relevance score (wire name `similarity`, what the shells read).
    pub similarity: f64,
}

fn to_result(h: RecallHit) -> SearchResult {
    SearchResult { id: h.id.to_string(), memory: h.content, similarity: h.score }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
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
pub struct SearchResponse {
    pub container_tag: String,
    pub results: Vec<SearchResult>,
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
    if req.q.trim().is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let hits = state
        .engine
        .recall_scoped(tenant, &actor_id, &container_tags, &req.q, clamp_limit(req.limit))
        .await?;
    Ok(Json(SearchResponse {
        container_tag: tag,
        results: hits.into_iter().map(to_result).collect(),
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRequest {
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    /// Optional; a blank or absent query returns the profile without recall.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileBody {
    pub static_facts: Vec<String>,
    pub dynamic_ctx: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResponse {
    pub container_tag: String,
    pub profile: ProfileBody,
    pub results: Vec<SearchResult>,
}

pub async fn profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let ctx = state
        .engine
        .profile_query(tenant, &actor_id, &container_tags, req.q.as_deref().unwrap_or(""), clamp_limit(req.limit))
        .await?;
    Ok(Json(ProfileResponse {
        container_tag: tag,
        profile: ProfileBody {
            static_facts: ctx.profile.static_facts,
            dynamic_ctx: ctx.profile.dynamic_ctx,
        },
        results: ctx.relevant.into_iter().map(to_result).collect(),
    }))
}
