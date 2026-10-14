use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Result;
use tree_sitter::{Node, Parser};

use crate::repomap::{RepoFileSymbols, Symbol, SymbolExtractor, SymbolKind};

/// Extracts top-level symbols from a parsed syntax tree.
type SymbolExtraction = for<'tree, 'source> fn(Node<'tree>, &'source [u8]) -> Vec<Symbol>;

/// Tree-sitter language support entry covering a set of file extensions.
struct LanguageSupport {
    extensions: &'static [&'static str],
    language: fn() -> tree_sitter::Language,
    extract: SymbolExtraction,
}

/// Registry of languages the repo map can parse.
///
/// The map stays language-agnostic: supporting a new language means adding an
/// entry here with its grammar and top-level symbol extractor. Files without a
/// registered extension still appear in the map as plain paths, which is the
/// expected fallback for languages without symbol support.
const LANGUAGE_REGISTRY: &[LanguageSupport] = &[LanguageSupport {
    extensions: &["rs"],
    language: tree_sitter_rust::language,
    extract: extract_container_symbols,
}];

/// Returns whether `path` carries an extension the registry can parse.
pub fn is_supported_path(path: &str) -> bool {
    language_for_path(path).is_some()
}

/// Returns the registry entry registered for `path`'s extension, if any.
fn language_for_path(path: &str) -> Option<&'static LanguageSupport> {
    let extension = Path::new(path).extension()?.to_str()?;
    LANGUAGE_REGISTRY
        .iter()
        .find(|support| support.extensions.contains(&extension))
}

/// Symbols cached for a single file, validated against its modification state.
struct CachedSymbols {
    modified: Option<SystemTime>,
    size: u64,
    symbols: Option<RepoFileSymbols>,
}

/// Reusable parser and the symbol cache it fills.
struct ExtractorState {
    parser: Parser,
    cache: HashMap<PathBuf, CachedSymbols>,
}

/// Default repository symbol extractor backed by Tree-sitter.
///
/// Holds one reusable parser and an mtime/size-validated symbol cache. The map
/// is regenerated whenever the active context changes, which would otherwise
/// re-parse every file outside that context on each round; the cache turns
/// those passes into lookups for files whose modification time and size are
/// unchanged.
pub struct TreeSitterExtractor {
    state: Mutex<ExtractorState>,
}

impl Default for TreeSitterExtractor {
    fn default() -> Self {
        Self {
            state: Mutex::new(ExtractorState {
                parser: Parser::new(),
                cache: HashMap::new(),
            }),
        }
    }
}

impl std::fmt::Debug for TreeSitterExtractor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TreeSitterExtractor")
            .finish_non_exhaustive()
    }
}

impl TreeSitterExtractor {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SymbolExtractor for TreeSitterExtractor {
    fn extract_symbols(&self, repo_root: &Path, files: &[String]) -> Result<Vec<RepoFileSymbols>> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(extract_treesitter_symbols(&mut state, repo_root, files))
    }
}

/// Extracts symbols from repository files, reusing the parser and symbol cache.
fn extract_treesitter_symbols(
    state: &mut ExtractorState,
    repo_root: &Path,
    files: &[String],
) -> Vec<RepoFileSymbols> {
    let mut result = Vec::new();
    for rel_path in files {
        let full_path = repo_root.join(rel_path);
        let metadata = match std::fs::metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let modified = metadata.modified().ok();
        let size = metadata.len();

        if let Some(cached) = state.cache.get(&full_path) {
            if cached.modified == modified && cached.size == size {
                if let Some(symbols) = cached.symbols.clone() {
                    result.push(symbols);
                }
                continue;
            }
        }

        let support = match language_for_path(rel_path) {
            Some(support) => support,
            None => continue,
        };
        let source = match std::fs::read_to_string(&full_path) {
            Ok(source) => source,
            Err(_) => continue,
        };
        let symbols = extract_symbols_with_support(&mut state.parser, support, rel_path, &source)
            .filter(|file| !file.symbols.is_empty());
        if let Some(symbols) = &symbols {
            result.push(symbols.clone());
        }
        state.cache.insert(
            full_path,
            CachedSymbols {
                modified,
                size,
                symbols,
            },
        );
    }

    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

/// Extracts symbols from an in-memory source string using the registry entry
/// registered for `path`'s extension.
pub fn extract_symbols_from_str(path: &str, source: &str) -> Result<RepoFileSymbols> {
    let support = language_for_path(path)
        .ok_or_else(|| anyhow::anyhow!("Unsupported source extension for path: {}", path))?;
    let mut parser = Parser::new();
    let symbols = extract_symbols_with_support(&mut parser, support, path, source)
        .unwrap_or_else(|| RepoFileSymbols {
            path: path.to_string(),
            symbols: Vec::new(),
            line_count: source.lines().count(),
        });
    Ok(symbols)
}

fn extract_symbols_with_support(
    parser: &mut Parser,
    support: &LanguageSupport,
    rel_path: &str,
    source: &str,
) -> Option<RepoFileSymbols> {
    parser.set_language(&(support.language)()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let symbols = (support.extract)(root, source.as_bytes());
    Some(RepoFileSymbols {
        path: rel_path.to_string(),
        symbols,
        line_count: source.lines().count(),
    })
}

fn extract_container_symbols(container: Node, source: &[u8]) -> Vec<Symbol> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_symbols_from_source_contours() {
        let code = r#"
pub mod sub_module;

pub struct User {
    pub id: u64,
    pub name: String,
    secret: String,
}

pub enum Status {
    Active,
    Pending(u32),
}

pub trait Greeter {
    fn greet(&self) -> String;
}

impl Greeter for User {
    fn greet(&self) -> String {
        format!("Hello {}", self.name)
    }
}

pub fn create_user(name: &str) -> User {
    User { id: 1, name: name.to_string(), secret: "none".into() }
}
"#;

        let res = extract_symbols_from_str("src/user.rs", code).unwrap();
        assert_eq!(res.path, "src/user.rs");
        assert_eq!(res.line_count, code.lines().count());

        let mod_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Mod).unwrap();
        assert_eq!(mod_sym.name, "sub_module");
        assert!(mod_sym.is_public);

        let struct_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Struct).unwrap();
        assert_eq!(struct_sym.name, "User");
        assert!(struct_sym.signature.as_ref().unwrap().contains("pub id: u64"));
        assert!(struct_sym.signature.as_ref().unwrap().contains("secret: String"));

        let enum_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Enum).unwrap();
        assert_eq!(enum_sym.name, "Status");
        assert!(enum_sym.signature.as_ref().unwrap().contains("Active"));
        assert!(enum_sym.signature.as_ref().unwrap().contains("Pending(u32)"));

        let trait_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Trait).unwrap();
        assert_eq!(trait_sym.name, "Greeter");
        assert_eq!(trait_sym.children.len(), 1);
        assert_eq!(trait_sym.children[0].name, "greet");

        let impl_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Impl).unwrap();
        assert!(impl_sym.name.contains("Greeter for User"));
        assert_eq!(impl_sym.children.len(), 1);
        assert_eq!(impl_sym.children[0].name, "greet");
        assert!(impl_sym.children[0].is_public);

        let fn_sym = res.symbols.iter().find(|s| s.kind == SymbolKind::Function).unwrap();
        assert_eq!(fn_sym.name, "create_user");
        assert!(fn_sym.signature.as_ref().unwrap().contains("pub fn create_user(name: &str) -> User"));
    }

    #[test]
    fn test_syntax_error_resilience() {
        let code = r#"
pub struct ValidOne {
    pub a: i32,
}

// Broken syntax node
fn broken_syntax( { let x = ; }

pub fn valid_after_error() -> bool {
    true
}
"#;
        let res = extract_symbols_from_str("src/broken.rs", code).unwrap();
        assert!(res.symbols.iter().any(|s| s.name == "ValidOne"));
        assert!(res.symbols.iter().any(|s| s.name == "valid_after_error"));
    }

    #[test]
    fn test_is_supported_path_registry() {
        assert!(is_supported_path("src/lib.rs"));
        assert!(is_supported_path("nested/dir/module.rs"));
        assert!(!is_supported_path("src/index.ts"));
        assert!(!is_supported_path("scripts/build.py"));
        assert!(!is_supported_path("Makefile"));
    }

    #[test]
    fn test_extract_symbols_from_str_rejects_unsupported_extension() {
        assert!(extract_symbols_from_str("src/app.ts", "export const value = 1;\n").is_err());
    }

    #[test]
    fn test_extractor_cache_reuses_unchanged_and_invalidates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.rs"), "pub fn first() -> i32 { 1 }\n").unwrap();
        let extractor = TreeSitterExtractor::new();
        let files = vec!["a.rs".to_string()];

        let first = extractor.extract_symbols(&root, &files).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].symbols[0].name, "first");
        assert_eq!(extractor.state.lock().unwrap().cache.len(), 1);

        // An unchanged file is served from the cache without a new entry.
        let second = extractor.extract_symbols(&root, &files).unwrap();
        assert_eq!(second, first);
        assert_eq!(extractor.state.lock().unwrap().cache.len(), 1);

        // Rewriting the file with a different size invalidates the entry.
        std::fs::write(root.join("a.rs"), "pub fn second_value() -> i32 { 2 }\n").unwrap();
        let third = extractor.extract_symbols(&root, &files).unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].symbols[0].name, "second_value");
    }
}
