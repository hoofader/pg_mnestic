// SPDX-License-Identifier: Apache-2.0

//! Orchestration: the write path that turns raw text into resolved memories.
//! Async model calls (extract, embed) run first; persistence and resolution then
//! run inside one transaction, so a failure rolls back with no partial state.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mnestic_core::{
    decide, Candidate, Ctx, Embedder, ExistingMatch, Extractor, MemType, Ontology, ResolveAction,
};
use mnestic_store::{NewMemoryFull, Store};
use uuid::Uuid;

mod error;
pub use error::{Error, Result};
pub use mnestic_store::RecallHit;

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

/// Confidence added each time an identical fact recurs.
const DEDUP_CONFIDENCE_BUMP: f32 = 0.1;

/// Bound on retries when a concurrent writer trips the single-valued EXCLUDE or a
/// serialization failure. Each retry re-resolves against the now-current state.
const MAX_CONFLICT_RETRIES: u32 = 3;

pub struct Engine {
    store: Store,
    embedder: Arc<dyn Embedder>,
    extractor: Arc<dyn Extractor>,
    ontology: Ontology,
}

impl Engine {
    pub fn new(store: Store, embedder: Arc<dyn Embedder>, extractor: Arc<dyn Extractor>) -> Self {
        Self {
            store,
            embedder,
            extractor,
            ontology: Ontology::starter(),
        }
    }

    /// Replace the attribute ontology (the synonym map used to canonicalize keys).
    pub fn with_ontology(mut self, ontology: Ontology) -> Self {
        self.ontology = ontology;
        self
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Recall the actor's most relevant memories for a query: embed the query, then
    /// run hybrid (vector + lexical) retrieval ranked by recency and confidence.
    ///
    /// The signature is provisional: it scopes by actor only (no `container_tags`
    /// filter yet, unlike the supermemory `containerTag` model), exposes no
    /// `search_mode`/`threshold`/`rerank`, and returns the store row type. Container
    /// filtering, cross-encoder reranking, and the document/chunk path are later
    /// increments.
    pub async fn recall(
        &self,
        tenant_id: Uuid,
        actor_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<RecallHit>> {
        let mut embeddings = self.embedder.embed(&[query.to_string()]).await?;
        let qvec = embeddings.pop().unwrap_or_default();
        check_embedding_dim(&qvec)?;
        Ok(self
            .store
            .recall_memories(tenant_id, actor_id, &qvec, query, limit)
            .await?)
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
        let ctx = Ctx {
            actor_id: actor_id.to_string(),
            container_tags: container_tags.to_vec(),
        };
        let candidates = self.extractor.extract(content, &ctx).await?;
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

        let req = WriteRequest {
            tenant_id,
            actor_id,
            container_tags,
            content,
            kind,
            custom_id,
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
                    Store::latest_matches_tx(&mut tx, req.tenant_id, req.actor_id, s, a).await?
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
                cand,
            };

            match decide(cand, &matches) {
                ResolveAction::Dedup { id } => {
                    let id = parse_id(&id)?;
                    Store::bump_confidence_tx(&mut tx, req.tenant_id, id, DEDUP_CONFIDENCE_BUMP).await?;
                    result.deduped.push(id);
                }
                ResolveAction::Supersede { prior_ids } => {
                    apply_supersede(&mut tx, &write, &matches, &prior_ids, &mut result).await?;
                }
                ResolveAction::Insert => {
                    let (vf, vu) = write.interval();
                    let id = write.insert(&mut tx, true, None, vf, vu).await?;
                    result.inserted.push(id);
                }
            }
        }

        tx.commit().await?;
        Ok(result)
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
    cand: &'a Candidate,
}

impl MemoryWrite<'_> {
    /// The candidate's validity interval, sanitized: an upper bound at or before the
    /// lower bound (a garbled extractor range) is dropped to open-ended, so the
    /// `tstzrange` is never inverted or empty.
    fn interval(&self) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
        let from = self.cand.temporal.valid_from().unwrap_or_else(Utc::now);
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
