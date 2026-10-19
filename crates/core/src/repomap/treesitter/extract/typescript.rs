//! TypeScript and TSX symbol extraction.
//!
//! Maps functions, classes, interfaces, enums, type aliases, and namespaces
//! onto the canonical symbol kinds. Interfaces map to traits and classes to
//! structs, so the renderer keeps a single, language-agnostic outline. Module
//! visibility follows the `export` keyword exactly as in JavaScript.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, child_of_kinds, collapsed_text, declaration_symbol, is_error_node,
    method_members, signature, signature_with_members, symbol, variable_function_symbols,
};

/// Collects the TypeScript declarations declared at module level.
pub(crate) fn extract_typescript_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_declarations(root, source, false, &mut symbols);
    symbols
}

fn collect_declarations(
    container: Node<'_>,
    source: &[u8],
    exported: bool,
    symbols: &mut Vec<Symbol>,
) {
    for index in 0..container.child_count() {
        if let Some(child) = container.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "export_statement" => collect_declarations(child, source, true, symbols),
                "function_declaration" | "generator_function_declaration" => symbols.push(
                    declaration_symbol(child, source, SymbolKind::Function, "body", exported),
                ),
                "class_declaration" | "abstract_class_declaration" => {
                    symbols.push(class_symbol(child, source, exported));
                }
                "interface_declaration" => symbols.push(interface_symbol(child, source, exported)),
                "enum_declaration" => symbols.push(enum_symbol(child, source, exported)),
                "type_alias_declaration" => {
                    let alias = collapsed_text(child, source);
                    let alias = alias.trim_end_matches(';').trim().to_string();
                    symbols.push(symbol(
                        child,
                        source,
                        SymbolKind::Struct,
                        alias,
                        exported,
                        Vec::new(),
                    ));
                }
                "internal_module" | "module" => {
                    symbols.push(namespace_symbol(child, source, exported))
                }
                "lexical_declaration" | "variable_declaration" => {
                    symbols.extend(variable_function_symbols(child, source, exported));
                }
                _ => {}
            }
        }
    }
}

fn class_symbol(node: Node<'_>, source: &[u8], exported: bool) -> Symbol {
    let body = node.child_by_field_name("body");
    let children = body
        .map(|body| {
            method_members(
                body,
                source,
                &[
                    "method_definition",
                    "method_signature",
                    "abstract_method_signature",
                ],
            )
        })
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Struct,
        signature(node, source, body),
        exported,
        children,
    )
}

fn interface_symbol(node: Node<'_>, source: &[u8], exported: bool) -> Symbol {
    let body = node
        .child_by_field_name("body")
        .or_else(|| child_of_kinds(node, &["object_type"]));
    let children = body
        .map(|body| method_members(body, source, &["method_signature"]))
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Trait,
        signature(node, source, body),
        exported,
        children,
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8], exported: bool) -> Symbol {
    let body = node
        .child_by_field_name("body")
        .or_else(|| child_of_kinds(node, &["enum_body"]));
    let signature = match body {
        Some(body) => {
            let header = signature(node, source, Some(body));
            signature_with_members(&header, &body_members(body, source))
        }
        None => collapsed_text(node, source),
    };
    symbol(
        node,
        source,
        SymbolKind::Enum,
        signature,
        exported,
        Vec::new(),
    )
}

fn namespace_symbol(node: Node<'_>, source: &[u8], exported: bool) -> Symbol {
    let body = node
        .child_by_field_name("body")
        .or_else(|| child_of_kinds(node, &["statement_block"]));
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, exported, &mut children);
    }
    symbol(
        node,
        source,
        SymbolKind::Mod,
        signature(node, source, body),
        exported,
        children,
    )
}
