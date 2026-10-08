use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod treesitter;

pub use treesitter::TreeSitterExtractor;

pub const DEFAULT_REPOMAP_TOKEN_BUDGET: usize = 4000;

/// Trait defining a strategy for extracting symbols from repository files.
///
/// Allows pluggable implementations (Tree-sitter, language servers, precomputed indices).
pub trait SymbolExtractor: Send + Sync {
    fn extract_symbols(&self, repo_root: &Path, files: &[String]) -> Result<Vec<RepoFileSymbols>>;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFileSymbols {
    pub path: String,
    pub symbols: Vec<Symbol>,
}

impl RepoFileSymbols {
    pub fn new(path: impl Into<String>, symbols: Vec<Symbol>) -> Self {
        Self {
            path: path.into(),
            symbols,
        }
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
    let extractor = TreeSitterExtractor;
    generate_repo_map_with_extractor(
        repo_root,
        available_files,
        context_files,
        token_budget,
        &extractor,
    )
}

/// Generates a semantic repository map outline formatted within `token_budget` using a custom extractor.
///
/// 1. Excludes files already in active context (`context_files`).
/// 2. Queries the provided `SymbolExtractor` for code files (`.rs`).
/// 3. Discovers all Git files missing from the symbol outline and includes them in the map.
/// 4. Formats symbols and file paths into a coherent outline with adaptive token budgeting.
pub fn generate_repo_map_with_extractor(
    repo_root: &Path,
    available_files: &[String],
    context_files: &[String],
    token_budget: usize,
    extractor: &dyn SymbolExtractor,
) -> Result<String> {
    let context_set: HashSet<&str> = context_files.iter().map(|s| s.as_str()).collect();

    let mut all_git_files: Vec<String> = available_files
        .iter()
        .filter(|p| !context_set.contains(p.as_str()))
        .filter(|p| repo_root.join(p).is_file())
        .cloned()
        .collect();

    all_git_files.sort();
    all_git_files.dedup();

    if all_git_files.is_empty() {
        return Ok(String::new());
    }

    let eligible_files: Vec<String> = all_git_files
        .iter()
        .filter(|p| p.ends_with(".rs"))
        .cloned()
        .collect();

    let symbols = if eligible_files.is_empty() {
        Vec::new()
    } else {
        extractor.extract_symbols(repo_root, &eligible_files)?
    };

    let outline = format_repo_map_with_budget(&symbols, &all_git_files, token_budget);
    Ok(outline)
}

/// Formats repository symbols into an outline, reducing detail level if `token_budget` is exceeded.
pub fn format_symbols_with_budget(files: &[RepoFileSymbols], token_budget: usize) -> String {
    format_repo_map_with_budget(files, &[], token_budget)
}

/// Formats repository symbols and all other Git files into an outline within `token_budget`.
pub fn format_repo_map_with_budget(
    symbols: &[RepoFileSymbols],
    all_git_files: &[String],
    token_budget: usize,
) -> String {
    let levels = [
        DetailLevel::Full,
        DetailLevel::PublicWithDetails,
        DetailLevel::OutlinesOnly,
        DetailLevel::TopLevelOnly,
    ];

    for &level in &levels {
        let outline = format_all_files(symbols, level);
        let combined = combine_outline_and_other_files(&outline, all_git_files);
        if !combined.is_empty() && crate::prompt::estimate_tokens(&combined) <= token_budget {
            return combined;
        }
    }

    // If even TopLevelOnly + other files exceeds budget, greedily include symbol blocks then other files
    let mut accumulated: Vec<String> = Vec::new();
    let mut current_tokens = 0;
    let separator_tokens = crate::prompt::estimate_tokens("\n\n");

    for file in symbols {
        let block = format_file_symbols(file, DetailLevel::TopLevelOnly);
        if block.is_empty() {
            continue;
        }
        let block_tokens = crate::prompt::estimate_tokens(&block);
        let needed_tokens = if accumulated.is_empty() {
            block_tokens
        } else {
            block_tokens + separator_tokens
        };

        if current_tokens + needed_tokens > token_budget {
            break;
        }

        current_tokens += needed_tokens;
        accumulated.push(block);
    }

    let outline = accumulated.join("\n\n");
    let present_in_outline: HashSet<&str> = outline
        .lines()
        .filter(|l| !l.starts_with(' ') && !l.starts_with('\t') && !l.is_empty())
        .collect();

    let other_files: Vec<&str> = all_git_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| !present_in_outline.contains(f))
        .collect();

    let mut other_accumulated: Vec<&str> = Vec::new();
    for file in other_files {
        let candidate_other = if other_accumulated.is_empty() {
            file.to_string()
        } else {
            format!("{}\n{}", other_accumulated.join("\n"), file)
        };
        let candidate_full = if outline.is_empty() {
            candidate_other.clone()
        } else {
            format!("{}\n\n{}", outline, candidate_other)
        };
        if crate::prompt::estimate_tokens(&candidate_full) > token_budget {
            break;
        }
        other_accumulated.push(file);
    }

    if other_accumulated.is_empty() {
        outline
    } else {
        let other_block = other_accumulated.join("\n");
        if outline.is_empty() {
            other_block
        } else {
            format!("{}\n\n{}", outline, other_block)
        }
    }
}

fn combine_outline_and_other_files(outline: &str, all_git_files: &[String]) -> String {
    let present_in_outline: HashSet<&str> = outline
        .lines()
        .filter(|l| !l.starts_with(' ') && !l.starts_with('\t') && !l.is_empty())
        .collect();

    let other_files: Vec<&str> = all_git_files
        .iter()
        .map(|s| s.as_str())
        .filter(|f| !present_in_outline.contains(f))
        .collect();

    if other_files.is_empty() {
        return outline.to_string();
    }

    let other_block = other_files.join("\n");
    if outline.is_empty() {
        other_block
    } else {
        format!("{}\n\n{}", outline, other_block)
    }
}

fn format_all_files(files: &[RepoFileSymbols], level: DetailLevel) -> String {
    let mut blocks = Vec::new();
    for file in files {
        let block = format_file_symbols(file, level);
        if !block.is_empty() {
            blocks.push(block);
        }
    }
    blocks.join("\n\n")
}

fn format_file_symbols(file: &RepoFileSymbols, level: DetailLevel) -> String {
    let mut lines = Vec::new();
    lines.push(file.path.clone());

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

            if level == DetailLevel::Full || level == DetailLevel::PublicWithDetails {
                for field in extract_body_lines(sig) {
                    lines.push(format!("{}{}", child_indent, field));
                }
            }

            if level != DetailLevel::TopLevelOnly {
                for m in &sym.children {
                    let m_sig = m.signature.as_deref().unwrap_or(&m.name);
                    lines.push(format!("{}{}", child_indent, m_sig));
                }
            }
        }
        SymbolKind::Enum => {
            let sig = sym.signature.as_deref().unwrap_or(&sym.name);
            let header = extract_header_from_sig(sig);
            lines.push(format!("{}{}", indent, header));

            if level == DetailLevel::Full || level == DetailLevel::PublicWithDetails {
                for variant in extract_body_lines(sig) {
                    lines.push(format!("{}{}", child_indent, variant));
                }
            }
        }
        SymbolKind::Trait => {
            let header = sym.signature.as_deref().unwrap_or(&sym.name);
            lines.push(format!("{}{}", indent, header));

            if level != DetailLevel::TopLevelOnly {
                for m in &sym.children {
                    let m_sig = m.signature.as_deref().unwrap_or(&m.name);
                    lines.push(format!("{}{}", child_indent, m_sig));
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
            lines.push(format!("{}{}", indent, header));

            if level != DetailLevel::TopLevelOnly {
                for m in methods {
                    let m_sig = m.signature.as_deref().unwrap_or(&m.name);
                    lines.push(format!("{}{}", child_indent, m_sig));
                }
            }
        }
        SymbolKind::Function => {
            let sig = sym.signature.as_deref().unwrap_or(&sym.name);
            lines.push(format!("{}{}", indent, sig));
        }
        SymbolKind::Method => {
            let sig = sym.signature.as_deref().unwrap_or(&sym.name);
            lines.push(format!("{}{}", indent, sig));
        }
        SymbolKind::Mod => {
            let header = sym.signature.as_deref().unwrap_or(&sym.name);
            lines.push(format!("{}{}", indent, header));

            for child in &sym.children {
                format_symbol_into_lines(child, indent_level + 1, level, lines);
            }
        }
    }
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
                    signature: Some("pub struct PublicStruct {\n    pub id: u64,\n    pub name: String,\n}".to_string()),
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
        }];

        // Generous budget: includes full details and private symbols
        let full = format_symbols_with_budget(&syms, 500);
        assert!(full.contains("src/sample.rs"));
        assert!(full.contains("pub struct PublicStruct"));
        assert!(full.contains("pub id: u64"));
        assert!(full.contains("fn private_func()"));
        assert!(full.contains("pub fn public_func() -> bool"));

        // Tight budget: drops private symbols and struct fields
        let restricted = format_symbols_with_budget(&syms, 20);
        assert!(restricted.contains("src/sample.rs"));
        assert!(!restricted.contains("fn private_func()"));
        assert!(!restricted.contains("pub id: u64"));
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
        }];

        let outline = format_symbols_with_budget(&syms, 100);
        assert!(outline.contains("impl Greeter for User"));
        assert!(outline.contains("fn greet(&self) -> String"));
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
        }];
        let all_files = vec![
            "Cargo.toml".to_string(),
            "README.md".to_string(),
            "src/main.rs".to_string(),
        ];

        let outline = format_repo_map_with_budget(&syms, &all_files, 500);
        assert!(outline.contains("src/main.rs"));
        assert!(outline.contains("fn main()"));
        assert!(outline.contains("Cargo.toml"));
        assert!(outline.contains("README.md"));
    }

    #[test]
    fn test_format_repo_map_with_budget_only_other_files() {
        let syms = vec![];
        let all_files = vec!["Cargo.toml".to_string(), "tauqe.toml".to_string()];

        let outline = format_repo_map_with_budget(&syms, &all_files, 100);
        assert!(outline.contains("Cargo.toml"));
        assert!(outline.contains("tauqe.toml"));
    }
}
