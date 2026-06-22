// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for the memory-relation edges: an inserted `extends` edge is
//! visible from both endpoints with the right `outgoing` flag, and forgetting either
//! endpoint deletes the edge so no dangling edge survives.

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
async fn relation_edges_roundtrip_and_forget_cleanup() {
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

    let tenant: Uuid =
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rel') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let store = Store::new(pool.clone());

    // Two same-subject memories: the second extends the first.
    let from_id = store
        .insert_memory(
            tenant,
            &NewMemory {
                actor_id: "user",
                content: "user adopted a dog",
                subject: Some("user"),
                attribute: None,
                value: None,
                single_valued: false,
            },
        )
        .await
        .expect("insert from");
    let to_id = store
        .insert_memory(
            tenant,
            &NewMemory {
                actor_id: "user",
                content: "user has a pet",
                subject: Some("user"),
                attribute: None,
                value: None,
                single_valued: false,
            },
        )
        .await
        .expect("insert to");

    {
        let mut tx = store.begin_tenant(tenant).await.expect("tx");
        Store::insert_relation_tx(&mut tx, tenant, "user", from_id, to_id, "extends")
            .await
            .expect("insert edge");
        // A second identical insert is a no-op (ON CONFLICT), so the edge stays unique.
        Store::insert_relation_tx(&mut tx, tenant, "user", from_id, to_id, "extends")
            .await
            .expect("insert edge again");
        tx.commit().await.expect("commit");
    }

    // From the `from_id` side the edge is outgoing; the neighbor is the `to` memory.
    let from_edges = store
        .relation_edges_for(tenant, "user", from_id)
        .await
        .expect("edges from");
    assert_eq!(from_edges.len(), 1, "exactly one edge from the from-side");
    assert!(from_edges[0].outgoing, "from-side sees an outgoing edge");
    assert_eq!(from_edges[0].relation, "extends");
    assert_eq!(from_edges[0].neighbor_content.as_deref(), Some("user has a pet"));

    // From the `to_id` side the same edge is incoming; the neighbor is the `from` memory.
    let to_edges = store
        .relation_edges_for(tenant, "user", to_id)
        .await
        .expect("edges to");
    assert_eq!(to_edges.len(), 1, "exactly one edge from the to-side");
    assert!(!to_edges[0].outgoing, "to-side sees an incoming edge");
    assert_eq!(to_edges[0].relation, "extends");
    assert_eq!(to_edges[0].neighbor_content.as_deref(), Some("user adopted a dog"));

    // Forgetting one endpoint deletes the edge, so neither side keeps a dangling edge.
    {
        let mut tx = store.begin_tenant(tenant).await.expect("tx");
        let n = Store::forget_memory_by_id_tx(&mut tx, tenant, "user", to_id, Some("test"))
            .await
            .expect("forget");
        assert_eq!(n, 1, "the to-memory was tombstoned");
        tx.commit().await.expect("commit");
    }
    assert!(
        store.relation_edges_for(tenant, "user", from_id).await.expect("edges").is_empty(),
        "forgetting an endpoint deletes the edge from the surviving side"
    );
    assert!(
        store.relation_edges_for(tenant, "user", to_id).await.expect("edges").is_empty(),
        "forgetting an endpoint deletes the edge from the forgotten side"
    );
}
