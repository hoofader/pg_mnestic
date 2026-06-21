// SPDX-License-Identifier: AGPL-3.0-only

//! Convert a LongMemEval JSON file into Mnestic's normalized dataset, on stdout.
//! No keys or network. Usage: `lme-convert longmemeval_s.json > dataset.json`.

use anyhow::{Context, Result};

use mnestic_eval::longmemeval;

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: lme-convert <longmemeval.json> > dataset.json")?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    let cases = longmemeval::convert(&raw)?;
    let abstention = cases
        .iter()
        .filter(|c| c.questions.iter().any(|q| q.abstention))
        .count();
    eprintln!("converted {} cases ({} abstention)", cases.len(), abstention);
    println!("{}", serde_json::to_string_pretty(&cases)?);
    Ok(())
}
