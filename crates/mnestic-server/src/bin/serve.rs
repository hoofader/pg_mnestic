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
//! (default 16). MNESTIC_EXTRACT_MODEL overrides the extraction model (default Opus 4.8) for
//! a cheaper tier. On SIGTERM/SIGINT the server stops accepting connections and drains
//! in-flight requests before exiting.

use std::sync::Arc;

use mnestic_engine::Engine;
use mnestic_server::{
    app, build_providers, connect_pool, init_tracing, shutdown_signal, AppState, RateLimiter,
};
use mnestic_store::{run_migrations, Store};

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

    let pool = connect_pool(&dsn).await?;
    run_migrations(&pool).await?;

    let (embedder, extractor) = build_providers(openai_key, &anthropic_key);
    let engine = Arc::new(Engine::new(Store::new(pool), embedder, extractor));
    let limiter = RateLimiter::from_env();

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "mnestic-server listening");
    axum::serve(listener, app(AppState { engine, limiter }))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("mnestic-server stopped");
    Ok(())
}
