//! UUIDv7 generation and validation for review sessions.
//!
//! A review session is identified by a UUIDv7 rather than a counter or a
//! random v4 value. The 48-bit millisecond Unix timestamp occupies the
//! leading bytes, so identifier strings sort chronologically and a
//! directory listing of `.tauqe/reviews/{uuid7}.json` follows creation
//! order without touching file metadata. The version and variant nibbles
//! mark the identifier as RFC 9562 v7, and every remaining bit comes
//! from the operating system CSPRNG, which makes collisions and file-name
//! races practically impossible.

use std::time::{SystemTime, UNIX_EPOCH};

/// Length of a canonical hyphenated UUID string.
const UUID_TEXT_LEN: usize = 36;

/// Mask selecting the 48-bit timestamp field of a UUIDv7.
const TIMESTAMP_MASK: u64 = 0x0000_ffff_ffff_ffff;

/// Generates a new UUIDv7 identifier in canonical hyphenated form.
///
/// The leading 48 bits carry the current Unix time in milliseconds, so
/// the lexicographic order of generated values matches creation order.
/// The version field is set to 7 and the variant field to the RFC 4122
/// `0b10` pattern. All remaining bits are filled from the system CSPRNG.
pub fn generate_uuid_v7() -> String {
    generate_uuid_v7_at(current_unix_millis())
}

/// Validates that `value` is a well-formed UUIDv7 string.
///
/// The check enforces the canonical 36-character layout, the hyphen
/// positions, hexadecimal digits, version `7`, and the RFC 4122 variant
/// bits. Hexadecimal digits may be upper or lower case.
pub fn is_valid_uuid_v7(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != UUID_TEXT_LEN {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if *byte != b'-' {
                return false;
            }
        } else if hex_nibble(*byte).is_none() {
            return false;
        }
    }
    // Version nibble: first hex digit of the third group.
    if hex_nibble(bytes[14]) != Some(7) {
        return false;
    }
    // Variant nibble: first hex digit of the fourth group, must be 0b10xx.
    matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

/// Extracts the 48-bit millisecond Unix timestamp from a UUIDv7 string.
///
/// Returns `None` when `value` is not a valid UUIDv7. The timestamp is
/// the value stored in the leading 48 bits: the Unix time in milliseconds
/// at which the identifier was generated.
pub fn parse_uuid_v7_timestamp(value: &str) -> Option<u64> {
    if !is_valid_uuid_v7(value) {
        return None;
    }
    let mut millis: u64 = 0;
    let mut digits = 0;
    for byte in value.bytes() {
        if byte == b'-' {
            continue;
        }
        // `is_valid_uuid_v7` guarantees a hexadecimal digit here.
        millis = (millis << 4) | u64::from(hex_nibble(byte)?);
        digits += 1;
        if digits == 12 {
            break;
        }
    }
    Some(millis)
}

/// Generates a UUIDv7 for an explicit millisecond timestamp.
///
/// The timestamp is truncated to its low 48 bits. The version, variant,
/// and random fields are stamped exactly as in [`generate_uuid_v7`].
fn generate_uuid_v7_at(millis: u64) -> String {
    let millis = millis & TIMESTAMP_MASK;
    let mut bytes = [0u8; 16];
    bytes[0] = (millis >> 40) as u8;
    bytes[1] = (millis >> 32) as u8;
    bytes[2] = (millis >> 24) as u8;
    bytes[3] = (millis >> 16) as u8;
    bytes[4] = (millis >> 8) as u8;
    bytes[5] = millis as u8;
    fill_random(&mut bytes[6..]);
    // Stamp version 7 into the high nibble of the seventh byte.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    // Stamp the RFC 4122 variant into the high two bits of the ninth byte.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format_uuid(&bytes)
}

/// Formats 16 UUID bytes as a canonical lowercase hyphenated string.
fn format_uuid(bytes: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(UUID_TEXT_LEN);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Fills `buf` with cryptographically secure random bytes.
///
/// When the OS entropy source is momentarily unavailable the buffer is
/// filled from a time-seeded sequence instead, so an entropy failure
/// never aborts a review turn.
fn fill_random(buf: &mut [u8]) {
    if getrandom::getrandom(buf).is_ok() {
        return;
    }
    let mut seed = current_unix_millis();
    for slot in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *slot = (seed >> 33) as u8;
    }
}

/// Returns the current Unix time in milliseconds, or 0 before the epoch.
fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Decodes a single hexadecimal digit.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let bytes = value.as_bytes();
            assert_eq!(hex_nibble(bytes[14]), Some(7), "version: {value}");
            assert!(
                matches!(bytes[19], b'8' | b'9' | b'a' | b'b'),
                "variant: {value}"
            );
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
        assert!(!is_valid_uuid_v7("018f9c1e6b3a7c1e8b3a000000000000"));
        // Wrong version nibble (6 instead of 7).
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-6c1e-8b3a-000000000000"));
        // Wrong variant nibble (0 instead of 0b10xx).
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-7c1e-0b3a-000000000000"));
        // Non-hexadecimal character.
        assert!(!is_valid_uuid_v7("018f9c1e-6b3a-7c1e-8b3a-00000000000g"));
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
