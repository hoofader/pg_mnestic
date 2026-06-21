// SPDX-License-Identifier: AGPL-3.0-only

//! Revokes an API key. Build with `--features cli`.
//! Usage: revoke-key <key-digest-hex | sm_token>
//! Env: DATABASE_URL.
//! Pass the hex digest from list-keys, or the cleartext sm_ token if you still have it.
//! Revocation takes effect on the next request and is idempotent.

use mnestic_server::keys::{revoke_key_by_digest, revoke_key_by_token};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key_ref = std::env::args()
        .nth(1)
        .ok_or("usage: revoke-key <key-digest-hex | sm_token>")?;
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let revoked = if key_ref.starts_with("sm_") {
        revoke_key_by_token(&pool, &key_ref).await?
    } else {
        revoke_key_by_digest(&pool, &key_ref).await?
    };

    if revoked {
        println!("revoked");
    } else {
        println!("no active key matched (already revoked or unknown)");
    }
    Ok(())
}
