use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use tree_sitter::Parser;

use crate::repomap::{RepoFileSymbols, SymbolExtractor};

mod extract;
mod languages;

pub use languages::is_supported_path;

use languages::{language_for_path, LanguageSupport};

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
#[derive(Clone)]
pub struct TreeSitterExtractor {
    state: Arc<Mutex<ExtractorState>>,
}

impl Default for TreeSitterExtractor {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ExtractorState {
                parser: Parser::new(),
                cache: HashMap::new(),
            })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repomap::SymbolKind;

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
