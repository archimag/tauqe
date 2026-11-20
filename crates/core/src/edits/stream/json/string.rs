//! Incremental decoding of fragments from a string already identified by json_skimmer.

/// Only the unfinished UTF-8 scalar, escape or surrogate pair is retained.
#[derive(Default)]
pub(super) struct StringSegments {
    pending: Vec<u8>,
}

impl StringSegments {
    pub(super) fn push(&mut self, fragment: &[u8]) -> Result<String, ()> {
        self.pending.extend_from_slice(fragment);
        let bytes = &self.pending;
        let mut end = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                if i + 1 >= bytes.len() {
                    break;
                }
                if bytes[i + 1] == b'u' {
                    if i + 6 > bytes.len() {
                        break;
                    }
                    let hex = std::str::from_utf8(&bytes[i + 2..i + 6]).map_err(|_| ())?;
                    let unit = u16::from_str_radix(hex, 16).map_err(|_| ())?;
                    if (0xd800..=0xdbff).contains(&unit) {
                        if i + 12 > bytes.len() {
                            break;
                        }
                        if &bytes[i + 6..i + 8] != b"\\u" {
                            return Err(());
                        }
                        i += 12;
                    } else {
                        i += 6;
                    }
                } else {
                    i += 2;
                }
            } else {
                let width = match bytes[i] {
                    0..=0x7f => 1,
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => return Err(()),
                };
                if i + width > bytes.len() {
                    break;
                }
                std::str::from_utf8(&bytes[i..i + width]).map_err(|_| ())?;
                i += width;
            }
            end = i;
        }
        if end == 0 {
            return Ok(String::new());
        }
        let mut quoted = Vec::with_capacity(end + 2);
        quoted.push(b'"');
        quoted.extend_from_slice(&bytes[..end]);
        quoted.push(b'"');
        let decoded = serde_json::from_slice(&quoted).map_err(|_| ())?;
        self.pending.drain(..end);
        debug_assert!(self.pending.len() <= 12);
        Ok(decoded)
    }

    pub(super) fn finish(&self) -> Result<(), ()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_decode_live_with_only_an_unfinished_tail() {
        for (raw, expected) in [
            ("é".as_bytes(), "é"),
            (br"\n", "\n"),
            (br"\uD83D\uDE00", "😀"),
        ] {
            let mut segments = StringSegments::default();
            let mut decoded = String::new();
            for byte in raw {
                decoded.push_str(&segments.push(&[*byte]).unwrap());
                assert!(segments.pending.len() <= 12);
            }
            assert_eq!(decoded, expected);
            assert!(segments.finish().is_ok());
        }
    }

    #[test]
    fn broken_escape_never_returns_raw_bytes() {
        let mut segments = StringSegments::default();
        assert_eq!(segments.push(br"\uD83D").unwrap(), "");
        assert!(segments.finish().is_err());
        assert!(segments.push(br"\uXXXX").is_err());
    }
}
