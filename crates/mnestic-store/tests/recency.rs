// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for recency scoring in recall_memories: decay is on event time
//! (valid_time), honors an as_of reference instant, and clamps a future-dated fact instead of
//! boosting it. Rows are inserted with identical content + embedding so RRF and confidence are
//! constant and the recency factor is the only thing that moves the score.

use std::time::Duration;

use chrono::{DateTime, Utc};
use mnestic_store::{run_migrations, RecallParams, Store};
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

/// A pgvector text literal of `dim` copies of `val`, e.g. `[0.1,0.1,...]`.
fn embed_literal(val: f32, dim: usize) -> String {
    let mut s = String::with_capacity(dim * 4 + 2);
    s.push('[');
    for i in 0..dim {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&val.to_string());
    }
    s.push(']');
    s
}

async fn insert_memory(
    store: &Store,
    tenant: Uuid,
    actor: &str,
    content: &str,
    embedding: &str,
    valid_from: DateTime<Utc>,
    custom_id: &str,
) -> Uuid {
    let mut tx = store.begin_tenant(tenant).await.unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO mnestic_memory \
           (tenant_id, actor_id, content, embedding, valid_time, custom_id, confidence) \
         VALUES ($1, $2, $3, $4::halfvec, tstzrange($5, NULL), $6, 0.9) RETURNING id",
    )
    .bind(tenant)
    .bind(actor)
    .bind(content)
    .bind(embedding)
    .bind(valid_from)
    .bind(custom_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn recency_decays_on_event_time_with_as_of() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('rec') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");

    let store = Store::new(pool.clone());
    let qvec: Vec<f32> = vec![0.1; 1536];
    let emb = embed_literal(0.1, 1536);
    let now = Utc::now();

    // Actor u: two facts with the same content and embedding, one whose event is two years
    // old and one whose event is yesterday. Both are inserted now.
    let id_old =
        insert_memory(&store, tenant, "u", "alpha bravo charlie", &emb, now - chrono::Duration::days(730), "u-old").await;
    let id_recent =
        insert_memory(&store, tenant, "u", "alpha bravo charlie", &emb, now - chrono::Duration::days(1), "u-recent").await;

    // 1. Event-time basis: the recent-event memory outranks the old one even though both were
    //    written at the same instant, because decay is on valid_time, not ingest time.
    let recall = |as_of| RecallParams {
        tenant_id: tenant,
        actor_id: "u",
        query_embedding: &qvec,
        query_text: "alpha bravo charlie",
        container_tags: &[],
        limit: 10,
        as_of,
        filter: None,
        include_forgotten: false,
    };
    let hits = store.recall_memories(recall(None)).await.expect("recall now");
    let score_of = |id: Uuid| hits.iter().find(|h| h.id == id).map(|h| h.score);
    assert!(
        score_of(id_recent).unwrap() > score_of(id_old).unwrap(),
        "recent-event memory scores higher: {hits:?}"
    );
    assert_eq!(hits[0].id, id_recent, "recent-event memory ranks first");

    // 2. as_of honored: the old memory scores far higher when recall is anchored near its own
    //    event time than when anchored at now (5 days of decay vs ~730).
    let as_of_then = Some(now - chrono::Duration::days(725));
    let hits_then = store.recall_memories(recall(as_of_then)).await.expect("recall as-of then");
    let old_then = hits_then.iter().find(|h| h.id == id_old).unwrap().score;
    let old_now = score_of(id_old).unwrap();
    assert!(
        old_then > old_now * 100.0,
        "as_of near the event lifts the old memory's score: then={old_then} now={old_now}"
    );

    // Actor v: a future-dated fact and a present one, same content and embedding.
    let id_future =
        insert_memory(&store, tenant, "v", "delta echo foxtrot", &emb, now + chrono::Duration::days(365), "v-future").await;
    let id_present =
        insert_memory(&store, tenant, "v", "delta echo foxtrot", &emb, now, "v-present").await;

    // 3. Future clamp: a fact whose event is still ahead of as_of is not boosted above a
    //    present one. Recency caps at 1.0 rather than exp of a positive age, so the two land
    //    within a hair of each other instead of the future one dominating by ~1e5.
    let hits_v = store
        .recall_memories(RecallParams {
            tenant_id: tenant,
            actor_id: "v",
            query_embedding: &qvec,
            query_text: "delta echo foxtrot",
            container_tags: &[],
            limit: 10,
            as_of: None,
            filter: None,
            include_forgotten: false,
        })
        .await
        .expect("recall v");
    let v_score = |id: Uuid| hits_v.iter().find(|h| h.id == id).unwrap().score;
    assert!(
        v_score(id_future) <= v_score(id_present) * 1.05,
        "future-dated event not boosted: future={} present={}",
        v_score(id_future),
        v_score(id_present)
    );
}
