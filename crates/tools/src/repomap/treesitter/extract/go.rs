//! Go symbol extraction.
//!
//! Maps functions, methods, struct types, interface types, and other type
//! declarations onto the canonical symbol kinds. Go declares visibility by
//! naming convention: an upper-case initial marks an exported name.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, child_of_kinds, collapsed_text, declaration_symbol, is_error_node, node_name,
    signature, signature_with_members, symbol,
};

/// Collects the Go declarations declared at file level.
pub(crate) fn extract_go_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for index in 0..root.child_count() {
        if let Some(child) = root.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "function_declaration" => symbols.push(declaration_symbol(
                    child,
                    source,
                    SymbolKind::Function,
                    "body",
                    is_exported(child, source),
                )),
                "method_declaration" => symbols.push(declaration_symbol(
                    child,
                    source,
                    SymbolKind::Method,
                    "body",
                    is_exported(child, source),
                )),
                "type_declaration" => collect_type_declarations(child, source, &mut symbols),
                _ => {}
            }
        }
    }
    symbols
}

fn collect_type_declarations(declaration: Node<'_>, source: &[u8], symbols: &mut Vec<Symbol>) {
    for index in 0..declaration.child_count() {
        if let Some(child) = declaration.child(index) {
            if child.kind() == "type_spec" {
                symbols.push(type_symbol(child, source));
            }
        }
    }
}

fn type_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let is_public = is_exported(node, source);
    if let Some(struct_type) = child_of_kinds(node, &["struct_type"]) {
        let fields = child_of_kinds(struct_type, &["field_declaration_list"]);
        let header = signature(node, source, fields);
        let signature = match fields {
            Some(fields) => signature_with_members(&header, &body_members(fields, source)),
            None => header,
        };
        return symbol(
            node,
            source,
            SymbolKind::Struct,
            signature,
            is_public,
            Vec::new(),
        );
    }
    if let Some(interface_type) = child_of_kinds(node, &["interface_type"]) {
        return symbol(
            node,
            source,
            SymbolKind::Trait,
            signature(node, source, Some(interface_type)),
            is_public,
            Vec::new(),
        );
    }
    symbol(
        node,
        source,
        SymbolKind::Struct,
        collapsed_text(node, source),
        is_public,
        Vec::new(),
    )
}

fn is_exported(node: Node<'_>, source: &[u8]) -> bool {
    match node_name(node, source) {
        Some(name) => name
            .chars()
            .next()
            .map(|first| first.is_uppercase())
            .unwrap_or(false),
        None => false,
    }
}
