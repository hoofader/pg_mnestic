// SPDX-License-Identifier: AGPL-3.0-only

//! supermemory's single `containerTag` maps onto Mnestic's `(actor_id, container_tags)`
//! (doc 04 §2). The mapping is reversible so a response can echo the exact tag the
//! caller sent, reconstructed from the two stored fields.
//!
//! Default convention (configurable later): a trailing `user:<id>` is the actor; any
//! preceding segments become container tags grouped as `key:value` pairs; a tag with no
//! `:` is the actor outright. The load-bearing invariant is `reconstruct(parse(tag)) ==
//! tag`, covered by tests.
//!
//! Out of scope here, owed by the endpoint layer: validating the supermemory pattern
//! (`^[a-zA-Z0-9_:-]+$`, non-empty, max 100) before parsing, and the plural
//! `containerTags[]` form. `Scope` reconstructs faithfully only for values produced by
//! `parse_container_tag`; a hand-built `Scope` has no such guarantee.

/// A parsed `containerTag`: who the memory is about, plus the coarser scope tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub actor_id: String,
    pub container_tags: Vec<String>,
}

const USER_PREFIX: &str = "user";

/// Map a `containerTag` to `(actor_id, container_tags)`. A tag with no `:` is the actor
/// with no tags. Otherwise the last segment is the actor, except a trailing `user:<id>`
/// pair stays together as the actor; the remaining leading segments become container
/// tags (one per segment).
pub fn parse_container_tag(tag: &str) -> Scope {
    let segments: Vec<&str> = tag.split(':').collect();
    if segments.len() < 2 {
        return Scope { actor_id: tag.to_string(), container_tags: Vec::new() };
    }
    let n = segments.len();
    if segments[n - 2] == USER_PREFIX {
        // Keep the trailing user:<id> together so the actor is the user, not the bare id.
        let actor_id = format!("{}:{}", USER_PREFIX, segments[n - 1]);
        // Pair the leading segments (org:123, project:9) so a container filter matches a
        // whole pair, not a bare key that would match any id under it.
        let container_tags = pair_up(&segments[..n - 2]);
        Scope { actor_id, container_tags }
    } else {
        let actor_id = segments[n - 1].to_string();
        let container_tags = segments[..n - 1].iter().map(|s| s.to_string()).collect();
        Scope { actor_id, container_tags }
    }
}

/// Group consecutive segments into `key:value` tags; a lone trailing segment is kept
/// as-is. Joining the result, then the actor, with `:` reconstructs the original.
fn pair_up(segments: &[&str]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        if i + 1 < segments.len() {
            tags.push(format!("{}:{}", segments[i], segments[i + 1]));
            i += 2;
        } else {
            tags.push(segments[i].to_string());
            i += 1;
        }
    }
    tags
}

/// Rebuild the original `containerTag` from a `Scope`. Inverse of `parse_container_tag`
/// for any tag it produced: the container tags and the actor rejoin with `:`, and the
/// actor may itself carry the `user:` colon, which `join` preserves.
pub fn reconstruct_container_tag(scope: &Scope) -> String {
    if scope.container_tags.is_empty() {
        return scope.actor_id.clone();
    }
    let mut parts = scope.container_tags.clone();
    parts.push(scope.actor_id.clone());
    parts.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tag_is_the_actor() {
        let s = parse_container_tag("alice");
        assert_eq!(s.actor_id, "alice");
        assert!(s.container_tags.is_empty());
    }

    #[test]
    fn trailing_user_pair_is_the_actor() {
        let s = parse_container_tag("org:123:user:456");
        assert_eq!(s.actor_id, "user:456");
        assert_eq!(s.container_tags, vec!["org:123"]);
    }

    #[test]
    fn leading_segments_group_into_pairs() {
        let s = parse_container_tag("org:123:project:9:user:456");
        assert_eq!(s.actor_id, "user:456");
        assert_eq!(s.container_tags, vec!["org:123", "project:9"]);
    }

    #[test]
    fn user_only_tag() {
        let s = parse_container_tag("user:456");
        assert_eq!(s.actor_id, "user:456");
        assert!(s.container_tags.is_empty());
    }

    #[test]
    fn non_user_hierarchy_uses_final_segment() {
        let s = parse_container_tag("org:123");
        assert_eq!(s.actor_id, "123");
        assert_eq!(s.container_tags, vec!["org"]);
    }

    #[test]
    fn round_trips() {
        for tag in [
            "alice",
            "user:456",
            "org:123:user:456",
            "org:123",
            "org:123:project:9:user:456",
            "a:b:c",
            "team-7:user:u_42",
        ] {
            assert_eq!(reconstruct_container_tag(&parse_container_tag(tag)), tag, "round-trip {tag:?}");
        }
    }
}
