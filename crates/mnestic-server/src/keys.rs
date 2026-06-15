// SPDX-License-Identifier: Apache-2.0

//! Tenant and API-key provisioning. One key resolves to one tenant (the RLS boundary,
//! doc 04 §2). Only the SHA-256 digest of a key is ever stored; the cleartext is returned
//! to the caller to show once and is unrecoverable afterward.

use rand::RngCore;
use sqlx::PgPool;
use uuid::Uuid;

/// A freshly minted key. `token` is the cleartext bearer, shown once; the database keeps
/// only its digest.
pub struct IssuedKey {
    pub tenant_id: Uuid,
    pub token: String,
}

/// An `sm_`-prefixed bearer with 192 bits of OS entropy. The `sm_` prefix is required: the
/// supermemory shells reject anything else before they ever reach us.
pub fn generate_token() -> String {
    let mut raw = [0u8; 24];
    let mut rng = rand::rngs::OsRng;
    rng.fill_bytes(&mut raw);
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    format!("sm_{hex}")
}

/// Resolve (or create) the tenant by external id, then store the digest of a new key for it.
/// Re-running for the same external id reuses the tenant and adds another key, so a caller
/// can rotate keys without disturbing the tenant or its data. The stored digest is computed
/// with the same `digest($1,'sha256')` that `auth::authenticate` looks up.
pub async fn issue_key(pool: &PgPool, external_id: &str) -> sqlx::Result<IssuedKey> {
    let token = generate_token();

    // UNION ALL returns the inserted row, or the existing one on conflict; LIMIT 1 keeps a
    // single id. A DO UPDATE no-op would also work but would churn the row pointlessly.
    let tenant_id: Uuid = sqlx::query_scalar(
        "WITH ins AS ( \
           INSERT INTO mnestic_tenant (external_id) VALUES ($1) \
           ON CONFLICT (external_id) DO NOTHING RETURNING id \
         ) \
         SELECT id FROM ins \
         UNION ALL \
         SELECT id FROM mnestic_tenant WHERE external_id = $1 \
         LIMIT 1",
    )
    .bind(external_id)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        "INSERT INTO mnestic_api_key (token_sha256, tenant_id) VALUES (digest($1, 'sha256'), $2)",
    )
    .bind(&token)
    .bind(tenant_id)
    .execute(pool)
    .await?;

    Ok(IssuedKey { tenant_id, token })
}
