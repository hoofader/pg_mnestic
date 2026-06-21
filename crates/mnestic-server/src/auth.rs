// SPDX-License-Identifier: AGPL-3.0-only

//! Bearer-token auth. The API key is the only thing that selects a tenant (doc 04 §2),
//! so this lookup is the security boundary; everything downstream is RLS-scoped to the
//! tenant it returns.

use axum::http::{header, HeaderMap};
use sqlx::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

/// Resolve the bearer token to its tenant by comparing the token's SHA-256 (computed in
/// the database via pgcrypto, so the cleartext token never lands in a query log as a
/// value to match) against the stored hash. Returns the tenant and the stored digest (the
/// per-key identity the rate limiter buckets on). Unknown or missing token is a 401.
pub async fn authenticate(pool: &PgPool, headers: &HeaderMap) -> Result<(Uuid, Vec<u8>), ApiError> {
    let token = bearer_token(headers).ok_or(ApiError::Unauthorized)?;
    let row = sqlx::query(
        "SELECT tenant_id, token_sha256 FROM mnestic_api_key \
         WHERE token_sha256 = digest($1, 'sha256') AND revoked_at IS NULL",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::Unauthorized)?;
    Ok((row.get("tenant_id"), row.get("token_sha256")))
}

/// Authenticate, then enforce the per-key rate limit. The check is keyed on the resolved
/// key's digest, so only valid keys consume a bucket; an over-limit key is a 429. Handlers
/// call this; `authenticate` stays the bare tenant resolver.
pub async fn authenticate_request(state: &AppState, headers: &HeaderMap) -> Result<Uuid, ApiError> {
    let (tenant, key) = authenticate(state.engine.store().pool(), headers).await?;
    if !state.limiter.allow(&key) {
        return Err(ApiError::TooManyRequests);
    }
    Ok(tenant)
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
