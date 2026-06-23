// SPDX-License-Identifier: MIT

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] mnestic_core::Error),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("stored id is not a uuid: {0}")]
    BadId(String),
    #[error("embedder returned {got} vectors for {expected} candidates")]
    EmbeddingCountMismatch { expected: usize, got: usize },
    #[error("embedding has {got} dimensions, expected {expected}")]
    EmbeddingDim { expected: usize, got: usize },
    #[error("document content is empty")]
    EmptyDocument,
    #[error("expected to close exactly one prior on supersession, closed {0}")]
    SupersedeFailed(u64),
    #[error("write kept conflicting after {0} attempts")]
    ConflictRetriesExhausted(u32),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// True for Postgres errors that a retry of the resolution may clear: the
    /// single-valued EXCLUDE (`23P01`) and serialization failures.
    pub(crate) fn is_transient_conflict(&self) -> bool {
        matches!(
            self,
            Error::Db(sqlx::Error::Database(db))
                if matches!(db.code().as_deref(), Some("23P01") | Some("40001") | Some("40P01"))
        )
    }
}
