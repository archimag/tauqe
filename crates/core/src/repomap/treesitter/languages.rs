use std::path::Path;

use tree_sitter::Node;

use crate::repomap::Symbol;

use super::extract::extract_container_symbols;

/// Extracts top-level symbols from a parsed syntax tree.
pub(super) type SymbolExtraction = for<'tree, 'source> fn(Node<'tree>, &'source [u8]) -> Vec<Symbol>;

/// Tree-sitter language support entry covering a set of file extensions.
pub(super) struct LanguageSupport {
    pub(super) extensions: &'static [&'static str],
    pub(super) language: fn() -> tree_sitter::Language,
    pub(super) extract: SymbolExtraction,
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
pub(super) fn language_for_path(path: &str) -> Option<&'static LanguageSupport> {
    let extension = Path::new(path).extension()?.to_str()?;
    LANGUAGE_REGISTRY
        .iter()
        .find(|support| support.extensions.contains(&extension))
}
