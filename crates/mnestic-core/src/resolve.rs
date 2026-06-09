// SPDX-License-Identifier: Apache-2.0

use crate::types::{Candidate, ExistingMatch, ResolveAction};

/// Decide what to do with a candidate given the latest existing matches (LLD §5.2).
///
/// Kept pure (no DB, no async) so the contradiction rules are unit-testable in
/// isolation from the bitemporal `valid_time` splitting the store performs.
pub fn decide(candidate: &Candidate, matches: &[ExistingMatch]) -> ResolveAction {
    let cand_value = candidate.value.as_deref();

    // Exact duplicate value -> dedup, regardless of cardinality.
    if let Some(cv) = cand_value {
        if let Some(m) = matches.iter().find(|m| m.value.as_deref() == Some(cv)) {
            return ResolveAction::Dedup { id: m.id.clone() };
        }
    }

    // Single-valued contradiction: every latest match with a different value must
    // be superseded so two active single-valued facts never coexist.
    if candidate.single_valued {
        let prior_ids: Vec<String> = matches
            .iter()
            .filter(|m| m.value.as_deref() != cand_value)
            .map(|m| m.id.clone())
            .collect();
        if !prior_ids.is_empty() {
            return ResolveAction::Supersede { prior_ids };
        }
    }

    // Multi-valued additive, or brand-new.
    ResolveAction::Insert
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemType, Temporal};

    fn candidate(value: &str, single_valued: bool) -> Candidate {
        Candidate {
            content: format!("user value is {value}"),
            subject: Some("user".into()),
            attribute: Some("location".into()),
            value: Some(value.into()),
            single_valued,
            mem_type: MemType::Fact,
            confidence: 0.9,
            is_static: false,
            temporal: Temporal::None,
            forget_after: None,
        }
    }

    fn existing(id: &str, value: &str, single_valued: bool) -> ExistingMatch {
        ExistingMatch {
            id: id.into(),
            value: Some(value.into()),
            single_valued,
        }
    }

    #[test]
    fn exact_duplicate_dedups() {
        let c = candidate("NYC", true);
        let matches = vec![existing("m1", "NYC", true)];
        assert_eq!(decide(&c, &matches), ResolveAction::Dedup { id: "m1".into() });
    }

    #[test]
    fn single_valued_different_value_supersedes() {
        let c = candidate("SF", true);
        let matches = vec![existing("m1", "NYC", true)];
        assert_eq!(
            decide(&c, &matches),
            ResolveAction::Supersede { prior_ids: vec!["m1".into()] }
        );
    }

    #[test]
    fn single_valued_supersedes_all_differing_priors() {
        // A degenerate state with more than one active single-valued prior must
        // close every differing prior, not just the first.
        let c = candidate("SF", true);
        let matches = vec![existing("m1", "NYC", true), existing("m2", "LA", true)];
        assert_eq!(
            decide(&c, &matches),
            ResolveAction::Supersede { prior_ids: vec!["m1".into(), "m2".into()] }
        );
    }

    #[test]
    fn multi_valued_coexist_inserts() {
        let c = candidate("French", false);
        let matches = vec![existing("m1", "English", false)];
        assert_eq!(decide(&c, &matches), ResolveAction::Insert);
    }

    #[test]
    fn brand_new_inserts() {
        let c = candidate("NYC", true);
        assert_eq!(decide(&c, &[]), ResolveAction::Insert);
    }
}
