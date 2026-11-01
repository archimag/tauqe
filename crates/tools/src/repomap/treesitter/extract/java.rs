//! Java symbol extraction.
//!
//! Maps classes, records, annotation types, interfaces, enums, and their
//! methods onto the canonical symbol kinds. Java declares visibility
//! explicitly through a `modifiers` child, so that node is the single source of
//! truth for `public`; interface members are public by declaration.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_of, collapsed_text, is_error_node, signature, signature_with_members, symbol,
};

/// Collects the Java declarations declared at compilation-unit level.
pub(crate) fn extract_java_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_declarations(root, source, false, &mut symbols);
    symbols
}

fn collect_declarations(
    container: Node<'_>,
    source: &[u8],
    default_public: bool,
    symbols: &mut Vec<Symbol>,
) {
    for index in 0..container.child_count() {
        if let Some(child) = container.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "class_declaration" | "record_declaration" | "annotation_type_declaration" => {
                    symbols.push(type_symbol(
                        child,
                        source,
                        SymbolKind::Struct,
                        default_public,
                    ))
                }
                "interface_declaration" => symbols.push(type_symbol(
                    child,
                    source,
                    SymbolKind::Trait,
                    default_public,
                )),
                "enum_declaration" => symbols.push(enum_symbol(child, source, default_public)),
                "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration"
                | "annotation_type_element_declaration" => {
                    symbols.push(method_symbol(child, source, default_public))
                }
                _ => {}
            }
        }
    }
}

fn type_symbol(node: Node<'_>, source: &[u8], kind: SymbolKind, default_public: bool) -> Symbol {
    let body = body_of(node);
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, kind == SymbolKind::Trait, &mut children);
    }
    symbol(
        node,
        source,
        kind,
        signature(node, source, body),
        is_public(node, default_public),
        children,
    )
}

fn method_symbol(node: Node<'_>, source: &[u8], default_public: bool) -> Symbol {
    symbol(
        node,
        source,
        SymbolKind::Method,
        signature(node, source, body_of(node)),
        is_public(node, default_public),
        Vec::new(),
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8], default_public: bool) -> Symbol {
    let body = body_of(node);
    let header = signature(node, source, body);
    let constants = body.map(|body| constants(body, source)).unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Enum,
        signature_with_members(&header, &constants),
        is_public(node, default_public),
        Vec::new(),
    )
}

/// Renders the named constants declared by an enum body.
fn constants(body: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut constants = Vec::new();
    for index in 0..body.child_count() {
        if let Some(child) = body.child(index) {
            if child.kind() == "enum_constant" {
                constants.push(collapsed_text(child, source));
            }
        }
    }
    constants
}

fn is_public(node: Node<'_>, default_public: bool) -> bool {
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            if child.kind() != "modifiers" {
                continue;
            }
            for inner in 0..child.child_count() {
                if let Some(token) = child.child(inner) {
                    if token.kind() == "public" {
                        return true;
                    }
                }
            }
        }
    }
    default_public
}
