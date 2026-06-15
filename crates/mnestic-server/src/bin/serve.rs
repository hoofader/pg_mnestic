// SPDX-License-Identifier: Apache-2.0

//! Runnable supermemory-compatible server. Build with `--features serve`.
//! Env: DATABASE_URL, OPENAI_API_KEY (embeddings), ANTHROPIC_API_KEY (extraction),
//! MNESTIC_BIND (default 127.0.0.1:8080), MNESTIC_TRUST_PROXY (set to 1 to allow a
//! non-loopback bind, asserting TLS is terminated by a reverse proxy; see DEPLOYMENT.md).
//! The server speaks plain HTTP and must run behind a TLS-terminating proxy in production.
//! Provision a key with the issue-key binary:
//! `cargo run -p mnestic-server --features cli --bin issue-key -- <tenant>`.
//! Logs: RUST_LOG sets levels (default `info`); set MNESTIC_LOG_FORMAT=json for structured
//! output to ship to a log aggregator. MNESTIC_DB_MAX_CONNECTIONS sizes the Postgres pool
//! (default 16). On SIGTERM/SIGINT the server stops accepting connections and drains
//! in-flight requests before exiting.

use std::sync::Arc;
use std::time::Duration;

use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{AnthropicExtractor, OpenAiEmbedder};
use mnestic_server::{app, AppState};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let dsn = std::env::var("DATABASE_URL")?;
    let openai_key = std::env::var("OPENAI_API_KEY")?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")?;
    let bind = std::env::var("MNESTIC_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let trust_proxy = std::env::var("MNESTIC_TRUST_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Fail fast rather than expose plaintext bearer tokens on a public socket.
    mnestic_server::check_bind_safety(&bind, trust_proxy)?;

    let max_conns = mnestic_server::db_max_connections(
        std::env::var("MNESTIC_DB_MAX_CONNECTIONS").ok().as_deref(),
    );
    let pool = PgPoolOptions::new()
        .max_connections(max_conns)
        // Fail a request that can't get a connection rather than hang it indefinitely when
        // the pool is saturated.
        .acquire_timeout(Duration::from_secs(10))
        .connect(&dsn)
        .await?;
    run_migrations(&pool).await?;

    let embedder: Arc<dyn Embedder> =
        Arc::new(OpenAiEmbedder::new(openai_key, "text-embedding-3-small"));
    let extractor: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::new(&anthropic_key));
    let engine = Arc::new(Engine::new(Store::new(pool), embedder, extractor));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, max_connections = max_conns, "mnestic-server listening");
    axum::serve(listener, app(AppState { engine }))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("mnestic-server stopped");
    Ok(())
}

/// Resolve when the process receives SIGTERM (the orchestrator's stop signal) or SIGINT
/// (ctrl-c). axum then stops accepting new connections and waits for in-flight requests to
/// finish before `serve` returns.
async fn shutdown_signal() {
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
    tracing::info!("shutdown signal received, draining in-flight requests");
}
