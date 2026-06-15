// SPDX-License-Identifier: Apache-2.0

//! Tenant and API-key provisioning. One key resolves to one tenant (the RLS boundary,
//! doc 04 §2). Only the SHA-256 digest of a key is ever stored; the cleartext is returned
//! to the caller to show once and is unrecoverable afterward.

use chrono::{DateTime, Utc};
use rand::RngCore;
use sqlx::PgPool;
use uuid::Uuid;

/// A freshly minted key. `token` is the cleartext bearer, shown once; the database keeps
/// only its digest.
pub struct IssuedKey {
    pub tenant_id: Uuid,
    pub token: String,
}

/// A key as seen in a listing. `digest_hex` is the hex SHA-256 of the token, the only stable
/// public handle (the cleartext is unrecoverable), so it is what `revoke_key_by_digest` takes.
#[derive(sqlx::FromRow)]
pub struct KeyInfo {
    pub digest_hex: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
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
/// with the same `digest($1,'sha256')` that `auth::authenticate` looks up. The optional label
/// is a human note shown in `list_keys`, not a credential.
pub async fn issue_key(
    pool: &PgPool,
    external_id: &str,
    label: Option<&str>,
) -> sqlx::Result<IssuedKey> {
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
        "INSERT INTO mnestic_api_key (token_sha256, tenant_id, label) \
         VALUES (digest($1, 'sha256'), $2, $3)",
    )
    .bind(&token)
    .bind(tenant_id)
    .bind(label)
    .execute(pool)
    .await?;

    Ok(IssuedKey { tenant_id, token })
}

/// List a tenant's keys (active and revoked) for operator inspection. Returns the hex digest
/// (the revocation handle), label, and lifecycle timestamps. The cleartext token is never
/// stored, so it cannot appear here.
pub async fn list_keys(pool: &PgPool, external_id: &str) -> sqlx::Result<Vec<KeyInfo>> {
    sqlx::query_as::<_, KeyInfo>(
        "SELECT encode(k.token_sha256, 'hex') AS digest_hex, k.label, k.created_at, k.revoked_at \
         FROM mnestic_api_key k \
         JOIN mnestic_tenant t ON t.id = k.tenant_id \
         WHERE t.external_id = $1 \
         ORDER BY k.created_at",
    )
    .bind(external_id)
    .fetch_all(pool)
    .await
}

/// Revoke a key by its hex digest (from `list_keys`). Returns true if an active key was
/// revoked, false if no active key matched (already revoked, or unknown digest). Revocation
/// is idempotent: a second call returns false rather than churning `revoked_at`.
pub async fn revoke_key_by_digest(pool: &PgPool, digest_hex: &str) -> sqlx::Result<bool> {
    let affected = sqlx::query(
        "UPDATE mnestic_api_key SET revoked_at = now() \
         WHERE token_sha256 = decode($1, 'hex') AND revoked_at IS NULL",
    )
    .bind(digest_hex)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Revoke a key by its cleartext token (for the operator who just minted it and still has
/// it). The digest is computed in the database, matching the issue and auth paths. Same
/// idempotent contract as `revoke_key_by_digest`.
pub async fn revoke_key_by_token(pool: &PgPool, token: &str) -> sqlx::Result<bool> {
    let affected = sqlx::query(
        "UPDATE mnestic_api_key SET revoked_at = now() \
         WHERE token_sha256 = digest($1, 'sha256') AND revoked_at IS NULL",
    )
    .bind(token)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}
