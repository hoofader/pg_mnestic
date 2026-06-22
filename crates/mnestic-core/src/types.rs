// SPDX-License-Identifier: AGPL-3.0-only

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemType {
    Fact,
    Preference,
    Episode,
}

/// How a candidate's truth interval is expressed by extraction. Drives how
/// `valid_time` is set on write (LLD §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Temporal {
    AsOf {
        timestamp: DateTime<Utc>,
    },
    Range {
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
    },
    None,
}

impl Temporal {
    /// Lower bound of the truth interval, if extraction gave one. The engine
    /// falls back to write time when this is None.
    pub fn valid_from(&self) -> Option<DateTime<Utc>> {
        match self {
            Temporal::AsOf { timestamp } => Some(*timestamp),
            Temporal::Range { from, .. } => *from,
            Temporal::None => None,
        }
    }

    /// Upper bound of the truth interval, if bounded. None means open-ended.
    pub fn valid_until(&self) -> Option<DateTime<Utc>> {
        match self {
            Temporal::Range { to, .. } => *to,
            _ => None,
        }
    }
}

/// A memory proposed by extraction, before resolution against existing rows.
/// Carries `temporal` and `forget_after`; the schema's `document_date`,
/// `event_date`, and `forget_reason` are populated by the Phase 1 extractor, not yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub content: String,
    pub subject: Option<String>,
    pub attribute: Option<String>,
    pub value: Option<String>,
    pub single_valued: bool,
    pub mem_type: MemType,
    pub confidence: f32,
    pub is_static: bool,
    pub temporal: Temporal,
    pub forget_after: Option<DateTime<Utc>>,
}

/// A latest existing row matched during resolution. `decide` reads `value` and
/// `single_valued`; the engine uses `valid_from` to order supersession in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingMatch {
    pub id: String,
    pub value: Option<String>,
    pub single_valued: bool,
    pub valid_from: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveAction {
    Dedup { id: String },
    /// Every active single-valued prior with a different value must be closed, so
    /// the set is returned rather than a single id.
    Supersede { prior_ids: Vec<String> },
    Insert,
}

/// A reranked candidate carrying its final score (LLD §5.4). `index` is the
/// position in the input slice, so callers map a result back to its source row.
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub index: usize,
    pub content: String,
    pub score: f32,
}

/// A graph relation between two memories beyond the supersession chain (which the SDK
/// names `updates`). `Extends` is a memory that adds detail to another; `Derives` is a
/// memory inferred from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relation {
    Extends,
    Derives,
}

impl Relation {
    /// The wire/SQL token. The `relation` column's CHECK and the SDK both use these.
    pub fn as_str(&self) -> &'static str {
        match self {
            Relation::Extends => "extends",
            Relation::Derives => "derives",
        }
    }
}

/// One relation the classifier found, pointing at a candidate by its position in the
/// slice the classifier was given, so the caller maps it back to a memory id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelationEdge {
    pub index: usize,
    pub relation: Relation,
}
