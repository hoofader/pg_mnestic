// SPDX-License-Identifier: AGPL-3.0-only

//! The supermemory `filters` metadata-filter DSL (sdk-ts `SearchMemoriesParams.filters`).
//! A recursive `OR`/`AND` tree of leaf predicates, evaluated in pure Rust against the
//! `metadata` JSON each hit already carries. We never build dynamic SQL from caller input:
//! the tree drives a Rust predicate over an over-fetched candidate pool, which keeps the
//! injection surface at zero and the SQL static.

use mnestic_engine::{MetaFilter, MetaKind, MetaLeaf, MetaOp};
use serde::Deserialize;

/// One node of the filter tree. Untagged so the three SDK shapes deserialize from the same
/// JSON: a combiner under the `OR`/`AND` key, or a bare leaf object. Combiner variants come
/// first so serde tries them before the leaf; a leaf has no `OR`/`AND` key, so it can never
/// be mistaken for a combiner, and a combiner object lacks the leaf's required `key`/`value`,
/// so it can never be mistaken for a leaf.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum FilterNode {
    Or {
        #[serde(rename = "OR")]
        or: Vec<FilterNode>,
    },
    And {
        #[serde(rename = "AND")]
        and: Vec<FilterNode>,
    },
    Leaf(Leaf),
}

/// A leaf predicate over one metadata key. `filter_type` selects the comparison; the default
/// (and `"metadata"`) is string equality.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leaf {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub filter_type: Option<String>,
    #[serde(default)]
    pub numeric_operator: Option<String>,
    #[serde(default, deserialize_with = "de_loose_bool")]
    pub negate: Option<bool>,
    #[serde(default, deserialize_with = "de_loose_bool")]
    pub ignore_case: Option<bool>,
}

/// The SDK types `negate`/`ignoreCase` as `boolean | "true" | "false"`, so accept either a JSON
/// boolean or those two strings; any other value (or null) reads as unset.
fn de_loose_bool<'de, D>(d: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrStr {
        Bool(bool),
        Str(String),
    }
    Ok(match Option::<BoolOrStr>::deserialize(d)? {
        Some(BoolOrStr::Bool(b)) => Some(b),
        Some(BoolOrStr::Str(s)) if s == "true" => Some(true),
        Some(BoolOrStr::Str(s)) if s == "false" => Some(false),
        _ => None,
    })
}

/// A malicious or buggy caller could nest combiners arbitrarily deep; cap recursion so the
/// evaluation cannot overflow the stack. Beyond the cap a node is treated as non-matching.
const MAX_DEPTH: usize = 16;

/// Whether `metadata` satisfies the filter tree. Entry point for callers; depth starts at 0.
pub fn matches(node: &FilterNode, metadata: &serde_json::Value) -> bool {
    matches_at(node, metadata, 0)
}

/// Convert the wire filter tree into the store's `MetaFilter`, so the memory recall path can push
/// it into SQL instead of retaining in Rust. The store's `to_sql` is built to mirror `matches()`,
/// so the two paths agree. `filterType` maps to `MetaKind` (unknown/absent -> Equality, matching
/// the `matches_leaf` fallback); `numericOperator` maps to `MetaOp` (unknown/absent -> None).
pub fn to_meta_filter(node: &FilterNode) -> MetaFilter {
    match node {
        FilterNode::Or { or } => MetaFilter::Or(or.iter().map(to_meta_filter).collect()),
        FilterNode::And { and } => MetaFilter::And(and.iter().map(to_meta_filter).collect()),
        FilterNode::Leaf(leaf) => MetaFilter::Leaf(MetaLeaf {
            key: leaf.key.clone(),
            value: leaf.value.clone(),
            kind: match leaf.filter_type.as_deref() {
                Some("numeric") => MetaKind::Numeric,
                Some("array_contains") => MetaKind::ArrayContains,
                Some("string_contains") => MetaKind::StringContains,
                // "metadata" and any unknown/absent type are string equality, like matches_leaf.
                _ => MetaKind::Equality,
            },
            op: match leaf.numeric_operator.as_deref() {
                Some(">") => Some(MetaOp::Gt),
                Some("<") => Some(MetaOp::Lt),
                Some(">=") => Some(MetaOp::Ge),
                Some("<=") => Some(MetaOp::Le),
                Some("=") => Some(MetaOp::Eq),
                _ => None,
            },
            negate: leaf.negate.unwrap_or(false),
            ignore_case: leaf.ignore_case.unwrap_or(false),
        }),
    }
}

fn matches_at(node: &FilterNode, metadata: &serde_json::Value, depth: usize) -> bool {
    if depth >= MAX_DEPTH {
        return false;
    }
    match node {
        // An empty combiner imposes no constraint: empty OR/AND both match. This mirrors the
        // "no filter" reading, so an empty array does not silently drop every hit. `.all()` is
        // already vacuously true on empty; `.any()` is not, so empty OR is special-cased.
        FilterNode::Or { or } => or.is_empty() || or.iter().any(|child| matches_at(child, metadata, depth + 1)),
        FilterNode::And { and } => and.iter().all(|child| matches_at(child, metadata, depth + 1)),
        FilterNode::Leaf(leaf) => matches_leaf(leaf, metadata),
    }
}

fn matches_leaf(leaf: &Leaf, metadata: &serde_json::Value) -> bool {
    let negate = leaf.negate.unwrap_or(false);
    let ignore_case = leaf.ignore_case.unwrap_or(false);
    // A missing key is a non-match before negate, so a negated filter on an absent key matches
    // (the key is not equal to the value because it is not present at all).
    let raw = match metadata.get(&leaf.key) {
        Some(v) => v,
        None => return negate,
    };
    let result = match leaf.filter_type.as_deref().unwrap_or("metadata") {
        "numeric" => eval_numeric(raw, &leaf.value, leaf.numeric_operator.as_deref()),
        "array_contains" => eval_array_contains(raw, &leaf.value, ignore_case),
        "string_contains" => eval_string_contains(raw, &leaf.value, ignore_case),
        // "metadata" and any unknown type fall back to string equality.
        _ => eval_equality(raw, &leaf.value, ignore_case),
    };
    result ^ negate
}

/// String equality against the metadata value rendered as a string. Only JSON strings, numbers,
/// and booleans render; an object/array/null at the key never equals a scalar filter value.
fn eval_equality(raw: &serde_json::Value, value: &str, ignore_case: bool) -> bool {
    match json_scalar_string(raw) {
        Some(s) => str_eq(&s, value, ignore_case),
        None => false,
    }
}

/// Numeric comparison: both sides parsed as a FINITE `f64`. A non-numeric or non-finite metadata
/// value or filter value, or an unknown operator, is a non-match (never a panic). Requiring finite
/// keeps this in step with the SQL path, which rejects inf/nan (numeric has no infinity).
fn eval_numeric(raw: &serde_json::Value, value: &str, op: Option<&str>) -> bool {
    let lhs = match json_as_f64(raw) {
        Some(n) if n.is_finite() => n,
        _ => return false,
    };
    let rhs = match value.trim().parse::<f64>() {
        Ok(n) if n.is_finite() => n,
        _ => return false,
    };
    match op {
        Some(">") => lhs > rhs,
        Some("<") => lhs < rhs,
        Some(">=") => lhs >= rhs,
        Some("<=") => lhs <= rhs,
        Some("=") => lhs == rhs,
        // A missing or unrecognized operator has no defined comparison.
        _ => false,
    }
}

/// The value at the key is a JSON array containing the filter string. Non-string array elements
/// are compared via their scalar rendering, so `["7"]` and `[7]` both contain `"7"`.
fn eval_array_contains(raw: &serde_json::Value, value: &str, ignore_case: bool) -> bool {
    match raw.as_array() {
        Some(items) => items
            .iter()
            .filter_map(json_scalar_string)
            .any(|s| str_eq(&s, value, ignore_case)),
        None => false,
    }
}

/// The value at the key, as a string, contains the filter substring.
fn eval_string_contains(raw: &serde_json::Value, value: &str, ignore_case: bool) -> bool {
    match json_scalar_string(raw) {
        Some(s) if ignore_case => s.to_lowercase().contains(&value.to_lowercase()),
        Some(s) => s.contains(value),
        None => false,
    }
}

/// Render a JSON scalar (string, number, bool) to the string a caller filters against. An
/// object, array, or null has no scalar form here and yields `None`.
fn json_scalar_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A JSON value as `f64`: a JSON number directly, or a numeric string (the SDK lets callers send
/// numbers as strings in metadata).
fn json_as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn str_eq(a: &str, b: &str, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(j: serde_json::Value) -> FilterNode {
        serde_json::from_value(j).expect("filter parses")
    }

    #[test]
    fn equality_match_and_miss() {
        let node = parse(json!({"key": "team", "value": "infra"}));
        assert!(matches(&node, &json!({"team": "infra"})));
        assert!(!matches(&node, &json!({"team": "sales"})));
        // A missing key is a non-match (before negate).
        assert!(!matches(&node, &json!({"other": "infra"})));
    }

    #[test]
    fn ignore_case_equality() {
        let node = parse(json!({"key": "team", "value": "Infra", "ignoreCase": true}));
        assert!(matches(&node, &json!({"team": "infra"})));
        let strict = parse(json!({"key": "team", "value": "Infra"}));
        assert!(!matches(&strict, &json!({"team": "infra"})));
    }

    #[test]
    fn negate_inverts_and_missing_key_negated_matches() {
        let node = parse(json!({"key": "team", "value": "infra", "negate": true}));
        // Present-and-equal inverts to a non-match.
        assert!(!matches(&node, &json!({"team": "infra"})));
        // Present-and-different inverts to a match.
        assert!(matches(&node, &json!({"team": "sales"})));
        // Missing key: not-present negated is a match.
        assert!(matches(&node, &json!({"other": "x"})));
    }

    #[test]
    fn numeric_each_operator() {
        let gt = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": ">"}));
        assert!(matches(&gt, &json!({"n": 6})));
        assert!(!matches(&gt, &json!({"n": 5})));
        let lt = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": "<"}));
        assert!(matches(&lt, &json!({"n": 4})));
        let ge = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": ">="}));
        assert!(matches(&ge, &json!({"n": 5})));
        let le = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": "<="}));
        assert!(matches(&le, &json!({"n": 5})));
        let eq = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": "="}));
        assert!(matches(&eq, &json!({"n": 5.0})));
        // A numeric value stored as a string still parses.
        assert!(matches(&eq, &json!({"n": "5"})));
        // Non-numeric metadata, and an unknown operator, are non-matches, not panics.
        assert!(!matches(&eq, &json!({"n": "not a number"})));
        let bad_op = parse(json!({"key": "n", "value": "5", "filterType": "numeric", "numericOperator": "~"}));
        assert!(!matches(&bad_op, &json!({"n": 5})));
        // A numeric filter with no operator at all has no defined comparison.
        let no_op = parse(json!({"key": "n", "value": "5", "filterType": "numeric"}));
        assert!(!matches(&no_op, &json!({"n": 5})));
    }

    #[test]
    fn array_contains() {
        let node = parse(json!({"key": "tags", "value": "rust", "filterType": "array_contains"}));
        assert!(matches(&node, &json!({"tags": ["rust", "go"]})));
        assert!(!matches(&node, &json!({"tags": ["python", "go"]})));
        // A non-array value at the key never "contains".
        assert!(!matches(&node, &json!({"tags": "rust"})));
        // Numeric element rendered to a string still matches a string filter.
        let num = parse(json!({"key": "ids", "value": "7", "filterType": "array_contains"}));
        assert!(matches(&num, &json!({"ids": [7, 8]})));
    }

    #[test]
    fn string_contains() {
        let node = parse(json!({"key": "note", "value": "power", "filterType": "string_contains"}));
        assert!(matches(&node, &json!({"note": "the powerhouse note"})));
        assert!(!matches(&node, &json!({"note": "unrelated"})));
        let ci = parse(json!({"key": "note", "value": "POWER", "filterType": "string_contains", "ignoreCase": true}));
        assert!(matches(&ci, &json!({"note": "the powerhouse"})));
    }

    #[test]
    fn nested_and_within_or() {
        // Match when team is infra, OR (env is prod AND tier is gold).
        let node = parse(json!({
            "OR": [
                {"key": "team", "value": "infra"},
                {"AND": [
                    {"key": "env", "value": "prod"},
                    {"key": "tier", "value": "gold"}
                ]}
            ]
        }));
        assert!(matches(&node, &json!({"team": "infra"})));
        assert!(matches(&node, &json!({"env": "prod", "tier": "gold"})));
        // The AND branch needs both keys; one alone does not satisfy it.
        assert!(!matches(&node, &json!({"env": "prod", "tier": "silver"})));
        assert!(!matches(&node, &json!({"team": "sales"})));
    }

    #[test]
    fn string_form_booleans() {
        // The SDK allows "true"/"false" strings for negate and ignoreCase.
        let neg = parse(json!({"key": "team", "value": "infra", "negate": "true"}));
        assert!(!matches(&neg, &json!({"team": "infra"})), "string negate inverts");
        assert!(matches(&neg, &json!({"team": "sales"})), "string negate inverts a miss to a match");
        let ci = parse(json!({"key": "team", "value": "INFRA", "ignoreCase": "true"}));
        assert!(matches(&ci, &json!({"team": "infra"})), "string ignoreCase is honored");
        // "false" strings read as false, and bool forms still work.
        let off = parse(json!({"key": "team", "value": "INFRA", "ignoreCase": "false"}));
        assert!(!matches(&off, &json!({"team": "infra"})), "ignoreCase=\"false\" stays case-sensitive");
        let bool_form = parse(json!({"key": "team", "value": "infra", "negate": true}));
        assert!(!matches(&bool_form, &json!({"team": "infra"})), "bool form still works");
    }

    #[test]
    fn empty_combiner() {
        // No constraint: an empty OR or AND matches anything.
        assert!(matches(&parse(json!({"OR": []})), &json!({"team": "infra"})));
        assert!(matches(&parse(json!({"AND": []})), &json!({"team": "infra"})));
    }

    #[test]
    fn combiner_variant_ordering() {
        // Combiner objects must deserialize as combiners, not as a leaf (which has required
        // `key`/`value` they lack).
        assert!(matches!(parse(json!({"OR": []})), FilterNode::Or { .. }));
        assert!(matches!(parse(json!({"AND": []})), FilterNode::And { .. }));
        assert!(matches!(parse(json!({"key": "k", "value": "v"})), FilterNode::Leaf(_)));
    }

    #[test]
    fn depth_cap_does_not_overflow() {
        // Build an AND chain deeper than the cap. The innermost leaf would match, but the cap
        // short-circuits to a non-match before the stack can blow.
        let mut node = json!({"key": "k", "value": "v"});
        for _ in 0..(MAX_DEPTH + 5) {
            node = json!({"AND": [node]});
        }
        let tree = parse(node);
        assert!(!matches(&tree, &json!({"k": "v"})));
        // A chain shallower than the cap still evaluates normally.
        let mut shallow = json!({"key": "k", "value": "v"});
        for _ in 0..3 {
            shallow = json!({"AND": [shallow]});
        }
        assert!(matches(&parse(shallow), &json!({"k": "v"})));
    }
}
