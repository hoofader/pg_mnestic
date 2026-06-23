// SPDX-License-Identifier: MIT

//! Smoke test for the custom Postgres image (`mnestic-pg:dev`): it must carry pg_graphwright
//! alongside pgvector, the mnestic migrations must still run on it, and the graphwright index
//! access method + `maintain()` must build an entity graph. This guards the image the rest of
//! the graph work builds on; if pg_graphwright is missing or broken, this fails first.

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
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

#[tokio::test]
async fn image_carries_pg_graphwright() {
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

    // The mnestic schema still applies cleanly on this image.
    mnestic_store::run_migrations(&pool).await.expect("migrations run on the custom image");

    // pg_graphwright is present and its index AM + resolve cycle work. Migration 0007 already
    // created the extension, so this is idempotent.
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_graphwright")
        .execute(&pool)
        .await
        .expect("create pg_graphwright");
    sqlx::query("CREATE TABLE gw_notes (id int PRIMARY KEY, body text)")
        .execute(&pool)
        .await
        .expect("notes table");
    sqlx::query("INSERT INTO gw_notes VALUES (1, 'Sara Tehran'), (2, 'Reza Berlin')")
        .execute(&pool)
        .await
        .expect("seed notes");
    sqlx::query("CREATE INDEX gw_notes_kg ON gw_notes USING graphwright (body)")
        .execute(&pool)
        .await
        .expect("graphwright index");
    sqlx::query("SELECT graphwright.maintain()").execute(&pool).await.expect("maintain");

    let entities: i64 = sqlx::query("SELECT count(*) AS n FROM graphwright.entity")
        .fetch_one(&pool)
        .await
        .expect("entity count")
        .get("n");
    assert!(entities > 0, "graphwright extracted entities from the seeded rows, got {entities}");
}
