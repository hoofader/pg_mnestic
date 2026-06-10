// SPDX-License-Identifier: Apache-2.0

//! memorybench-style evaluation harness for Mnestic: ingest a benchmark's prior
//! conversations into memory, answer its questions from recall, and grade the
//! answers, reporting accuracy alongside recall latency and context size.
//!
//! The orchestration runs on any provider implementing the `Answerer`/`Judge`
//! traits, so it is mock-testable without network. The `real` feature adds the
//! Claude-backed providers and the `memorybench` binary.

pub mod dataset;
pub mod longmemeval;
pub mod mock;
pub mod runner;
pub mod score;

#[cfg(feature = "real")]
pub mod providers;

pub use dataset::{Case, Qa, Turn};
pub use runner::{
    evaluate_cases, ingest_cases, run_eval, Answerer, IngestOutcome, Judge, RunReport,
};
pub use score::{MemScore, QuestionResult};
