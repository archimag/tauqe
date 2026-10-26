//! Opaque line anchors: a path-seeded bijective permutation of line indices
//! rendered as short, copy-friendly tokens.
//!
//! A search/replace edit forces the model to echo every replaced line verbatim,
//! and that echo is the most expensive and most fragile part of a patch. An
//! opaque anchor replaces the echo with a compact token naming the line instead.
//!
//! The codec guarantees four properties the harness relies on:
//!
//! - **Bijective.** Every line index of the sixteen-bit domain maps to exactly
//!   one token, and every valid token maps back to exactly one index. Two lines
//!   of one file therefore never share an anchor, and a decoded anchor is never
//!   a guess.
//! - **Path-seeded.** The permutation is keyed by the repository path of the
//!   file, so the same line of two different files carries unrelated tokens and
//!   a token copied out of another file's block names no line its author
//!   intended. The seed derives deterministically from the path, so an anchor
//!   rendered in one round decodes in any later round, in any process, without
//!   shared state.
//! - **Avalanching.** The index first travels through a four-round Feistel
//!   network, so the tokens of neighbouring lines share no visible structure.
//!   The model cannot infer the counting direction from the tokens themselves
//!   and has to copy them, which is the exact-copy behavior the protocol wants.
//! - **Fixed length.** Every token is [`ANCHOR_LEN`] characters over a
//!   thirty-two symbol alphabet that omits visually ambiguous glyphs, so a token
//!   stays unambiguous in a prompt, a diff, and a terminal.
//!
//! Sixteen bits are the whole domain the network covers: a file longer than
//! `u16::MAX` lines cannot be addressed by anchors at all.

use thiserror::Error;

/// Number of characters in every anchor.
pub const ANCHOR_LEN: usize = 4;

/// Bits carried by one anchor symbol; the alphabet has `1 << 5 == 32` symbols.
const BITS_PER_SYMBOL: u32 = 5;

/// Bit offsets of the four symbols of an anchor, most significant first.
const SYMBOL_SHIFTS: [u32; ANCHOR_LEN] = [15, 10, 5, 0];

/// Shuffled alphabet of the codec.
///
/// The glyphs that are easy to confuse while reading a prompt or a terminal —
/// `0`, `O`, `1`, `I`, `L` and their lowercase counterparts — are absent. The
/// remaining symbols are shuffled, so an anchor never reads as a counter.
const ALPHABET: &[u8; 32] = b"kR7tQw2nZbY9mDgHpXsVcEaN4jUf8KrW";

/// Offset basis of the FNV-1a hash that turns a repository path into round keys.
///
/// The seed derivation is written inline on purpose: the codec is the
/// deliberate subject of this module, and a seed needs no contract beyond
/// determinism — one path must derive one key set in every process.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// Prime of the FNV-1a hash that turns a repository path into round keys.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Every rejection the codec can produce.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnchorError {
    #[error(
        "Anchor '{anchor}' has {length} characters; an anchor is exactly four characters long."
    )]
    InvalidLength { anchor: String, length: usize },

    #[error("Anchor '{anchor}' contains {symbol:?}, which is not part of the anchor alphabet.")]
    InvalidChar { anchor: String, symbol: char },

    #[error("Anchor '{anchor}' does not name a line of this file.")]
    OutsideDomain { anchor: String },
}

/// Renders the opaque anchor of a line of one file.
///
/// The path seeds the permutation and the index is a `u16` because the network
/// permutes exactly sixteen bits. The domain is therefore explicit at the type
/// level: a caller holding a longer file has to decide what happens above line
/// 65535 instead of silently receiving the anchor of a different line.
pub fn encode_anchor(path: &str, line_idx: u16) -> String {
    let permuted = feistel_permute(&round_keys(path), line_idx);
    let mut anchor = String::with_capacity(ANCHOR_LEN);
    for shift in SYMBOL_SHIFTS {
        // The mask bounds the digit to the thirty-two alphabet symbols, so the
        // lookup can never leave the table.
        let digit = usize::from((permuted >> shift) & 0x1F);
        anchor.push(char::from(ALPHABET[digit]));
    }
    anchor
}

/// Decodes an opaque anchor of one file back into the line index it named.
///
/// Decoding is the exact inverse of [`encode_anchor`] for the same path and
/// costs one network pass, so a staged change set validates and applies its
/// anchors without searching.
pub fn decode_anchor(path: &str, anchor: &str) -> Result<usize, AnchorError> {
    // Counting symbols rather than bytes keeps a multi-byte glyph inside a
    // length diagnostic instead of splitting a character.
    let length = anchor.chars().count();
    if length != ANCHOR_LEN {
        return Err(AnchorError::InvalidLength {
            anchor: anchor.to_owned(),
            length,
        });
    }

    let mut permuted: u32 = 0;
    for symbol in anchor.chars() {
        let digit = symbol_index(symbol).ok_or_else(|| AnchorError::InvalidChar {
            anchor: anchor.to_owned(),
            symbol,
        })?;
        permuted = (permuted << BITS_PER_SYMBOL) | digit;
    }

    // Four symbols span twenty bits while the network permutes sixteen, so an
    // anchor whose leading symbol is too large names no line at all. Rejecting
    // it here keeps every accepted anchor inside the bijection instead of
    // silently wrapping around.
    let permuted = u16::try_from(permuted).map_err(|_| AnchorError::OutsideDomain {
        anchor: anchor.to_owned(),
    })?;
    Ok(usize::from(feistel_inverse(&round_keys(path), permuted)))
}

/// Position of a symbol in the alphabet, if the alphabet contains it.
fn symbol_index(symbol: char) -> Option<u32> {
    ALPHABET
        .iter()
        .position(|candidate| char::from(*candidate) == symbol)
        .and_then(|index| u32::try_from(index).ok())
}

/// Derives the four round keys of the Feistel network from a repository path.
///
/// The path is the seed of the permutation: two files share a key set only when
/// their normalized paths are equal, while one path always derives one key set,
/// in every process and every session. The finalizer spreads the hash across
/// the whole word, so a path that differs in one late byte still moves every
/// key.
fn round_keys(path: &str) -> [u8; 4] {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    [
        (hash >> 24) as u8,
        (hash >> 32) as u8,
        (hash >> 40) as u8,
        (hash >> 48) as u8,
    ]
}

/// Runs the line index through the Feistel network.
///
/// The network splits the sixteen bits into two eight-bit halves and runs four
/// rounds, each replacing `(left, right)` with `(right, left ^ F(right))`. Every
/// round is a bijection for any round function, so the permutation is exact by
/// construction and the round function only has to provide diffusion.
fn feistel_permute(keys: &[u8; 4], line_idx: u16) -> u16 {
    let [mut left, mut right] = line_idx.to_be_bytes();
    for key in keys {
        let mixed = left ^ round_function(right, *key);
        left = right;
        right = mixed;
    }
    u16::from_be_bytes([left, right])
}

/// Inverts the network by replaying its rounds in reverse.
///
/// Inverting one round is `(left, right) -> (right ^ F(left), left)`, which is
/// evaluated for every key from the last round back to the first, so decoding
/// costs the same four rounds as encoding.
fn feistel_inverse(keys: &[u8; 4], permuted: u16) -> u16 {
    let [mut left, mut right] = permuted.to_be_bytes();
    for key in keys.iter().rev() {
        let previous_left = right ^ round_function(left, *key);
        right = left;
        left = previous_left;
    }
    u16::from_be_bytes([left, right])
}

/// Round function of the network.
///
/// A Feistel round stays a bijection whatever this function returns, so its only
/// job is diffusion: the multiply spreads every input bit across the byte, the
/// rotation moves the result away from the input positions, and the keys keep the
/// rounds from collapsing into the same small set of permutations.
fn round_function(half: u8, key: u8) -> u8 {
    let mixed = half.wrapping_mul(0x9D).wrapping_add(key);
    mixed.rotate_left(3) ^ key.rotate_right(2)
}

#[cfg(test)]
mod anchor_tests {
    use super::*;
    use std::collections::HashSet;

    /// Paths of distinct shapes and lengths, so every exhaustive sweep observes
    /// more than one seed of the network.
    const PATHS: [&str; 3] = ["a.rs", "crates/core/src/deep/module.rs", ""];

    #[test]
    fn every_line_index_round_trips_and_no_two_lines_share_an_anchor() {
        for path in PATHS {
            let mut anchors = HashSet::new();
            for index in 0..=u16::MAX {
                let anchor = encode_anchor(path, index);
                assert_eq!(anchor.chars().count(), ANCHOR_LEN);
                assert_eq!(
                    decode_anchor(path, &anchor),
                    Ok(usize::from(index)),
                    "the anchor {anchor} of line {index} in {path:?} must decode back"
                );
                assert!(
                    anchors.insert(anchor),
                    "line {index} of {path:?} reused the anchor of an earlier line"
                );
            }
            assert_eq!(anchors.len(), usize::from(u16::MAX) + 1);
        }
    }

    #[test]
    fn feistel_network_permutes_the_entire_domain_for_every_seed() {
        for path in PATHS {
            let keys = round_keys(path);
            let mut permuted = HashSet::new();
            for index in 0..=u16::MAX {
                let value = feistel_permute(&keys, index);
                assert_eq!(feistel_inverse(&keys, value), index);
                assert!(
                    permuted.insert(value),
                    "the network of {path:?} mapped two different lines onto {value}"
                );
            }
            assert_eq!(permuted.len(), usize::from(u16::MAX) + 1);
        }
    }

    #[test]
    fn one_path_seeds_one_permutation_and_paths_rarely_agree() {
        // The seed is a pure function of the path: one repository path derives
        // one key set everywhere, and different paths derive different ones.
        assert_eq!(round_keys("a.rs"), round_keys("a.rs"));
        assert_ne!(round_keys("a.rs"), round_keys("b.rs"));
        assert_ne!(round_keys("a.rs"), round_keys("src/a.rs"));

        // Two key sets name two different permutations of the same domain, so
        // the two files agree on only a handful of lines instead of sharing
        // their tokens line for line.
        let mut agreements = 0usize;
        for index in 0..=u16::MAX {
            if encode_anchor("a.rs", index) == encode_anchor("b.rs", index) {
                agreements += 1;
            }
        }
        assert!(
            agreements <= 16,
            "the two paths agreed on {agreements} anchors of the same lines"
        );

        // An anchor decodes to its own line only under the path that rendered
        // it; under any other path it names a different line or none at all.
        let differing = (0..=u16::MAX)
            .find(|index| encode_anchor("a.rs", *index) != encode_anchor("b.rs", *index))
            .expect("two seeded permutations cannot agree on every line");
        let anchor = encode_anchor("a.rs", differing);
        assert_eq!(decode_anchor("a.rs", &anchor), Ok(usize::from(differing)));
        assert_ne!(
            decode_anchor("b.rs", &anchor),
            Ok(usize::from(differing)),
            "an anchor must not name its line under a foreign path"
        );
    }

    #[test]
    fn neighbouring_lines_produce_unrelated_anchors() {
        for path in PATHS {
            let mut pairs = 0usize;
            let mut total_distance = 0usize;
            let mut leading_changes = 0usize;
            let mut previous = encode_anchor(path, 0);
            for index in 1..=u16::MAX {
                let anchor = encode_anchor(path, index);
                let distance = previous
                    .chars()
                    .zip(anchor.chars())
                    .filter(|(before, after)| before != after)
                    .count();
                assert!(
                    distance > 0,
                    "lines {} and {index} of {path:?} share the anchor {anchor}",
                    index - 1
                );
                total_distance += distance;
                if previous.chars().next() != anchor.chars().next() {
                    leading_changes += 1;
                }
                pairs += 1;
                previous = anchor;
            }
            // Avalanche: well over half of the symbol positions of two
            // consecutive lines differ, and the leading symbol changes on
            // most of the pairs too.
            assert!(
                total_distance * 2 >= pairs * ANCHOR_LEN,
                "consecutive anchors of {path:?} are too similar: {total_distance} differences over {pairs} pairs"
            );
            assert!(
                leading_changes * 4 >= pairs,
                "the leading symbol of {path:?} barely changes: {leading_changes} of {pairs} pairs"
            );
        }
    }

    #[test]
    fn the_alphabet_is_unambiguous_and_fully_distinct() {
        let symbols: HashSet<u8> = ALPHABET.iter().copied().collect();
        assert_eq!(symbols.len(), ALPHABET.len());
        assert_eq!(ALPHABET.len(), 1usize << BITS_PER_SYMBOL);
        for forbidden in ['0', 'O', '1', 'I', 'L', 'o', 'i', 'l'] {
            assert!(
                !symbols.contains(&u8::try_from(forbidden).unwrap_or_default()),
                "the ambiguous glyph {forbidden} is part of the alphabet"
            );
        }
        assert!(ALPHABET.iter().all(u8::is_ascii_alphanumeric));
    }

    #[test]
    fn malformed_anchors_are_rejected() {
        let path = "a.rs";
        for wrong_length in ["", "abc", "abcde", "яЯя"] {
            assert!(
                matches!(
                    decode_anchor(path, wrong_length),
                    Err(AnchorError::InvalidLength { .. })
                ),
                "{wrong_length:?} must be rejected by length"
            );
        }
        assert_eq!(
            decode_anchor(path, "abc"),
            Err(AnchorError::InvalidLength {
                anchor: "abc".to_owned(),
                length: 3,
            })
        );

        // A multi-byte glyph is one symbol, never a byte slice.
        assert!(matches!(
            decode_anchor(path, "яЯяЯ"),
            Err(AnchorError::InvalidChar { symbol: 'я', .. })
        ));

        // The visually ambiguous glyphs are outside the alphabet.
        for anchor in ["kR70", "kR7O", "kR71", "kR7I", "kR7L"] {
            assert!(
                matches!(
                    decode_anchor(path, anchor),
                    Err(AnchorError::InvalidChar { .. })
                ),
                "{anchor} must be rejected as an invalid glyph"
            );
        }

        // Well formed but outside the sixteen-bit image: four symbols span
        // twenty bits, so only anchors the codec emitted are accepted.
        for anchor in ["7ttt", "WkR7"] {
            assert!(
                matches!(
                    decode_anchor(path, anchor),
                    Err(AnchorError::OutsideDomain { .. })
                ),
                "{anchor} must be rejected as outside the domain"
            );
        }
    }
}
