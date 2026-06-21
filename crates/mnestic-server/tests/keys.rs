// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for key provisioning: issue_key mints an sm_ token, stores only its
//! digest, and the cleartext round-trips through auth::authenticate back to its tenant.

use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue};
use mnestic_server::auth::authenticate;
use mnestic_server::keys::{issue_key, list_keys, revoke_key_by_digest, revoke_key_by_token};
use mnestic_store::run_migrations;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

async fn connect(opts: PgConnectOptions) -> PgPool {
    let mut last_err = None;
    for _ in 0..30 {
        match PgPoolOptions::new().max_connections(5).connect_with(opts.clone()).await {
            Ok(pool) => return pool,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to postgres: {last_err:?}");
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

#[tokio::test]
async fn issued_key_authenticates_to_its_tenant() {
    let container = GenericImage::new("pgvector/pgvector", "pg16")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await
        .expect("start pgvector container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432.tcp()).await.expect("mapped port");
    let opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let pool = connect(opts).await;
    run_migrations(&pool).await.expect("migrations");

    // A fresh tenant gets a token that the shells will accept and that auth resolves back.
    let first = issue_key(&pool, "acme", Some("ci")).await.expect("issue first key");
    assert!(first.token.starts_with("sm_"), "sm_ prefix required, got {}", first.token);
    assert_eq!(first.token.len(), 3 + 48, "sm_ plus 24 bytes of hex");
    let (resolved, _digest) = authenticate(&pool, &bearer(&first.token)).await.expect("authenticate");
    assert_eq!(resolved, first.tenant_id, "token resolves to its tenant");

    // The cleartext is not stored; only the digest is.
    let stored_clear: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mnestic_api_key WHERE token_sha256 = $1::bytea")
            .bind(first.token.as_bytes())
            .fetch_one(&pool)
            .await
            .expect("count by cleartext");
    assert_eq!(stored_clear, 0, "the cleartext token is never the stored key");

    // Re-issuing for the same external id reuses the tenant and mints a distinct token, so a
    // key can be rotated without disturbing the tenant or its data. Both keys stay valid.
    let second = issue_key(&pool, "acme", None).await.expect("issue second key");
    assert_eq!(second.tenant_id, first.tenant_id, "same tenant on re-issue");
    assert_ne!(second.token, first.token, "a new token each time");
    let (resolved2, _) = authenticate(&pool, &bearer(&second.token)).await.expect("authenticate second");
    assert_eq!(resolved2, first.tenant_id);
    // The prior key is untouched: issuing a new one does not revoke it (revocation is a
    // separate, explicit operation).
    assert_eq!(
        authenticate(&pool, &bearer(&first.token)).await.expect("first still valid").0,
        first.tenant_id,
    );

    // A different external id is a different tenant (the isolation boundary).
    let other = issue_key(&pool, "globex", None).await.expect("issue other tenant");
    assert_ne!(other.tenant_id, first.tenant_id, "distinct tenants");

    // An unknown token is rejected.
    assert!(
        authenticate(&pool, &bearer("sm_deadbeef")).await.is_err(),
        "unknown token rejected"
    );

    // The listing shows both of the tenant's keys, with the label that was set.
    let listed = list_keys(&pool, "acme").await.expect("list acme keys");
    assert_eq!(listed.len(), 2, "two keys for acme");
    assert!(listed.iter().any(|k| k.label.as_deref() == Some("ci")), "label round-trips");
    assert!(listed.iter().all(|k| k.revoked_at.is_none()), "both active before revocation");

    // Revoke the first key by its hex digest; auth must reject it while the second still works.
    let first_digest = listed
        .iter()
        .find(|k| k.label.as_deref() == Some("ci"))
        .expect("first key in listing")
        .digest_hex
        .clone();
    assert!(revoke_key_by_digest(&pool, &first_digest).await.expect("revoke"), "revoked");
    assert!(
        authenticate(&pool, &bearer(&first.token)).await.is_err(),
        "revoked key no longer authenticates"
    );
    assert_eq!(
        authenticate(&pool, &bearer(&second.token)).await.expect("second still valid").0,
        first.tenant_id,
        "revoking one key does not affect another"
    );

    // Revocation is idempotent: a second revoke of the same key reports no change.
    assert!(!revoke_key_by_digest(&pool, &first_digest).await.expect("re-revoke"), "already revoked");

    // Revoking by cleartext token works for the operator who still holds it.
    assert!(revoke_key_by_token(&pool, &second.token).await.expect("revoke by token"), "revoked");
    assert!(
        authenticate(&pool, &bearer(&second.token)).await.is_err(),
        "token-revoked key no longer authenticates"
    );

    // The listing reflects the revocations.
    let after = list_keys(&pool, "acme").await.expect("list after revoke");
    assert!(after.iter().all(|k| k.revoked_at.is_some()), "both acme keys now revoked");
}
