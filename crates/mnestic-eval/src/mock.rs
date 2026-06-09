// SPDX-License-Identifier: Apache-2.0

//! Network-free answerer/judge for testing the harness wiring.

use anyhow::Result;
use async_trait::async_trait;

use crate::runner::{Answerer, Judge};

/// Answers by concatenating the recalled context, so a correct memory surfaces in
/// the prediction and the substring judge can grade it.
pub struct EchoAnswerer;

#[async_trait]
impl Answerer for EchoAnswerer {
    async fn answer(&self, _question: &str, context: &[String]) -> Result<String> {
        Ok(context.join(" | "))
    }
}

/// Grades correct when the gold answer appears in the prediction (case-insensitive).
pub struct SubstringJudge;

#[async_trait]
impl Judge for SubstringJudge {
    async fn judge(&self, _question: &str, gold: &str, predicted: &str) -> Result<bool> {
        Ok(predicted.to_lowercase().contains(&gold.to_lowercase()))
    }
}
