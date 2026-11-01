//! Deterministic parsing of toolchain output into structured diagnostics.
//!
//! The harness never reasons about terminal transcripts: the output of a
//! verification command is parsed into typed [`Diagnostic`] values, so a
//! verification-healing round reads the offending file, line, and message
//! instead of a compiler banner full of ANSI escapes.
//!
//! Two input shapes are accepted, because toolchains report failures in two ways
//! and a single run frequently mixes them: a structured record stream
//! (`--message-format=json`, which also carries the pre-rendered text) and plain
//! rendered lines. An unreadable record degrades to text parsing instead of
//! discarding the remaining findings of a failed verification.

use serde_json::Value;

use tauqe_protocol::{Diagnostic, DiagnosticSeverity};

/// Upper bound on the findings one verification command may report.
///
/// A failing build repeats the same finding for every generic instantiation, so
/// the bound keeps the rendered report a predictable part of the model context.
pub(super) const MAX_DIAGNOSTICS: usize = 256;

/// Parses the combined output of one verification command.
pub(super) fn parse_diagnostics(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = parse_record_stream(output);
    diagnostics.truncate(MAX_DIAGNOSTICS);
    diagnostics
}

/// Parses a structured record stream, keeping non-JSON lines as rendered text.
///
/// A structured stream still sends the human-readable rendering of some failures
/// (linker errors, test panics) through the same pipe, so the two sources are
/// merged instead of choosing one.
fn parse_record_stream(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut rendered = String::new();

    for line in output.lines() {
        match serde_json::from_str::<Value>(line.trim()) {
            Ok(record) => diagnostics.extend(record_diagnostic(&record)),
            Err(_) => {
                rendered.push_str(line);
                rendered.push('\n');
            }
        }
    }

    diagnostics.extend(parse_rendered_text(&rendered));
    diagnostics
}

/// Extracts the diagnostic carried by one structured record.
///
/// Every other record reason describes build bookkeeping, not a finding, and
/// yields `None`.
fn record_diagnostic(record: &Value) -> Option<Diagnostic> {
    if record.get("reason").and_then(Value::as_str) != Some("compiler-message") {
        return None;
    }

    let message = record.get("message")?;
    let span = primary_span(message);
    let text = message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if text.is_empty() && span.is_none() {
        return None;
    }

    Some(Diagnostic {
        file: span
            .and_then(|span| span.get("file_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        line: span_number(span, "line_start"),
        column: span_number(span, "column_start"),
        severity: message
            .get("level")
            .and_then(Value::as_str)
            .map(DiagnosticSeverity::parse_lossy)
            .unwrap_or_default(),
        code: message
            .get("code")
            .and_then(|code| code.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        message: text.to_string(),
        rendered: message
            .get("rendered")
            .and_then(Value::as_str)
            .map(strip_ansi)
            .map(|rendered| rendered.trim().to_string())
            .filter(|rendered| !rendered.is_empty()),
    })
}

/// Returns the primary span of a record, falling back to the first span, so a
/// record whose toolchain marked no primary span is still located.
fn primary_span(message: &Value) -> Option<&Value> {
    let spans = message.get("spans").and_then(Value::as_array)?;
    spans
        .iter()
        .find(|span| {
            span.get("is_primary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| spans.first())
}

/// Reads a one-based span coordinate, dropping values a `u32` cannot hold.
fn span_number(span: Option<&Value>, key: &str) -> Option<u32> {
    let number = span?.get(key)?.as_u64()?;
    u32::try_from(number).ok()
}

/// Parses rendered toolchain lines into diagnostics.
///
/// Three shapes are recognized: a header (`error[E0308]: mismatched types`)
/// followed by a `--> file:line:col` location line, a single located line
/// (`file:line:col: severity: message`, also in the `file(line,col)` form a
/// TypeScript compiler emits), and a test panic report
/// (`thread '...' panicked at file:line:col:`). Everything else — source
/// excerpts, carets, and summary banners — is skipped.
fn parse_rendered_text(output: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut pending: Option<Diagnostic> = None;
    let mut lines = output.lines().map(strip_ansi);

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // A rendered location belongs to the header rendered above it.
        if let Some(rest) = trimmed.strip_prefix("-->") {
            let location = locate(rest.trim_start());
            let mut diagnostic = pending.take();
            if let (Some(diagnostic), Some((file, number, column, _))) =
                (diagnostic.as_mut(), location)
            {
                diagnostic.file = Some(file.to_string());
                diagnostic.line = Some(number);
                diagnostic.column = column;
            }
            if let Some(diagnostic) = diagnostic {
                diagnostics.push(diagnostic);
            }
            continue;
        }

        // A panic report places its message after the location, or on the next
        // non-blank line when the report wraps.
        if let Some((_, rest)) = trimmed.split_once("panicked at ") {
            if let Some((file, number, column, tail)) = locate(rest) {
                let mut message = tail.trim().trim_start_matches(':').trim().to_string();
                if message.is_empty() {
                    message = next_non_empty(&mut lines);
                }
                push_diagnostic(
                    &mut diagnostics,
                    &mut pending,
                    Diagnostic {
                        file: Some(file.to_string()),
                        line: Some(number),
                        column,
                        severity: DiagnosticSeverity::Error,
                        code: None,
                        message,
                        rendered: None,
                    },
                );
                continue;
            }
        }

        if let Some(diagnostic) = located_diagnostic(trimmed) {
            push_diagnostic(&mut diagnostics, &mut pending, diagnostic);
            continue;
        }

        if let Some(diagnostic) = header_diagnostic(trimmed) {
            if let Some(previous) = pending.replace(diagnostic) {
                diagnostics.push(previous);
            }
        }
    }

    if let Some(diagnostic) = pending {
        diagnostics.push(diagnostic);
    }
    diagnostics
}

/// Pushes a fully located diagnostic, flushing a header that is still awaiting
/// its location first.
///
/// Findings must keep the order the toolchain reported them: a rendered header
/// followed by an unrelated located line would otherwise be reported after it.
fn push_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    pending: &mut Option<Diagnostic>,
    diagnostic: Diagnostic,
) {
    if let Some(previous) = pending.take() {
        diagnostics.push(previous);
    }
    diagnostics.push(diagnostic);
}

/// Reads the next non-blank line, which a wrapped panic report places after its
/// location.
fn next_non_empty<I: Iterator<Item = String>>(lines: &mut I) -> String {
    for line in lines {
        let line = line.trim();
        if !line.is_empty() {
            return line.to_string();
        }
    }
    String::new()
}

/// Builds a diagnostic from a line that carries its own location and severity.
fn located_diagnostic(line: &str) -> Option<Diagnostic> {
    if let Some((file, number, column, tail)) = locate(line) {
        if let Some((severity, code, message)) = severity_after_location(tail) {
            return Some(located(file, number, column, severity, code, message));
        }
    }
    if let Some((file, number, column, tail)) = locate_parenthesized(line) {
        if let Some((severity, code, message)) = severity_after_location(tail) {
            return Some(located(file, number, column, severity, code, message));
        }
    }
    None
}

/// Builds an unlocated diagnostic from a rendered header line.
fn header_diagnostic(line: &str) -> Option<Diagnostic> {
    let (severity, code, message) = parse_header(line)?;
    Some(Diagnostic {
        file: None,
        line: None,
        column: None,
        severity,
        code,
        message,
        rendered: None,
    })
}

/// Assembles a located diagnostic from parsed parts.
fn located(
    file: &str,
    line: u32,
    column: Option<u32>,
    severity: DiagnosticSeverity,
    code: Option<String>,
    message: String,
) -> Diagnostic {
    Diagnostic {
        file: Some(file.to_string()),
        line: Some(line),
        column,
        severity,
        code,
        message,
        rendered: None,
    }
}

/// Splits `severity[code]: message` from what follows a location.
///
/// A toolchain that reports `file:line:col: message` without a level word — as
/// `go vet` and a `go test` failure do — still yields a finding: the tail becomes
/// the message of an [`DiagnosticSeverity::Error`], which is the only severity a
/// failing step can carry. A tail that is empty after the location carries no
/// finding at all.
fn severity_after_location(tail: &str) -> Option<(DiagnosticSeverity, Option<String>, String)> {
    let tail = tail.strip_prefix(':').unwrap_or(tail).trim_start();
    if let Some((severity, code, message)) = parse_header(tail) {
        return Some((severity, code, message));
    }
    (!tail.is_empty()).then(|| (DiagnosticSeverity::Error, None, tail.to_string()))
}

/// Splits a rendered header into severity, optional code, and message.
///
/// A header is recognized only when its first word is a level name, so a prose
/// line that merely starts with a word is never mistaken for a finding.
fn parse_header(line: &str) -> Option<(DiagnosticSeverity, Option<String>, String)> {
    let (word, rest) = split_word(line.trim_start());
    let severity = match word {
        "error" | "warning" | "note" | "help" => DiagnosticSeverity::parse_lossy(word),
        _ => return None,
    };

    let (code, rest) = split_code(rest);
    let (code, rest) = match code {
        Some(code) => (Some(code), rest),
        // A toolchain may write its code without brackets, as `error TS2322: ...`.
        None => split_bare_code(rest.trim_start()),
    };

    let message = rest
        .strip_prefix(':')
        .map(str::trim_start)
        .unwrap_or(rest)
        .trim();
    Some((severity, code, message.to_string()))
}

/// Splits the leading word of a header, stopping at a bracket, a colon, or
/// whitespace.
fn split_word(text: &str) -> (&str, &str) {
    let end = text
        .find(|character: char| character.is_whitespace() || character == '[' || character == ':')
        .unwrap_or(text.len());
    text.split_at(end)
}

/// Splits a bracketed diagnostic code such as `[E0308]`.
fn split_code(rest: &str) -> (Option<String>, &str) {
    let Some(after) = rest.strip_prefix('[') else {
        return (None, rest);
    };
    let Some(end) = after.find(']') else {
        return (None, rest);
    };
    let code = after[..end].trim();
    let code = if code.is_empty() {
        None
    } else {
        Some(code.to_string())
    };
    (code, &after[end + 1..])
}

/// Splits an unbracketed diagnostic code such as `TS2322`, requiring a digit so
/// a prose word before a colon is never read as a code.
fn split_bare_code(rest: &str) -> (Option<String>, &str) {
    let Some(end) = rest.find(':') else {
        return (None, rest);
    };
    let token = rest[..end].trim();
    if token.is_empty()
        || token.chars().any(char::is_whitespace)
        || !token.chars().any(|character| character.is_ascii_digit())
    {
        return (None, rest);
    }
    (Some(token.to_string()), &rest[end + 1..])
}

/// Splits a `path:line[:column]` location from a longer line.
///
/// The path is the whitespace-delimited token that ends at the colon, so prose
/// that merely mentions a location is not mistaken for one, while a toolchain
/// that prefixes its report (`vet: main.go:9:1: ...`) is still located.
fn locate(text: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    for (index, _) in text
        .char_indices()
        .filter(|(_, character)| *character == ':')
    {
        let head = &text[..index];
        // A colon that directly follows whitespace cannot close a path.
        if head.chars().last().is_some_and(char::is_whitespace) {
            continue;
        }
        let path = head.split_whitespace().next_back().unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let Some((number, rest)) = digits_prefix(&text[index + 1..]) else {
            continue;
        };
        let (column, rest) = match rest.strip_prefix(':') {
            Some(after) => match digits_prefix(after) {
                Some((column, rest)) => (Some(column), rest),
                None => (None, rest),
            },
            None => (None, rest),
        };
        return Some((path, number, column, rest));
    }
    None
}

/// Splits a `path(line,column)` location, the form a TypeScript compiler emits.
fn locate_parenthesized(text: &str) -> Option<(&str, u32, Option<u32>, &str)> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let path = text[..open].trim();
    if path.is_empty() || path.chars().any(char::is_whitespace) {
        return None;
    }
    let (number, column) = text[open + 1..close].split_once(',')?;
    let number: u32 = number.trim().parse().ok()?;
    let column: Option<u32> = column.trim().parse().ok();
    Some((path, number, column, &text[close + 1..]))
}

/// Reads an ASCII digit prefix as a number, dropping values a `u32` cannot hold.
fn digits_prefix(text: &str) -> Option<(u32, &str)> {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    let number: u32 = text[..end].parse().ok()?;
    Some((number, &text[end..]))
}

/// Removes ANSI escape sequences from rendered toolchain text.
///
/// Both the CSI sequences used for color and the OSC sequences used for
/// terminal titles are removed, so the retained rendering carries no escape
/// bytes into the model context.
fn strip_ansi(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut characters = text.chars();

    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            stripped.push(character);
            continue;
        }
        match characters.next() {
            Some('[') => {
                for next in characters.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut previous = ' ';
                for next in characters.by_ref() {
                    if next == '\u{7}' || (previous == '\u{1b}' && next == '\\') {
                        break;
                    }
                    previous = next;
                }
            }
            _ => {}
        }
    }

    stripped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structured record whose primary span is the second of two spans.
    const CARGO_ERROR: &str = r#"{"reason":"compiler-message","message":{"rendered":"\u001b[31merror[E0308]\u001b[0m: mismatched types\n","level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/lib.rs","line_start":7,"line_end":7,"column_start":18,"column_end":22,"is_primary":false},{"file_name":"crates/core/src/lib.rs","line_start":9,"line_end":9,"column_start":18,"column_end":23,"is_primary":true}]}}"#;

    /// Asserts that the parse produced exactly one diagnostic and returns it.
    fn only(diagnostics: &[Diagnostic]) -> &Diagnostic {
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        &diagnostics[0]
    }

    #[test]
    fn test_structured_record_is_parsed_from_its_primary_span() {
        let diagnostics = parse_diagnostics(CARGO_ERROR);
        let diagnostic = only(&diagnostics);

        assert_eq!(diagnostic.file.as_deref(), Some("crates/core/src/lib.rs"));
        assert_eq!(diagnostic.line, Some(9));
        assert_eq!(diagnostic.column, Some(18));
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
        assert_eq!(diagnostic.message, "mismatched types");
        assert_eq!(
            diagnostic.rendered.as_deref().unwrap_or_default(),
            "error[E0308]: mismatched types"
        );
    }

    #[test]
    fn test_structured_stream_keeps_rendered_lines() {
        let output = concat!(
            r#"{"reason":"compiler-artifact","package_id":"tauqe-core 0.1.0"}"#,
            "\n",
            "error[E0433]: failed to resolve\n",
            " --> src/main.rs:3:5\n",
        );

        let diagnostics = parse_diagnostics(output);
        let diagnostic = only(&diagnostics);

        assert_eq!(diagnostic.file.as_deref(), Some("src/main.rs"));
        assert_eq!(diagnostic.line, Some(3));
        assert_eq!(diagnostic.column, Some(5));
        assert_eq!(diagnostic.code.as_deref(), Some("E0433"));
        assert_eq!(diagnostic.message, "failed to resolve");
    }

    #[test]
    fn test_rendered_rustc_output_is_parsed() {
        let output = concat!(
            "error[E0308]: mismatched types\n",
            "  --> crates/core/src/lib.rs:7:18\n",
            "   |\n",
            " 7 |     let value: u32 = \"text\";\n",
            "   |                ^^^ expected `u32`, found `&str`\n",
            "\n",
            "note: expected because of this\n",
            " --> crates/core/src/lib.rs:6:9\n",
            "error: could not compile `tauqe-core`\n",
        );

        let diagnostics = parse_diagnostics(output);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
        assert_eq!(
            diagnostics[0].file.as_deref(),
            Some("crates/core/src/lib.rs")
        );
        assert_eq!(diagnostics[0].line, Some(7));
        assert_eq!(diagnostics[0].column, Some(18));
        assert_eq!(diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(diagnostics[1].severity, DiagnosticSeverity::Note);
        assert_eq!(diagnostics[2].file.as_deref(), None);
        assert_eq!(diagnostics[2].message, "could not compile `tauqe-core`");
    }

    #[test]
    fn test_go_and_typescript_lines_are_located() {
        let go = parse_diagnostics(
            "main.go:5:2: undefined: helper\nvet: util.go:9:1: unreachable code\n",
        );
        assert_eq!(go.len(), 2, "{go:#?}");
        assert_eq!(go[0].file.as_deref(), Some("main.go"));
        assert_eq!(go[0].severity, DiagnosticSeverity::Error);
        assert_eq!(go[0].message, "undefined: helper");

        let typescript =
            parse_diagnostics("src/app.ts(12,5): error TS2322: Type 'string' is not assignable.\n");
        let diagnostic = only(&typescript);
        assert_eq!(diagnostic.file.as_deref(), Some("src/app.ts"));
        assert_eq!(diagnostic.line, Some(12));
        assert_eq!(diagnostic.column, Some(5));
        assert_eq!(diagnostic.code.as_deref(), Some("TS2322"));
        assert_eq!(diagnostic.message, "Type 'string' is not assignable.");
    }

    #[test]
    fn test_panic_report_and_ansi_output_are_parsed() {
        let output = concat!(
            "thread 'tests::roundtrip' panicked at crates/core/src/lib.rs:21:9:\n",
            "assertion `left == right` failed\n",
        );
        let diagnostics = parse_diagnostics(output);
        let diagnostic = only(&diagnostics);
        assert_eq!(diagnostic.file.as_deref(), Some("crates/core/src/lib.rs"));
        assert_eq!(diagnostic.line, Some(21));
        assert_eq!(diagnostic.message, "assertion `left == right` failed");

        let coloured =
            parse_diagnostics("\u{1b}[31mmain.go:5:2: error: undefined: helper\u{1b}[0m\n");
        let diagnostic = only(&coloured);
        assert_eq!(diagnostic.file.as_deref(), Some("main.go"));
        assert_eq!(diagnostic.message, "undefined: helper");
    }

    #[test]
    fn test_unrelated_output_yields_no_diagnostics_and_the_list_is_bounded() {
        let output = concat!(
            "# command-line-arguments\n",
            "ok  \ttauqe-core\t0.01s\n",
            "   Compiling tauqe-protocol v0.1.0\n",
            "expected 1 argument\n",
        );
        assert!(parse_diagnostics(output).is_empty());

        let mut many = String::new();
        for index in 0..(MAX_DIAGNOSTICS + 20) {
            many.push_str(&format!("error: failure {index}\n"));
        }
        assert_eq!(parse_diagnostics(&many).len(), MAX_DIAGNOSTICS);
    }
}
