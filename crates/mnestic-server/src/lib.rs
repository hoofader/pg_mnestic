// SPDX-License-Identifier: MIT

//! The supermemory-compatible REST shim (doc 04). It maps supermemory's wire contract
//! onto the Mnestic engine so the existing shells drive Mnestic unchanged. This module
//! wires the router and shared state; the scoping mapping lives in `container_tag`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use mnestic_engine::Engine;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

pub mod auth;
pub mod container_tag;
pub mod filter;
pub mod keys;
mod directory;
mod documents;
pub mod error;
mod mcp;
mod memories;
mod memory_tool;
mod query;
pub mod rate_limit;

pub use container_tag::{parse_container_tag, reconstruct_container_tag, Scope};
pub use rate_limit::RateLimiter;

/// Shared handler state. The engine carries its own store/pool, which the auth lookup
/// reuses (the api_key table is outside RLS, so no tenant context is needed for it). The
/// limiter is per-key and per-process.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub limiter: Arc<RateLimiter>,
}

/// Build the router. Caller supplies an engine (real providers in the binary, mocks in
/// tests), so the HTTP surface is testable without network or keys.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/v4/memories",
            post(memories::add_memory)
                .delete(memories::forget_memory)
                .patch(memories::update_memory),
        )
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
        // Outermost, so every request gets a span with method, path, status, and latency. The
        // default span omits headers and bodies, so the bearer token and memory content are
        // not logged. Span and response are lifted to INFO so one line per request shows at
        // the default log level, not only under debug.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// Guard against shipping bearer tokens in cleartext. The server speaks plain HTTP, so it must
/// sit behind a TLS-terminating reverse proxy (DEPLOYMENT.md). A loopback bind is always
/// allowed: the only reachable peer is on the same host (the proxy, or local dev). A
/// non-loopback bind puts the plaintext socket on the network, so it is refused unless the
/// operator asserts TLS is terminated upstream by setting `MNESTIC_TRUST_PROXY=1`.
///
/// `bind` must be an `ip:port`. A hostname (including `localhost`) is rejected, because
/// loopback-vs-network cannot be reasoned about before resolution and resolution is
/// spoofable.
pub fn check_bind_safety(bind: &str, trust_proxy: bool) -> Result<(), String> {
    let addr: SocketAddr = bind.parse().map_err(|e| {
        format!("invalid bind address '{bind}': {e}; use an ip:port like 127.0.0.1:8080")
    })?;
    if addr.ip().is_loopback() || trust_proxy {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {bind}: it is reachable off-host and the server speaks plain \
             HTTP, so bearer tokens would cross the network in cleartext. Terminate TLS at a \
             reverse proxy and set MNESTIC_TRUST_PROXY=1, or bind a loopback address. See \
             DEPLOYMENT.md."
        ))
    }
}

/// Default Postgres pool size when `MNESTIC_DB_MAX_CONNECTIONS` is unset.
const DEFAULT_DB_MAX_CONNECTIONS: u32 = 16;

/// Pool size from the `MNESTIC_DB_MAX_CONNECTIONS` value. A missing, non-numeric, or zero
/// value falls back to the default rather than failing startup on a typo. Size it to the
/// database's connection budget, not the request rate.
pub fn db_max_connections(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_DB_MAX_CONNECTIONS)
}

/// Install the tracing subscriber shared by the binaries. RUST_LOG sets levels (default
/// `info`); `MNESTIC_LOG_FORMAT=json` switches to structured output for a log aggregator.
#[cfg(feature = "serve")]
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let json = std::env::var("MNESTIC_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Open the Postgres pool the binaries use: size from `MNESTIC_DB_MAX_CONNECTIONS` and a
/// 10s acquire timeout so a saturated pool fails fast instead of hanging.
#[cfg(feature = "serve")]
pub async fn connect_pool(dsn: &str) -> Result<sqlx::PgPool, sqlx::Error> {
    let max_conns = db_max_connections(std::env::var("MNESTIC_DB_MAX_CONNECTIONS").ok().as_deref());
    tracing::info!(max_connections = max_conns, "connecting database pool");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_conns)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(dsn)
        .await
}

/// Bring the schema and the runtime role to the state serving needs, before the serving pool
/// opens. Two paths:
///
/// - `MNESTIC_MIGRATION_DATABASE_URL` set: migrations need a superuser (`CREATE EXTENSION`), but
///   serving must not be one (a superuser bypasses RLS even with FORCE, which silently disables
///   the tenant isolation). So migrate and provision the runtime role over a short-lived
///   superuser connection, then close it; the caller serves on the non-super `DATABASE_URL`. The
///   role name is taken from `DATABASE_URL`'s username and its password from `MNESTIC_APP_PASSWORD`.
/// - unset: migrate on `serving_dsn` itself (the single-role path), so a simple deploy that runs
///   everything as one role still works.
///
/// The superuser connection is opened, used, and dropped here so the long-lived pool never holds
/// superuser credentials.
#[cfg(feature = "serve")]
pub async fn prepare_database(serving_dsn: &str) -> Result<(), Box<dyn std::error::Error>> {
    let migration_dsn = std::env::var("MNESTIC_MIGRATION_DATABASE_URL").ok();
    let migration_dsn = migration_dsn.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match migration_dsn {
        Some(su_dsn) => {
            let role = pg_username(serving_dsn).ok_or(
                "DATABASE_URL must carry the runtime role as its username when \
                 MNESTIC_MIGRATION_DATABASE_URL is set",
            )?;
            let password = std::env::var("MNESTIC_APP_PASSWORD").map_err(|_| {
                "MNESTIC_APP_PASSWORD must be set when MNESTIC_MIGRATION_DATABASE_URL is set"
            })?;
            // A bounded, short-lived pool: just enough to migrate and provision, then dropped.
            let su_pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(std::time::Duration::from_secs(10))
                .connect(su_dsn)
                .await?;
            tracing::info!("running migrations over the migration connection");
            mnestic_store::run_migrations(&su_pool).await?;
            tracing::info!(role = %role, "provisioning the non-superuser runtime role");
            mnestic_store::provision_app_role(&su_pool, &role, &password).await?;
            su_pool.close().await;
            Ok(())
        }
        None => {
            let pool = connect_pool(serving_dsn).await?;
            mnestic_store::run_migrations(&pool).await?;
            pool.close().await;
            Ok(())
        }
    }
}

/// The username component of a Postgres connection URL, percent-decoded. Used to name the
/// runtime role to provision, so it always matches the role the serving pool authenticates as.
#[cfg(feature = "serve")]
fn pg_username(dsn: &str) -> Option<String> {
    let opts: sqlx::postgres::PgConnectOptions = dsn.parse().ok()?;
    Some(opts.get_username().to_string())
}

/// Build the cloud providers shared by the server and the worker. The embedder model is fixed
/// (its 1536 dimension is baked into the halfvec schema); extraction defaults to Opus 4.8 and
/// `MNESTIC_EXTRACT_MODEL` drops it to a cheaper tier.
#[cfg(feature = "serve")]
pub fn build_providers(
    openai_key: String,
    anthropic_key: &str,
) -> (
    std::sync::Arc<dyn mnestic_core::Embedder>,
    std::sync::Arc<dyn mnestic_core::Extractor>,
) {
    use std::sync::Arc;
    let embedder: Arc<dyn mnestic_core::Embedder> = Arc::new(mnestic_model::OpenAiEmbedder::new(
        openai_key,
        "text-embedding-3-small",
    ));
    let mut anthropic = mnestic_model::AnthropicExtractor::new(anthropic_key);
    if let Ok(model) = std::env::var("MNESTIC_EXTRACT_MODEL") {
        let model = model.trim();
        if !model.is_empty() {
            anthropic = anthropic.with_model(model);
            tracing::info!(extract_model = model, "extraction model overridden");
        }
    }
    let extractor: Arc<dyn mnestic_core::Extractor> = Arc::new(anthropic);
    (embedder, extractor)
}

/// Resolve when the process receives SIGTERM (the orchestrator's stop signal) or SIGINT, so a
/// server or worker can drain before exiting.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
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

    #[test]
    fn bind_safety_allows_loopback_and_refuses_exposed() {
        // Loopback is always fine: nothing off-host can reach it.
        assert!(check_bind_safety("127.0.0.1:8080", false).is_ok());
        assert!(check_bind_safety("[::1]:8080", false).is_ok());
        // A network-reachable bind over plain HTTP is refused without the proxy assertion.
        assert!(check_bind_safety("0.0.0.0:8080", false).is_err());
        assert!(check_bind_safety("[::]:8080", false).is_err());
        assert!(check_bind_safety("10.0.0.5:8080", false).is_err());
        // Allowed once the operator asserts TLS is terminated upstream.
        assert!(check_bind_safety("0.0.0.0:8080", true).is_ok());
        // A non-address is a clear error, not a silent pass-through.
        assert!(check_bind_safety("localhost:8080", false).is_err());
    }

    #[test]
    fn db_max_connections_parses_or_defaults() {
        assert_eq!(db_max_connections(Some("32")), 32);
        assert_eq!(db_max_connections(Some("  8 ")), 8);
        // Missing, non-numeric, and zero all fall back to the default, never panic or 0.
        assert_eq!(db_max_connections(None), super::DEFAULT_DB_MAX_CONNECTIONS);
        assert_eq!(db_max_connections(Some("lots")), super::DEFAULT_DB_MAX_CONNECTIONS);
        assert_eq!(db_max_connections(Some("0")), super::DEFAULT_DB_MAX_CONNECTIONS);
    }
}
