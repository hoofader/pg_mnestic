// SPDX-License-Identifier: Apache-2.0

//! Bearer-token auth. The API key is the only thing that selects a tenant (doc 04 §2),
//! so this lookup is the security boundary; everything downstream is RLS-scoped to the
//! tenant it returns.

use axum::http::{header, HeaderMap};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;

/// Resolve the bearer token to its tenant by comparing the token's SHA-256 (computed in
/// the database via pgcrypto, so the cleartext token never lands in a query log as a
/// value to match) against the stored hash. Unknown or missing token is a 401.
pub async fn authenticate(pool: &PgPool, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let token = bearer_token(headers).ok_or(ApiError::Unauthorized)?;
    let tenant: Option<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM mnestic_api_key WHERE token_sha256 = digest($1, 'sha256')",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;
    tenant.ok_or(ApiError::Unauthorized)
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    // RFC 7235: the auth scheme is case-insensitive, and clients vary the spacing.
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}
