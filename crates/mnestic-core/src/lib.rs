// SPDX-License-Identifier: AGPL-3.0-only

//! Domain types, provider traits, and the pure resolution logic for Mnestic.
//! No DB and no network live here, so the contradiction rules stay testable in
//! isolation.

pub mod chunk;
pub mod error;
pub mod ontology;
pub mod resolve;
pub mod traits;
pub mod types;

pub use chunk::chunk_text;
pub use error::{Error, Result};
pub use ontology::{normalize_key, Ontology};
pub use resolve::decide;
pub use traits::{Ctx, Embedder, Extractor, QueryRewriter, Reranker};
pub use types::{
    Candidate, ExistingMatch, MemType, ResolveAction, Scored, Temporal,
};
