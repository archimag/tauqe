//! JavaScript symbol extraction.
//!
//! Maps functions, classes, and method definitions of the JavaScript grammar
//! (which also parses JSX) onto the canonical symbol kinds. Module-level
//! visibility follows the `export` keyword, so a module-local declaration stays
//! private until it is exported.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    declaration_symbol, is_error_node, method_members, signature, symbol, variable_function_symbols,
};

/// Collects the JavaScript declarations declared at module level.
pub(crate) fn extract_javascript_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
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
                "class_declaration" => symbols.push(class_symbol(child, source, exported)),
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
        .map(|body| method_members(body, source, &["method_definition"]))
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
