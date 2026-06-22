// SPDX-License-Identifier: AGPL-3.0-only

//! The read endpoints: `/v4/search` (recall) and `/v4/profile` (profile plus
//! query-relevant memories). Both scope the actor and container tags from the
//! `containerTag` and echo it back.

use std::time::Instant;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use mnestic_engine::{ChunkHit, LineageRow, RecallHit, RelEdge, RelatedRow, SourceRow};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::authenticate_request;
use crate::container_tag::{parse_container_tag, Scope};
use crate::error::ApiError;
use crate::filter::{to_meta_filter, FilterNode};
use crate::{clamp_limit, resolve_container_tag, AppState};

/// Candidate pool when `filters` or `threshold` will thin the results in Rust (not in SQL), so
/// over-fetch and then retain survivors. The `* 4` gives headroom for a selective filter or a
/// high cutoff; `.min(200)` keeps the engine scan bounded.
fn filter_pool(limit: i64) -> i64 {
    limit.saturating_mul(4).min(200)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub id: String,
    /// Cleartext content, or null for an encrypted-at-rest row.
    pub memory: Option<String>,
    /// Document-chunk text under `searchMode` `documents`/`hybrid`; absent for memory hits so
    /// the SDK can tell the two apart on one result type (sdk-ts `memory?` vs `chunk?`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk: Option<String>,
    /// The fused relevance score (wire name `similarity`, what the shells read).
    pub similarity: f64,
    /// System time of the row (doc 04 §4 hit field).
    pub updated_at: Option<String>,
    /// Caller metadata, `{}` until ingest stores any (sdk-ts types it required, nullable).
    pub metadata: serde_json::Value,
    /// Aggregate view (sdk-ts SearchMemoriesResponse.Result `context`), present only when
    /// the request set `aggregate`. None keeps the wire shape unchanged otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<MemoryContext>,
    /// The source documents behind this memory (sdk-ts `documents`); set only when aggregating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<Vec<DocumentRef>>,
    /// sdk-ts `isAggregated`. Set only when aggregating, so a plain search omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_aggregated: Option<bool>,
}

/// One related memory in the aggregate `context` (sdk-ts Node). `relation` is `"updates"`
/// for a supersession-chain version, or `"extends"`/`"derives"` for a graph edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextNode {
    pub memory: Option<String>,
    pub relation: String,
    pub updated_at: Option<String>,
    pub metadata: serde_json::Value,
    pub version: i32,
}

/// The aggregate `context` for a memory. `parents` holds the chain versions earlier than
/// this one plus the neighbors it extends/derives FROM; `children` holds the later chain
/// versions plus the neighbors that extend/derive FROM it. `related` holds memories that share
/// a knowledge-graph entity with it (an undirected association, not a lineage or edge).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryContext {
    pub parents: Vec<ContextNode>,
    pub children: Vec<ContextNode>,
    pub related: Vec<ContextNode>,
}

/// One source document behind a memory (sdk-ts `documents` element). `created_at` and
/// `updated_at` both carry the source's `ingested_at`, since a source is ingested once.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRef {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub metadata: serde_json::Value,
    #[serde(rename = "type")]
    pub doc_type: Option<String>,
}

fn to_result(h: RecallHit) -> SearchResult {
    SearchResult {
        id: h.id.to_string(),
        memory: h.content,
        chunk: None,
        similarity: h.score,
        updated_at: h.recorded_at.map(|t| t.to_rfc3339()),
        metadata: h.metadata,
        context: None,
        documents: None,
        is_aggregated: None,
    }
}

fn to_doc_result(h: ChunkHit) -> SearchResult {
    SearchResult {
        id: h.id.to_string(),
        memory: None,
        chunk: Some(h.content),
        similarity: h.score,
        updated_at: h.document_created_at.map(|t| t.to_rfc3339()),
        metadata: h.document_metadata,
        context: None,
        documents: None,
        is_aggregated: None,
    }
}

/// Map a chain row to a context node. Relation is `"updates"` for every chain row, since
/// these are all versions of the same memory.
fn to_context_node(r: LineageRow) -> ContextNode {
    ContextNode {
        memory: r.content,
        relation: "updates".to_string(),
        updated_at: r.updated_at.map(|t| t.to_rfc3339()),
        metadata: r.metadata,
        version: r.version,
    }
}

/// Map a relation edge to a context node carrying the neighbor's row. The edge's own
/// `relation` ("extends"/"derives") rides through, so the caller can place the node in
/// parents (outgoing) or children (incoming).
fn edge_to_context_node(e: RelEdge) -> ContextNode {
    ContextNode {
        memory: e.neighbor_content,
        relation: e.relation,
        updated_at: e.neighbor_updated_at.map(|t| t.to_rfc3339()),
        metadata: e.neighbor_metadata,
        version: e.neighbor_version,
    }
}

/// Map a graph-entity-sharing neighbor to a context node. The relation is `"related"`: the two
/// memories mention a common entity, which is an association, not a supersession or a directed
/// `extends`/`derives` edge.
fn related_to_context_node(r: RelatedRow) -> ContextNode {
    ContextNode {
        memory: r.content,
        relation: "related".to_string(),
        updated_at: r.updated_at.map(|t| t.to_rfc3339()),
        metadata: r.metadata,
        version: r.version,
    }
}

/// Map a source row to the one document reference behind a memory.
fn to_document_ref(s: SourceRow) -> DocumentRef {
    let at = s.ingested_at.to_rfc3339();
    DocumentRef {
        id: s.id.to_string(),
        created_at: at.clone(),
        updated_at: at,
        metadata: s.metadata,
        doc_type: Some(s.kind),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub q: String,
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// Optional metadata-filter tree (sdk-ts `SearchMemoriesParams.filters`).
    #[serde(default)]
    pub filters: Option<FilterNode>,
    /// `memories` (default/unknown), `documents`, or `hybrid` (sdk-ts `searchMode`). Lenient by
    /// design: anything we don't recognize falls back to memory recall to preserve today's path.
    #[serde(default)]
    pub search_mode: Option<String>,
    /// Optional relative cutoff in `[0, 1]` (sdk-ts `threshold`); see `search` for the semantics.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// sdk-ts `SearchMemoriesParams.include`; `forgottenMemories` surfaces tombstoned memories.
    #[serde(default)]
    pub include: Option<Include>,
    /// sdk-ts `SearchMemoriesParams.rerank`. Opt out of reranking for this request even when a
    /// reranker is configured; defaults to true so a configured reranker is used by default.
    #[serde(default)]
    pub rerank: Option<bool>,
    /// sdk-ts `SearchMemoriesParams.aggregate`. When true, each memory hit is enriched with its
    /// supersession `context` and `documents`. Absent/false leaves the wire shape unchanged.
    #[serde(default)]
    pub aggregate: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Include {
    #[serde(default)]
    pub forgotten_memories: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub container_tag: String,
    pub results: Vec<SearchResult>,
    /// Wall-clock milliseconds for the recall (sdk-ts SearchMemoriesResponse.timing).
    pub timing: u64,
    pub total: usize,
}

/// Recall scoped to the actor. `searchMode` picks the corpus (memories, documents, or both).
/// `filters` is pushed into SQL on both corpora (memory metadata and document metadata), so it is
/// exact. `threshold` still thins the results in Rust over the over-fetched pool, so at extreme
/// scale a hit ranked below the over-fetch pool is not seen; the cutoff is best-effort.
pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    if req.q.trim().is_empty() {
        return Err(ApiError::BadRequest("q is empty".into()));
    }
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let limit = clamp_limit(req.limit);
    // Both the memory and document paths push the filter into SQL, so it is exact and needs no
    // filter over-fetch. `threshold` is always a Rust cutoff, so it alone forces an over-fetch.
    let overfetch = req.threshold.is_some();
    let recall_limit = if overfetch { filter_pool(limit) } else { limit };
    // Pushed into SQL on both paths: the memory recall and the document chunk search.
    let meta_filter = req.filters.as_ref().map(to_meta_filter);
    // When set, memory recall also returns forgotten (tombstoned) rows.
    let include_forgotten = req.include.as_ref().and_then(|i| i.forgotten_memories).unwrap_or(false);
    // Default true so a configured reranker is applied unless the caller opts out.
    let rerank = req.rerank.unwrap_or(true);

    // Time only the engine fetch(es), not the in-Rust threshold/truncate.
    let started = Instant::now();
    let mut results: Vec<SearchResult> = match req.search_mode.as_deref() {
        Some("documents") => state
            .engine
            .search_documents(
                tenant,
                &actor_id,
                &container_tags,
                &req.q,
                recall_limit,
                meta_filter.as_ref(),
            )
            .await?
            .into_iter()
            .map(to_doc_result)
            .collect(),
        Some("hybrid") => {
            let mem = state
                .engine
                .recall_scoped(
                    tenant,
                    &actor_id,
                    &container_tags,
                    &req.q,
                    recall_limit,
                    meta_filter.as_ref(),
                    include_forgotten,
                    rerank,
                )
                .await?;
            let docs = state
                .engine
                .search_documents(
                    tenant,
                    &actor_id,
                    &container_tags,
                    &req.q,
                    recall_limit,
                    meta_filter.as_ref(),
                )
                .await?;
            // Memory and chunk scores come from independent indexes (different corpora, different
            // RRF rank lists), so they are not comparable on one axis. Keep memories first, then
            // documents, rather than merge-sorting by `similarity` across the two.
            mem.into_iter()
                .map(to_result)
                .chain(docs.into_iter().map(to_doc_result))
                .collect()
        }
        // `memories`, absent, and any unknown value keep the original memory-recall behavior.
        _ => state
            .engine
            .recall_scoped(
                tenant,
                &actor_id,
                &container_tags,
                &req.q,
                recall_limit,
                meta_filter.as_ref(),
                include_forgotten,
                rerank,
            )
            .await?
            .into_iter()
            .map(to_result)
            .collect(),
    };
    let timing = started.elapsed().as_millis() as u64;

    if let Some(threshold) = req.threshold {
        // Our `similarity` is a fused RRF score, not an absolute 0-1 cosine, so an absolute cutoff
        // would be meaningless across queries. We read `threshold` as a relative cutoff against the
        // strongest hit for this query: keep hits scoring at least `threshold` of the top score.
        // This diverges from supermemory, where `threshold` is an absolute score. In `hybrid` mode
        // the top is taken over the concatenated list, and chunk scores carry no confidence/recency
        // weighting, so a moderate cutoff can drop chunks even when relevant.
        let top = results.iter().map(|r| r.similarity).fold(0.0_f64, f64::max);
        if top > 0.0 {
            results.retain(|r| r.similarity / top >= threshold);
        }
    }
    // Hybrid concatenation and over-fetch can both exceed `limit`, so truncate on every path now.
    results.truncate(limit as usize);

    // Enrich after truncate, so at most `limit` extra context reads run regardless of the
    // over-fetch pool. Document-mode (`chunk`) hits have no memory lineage, so they are left
    // un-enriched. The enrichment is extra read work, so it stays outside the timed section,
    // matching how the threshold/truncate are.
    if req.aggregate == Some(true) {
        for r in results.iter_mut() {
            if r.chunk.is_some() {
                continue;
            }
            let Ok(memory_id) = Uuid::parse_str(&r.id) else { continue };
            let ctx = state.engine.memory_context(tenant, &actor_id, memory_id).await?;
            let (chain_parents, chain_children): (Vec<LineageRow>, Vec<LineageRow>) =
                ctx.lineage.into_iter().partition(|l| l.is_parent);
            // An outgoing edge means the memory extends/derives FROM the neighbor, so the
            // neighbor is a parent; an incoming edge makes the neighbor a child.
            let (out_edges, in_edges): (Vec<RelEdge>, Vec<RelEdge>) =
                ctx.edges.into_iter().partition(|e| e.outgoing);
            let parents: Vec<ContextNode> = chain_parents
                .into_iter()
                .map(to_context_node)
                .chain(out_edges.into_iter().map(edge_to_context_node))
                .collect();
            let children: Vec<ContextNode> = chain_children
                .into_iter()
                .map(to_context_node)
                .chain(in_edges.into_iter().map(edge_to_context_node))
                .collect();
            let related: Vec<ContextNode> =
                ctx.related.into_iter().map(related_to_context_node).collect();
            r.context = Some(MemoryContext { parents, children, related });
            r.documents = Some(ctx.source.into_iter().map(to_document_ref).collect());
            r.is_aggregated = Some(true);
        }
    }

    let total = results.len();
    Ok(Json(SearchResponse { container_tag: tag, results, timing, total }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRequest {
    #[serde(default)]
    pub container_tag: Option<String>,
    #[serde(default)]
    pub container_tags: Option<Vec<String>>,
    /// Optional; a blank or absent query returns the profile without recall.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// Optional metadata-filter tree (sdk-ts `ProfileParams.filters`); applied to the recall
    /// results only, never to the profile's static/dynamic arrays.
    #[serde(default)]
    pub filters: Option<FilterNode>,
}

// The supermemory SDK reads `profile.static` / `profile.dynamic` (sdk-ts ProfileResponse.Profile).
#[derive(Serialize)]
pub struct ProfileBody {
    #[serde(rename = "static")]
    pub static_facts: Vec<String>,
    #[serde(rename = "dynamic")]
    pub dynamic_ctx: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSearchResults {
    pub results: Vec<SearchResult>,
    /// Wall-clock milliseconds to build the profile and run recall.
    pub timing: u64,
    pub total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResponse {
    pub container_tag: String,
    pub profile: ProfileBody,
    /// Present only when a query was given, mirroring the SDK's optional `searchResults`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_results: Option<ProfileSearchResults>,
}

/// `filters`, when present, is applied in Rust over the recall pool under `searchResults`, not
/// pushed into the SQL. Same best-effort caveat as `/v4/search`: a match ranked below the
/// over-fetch pool is not seen. The profile's static/dynamic arrays are not filtered.
pub async fn profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProfileRequest>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let tenant = authenticate_request(&state, &headers).await?;
    let tag = resolve_container_tag(req.container_tag, req.container_tags)?;
    let Scope { actor_id, container_tags } = parse_container_tag(&tag);
    let has_query = req.q.as_deref().is_some_and(|s| !s.trim().is_empty());
    let limit = clamp_limit(req.limit);
    // The filter is pushed into the recall SQL, so the profile recall is exact and needs no
    // over-fetch.
    let meta_filter = req.filters.as_ref().map(to_meta_filter);
    let started = Instant::now();
    let ctx = state
        .engine
        .profile_query(
            tenant,
            &actor_id,
            &container_tags,
            req.q.as_deref().unwrap_or(""),
            limit,
            meta_filter.as_ref(),
        )
        .await?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let search_results = has_query.then(|| {
        // The recall already applied the filter in SQL; no Rust retain on this path.
        let results: Vec<SearchResult> = ctx.relevant.into_iter().map(to_result).collect();
        let total = results.len();
        ProfileSearchResults { results, timing: elapsed_ms, total }
    });
    Ok(Json(ProfileResponse {
        container_tag: tag,
        profile: ProfileBody {
            static_facts: ctx.profile.static_facts,
            dynamic_ctx: ctx.profile.dynamic_ctx,
        },
        search_results,
    }))
}
