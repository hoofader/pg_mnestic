// SPDX-License-Identifier: Apache-2.0

//! The supermemory-compatible REST shim (doc 04). It maps supermemory's wire contract
//! onto the Mnestic engine so the existing shells drive Mnestic unchanged. This module
//! wires the router and shared state; the scoping mapping lives in `container_tag`.

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use mnestic_engine::Engine;

pub mod auth;
pub mod container_tag;
mod directory;
mod documents;
pub mod error;
mod mcp;
mod memories;
mod memory_tool;
mod query;

pub use container_tag::{parse_container_tag, reconstruct_container_tag, Scope};

/// Shared handler state. The engine carries its own store/pool, which the auth lookup
/// reuses (the api_key table is outside RLS, so no tenant context is needed for it).
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
}

/// Build the router. Caller supplies an engine (real providers in the binary, mocks in
/// tests), so the HTTP surface is testable without network or keys.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v4/memories", post(memories::add_memory))
        .route("/v4/memory", post(memory_tool::memory_tool))
        .route("/v4/conversations", post(memories::ingest_conversation))
        .route("/v4/search", post(query::search))
        .route("/v4/profile", post(query::profile))
        .route("/v3/documents", post(documents::ingest_document))
        .route("/v3/search", post(documents::search_documents))
        .route("/v3/session", get(directory::session))
        .route("/v3/projects", get(directory::projects))
        .route("/mcp", post(mcp::mcp))
        // Bound the body so a single request cannot push a huge extract/embed job.
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// Recall fan-out default and cap. Clamping a client value keeps it out of a negative
/// SQL `LIMIT` (a 500) and bounds how large a single query can get.
const DEFAULT_LIMIT: i64 = 10;
const MAX_LIMIT: i64 = 100;

pub(crate) fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// supermemory sends the same scope as either a singular `containerTag` or a plural
/// `containerTags` (doc 04 §2), so accept both and resolve to the one tag string the
/// scoping mapping parses. A multi-element array has no single actor, so it is rejected
/// rather than guessed.
pub(crate) fn resolve_container_tag(
    singular: Option<String>,
    plural: Option<Vec<String>>,
) -> Result<String, error::ApiError> {
    let tag = match (singular, plural) {
        (Some(t), _) if !t.is_empty() => t,
        (_, Some(v)) if v.len() == 1 && !v[0].is_empty() => v.into_iter().next().unwrap(),
        (_, Some(v)) if v.len() > 1 => {
            return Err(error::ApiError::BadRequest("multiple containerTags is not supported yet".into()))
        }
        _ => return Err(error::ApiError::BadRequest("containerTag is required".into())),
    };
    validate_container_tag(&tag)?;
    Ok(tag)
}

/// Enforce supermemory's `containerTag` shape (`^[a-zA-Z0-9_:-]+$`, 1..=100) at the edge,
/// so malformed input is a 400, not a confusing downstream actor/key.
fn validate_container_tag(tag: &str) -> Result<(), error::ApiError> {
    if tag.is_empty() || tag.chars().count() > 100 {
        return Err(error::ApiError::BadRequest("containerTag must be 1 to 100 characters".into()));
    }
    if !tag.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-')) {
        return Err(error::ApiError::BadRequest(
            "containerTag allows only letters, digits, '_', ':', '-'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(s: Option<&str>, p: Option<Vec<&str>>) -> Result<String, error::ApiError> {
        resolve_container_tag(
            s.map(str::to_string),
            p.map(|v| v.iter().map(|x| x.to_string()).collect()),
        )
    }

    #[test]
    fn resolve_accepts_singular_and_single_plural() {
        assert_eq!(resolve(Some("org:7:user:9"), None).unwrap(), "org:7:user:9");
        assert_eq!(resolve(None, Some(vec!["user:1"])).unwrap(), "user:1");
    }

    #[test]
    fn resolve_rejects_missing_multi_and_malformed() {
        assert!(resolve(None, None).is_err(), "missing");
        assert!(resolve(None, Some(vec!["a", "b"])).is_err(), "multi-element");
        assert!(resolve(Some("has space"), None).is_err(), "invalid char");
        assert!(resolve(Some("a/b"), None).is_err(), "slash not allowed");
        assert!(resolve(Some(&"x".repeat(101)), None).is_err(), "too long");
        assert!(resolve(Some(""), None).is_err(), "empty");
    }
}
