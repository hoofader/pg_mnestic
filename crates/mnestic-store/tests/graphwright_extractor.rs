// SPDX-License-Identifier: MIT

//! Migration 0009 installs the `http` extension (from the image) and the `mnestic_gliner_extract`
//! function that the pg_graphwright extractor seam calls to reach the GLiNER sidecar. This guards
//! the image carrying pgsql-http and the function being valid; the live GLiNER path (the sidecar +
//! the model) is exercised out of band, since CI carries no model.

use std::time::Duration;

use mnestic_store::run_migrations;
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
async fn migration_installs_http_and_extractor_function() {
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
    run_migrations(&pool).await.expect("migrations incl. 0009");

    // pgsql-http is built into the image, so 0009's CREATE EXTENSION http succeeds.
    let http: bool =
        sqlx::query("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'http')")
            .fetch_one(&pool)
            .await
            .expect("http lookup")
            .get(0);
    assert!(http, "the image carries pgsql-http and 0009 created the extension");

    // The extractor seam function is installed (opt-in; graphwright.extractor is not set here, so
    // the graph still uses the built-in tokenizer until an operator activates it).
    let func: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'mnestic_gliner_extract')",
    )
    .fetch_one(&pool)
    .await
    .expect("function lookup")
    .get(0);
    assert!(func, "0009 installed mnestic_gliner_extract");
}
