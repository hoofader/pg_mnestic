// SPDX-License-Identifier: AGPL-3.0-only

//! Dockerized test for /v4/search and /v4/profile: ingest under an actor via the
//! engine, then drive the read endpoints with tower::oneshot and a mock engine.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mnestic_core::{Embedder, Extractor};
use mnestic_engine::Engine;
use mnestic_model::{MockEmbedder, MockExtractor};
use mnestic_server::{app, AppState};
use mnestic_store::{run_migrations, Store};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};
use tower::ServiceExt;
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

fn post(uri: &str, token: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn search_and_profile_endpoints() {
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
        sqlx::query_scalar("INSERT INTO mnestic_tenant (external_id) VALUES ('q') RETURNING id")
            .fetch_one(&pool)
            .await
            .expect("tenant");
    sqlx::query("INSERT INTO mnestic_api_key (token_sha256, tenant_id) VALUES (digest($1, 'sha256'), $2)")
        .bind("sk-test")
        .bind(tenant)
        .execute(&pool)
        .await
        .expect("api key");

    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder);
    let extractor: Arc<dyn Extractor> = Arc::new(MockExtractor);
    let engine = Arc::new(Engine::new(Store::new(pool.clone()), embedder, extractor));
    let state = AppState { engine: engine.clone(), limiter: mnestic_server::RateLimiter::disabled() };

    // Seed memory under actor user:99 (the actor a containerTag of org:7:user:99 maps to).
    engine
        .add(tenant, "user:99", &["org:7".to_string()], "the user loves climbing", "conversation", None)
        .await
        .unwrap();

    // Search finds the seeded memory.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["containerTag"], "org:7:user:99", "echoes the tag");
    let results = j["results"].as_array().unwrap();
    let memories: Vec<&str> = results.iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "search returns the memory, got {memories:?}");
    assert!(results.iter().all(|r| r["similarity"].is_number()), "each result carries a similarity score");
    assert!(results.iter().all(|r| r["updatedAt"].is_string()), "each result carries updatedAt");
    // sdk-ts SearchMemoriesResponse: top-level timing/total, per-result metadata object.
    assert!(j["timing"].is_number(), "search carries timing");
    assert!(j["total"].is_number(), "search carries total");
    assert!(results.iter().all(|r| r["metadata"].is_object()), "each result carries a metadata object");
    // Memory hits carry `memory` and never a `chunk` key (it is skipped when None).
    assert!(results.iter().all(|r| r.get("chunk").is_none()), "memory hits omit chunk");

    // Seed a document for the same actor so the searchMode/hybrid paths have a chunk to find.
    engine
        .ingest_document(
            tenant,
            "user:99",
            &["org:7".to_string()],
            Some("Doc"),
            None,
            "a note about climbing knots and belaying",
            Some("dq"),
            &serde_json::json!({}),
        )
        .await
        .unwrap();

    // searchMode "documents" returns chunk hits: each entry carries a `chunk` string and null memory.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"documents"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(!results.is_empty(), "documents mode returns at least one chunk");
    assert!(
        results.iter().all(|r| r["chunk"].is_string() && r["memory"].is_null()),
        "documents mode entries carry a chunk string and null memory, got {results:?}"
    );

    // searchMode "memories" (explicit) matches the default: memory set, no chunk key.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"memories"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["memory"].as_str() == Some("the user loves climbing")),
        "memories mode returns the memory"
    );
    assert!(results.iter().all(|r| r.get("chunk").is_none()), "memories mode omits chunk");

    // searchMode "hybrid" returns both kinds: at least one memory and at least one chunk.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","searchMode":"hybrid"}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["memory"].is_string()),
        "hybrid returns at least one memory hit, got {results:?}"
    );
    assert!(
        results.iter().any(|r| r["chunk"].is_string()),
        "hybrid returns at least one chunk hit, got {results:?}"
    );

    // Seed a second memory that is lexically irrelevant to "climbing". In RECALL_SQL a memory whose
    // text matches the tsquery gets both a vec and a lex RRF contribution, while one that does not
    // match the tsquery is dropped from the lex CTE and keeps only the vec contribution. So the
    // matching memory scores roughly twice the non-matching one (mock embeddings are a hash, so the
    // vec ranks are noise but bounded; the lex doubling dominates). A relative threshold of 0.9 thus
    // deterministically keeps the strong hit and drops the weak one.
    engine
        .add(tenant, "user:99", &["org:7".to_string()], "the user dislikes loud music", "conversation", None)
        .await
        .unwrap();

    // Without a threshold, both memories come back.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let memories: Vec<&str> =
        j["results"].as_array().unwrap().iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "strong hit present without threshold");
    assert!(memories.contains(&"the user dislikes loud music"), "weak hit present without threshold");

    // A high threshold keeps the strong hit and drops the weak one.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"climbing","containerTag":"org:7:user:99","threshold":0.9}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let memories: Vec<&str> =
        j["results"].as_array().unwrap().iter().filter_map(|r| r["memory"].as_str()).collect();
    assert!(memories.contains(&"the user loves climbing"), "threshold keeps the strong hit");
    assert!(!memories.contains(&"the user dislikes loud music"), "threshold drops the weak hit, got {memories:?}");

    // include.forgottenMemories surfaces tombstoned memories. Forget the weak memory; it drops
    // from a normal search but returns when the flag is set.
    let weak_id = engine
        .recall(tenant, "user:99", "music", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.content.as_deref() == Some("the user dislikes loud music"))
        .expect("weak memory present")
        .id;
    engine.forget_by_id(tenant, "user:99", weak_id, Some("test")).await.unwrap();
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"music","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert!(
        !j["results"].as_array().unwrap().iter().any(|r| r["memory"] == "the user dislikes loud music"),
        "forgotten memory is absent from a normal search"
    );
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"music","containerTag":"org:7:user:99","include":{"forgottenMemories":true}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    assert!(
        j["results"].as_array().unwrap().iter().any(|r| r["memory"] == "the user dislikes loud music"),
        "include.forgottenMemories surfaces the tombstoned memory"
    );

    // A malformed containerTag is rejected at the edge.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"x","containerTag":"bad tag!"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "invalid containerTag rejected");

    // Plural containerTags (single-element) is accepted as the same scope.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTags":["org:7:user:99"]}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "plural containerTags accepted");

    // An empty query is rejected, and a bad limit is clamped (not a 500).
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"  ","containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "empty q rejected");
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "sk-test", r#"{"q":"climbing","containerTag":"org:7:user:99","limit":-5}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "negative limit is clamped, not a 500");

    // Profile returns the actor's profile; a query also returns relevant memories.
    let resp = app(state.clone())
        .oneshot(post("/v4/profile", "sk-test", r#"{"containerTag":"org:7:user:99","q":"climbing"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    // sdk-ts ProfileResponse: profile.static / profile.dynamic, recall under searchResults.
    assert!(j["profile"]["static"].is_array(), "profile.static present");
    assert!(j["profile"]["dynamic"].is_array(), "profile.dynamic present");
    assert!(j["searchResults"]["timing"].is_number(), "searchResults.timing present");
    assert!(j["searchResults"]["total"].is_number(), "searchResults.total present");
    let rel: Vec<&str> = j["searchResults"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["memory"].as_str())
        .collect();
    assert!(rel.contains(&"the user loves climbing"), "profile query returns relevant memory");

    // Without a query there is no searchResults block (the SDK types it optional).
    let resp = app(state.clone())
        .oneshot(post("/v4/profile", "sk-test", r#"{"containerTag":"org:7:user:99"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(j["profile"]["static"].is_array(), "profile body present without a query");
    assert!(j["searchResults"].is_null(), "no searchResults block without a query");

    // Read endpoints also require auth.
    let resp = app(state.clone())
        .oneshot(post("/v4/search", "nope", r#"{"q":"x","containerTag":"alice"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Seed two memories under one actor with different metadata; the `filters` tree retains
    // only the matching one. Both share the term "hiking" so recall returns both candidates.
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking with the infra team",
            "conversation",
            None,
            None,
            &serde_json::json!({"team": "infra"}),
        )
        .await
        .unwrap();
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking with the sales team",
            "conversation",
            None,
            None,
            &serde_json::json!({"team": "sales"}),
        )
        .await
        .unwrap();
    // Two more rows that LACK the `team` key but carry a numeric `level`, so the negate and
    // numeric cases below have rows to exercise the SQL parity edges.
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking at level three",
            "conversation",
            None,
            None,
            &serde_json::json!({"level": "3", "team": "infra"}),
        )
        .await
        .unwrap();
    engine
        .add_at(
            tenant,
            "user:fil",
            &["org:9".to_string()],
            "hiking at level seven no team key",
            "conversation",
            None,
            None,
            &serde_json::json!({"level": "7"}),
        )
        .await
        .unwrap();

    // An AND filter on team=infra keeps only the infra memory.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"infra"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    let teams: Vec<&str> =
        results.iter().filter_map(|r| r["metadata"]["team"].as_str()).collect();
    // Two rows carry team=infra; the filter keeps exactly those and nothing else.
    assert!(
        !teams.is_empty() && teams.iter().all(|t| *t == "infra"),
        "filter keeps only infra memories, got {teams:?}"
    );

    // `negate` flips the same predicate. A row that LACKS the `team` key matches under negate
    // (missing-key + negate -> match), proving SQL parity with the Rust path. The level-7 row has
    // no `team` key, so it is in the result but contributes no team string here.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"infra","negate":true}]}}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    // No surviving row may carry team=infra.
    assert!(
        results.iter().all(|r| r["metadata"]["team"].as_str() != Some("infra")),
        "negate excludes every infra row"
    );
    // The missing-key row (level 7, no team) survives negate, matching the Rust path.
    assert!(
        results.iter().any(|r| r["metadata"]["level"].as_str() == Some("7")),
        "missing-key row survives negate, got {results:?}"
    );

    // A numeric `>` filter returns only rows whose `level` is above the value (level 7, not 3).
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"level","value":"5","filterType":"numeric","numericOperator":">"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let levels: Vec<&str> = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["metadata"]["level"].as_str())
        .collect();
    assert_eq!(levels, vec!["7"], "numeric > keeps only the level-7 row, got {levels:?}");

    // An AND of two keys requires both: team=infra AND level=3 matches only the level-3 infra row.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"infra"},{"key":"level","value":"3"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let results = j["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "AND of two keys matches exactly one row, got {results:?}");
    assert_eq!(results[0]["metadata"]["team"].as_str(), Some("infra"));
    assert_eq!(results[0]["metadata"]["level"].as_str(), Some("3"));

    // A filter that matches nothing returns an empty result set, not an error.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"hiking","containerTag":"org:9:user:fil","filters":{"AND":[{"key":"team","value":"nobody"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["results"].as_array().unwrap().len(), 0, "non-matching filter returns empty");
    assert_eq!(j["total"], 0, "total reflects the filtered set");

    // The same filter applies to /v4/profile's recall results.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/profile",
            "sk-test",
            r#"{"containerTag":"org:9:user:fil","q":"hiking","filters":{"AND":[{"key":"team","value":"infra"}]}}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let teams: Vec<&str> = j["searchResults"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["metadata"]["team"].as_str())
        .collect();
    assert!(
        !teams.is_empty() && teams.iter().all(|t| *t == "infra"),
        "profile filter keeps only infra memories, got {teams:?}"
    );

    // aggregate relations: seed two same-subject memories and wire an `extends` edge between
    // them via the store (no live classifier in the test), then assert the aggregate context
    // surfaces it. The outgoing endpoint carries the neighbor as a parent.
    engine
        .add(tenant, "user:rel", &["org:2".to_string()], "user adopted a dog", "conversation", None)
        .await
        .unwrap();
    engine
        .add(tenant, "user:rel", &["org:2".to_string()], "user has a pet animal", "conversation", None)
        .await
        .unwrap();
    let dog_id = engine
        .recall(tenant, "user:rel", "dog", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.content.as_deref() == Some("user adopted a dog"))
        .expect("dog memory present")
        .id;
    let pet_id = engine
        .recall(tenant, "user:rel", "pet", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.content.as_deref() == Some("user has a pet animal"))
        .expect("pet memory present")
        .id;
    {
        let mut tx = engine.store().begin_tenant(tenant).await.unwrap();
        Store::insert_relation_tx(&mut tx, tenant, "user:rel", dog_id, pet_id, "extends")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    // The `from` memory (dog) extends FROM the `to` memory (pet), so pet lands in parents.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"dog","containerTag":"org:2:user:rel","aggregate":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(dog_id.to_string().as_str()))
        .expect("the extending memory present in aggregate search");
    let parents = hit["context"]["parents"].as_array().unwrap();
    assert!(
        parents.iter().any(|n| n["relation"].as_str() == Some("extends")
            && n["memory"].as_str() == Some("user has a pet animal")),
        "the extends neighbor lands in parents, got {parents:?}"
    );
    // The `to` memory (pet) sees the same edge as incoming, so the dog lands in its children.
    let resp = app(state.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"pet","containerTag":"org:2:user:rel","aggregate":true}"#,
        ))
        .await
        .unwrap();
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(pet_id.to_string().as_str()))
        .expect("the extended memory present in aggregate search");
    let children = hit["context"]["children"].as_array().unwrap();
    assert!(
        children.iter().any(|n| n["relation"].as_str() == Some("extends")
            && n["memory"].as_str() == Some("user adopted a dog")),
        "the incoming extends neighbor lands in children, got {children:?}"
    );

    // aggregate: seed a memory, then edit it so there is a 2-version chain (v2 supersedes v1).
    // The edit (via the engine, the same path as PATCH /v4/memories) preserves the prior as
    // history, so the latest version's aggregate context carries the prior chain version.
    let state_agg = state;
    engine
        .add(tenant, "user:agg", &["org:1".to_string()], "the user enjoys sailing", "conversation", None)
        .await
        .unwrap();
    let v1_id = engine
        .recall(tenant, "user:agg", "sailing", 10)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.content.as_deref() == Some("the user enjoys sailing"))
        .expect("seeded memory present")
        .id;
    let v2 = engine
        .update_memory(
            tenant,
            "user:agg",
            v1_id,
            "the user enjoys sailing and kayaking",
            None,
            None,
            &serde_json::json!({}),
            None,
            None,
        )
        .await
        .unwrap()
        .expect("edit creates a new version");
    let v2_id = v2.id.to_string();

    // With aggregate, the latest version's result carries isAggregated, a context object with
    // the prior chain version (relation "updates"), and a documents array (the source).
    let resp = app(state_agg.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"kayaking","containerTag":"org:1:user:agg","aggregate":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(v2_id.as_str()))
        .expect("latest version present in aggregate search");
    assert_eq!(hit["isAggregated"], true, "aggregate marks the result");
    assert!(hit["context"].is_object(), "aggregate carries a context object");
    // The prior version (v1) appears in parents or children, with relation "updates". v1 is an
    // earlier version, so it lands in parents.
    let chain: Vec<&serde_json::Value> = hit["context"]["parents"]
        .as_array()
        .unwrap()
        .iter()
        .chain(hit["context"]["children"].as_array().unwrap().iter())
        .collect();
    assert!(
        chain.iter().any(|n| n["relation"].as_str() == Some("updates")),
        "the chain version has relation updates, got {chain:?}"
    );
    assert!(
        hit["context"]["parents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["memory"].as_str() == Some("the user enjoys sailing")),
        "the earlier version lands in parents, got {:?}",
        hit["context"]["parents"]
    );
    // documents is always present under aggregate. The latest version is the edit row, which
    // carries no source of its own (the source rode the prior version), so its documents array
    // is empty; the source surfacing is asserted below on a memory that owns one.
    assert!(hit["documents"].is_array(), "aggregate carries documents");

    // A memory added directly (not edited) owns its source, so its documents array carries the
    // source row, typed by the source kind.
    engine
        .add(tenant, "user:agg", &["org:1".to_string()], "the user owns a telescope", "conversation", None)
        .await
        .unwrap();
    let resp = app(state_agg.clone())
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"telescope","containerTag":"org:1:user:agg","aggregate":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["memory"].as_str() == Some("the user owns a telescope"))
        .expect("the source-owning memory present");
    let docs = hit["documents"].as_array().unwrap();
    assert!(!docs.is_empty(), "the memory's source surfaces as a document, got {docs:?}");
    assert_eq!(docs[0]["type"].as_str(), Some("conversation"), "document type is the source kind");
    assert!(docs[0]["id"].is_string(), "document carries the source id");
    assert!(docs[0]["createdAt"].is_string() && docs[0]["updatedAt"].is_string(), "document carries timestamps");

    // Without aggregate, the result shape is unchanged: no context/documents/isAggregated keys.
    let resp = app(state_agg)
        .oneshot(post(
            "/v4/search",
            "sk-test",
            r#"{"q":"kayaking","containerTag":"org:1:user:agg"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let hit = j["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"].as_str() == Some(v2_id.as_str()))
        .expect("latest version present without aggregate");
    assert!(hit.get("context").is_none(), "no context key without aggregate");
    assert!(hit.get("documents").is_none(), "no documents key without aggregate");
    assert!(hit.get("isAggregated").is_none(), "no isAggregated key without aggregate");
}
