//! Python symbol extraction.
//!
//! Maps classes, functions, methods, and decorated definitions onto the
//! canonical symbol kinds. Python declares visibility by naming convention: a
//! leading underscore marks a private name, so the name itself is the single
//! source of truth.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    declaration_symbol, is_error_node, is_public_name, method_members, signature, symbol,
};

/// Collects the Python definitions declared at module level.
pub(crate) fn extract_python_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_definitions(root, source, &mut symbols);
    symbols
}

fn collect_definitions(container: Node<'_>, source: &[u8], symbols: &mut Vec<Symbol>) {
    for index in 0..container.child_count() {
        if let Some(child) = container.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "decorated_definition" => collect_definitions(child, source, symbols),
                "class_definition" => symbols.push(class_symbol(child, source)),
                "function_definition" => symbols.push(declaration_symbol(
                    child,
                    source,
                    SymbolKind::Function,
                    "body",
                    is_public_name(child, source),
                )),
                _ => {}
            }
        }
    }
}

fn class_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = node.child_by_field_name("body");
    let children = body
        .map(|body| method_members(body, source, &["function_definition"]))
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Struct,
        signature(node, source, body),
        is_public_name(node, source),
        children,
    )
}
