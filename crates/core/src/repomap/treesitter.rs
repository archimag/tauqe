use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
    symbols: Option<Arc<RepoFileSymbols>>,
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
///
/// Production callers resolve the process-wide shared instance through
/// [`shared_extractor`], so parsed symbols stay warm across turns, discovery
/// rounds, and targeted map requests instead of being discarded with a
/// per-call extractor.
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

/// Process-wide shared extractor instance.
///
/// Repository-map generation runs once per bounded turn and once per targeted
/// `<repo_map>` request, so building a fresh extractor per call would discard
/// the mtime/size cache before it could ever record a cross-invocation hit.
/// The shared instance keeps parsed symbols warm for the whole process, and
/// cache keys are absolute paths, so entries never collide across repositories.
static SHARED_EXTRACTOR: OnceLock<TreeSitterExtractor> = OnceLock::new();

/// Returns the process-wide shared Tree-sitter extractor.
pub fn shared_extractor() -> &'static TreeSitterExtractor {
    SHARED_EXTRACTOR.get_or_init(TreeSitterExtractor::new)
}

impl SymbolExtractor for TreeSitterExtractor {
    fn extract_symbols(
        &self,
        repo_root: &Path,
        files: &[String],
    ) -> Result<Vec<Arc<RepoFileSymbols>>> {
        crate::blocking::run_blocking(|| {
            Ok(extract_treesitter_symbols(&self.state, repo_root, files))
        })
    }
}

/// Extracts symbols from repository files, reusing the parser and symbol cache.
fn extract_treesitter_symbols(
    state_lock: &Mutex<ExtractorState>,
    repo_root: &Path,
    files: &[String],
) -> Vec<Arc<RepoFileSymbols>> {
    let mut result = Vec::new();
    for rel_path in files {
        let full_path = repo_root.join(rel_path);
        let metadata = match std::fs::metadata(&full_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                // The file no longer exists: drop any stale entry so deleted
                // files do not accumulate in the shared, process-wide cache.
                let mut state = match state_lock.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.cache.remove(&full_path);
                continue;
            }
        };
        let modified = metadata.modified().ok();
        let size = metadata.len();

        let cached_hit = {
            let state = match state_lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.cache.get(&full_path).and_then(|cached| {
                if cached.modified == modified && cached.size == size {
                    Some(cached.symbols.clone())
                } else {
                    None
                }
            })
        };

        if let Some(cached_symbols) = cached_hit {
            if let Some(symbols) = cached_symbols {
                result.push(symbols);
            }
            continue;
        }

        let support = match language_for_path(rel_path) {
            Some(support) => support,
            None => continue,
        };
        let source = match std::fs::read_to_string(&full_path) {
            Ok(source) => source,
            Err(_) => continue,
        };

        let mut state = match state_lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(cached) = state.cache.get(&full_path) {
            if cached.modified == modified && cached.size == size {
                if let Some(symbols) = cached.symbols.clone() {
                    result.push(symbols);
                }
                continue;
            }
        }

        let symbols = extract_symbols_with_support(&mut state.parser, support, rel_path, &source)
            .filter(|file| !file.symbols.is_empty())
            .map(Arc::new);
        if let Some(symbols) = &symbols {
            result.push(Arc::clone(symbols));
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
    let symbols =
        extract_symbols_with_support(&mut parser, support, path, source).unwrap_or_else(|| {
            RepoFileSymbols {
                path: path.to_string(),
                symbols: Vec::new(),
                line_count: source.lines().count(),
            }
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
    use crate::repomap::{Symbol, SymbolKind};

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

        let mod_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Mod)
            .unwrap();
        assert_eq!(mod_sym.name, "sub_module");
        assert!(mod_sym.is_public);

        let struct_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(struct_sym.name, "User");
        assert!(struct_sym
            .signature
            .as_ref()
            .unwrap()
            .contains("pub id: u64"));
        assert!(struct_sym
            .signature
            .as_ref()
            .unwrap()
            .contains("secret: String"));

        let enum_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Enum)
            .unwrap();
        assert_eq!(enum_sym.name, "Status");
        assert!(enum_sym.signature.as_ref().unwrap().contains("Active"));
        assert!(enum_sym
            .signature
            .as_ref()
            .unwrap()
            .contains("Pending(u32)"));

        let trait_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Trait)
            .unwrap();
        assert_eq!(trait_sym.name, "Greeter");
        assert_eq!(trait_sym.children.len(), 1);
        assert_eq!(trait_sym.children[0].name, "greet");

        let impl_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Impl)
            .unwrap();
        assert!(impl_sym.name.contains("Greeter for User"));
        assert_eq!(impl_sym.children.len(), 1);
        assert_eq!(impl_sym.children[0].name, "greet");
        assert!(impl_sym.children[0].is_public);

        let fn_sym = res
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Function)
            .unwrap();
        assert_eq!(fn_sym.name, "create_user");
        assert!(fn_sym
            .signature
            .as_ref()
            .unwrap()
            .contains("pub fn create_user(name: &str) -> User"));
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
        assert!(is_supported_path("src/index.ts"));
        assert!(is_supported_path("scripts/build.py"));
        assert!(is_supported_path("src/Main.java"));
        assert!(is_supported_path("src/point.c"));
        assert!(is_supported_path("include/point.h"));
        assert!(is_supported_path("src/renderer.cpp"));
        assert!(is_supported_path("include/renderer.hpp"));
        assert!(is_supported_path("src/engine.cc"));
        assert!(is_supported_path("src/engine.cxx"));
        assert!(is_supported_path("src/Service.cs"));
        assert!(is_supported_path("src/Service.php"));
        assert!(!is_supported_path("src/app.rb"));
        assert!(!is_supported_path("Makefile"));
    }

    #[test]
    fn test_extract_symbols_from_str_rejects_unsupported_extension() {
        assert!(extract_symbols_from_str("src/app.kt", "fun main() {}\n").is_err());
    }

    #[test]
    fn test_extract_java_declarations() {
        let code = r#"
package com.example;

public class Service implements Greeter {
    private int retries;

    public Service(int retries) {
        this.retries = retries;
    }

    public String greet(String name) {
        return name;
    }
}

interface Greeter {
    String greet(String name);
}

enum Status {
    Active,
    Pending
}
"#;
        let file = extract_symbols_from_str("src/Service.java", code).unwrap();

        let service = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Service")
            .unwrap();
        assert_eq!(service.kind, SymbolKind::Struct);
        assert!(service.is_public);
        assert!(service
            .signature
            .as_deref()
            .unwrap()
            .contains("implements Greeter"));
        assert!(service
            .children
            .iter()
            .any(|member| member.name == "Service" && member.kind == SymbolKind::Method));
        assert!(service
            .children
            .iter()
            .any(|member| member.name == "greet" && member.is_public));

        let greeter = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Greeter")
            .unwrap();
        assert_eq!(greeter.kind, SymbolKind::Trait);
        assert_eq!(greeter.children[0].name, "greet");
        // Interface members are public by declaration, without a modifier.
        assert!(greeter.children[0].is_public);

        let status = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Status")
            .unwrap();
        assert_eq!(status.kind, SymbolKind::Enum);
        assert!(status.signature.as_deref().unwrap().contains("Pending"));
    }

    #[test]
    fn test_extract_c_declarations() {
        let code = r#"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

enum Color {
    Red,
    Green
};

int add(int a, int b);

int add(int a, int b) {
    return a + b;
}
"#;
        let file = extract_symbols_from_str("src/point.c", code).unwrap();

        let point = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Point")
            .unwrap();
        assert_eq!(point.kind, SymbolKind::Struct);
        assert!(point.signature.as_deref().unwrap().contains("int x;"));

        let color = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Color")
            .unwrap();
        assert_eq!(color.kind, SymbolKind::Enum);
        assert!(color.signature.as_deref().unwrap().contains("Red"));

        // A prototype and its definition are both announced.
        let add: Vec<&Symbol> = file
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "add")
            .collect();
        assert_eq!(add.len(), 2);
        assert!(add.iter().all(|symbol| symbol.kind == SymbolKind::Function));
        assert!(add
            .iter()
            .all(|symbol| symbol.signature.as_deref().unwrap().contains("int a")));
    }

    #[test]
    fn test_extract_cpp_declarations() {
        let code = r#"
namespace app {

class Service : public Greeter {
public:
    explicit Service(int retries);
    void greet(const std::string &name) override;
    int retries() const { return retries_; }

private:
    int retries_;
};

}  // namespace app

template <typename T>
T identity(T value) {
    return value;
}
"#;
        let file = extract_symbols_from_str("src/service.cpp", code).unwrap();

        let namespace = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Mod)
            .unwrap();
        assert_eq!(namespace.name, "app");

        let service = namespace
            .children
            .iter()
            .find(|symbol| symbol.name == "Service")
            .unwrap();
        assert_eq!(service.kind, SymbolKind::Struct);
        let members: Vec<&str> = service
            .children
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert!(members.contains(&"Service"));
        assert!(members.contains(&"greet"));
        assert!(members.contains(&"retries"));
        assert!(service
            .children
            .iter()
            .all(|member| member.kind == SymbolKind::Method));

        let identity = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "identity")
            .unwrap();
        assert_eq!(identity.kind, SymbolKind::Function);
    }

    #[test]
    fn test_extract_c_sharp_declarations() {
        let code = r#"
namespace Sample
{
    public class Service : IGreeter
    {
        private int retries;

        public Service(int retries)
        {
            this.retries = retries;
        }

        public string Greet(string name) => name;
    }

    public interface IGreeter
    {
        string Greet(string name);
    }

    public enum Status
    {
        Active,
        Pending
    }
}
"#;
        let file = extract_symbols_from_str("src/Service.cs", code).unwrap();

        let namespace = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Mod)
            .unwrap();
        assert_eq!(namespace.name, "Sample");

        let service = namespace
            .children
            .iter()
            .find(|symbol| symbol.name == "Service")
            .unwrap();
        assert_eq!(service.kind, SymbolKind::Struct);
        assert!(service.is_public);
        assert!(service
            .children
            .iter()
            .any(|member| member.name == "Greet" && member.is_public));

        let greeter = namespace
            .children
            .iter()
            .find(|symbol| symbol.name == "IGreeter")
            .unwrap();
        assert_eq!(greeter.kind, SymbolKind::Trait);
        assert!(greeter.children[0].is_public);

        let status = namespace
            .children
            .iter()
            .find(|symbol| symbol.name == "Status")
            .unwrap();
        assert_eq!(status.kind, SymbolKind::Enum);
        assert!(status.signature.as_deref().unwrap().contains("Pending"));
    }

    #[test]
    fn test_extract_php_declarations() {
        let code = r#"<?php

namespace App;

interface Greeter
{
    public function greet(string $name): string;
}

class Service implements Greeter
{
    private int $retries;

    public function greet(string $name): string
    {
        return $name;
    }

    private function reset(): void
    {
    }
}

enum Status
{
    case Active;
    case Pending;
}

function build_service(int $retries): Service
{
    return new Service($retries);
}
"#;
        let file = extract_symbols_from_str("src/Service.php", code).unwrap();

        let namespace = file
            .symbols
            .iter()
            .find(|symbol| symbol.kind == SymbolKind::Mod)
            .unwrap();
        assert_eq!(namespace.name, "App");

        let greeter = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Greeter")
            .unwrap();
        assert_eq!(greeter.kind, SymbolKind::Trait);
        assert_eq!(greeter.children[0].name, "greet");
        assert!(greeter.children[0].is_public);

        let service = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Service")
            .unwrap();
        assert_eq!(service.kind, SymbolKind::Struct);
        assert!(service
            .children
            .iter()
            .any(|member| member.name == "greet" && member.kind == SymbolKind::Method));
        assert!(service
            .children
            .iter()
            .any(|member| member.name == "reset" && !member.is_public));

        let status = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Status")
            .unwrap();
        assert_eq!(status.kind, SymbolKind::Enum);
        assert!(status.signature.as_deref().unwrap().contains("Active"));

        let build = file
            .symbols
            .iter()
            .find(|symbol| symbol.name == "build_service")
            .unwrap();
        assert_eq!(build.kind, SymbolKind::Function);
        assert!(build.is_public);
    }

    #[test]
    fn test_extracts_declarations_preceding_syntax_errors_in_every_language() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "src/broken.rs",
                "pub fn valid_after() -> bool {\n    true\n}\n\nfn broken_syntax( { let x = ; }\n",
                "valid_after",
            ),
            (
                "src/broken.ts",
                "export function validAfter(): number {\n  return 1;\n}\n\nfunction broken( {\n",
                "validAfter",
            ),
            (
                "src/broken.js",
                "export function validAfter() {\n  return 1;\n}\n\nconst broken = ;\n",
                "validAfter",
            ),
            (
                "src/broken.py",
                "def valid_after():\n    return True\n\ndef broken(:\n",
                "valid_after",
            ),
            (
                "src/broken.go",
                "package broken\n\nfunc ValidAfter() bool {\n\treturn true\n}\n\nfunc broken( {\n",
                "ValidAfter",
            ),
            (
                "src/Broken.java",
                "public class ValidAfter {\n}\n\nclass Broken {\n    void broken( {\n}\n",
                "ValidAfter",
            ),
            (
                "src/broken.c",
                "int valid_after(void) {\n    return 1;\n}\n\nint broken(void) {\n    return 1;\n",
                "valid_after",
            ),
            (
                "src/broken.cpp",
                "int valid_after() {\n    return 1;\n}\n\nclass Broken {\n    void broken( {\n",
                "valid_after",
            ),
            (
                "src/Broken.cs",
                "public class ValidAfter\n{\n}\n\npublic class Broken\n{\n    void broken( {\n}\n",
                "ValidAfter",
            ),
            (
                "src/broken.php",
                "<?php\n\nfunction valid_after(): bool\n{\n    return true;\n}\n\nfunction broken( {\n",
                "valid_after",
            ),
        ];

        for (path, code, expected) in cases {
            let file = extract_symbols_from_str(path, code).unwrap();
            assert!(
                file.symbols.iter().any(|symbol| symbol.name == *expected),
                "{path} must still extract {expected}"
            );
        }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_symbol_extractions_on_multi_thread_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for i in 0..10 {
            std::fs::write(
                root.join(format!("file_{}.rs", i)),
                format!("pub fn func_{}() -> i32 {{ {} }}\n", i, i),
            )
            .unwrap();
        }

        let extractor = TreeSitterExtractor::new();
        let mut handles = Vec::new();

        for _ in 0..8 {
            let extractor = extractor.clone();
            let root = root.clone();
            let handle = tokio::spawn(async move {
                let files: Vec<String> = (0..10).map(|i| format!("file_{}.rs", i)).collect();
                extractor.extract_symbols(&root, &files).unwrap()
            });
            handles.push(handle);
        }

        for handle in handles {
            let res = handle.await.unwrap();
            assert_eq!(res.len(), 10);
        }

        assert_eq!(extractor.state.lock().unwrap().cache.len(), 10);
    }

    #[test]
    fn test_shared_extractor_serves_two_generations_without_reparse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("shared_a.rs"),
            "pub fn shared_alpha() -> i32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("shared_b.rs"),
            "pub fn shared_beta() -> i32 { 2 }\n",
        )
        .unwrap();
        let files = vec!["shared_a.rs".to_string(), "shared_b.rs".to_string()];

        let first = shared_extractor().extract_symbols(&root, &files).unwrap();
        assert_eq!(first.len(), 2);
        let entry_before = {
            let state = shared_extractor().state.lock().unwrap();
            state
                .cache
                .get(&root.join("shared_a.rs"))
                .and_then(|cached| cached.symbols.clone())
                .expect("generation registers the parsed file")
        };

        // A second generation over the unchanged repository must be served from
        // the persistent cache: the entry keeps its identity, proving that no
        // file was re-parsed and no symbol payload was rebuilt.
        let second = shared_extractor().extract_symbols(&root, &files).unwrap();
        assert_eq!(second, first);
        let entry_after = {
            let state = shared_extractor().state.lock().unwrap();
            state
                .cache
                .get(&root.join("shared_a.rs"))
                .and_then(|cached| cached.symbols.clone())
                .expect("the cached entry survives the second generation")
        };
        assert!(
            Arc::ptr_eq(&entry_before, &entry_after),
            "an unchanged file must be served from the shared cache without a re-parse"
        );

        // A deleted file evicts its entry instead of leaving stale state in
        // the process-wide cache.
        std::fs::remove_file(root.join("shared_a.rs")).unwrap();
        let third = shared_extractor().extract_symbols(&root, &files).unwrap();
        assert_eq!(third.len(), 1);
        let stale_entry_present = {
            let state = shared_extractor().state.lock().unwrap();
            state.cache.contains_key(&root.join("shared_a.rs"))
        };
        assert!(!stale_entry_present, "a missing file must evict its entry");
    }

    #[test]
    fn test_generate_repo_map_reuses_the_shared_extractor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("shared_map.rs"), "pub fn shared_map_fn() {}\n").unwrap();
        let files = vec!["shared_map.rs".to_string()];

        // The public map-generation path resolves through the process-wide
        // extractor, so its parse lands in the shared cache.
        let first = crate::repomap::generate_repo_map(&root, &files, &[], 1000).unwrap();
        assert!(first.contains("pub fn shared_map_fn"));
        let entry_before = {
            let state = shared_extractor().state.lock().unwrap();
            state
                .cache
                .get(&root.join("shared_map.rs"))
                .and_then(|cached| cached.symbols.clone())
                .expect("map generation shares the process-wide extractor")
        };

        // The second generation over the unchanged repository reuses the cached
        // parse instead of re-running the Tree-sitter pass.
        let second = crate::repomap::generate_repo_map(&root, &files, &[], 1000).unwrap();
        assert_eq!(second, first);
        let entry_after = {
            let state = shared_extractor().state.lock().unwrap();
            state
                .cache
                .get(&root.join("shared_map.rs"))
                .and_then(|cached| cached.symbols.clone())
                .expect("the cached entry survives the second generation")
        };
        assert!(
            Arc::ptr_eq(&entry_before, &entry_after),
            "the second map generation must reuse the cached parse"
        );
    }
}
