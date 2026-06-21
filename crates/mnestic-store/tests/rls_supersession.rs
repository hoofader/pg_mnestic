// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized integration test: RLS isolation, single-valued supersession, and
//! multi-valued coexistence against pgvector/pgvector:pg16. RLS is exercised as
//! a non-superuser, since superusers bypass it.

use std::time::Duration;

use chrono::Utc;
use mnestic_store::{run_migrations, NewMemory, Store};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::Row;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

async fn connect(opts: PgConnectOptions) -> sqlx::PgPool {
    // Postgres logs "ready to accept connections" twice during init, so trust a
    // real connection rather than the first log line. Retry for a few seconds.
    let mut last_err = None;
    for _ in 0..30 {
        match PgPoolOptions::new()
            .max_connections(5)
            .connect_with(opts.clone())
            .await
        {
            Ok(pool) => return pool,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("could not connect to postgres: {last_err:?}");
}

#[tokio::test]
async fn rls_isolation_and_supersession() {
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
    let port = container
        .get_host_port_ipv4(5432.tcp())
        .await
        .expect("mapped port");

    let su_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");

    // (b) Connect as superuser, run migrations.
    let su_pool = connect(su_opts).await;
    run_migrations(&su_pool).await.expect("migrations");

    // (c) Create the app role without BYPASSRLS, granted DML on the schema.
    // GRANT ON ALL TABLES covers only tables that exist now; ALTER DEFAULT PRIVILEGES
    // makes future migrations' tables reachable too, so the app role does not silently
    // lose access after the next migration.
    for stmt in [
        "CREATE ROLE mnestic_app LOGIN PASSWORD 'app' NOBYPASSRLS",
        "GRANT USAGE ON SCHEMA public TO mnestic_app",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO mnestic_app",
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public \
         GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO mnestic_app",
    ] {
        sqlx::query(stmt)
            .execute(&su_pool)
            .await
            .unwrap_or_else(|e| panic!("setup stmt failed [{stmt}]: {e}"));
    }

    // (d) Insert two tenants as superuser (mnestic_tenant has no RLS).
    let tenant_a: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('A') RETURNING id")
            .fetch_one(&su_pool)
            .await
            .expect("tenant A");
    let tenant_b: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('B') RETURNING id")
            .fetch_one(&su_pool)
            .await
            .expect("tenant B");

    // (e) Open a second pool as mnestic_app. All assertions use this pool.
    let app_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("mnestic_app")
        .password("app")
        .database("postgres");
    let app_pool = connect(app_opts).await;
    let store = Store::new(app_pool);

    // (f) RLS isolation.
    store
        .insert_memory(
            tenant_a,
            &NewMemory {
                actor_id: "user",
                content: "A memory one",
                subject: None,
                attribute: None,
                value: None,
                single_valued: false,
            },
        )
        .await
        .expect("insert A");
    store
        .insert_memory(
            tenant_b,
            &NewMemory {
                actor_id: "user",
                content: "B memory one",
                subject: None,
                attribute: None,
                value: None,
                single_valued: false,
            },
        )
        .await
        .expect("insert B");

    let visible_a = store
        .count_visible_memories(Some(tenant_a))
        .await
        .expect("count A");
    assert_eq!(visible_a, 1, "tenant A must see only its own row");

    let visible_unset = store
        .count_visible_memories(None)
        .await
        .expect("count unset");
    assert_eq!(visible_unset, 0, "unset GUC must be fail-closed");

    // (g) Single-valued supersession.
    let prior_id = store
        .insert_memory(
            tenant_a,
            &NewMemory {
                actor_id: "user",
                content: "user lives in NYC",
                subject: Some("user"),
                attribute: Some("location"),
                value: Some("NYC"),
                single_valued: true,
            },
        )
        .await
        .expect("insert NYC");

    store
        .supersede_single_valued(
            tenant_a,
            prior_id,
            &NewMemory {
                actor_id: "user",
                content: "user lives in SF",
                subject: Some("user"),
                attribute: Some("location"),
                value: Some("SF"),
                single_valued: true,
            },
            Utc::now(),
        )
        .await
        .expect("supersede to SF");

    let latest = store
        .latest_single_valued(tenant_a, "user", "user", "location")
        .await
        .expect("latest query")
        .expect("one latest row");
    assert_eq!(latest.value.as_deref(), Some("SF"));

    // A second active overlapping single-valued row must trip the EXCLUDE
    // constraint (SQLSTATE 23P01, exclusion_violation).
    let dup = store
        .insert_memory(
            tenant_a,
            &NewMemory {
                actor_id: "user",
                content: "user lives in LA",
                subject: Some("user"),
                attribute: Some("location"),
                value: Some("LA"),
                single_valued: true,
            },
        )
        .await;
    match dup {
        Err(sqlx::Error::Database(db)) => {
            assert_eq!(
                db.code().as_deref(),
                Some("23P01"),
                "expected exclusion_violation, got {db:?}"
            );
        }
        other => panic!("expected exclusion_violation, got {other:?}"),
    }

    // (h) Multi-valued coexistence: both active overlapping rows must persist.
    store
        .insert_memory(
            tenant_a,
            &NewMemory {
                actor_id: "user",
                content: "user speaks English",
                subject: Some("user"),
                attribute: Some("language"),
                value: Some("English"),
                single_valued: false,
            },
        )
        .await
        .expect("insert English");
    store
        .insert_memory(
            tenant_a,
            &NewMemory {
                actor_id: "user",
                content: "user speaks French",
                subject: Some("user"),
                attribute: Some("language"),
                value: Some("French"),
                single_valued: false,
            },
        )
        .await
        .expect("insert French (must coexist)");

    let langs: i64 = {
        let mut tx = store.pool().begin().await.expect("tx");
        sqlx::query("SELECT set_config('mnestic.tenant_id', $1, true)")
            .bind(tenant_a.to_string())
            .execute(&mut *tx)
            .await
            .expect("set guc");
        let n: i64 = sqlx::query(
            "SELECT count(*) AS n FROM mnestic_memory \
             WHERE actor_id = 'user' AND attribute = 'language' AND status = 'active'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("count langs")
        .get("n");
        tx.commit().await.expect("commit");
        n
    };
    assert_eq!(langs, 2, "multi-valued facts must coexist");
}
