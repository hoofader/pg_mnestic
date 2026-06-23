// SPDX-License-Identifier: MIT

//! Provisions a tenant and mints an API key. Build with `--features cli`.
//! Usage: issue-key <tenant-external-id> [label]
//! Env: DATABASE_URL.
//! The bearer token is printed once and is not stored; only its SHA-256 digest is kept, so
//! capture it now. Re-running for the same tenant adds another key (key rotation). List keys
//! with list-keys and revoke one with revoke-key.

use mnestic_server::keys::issue_key;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let external_id = args.next().ok_or("usage: issue-key <tenant-external-id> [label]")?;
    let label = args.next();
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let issued = issue_key(&pool, &external_id, label.as_deref()).await?;

    println!("tenant {external_id} = {}", issued.tenant_id);
    println!("token (shown once, store it now): {}", issued.token);
    Ok(())
}
