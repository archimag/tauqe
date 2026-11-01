//! C# symbol extraction.
//!
//! Maps namespaces, classes, structs, records, interfaces, enums, and their
//! members onto the canonical symbol kinds. C# declares visibility explicitly
//! through `modifier` children, so those nodes are the single source of truth
//! for `public`; interface members are public by declaration.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, body_of, is_error_node, node_text, signature, signature_with_members, symbol,
};

/// Collects the C# declarations declared in a compilation unit.
pub(crate) fn extract_c_sharp_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
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
                "namespace_declaration" => symbols.push(namespace_symbol(child, source)),
                "class_declaration" | "struct_declaration" | "record_declaration" => symbols.push(
                    type_symbol(child, source, SymbolKind::Struct, default_public),
                ),
                "interface_declaration" => symbols.push(type_symbol(
                    child,
                    source,
                    SymbolKind::Trait,
                    default_public,
                )),
                "enum_declaration" => symbols.push(enum_symbol(child, source, default_public)),
                "method_declaration"
                | "constructor_declaration"
                | "destructor_declaration"
                | "operator_declaration" => {
                    symbols.push(method_symbol(child, source, default_public))
                }
                _ => {}
            }
        }
    }
}

fn namespace_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = body_of(node);
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, false, &mut children);
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

fn method_symbol(node: Node<'_>, source: &[u8], default_public: bool) -> Symbol {
    symbol(
        node,
        source,
        SymbolKind::Method,
        signature(node, source, body_of(node)),
        is_public(node, source, default_public),
        Vec::new(),
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8], default_public: bool) -> Symbol {
    let body = body_of(node);
    let header = signature(node, source, body);
    let members = body
        .map(|body| body_members(body, source))
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Enum,
        signature_with_members(&header, &members),
        is_public(node, source, default_public),
        Vec::new(),
    )
}

fn is_public(node: Node<'_>, source: &[u8], default_public: bool) -> bool {
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            if child.kind() == "modifier" && node_text(child, source) == "public" {
                return true;
            }
        }
    }
    default_public
}
