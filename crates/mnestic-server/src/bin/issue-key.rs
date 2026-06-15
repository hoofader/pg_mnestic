// SPDX-License-Identifier: Apache-2.0

//! Provisions a tenant and mints an API key. Build with `--features cli`.
//! Usage: issue-key <tenant-external-id>
//! Env: DATABASE_URL.
//! The bearer token is printed once and is not stored; only its SHA-256 digest is kept, so
//! capture it now. Re-running for the same tenant adds another key (key rotation).

use mnestic_server::keys::issue_key;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let external_id = std::env::args()
        .nth(1)
        .ok_or("usage: issue-key <tenant-external-id>")?;
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let issued = issue_key(&pool, &external_id).await?;

    println!("tenant {external_id} = {}", issued.tenant_id);
    println!("token (shown once, store it now): {}", issued.token);
    Ok(())
}
