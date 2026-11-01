//! Rust symbol extraction.
//!
//! Maps module, struct, enum, trait, impl, and function items onto the
//! canonical symbol kinds. Rust declares visibility explicitly, so the
//! `visibility_modifier` child is the single source of truth for `pub`.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, child_of_kinds, collapsed_text, declaration_symbol, has_child_kind,
    is_error_node, signature, signature_with_members, symbol,
};

/// Collects the Rust items declared in `container`.
pub(crate) fn extract_rust_symbols(container: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for index in 0..container.child_count() {
        if let Some(child) = container.child(index) {
            if is_error_node(child) {
                continue;
            }
            let extracted = match child.kind() {
                "mod_item" => Some(mod_symbol(child, source)),
                "struct_item" => Some(struct_symbol(child, source)),
                "enum_item" => Some(enum_symbol(child, source)),
                "trait_item" => Some(trait_symbol(child, source)),
                "impl_item" => Some(impl_symbol(child, source)),
                "function_item" => Some(declaration_symbol(
                    child,
                    source,
                    SymbolKind::Function,
                    "body",
                    is_visible(child),
                )),
                _ => None,
            };
            if let Some(symbol) = extracted {
                symbols.push(symbol);
            }
        }
    }
    symbols
}

fn is_visible(node: Node<'_>) -> bool {
    has_child_kind(node, "visibility_modifier")
}

fn mod_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = child_of_kinds(node, &["declaration_list"]);
    let children = body
        .map(|body| extract_rust_symbols(body, source))
        .unwrap_or_default();
    symbol(
        node,
        source,
        SymbolKind::Mod,
        signature(node, source, body),
        is_visible(node),
        children,
    )
}

fn struct_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = child_of_kinds(
        node,
        &["field_declaration_list", "ordered_field_declaration_list"],
    );
    let signature = match body {
        Some(body) if body.kind() == "field_declaration_list" => {
            let header = signature(node, source, Some(body));
            signature_with_members(&header, &body_members(body, source))
        }
        // Tuple and unit structs keep their payload inside the header itself.
        _ => collapsed_text(node, source)
            .trim_end_matches(';')
            .trim()
            .to_string(),
    };
    symbol(
        node,
        source,
        SymbolKind::Struct,
        signature,
        is_visible(node),
        Vec::new(),
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = child_of_kinds(node, &["enum_variant_list"]);
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
        is_visible(node),
        Vec::new(),
    )
}

fn trait_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = child_of_kinds(node, &["declaration_list"]);
    let children = body_methods(body, source, true);
    symbol(
        node,
        source,
        SymbolKind::Trait,
        signature(node, source, body),
        is_visible(node),
        children,
    )
}

fn impl_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = child_of_kinds(node, &["declaration_list"]);
    let header = signature(node, source, body);
    let is_trait_impl = node.child_by_field_name("trait").is_some()
        || (0..node.child_count())
            .filter_map(|index| node.child(index))
            .any(|child| child.kind() == "for");
    Symbol {
        name: header.clone(),
        kind: SymbolKind::Impl,
        signature: Some(header),
        is_public: true,
        line: node.start_position().row + 1,
        children: body_methods(body, source, is_trait_impl),
    }
}

/// Collects the methods of a trait or impl body; trait members are always public.
fn body_methods(body: Option<Node<'_>>, source: &[u8], trait_like: bool) -> Vec<Symbol> {
    let body = match body {
        Some(body) => body,
        None => return Vec::new(),
    };
    let mut methods = Vec::new();
    for index in 0..body.child_count() {
        if let Some(item) = body.child(index) {
            if matches!(item.kind(), "function_item" | "function_signature_item") {
                let is_public = trait_like || is_visible(item);
                methods.push(declaration_symbol(
                    item,
                    source,
                    SymbolKind::Method,
                    "body",
                    is_public,
                ));
            }
        }
    }
    methods
}
