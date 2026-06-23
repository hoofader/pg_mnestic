// SPDX-License-Identifier: MIT

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("model provider error: {0}")]
    Provider(String),

    #[error("extraction produced invalid output: {0}")]
    Extraction(String),

    #[error("serialization error: {0}")]
    Serde(String),
}

pub type Result<T> = std::result::Result<T, Error>;
