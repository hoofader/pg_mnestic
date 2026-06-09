// SPDX-License-Identifier: Apache-2.0

//! Domain types, provider traits, and the pure resolution logic for Mnestic.
//! No DB and no network live here, so the contradiction rules stay testable in
//! isolation.

pub mod error;
pub mod resolve;
pub mod traits;
pub mod types;

pub use error::{Error, Result};
pub use resolve::decide;
pub use traits::{Ctx, Embedder, Extractor, Reranker};
pub use types::{
    Candidate, ExistingMatch, MemType, ResolveAction, Scored, Temporal,
};
