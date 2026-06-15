// SPDX-License-Identifier: Apache-2.0

//! Runnable supermemory-compatible server. Build with `--features serve`.
//! Env: DATABASE_URL, OPENAI_API_KEY (embeddings), ANTHROPIC_API_KEY (extraction),
//! MNESTIC_BIND (default 127.0.0.1:8080), MNESTIC_TRUST_PROXY (set to 1 to allow a
//! non-loopback bind, asserting TLS is terminated by a reverse proxy; see DEPLOYMENT.md).
//! The server speaks plain HTTP and must run behind a TLS-terminating proxy in production.
//! Provision a key with the issue-key binary:
//! `cargo run -p mnestic-server --features cli --bin issue-key -- <tenant>`.

use std::sync::Arc;

use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{AnthropicExtractor, OpenAiEmbedder};
use mnestic_server::{app, AppState};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dsn = std::env::var("DATABASE_URL")?;
    let openai_key = std::env::var("OPENAI_API_KEY")?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")?;
    let bind = std::env::var("MNESTIC_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let trust_proxy = std::env::var("MNESTIC_TRUST_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Fail fast rather than expose plaintext bearer tokens on a public socket.
    mnestic_server::check_bind_safety(&bind, trust_proxy)?;

    let pool = PgPoolOptions::new().max_connections(8).connect(&dsn).await?;
    run_migrations(&pool).await?;

    let embedder: Arc<dyn Embedder> =
        Arc::new(OpenAiEmbedder::new(openai_key, "text-embedding-3-small"));
    let extractor: Arc<dyn Extractor> = Arc::new(AnthropicExtractor::new(&anthropic_key));
    let engine = Arc::new(Engine::new(Store::new(pool), embedder, extractor));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("mnestic-server listening on {bind}");
    axum::serve(listener, app(AppState { engine })).await?;
    Ok(())
}
