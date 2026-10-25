//! UUIDv7 generation and validation for review sessions.
//!
//! A review session is identified by a UUIDv7 rather than a counter or a
//! random v4 value. The 48-bit millisecond Unix timestamp occupies the
//! leading bytes, so identifier strings sort chronologically and a
//! directory listing of `.tauqe/reviews/{uuid7}.json` follows creation
//! order without touching file metadata.
//!
//! Bit layout, entropy, version and variant stamping, and text rendering
//! all belong to the `uuid` crate. This module only narrows that crate to
//! the contract the rest of the workspace relies on: a canonical
//! hyphenated string out, the same canonical spelling recognized on the
//! way back in, and the millisecond timestamp behind an identifier.

use uuid::{Uuid, Variant};

/// Generates a new UUIDv7 identifier in canonical hyphenated form.
///
/// The leading 48 bits carry the current Unix time in milliseconds, so
/// the lexicographic order of generated values matches creation order.
/// The crate stamps the version and variant fields and fills every
/// remaining bit from the operating system CSPRNG.
pub fn generate_uuid_v7() -> String {
    Uuid::now_v7().to_string()
}

/// Validates that `value` is a well-formed UUIDv7 string.
///
/// Only the canonical hyphenated spelling is accepted: the parser also
/// accepts the compact, braced, and URN spellings, so the identifier is
/// rendered back and compared with the input before it qualifies.
/// Hexadecimal digits may be upper or lower case.
pub fn is_valid_uuid_v7(value: &str) -> bool {
    parse_canonical_v7(value).is_some()
}

/// Extracts the 48-bit millisecond Unix timestamp from a UUIDv7 string.
///
/// Returns `None` when `value` is not a valid UUIDv7. The timestamp is the
/// value stored in the leading 48 bits: the Unix time in milliseconds at
/// which the identifier was generated.
pub fn parse_uuid_v7_timestamp(value: &str) -> Option<u64> {
    let uuid = parse_canonical_v7(value)?;
    let (seconds, nanos) = uuid.get_timestamp()?.to_unix();
    Some(seconds.saturating_mul(1000) + u64::from(nanos) / 1_000_000)
}

/// Parses `value` as a canonical hyphenated UUIDv7.
///
/// Returns `None` for a malformed spelling, a different version, or a
/// variant other than the RFC 4122 `0b10` pattern.
fn parse_canonical_v7(value: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(value).ok()?;
    if !uuid.to_string().eq_ignore_ascii_case(value) {
        return None;
    }
    if uuid.get_version_num() != 7 || uuid.get_variant() != Variant::RFC4122 {
        return None;
    }
    Some(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::timestamp::context;
    use uuid::Timestamp;

    /// Length of a canonical hyphenated UUID string.
    const UUID_TEXT_LEN: usize = 36;

    /// Mask selecting the 48-bit timestamp field of a UUIDv7.
    const TIMESTAMP_MASK: u64 = 0x0000_ffff_ffff_ffff;

    /// Builds a UUIDv7 for an explicit millisecond timestamp.
    ///
    /// The timestamp is truncated to its low 48 bits, so the round-trip and
    /// ordering checks can reach the top of the field.
    fn generate_uuid_v7_at(millis: u64) -> String {
        let millis = millis & TIMESTAMP_MASK;
        let timestamp = Timestamp::from_unix(
            context::NoContext,
            millis / 1000,
            ((millis % 1000) * 1_000_000) as u32,
        );
        Uuid::new_v7(timestamp).to_string()
    }

    #[test]
    fn generated_identifier_has_canonical_shape() {
        for _ in 0..64 {
            let value = generate_uuid_v7();
            assert_eq!(value.len(), UUID_TEXT_LEN);
            let bytes = value.as_bytes();
            for position in [8, 13, 18, 23] {
                assert_eq!(
                    bytes[position], b'-',
                    "missing hyphen at {position}: {value}"
                );
            }
            assert_eq!(value.chars().filter(|c| *c == '-').count(), 4);
            assert!(value.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
            assert!(value.chars().all(|c| !c.is_ascii_uppercase()));
        }
    }

    #[test]
    fn version_and_variant_are_stamped() {
        for _ in 0..64 {
            let value = generate_uuid_v7();
            let uuid = Uuid::parse_str(&value).expect("generated identifier parses");
            assert_eq!(uuid.get_version_num(), 7, "version: {value}");
            assert_eq!(uuid.get_variant(), Variant::RFC4122, "variant: {value}");
        }
    }

    #[test]
    fn generated_identifier_validates() {
        for _ in 0..64 {
            let value = generate_uuid_v7();
            assert!(is_valid_uuid_v7(&value), "should validate: {value}");
        }
    }

    #[test]
    fn timestamp_round_trips() {
        for millis in [0u64, 1, 1_700_000_000_000, TIMESTAMP_MASK] {
            let value = generate_uuid_v7_at(millis);
            assert_eq!(parse_uuid_v7_timestamp(&value), Some(millis));
        }
    }

    #[test]
    fn parsed_timestamps_are_monotonic() {
        let mut previous = 0u64;
        for _ in 0..1000 {
            let value = generate_uuid_v7();
            let millis = parse_uuid_v7_timestamp(&value).expect("valid uuid");
            assert!(millis >= previous, "timestamp decreased: {value}");
            previous = millis;
        }
    }

    #[test]
    fn lexicographic_order_follows_timestamp() {
        let earlier = generate_uuid_v7_at(1_000_000);
        let later = generate_uuid_v7_at(2_000_000);
        let latest = generate_uuid_v7_at(TIMESTAMP_MASK);
        assert!(earlier < later, "{earlier} < {later}");
        assert!(later < latest, "{later} < {latest}");
    }

    #[test]
    fn rejects_malformed_identifiers() {
        assert!(!is_valid_uuid_v7(""));
        assert!(!is_valid_uuid_v7("not-a-uuid"));
        // The compact spelling without hyphens is not the canonical form.
        assert!(!is_valid_uuid_v7("018f9c1e6b3a7c1e8b3a000000000000"));
        // The braced and URN spellings are parsed by the crate but rejected here.
        assert!(!is_valid_uuid_v7("{018f9c1e-6b3a-7c1e-8b3a-000000000000}"));
        assert!(!is_valid_uuid_v7("urn:uuid:018f9c1e-6b3a-7c1e-8b3a-000000000000"));
        // Wrong version nibble (6 instead of 7).
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-6c1e-8b3a-000000000000"));
        // Wrong variant nibble (0 instead of 0b10xx).
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-7c1e-0b3a-000000000000"));
        // Non-hexadecimal character.
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-7c1e-8b3a-00000000000g"));
    }

    #[test]
    fn accepts_uppercase_canonical_spelling() {
        let value = generate_uuid_v7_at(1_700_000_000_000);
        let uppercase = value.to_uppercase();
        assert!(is_valid_uuid_v7(&uppercase), "uppercase must validate");
        assert_eq!(parse_uuid_v7_timestamp(&uppercase), Some(1_700_000_000_000));
    }

    #[test]
    fn parse_returns_none_for_invalid_identifiers() {
        assert_eq!(parse_uuid_v7_timestamp("not-a-uuid"), None);
        assert_eq!(
            parse_uuid_v7_timestamp("018f9c1e-6b3a-6c1e-8b3a-000000000000"),
            None
        );
    }
}
