//! Node helpers shared by the per-language symbol extractors.
//!
//! The helpers below implement everything that does not depend on a grammar:
//! reading a declared name, collapsing a declaration header to a single line,
//! and collecting body members as indented signature lines. Grammar-specific
//! node kinds and visibility rules stay in the language submodules.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

/// Punctuation nodes that never become signature members.
const PUNCTUATION: &[&str] = &["{", "}", "(", ")", "[", "]", ",", ";", ":"];

/// Returns the node's source text, or an empty string when it is not UTF-8.
pub(super) fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Collapses every whitespace run into one space and trims the result.
pub(super) fn collapse_whitespace(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(character);
            in_whitespace = false;
        }
    }
    collapsed.trim().to_string()
}

/// Renders a node as a single collapsed line.
pub(super) fn collapsed_text(node: Node<'_>, source: &[u8]) -> String {
    collapse_whitespace(node_text(node, source))
}

/// Returns the declared name of `node`, falling back to its first identifier child.
pub(super) fn node_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if let Some(name) = node.child_by_field_name("name") {
        return name.utf8_text(source).ok();
    }
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            if matches!(
                child.kind(),
                "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
            ) {
                return child.utf8_text(source).ok();
            }
        }
    }
    None
}

/// Returns whether a declared name is public: no private prefix by convention.
pub(super) fn is_public_name(node: Node<'_>, source: &[u8]) -> bool {
    match node_name(node, source) {
        Some(name) => !name.starts_with('_') && !name.starts_with('#'),
        None => false,
    }
}

/// Returns the first direct child whose kind is listed.
pub(super) fn child_of_kinds<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .find(|child| kinds.contains(&child.kind()))
}

/// Returns whether the node holds a direct child of `kind`.
pub(super) fn has_child_kind(node: Node<'_>, kind: &str) -> bool {
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .any(|child| child.kind() == kind)
}

/// Node kinds that declare the members of a type or a scope.
const BODY_KINDS: &[&str] = &[
    "annotation_type_body",
    "arrow_expression_clause",
    "block",
    "class_body",
    "compound_statement",
    "declaration_list",
    "enum_body",
    "enum_declaration_list",
    "enum_member_declaration_list",
    "field_declaration_list",
    "interface_body",
    "statement_block",
];

/// Returns the declaration body: the `body` field, else a known container kind.
pub(super) fn body_of(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
        .or_else(|| child_of_kinds(node, BODY_KINDS))
}

/// Returns whether the node is a syntax error placeholder.
pub(super) fn is_error_node(node: Node<'_>) -> bool {
    node.is_error() || node.kind() == "ERROR"
}

/// Renders a declaration header, ending where its body starts.
pub(super) fn signature(node: Node<'_>, source: &[u8], body: Option<Node<'_>>) -> String {
    let start = node.start_byte();
    let end = body.map_or_else(|| node.end_byte(), |body| body.start_byte());
    if start > end || end > source.len() {
        return collapsed_text(node, source);
    }
    collapse_whitespace(&String::from_utf8_lossy(&source[start..end]))
        .trim_end_matches(&['{', ':', ';', '='][..])
        .trim()
        .to_string()
}

/// Collects the members of a body node as collapsed lines without trailing commas.
pub(super) fn body_members(body: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut members = Vec::new();
    for index in 0..body.child_count() {
        if let Some(child) = body.child(index) {
            if is_error_node(child) || PUNCTUATION.contains(&child.kind()) {
                continue;
            }
            let member = collapse_whitespace(node_text(child, source));
            let member = member.trim_end_matches(',').trim();
            if !member.is_empty() {
                members.push(member.to_string());
            }
        }
    }
    members
}

/// Renders a header followed by its members as an indented brace block.
pub(super) fn signature_with_members(header: &str, members: &[String]) -> String {
    if members.is_empty() {
        return header.to_string();
    }
    let rendered = members
        .iter()
        .map(|member| format!("    {},", member))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{} {{\n{}\n}}", header, rendered)
}

/// Assembles a symbol from an already rendered signature.
pub(super) fn symbol(
    node: Node<'_>,
    source: &[u8],
    kind: SymbolKind,
    signature: String,
    is_public: bool,
    children: Vec<Symbol>,
) -> Symbol {
    Symbol {
        name: node_name(node, source).unwrap_or("anonymous").to_string(),
        kind,
        signature: Some(signature),
        is_public,
        line: node.start_position().row + 1,
        children,
    }
}

/// Builds a symbol from a declaration whose signature ends at `body_field`.
pub(super) fn declaration_symbol(
    node: Node<'_>,
    source: &[u8],
    kind: SymbolKind,
    body_field: &str,
    is_public: bool,
) -> Symbol {
    let signature = signature(node, source, node.child_by_field_name(body_field));
    symbol(node, source, kind, signature, is_public, Vec::new())
}

/// Collects method symbols declared in a class body.
pub(super) fn method_members(body: Node<'_>, source: &[u8], kinds: &[&str]) -> Vec<Symbol> {
    let mut methods = Vec::new();
    for index in 0..body.child_count() {
        if let Some(child) = body.child(index) {
            if is_error_node(child) || !kinds.contains(&child.kind()) {
                continue;
            }
            let signature = signature(child, source, child.child_by_field_name("body"));
            methods.push(symbol(
                child,
                source,
                SymbolKind::Method,
                signature,
                is_public_name(child, source),
                Vec::new(),
            ));
        }
    }
    methods
}

/// Collects function-valued variable declarators such as `const run = () => ...`.
pub(super) fn variable_function_symbols(
    declaration: Node<'_>,
    source: &[u8],
    is_public: bool,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for index in 0..declaration.child_count() {
        if let Some(declarator) = declaration.child(index) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let value = match declarator.child_by_field_name("value") {
                Some(value) => value,
                None => continue,
            };
            if !matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "function" | "generator_function"
            ) {
                continue;
            }
            let header = signature(declarator, source, Some(value));
            symbols.push(symbol(
                declarator,
                source,
                SymbolKind::Function,
                header,
                is_public,
                Vec::new(),
            ));
        }
    }
    symbols
}
