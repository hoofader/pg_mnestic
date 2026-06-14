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
pub mod error;
mod memories;

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
        // Bound the body so a single request cannot push a huge extract/embed job.
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}
