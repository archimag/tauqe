use std::path::Path;

use tree_sitter::Node;

use crate::repomap::Symbol;

use super::extract::{
    extract_c_family_symbols, extract_c_sharp_symbols, extract_go_symbols, extract_java_symbols,
    extract_javascript_symbols, extract_php_symbols, extract_python_symbols, extract_rust_symbols,
    extract_typescript_symbols,
};

/// Extracts top-level symbols from a parsed syntax tree.
pub(super) type SymbolExtraction =
    for<'tree, 'source> fn(Node<'tree>, &'source [u8]) -> Vec<Symbol>;

/// Tree-sitter language support entry covering a set of file extensions.
pub(super) struct LanguageSupport {
    pub(super) extensions: &'static [&'static str],
    pub(super) language: fn() -> tree_sitter::Language,
    pub(super) extract: SymbolExtraction,
}

/// Registry of languages the repo map can parse.
///
/// The map stays language-agnostic: supporting a new language means adding an
/// entry here with its official grammar and top-level symbol extractor. Files
/// without a registered extension still appear in the map as plain paths, which
/// is the expected fallback for languages without symbol support.
///
/// TypeScript and TSX need two grammars, so they register the same extractor
/// under separate entries: the node kinds are identical, only the grammar that
/// produces them differs. C and C++ likewise share one extractor, and a header
/// is parsed with the C grammar unless it carries a C++ extension.
const LANGUAGE_REGISTRY: &[LanguageSupport] = &[
    LanguageSupport {
        extensions: &["rs"],
        language: tree_sitter_rust::language,
        extract: extract_rust_symbols,
    },
    LanguageSupport {
        extensions: &["ts"],
        language: tree_sitter_typescript::language_typescript,
        extract: extract_typescript_symbols,
    },
    LanguageSupport {
        extensions: &["tsx"],
        language: tree_sitter_typescript::language_tsx,
        extract: extract_typescript_symbols,
    },
    LanguageSupport {
        extensions: &["js", "jsx", "mjs", "cjs"],
        language: tree_sitter_javascript::language,
        extract: extract_javascript_symbols,
    },
    LanguageSupport {
        extensions: &["py"],
        language: tree_sitter_python::language,
        extract: extract_python_symbols,
    },
    LanguageSupport {
        extensions: &["go"],
        language: tree_sitter_go::language,
        extract: extract_go_symbols,
    },
    LanguageSupport {
        extensions: &["java"],
        language: tree_sitter_java::language,
        extract: extract_java_symbols,
    },
    LanguageSupport {
        extensions: &["c", "h"],
        language: tree_sitter_c::language,
        extract: extract_c_family_symbols,
    },
    LanguageSupport {
        extensions: &["cc", "cpp", "cxx", "hpp"],
        language: tree_sitter_cpp::language,
        extract: extract_c_family_symbols,
    },
    LanguageSupport {
        extensions: &["cs"],
        language: tree_sitter_c_sharp::language,
        extract: extract_c_sharp_symbols,
    },
    LanguageSupport {
        extensions: &["php"],
        language: tree_sitter_php::language_php,
        extract: extract_php_symbols,
    },
];

/// Returns whether `path` carries an extension the registry can parse.
pub fn is_supported_path(path: &str) -> bool {
    language_for_path(path).is_some()
}

/// Returns the registry entry registered for `path`'s extension, if any.
pub(super) fn language_for_path(path: &str) -> Option<&'static LanguageSupport> {
    let extension = Path::new(path).extension()?.to_str()?;
    LANGUAGE_REGISTRY
        .iter()
        .find(|support| support.extensions.contains(&extension))
}
