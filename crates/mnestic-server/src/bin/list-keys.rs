// SPDX-License-Identifier: AGPL-3.0-only

//! Lists a tenant's API keys (active and revoked). Build with `--features cli`.
//! Usage: list-keys <tenant-external-id>
//! Env: DATABASE_URL.
//! Prints one key per line: <digest-hex> <created_at> <status> <label>. The digest is the
//! handle revoke-key takes; the cleartext token is never stored and never shown.

use mnestic_server::keys::list_keys;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let external_id = std::env::args()
        .nth(1)
        .ok_or("usage: list-keys <tenant-external-id>")?;
    let dsn = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new().max_connections(1).connect(&dsn).await?;
    let keys = list_keys(&pool, &external_id).await?;

    if keys.is_empty() {
        println!("no keys for tenant {external_id}");
    }
    for k in keys {
        let status = match k.revoked_at {
            Some(t) => format!("revoked {t}"),
            None => "active".to_string(),
        };
        println!("{}  {}  {}  {}", k.digest_hex, k.created_at, status, k.label.unwrap_or_default());
    }
    Ok(())
}
