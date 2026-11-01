//! C and C++ symbol extraction.
//!
//! Both grammars share the translation-unit, declarator, and aggregate shapes,
//! so one extractor serves them. A declaration has no name field: its name is
//! reached through the nested declarators of the declaration, which is why the
//! name resolution below walks the declarator chain instead of reading a field.
//! Visibility is by naming convention only, since neither language marks an
//! exported declaration.

use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

use super::common::{
    body_members, body_of, child_of_kinds, collapsed_text, has_child_kind, is_error_node,
    node_name, signature, signature_with_members, symbol,
};

/// Collects the C and C++ declarations declared at translation-unit level.
pub(crate) fn extract_c_family_symbols(root: Node<'_>, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    collect_declarations(root, source, SymbolKind::Function, &mut symbols);
    symbols
}

fn collect_declarations(
    container: Node<'_>,
    source: &[u8],
    function_kind: SymbolKind,
    symbols: &mut Vec<Symbol>,
) {
    for index in 0..container.child_count() {
        if let Some(child) = container.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "function_definition" => {
                    symbols.push(function_symbol(child, source, function_kind))
                }
                "declaration" | "field_declaration" => {
                    collect_declaration(child, source, function_kind, symbols)
                }
                "class_specifier" | "struct_specifier" | "union_specifier" => {
                    symbols.push(aggregate_symbol(child, source))
                }
                "enum_specifier" => symbols.push(enum_symbol(child, source)),
                "namespace_definition" => symbols.push(namespace_symbol(child, source)),
                "type_definition" | "alias_declaration" => {
                    symbols.push(typedef_symbol(child, source))
                }
                "linkage_specification" | "template_declaration" | "declaration_list" => {
                    collect_declarations(child, source, function_kind, symbols)
                }
                _ => {}
            }
        }
    }
}

/// Handles a declaration statement: the named aggregates it defines and, when
/// it only announces a prototype, the function it declares.
fn collect_declaration(
    node: Node<'_>,
    source: &[u8],
    function_kind: SymbolKind,
    symbols: &mut Vec<Symbol>,
) {
    let mut defined_type = false;
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index) {
            if is_error_node(child) {
                continue;
            }
            match child.kind() {
                "class_specifier" | "struct_specifier" | "union_specifier" => {
                    if body_of(child).is_some() {
                        symbols.push(aggregate_symbol(child, source));
                        defined_type = true;
                    }
                }
                "enum_specifier" if body_of(child).is_some() => {
                    symbols.push(enum_symbol(child, source));
                    defined_type = true;
                }
                _ => {}
            }
        }
    }
    if defined_type || !has_child_kind(node, "function_declarator") {
        return;
    }
    let name = match declaration_name(node, source) {
        Some(name) => name,
        None => return,
    };
    symbols.push(declared_symbol(node, source, function_kind, name));
}

/// Resolves the declared name of a function, prototype, or method.
fn declaration_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("declarator")
        .and_then(|declarator| innermost_name(declarator, source))
        .or_else(|| {
            child_of_kinds(node, &["function_declarator"])
                .and_then(|declarator| innermost_name(declarator, source))
        })
}

/// Walks a declarator chain down to the declared identifier.
fn innermost_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    if matches!(
        node.kind(),
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name"
    ) {
        return node.utf8_text(source).ok();
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        if let Some(name) = innermost_name(declarator, source) {
            return Some(name);
        }
    }
    (0..node.child_count())
        .filter_map(|index| node.child(index))
        .find_map(|child| innermost_name(child, source))
}

fn function_symbol(node: Node<'_>, source: &[u8], kind: SymbolKind) -> Symbol {
    match declaration_name(node, source) {
        Some(name) => declared_symbol(node, source, kind, name),
        None => {
            let text = collapsed_text(node, source);
            symbol(node, source, kind, text, false, Vec::new())
        }
    }
}

fn declared_symbol(node: Node<'_>, source: &[u8], kind: SymbolKind, name: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind,
        signature: Some(signature(node, source, body_of(node))),
        is_public: is_public_name(name),
        line: node.start_position().row + 1,
        children: Vec::new(),
    }
}

fn aggregate_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = body_of(node);
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, SymbolKind::Method, &mut children);
    }
    let header = signature(node, source, body);
    // Member lines are rendered only when the aggregate declares no nested
    // symbols, so a method never appears both inside the block and below it.
    let signature = match (body, children.is_empty()) {
        (Some(body), true) => signature_with_members(&header, &body_members(body, source)),
        _ => header,
    };
    symbol(
        node,
        source,
        SymbolKind::Struct,
        signature,
        declaration_visibility(node, source),
        children,
    )
}

fn enum_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = body_of(node);
    let header = signature(node, source, body);
    let signature = match body {
        Some(body) => signature_with_members(&header, &body_members(body, source)),
        None => header,
    };
    symbol(
        node,
        source,
        SymbolKind::Enum,
        signature,
        declaration_visibility(node, source),
        Vec::new(),
    )
}

fn namespace_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let body = body_of(node);
    let mut children = Vec::new();
    if let Some(body) = body {
        collect_declarations(body, source, SymbolKind::Function, &mut children);
    }
    symbol(
        node,
        source,
        SymbolKind::Mod,
        signature(node, source, body),
        declaration_visibility(node, source),
        children,
    )
}

fn typedef_symbol(node: Node<'_>, source: &[u8]) -> Symbol {
    let declaration = collapsed_text(node, source);
    let declaration = declaration.trim_end_matches(';').trim().to_string();
    symbol(
        node,
        source,
        SymbolKind::Struct,
        declaration,
        declaration_visibility(node, source),
        Vec::new(),
    )
}

fn declaration_visibility(node: Node<'_>, source: &[u8]) -> bool {
    match node_name(node, source) {
        Some(name) => is_public_name(name),
        None => true,
    }
}

fn is_public_name(name: &str) -> bool {
    !name.starts_with('_')
}
