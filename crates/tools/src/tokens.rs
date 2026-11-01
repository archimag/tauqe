//! Deterministic token-size heuristic shared by the whole engine.
//!
//! The engine shares a single heuristic of roughly four bytes per token. Basing
//! the estimate on bytes keeps the on-disk sizing (only known in bytes) and the
//! in-memory sizing consistent, so multi-byte UTF-8 (Cyrillic, CJK, emoji) is
//! never counted two to three times heavier by one code path than by another.
//!
//! The heuristic belongs to the substrate: the repository map budgets its
//! outline with it, the context layer prices a file with it, and the history
//! layer sizes its transcript with it. One definition serves them all, so no
//! consumer can drift to a private ratio.

/// Estimates a token count from a raw byte length.
pub fn estimate_tokens_from_bytes(byte_len: u64) -> u64 {
    byte_len.div_ceil(4)
}

/// Estimates a token count for an in-memory string.
///
/// Delegates to [`estimate_tokens_from_bytes`] so that string content and
/// file metadata share one consistent heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    estimate_tokens_from_bytes(text.len() as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::{estimate_tokens, estimate_tokens_from_bytes};

    #[test]
    fn sizes_bytes_and_string_content_with_one_ratio() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Multi-byte UTF-8 is sized by encoded byte length, not scalar count.
        assert_eq!(estimate_tokens("абвг"), 2);
        assert_eq!(estimate_tokens_from_bytes(0), 0);
        assert_eq!(estimate_tokens_from_bytes(4), 1);
        assert_eq!(estimate_tokens_from_bytes(5), 2);
    }
}
