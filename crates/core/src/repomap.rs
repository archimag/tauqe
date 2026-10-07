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
/// 2. Queries the provided `SymbolExtractor`.
/// 3. Formats symbols into a hierarchical outline with adaptive token budgeting.
pub fn generate_repo_map_with_extractor(
    repo_root: &Path,
    available_files: &[String],
    context_files: &[String],
    token_budget: usize,
    extractor: &dyn SymbolExtractor,
) -> Result<String> {
    let context_set: HashSet<&str> = context_files.iter().map(|s| s.as_str()).collect();

    let mut eligible_files: Vec<String> = available_files
        .iter()
        .filter(|p| !context_set.contains(p.as_str()))
        .filter(|p| p.ends_with(".rs"))
        .filter(|p| repo_root.join(p).is_file())
        .cloned()
        .collect();

    eligible_files.sort();

    if eligible_files.is_empty() {
        return Ok(String::new());
    }

    let symbols = extractor.extract_symbols(repo_root, &eligible_files)?;
    let outline = format_symbols_with_budget(&symbols, token_budget);
    Ok(outline)
}

/// Formats repository symbols into an outline, reducing detail level if `token_budget` is exceeded.
pub fn format_symbols_with_budget(files: &[RepoFileSymbols], token_budget: usize) -> String {
    let levels = [
        DetailLevel::Full,
        DetailLevel::PublicWithDetails,
        DetailLevel::OutlinesOnly,
        DetailLevel::TopLevelOnly,
    ];

    for &level in &levels {
        let formatted = format_all_files(files, level);
        if !formatted.is_empty() && crate::prompt::estimate_tokens(&formatted) <= token_budget {
            return formatted;
        }
    }

    // If even TopLevelOnly exceeds budget, greedily include files until budget is reached
    let mut accumulated: Vec<String> = Vec::new();
    let mut current_tokens = 0;
    let separator_tokens = crate::prompt::estimate_tokens("\n\n");

    for file in files {
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

    accumulated.join("\n\n")
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
}
