// SPDX-License-Identifier: Apache-2.0

//! Lexical normalization and a small synonym ontology, so surface variants of a
//! subject or attribute ("lives in", "current city", "Location:") collapse to one
//! key and contradictions actually resolve (LLD §5.1). Embedding-based matching of
//! novel attributes against existing keys is a later phase.

use std::collections::HashMap;

/// Lowercase, trim, collapse internal whitespace, and drop surrounding ASCII
/// punctuation. ASCII-leaning on purpose: it applies no Unicode NFC folding, and
/// `to_lowercase` is locale-independent, so some non-ASCII surface variants will not
/// collapse. May return an empty string for punctuation-only input; callers must
/// treat an empty key as "no key".
pub fn normalize_key(s: &str) -> String {
    let lowered = s.trim().to_lowercase();
    let collapsed = lowered.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| matches!(c, '.' | ':' | ',' | ';' | '?' | '!'))
        .trim()
        .to_string()
}

/// Maps attribute surface forms to a canonical attribute. Subjects are normalized
/// lexically only (no synonym table).
#[derive(Debug, Clone, Default)]
pub struct Ontology {
    synonyms: HashMap<String, String>,
}

impl Ontology {
    pub fn new() -> Self {
        Self {
            synonyms: HashMap::new(),
        }
    }

    /// Add (surface, canonical) pairs. Both sides are normalized first, so callers
    /// do not have to pre-normalize the table. Accepts owned strings too, so a
    /// per-tenant map can be built from runtime data.
    pub fn with_synonyms<S, I>(mut self, pairs: I) -> Self
    where
        S: Into<String>,
        I: IntoIterator<Item = (S, S)>,
    {
        for (surface, canonical) in pairs {
            self.synonyms
                .insert(normalize_key(&surface.into()), normalize_key(&canonical.into()));
        }
        self
    }

    pub fn normalize_subject(&self, subject: &str) -> String {
        normalize_key(subject)
    }

    pub fn canonical_attribute(&self, attribute: &str) -> String {
        let key = normalize_key(attribute);
        self.synonyms.get(&key).cloned().unwrap_or(key)
    }

    /// A starter map for common personal attributes, assuming the subject is the
    /// actor. Deployments extend it with `with_synonyms` or replace it. Over-broad
    /// surface forms (e.g. a bare "city") are left out so they do not misfire.
    pub fn starter() -> Self {
        Ontology::new().with_synonyms([
            ("lives in", "location"),
            ("current city", "location"),
            ("located in", "location"),
            ("residence", "location"),
            ("works at", "employer"),
            ("works for", "employer"),
            ("company", "employer"),
            ("job title", "role"),
            ("title", "role"),
            ("position", "role"),
            ("speaks", "language"),
            ("languages", "language"),
            ("full name", "name"),
            ("e-mail", "email"),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_key_lowercases_trims_and_collapses() {
        assert_eq!(normalize_key("  Lives   In "), "lives in");
        assert_eq!(normalize_key("Location:"), "location");
        assert_eq!(normalize_key("Current  City."), "current city");
    }

    #[test]
    fn canonical_attribute_maps_synonyms() {
        let o = Ontology::starter();
        assert_eq!(o.canonical_attribute("Current City"), "location");
        assert_eq!(o.canonical_attribute("lives in"), "location");
        // An attribute already canonical, or unknown, passes through normalized.
        assert_eq!(o.canonical_attribute("location"), "location");
        assert_eq!(o.canonical_attribute("Favorite Color"), "favorite color");
    }

    #[test]
    fn punctuation_only_normalizes_to_empty() {
        assert_eq!(normalize_key("?"), "");
        assert_eq!(normalize_key("..."), "");
        assert_eq!(normalize_key("  :  "), "");
    }
}
