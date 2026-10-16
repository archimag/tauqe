use tree_sitter::Node;

use crate::repomap::{Symbol, SymbolKind};

pub(super) fn extract_container_symbols(container: Node, source: &[u8]) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for i in 0..container.child_count() {
        let child = match container.child(i) {
            Some(c) => c,
            None => continue,
        };
        // Skip syntax error nodes gracefully without failing
        if child.is_error() || child.kind() == "ERROR" {
            continue;
        }

        match child.kind() {
            "mod_item" => symbols.push(extract_mod_symbol(child, source)),
            "struct_item" => symbols.push(extract_struct_symbol(child, source)),
            "enum_item" => symbols.push(extract_enum_symbol(child, source)),
            "trait_item" => symbols.push(extract_trait_symbol(child, source)),
            "impl_item" => symbols.push(extract_impl_symbol(child, source)),
            "function_item" => symbols.push(extract_function_symbol(child, source)),
            _ => {}
        }
    }
    symbols
}

fn has_visibility(node: Node) -> bool {
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" {
                return true;
            }
        }
    }
    false
}

fn get_node_name<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok();
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "identifier" || child.kind() == "type_identifier" {
                return child.utf8_text(source).ok();
            }
        }
    }
    None
}

fn collapse_whitespace(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                res.push(' ');
                in_ws = true;
            }
        } else {
            res.push(ch);
            in_ws = false;
        }
    }
    res.trim().to_string()
}

fn extract_fn_signature(node: Node, source: &[u8]) -> String {
    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            match child.kind() {
                "visibility_modifier" | "function_modifiers" | "fn" => {
                    start_byte = child.start_byte();
                    break;
                }
                _ => {}
            }
        }
    }

    let end_byte = if let Some(body) = node.child_by_field_name("body") {
        body.start_byte()
    } else {
        node.end_byte()
    };

    if start_byte <= end_byte && end_byte <= source.len() {
        String::from_utf8_lossy(&source[start_byte..end_byte])
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string()
    } else {
        node.utf8_text(source).unwrap_or("").trim().to_string()
    }
}

fn extract_struct_symbol(node: Node, source: &[u8]) -> Symbol {
    let is_pub = has_visibility(node);
    let name = get_node_name(node, source).unwrap_or("AnonymousStruct");
    let line = node.start_position().row + 1;

    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" || child.kind() == "struct" {
                start_byte = child.start_byte();
                break;
            }
        }
    }

    let mut field_lines = Vec::new();
    let mut header_end = node.end_byte();

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "field_declaration_list" {
                header_end = child.start_byte();
                for j in 0..child.child_count() {
                    if let Some(field) = child.child(j) {
                        if field.kind() == "field_declaration" {
                            let mut f_start = field.start_byte();
                            for k in 0..field.child_count() {
                                if let Some(fc) = field.child(k) {
                                    if fc.kind() == "visibility_modifier"
                                        || fc.kind() == "field_identifier"
                                    {
                                        f_start = fc.start_byte();
                                        break;
                                    }
                                }
                            }
                            let f_text = String::from_utf8_lossy(&source[f_start..field.end_byte()])
                                .trim()
                                .trim_end_matches(',')
                                .trim()
                                .to_string();
                            if !f_text.is_empty() {
                                field_lines.push(collapse_whitespace(&f_text));
                            }
                        }
                    }
                }
                break;
            } else if child.kind() == "ordered_field_declaration_list" {
                header_end = child.end_byte();
                break;
            }
        }
    }

    let header = if start_byte <= header_end && header_end <= source.len() {
        collapse_whitespace(&String::from_utf8_lossy(&source[start_byte..header_end]))
    } else {
        format!("struct {}", name)
    };

    let signature = if !field_lines.is_empty() {
        format!(
            "{} {{\n{}\n}}",
            header,
            field_lines
                .iter()
                .map(|f| format!("    {},", f))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        header.trim_end_matches(';').trim().to_string()
    };

    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Struct,
        signature: Some(signature),
        is_public: is_pub,
        line,
        children: Vec::new(),
    }
}

fn extract_enum_symbol(node: Node, source: &[u8]) -> Symbol {
    let is_pub = has_visibility(node);
    let name = get_node_name(node, source).unwrap_or("AnonymousEnum");
    let line = node.start_position().row + 1;

    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" || child.kind() == "enum" {
                start_byte = child.start_byte();
                break;
            }
        }
    }

    let mut variant_lines = Vec::new();
    let mut header_end = node.end_byte();

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "enum_variant_list" {
                header_end = child.start_byte();
                for j in 0..child.child_count() {
                    if let Some(variant) = child.child(j) {
                        if variant.kind() == "enum_variant" {
                            let v_text = variant
                                .utf8_text(source)
                                .unwrap_or("")
                                .trim()
                                .trim_end_matches(',')
                                .trim();
                            if !v_text.is_empty() {
                                variant_lines.push(collapse_whitespace(v_text));
                            }
                        }
                    }
                }
                break;
            }
        }
    }

    let header = if start_byte <= header_end && header_end <= source.len() {
        collapse_whitespace(&String::from_utf8_lossy(&source[start_byte..header_end]))
    } else {
        format!("enum {}", name)
    };

    let signature = if !variant_lines.is_empty() {
        format!(
            "{} {{\n{}\n}}",
            header,
            variant_lines
                .iter()
                .map(|v| format!("    {},", v))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        header
    };

    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Enum,
        signature: Some(signature),
        is_public: is_pub,
        line,
        children: Vec::new(),
    }
}

fn extract_trait_symbol(node: Node, source: &[u8]) -> Symbol {
    let is_pub = has_visibility(node);
    let name = get_node_name(node, source).unwrap_or("AnonymousTrait");
    let line = node.start_position().row + 1;

    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" || child.kind() == "trait" {
                start_byte = child.start_byte();
                break;
            }
        }
    }

    let mut header_end = node.end_byte();
    let mut methods = Vec::new();

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "declaration_list" {
                header_end = child.start_byte();
                for j in 0..child.child_count() {
                    if let Some(item) = child.child(j) {
                        if item.kind() == "function_item"
                            || item.kind() == "function_signature_item"
                        {
                            let m_name = get_node_name(item, source).unwrap_or("anon");
                            let m_line = item.start_position().row + 1;
                            let m_sig = extract_fn_signature(item, source);
                            methods.push(Symbol {
                                name: m_name.to_string(),
                                kind: SymbolKind::Method,
                                signature: Some(collapse_whitespace(&m_sig)),
                                is_public: true,
                                line: m_line,
                                children: Vec::new(),
                            });
                        }
                    }
                }
                break;
            }
        }
    }

    let header = if start_byte <= header_end && header_end <= source.len() {
        collapse_whitespace(&String::from_utf8_lossy(&source[start_byte..header_end]))
    } else {
        format!("trait {}", name)
    };

    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Trait,
        signature: Some(header),
        is_public: is_pub,
        line,
        children: methods,
    }
}

fn extract_impl_symbol(node: Node, source: &[u8]) -> Symbol {
    let line = node.start_position().row + 1;
    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "impl" {
                start_byte = child.start_byte();
                break;
            }
        }
    }

    let is_trait_impl = node.child_by_field_name("trait").is_some()
        || (0..node.child_count()).any(|i| node.child(i).map(|c| c.kind() == "for").unwrap_or(false));

    let mut header_end = node.end_byte();
    let mut methods = Vec::new();

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "declaration_list" {
                header_end = child.start_byte();
                for j in 0..child.child_count() {
                    if let Some(item) = child.child(j) {
                        if item.kind() == "function_item" {
                            let m_is_pub = is_trait_impl || has_visibility(item);
                            let m_name = get_node_name(item, source).unwrap_or("anon");
                            let m_line = item.start_position().row + 1;
                            let m_sig = extract_fn_signature(item, source);
                            methods.push(Symbol {
                                name: m_name.to_string(),
                                kind: SymbolKind::Method,
                                signature: Some(collapse_whitespace(&m_sig)),
                                is_public: m_is_pub,
                                line: m_line,
                                children: Vec::new(),
                            });
                        }
                    }
                }
                break;
            }
        }
    }

    let header = if start_byte <= header_end && header_end <= source.len() {
        collapse_whitespace(&String::from_utf8_lossy(&source[start_byte..header_end]))
    } else {
        "impl".to_string()
    };

    Symbol {
        name: header.clone(),
        kind: SymbolKind::Impl,
        signature: Some(header),
        is_public: true,
        line,
        children: methods,
    }
}

fn extract_mod_symbol(node: Node, source: &[u8]) -> Symbol {
    let is_pub = has_visibility(node);
    let name = get_node_name(node, source).unwrap_or("anon_mod");
    let line = node.start_position().row + 1;

    let mut start_byte = node.start_byte();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "visibility_modifier" || child.kind() == "mod" {
                start_byte = child.start_byte();
                break;
            }
        }
    }

    let mut header_end = node.end_byte();
    let mut children = Vec::new();

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            if child.kind() == "declaration_list" {
                header_end = child.start_byte();
                children = extract_container_symbols(child, source);
                break;
            }
        }
    }

    let header = if start_byte <= header_end && header_end <= source.len() {
        collapse_whitespace(&String::from_utf8_lossy(&source[start_byte..header_end]))
            .trim_end_matches(';')
            .trim()
            .to_string()
    } else {
        format!("mod {}", name)
    };

    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Mod,
        signature: Some(header),
        is_public: is_pub,
        line,
        children,
    }
}

fn extract_function_symbol(node: Node, source: &[u8]) -> Symbol {
    let is_pub = has_visibility(node);
    let name = get_node_name(node, source).unwrap_or("anon_fn");
    let line = node.start_position().row + 1;
    let sig = extract_fn_signature(node, source);

    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        signature: Some(collapse_whitespace(&sig)),
        is_public: is_pub,
        line,
        children: Vec::new(),
    }
}
