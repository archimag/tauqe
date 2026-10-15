//! Shared sanitizers for interpolating untrusted text into prompt scaffolding.
//!
//! Plan, review, and model-authored text is embedded into XML-like structural
//! markers. Interpolated content is sanitized with a single uniform policy so
//! that a stray quote cannot break an attribute value, a model-authored
//! closing tag cannot close an enclosing element early, and an embedded
//! newline cannot split a single-line header.

/// Escapes a string for use as a double-quoted XML attribute value.
///
/// `&` is escaped first so that the entities introduced for the remaining
/// characters are not themselves re-escaped.
pub fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes a string for use as an XML text node.
///
/// Quotes carry no structural meaning inside a text node, so only `&`, `<`,
/// and `>` are escaped.
pub fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Collapses embedded newlines and trims surrounding whitespace.
///
/// Used for single-line headers and labels, where an embedded newline would
/// break the line-oriented structure of the rendered prompt.
pub fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_xml_attr_escapes_quotes_and_markup() {
        assert_eq!(escape_xml_attr("a & b"), "a &amp; b");
        assert_eq!(escape_xml_attr("w\"x<y>z"), "w&quot;x&lt;y&gt;z");
    }

    #[test]
    fn escape_xml_text_keeps_quotes_and_escapes_markup() {
        assert_eq!(escape_xml_text("a & b \"c\" <d>"), "a &amp; b \"c\" &lt;d&gt;");
    }

    #[test]
    fn single_line_collapses_newlines_and_trims() {
        assert_eq!(single_line("  a\nb\nc  "), "a b c");
        assert_eq!(single_line("plain"), "plain");
    }
}
