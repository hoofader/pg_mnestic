// SPDX-License-Identifier: Apache-2.0

//! Provider trait impls. Mock impls are always built and network-free; the
//! OpenAI impls sit behind the `openai` feature.

pub mod mock;

pub use mock::{MockEmbedder, MockExtractor, MockReranker};

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "openai")]
pub use openai::{OpenAiEmbedder, OpenAiExtractor};
