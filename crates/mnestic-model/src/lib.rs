// SPDX-License-Identifier: AGPL-3.0-only

//! Provider trait impls. Mock impls are always built and network-free; the
//! OpenAI impls sit behind the `openai` feature.

pub mod mock;

pub use mock::{MockEmbedder, MockExtractor, MockRelationClassifier, MockReranker, MockRewriter};

#[cfg(any(feature = "openai", feature = "anthropic"))]
mod extract_schema;

#[cfg(feature = "anthropic")]
mod classify;

#[cfg(feature = "anthropic")]
pub use classify::AnthropicRelationClassifier;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "openai")]
pub use openai::{OpenAiEmbedder, OpenAiExtractor};

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicExtractor;

#[cfg(feature = "rerank")]
pub mod tei;

#[cfg(feature = "rerank")]
pub use tei::TeiReranker;
