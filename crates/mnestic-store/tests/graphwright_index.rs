// SPDX-License-Identifier: MIT

//! Migration 0008 registers a graphwright watch on `mnestic_memory(content)` keyed on the stable
//! `id` (replacing 0007's ctid-keyed index), and the worker's `graphwright_maintain()` resolves
//! memory content into the entity graph. This asserts the real schema path (not the ad-hoc table
//! the image smoke uses): run the migrations, write memories, maintain, and see entities derived
//! from their content.

use std::time::Duration;

use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

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

#[tokio::test]
async fn migration_registers_watch_and_resolves_entities() {
    let container = GenericImage::new("mnestic-pg", "dev")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await
        .expect("start mnestic-pg image");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432.tcp()).await.expect("mapped port");
    let opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let pool = connect(opts).await;

    // 0008 drops the ctid index and registers a watch on mnestic_memory(content) keyed on `id`.
    run_migrations(&pool).await.expect("migrations incl. 0008");

    let watch_pk: Option<String> = sqlx::query_scalar(
        "SELECT pk_column FROM graphwright.watch \
         WHERE source_table = 'mnestic_memory'::regclass",
    )
    .fetch_optional(&pool)
    .await
    .expect("watch lookup");
    assert_eq!(
        watch_pk.as_deref(),
        Some("id"),
        "migration 0008 registered a watch on mnestic_memory keyed on the stable id"
    );

    let store = Store::new(pool.clone());
    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('kg') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    // Write memory content the way the engine does (tenant GUC set on the tx), so the index
    // marks the rows.
    let mut tx = store.begin_tenant(tenant).await.expect("tx");
    for content in ["Sara lives in Tehran", "Reza moved to Berlin"] {
        sqlx::query("INSERT INTO mnestic_memory (tenant_id, actor_id, content) VALUES ($1, 'u', $2)")
            .bind(tenant)
            .bind(content)
            .execute(&mut *tx)
            .await
            .expect("insert memory");
    }
    tx.commit().await.expect("commit");

    let maintained = store.graphwright_maintain().await.expect("maintain");
    assert!(maintained >= 1, "maintain ran over the memory index, got {maintained}");

    let entities: i64 = sqlx::query("SELECT count(*) AS n FROM graphwright.entity")
        .fetch_one(&pool)
        .await
        .expect("entity count")
        .get("n");
    assert!(entities > 0, "memory content resolved into graph entities, got {entities}");
}
