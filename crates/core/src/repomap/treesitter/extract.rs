//! Per-language symbol extraction for the Tree-sitter repository map.
//!
//! Each supported language lives in its own submodule and maps the node kinds
//! of its official grammar onto the canonical [`crate::repomap::SymbolKind`]
//! vocabulary. The registry in `languages.rs` decides which extractor runs for
//! a file extension, so the map itself stays language-agnostic while every
//! grammar keeps its own node shapes and visibility rules.
//!
//! C and C++ share one extractor because both grammars use the same
//! translation-unit, declarator, and aggregate shapes.

mod c_family;
mod c_sharp;
mod common;
mod go;
mod java;
mod javascript;
mod php;
mod python;
mod rust;
mod typescript;

pub(super) use c_family::extract_c_family_symbols;
pub(super) use c_sharp::extract_c_sharp_symbols;
pub(super) use go::extract_go_symbols;
pub(super) use java::extract_java_symbols;
pub(super) use javascript::extract_javascript_symbols;
pub(super) use php::extract_php_symbols;
pub(super) use python::extract_python_symbols;
pub(super) use rust::extract_rust_symbols;
pub(super) use typescript::extract_typescript_symbols;
