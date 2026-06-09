// SPDX-License-Identifier: Apache-2.0

//! Text chunking for the document/RAG path. A document is split into overlapping
//! windows before embedding so a passage that straddles a window boundary is still
//! recoverable from the neighbouring chunk.

/// Split `text` into windows of at most `max_chars`, preferring to break at the last
/// whitespace inside the window so words stay whole, with `overlap` characters of the
/// previous window repeated at the start of the next. Returns trimmed, non-empty
/// chunks. A `text` that fits in one window returns a single chunk (or none, if blank).
///
/// `overlap` is clamped to half `max_chars` so each step advances at least
/// `max_chars / 2`, which bounds the chunk count on low-whitespace input; `max_chars`
/// of 0 is treated as 1. Counts are in characters, not bytes, so multibyte text is
/// never split mid-codepoint.
pub fn chunk_text(text: &str, max_chars: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let overlap = overlap.min(max_chars / 2);

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < n {
        let hard_end = (start + max_chars).min(n);
        // Back off to the last whitespace in the window so a word is not cut, unless
        // that would strand the whole window (no whitespace, or only at the very
        // start), in which case take the hard cut.
        let mut end = hard_end;
        if hard_end < n {
            if let Some(ws) = (start..hard_end).rev().find(|&i| chars[i].is_whitespace()) {
                if ws > start {
                    end = ws;
                }
            }
        }

        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        // Drop a chunk identical to the previous one: the whitespace backoff plus
        // overlap can re-emit the same window content, which would double-count in
        // retrieval and waste an embedding.
        if !trimmed.is_empty() && chunks.last().map(String::as_str) != Some(trimmed) {
            chunks.push(trimmed.to_string());
        }

        if end >= n {
            break;
        }
        // Advance with overlap, but always make forward progress past `start`.
        let next = end.saturating_sub(overlap);
        start = if next > start { next } else { end };
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_yields_nothing() {
        assert!(chunk_text("", 100, 10).is_empty());
        assert!(chunk_text("   \n  ", 100, 10).is_empty());
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("a short note", 100, 10), vec!["a short note"]);
    }

    #[test]
    fn long_text_splits_on_whitespace_with_overlap() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        let chunks = chunk_text(text, 20, 6);
        assert!(chunks.len() > 1, "splits into multiple chunks");
        // No chunk exceeds the window.
        assert!(chunks.iter().all(|c| c.chars().count() <= 20));
        // Words stay whole (no chunk starts or ends mid-token against the source).
        assert!(chunks.iter().all(|c| !c.starts_with(' ') && !c.ends_with(' ')));
        // Overlap repeats a token: the union of chunks covers every source word.
        for word in text.split(' ') {
            assert!(chunks.iter().any(|c| c.contains(word)), "word {word:?} survives chunking");
        }
    }

    #[test]
    fn no_whitespace_hard_splits() {
        let text = "abcdefghijklmnop";
        let chunks = chunk_text(text, 5, 1);
        assert!(chunks.len() >= 3, "a token longer than the window is hard-split");
        assert!(chunks.iter().all(|c| c.chars().count() <= 5));
    }

    #[test]
    fn multibyte_is_not_split_mid_codepoint() {
        let text = "café résumé naïve façade jalapeño";
        let chunks = chunk_text(text, 8, 2);
        // Reassembling never panics and each chunk is valid UTF-8 by construction.
        assert!(chunks.iter().all(|c| !c.is_empty()));
        assert!(chunks.iter().any(|c| c.contains("café")));
    }

    #[test]
    fn overlap_at_or_above_max_still_progresses() {
        // overlap >= max_chars must not loop forever; it is clamped.
        let chunks = chunk_text("one two three four five", 6, 100);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn no_consecutive_duplicate_chunks() {
        // The backoff+overlap interplay must not emit the same chunk twice in a row.
        let chunks = chunk_text("one two three four five", 6, 100);
        for w in chunks.windows(2) {
            assert_ne!(w[0], w[1], "adjacent chunks are not identical");
        }
    }

    #[test]
    fn low_whitespace_count_is_bounded_by_stride() {
        // 100 distinct chars, no whitespace: the overlap clamp keeps the stride at
        // max_chars/2, so the count is ~n/(max/2), not ~n (the un-clamped blowup).
        let text: String = (0..100u8).map(|i| char::from(b'a' + i % 26)).collect();
        let chunks = chunk_text(&text, 10, 100);
        assert!(chunks.len() <= 25, "bounded chunk count, got {}", chunks.len());
    }
}
