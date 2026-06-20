// SPDX-License-Identifier: Apache-2.0

//! Orchestration: the write path that turns raw text into resolved memories.
//! Async model calls (extract, embed) run first; persistence and resolution then
//! run inside one transaction, so a failure rolls back with no partial state.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mnestic_core::{
    decide, Candidate, Ctx, Embedder, ExistingMatch, Extractor, MemType, Ontology, QueryRewriter,
    Reranker, ResolveAction, Scored,
};
use mnestic_store::{NewChunk, NewMemoryFull, PendingSource, Store};
use uuid::Uuid;

mod error;
pub use error::{Error, Result};
pub use mnestic_store::{ChunkHit, Profile, RecallHit};

/// An actor's durable profile plus the memories most relevant to a query. Backs the
/// supermemory `/v4/profile` shape (profile, optionally with query-scoped results).
#[derive(Debug, Default, Clone)]
pub struct ProfileContext {
    pub profile: Profile,
    pub relevant: Vec<RecallHit>,
}

/// What `ingest_document` created. On an idempotent repeat (same `custom_id`),
/// `idempotent_skip` is true and `document_id`/`chunk_ids` are empty.
#[derive(Debug, Default, Clone)]
pub struct DocumentResult {
    pub source_id: Uuid,
    pub document_id: Uuid,
    pub chunk_ids: Vec<Uuid>,
    pub idempotent_skip: bool,
}

/// What `add` did with each extracted candidate.
#[derive(Debug, Default, Clone)]
pub struct AddResult {
    pub source_id: Uuid,
    pub inserted: Vec<Uuid>,
    pub superseded: Vec<Uuid>,
    pub deduped: Vec<Uuid>,
    /// True when this (tenant, custom_id) was already ingested and the pipeline was
    /// skipped. The id fields are empty in that case.
    pub idempotent_skip: bool,
}

/// What `enqueue` did. `queued` is false on an idempotent repeat (same custom_id already
/// ingested or enqueued), in which case `source_id` is the prior source.
#[derive(Debug, Default, Clone)]
pub struct EnqueueResult {
    pub source_id: Uuid,
    pub queued: bool,
}

/// Confidence added each time an identical fact recurs.
const DEDUP_CONFIDENCE_BUMP: f32 = 0.1;

/// Bound on retries when a concurrent writer trips the single-valued EXCLUDE or a
/// serialization failure. Each retry re-resolves against the now-current state.
const MAX_CONFLICT_RETRIES: u32 = 3;

/// A fact is "static" (durable profile material) when it is flagged static or its
/// confidence clears this bar. Caps bound how much the profile holds.
const STATIC_CONFIDENCE: f32 = 0.8;
const STATIC_FACTS_CAP: i64 = 50;
const DYNAMIC_CTX_CAP: i64 = 20;

/// Candidate pool pulled before reranking, when a reranker is configured. The
/// reranker reorders this pool and the top `limit` are returned. RECALL_SQL also
/// clamps each signal's fan-out to at least 50, so the two agree for `limit <= 50`.
const RERANK_POOL: i64 = 50;

/// Document chunking defaults: window size and overlap in characters. ~1000 chars is
/// a few sentences, enough context per chunk for retrieval; the overlap keeps a
/// passage that straddles a boundary recoverable from the neighbouring chunk.
const DOC_CHUNK_CHARS: usize = 1000;
const DOC_CHUNK_OVERLAP: usize = 100;

pub struct Engine {
    store: Store,
    embedder: Arc<dyn Embedder>,
    extractor: Arc<dyn Extractor>,
    ontology: Ontology,
    reranker: Option<Arc<dyn Reranker>>,
    rewriter: Option<Arc<dyn QueryRewriter>>,
}

impl Engine {
    pub fn new(store: Store, embedder: Arc<dyn Embedder>, extractor: Arc<dyn Extractor>) -> Self {
        Self {
            store,
            embedder,
            extractor,
            ontology: Ontology::starter(),
            reranker: None,
            rewriter: None,
        }
    }

    /// Replace the attribute ontology (the synonym map used to canonicalize keys).
    pub fn with_ontology(mut self, ontology: Ontology) -> Self {
        self.ontology = ontology;
        self
    }

    /// Add a reranker. Recall then pulls a larger candidate pool, reranks it against
    /// the user's original query, and returns the top `limit`.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// Add a query rewriter applied to the query before retrieval (embedding +
    /// lexical). Reranking still scores against the original query. Expansion trades
    /// lexical precision for recall (more tokens match more rows) and the expanded
    /// text is what gets embedded, so it is meant to pair with a reranker that
    /// repairs the precision loss.
    pub fn with_rewriter(mut self, rewriter: Arc<dyn QueryRewriter>) -> Self {
        self.rewriter = Some(rewriter);
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Recall the actor's most relevant memories for a query, across all containers.
    /// Thin wrapper over `recall_scoped` for the common unfiltered case.
    pub async fn recall(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<RecallHit>> {
        self.recall_scoped(tenant_id, actor_id, &[], query, limit).await
    }

    /// Recall the actor's most relevant memories for a query, restricted to memories
    /// carrying all of `container_tags` (an empty slice imposes no filter). When a
    /// rewriter is set, the query is expanded for retrieval (embedding + lexical).
    /// Hybrid retrieval then pulls a candidate pool, and when a reranker is set the
    /// pool is reranked against the user's original query before the top `limit` are
    /// returned.
    ///
    /// The signature is provisional: it exposes no `search_mode`/`threshold` and
    /// returns the store row type. The document/chunk path is a later increment.
    pub async fn recall_scoped(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        query: &str,
        limit: i64,
    ) -> Result<Vec<RecallHit>> {
        let retrieval_query = match &self.rewriter {
            Some(r) => r.rewrite(query).await?,
            None => query.to_string(),
        };

        let mut embeddings = self.embedder.embed(std::slice::from_ref(&retrieval_query)).await?;
        let qvec = embeddings.pop().unwrap_or_default();
        check_embedding_dim(&qvec)?;

        // Pull a larger pool when reranking, so the reranker has candidates to reorder.
        let pool = if self.reranker.is_some() {
            limit.max(RERANK_POOL)
        } else {
            limit
        };
        // as_of is None: live recall decays toward now(). The store carries the as-of
        // capability for time-travel reads; no caller threads a past instant yet.
        let mut hits = self
            .store
            .recall_memories(mnestic_store::RecallParams {
                tenant_id,
                actor_id,
                query_embedding: &qvec,
                query_text: &retrieval_query,
                container_tags,
                limit: pool,
                as_of: None,
            })
            .await?;

        if let Some(reranker) = &self.reranker {
            // Rerank against the original query (relevance to what the user asked, not
            // the expanded retrieval query). A None-content (encrypted) row is fed an
            // empty string and so sorts low; accepted for now, since the reranker has
            // no text to score it on.
            let texts: Vec<String> = hits.iter().map(|h| h.content.clone().unwrap_or_default()).collect();
            let scored: Vec<Scored> = reranker.rerank(query, &texts).await?;
            let mut seen = std::collections::HashSet::new();
            let mut reordered = Vec::with_capacity(hits.len());
            for s in &scored {
                if s.index < hits.len() && seen.insert(s.index) {
                    reordered.push(hits[s.index].clone());
                }
            }
            // Keep candidates the reranker omitted (a top-k reranker) after the
            // reranked ones, in their original order, so reranking never shrinks the
            // result below `limit`.
            for (i, hit) in hits.iter().enumerate() {
                if !seen.contains(&i) {
                    reordered.push(hit.clone());
                }
            }
            hits = reordered;
        }

        hits.truncate(limit.max(0) as usize);
        Ok(hits)
    }

    /// Ingest raw text: extract candidate memories, embed them, then resolve and
    /// persist them against the actor's current memories in one transaction.
    ///
    /// Extraction and embedding run once, outside the transaction. The persist step
    /// retries on a transient conflict, re-reading current state each time, so two
    /// concurrent writers for the same fact converge instead of surfacing a raw
    /// exclusion violation.
    pub async fn add(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        content: &str,
        kind: &str,
        custom_id: Option<&str>,
    ) -> Result<AddResult> {
        self.add_at(tenant_id, actor_id, container_tags, content, kind, custom_id, None)
            .await
    }

    /// Like `add`, but `as_of` sets the default `valid_from` for extracted facts
    /// whose extraction did not pin a time. Use it to ingest dated history (a
    /// benchmark session timestamp, a backfilled log) so supersession and as-of
    /// queries order by event time rather than ingest time.
    ///
    /// Precedence is extraction-time, then `as_of`, then write time. Today the
    /// extractors always emit `Temporal::None`, so `as_of` wins; once temporal
    /// extraction lands, revisit whether a trusted `as_of` should override a
    /// model-guessed time for callers that pass a known-good timestamp.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_at(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        content: &str,
        kind: &str,
        custom_id: Option<&str>,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<AddResult> {
        let ctx = Ctx {
            actor_id: actor_id.to_string(),
            container_tags: container_tags.to_vec(),
        };
        let (candidates, embeddings) = self.extract_and_embed(content, &ctx).await?;

        let req = WriteRequest {
            tenant_id,
            actor_id,
            container_tags,
            content,
            kind,
            custom_id,
            as_of,
            candidates: &candidates,
            embeddings: &embeddings,
        };

        for attempt in 0..MAX_CONFLICT_RETRIES {
            match self.persist(&req).await {
                Ok(result) => return Ok(result),
                Err(e) if e.is_transient_conflict() && attempt + 1 < MAX_CONFLICT_RETRIES => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::ConflictRetriesExhausted(MAX_CONFLICT_RETRIES))
    }

    /// One atomic attempt at the write: insert the source, then resolve and persist
    /// each candidate. The transaction rolls back on any error, including a conflict
    /// that `add` will retry.
    async fn persist(&self, req: &WriteRequest<'_>) -> Result<AddResult> {
        let mut tx = self.store.begin_tenant(req.tenant_id).await?;

        let source_id = match Store::insert_source_tx(
            &mut tx,
            req.tenant_id,
            req.actor_id,
            req.container_tags,
            req.kind,
            req.content,
            req.custom_id,
            false,
        )
        .await?
        {
            Some(id) => id,
            None => {
                // This (tenant, custom_id) was already ingested. Skip the pipeline so
                // a retry does not duplicate memories, and return the prior source.
                let existing =
                    Store::source_id_by_custom_id_tx(&mut tx, req.tenant_id, req.custom_id.unwrap_or(""))
                        .await?
                        .unwrap_or_default();
                tx.commit().await?;
                return Ok(AddResult {
                    source_id: existing,
                    idempotent_skip: true,
                    ..Default::default()
                });
            }
        };

        let result = self.persist_candidates(&mut tx, req, source_id).await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Resolve and persist each candidate against `source_id`, then refresh the actor's
    /// profile, all on the caller's transaction (the caller commits). Shared by the sync
    /// write and the async worker, so the worker can mark the source extracted in the same
    /// commit that writes its memories.
    async fn persist_candidates(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        req: &WriteRequest<'_>,
        source_id: Uuid,
    ) -> Result<AddResult> {
        let mut result = AddResult {
            source_id,
            ..Default::default()
        };

        for (cand, embedding) in req.candidates.iter().zip(req.embeddings) {
            check_embedding_dim(embedding)?;

            // Collapse subject/attribute surface forms to canonical keys, so variants
            // like "lives in" and "current city" resolve against the same prior. A key
            // that normalizes to empty (punctuation only) is dropped to None so it is
            // not stored as an empty-string triple that would bypass the single-valued
            // guard and collide with other empty keys.
            let subject = cand
                .subject
                .as_deref()
                .map(|s| self.ontology.normalize_subject(s))
                .filter(|s| !s.is_empty());
            let attribute = cand
                .attribute
                .as_deref()
                .map(|a| self.ontology.canonical_attribute(a))
                .filter(|a| !a.is_empty());
            // A row is single-valued only when it actually has a structured key.
            let single_valued = cand.single_valued && subject.is_some() && attribute.is_some();

            // Only structured facts can match a prior by key; unstructured content is
            // always inserted (semantic dedup is a later phase).
            let matches = match (&subject, &attribute) {
                (Some(s), Some(a)) => {
                    Store::latest_matches_tx(&mut *tx, req.tenant_id, req.actor_id, s, a).await?
                }
                _ => Vec::new(),
            };

            let write = MemoryWrite {
                tenant_id: req.tenant_id,
                actor_id: req.actor_id,
                container_tags: req.container_tags,
                source_id,
                embedding,
                subject,
                attribute,
                single_valued,
                as_of: req.as_of,
                cand,
            };

            match decide(cand, &matches) {
                ResolveAction::Dedup { id } => {
                    let id = parse_id(&id)?;
                    Store::bump_confidence_tx(&mut *tx, req.tenant_id, id, DEDUP_CONFIDENCE_BUMP).await?;
                    result.deduped.push(id);
                }
                ResolveAction::Supersede { prior_ids } => {
                    apply_supersede(&mut *tx, &write, &matches, &prior_ids, &mut result).await?;
                }
                ResolveAction::Insert => {
                    let (vf, vu) = write.interval();
                    let id = write.insert(&mut *tx, true, None, vf, vu).await?;
                    result.inserted.push(id);
                }
            }
        }

        // Refresh the actor's profile from the now-current memories, in the same
        // transaction so the cached profile never lags a committed write.
        Store::refresh_profile_tx(
            &mut *tx,
            req.tenant_id,
            req.actor_id,
            STATIC_CONFIDENCE,
            STATIC_FACTS_CAP,
            DYNAMIC_CTX_CAP,
        )
        .await?;

        Ok(result)
    }

    /// Extract candidates from raw content and embed their text. Empty-content candidates are
    /// dropped: they carry no memory and some embedders reject an empty string in a batch,
    /// which would abort the whole ingest. Shared by the sync write and the async worker.
    async fn extract_and_embed(
        &self,
        content: &str,
        ctx: &Ctx,
    ) -> Result<(Vec<Candidate>, Vec<Vec<f32>>)> {
        let mut candidates = self.extractor.extract(content, ctx).await?;
        candidates.retain(|c| !c.content.trim().is_empty());
        let texts: Vec<String> = candidates.iter().map(|c| c.content.clone()).collect();
        let embeddings = if texts.is_empty() {
            Vec::new()
        } else {
            self.embedder.embed(&texts).await?
        };
        if embeddings.len() != candidates.len() {
            return Err(Error::EmbeddingCountMismatch {
                expected: candidates.len(),
                got: embeddings.len(),
            });
        }
        Ok((candidates, embeddings))
    }

    /// Enqueue raw content for out-of-band extraction (`dreaming: dynamic`): persist the
    /// source and return without running the model. A worker later extracts it via
    /// `process_pending`. `queued` is false when this (tenant, custom_id) was already
    /// ingested or enqueued, matching the sync path's idempotent skip.
    pub async fn enqueue(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        content: &str,
        kind: &str,
        custom_id: Option<&str>,
    ) -> Result<EnqueueResult> {
        match self
            .store
            .enqueue_source(tenant_id, actor_id, container_tags, kind, content, custom_id)
            .await?
        {
            Some(source_id) => Ok(EnqueueResult {
                source_id,
                queued: true,
            }),
            None => {
                let mut tx = self.store.begin_tenant(tenant_id).await?;
                let existing =
                    Store::source_id_by_custom_id_tx(&mut tx, tenant_id, custom_id.unwrap_or(""))
                        .await?
                        .unwrap_or_default();
                tx.commit().await?;
                Ok(EnqueueResult {
                    source_id: existing,
                    queued: false,
                })
            }
        }
    }

    /// Process up to `max` pending sources for one tenant. Each is leased for `lease_secs`
    /// (so a crashed worker's claim is reclaimable), extracted, and persisted; the flag is
    /// cleared in the same commit as the memories. Returns how many committed. A source whose
    /// processing errors is logged and left enqueued: it stays leased for the rest of this
    /// batch (so a poison source can't head-of-line the queue) and is retried once its lease
    /// lapses. A claim failure aborts the batch, since it means the database is unreachable.
    pub async fn process_pending(
        &self,
        tenant_id: Uuid,
        lease_secs: i64,
        max: usize,
    ) -> Result<usize> {
        // A non-positive lease would make a just-claimed source immediately reclaimable, so a
        // pair of workers could both grab it; keep at least a second of exclusivity.
        let lease_secs = lease_secs.max(1);
        let mut processed = 0;
        for _ in 0..max {
            let Some(pending) = self.store.claim_pending_source(tenant_id, lease_secs).await? else {
                break;
            };
            match self.process_claimed(tenant_id, &pending).await {
                Ok(()) => processed += 1,
                Err(e) => tracing::warn!(
                    source_id = %pending.id,
                    error = %e,
                    "extraction failed; source left for retry after its lease lapses"
                ),
            }
        }
        Ok(processed)
    }

    async fn process_claimed(&self, tenant_id: Uuid, pending: &PendingSource) -> Result<()> {
        let ctx = Ctx {
            actor_id: pending.actor_id.clone(),
            container_tags: pending.container_tags.clone(),
        };
        let (candidates, embeddings) = self.extract_and_embed(&pending.content, &ctx).await?;
        // kind is read only by source insertion, which already ran at enqueue, so it is inert
        // here.
        let req = WriteRequest {
            tenant_id,
            actor_id: &pending.actor_id,
            container_tags: &pending.container_tags,
            content: &pending.content,
            kind: "conversation",
            custom_id: None,
            as_of: None,
            candidates: &candidates,
            embeddings: &embeddings,
        };
        for attempt in 0..MAX_CONFLICT_RETRIES {
            // Ok(_) covers both "we committed" and "we lost the claim and rolled back": either
            // way this source is handled (by us or the reclaiming worker), so do not retry.
            match self.persist_into_source(&req, pending.id, pending.claimed_at).await {
                Ok(_) => return Ok(()),
                Err(e) if e.is_transient_conflict() && attempt + 1 < MAX_CONFLICT_RETRIES => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::ConflictRetriesExhausted(MAX_CONFLICT_RETRIES))
    }

    /// Persist a claimed source's candidates and clear its extraction flag in one
    /// transaction, so the source is marked done exactly when its memories commit. Returns
    /// false (and rolls back the writes) when the claim was lost mid-extraction: another
    /// worker reclaimed the source after the lease lapsed, so this pass's memories would be
    /// duplicates and are discarded.
    async fn persist_into_source(
        &self,
        req: &WriteRequest<'_>,
        source_id: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut tx = self.store.begin_tenant(req.tenant_id).await?;
        self.persist_candidates(&mut tx, req, source_id).await?;
        let still_ours =
            Store::mark_source_extracted_tx(&mut tx, req.tenant_id, source_id, claimed_at).await?;
        if still_ours {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(still_ours)
    }

    /// Read the actor's cached profile (durable facts plus recent context). Returns
    /// an empty profile if the actor has no memories yet.
    pub async fn profile(&self, tenant_id: Uuid, actor_id: &str) -> Result<Profile> {
        Ok(self.store.get_profile(tenant_id, actor_id).await?.unwrap_or_default())
    }

    /// The actor's profile plus the memories most relevant to `query`. A blank query
    /// returns just the profile (no recall), matching the optional `q` on the
    /// supermemory profile call. `container_tags` and `limit` bound only `relevant`,
    /// not the per-actor `profile`. The recall runs the full pipeline (rewrite, rerank)
    /// when configured; it has no `threshold`/`filters` yet, so a Phase 2 `/v4/profile`
    /// adapter cannot honor those client params.
    pub async fn profile_query(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        query: &str,
        limit: i64,
    ) -> Result<ProfileContext> {
        let profile = self.profile(tenant_id, actor_id).await?;
        let relevant = if query.trim().is_empty() {
            Vec::new()
        } else {
            self.recall_scoped(tenant_id, actor_id, container_tags, query, limit).await?
        };
        Ok(ProfileContext { profile, relevant })
    }

    /// Ingest a document for RAG: chunk it, embed each chunk, and store the document
    /// and its chunks in one transaction. Unlike `add`, the text is not run through
    /// memory extraction; it is reference material retrieved by `search_documents`.
    /// Idempotent on `custom_id`: a repeat returns the existing source with
    /// `idempotent_skip` set and writes no new document or chunks.
    #[allow(clippy::too_many_arguments)]
    pub async fn ingest_document(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        title: Option<&str>,
        uri: Option<&str>,
        content: &str,
        custom_id: Option<&str>,
    ) -> Result<DocumentResult> {
        // Reject an empty document up front: it would store a source and document row
        // with no chunks, searchable by nothing.
        if content.trim().is_empty() {
            return Err(Error::EmptyDocument);
        }
        // Chunk and embed outside the transaction (the embed call is the slow part and
        // must not pin a pooled connection); the writes then run as one atomic unit.
        let chunks = mnestic_core::chunk_text(content, DOC_CHUNK_CHARS, DOC_CHUNK_OVERLAP);
        let embeddings = if chunks.is_empty() {
            Vec::new()
        } else {
            self.embedder.embed(&chunks).await?
        };
        if embeddings.len() != chunks.len() {
            return Err(Error::EmbeddingCountMismatch {
                expected: chunks.len(),
                got: embeddings.len(),
            });
        }

        let mut tx = self.store.begin_tenant(tenant_id).await?;
        // kind="document" anchors provenance and the custom_id idempotency key. Documents
        // are not extraction targets (no needs_extraction), so the text is not also
        // turned into memories; it is reachable only through search_documents.
        let source_id = match Store::insert_source_tx(
            &mut tx,
            tenant_id,
            actor_id,
            container_tags,
            "document",
            content,
            custom_id,
            false,
        )
        .await?
        {
            Some(id) => id,
            None => {
                let existing =
                    Store::source_id_by_custom_id_tx(&mut tx, tenant_id, custom_id.unwrap_or(""))
                        .await?
                        .unwrap_or_default();
                tx.commit().await?;
                return Ok(DocumentResult {
                    source_id: existing,
                    idempotent_skip: true,
                    ..Default::default()
                });
            }
        };

        let document_id = Store::insert_document_tx(
            &mut tx,
            tenant_id,
            actor_id,
            Some(source_id),
            container_tags,
            title,
            uri,
        )
        .await?;

        let mut chunk_ids = Vec::with_capacity(chunks.len());
        for (ord, (chunk, embedding)) in chunks.iter().zip(&embeddings).enumerate() {
            check_embedding_dim(embedding)?;
            let id = Store::insert_chunk_tx(
                &mut tx,
                &NewChunk {
                    tenant_id,
                    actor_id,
                    document_id,
                    container_tags,
                    ord: ord as i32,
                    content: chunk,
                    embedding,
                },
            )
            .await?;
            chunk_ids.push(id);
        }
        tx.commit().await?;

        Ok(DocumentResult {
            source_id,
            document_id,
            chunk_ids,
            idempotent_skip: false,
        })
    }

    /// Search an actor's document chunks for `query`, scoped to `container_tags`. The
    /// query rewriter (when set) expands the query first, matching recall; the memory
    /// reranker is not applied to chunks. Hits are chunk-level (`ChunkHit` carries the
    /// `document_id`); a caller wanting the document title/uri joins `mnestic_document`.
    pub async fn search_documents(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        container_tags: &[String],
        query: &str,
        limit: i64,
    ) -> Result<Vec<ChunkHit>> {
        let retrieval_query = match &self.rewriter {
            Some(r) => r.rewrite(query).await?,
            None => query.to_string(),
        };
        let mut embeddings = self.embedder.embed(std::slice::from_ref(&retrieval_query)).await?;
        let qvec = embeddings.pop().unwrap_or_default();
        check_embedding_dim(&qvec)?;
        Ok(self
            .store
            .search_chunks(tenant_id, actor_id, &qvec, &retrieval_query, container_tags, limit)
            .await?)
    }

    /// Forget the memories an earlier `add` produced under `custom_id`, tombstoning
    /// them so recall and the profile stop returning them. Returns the forgotten ids;
    /// empty when nothing matched (an unknown or already-forgotten `custom_id`, or a
    /// `custom_id` owned by a different actor), so it is idempotent and a caller may
    /// safely re-issue it on a transient conflict (it is not retried internally). The
    /// profile refresh shares the tombstone transaction, so the cached profile never
    /// lags a committed forget.
    ///
    /// `custom_id` stays an idempotency key on the source, so re-`add`ing the same
    /// `custom_id` after a forget is a no-op; re-ingest under a fresh `custom_id`.
    pub async fn forget(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        custom_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<Uuid>> {
        let mut tx = self.store.begin_tenant(tenant_id).await?;
        let ids =
            Store::forget_source_memories_tx(&mut tx, tenant_id, actor_id, custom_id, reason).await?;
        if !ids.is_empty() {
            Store::refresh_profile_tx(
                &mut tx,
                tenant_id,
                actor_id,
                STATIC_CONFIDENCE,
                STATIC_FACTS_CAP,
                DYNAMIC_CTX_CAP,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Forget by content (the supermemory `memory` tool's content-based forget). Extract
    /// the facts the text describes and tombstone the actor's latest active memory for
    /// each resolved (subject, attribute) key. When the content names a value, only the
    /// row carrying that value is removed, so "forget I worked at Acme" cannot delete a
    /// since-updated employer; a keyless mention ("forget where I work") wipes the key's
    /// latest row. Content that yields no structured key matches nothing: forget is
    /// destructive, so it errs toward removing too little. Returns the tombstoned ids;
    /// the profile refresh shares the transaction.
    pub async fn forget_by_content(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        content: &str,
    ) -> Result<Vec<Uuid>> {
        let ctx = Ctx { actor_id: actor_id.to_string(), container_tags: Vec::new() };
        let candidates = self.extractor.extract(content, &ctx).await?;

        let mut tx = self.store.begin_tenant(tenant_id).await?;
        let mut forgotten = Vec::new();
        for cand in &candidates {
            let subject = cand
                .subject
                .as_deref()
                .map(|s| self.ontology.normalize_subject(s))
                .filter(|s| !s.is_empty());
            let attribute = cand
                .attribute
                .as_deref()
                .map(|a| self.ontology.canonical_attribute(a))
                .filter(|a| !a.is_empty());
            let (Some(s), Some(a)) = (subject, attribute) else { continue };
            let want_value = cand.value.as_deref().map(str::trim).filter(|v| !v.is_empty());

            for m in Store::latest_matches_tx(&mut tx, tenant_id, actor_id, &s, &a).await? {
                let value_matches = match (want_value, m.value.as_deref()) {
                    (None, _) => true,
                    (Some(w), Some(stored)) => w.eq_ignore_ascii_case(stored.trim()),
                    (Some(_), None) => false,
                };
                if !value_matches {
                    continue;
                }
                let id = parse_id(&m.id)?;
                if Store::forget_memory_by_id_tx(&mut tx, tenant_id, id, Some("forgotten by content")).await? == 1 {
                    forgotten.push(id);
                }
            }
        }
        if !forgotten.is_empty() {
            Store::refresh_profile_tx(
                &mut tx,
                tenant_id,
                actor_id,
                STATIC_CONFIDENCE,
                STATIC_FACTS_CAP,
                DYNAMIC_CTX_CAP,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(forgotten)
    }
}

/// Apply a single-valued contradiction in event-time order (LLD §5.2). A candidate
/// newer than the current latest supersedes it (the prior is closed at the
/// candidate's start). An older candidate is recorded as history, capped so it does
/// not overlap the next known segment, and the still-current latest is left in place.
async fn apply_supersede(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    write: &MemoryWrite<'_>,
    matches: &[ExistingMatch],
    prior_ids: &[String],
    result: &mut AddResult,
) -> Result<()> {
    let (cand_vf, cand_vu) = write.interval();
    let priors: Vec<&ExistingMatch> = matches.iter().filter(|m| prior_ids.contains(&m.id)).collect();
    let newest_vf = priors.iter().filter_map(|p| p.valid_from).max();
    let cand_is_newer = newest_vf.is_none_or(|nf| cand_vf > nf);

    if cand_is_newer {
        let mut newest_prior: Option<Uuid> = None;
        for p in &priors {
            let pid = parse_id(&p.id)?;
            let closed = Store::close_prior_tx(tx, write.tenant_id, pid, cand_vf).await?;
            if closed != 1 {
                // A prior we meant to close did not close (e.g. its validity start is
                // not before the candidate's). Fail loudly rather than leave two
                // active rows or report a supersession that did not happen.
                return Err(Error::SupersedeFailed(closed));
            }
            result.superseded.push(pid);
            if p.valid_from == newest_vf {
                newest_prior = Some(pid);
            }
        }
        let id = write.insert(tx, true, newest_prior, cand_vf, cand_vu).await?;
        result.inserted.push(id);
    } else {
        // Cap the historical row at the start of the nearest later segment (active or
        // superseded), so it partitions the validity axis instead of overlapping an
        // existing interval. There is always at least the current latest's start.
        let (subject, attribute) = match (&write.subject, &write.attribute) {
            (Some(s), Some(a)) => (s.as_str(), a.as_str()),
            _ => return Ok(()),
        };
        let starts =
            Store::segment_starts_tx(tx, write.tenant_id, write.actor_id, subject, attribute).await?;
        let next_start = starts.into_iter().filter(|s| *s > cand_vf).min();
        let upper = match (cand_vu, next_start) {
            (Some(vu), Some(ns)) => Some(vu.min(ns)),
            (Some(vu), None) => Some(vu),
            (None, ns) => ns,
        };
        // Both `next_start` and a sanitized `cand_vu` are strictly after `cand_vf`,
        // so the interval is non-empty.
        let id = write.insert(tx, false, None, cand_vf, upper).await?;
        result.inserted.push(id);
    }
    Ok(())
}

/// Inputs for one `persist` attempt. Held by reference so retries reuse the
/// already-computed candidates and embeddings.
struct WriteRequest<'a> {
    tenant_id: Uuid,
    actor_id: &'a str,
    container_tags: &'a [String],
    content: &'a str,
    kind: &'a str,
    custom_id: Option<&'a str>,
    as_of: Option<DateTime<Utc>>,
    candidates: &'a [Candidate],
    embeddings: &'a [Vec<f32>],
}

/// Bundles the per-candidate write inputs so the insert call sites stay short.
struct MemoryWrite<'a> {
    tenant_id: Uuid,
    actor_id: &'a str,
    container_tags: &'a [String],
    source_id: Uuid,
    embedding: &'a [f32],
    /// Canonicalized key (post-ontology), stored so resolution and storage agree.
    subject: Option<String>,
    attribute: Option<String>,
    /// Gated on a present canonical key, so an empty/dropped key never stores a
    /// single-valued row without a triple.
    single_valued: bool,
    /// Default validity start when extraction pinned none (e.g. a dated session).
    as_of: Option<DateTime<Utc>>,
    cand: &'a Candidate,
}

impl MemoryWrite<'_> {
    /// The candidate's validity interval, sanitized: an upper bound at or before the
    /// lower bound (a garbled extractor range) is dropped to open-ended, so the
    /// `tstzrange` is never inverted or empty. `valid_from` is the extracted time, or
    /// the caller's `as_of`, or write time, in that order of preference.
    fn interval(&self) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
        let from = self
            .cand
            .temporal
            .valid_from()
            .or(self.as_of)
            .unwrap_or_else(Utc::now);
        let until = self.cand.temporal.valid_until().filter(|u| *u > from);
        (from, until)
    }

    async fn insert(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        is_latest: bool,
        supersedes_id: Option<Uuid>,
        valid_from: DateTime<Utc>,
        valid_until: Option<DateTime<Utc>>,
    ) -> Result<Uuid> {
        let c = self.cand;
        let row = NewMemoryFull {
            tenant_id: self.tenant_id,
            actor_id: self.actor_id,
            container_tags: self.container_tags,
            content: &c.content,
            subject: self.subject.as_deref(),
            attribute: self.attribute.as_deref(),
            value: c.value.as_deref(),
            single_valued: self.single_valued,
            confidence: c.confidence,
            is_static: c.is_static,
            mem_type: mem_type_str(c.mem_type),
            embedding: Some(self.embedding),
            source_id: Some(self.source_id),
            valid_from,
            valid_until,
            is_latest,
            supersedes_id,
            forget_after: c.forget_after,
        };
        Ok(Store::insert_memory_full_tx(tx, &row).await?)
    }
}

fn mem_type_str(t: MemType) -> &'static str {
    match t {
        MemType::Fact => "fact",
        MemType::Preference => "preference",
        MemType::Episode => "episode",
    }
}

fn parse_id(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|_| Error::BadId(s.to_string()))
}

/// Reject an empty or wrong-dimension embedding before it reaches a `::halfvec`
/// cast, where it would otherwise surface as an opaque database error.
fn check_embedding_dim(v: &[f32]) -> Result<()> {
    if v.len() != mnestic_store::EMBEDDING_DIM {
        return Err(Error::EmbeddingDim {
            expected: mnestic_store::EMBEDDING_DIM,
            got: v.len(),
        });
    }
    Ok(())
}
