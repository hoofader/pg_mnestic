// SPDX-License-Identifier: AGPL-3.0-only

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
use crate::filter::{matches, FilterNode};
use crate::{clamp_limit, resolve_container_tag, AppState};

/// Candidate pool when a `filters` tree is present. Filtering happens in Rust over the hits the
/// engine returns, not in SQL, so over-fetch and then retain matches. The `* 4` gives headroom
/// for a selective filter; `.min(200)` keeps the engine scan bounded.
fn filter_pool(limit: i64) -> i64 {
    limit.saturating_mul(4).min(200)
}

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
    /// Optional metadata-filter tree (sdk-ts `SearchMemoriesParams.filters`).
    #[serde(default)]
    pub filters: Option<FilterNode>,
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

/// `filters`, when present, is applied in Rust over the retrieved candidate pool, not pushed
/// into the SQL. At extreme scale a hit that matches the filter but ranks below the over-fetch
/// pool will not be seen, so filtering is best-effort against the top candidates.
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
    let limit = clamp_limit(req.limit);
    // Over-fetch only when a filter will thin the pool; without one, recall the exact `limit`.
    let recall_limit = if req.filters.is_some() { filter_pool(limit) } else { limit };
    let started = Instant::now();
    let hits = state
        .engine
        .recall_scoped(tenant, &actor_id, &container_tags, &req.q, recall_limit)
        .await?;
    let timing = started.elapsed().as_millis() as u64;
    let mut results: Vec<SearchResult> = hits.into_iter().map(to_result).collect();
    if let Some(filter) = &req.filters {
        results.retain(|r| matches(filter, &r.metadata));
        results.truncate(limit as usize);
    }
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
    /// Optional metadata-filter tree (sdk-ts `ProfileParams.filters`); applied to the recall
    /// results only, never to the profile's static/dynamic arrays.
    #[serde(default)]
    pub filters: Option<FilterNode>,
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

/// `filters`, when present, is applied in Rust over the recall pool under `searchResults`, not
/// pushed into the SQL. Same best-effort caveat as `/v4/search`: a match ranked below the
/// over-fetch pool is not seen. The profile's static/dynamic arrays are not filtered.
pub async fn profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let has_query = req.q.as_deref().is_some_and(|s| !s.trim().is_empty());
    let limit = clamp_limit(req.limit);
    // Only the recall under a query is filtered, so over-fetch only when both hold; a no-query
    // profile request never uses the recall, so the larger scan would be wasted.
    let recall_limit = if has_query && req.filters.is_some() { filter_pool(limit) } else { limit };
    let started = Instant::now();
    let ctx = state
        .engine
        .profile_query(tenant, &actor_id, &container_tags, req.q.as_deref().unwrap_or(""), recall_limit)
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let search_results = has_query.then(|| {
        let mut results: Vec<SearchResult> = ctx.relevant.into_iter().map(to_result).collect();
        if let Some(filter) = &req.filters {
            results.retain(|r| matches(filter, &r.metadata));
            results.truncate(limit as usize);
        }
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
