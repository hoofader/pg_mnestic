// SPDX-License-Identifier: Apache-2.0

//! Identity and projects: `GET /v3/session` (key validation, returns the user) and
//! `GET /v3/projects` (the container tags in use). Both are read-only and auth-gated.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Serialize;

use crate::auth::authenticate;
use crate::error::ApiError;
use crate::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    /// The tenant's external id. Mnestic does not model email/name, so they are null.
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, ApiError> {
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
    let user_id = state.engine.store().tenant_external_id(tenant).await?.unwrap_or_default();
    Ok(Json(SessionResponse { user_id, email: None, name: None }))
}

pub async fn projects(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, ApiError> {
    let tenant = authenticate(state.engine.store().pool(), &headers).await?;
    let tags = state.engine.store().list_container_tags(tenant).await?;
    Ok(Json(tags))
}
