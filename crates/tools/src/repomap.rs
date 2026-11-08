use std::collections::HashSet;
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

pub const DEFAULT_REPOMAP_TOKEN_BUDGET: usize = 4000;

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
    pub line: usize,
    pub children: Vec<Symbol>,
}

impl Symbol {
    pub fn new(
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
    pub path: String,
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub line_count: usize,
}

impl RepoFileSymbols {
    pub fn new(path: impl Into<String>, symbols: Vec<Symbol>) -> Self {
        Self {
            path: path.into(),
            symbols,
            line_count: 0,
        }
    }

    pub fn with_line_count(mut self, line_count: usize) -> Self {
        self.line_count = line_count;
        self
    }
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

/// Generates a semantic repository map outline formatted within `token_budget` using the default Tree-sitter extractor.
pub fn generate_repo_map(
    repo_root: &Path,
    available_files: &[String],
    context_files: &[String],
    token_budget: usize,
) -> Result<String> {
    generate_repo_map_with_extractor(
        repo_root,
        available_files,
        context_files,
        token_budget,
        treesitter::shared_extractor(),
    )
}

/// Generates a semantic repository map outline formatted within `token_budget` using a custom extractor.
///
/// 1. Excludes files already in active context (`context_files`) and drops paths
///    under a service directory (`.git`, `.tauqe`, `target`, `node_modules`), so
///    the symbol map reports the same repository shape as the directory outline.
/// 2. Queries the provided `SymbolExtractor` for supported code files.
/// 3. Discovers all Git files missing from the symbol outline and includes them in the map.
/// 4. Formats symbols and file paths into a coherent outline with adaptive token budgeting.
pub fn generate_repo_map_with_extractor(
    repo_root: &Path,
    available_files: &[String],
    context_files: &[String],
    token_budget: usize,
    extractor: &dyn SymbolExtractor,
) -> Result<String> {
    crate::blocking::run_blocking(|| {
        let context_set: HashSet<&str> = context_files.iter().map(|s| s.as_str()).collect();

        let mut all_git_files: Vec<String> = Vec::new();
        let mut eligible_files: Vec<String> = Vec::new();

        for path in available_files {
            if context_set.contains(path.as_str()) {
                continue;
            }
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

        let outline = format_repo_map_with_budget(&symbols, &all_git_files, token_budget);
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
/// Unlike the eager global map, a targeted map never drops files that are
/// already in context: the request exists precisely to read the symbols of the
/// subsystem the model is about to navigate. The outline is rendered through the
/// same adaptive token budget as the global map, and an unknown prefix yields an
/// empty string instead of a repository-wide outline.
pub fn generate_targeted_repo_map(
    repo_root: &Path,
    available_files: &[String],
    target_path: &str,
    token_budget: usize,
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
    generate_repo_map(repo_root, &targeted, &[], token_budget)
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

/// Formats repository symbols into an outline, reducing detail level if `token_budget` is exceeded.
pub fn format_symbols_with_budget<S: AsRef<RepoFileSymbols>>(
    files: &[S],
    token_budget: usize,
) -> String {
    format_repo_map_with_budget(files, &[], token_budget)
}

/// Formats repository symbols and all other Git files into an outline within `token_budget`.
///
/// Every detail level is priced from the byte lengths its blocks and the
/// remaining plain entries occupy once joined, because the token estimate is a
/// pure function of that length: a level that does not fit never materializes
/// the document it would immediately discard, and the winning level is joined
/// once, into a buffer sized by the count the fit decision already computed.
///
/// Accepts both owned payloads and shared `Arc` payloads, so map generation
/// formats cached symbols without cloning them.
pub fn format_repo_map_with_budget<S: AsRef<RepoFileSymbols>>(
    symbols: &[S],
    all_git_files: &[String],
    token_budget: usize,
) -> String {
    let levels = [
        DetailLevel::Full,
        DetailLevel::PublicWithDetails,
        DetailLevel::OutlinesOnly,
        DetailLevel::TopLevelOnly,
    ];

    // The cheapest level also seeds the greedy fallback below, so the last
    // candidate leaves the loop instead of being rendered a second time.
    let mut cheapest: Option<Vec<RenderedBlock>> = None;

    for &level in &levels {
        let blocks = format_all_files(symbols, level);
        let block_texts: Vec<&str> = blocks.iter().map(|block| block.text.as_str()).collect();
        let entries = plain_entries(&blocks, all_git_files);
        let byte_len = combined_bytes(
            joined_bytes(&block_texts, BLOCK_SEPARATOR),
            joined_bytes(&entries, ENTRY_SEPARATOR),
        );
        if byte_len > 0 && fits_budget(byte_len, token_budget) {
            return join_document(&block_texts, &entries, byte_len);
        }
        cheapest = Some(blocks);
    }

    // The level list is never empty, so a candidate always exists; the
    // degenerate answer stays an empty outline instead of a panic.
    cheapest
        .map(|blocks| greedy_outline(&blocks, all_git_files, token_budget))
        .unwrap_or_default()
}

/// The blank line separating two rendered symbol blocks.
const BLOCK_SEPARATOR: &str = "\n\n";

/// The newline separating two plain repository entries.
const ENTRY_SEPARATOR: &str = "\n";

/// One rendered symbol block paired with the repository path it describes.
///
/// A rendered header carries a ` (N lines)` suffix and a repository path may
/// itself contain ` (`, so an outline line cannot be parsed back into a path:
/// splitting on the first separator truncates such a path and leaves the file
/// to be listed a second time among the plain entries. The path therefore
/// travels beside its block, taken from the structured `RepoFileSymbols::path`
/// field while the block is produced.
struct RenderedBlock {
    path: String,
    text: String,
}

/// Renders every symbol block at `level`, in repository order.
fn format_all_files<S: AsRef<RepoFileSymbols>>(
    files: &[S],
    level: DetailLevel,
) -> Vec<RenderedBlock> {
    let mut blocks: Vec<RenderedBlock> = Vec::new();
    for file in files {
        let file = file.as_ref();
        let text = format_file_symbols(file, level);
        if !text.is_empty() {
            blocks.push(RenderedBlock {
                path: file.path.clone(),
                text,
            });
        }
    }
    blocks
}

/// The repository entries the outline does not name, in repository order.
fn plain_entries<'a>(blocks: &[RenderedBlock], all_git_files: &'a [String]) -> Vec<&'a str> {
    let rendered: HashSet<&str> = blocks.iter().map(|block| block.path.as_str()).collect();
    all_git_files
        .iter()
        .map(|path| path.as_str())
        .filter(|path| !rendered.contains(*path))
        .collect()
}

/// Byte length a list of items occupies once joined by `separator`.
fn joined_bytes<T: AsRef<str>>(items: &[T], separator: &str) -> usize {
    let content: usize = items.iter().map(|item| item.as_ref().len()).sum();
    let separators = separator.len() * items.len().saturating_sub(1);
    content + separators
}

/// Bytes `separator` contributes before an item appended to a non-empty list.
fn separator_bytes<T>(items: &[T], separator: &str) -> usize {
    if items.is_empty() {
        0
    } else {
        separator.len()
    }
}

/// Byte length of the document assembled from a joined outline and the plain
/// entries that follow it.
fn combined_bytes(outline_bytes: usize, entries_bytes: usize) -> usize {
    let separator = if outline_bytes > 0 && entries_bytes > 0 {
        BLOCK_SEPARATOR.len()
    } else {
        0
    };
    outline_bytes + separator + entries_bytes
}

/// Reports whether a document of `byte_len` bytes fits the token budget.
///
/// The token estimate is a pure function of the byte length, so a candidate is
/// priced from its bytes alone and only a candidate that passes this check is
/// materialized.
fn fits_budget(byte_len: usize, token_budget: usize) -> bool {
    crate::tokens::estimate_tokens_from_bytes(byte_len as u64) <= token_budget as u64
}

/// Materializes the priced document exactly once, into a buffer sized by the
/// byte count the fit decision already computed.
fn join_document(blocks: &[&str], entries: &[&str], byte_len: usize) -> String {
    let mut document = String::with_capacity(byte_len);
    let mut pending = "";
    for block in blocks {
        document.push_str(pending);
        document.push_str(block);
        pending = BLOCK_SEPARATOR;
    }
    for entry in entries {
        document.push_str(pending);
        document.push_str(entry);
        pending = ENTRY_SEPARATOR;
    }
    document
}

/// Extends the cheapest rendered outline with the plain entries it leaves out
/// until the budget is exhausted.
///
/// The blocks arrive already rendered at the cheapest level, and every addition
/// is priced by the byte length the joined document would gain, so the fallback
/// neither renders the repository a second time nor rounds a per-block token
/// estimate against a budget that is checked over the whole document.
fn greedy_outline(
    blocks: &[RenderedBlock],
    all_git_files: &[String],
    token_budget: usize,
) -> String {
    let mut included: Vec<&RenderedBlock> = Vec::new();
    let mut outline_bytes = 0usize;
    for block in blocks {
        let candidate_bytes =
            outline_bytes + separator_bytes(&included, BLOCK_SEPARATOR) + block.text.len();
        if !fits_budget(candidate_bytes, token_budget) {
            break;
        }
        outline_bytes = candidate_bytes;
        included.push(block);
    }

    let rendered: HashSet<&str> = included.iter().map(|block| block.path.as_str()).collect();
    let mut entries: Vec<&str> = Vec::new();
    let mut entries_bytes = 0usize;
    for path in all_git_files {
        if rendered.contains(path.as_str()) {
            continue;
        }
        // Listed files are joined by single newlines, so only a file that
        // already has a predecessor adds a separator byte.
        let candidate_bytes =
            entries_bytes + separator_bytes(&entries, ENTRY_SEPARATOR) + path.len();
        if !fits_budget(combined_bytes(outline_bytes, candidate_bytes), token_budget) {
            break;
        }
        entries_bytes = candidate_bytes;
        entries.push(path.as_str());
    }

    let block_texts: Vec<&str> = included.iter().map(|block| block.text.as_str()).collect();
    let byte_len = combined_bytes(outline_bytes, entries_bytes);
    join_document(&block_texts, &entries, byte_len)
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

        // Generous budget: includes full outlines and private symbols
        let full = format_symbols_with_budget(&syms, 500);
        assert!(full.contains("src/sample.rs (10 lines)"));
        assert!(full.contains("pub struct PublicStruct"));
        assert!(!full.contains("pub id: u64"));
        assert!(full.contains("fn private_func"));
        assert!(full.contains("pub fn public_func"));
        assert!(!full.contains("pub fn public_func() -> bool"));

        // Tight budget: drops private symbols
        let restricted = format_symbols_with_budget(&syms, 20);
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

        // Budget enough for one file's outline (~6 tokens) but not both (~11 tokens)
        let formatted = format_symbols_with_budget(&syms, 10);
        assert!(formatted.contains("src/a.rs"));
        assert!(!formatted.contains("src/b.rs"));
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

        let outline = format_symbols_with_budget(&syms, 100);
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

        let outline = format_repo_map_with_budget(&syms, &all_files, 500);
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

        let outline = format_symbols_with_budget(&syms, 100);
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

        let outline = format_repo_map_with_budget(&syms, &all_files, 100);
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

        let map = generate_targeted_repo_map(root, &available, "crates/core", 1000).unwrap();
        assert!(map.contains("crates/core/src/lib.rs"));
        assert!(map.contains("pub fn core_fn"));
        // A sibling directory sharing only a textual stem is excluded.
        assert!(!map.contains("crates/protocol"));
        assert!(!map.contains("crates/core-utils"));
        assert!(!map.contains("pub fn utils_fn"));

        // A cosmetic `./` prefix and a trailing slash name the same subtree.
        let dotted = generate_targeted_repo_map(root, &available, "./crates/core/", 1000).unwrap();
        assert_eq!(dotted, map);

        // Naming a single file narrows the outline to that file.
        let single =
            generate_targeted_repo_map(root, &available, "crates/core/src/lib.rs", 1000).unwrap();
        assert!(single.contains("pub fn core_fn"));
        assert!(!single.contains("crates/protocol"));

        // An unknown prefix yields an empty outline instead of a full map.
        assert!(
            generate_targeted_repo_map(root, &available, "missing", 1000)
                .unwrap()
                .is_empty()
        );

        // An empty prefix covers the whole repository.
        let whole = generate_targeted_repo_map(root, &available, "", 1000).unwrap();
        assert!(whole.contains("pub fn core_fn"));
        assert!(whole.contains("pub fn protocol_fn"));
    }

    #[test]
    fn test_generate_targeted_repo_map_honors_the_token_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
        let available = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];

        let generous = generate_targeted_repo_map(root, &available, "src", 1000).unwrap();
        assert!(generous.contains("src/a.rs"));
        assert!(generous.contains("src/b.rs"));

        let tight = generate_targeted_repo_map(root, &available, "src", 2).unwrap();
        assert!(crate::tokens::estimate_tokens(&tight) <= 2);
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

        let map = generate_repo_map(root, &available, &[], 1000).unwrap();
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
        let context = vec![];

        let handle =
            tokio::spawn(
                async move { generate_repo_map(&root, &available, &context, 1000).unwrap() },
            );

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
        let map = generate_repo_map(root, &available, &[], 1000).unwrap();

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
        let map = generate_repo_map(root, &available, &[], 1000).unwrap();

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
        let map = generate_repo_map(root, &available, &[], 1000).unwrap();

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
        let map = generate_repo_map(root, &available, &[], 1000).unwrap();

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
        let map = generate_repo_map(root, &available, &[], 1000).unwrap();

        assert_eq!(map.matches("alias.rs -> lib.rs").count(), 1);
        assert_eq!(map.matches("pub fn real_fn").count(), 1);
    }

    #[test]
    fn test_format_repo_map_budget_drops_symlink_markers() {
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

        let generous = format_repo_map_with_budget(&syms, &all_files, 500);
        assert!(generous.contains("src/a.rs (1 line)"));
        assert!(generous.contains("alias.rs -> lib.rs"));

        // A budget below the cost of the marker drops it instead of overshooting.
        let tight = format_repo_map_with_budget(&syms, &all_files, 1);
        assert!(!tight.contains("alias.rs -> lib.rs"));
        assert!(crate::tokens::estimate_tokens(&tight) <= 1);
    }

    #[test]
    fn test_format_repo_map_greedy_truncation_keeps_marker_within_budget() {
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

        // Budget admits the symbol block but not the additional marker entry.
        let outline = std::slice::from_ref(&symbols_outline);
        let symbol_only = format_repo_map_with_budget(outline, &[], 500);
        let budget = crate::tokens::estimate_tokens(&symbol_only) + 2;
        let truncated = format_repo_map_with_budget(outline, &all_files, budget);

        assert!(truncated.contains("pub fn a"));
        assert!(!truncated.contains("alias.rs -> lib.rs"));
        assert!(crate::tokens::estimate_tokens(&truncated) <= budget);
    }
    #[test]
    fn test_format_repo_map_greedy_other_files_follow_the_exact_byte_budget() {
        // Without symbols the outline stays empty, so the greedy fallback lists
        // plain file names and the budget alone decides how many it admits.
        let symbols: Vec<RepoFileSymbols> = Vec::new();
        let all_files: Vec<String> = (0..10).map(|index| format!("f{index}.rs")).collect();

        // Every name is five bytes, and neighbors are joined by one newline, so
        // each additional name grows the block by six bytes.
        let two = "f0.rs\nf1.rs";
        let three = "f0.rs\nf1.rs\nf2.rs";

        let budget_two = crate::tokens::estimate_tokens(two);
        let budget_three = crate::tokens::estimate_tokens(three);

        assert_eq!(
            format_repo_map_with_budget(&symbols, &all_files, budget_two),
            two
        );
        assert_eq!(
            format_repo_map_with_budget(&symbols, &all_files, budget_three),
            three
        );
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

        // A generous budget renders both symbol blocks, so the entry list
        // stays empty and the path appears exactly once.
        let full = format_repo_map_with_budget(&symbols, &all_files, 1000);
        assert_eq!(full.matches("src/report (draft).rs").count(), 1);
        assert!(full.contains("src/big.rs (40 lines)"));

        // A budget that admits one symbol block forces the greedy fallback,
        // which lists the truncated file as a plain entry. The rendered path
        // must not be listed again beside its own block.
        let paren_block = format_file_symbols(&symbols[0], DetailLevel::TopLevelOnly);
        let greedy_bytes = format!("{}\n\n{}\n{}", paren_block, paren.path, big.path);
        let budget = crate::tokens::estimate_tokens(&greedy_bytes);
        let truncated = format_repo_map_with_budget(&symbols, &all_files, budget);
        assert!(truncated.contains("pub fn draft"));
        assert!(!truncated.contains("pub fn big0"));
        assert_eq!(truncated.matches("src/report (draft).rs").count(), 1);
        assert!(truncated.contains("src/big.rs"));
    }
}
