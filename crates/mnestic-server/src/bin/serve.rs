// SPDX-License-Identifier: Apache-2.0

//! Runnable supermemory-compatible server. Build with `--features serve`.
//! Env: DATABASE_URL, OPENAI_API_KEY (embeddings), ANTHROPIC_API_KEY (extraction),
//! MNESTIC_BIND (default 127.0.0.1:8080; set 0.0.0.0:8080 to expose). Provision a key with
//! `INSERT INTO mnestic_api_key (token_sha256, tenant_id) VALUES (digest('<token>','sha256'), '<tenant>')`.

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
