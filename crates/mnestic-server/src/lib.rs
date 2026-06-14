// SPDX-License-Identifier: Apache-2.0

//! The supermemory-compatible REST/MCP shim (LLD doc 04). It maps supermemory's wire
//! contract onto the Mnestic engine so the existing shells drive Mnestic unchanged.
//! This module is the scoping mapping; the axum surface and auth land in later
//! increments.

pub mod container_tag;

pub use container_tag::{parse_container_tag, reconstruct_container_tag, Scope};
