// SPDX-License-Identifier: Apache-2.0

//! The read endpoints: `/v4/search` (recall) and `/v4/profile` (profile plus
//! query-relevant memories). Both scope the actor and container tags from the
//! `containerTag` and echo it back.

use std::time::Instant;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::RecallHit;
use serde::{Deserialize, Serialize};

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::{clamp_limit, resolve_container_tag, AppState};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    /// Cleartext content, or null for an encrypted-at-rest row.
    pub memory: Option<String>,
    /// The fused relevance score (wire name `similarity`, what the shells read).
    pub similarity: f64,
    /// System time of the row (doc 04 §4 hit field).
    pub updated_at: Option<String>,
    /// Caller metadata, `{}` until ingest stores any (sdk-ts types it required, nullable).
    pub metadata: serde_json::Value,
}

fn to_result(h: RecallHit) -> SearchResult {
    SearchResult {
        id: h.id.to_string(),
        memory: h.content,
        similarity: h.score,
        updated_at: h.recorded_at.map(|t| t.to_rfc3339()),
        metadata: h.metadata,
    }
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
    /// Wall-clock milliseconds for the recall (sdk-ts SearchMemoriesResponse.timing).
    pub timing: u64,
    pub total: usize,
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.q.trim().is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let started = Instant::now();
    let hits = state
        .engine
        .recall_scoped(tenant, &actor_id, &container_tags, &req.q, clamp_limit(req.limit))
        .await?;
    let timing = started.elapsed().as_millis() as u64;
    let results: Vec<SearchResult> = hits.into_iter().map(to_result).collect();
    let total = results.len();
    Ok(Json(SearchResponse { container_tag: tag, results, timing, total }))
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

// The supermemory SDK reads `profile.static` / `profile.dynamic` (sdk-ts ProfileResponse.Profile).
#[derive(Serialize)]
pub struct ProfileBody {
    #[serde(rename = "static")]
    pub static_facts: Vec<String>,
    #[serde(rename = "dynamic")]
    pub dynamic_ctx: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSearchResults {
    pub results: Vec<SearchResult>,
    /// Wall-clock milliseconds to build the profile and run recall.
    pub timing: u64,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResponse {
    pub container_tag: String,
    pub profile: ProfileBody,
    /// Present only when a query was given, mirroring the SDK's optional `searchResults`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_results: Option<ProfileSearchResults>,
}

pub async fn profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let has_query = req.q.as_deref().is_some_and(|s| !s.trim().is_empty());
    let started = Instant::now();
    let ctx = state
        .engine
        .profile_query(tenant, &actor_id, &container_tags, req.q.as_deref().unwrap_or(""), clamp_limit(req.limit))
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let search_results = has_query.then(|| {
        let results: Vec<SearchResult> = ctx.relevant.into_iter().map(to_result).collect();
        let total = results.len();
        ProfileSearchResults { results, timing: elapsed_ms, total }
    });
    Ok(Json(ProfileResponse {
        container_tag: tag,
        profile: ProfileBody {
            static_facts: ctx.profile.static_facts,
            dynamic_ctx: ctx.profile.dynamic_ctx,
        },
        search_results,
    }))
}
