// SPDX-License-Identifier: MIT

//! Dockerized proof that the shipped runtime role enforces tenant isolation. The deployment
//! serves as the non-superuser `mnestic_app` (a superuser bypasses RLS even with FORCE), so this
//! provisions that role through `provision_app_role` (the same helper the server runs) and runs
//! every assertion over a pool connected as that role. It proves three things under the app role:
//! core operations work, a cross-tenant memory read returns zero rows, and the knowledge graph's
//! RLS delegation holds (one tenant's `related_memory_ids` / `actor_entities` never surface
//! another tenant's rows, even when both share a distinctive entity).

use std::time::Duration;

use mnestic_store::{provision_app_role, run_migrations, NewMemory, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use uuid::Uuid;

const APP_ROLE: &str = "mnestic_app";
const APP_PASSWORD: &str = "app-secret";

async fn connect(opts: PgConnectOptions) -> PgPool {
    // Postgres logs "ready to accept connections" twice during init, so trust a real connection
    // rather than the first log line. Retry for a few seconds.
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

fn mem(content: &'static str) -> NewMemory<'static> {
    NewMemory {
        actor_id: "user",
        content,
        subject: None,
        attribute: None,
        value: None,
        single_valued: false,
    }
}

#[tokio::test]
async fn app_role_enforces_tenant_isolation_including_the_graph() {
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

    // Superuser: migrate, then provision the runtime role through the real helper.
    let su_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    let su_pool = connect(su_opts).await;
    run_migrations(&su_pool).await.expect("migrations");
    provision_app_role(&su_pool, APP_ROLE, APP_PASSWORD).await.expect("provision app role");

    // The provisioned role must be NOBYPASSRLS, or the rest of this test would pass vacuously.
    let bypass: bool = sqlx::query_scalar("SELECT rolbypassrls FROM pg_roles WHERE rolname = $1")
        .bind(APP_ROLE)
        .fetch_one(&su_pool)
        .await
        .expect("role attrs");
    assert!(!bypass, "the runtime role must be NOBYPASSRLS");

    // Tenants live in the registry table (no RLS), so seed them as superuser.
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

    // Every assertion below runs as the non-superuser app role.
    let app_opts = PgConnectOptions::new()
        .host(&host.to_string())
        .port(port)
        .username(APP_ROLE)
        .password(APP_PASSWORD)
        .database("postgres");
    let store = Store::new(connect(app_opts).await);

    // (a) Core write works under the app role, and a recall (via the visibility count) sees it.
    let a_first =
        store.insert_memory(tenant_a, &mem("Zarathustra wrote at dawn")).await.expect("insert A1");
    store
        .insert_memory(tenant_a, &mem("Zarathustra climbed the mountain"))
        .await
        .expect("insert A2");
    assert_eq!(
        store.count_visible_memories(Some(tenant_a)).await.expect("count A"),
        2,
        "tenant A sees its own two rows under the app role"
    );

    // (b) Cross-tenant memory read returns zero of tenant A's rows. Tenant B writes a memory that
    // shares the rare "Zarathustra" surface, so the graph below has a real cross-tenant overlap.
    store
        .insert_memory(tenant_b, &mem("Zarathustra is tenant B's secret"))
        .await
        .expect("insert B1");
    store.insert_memory(tenant_b, &mem("Zarathustra B private note")).await.expect("insert B2");
    assert_eq!(
        store.count_visible_memories(Some(tenant_b)).await.expect("count B"),
        2,
        "tenant B sees only its own rows, never tenant A's"
    );
    // With the GUC unset the policy matches nothing (fail-closed), even for the app role.
    assert_eq!(
        store.count_visible_memories(None).await.expect("count unset"),
        0,
        "an unset tenant GUC is fail-closed under the app role"
    );

    // Resolve the graph as the app role (the worker drives this in production). EXECUTE on
    // maintain() is the only graphwright function grant the role holds.
    store.graphwright_maintain().await.expect("maintain under app role");

    // (c) Graph cross-tenant isolation: tenant A's related/entity reads never surface tenant B's
    // rows, even though both tenants' memories mention "Zarathustra".
    let related =
        store.related_memory_ids(tenant_a, "user", a_first, 50).await.expect("related under A");
    for r in &related {
        let content = r.content.as_deref().unwrap_or("");
        assert!(
            !content.contains("tenant B") && !content.contains("B private"),
            "tenant A related-memory leaked a tenant B row: {content:?}"
        );
    }
    // The within-tenant relation still resolves, so the isolation is not just an empty result.
    assert!(
        related.iter().any(|r| r.content.as_deref() == Some("Zarathustra climbed the mountain")),
        "the within-tenant Zarathustra memory still relates, got {related:?}"
    );

    let entities = store.actor_entities(tenant_a, "user", 50).await.expect("entities under A");
    let zarathustra = entities
        .iter()
        .find(|(surface, _)| surface.eq_ignore_ascii_case("zarathustra"))
        .map(|(_, n)| *n)
        .expect("tenant A mentions Zarathustra");
    // Tenant A has exactly two memories mentioning the surface; tenant B's two must not be counted.
    assert_eq!(
        zarathustra, 2,
        "tenant A's Zarathustra mention count must exclude tenant B's, got {zarathustra}"
    );

    // Belt-and-braces: the raw mention catalog is itself RLS-delegated for the app role, so a
    // direct count under tenant A's GUC sees only tenant A's mentions, not all four memories'.
    let direct: i64 = {
        let mut tx = store.begin_tenant(tenant_a).await.expect("tx");
        let n: i64 = sqlx::query(
            "SELECT count(DISTINCT m.source_pk) AS n FROM graphwright.mention m \
             JOIN mnestic_memory mem ON mem.id = m.source_pk::uuid",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("mention count")
        .get("n");
        tx.commit().await.expect("commit");
        n
    };
    assert_eq!(direct, 2, "graph mentions are RLS-scoped to tenant A under the app role");
}
