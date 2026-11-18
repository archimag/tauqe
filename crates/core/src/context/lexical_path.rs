//! Lexical decoding of repository-relative paths; filesystem confinement is separate.
use std::borrow::Cow;
pub(crate) fn strip_surrounding_markup(raw: &str) -> &str {
    raw.trim().trim_matches('`').trim()
}

fn decode_path_separators(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(index) = rest.find('\\') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        match rest.strip_prefix('\\') {
            Some(tail) => {
                decoded.push('\\');
                rest = tail;
            }
            None => decoded.push('/'),
        }
    }
    decoded.push_str(rest);
    Cow::Owned(decoded)
}

fn is_drive_root(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

pub(crate) fn decode_repo_path(raw: &str) -> Result<String, String> {
    if raw.contains('\0') {
        return Err("Change path must not contain NUL bytes".to_string());
    }
    let slashed = decode_path_separators(strip_surrounding_markup(raw));
    let trimmed = slashed.trim();
    // An absolute spelling leaves the repository whatever separator spells
    // it: a leading forward slash names the file-system root, and a leading
    // literal backslash byte names a Windows drive root or a UNC server root.
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Err(format!(
            "Change path '{}' must be repository-relative",
            trimmed
        ));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(format!(
                    "Change path '{}' must not contain '..' components",
                    trimmed
                ));
            }
            other => {
                // The drive root is decided from the first segment that
                // survives the collapse, so a spelling that only reaches it
                // through dot segments is refused exactly like the bare drive
                // path, while a later segment stays an ordinary file name.
                if segments.is_empty() && is_drive_root(other) {
                    return Err(format!(
                        "Change path '{}' must be repository-relative",
                        trimmed
                    ));
                }
                segments.push(other);
            }
        }
    }
    Ok(segments.join("/"))
}

pub fn normalize_repo_path(raw: &str) -> Result<String, String> {
    let normalized = decode_repo_path(raw)?;
    if normalized.is_empty() {
        return Err("Change path must not be empty".to_string());
    }
    Ok(normalized)
}
