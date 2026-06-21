// SPDX-License-Identifier: AGPL-3.0-only

//! Async-ingestion worker. Build with `--features serve`. Drains sources enqueued by the
//! `dreaming: dynamic` path: it polls each tenant, claims pending sources (leasing them), and
//! runs extraction + persistence off the request path.
//!
//! Env: DATABASE_URL, OPENAI_API_KEY, ANTHROPIC_API_KEY (same as the server), plus
//! MNESTIC_WORKER_POLL_SECS (idle poll interval, default 5), MNESTIC_WORKER_LEASE_SECS
//! (claim lease, default 300; set above the slowest extraction so a busy claim is not
//! reclaimed early), MNESTIC_WORKER_BATCH (max sources per tenant per cycle, default 16).
//! Shares MNESTIC_DB_MAX_CONNECTIONS, MNESTIC_EXTRACT_MODEL, and the log env with the server.
//! Stops on SIGTERM/SIGINT after the current cycle.

use std::time::Duration;

use mnestic_engine::Engine;
use mnestic_server::{build_providers, connect_pool, init_tracing, shutdown_signal};
use mnestic_store::{run_migrations, Store};

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let dsn = std::env::var("DATABASE_URL")?;
    let openai_key = std::env::var("OPENAI_API_KEY")?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY")?;
    let poll = Duration::from_secs(env_or("MNESTIC_WORKER_POLL_SECS", 5));
    let lease_secs: i64 = env_or("MNESTIC_WORKER_LEASE_SECS", 300);
    let batch: usize = env_or("MNESTIC_WORKER_BATCH", 16);

    let pool = connect_pool(&dsn).await?;
    run_migrations(&pool).await?;
    let (embedder, extractor) = build_providers(openai_key, &anthropic_key);
    let engine = Engine::new(Store::new(pool.clone()), embedder, extractor);
    let store = Store::new(pool);

    tracing::info!(lease_secs, batch, "mnestic worker started");
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        let processed = run_cycle(&engine, &store, lease_secs, batch).await;
        // Drain a backlog without sleeping; idle only when a full cycle found nothing.
        let nap = if processed > 0 { Duration::ZERO } else { poll };
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(nap) => {}
        }
    }
    tracing::info!("mnestic worker stopped");
    Ok(())
}

/// One pass over every tenant. A tenant whose claim query fails (e.g. a transient database
/// error) is logged and skipped so one tenant cannot stall the others; per-source extraction
/// failures are already handled inside `process_pending`. Returns the total processed.
async fn run_cycle(engine: &Engine, store: &Store, lease_secs: i64, batch: usize) -> usize {
    let tenants = match store.list_tenant_ids().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "listing tenants failed");
            return 0;
        }
    };
    let mut total = 0;
    for tenant in tenants {
        match engine.process_pending(tenant, lease_secs, batch).await {
            Ok(n) => total += n,
            Err(e) => tracing::error!(%tenant, error = %e, "processing tenant failed"),
        }
    }
    total
}
