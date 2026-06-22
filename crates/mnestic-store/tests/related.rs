// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for `related_memory_ids`: two memories that share a distinctive graph
//! entity surface relate to each other after maintain resolves the watch, while an unrelated
//! memory does not. The shared surface is a rare token ("Zarathustra") so the built-in
//! tokenizer's noise (common words like "in"/"the") cannot pull the unrelated memory in.

use std::time::Duration;

use mnestic_store::{run_migrations, NewMemory, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
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
async fn related_memories_share_a_graph_entity() {
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
    run_migrations(&pool).await.expect("migrations");

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rel') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let store = Store::new(pool.clone());
    let mem = |content: &'static str| NewMemory {
        actor_id: "user",
        content,
        subject: None,
        attribute: None,
        value: None,
        single_valued: false,
    };

    // Two memories share the rare surface "Zarathustra"; the third shares nothing distinctive.
    let first = store.insert_memory(tenant, &mem("Zarathustra wrote at dawn")).await.expect("first");
    let second =
        store.insert_memory(tenant, &mem("Zarathustra climbed the mountain")).await.expect("second");
    let _unrelated =
        store.insert_memory(tenant, &mem("quokka pancakes for breakfast")).await.expect("unrelated");

    // The worker drives maintain in production; the test drives it directly to resolve the watch.
    store.graphwright_maintain().await.expect("maintain");

    let related = store.related_memory_ids(tenant, "user", first, 10).await.expect("related");
    let ids: Vec<Uuid> = related.iter().map(|r| r.memory_id).collect();
    assert!(
        ids.contains(&second),
        "the memory sharing the Zarathustra entity is related, got {ids:?}"
    );
    assert!(
        !ids.contains(&first),
        "a memory is not related to itself, got {ids:?}"
    );
    // The unrelated memory shares no distinctive surface, so it must not appear.
    assert!(
        related.iter().all(|r| r.content.as_deref() != Some("quokka pancakes for breakfast")),
        "the unrelated memory does not surface, got {related:?}"
    );
    // The related row carries the neighbor's content and a positive shared-entity count.
    let hit = related.iter().find(|r| r.memory_id == second).expect("second present");
    assert_eq!(hit.content.as_deref(), Some("Zarathustra climbed the mountain"));
    assert!(hit.shared >= 1, "at least one shared entity, got {}", hit.shared);
}
