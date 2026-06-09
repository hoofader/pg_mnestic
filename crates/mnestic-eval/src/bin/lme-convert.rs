// SPDX-License-Identifier: Apache-2.0

//! Convert a LongMemEval JSON file into Mnestic's normalized dataset, on stdout.
//! No keys or network. Usage: `lme-convert longmemeval_s.json > dataset.json`.

use anyhow::{Context, Result};

use mnestic_eval::longmemeval;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: lme-convert <longmemeval.json> > dataset.json")?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let (cases, skipped) = longmemeval::convert(&raw)?;
    eprintln!(
        "converted {} cases ({} abstention questions skipped)",
        cases.len(),
        skipped
    );
    println!("{}", serde_json::to_string_pretty(&cases)?);
    Ok(())
}
