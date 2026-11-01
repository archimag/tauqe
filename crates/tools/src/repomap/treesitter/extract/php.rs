//! PHP symbol extraction.
//!
//! Maps namespaces, interfaces, traits, classes, enums, functions, and methods
//! onto the canonical symbol kinds. PHP declares visibility through a
//! `visibility_modifier` child; a declaration that carries none is public at
//! program level and private inside a class body.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, body_of, is_error_node, node_text, signature, signature_with_members, symbol,
};

/// Collects the PHP declarations declared at program level.
pub(crate) fn extract_php_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_declarations(root, source, true, &mut symbols);
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
                "function_definition" => symbols.push(callable_symbol(
                    child,
                    source,
                    SymbolKind::Function,
                    default_public,
                )),
                "method_declaration" => symbols.push(callable_symbol(
                    child,
                    source,
                    SymbolKind::Method,
                    default_public,
                )),
                "class_declaration" => symbols.push(type_symbol(
                    child,
                    source,
                    SymbolKind::Struct,
                    default_public,
                )),
                "interface_declaration" | "trait_declaration" => symbols.push(type_symbol(
                    child,
                    source,
                    SymbolKind::Trait,
                    default_public,
                )),
                "enum_declaration" => symbols.push(enum_symbol(child, source, default_public)),
                "namespace_definition" => symbols.push(namespace_symbol(child, source)),
                _ => {}
            }
        }
    }
}

fn namespace_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = body_of(node);
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, true, &mut children);
    }
    symbol(
        node,
        source,
        SymbolKind::Mod,
        signature(node, source, body),
        true,
        children,
    )
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
        is_public(node, source, default_public),
        children,
    )
}

fn callable_symbol(
    node: Node<'_>,
    source: &[u8],
    kind: SymbolKind,
    default_public: bool,
) -> Symbol {
    symbol(
        node,
        source,
        kind,
        signature(node, source, body_of(node)),
        is_public(node, source, default_public),
        Vec::new(),
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8], default_public: bool) -> Symbol {
    let body = body_of(node);
    let header = signature(node, source, body);
    let cases = body
        .map(|body| body_members(body, source))
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Enum,
        signature_with_members(&header, &cases),
        is_public(node, source, default_public),
        Vec::new(),
    )
}

fn is_public(node: Node<'_>, source: &[u8], default_public: bool) -> bool {
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            if child.kind() != "visibility_modifier" {
                continue;
            }
            return node_text(child, source)
                .split_whitespace()
                .any(|word| word == "public");
        }
    }
    default_public
}
