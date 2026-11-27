use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::paths::is_excluded_path;
use classify::{classify_repo_path, RepoPathKind, SymlinkStatus, SymlinkTarget};

pub mod classify;
pub mod dirs;
pub mod treesitter;

pub use treesitter::TreeSitterExtractor;

/// Trait defining a strategy for extracting symbols from repository files.
///
/// Allows pluggable implementations (Tree-sitter, language servers, precomputed indices).
/// Implementations return shared payloads (`Arc<RepoFileSymbols>`), so a cached
/// parse is handed to every consumer without rebuilding the symbol vectors.
pub trait SymbolExtractor: Send + Sync {
    fn extract_symbols(
        &self,
        repo_root: &Path,
        files: &[String],
    ) -> Result<Vec<Arc<RepoFileSymbols>>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Struct,
    Enum,
    Trait,
    Function,
    Method,
    Impl,
    Mod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub signature: Option<String>,
    pub is_public: bool,
    line: usize,
    pub children: Vec<Symbol>,
}

impl Symbol {
    #[cfg(test)]
    fn new(
        name: impl Into<String>,
        kind: SymbolKind,
        signature: Option<String>,
        is_public: bool,
        line: usize,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            signature,
            is_public,
            line,
            children: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFileSymbols {
    path: String,
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    line_count: usize,
}

/// Reflexive projection so owned payloads and shared `Arc` payloads satisfy the
/// same `S: AsRef<RepoFileSymbols>` rendering bound, letting map generation
/// format cached symbols without cloning them.
impl AsRef<RepoFileSymbols> for RepoFileSymbols {
    fn as_ref(&self) -> &RepoFileSymbols {
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DetailLevel {
    Full,
    PublicWithDetails,
    OutlinesOnly,
    TopLevelOnly,
}

/// Generates the complete top-level repository outline with the default extractor.
pub fn generate_repo_map(repo_root: &Path, available_files: &[String]) -> Result<String> {
    generate_repo_map_with_extractor(repo_root, available_files, treesitter::shared_extractor())
}

/// Generates the complete top-level repository outline with a custom extractor.
///
/// 1. Drops paths under a service directory (`.git`, `.tauqe`, `target`, `node_modules`), so
///    the symbol map reports the same repository shape as the directory outline.
/// 2. Queries the provided `SymbolExtractor` for supported code files.
/// 3. Includes every other tracked path, without truncation.
pub fn generate_repo_map_with_extractor(
    repo_root: &Path,
    available_files: &[String],
    extractor: &dyn SymbolExtractor,
) -> Result<String> {
    crate::blocking::run_blocking(|| {
        let mut all_git_files: Vec<String> = Vec::new();
        let mut eligible_files: Vec<String> = Vec::new();

        for path in available_files {
            // A service directory is not part of the navigable repository shape,
            // so the symbol map and the directory outline drop the same paths.
            if is_excluded_path(path.as_str()) {
                continue;
            }
            match classify_repo_path(repo_root, path.as_str()) {
                RepoPathKind::RegularFile => {
                    if treesitter::is_supported_path(path.as_str()) {
                        eligible_files.push(path.to_string());
                    }
                    all_git_files.push(path.to_string());
                }
                RepoPathKind::Symlink(link) => {
                    all_git_files.push(format_map_symlink(path.as_str(), &link));
                }
                RepoPathKind::Other => {}
            }
        }

        all_git_files.sort();
        all_git_files.dedup();
        eligible_files.sort();
        eligible_files.dedup();

        if all_git_files.is_empty() {
            return Ok(String::new());
        }

        let symbols = if eligible_files.is_empty() {
            Vec::new()
        } else {
            extractor.extract_symbols(repo_root, &eligible_files)?
        };

        let outline = format_repo_map(&symbols, &all_git_files, DetailLevel::TopLevelOnly);
        Ok(outline)
    })
}

/// Renders a tracked symbolic link as a repository-map entry naming its target.
///
/// The link never enters the Tree-sitter outline, so an internal link cannot
/// duplicate the target's symbols and an escaping link cannot leak external
/// content. Only the link path and its stored target are shown, with a suffix
/// when the link is broken or leaves the repository.
fn format_map_symlink(path: &str, link: &SymlinkTarget) -> String {
    let suffix = match link.status {
        SymlinkStatus::Internal => "",
        SymlinkStatus::OutsideRepository => " (outside repository)",
        SymlinkStatus::Broken => " (broken)",
    };
    if link.target.is_empty() {
        format!("{path}{suffix}")
    } else {
        format!("{path} -> {}{suffix}", link.target)
    }
}

/// Generates a symbol outline restricted to the files under `target_path`.
///
/// The target is a repository-relative prefix matched on whole path components,
/// so `crates/core` covers `crates/core/src/lib.rs` but never
/// `crates/core-utils/lib.rs`; naming a single file narrows the outline to that
/// file. An empty prefix covers the whole repository.
///
/// The target includes files already in context and renders full symbol detail.
/// An unknown prefix yields an empty string instead of a repository-wide outline.
pub fn generate_targeted_repo_map(
    repo_root: &Path,
    available_files: &[String],
    target_path: &str,
) -> Result<String> {
    let prefix = normalize_target_prefix(target_path);
    let targeted: Vec<String> = available_files
        .iter()
        .filter(|path| path_is_under_prefix(path, &prefix))
        .cloned()
        .collect();
    if targeted.is_empty() {
        return Ok(String::new());
    }
    generate_repo_map_at_level(repo_root, &targeted, DetailLevel::Full)
}

fn generate_repo_map_at_level(
    repo_root: &Path,
    available_files: &[String],
    level: DetailLevel,
) -> Result<String> {
    crate::blocking::run_blocking(|| {
        let mut all_git_files = Vec::new();
        let mut eligible_files = Vec::new();
        for path in available_files {
            if is_excluded_path(path) {
                continue;
            }
            match classify_repo_path(repo_root, path) {
                RepoPathKind::RegularFile => {
                    if treesitter::is_supported_path(path) {
                        eligible_files.push(path.clone());
                    }
                    all_git_files.push(path.clone());
                }
                RepoPathKind::Symlink(link) => all_git_files.push(format_map_symlink(path, &link)),
                RepoPathKind::Other => {}
            }
        }
        all_git_files.sort();
        all_git_files.dedup();
        eligible_files.sort();
        eligible_files.dedup();
        let symbols = if eligible_files.is_empty() {
            Vec::new()
        } else {
            treesitter::shared_extractor().extract_symbols(repo_root, &eligible_files)?
        };
        Ok(format_repo_map(&symbols, &all_git_files, level))
    })
}

/// Normalizes a requested map path into a repository-relative prefix.
///
/// Leading `./` markers and `/` separators are stripped and a trailing `/` is
/// dropped, so `./crates/core/` and `crates/core` name the same subtree. An
/// empty result names the whole repository.
fn normalize_target_prefix(target_path: &str) -> String {
    let trimmed = target_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .trim_end_matches('/');
    match trimmed {
        "" | "." => String::new(),
        other => other.to_string(),
    }
}

/// Reports whether a repository-relative file path lies under `prefix`.
///
/// The comparison is component-wise, so a sibling directory that merely shares a
/// textual stem is never included. An empty prefix matches every path.
fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Renders all symbol blocks and tracked paths in stable path order.
fn format_repo_map<S: AsRef<RepoFileSymbols>>(
    symbols: &[S],
    all_git_files: &[String],
    level: DetailLevel,
) -> String {
    let mut rendered = std::collections::BTreeMap::new();
    for file in symbols {
        let file = file.as_ref();
        let block = format_file_symbols(file, level);
        if !block.is_empty() {
            rendered.insert(file.path.as_str(), block);
        }
    }
    let mut entries = Vec::new();
    for path in all_git_files {
        // Symlink annotations are plain entries; they are never parsed as symbols.
        entries.push(
            rendered
                .remove(path.as_str())
                .unwrap_or_else(|| path.clone()),
        );
    }
    entries.join("\n\n")
}

fn format_file_symbols(file: &RepoFileSymbols, level: DetailLevel) -> String {
    let mut lines = Vec::new();
    if file.line_count > 0 {
        let suffix = if file.line_count == 1 {
            "line"
        } else {
            "lines"
        };
        lines.push(format!("{} ({} {})", file.path, file.line_count, suffix));
    } else {
        lines.push(file.path.clone());
    }

    for sym in &file.symbols {
        format_symbol_into_lines(sym, 1, level, &mut lines);
    }

    if lines.len() <= 1 {
        // No symbols rendered for this file
        String::new()
    } else {
        lines.join("\n")
    }
}

fn format_symbol_into_lines(
    sym: &Symbol,
    indent_level: usize,
    level: DetailLevel,
    lines: &mut Vec<String>,
) {
    if level != DetailLevel::Full && !sym.is_public && sym.kind != SymbolKind::Impl {
        return;
    }

    let indent = "  ".repeat(indent_level);
    let child_indent = "  ".repeat(indent_level + 1);

    match sym.kind {
        SymbolKind::Struct => {
            let sig = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(sig);
            lines.push(format!("{}{}", indent, header));

            if level != DetailLevel::TopLevelOnly {
                for m in &sym.children {
                    if level == DetailLevel::Full || m.is_public {
                        lines.push(format!(
                            "{}{}",
                            child_indent,
                            format_func_signature(m, false)
                        ));
                    }
                }
            }
        }
        SymbolKind::Enum => {
            let sig = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(sig);
            lines.push(format!("{}{}", indent, header));

            if level == DetailLevel::Full || level == DetailLevel::PublicWithDetails {
                for variant in extract_enum_variants(sig) {
                    lines.push(format!("{}{}", child_indent, variant));
                }
            }
        }
        SymbolKind::Trait => {
            let header = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(header);
            lines.push(format!("{}{}", indent, header));

            if level != DetailLevel::TopLevelOnly {
                for m in &sym.children {
                    if level == DetailLevel::Full || m.is_public {
                        lines.push(format!(
                            "{}{}",
                            child_indent,
                            format_func_signature(m, true)
                        ));
                    }
                }
            }
        }
        SymbolKind::Impl => {
            let is_trait_impl = sym.name.contains(" for ");
            let methods: Vec<_> = sym
                .children
                .iter()
                .filter(|m| level == DetailLevel::Full || is_trait_impl || m.is_public)
                .collect();

            if methods.is_empty() && level != DetailLevel::Full {
                return;
            }

            let header = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(header);
            lines.push(format!("{}{}", indent, header));

            if level != DetailLevel::TopLevelOnly {
                for m in methods {
                    lines.push(format!(
                        "{}{}",
                        child_indent,
                        format_func_signature(m, is_trait_impl)
                    ));
                }
            }
        }
        SymbolKind::Function | SymbolKind::Method => {
            lines.push(format!("{}{}", indent, format_func_signature(sym, false)));
        }
        SymbolKind::Mod => {
            let header = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(header);
            lines.push(format!("{}{}", indent, header));

            for child in &sym.children {
                format_symbol_into_lines(child, indent_level + 1, level, lines);
            }
        }
    }
}

fn format_func_signature(sym: &Symbol, is_trait_context: bool) -> String {
    let name = if sym.name.is_empty() {
        sym.signature.as_deref().unwrap_or("fn")
    } else {
        &sym.name
    };

    if is_trait_context {
        format!("fn {}", name)
    } else if sym.is_public {
        format!("pub fn {}", name)
    } else {
        format!("fn {}", name)
    }
}

fn extract_enum_variants(sig: &str) -> Vec<String> {
    let mut res = Vec::new();
    for line in extract_body_lines(sig) {
        let name = line
            .split(&['(', '{', '='][..])
            .next()
            .unwrap_or(line)
            .trim();
        let name = name.trim_end_matches(',').trim();
        if !name.is_empty()
            && !name.starts_with("//")
            && !name.starts_with("/*")
            && !name.starts_with('#')
        {
            res.push(name.to_string());
        }
    }
    res
}

fn extract_header_from_sig(sig: &str) -> &str {
    let trimmed = sig.trim();
    if let Some(pos) = trimmed.find('{') {
        trimmed[..pos].trim()
    } else if let Some(pos) = trimmed.find(';') {
        trimmed[..pos].trim()
    } else {
        trimmed
    }
}

fn extract_body_lines(sig: &str) -> Vec<&str> {
    let mut res = Vec::new();
    let start = match sig.find('{') {
        Some(pos) => pos + 1,
        None => return res,
    };
    let end = match sig.rfind('}') {
        Some(pos) => pos,
        None => sig.len(),
    };
    if start < end {
        for line in sig[start..end].lines() {
            let trimmed = line.trim().trim_end_matches(',');
            if !trimmed.is_empty() {
                res.push(trimmed);
            }
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_symbols_with_budget() {
        let syms = vec![RepoFileSymbols {
            path: "src/sample.rs".to_string(),
            symbols: vec![
                Symbol {
                    name: "PublicStruct".to_string(),
                    kind: SymbolKind::Struct,
                    signature: Some(
                        "pub struct PublicStruct {\n    pub id: u64,\n    pub name: String,\n}"
                            .to_string(),
                    ),
                    is_public: true,
                    line: 1,
                    children: Vec::new(),
                },
                Symbol {
                    name: "private_func".to_string(),
                    kind: SymbolKind::Function,
                    signature: Some("fn private_func()".to_string()),
                    is_public: false,
                    line: 6,
                    children: Vec::new(),
                },
                Symbol {
                    name: "public_func".to_string(),
                    kind: SymbolKind::Function,
                    signature: Some("pub fn public_func() -> bool".to_string()),
                    is_public: true,
                    line: 8,
                    children: Vec::new(),
                },
            ],
            line_count: 10,
        }];

        let full = format_repo_map(&syms, &["src/sample.rs".into()], DetailLevel::Full);
        assert!(full.contains("src/sample.rs (10 lines)"));
        assert!(full.contains("pub struct PublicStruct"));
        assert!(!full.contains("pub id: u64"));
        assert!(full.contains("fn private_func"));
        assert!(full.contains("pub fn public_func"));
        assert!(!full.contains("pub fn public_func() -> bool"));

        let restricted =
            format_repo_map(&syms, &["src/sample.rs".into()], DetailLevel::TopLevelOnly);
        assert!(restricted.contains("src/sample.rs"));
        assert!(!restricted.contains("fn private_func"));
        assert!(restricted.contains("pub fn public_func"));
    }

    #[test]
    fn test_format_symbols_greedy_budget_truncation() {
        let syms = vec![
            RepoFileSymbols {
                path: "src/a.rs".to_string(),
                symbols: vec![Symbol::new(
                    "a",
                    SymbolKind::Function,
                    Some("pub fn a()".into()),
                    true,
                    1,
                )],
                line_count: 1,
            },
            RepoFileSymbols {
                path: "src/b.rs".to_string(),
                symbols: vec![Symbol::new(
                    "b",
                    SymbolKind::Function,
                    Some("pub fn b()".into()),
                    true,
                    1,
                )],
                line_count: 1,
            },
        ];

        let formatted = format_repo_map(
            &syms,
            &["src/a.rs".into(), "src/b.rs".into()],
            DetailLevel::TopLevelOnly,
        );
        assert!(formatted.contains("src/a.rs"));
        assert!(formatted.contains("src/b.rs"));
    }

    #[test]
    fn test_format_trait_impl_methods_retention() {
        let syms = vec![RepoFileSymbols {
            path: "src/sample.rs".to_string(),
            symbols: vec![Symbol {
                name: "impl Greeter for User".to_string(),
                kind: SymbolKind::Impl,
                signature: Some("impl Greeter for User".to_string()),
                is_public: true,
                line: 1,
                children: vec![Symbol {
                    name: "greet".to_string(),
                    kind: SymbolKind::Method,
                    signature: Some("fn greet(&self) -> String".to_string()),
                    is_public: true,
                    line: 2,
                    children: Vec::new(),
                }],
            }],
            line_count: 5,
        }];

        let outline = format_repo_map(&syms, &["src/sample.rs".into()], DetailLevel::Full);
        assert!(outline.contains("impl Greeter for User"));
        assert!(outline.contains("fn greet"));
        assert!(!outline.contains("fn greet(&self) -> String"));
    }

    #[test]
    fn test_format_repo_map_with_budget_includes_other_git_files() {
        let syms = vec![RepoFileSymbols {
            path: "src/main.rs".to_string(),
            symbols: vec![Symbol::new(
                "main",
                SymbolKind::Function,
                Some("fn main()".into()),
                true,
                1,
            )],
            line_count: 1,
        }];
        let all_files = vec![
            "Cargo.toml".to_string(),
            "README.md".to_string(),
            "src/main.rs".to_string(),
        ];

        let outline = format_repo_map(&syms, &all_files, DetailLevel::TopLevelOnly);
        assert!(outline.contains("src/main.rs (1 line)"));
        assert!(outline.contains("pub fn main"));
        assert!(outline.contains("Cargo.toml"));
        assert!(outline.contains("README.md"));
    }

    #[test]
    fn test_format_enum_variants_minimalistic() {
        let syms = vec![RepoFileSymbols {
            path: "src/status.rs".to_string(),
            symbols: vec![Symbol {
                name: "Status".to_string(),
                kind: SymbolKind::Enum,
                signature: Some(
                    "pub enum Status {\n    Ready,\n    Running(u32),\n    Done,\n}".to_string(),
                ),
                is_public: true,
                line: 1,
                children: Vec::new(),
            }],
            line_count: 6,
        }];

        let outline = format_repo_map(&syms, &["src/status.rs".into()], DetailLevel::Full);
        assert!(outline.contains("pub enum Status"));
        assert!(outline.contains("Ready"));
        assert!(outline.contains("Running"));
        assert!(outline.contains("Done"));
        assert!(!outline.contains("Running(u32)"));
    }

    #[test]
    fn test_format_repo_map_with_budget_only_other_files() {
        let syms: Vec<RepoFileSymbols> = vec![];
        let all_files = vec!["Cargo.toml".to_string(), "tauqe.toml".to_string()];

        let outline = format_repo_map(&syms, &all_files, DetailLevel::TopLevelOnly);
        assert!(outline.contains("Cargo.toml"));
        assert!(outline.contains("tauqe.toml"));
    }

    #[test]
    fn test_generate_targeted_repo_map_filters_by_component_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/core/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/protocol/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/core-utils/src")).unwrap();
        std::fs::write(root.join("crates/core/src/lib.rs"), "pub fn core_fn() {}\n").unwrap();
        std::fs::write(
            root.join("crates/protocol/src/lib.rs"),
            "pub fn protocol_fn() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/core-utils/src/lib.rs"),
            "pub fn utils_fn() {}\n",
        )
        .unwrap();

        let available = vec![
            "crates/core/src/lib.rs".to_string(),
            "crates/protocol/src/lib.rs".to_string(),
            "crates/core-utils/src/lib.rs".to_string(),
        ];

        let map = generate_targeted_repo_map(root, &available, "crates/core").unwrap();
        assert!(map.contains("crates/core/src/lib.rs"));
        assert!(map.contains("pub fn core_fn"));
        // A sibling directory sharing only a textual stem is excluded.
        assert!(!map.contains("crates/protocol"));
        assert!(!map.contains("crates/core-utils"));
        assert!(!map.contains("pub fn utils_fn"));

        // A cosmetic `./` prefix and a trailing slash name the same subtree.
        let dotted = generate_targeted_repo_map(root, &available, "./crates/core/").unwrap();
        assert_eq!(dotted, map);

        // Naming a single file narrows the outline to that file.
        let single =
            generate_targeted_repo_map(root, &available, "crates/core/src/lib.rs").unwrap();
        assert!(single.contains("pub fn core_fn"));
        assert!(!single.contains("crates/protocol"));

        // An unknown prefix yields an empty outline instead of a full map.
        assert!(generate_targeted_repo_map(root, &available, "missing")
            .unwrap()
            .is_empty());

        // An empty prefix covers the whole repository.
        let whole = generate_targeted_repo_map(root, &available, "").unwrap();
        assert!(whole.contains("pub fn core_fn"));
        assert!(whole.contains("pub fn protocol_fn"));
    }

    #[test]
    fn test_generate_targeted_repo_map_retains_full_detail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn alpha() {}\nfn private() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
        let available = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        let map = generate_targeted_repo_map(root, &available, "src").unwrap();
        assert!(map.contains("src/a.rs"));
        assert!(map.contains("src/b.rs"));
        assert!(map.contains("fn private"));
    }

    #[test]
    fn test_generate_repo_map_excludes_service_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn kept_fn() {}\n").unwrap();
        std::fs::write(
            root.join("target/debug/generated.rs"),
            "pub fn generated_fn() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("node_modules/pkg/index.rs"),
            "pub fn dependency_fn() {}\n",
        )
        .unwrap();

        let available = vec![
            "src/lib.rs".to_string(),
            "target/debug/generated.rs".to_string(),
            "node_modules/pkg/index.rs".to_string(),
        ];

        let map = generate_repo_map(root, &available).unwrap();
        assert!(map.contains("src/lib.rs"));
        assert!(map.contains("pub fn kept_fn"));
        assert!(!map.contains("generated.rs"));
        assert!(!map.contains("pub fn generated_fn"));
        assert!(!map.contains("node_modules"));
        assert!(!map.contains("pub fn dependency_fn"));

        // The directory outline draws from the same selection boundary, so both
        // generators describe one repository shape from the same input.
        assert_eq!(dirs::generate_repo_dirs(&available), "src\n");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_generate_repo_map_on_multi_thread_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("main.rs"), "pub fn main() {}\n").unwrap();
        let available = vec!["main.rs".to_string()];
        let handle = tokio::spawn(async move { generate_repo_map(&root, &available).unwrap() });

        let map = handle.await.unwrap();
        assert!(map.contains("main.rs"));
        assert!(map.contains("pub fn main"));
    }

    #[test]
    fn test_format_map_symlink_markers() {
        let internal = SymlinkTarget {
            target: "src/lib.rs".to_string(),
            status: SymlinkStatus::Internal,
        };
        assert_eq!(
            format_map_symlink("alias.rs", &internal),
            "alias.rs -> src/lib.rs"
        );

        let outside = SymlinkTarget {
            target: "/etc/passwd".to_string(),
            status: SymlinkStatus::OutsideRepository,
        };
        assert_eq!(
            format_map_symlink("escape.rs", &outside),
            "escape.rs -> /etc/passwd (outside repository)"
        );

        let broken = SymlinkTarget {
            target: "missing.rs".to_string(),
            status: SymlinkStatus::Broken,
        };
        assert_eq!(
            format_map_symlink("dangling.rs", &broken),
            "dangling.rs -> missing.rs (broken)"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_repo_map_excludes_symlinks_from_outline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lib.rs"), "pub fn real_fn() {}\n").unwrap();
        std::os::unix::fs::symlink("lib.rs", root.join("alias.rs")).unwrap();

        let available = vec!["alias.rs".to_string(), "lib.rs".to_string()];
        let map = generate_repo_map(root, &available).unwrap();

        // The link is never parsed, so the declaration is rendered exactly once.
        assert_eq!(map.matches("pub fn real_fn").count(), 1);
        // The link is still listed in the file map, annotated with its target.
        assert!(map.contains("alias.rs -> lib.rs"));
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_repo_map_marks_broken_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink("missing.rs", root.join("dangling.rs")).unwrap();

        let available = vec!["dangling.rs".to_string()];
        let map = generate_repo_map(root, &available).unwrap();

        assert!(map.contains("dangling.rs -> missing.rs (broken)"));
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_repo_map_marks_outside_symlinks_without_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.rs");
        std::fs::write(&outside_file, "pub fn secret() {}\n").unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(&outside_file, root.join("secret.rs")).unwrap();

        let available = vec!["secret.rs".to_string()];
        let map = generate_repo_map(root, &available).unwrap();

        assert!(map.contains("(outside repository)"));
        // The escaping target is never fed to the symbol extractor.
        assert!(!map.contains("pub fn secret"));
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_repo_map_marks_directory_symlinks_without_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn dir_fn() {}\n").unwrap();
        std::os::unix::fs::symlink("src", root.join("crate")).unwrap();

        let available = vec!["crate".to_string(), "src/lib.rs".to_string()];
        let map = generate_repo_map(root, &available).unwrap();

        // A directory link is never traversed, so its symbols appear once.
        assert_eq!(map.matches("pub fn dir_fn").count(), 1);
        // It is still listed with its target.
        assert!(map.contains("crate -> src"));
        assert!(!map.contains("(outside repository)"));
        assert!(!map.contains("(broken)"));
    }

    #[test]
    #[cfg(unix)]
    fn test_generate_repo_map_deduplicates_repeated_symlink_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("lib.rs"), "pub fn real_fn() {}\n").unwrap();
        std::os::unix::fs::symlink("lib.rs", root.join("alias.rs")).unwrap();

        let available = vec![
            "alias.rs".to_string(),
            "alias.rs".to_string(),
            "lib.rs".to_string(),
        ];
        let map = generate_repo_map(root, &available).unwrap();

        assert_eq!(map.matches("alias.rs -> lib.rs").count(), 1);
        assert_eq!(map.matches("pub fn real_fn").count(), 1);
    }

    #[test]
    fn test_format_repo_map_retains_symlink_markers() {
        let syms = vec![RepoFileSymbols {
            path: "src/a.rs".to_string(),
            symbols: vec![Symbol::new(
                "a",
                SymbolKind::Function,
                Some("pub fn a()".into()),
                true,
                1,
            )],
            line_count: 1,
        }];
        let all_files = vec!["src/a.rs".to_string(), "alias.rs -> lib.rs".to_string()];

        let map = format_repo_map(&syms, &all_files, DetailLevel::TopLevelOnly);
        assert!(map.contains("src/a.rs (1 line)"));
        assert!(map.contains("alias.rs -> lib.rs"));
    }

    #[test]
    fn test_format_repo_map_keeps_marker_with_symbols() {
        let symbols_outline = RepoFileSymbols {
            path: "src/a.rs".to_string(),
            symbols: vec![Symbol::new(
                "a",
                SymbolKind::Function,
                Some("pub fn a()".into()),
                true,
                1,
            )],
            line_count: 1,
        };
        let all_files = vec!["src/a.rs".to_string(), "alias.rs -> lib.rs".to_string()];

        let outline = std::slice::from_ref(&symbols_outline);
        let map = format_repo_map(outline, &all_files, DetailLevel::TopLevelOnly);

        assert!(map.contains("pub fn a"));
        assert!(map.contains("alias.rs -> lib.rs"));
    }
    #[test]
    fn test_format_repo_map_lists_every_plain_file() {
        let symbols: Vec<RepoFileSymbols> = Vec::new();
        let all_files: Vec<String> = (0..10).map(|index| format!("f{index}.rs")).collect();
        let map = format_repo_map(&symbols, &all_files, DetailLevel::TopLevelOnly);
        for path in &all_files {
            assert!(map.contains(path));
        }
    }

    #[test]
    fn test_format_repo_map_deduplicates_paths_containing_parens() {
        let paren = RepoFileSymbols {
            path: "src/report (draft).rs".to_string(),
            symbols: vec![Symbol::new(
                "draft",
                SymbolKind::Function,
                Some("pub fn draft()".into()),
                true,
                1,
            )],
            line_count: 3,
        };
        let mut big_symbols = Vec::new();
        for index in 0..10 {
            big_symbols.push(Symbol::new(
                format!("big{index}"),
                SymbolKind::Function,
                Some(format!("pub fn big{index}()")),
                true,
                index + 1,
            ));
        }
        let big = RepoFileSymbols {
            path: "src/big.rs".to_string(),
            symbols: big_symbols,
            line_count: 40,
        };
        let symbols = vec![paren.clone(), big.clone()];
        let all_files = vec![paren.path.clone(), big.path.clone()];

        let full = format_repo_map(&symbols, &all_files, DetailLevel::TopLevelOnly);
        assert_eq!(full.matches("src/report (draft).rs").count(), 1);
        assert!(full.contains("src/big.rs (40 lines)"));
        assert!(full.contains("pub fn draft"));
        assert!(full.contains("pub fn big0"));
    }
}
