use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::repomap::{RepoFileSymbols, Symbol, SymbolKind};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawRustdocOutput {
    pub root: Option<String>,
    pub index: HashMap<String, RawItem>,
    pub format_version: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawItem {
    pub id: Option<String>,
    pub crate_id: Option<u32>,
    pub name: Option<String>,
    pub span: Option<RawSpan>,
    pub visibility: Option<serde_json::Value>,
    pub docs: Option<String>,
    pub inner: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawSpan {
    pub filename: Option<String>,
    pub begin: Option<serde_json::Value>,
    pub end: Option<serde_json::Value>,
}

impl RawSpan {
    pub fn line(&self) -> usize {
        if let Some(begin) = &self.begin {
            if let Some(arr) = begin.as_array() {
                if let Some(first) = arr.first().and_then(|v| v.as_u64()) {
                    return first as usize;
                }
            } else if let Some(n) = begin.as_u64() {
                return n as usize;
            }
        }
        1
    }
}

/// Executes cargo rustdoc with JSON output format and parses generated documentation into IR.
///
/// Immediately returns Err on any non-zero exit code or failure (e.g. nightly unavailable,
/// unstable options disabled, compilation errors), cleanly falling back to Tree-sitter.
pub fn extract_rustdoc_symbols(
    repo_root: &Path,
    files: &[String],
) -> Result<Vec<RepoFileSymbols>> {
    let cargo_toml = repo_root.join("Cargo.toml");
    if !cargo_toml.is_file() {
        bail!("No Cargo.toml found in repo root: {:?}", repo_root);
    }

    let output = Command::new("cargo")
        .current_dir(repo_root)
        .args(["rustdoc", "--", "-Z", "unstable-options", "--output-format", "json"])
        .output()
        .context("Failed to spawn cargo rustdoc command")?;

    if !output.status.success() {
        bail!(
            "cargo rustdoc exited with non-zero status {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let doc_dir = repo_root.join("target").join("doc");
    if !doc_dir.is_dir() {
        bail!("target/doc directory not found after cargo rustdoc execution");
    }

    let mut json_files = Vec::new();
    for entry in std::fs::read_dir(&doc_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() && p.extension().is_some_and(|ext| ext == "json") {
            json_files.push(p);
        }
    }

    if json_files.is_empty() {
        bail!("No rustdoc JSON output files found in {:?}", doc_dir);
    }

    let mut all_results = Vec::new();
    for json_file in json_files {
        let content = std::fs::read_to_string(&json_file)?;
        let parsed = parse_rustdoc_json(repo_root, &content, files)?;
        all_results.extend(parsed);
    }

    let mut map: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();
    for item in all_results {
        map.entry(item.path).or_default().extend(item.symbols);
    }

    let mut final_results: Vec<RepoFileSymbols> = map
        .into_iter()
        .map(|(path, mut symbols)| {
            symbols.sort_by_key(|s| s.line);
            RepoFileSymbols { path, symbols }
        })
        .collect();

    final_results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(final_results)
}

/// Parses rustdoc JSON string and converts workspace entities into unified IR (RepoFileSymbols).
pub fn parse_rustdoc_json(
    repo_root: &Path,
    json_str: &str,
    target_files: &[String],
) -> Result<Vec<RepoFileSymbols>> {
    let raw: RawRustdocOutput = serde_json::from_str(json_str)?;
    let target_set: HashSet<&str> = target_files.iter().map(|s| s.as_str()).collect();

    // Collect child IDs so they aren't processed as top-level free symbols
    let mut child_ids = HashSet::new();
    for item in raw.index.values() {
        if let Some((kind, payload)) = get_item_kind_and_payload(item) {
            match kind {
                "struct" => {
                    let field_ids = payload
                        .get("kind")
                        .and_then(|k| k.get("plain"))
                        .and_then(|p| p.get("fields"))
                        .or_else(|| payload.get("fields"))
                        .and_then(|f| f.as_array());
                    if let Some(fields) = field_ids {
                        for f in fields {
                            if let Some(id) = f.as_str() {
                                child_ids.insert(id.to_string());
                            }
                        }
                    }
                }
                "enum" => {
                    if let Some(vars) = payload.get("variants").and_then(|v| v.as_array()) {
                        for v in vars {
                            if let Some(id) = v.as_str() {
                                child_ids.insert(id.to_string());
                            }
                        }
                    }
                }
                "trait" | "impl" => {
                    if let Some(items) = payload.get("items").and_then(|i| i.as_array()) {
                        for it in items {
                            if let Some(id) = it.as_str() {
                                child_ids.insert(id.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut file_symbols_map: BTreeMap<String, Vec<Symbol>> = BTreeMap::new();

    for (item_id, item) in &raw.index {
        if child_ids.contains(item_id) {
            continue;
        }

        let Some(rel_path) = get_relative_file_path(repo_root, &item.span) else {
            continue;
        };

        if !target_set.is_empty() && !target_set.contains(rel_path.as_str()) {
            continue;
        }

        let Some((kind, payload)) = get_item_kind_and_payload(item) else {
            continue;
        };

        let line = item.span.as_ref().map(|s| s.line()).unwrap_or(1);
        let is_pub = is_public_vis(&item.visibility);

        match kind {
            "struct" => {
                let name = item.name.as_deref().unwrap_or("AnonymousStruct");
                let sig = extract_struct_signature(name, is_pub, payload, &raw.index);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Struct,
                    signature: Some(sig),
                    is_public: is_pub,
                    line,
                    docs: item.docs.clone(),
                    children: Vec::new(),
                });
            }
            "enum" => {
                let name = item.name.as_deref().unwrap_or("AnonymousEnum");
                let sig = extract_enum_signature(name, is_pub, payload, &raw.index);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Enum,
                    signature: Some(sig),
                    is_public: is_pub,
                    line,
                    docs: item.docs.clone(),
                    children: Vec::new(),
                });
            }
            "trait" => {
                let name = item.name.as_deref().unwrap_or("AnonymousTrait");
                let vis = if is_pub { "pub " } else { "" };
                let sig = format!("{}trait {}", vis, name);
                let methods = extract_methods(payload.get("items"), &raw.index, true);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Trait,
                    signature: Some(sig),
                    is_public: is_pub,
                    line,
                    docs: item.docs.clone(),
                    children: methods,
                });
            }
            "impl" => {
                if payload.get("synthetic").and_then(|v| v.as_bool()) == Some(true)
                    || payload.get("blanket_impl").is_some()
                {
                    continue;
                }
                let for_type = format_type(&payload["for"]);
                if for_type.is_empty() {
                    continue;
                }
                let trait_type = payload.get("trait").map(format_type).filter(|s| !s.is_empty());
                let header = match trait_type {
                    Some(t) => format!("impl {} for {}", t, for_type),
                    None => format!("impl {}", for_type),
                };
                let methods = extract_methods(payload.get("items"), &raw.index, false);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: header.clone(),
                    kind: SymbolKind::Impl,
                    signature: Some(header),
                    is_public: true,
                    line,
                    docs: item.docs.clone(),
                    children: methods,
                });
            }
            "function" => {
                let name = item.name.as_deref().unwrap_or("anon_fn");
                let sig = extract_function_signature(name, is_pub, payload);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    signature: Some(sig),
                    is_public: is_pub,
                    line,
                    docs: item.docs.clone(),
                    children: Vec::new(),
                });
            }
            "module" => {
                if payload.get("is_crate").and_then(|v| v.as_bool()) == Some(true) {
                    continue;
                }
                let name = item.name.as_deref().unwrap_or("anon_mod");
                let vis = if is_pub { "pub " } else { "" };
                let sig = format!("{}mod {}", vis, name);
                file_symbols_map.entry(rel_path).or_default().push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Mod,
                    signature: Some(sig),
                    is_public: is_pub,
                    line,
                    docs: item.docs.clone(),
                    children: Vec::new(),
                });
            }
            _ => {}
        }
    }

    let mut results: Vec<RepoFileSymbols> = file_symbols_map
        .into_iter()
        .map(|(path, mut symbols)| {
            symbols.sort_by_key(|s| s.line);
            RepoFileSymbols { path, symbols }
        })
        .collect();

    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(results)
}

fn get_item_kind_and_payload(item: &RawItem) -> Option<(&str, &serde_json::Value)> {
    if let Some(inner) = &item.inner {
        if let Some(obj) = inner.as_object() {
            if let Some((k, v)) = obj.iter().next() {
                return Some((k.as_str(), v));
            }
        }
    }
    None
}

fn is_public_vis(vis: &Option<serde_json::Value>) -> bool {
    match vis {
        Some(serde_json::Value::String(s)) => s == "public",
        Some(serde_json::Value::Object(obj)) => obj.contains_key("public"),
        _ => false,
    }
}

fn get_relative_file_path(repo_root: &Path, span: &Option<RawSpan>) -> Option<String> {
    let span = span.as_ref()?;
    let filename_str = span.filename.as_deref()?;
    if filename_str.is_empty() {
        return None;
    }

    let p = Path::new(filename_str);
    let rel_path = if p.is_absolute() {
        match p.strip_prefix(repo_root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => return None, // External crate or stdlib file outside workspace root
        }
    } else {
        p.to_path_buf()
    };

    let normalized = rel_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn extract_methods(
    item_ids: Option<&serde_json::Value>,
    raw_index: &HashMap<String, RawItem>,
    default_pub: bool,
) -> Vec<Symbol> {
    let mut methods = Vec::new();
    let Some(arr) = item_ids.and_then(|v| v.as_array()) else {
        return methods;
    };

    for id_val in arr {
        let Some(id) = id_val.as_str() else { continue; };
        let Some(item) = raw_index.get(id) else { continue; };
        let Some((kind, payload)) = get_item_kind_and_payload(item) else { continue; };
        if kind == "function" || kind == "method" {
            let name = item.name.as_deref().unwrap_or("anon_fn");
            let is_pub = default_pub || is_public_vis(&item.visibility);
            let line = item.span.as_ref().map(|s| s.line()).unwrap_or(1);
            let sig = extract_function_signature(name, is_pub, payload);
            methods.push(Symbol {
                name: name.to_string(),
                kind: SymbolKind::Method,
                signature: Some(sig),
                is_public: is_pub,
                line,
                docs: item.docs.clone(),
                children: Vec::new(),
            });
        }
    }
    methods.sort_by_key(|m| m.line);
    methods
}

fn extract_function_signature(
    name: &str,
    is_pub: bool,
    payload: &serde_json::Value,
) -> String {
    let decl = payload.get("decl");
    let mut args = Vec::new();

    if let Some(inputs) = decl.and_then(|d| d.get("inputs")).and_then(|i| i.as_array()) {
        for input in inputs {
            if let Some(arr) = input.as_array() {
                let arg_name = arr.first().and_then(|v| v.as_str()).unwrap_or("_");
                if arg_name == "self" {
                    if let Some(type_val) = arr.get(1) {
                        let self_str = match type_val.get("borrowed_ref") {
                            Some(b) if b.get("mutable").and_then(|m| m.as_bool()) == Some(true) => {
                                "&mut self"
                            }
                            Some(_) => "&self",
                            None => "self",
                        };
                        args.push(self_str.to_string());
                        continue;
                    } else {
                        args.push("self".to_string());
                        continue;
                    }
                }
                let type_str = arr.get(1).map(format_type).unwrap_or_default();
                if type_str.is_empty() {
                    args.push(arg_name.to_string());
                } else {
                    args.push(format!("{}: {}", arg_name, type_str));
                }
            } else if let Some(arg_str) = input.as_str() {
                args.push(arg_str.to_string());
            }
        }
    }

    let ret_str = if let Some(output) = decl.and_then(|d| d.get("output")) {
        let ret = format_type(output);
        if !ret.is_empty() && ret != "()" {
            format!(" -> {}", ret)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let header = payload.get("header");
    let mut mods = Vec::new();
    if header_has(header, "const") {
        mods.push("const");
    }
    if header_has(header, "async") {
        mods.push("async");
    }
    if header_has(header, "unsafe") {
        mods.push("unsafe");
    }

    let vis = if is_pub { "pub " } else { "" };
    let mod_prefix = if mods.is_empty() {
        String::new()
    } else {
        format!("{} ", mods.join(" "))
    };

    format!("{}{}fn {}({}){}", vis, mod_prefix, name, args.join(", "), ret_str)
}

fn header_has(header: Option<&serde_json::Value>, flag: &str) -> bool {
    let Some(h) = header else { return false; };
    if let Some(arr) = h.as_array() {
        arr.iter().any(|v| v.as_str() == Some(flag))
    } else if let Some(obj) = h.as_object() {
        obj.get(flag).and_then(|v| v.as_bool()).unwrap_or(false)
    } else {
        false
    }
}

fn format_type(val: &serde_json::Value) -> String {
    if let Some(s) = val.as_str() {
        return s.to_string();
    }
    if let Some(prim) = val.get("primitive").and_then(|v| v.as_str()) {
        return prim.to_string();
    }
    if let Some(path) = val.get("resolved_path") {
        let name = path.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let mut generic_parts = Vec::new();
        if let Some(args) = path
            .get("args")
            .and_then(|a| a.get("angle_bracketed"))
            .and_then(|ab| ab.get("args"))
            .and_then(|a| a.as_array())
        {
            for arg in args {
                if let Some(ty) = arg.get("type") {
                    let formatted = format_type(ty);
                    if !formatted.is_empty() {
                        generic_parts.push(formatted);
                    }
                } else {
                    let formatted = format_type(arg);
                    if !formatted.is_empty() {
                        generic_parts.push(formatted);
                    }
                }
            }
        }
        if !generic_parts.is_empty() {
            return format!("{}<{}>", name, generic_parts.join(", "));
        }
        return name.to_string();
    }
    if let Some(b) = val.get("borrowed_ref") {
        let is_mut = b.get("mutable").and_then(|m| m.as_bool()).unwrap_or(false);
        let inner = b.get("type").map(format_type).unwrap_or_default();
        if is_mut {
            return format!("&mut {}", inner);
        } else {
            return format!("&{}", inner);
        }
    }
    if let Some(slice) = val.get("slice") {
        return format!("[{}]", format_type(slice));
    }
    if let Some(arr) = val.get("tuple").and_then(|t| t.as_array()) {
        let items: Vec<String> = arr.iter().map(format_type).collect();
        return format!("({})", items.join(", "));
    }
    if let Some(gen) = val.get("generic").and_then(|v| v.as_str()) {
        return gen.to_string();
    }
    if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
        return name.to_string();
    }
    String::new()
}

fn extract_struct_signature(
    name: &str,
    is_pub: bool,
    payload: &serde_json::Value,
    raw_index: &HashMap<String, RawItem>,
) -> String {
    let vis = if is_pub { "pub " } else { "" };
    let mut field_lines = Vec::new();

    let field_ids = payload
        .get("kind")
        .and_then(|k| k.get("plain"))
        .and_then(|p| p.get("fields"))
        .or_else(|| payload.get("fields"))
        .and_then(|f| f.as_array());

    if let Some(ids) = field_ids {
        for id_val in ids {
            let field_id = id_val.as_str().unwrap_or_default();
            if let Some(field_item) = raw_index.get(field_id) {
                let f_name = field_item.name.as_deref().unwrap_or("anon");
                let f_pub = is_public_vis(&field_item.visibility);
                let f_vis = if f_pub { "pub " } else { "" };
                let f_type = field_item
                    .inner
                    .as_ref()
                    .and_then(|inner| inner.get("struct_field"))
                    .map(format_type)
                    .unwrap_or_default();
                if f_type.is_empty() {
                    field_lines.push(format!("{}{}", f_vis, f_name));
                } else {
                    field_lines.push(format!("{}{}: {}", f_vis, f_name, f_type));
                }
            }
        }
    }

    if !field_lines.is_empty() {
        format!(
            "{}struct {} {{\n{}\n}}",
            vis,
            name,
            field_lines
                .iter()
                .map(|f| format!("    {},", f))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        format!("{}struct {}", vis, name)
    }
}

fn extract_enum_signature(
    name: &str,
    is_pub: bool,
    payload: &serde_json::Value,
    raw_index: &HashMap<String, RawItem>,
) -> String {
    let vis = if is_pub { "pub " } else { "" };
    let mut variant_lines = Vec::new();

    if let Some(variant_ids) = payload.get("variants").and_then(|v| v.as_array()) {
        for id_val in variant_ids {
            let var_id = id_val.as_str().unwrap_or_default();
            if let Some(var_item) = raw_index.get(var_id) {
                let v_name = var_item.name.as_deref().unwrap_or("anon");
                let v_kind = var_item
                    .inner
                    .as_ref()
                    .and_then(|inn| inn.get("variant"))
                    .and_then(|v| v.get("kind"));

                if let Some(tuple_fields) =
                    v_kind.and_then(|k| k.get("tuple")).and_then(|t| t.as_array())
                {
                    let types: Vec<String> = tuple_fields.iter().map(format_type).collect();
                    variant_lines.push(format!("{}({})", v_name, types.join(", ")));
                } else {
                    variant_lines.push(v_name.to_string());
                }
            }
        }
    }

    if !variant_lines.is_empty() {
        format!(
            "{}enum {} {{\n{}\n}}",
            vis,
            name,
            variant_lines
                .iter()
                .map(|v| format!("    {},", v))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        format!("{}enum {}", vis, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_parse_rustdoc_json_success() {
        let json_data = r#"{
            "root": "0:0",
            "format_version": 28,
            "index": {
                "0:1": {
                    "id": "0:1",
                    "name": "User",
                    "span": { "filename": "src/user.rs", "begin": [3, 0] },
                    "visibility": "public",
                    "docs": "A user in the system.",
                    "inner": {
                        "struct": {
                            "kind": { "plain": { "fields": ["0:2", "0:3"] } },
                            "impls": ["0:10"]
                        }
                    }
                },
                "0:2": {
                    "id": "0:2",
                    "name": "id",
                    "span": { "filename": "src/user.rs", "begin": [4, 4] },
                    "visibility": "public",
                    "inner": { "struct_field": { "primitive": "u64" } }
                },
                "0:3": {
                    "id": "0:3",
                    "name": "secret",
                    "span": { "filename": "src/user.rs", "begin": [5, 4] },
                    "visibility": "default",
                    "inner": { "struct_field": { "resolved_path": { "name": "String" } } }
                },
                "0:4": {
                    "id": "0:4",
                    "name": "Status",
                    "span": { "filename": "src/user.rs", "begin": [8, 0] },
                    "visibility": "public",
                    "inner": {
                        "enum": {
                            "variants": ["0:5", "0:6"]
                        }
                    }
                },
                "0:5": {
                    "id": "0:5",
                    "name": "Active",
                    "span": { "filename": "src/user.rs", "begin": [9, 4] },
                    "inner": { "variant": { "kind": "plain" } }
                },
                "0:6": {
                    "id": "0:6",
                    "name": "Pending",
                    "span": { "filename": "src/user.rs", "begin": [10, 4] },
                    "inner": { "variant": { "kind": { "tuple": [{ "primitive": "u32" }] } } }
                },
                "0:7": {
                    "id": "0:7",
                    "name": "Greeter",
                    "span": { "filename": "src/user.rs", "begin": [13, 0] },
                    "visibility": "public",
                    "inner": {
                        "trait": {
                            "items": ["0:8"]
                        }
                    }
                },
                "0:8": {
                    "id": "0:8",
                    "name": "greet",
                    "span": { "filename": "src/user.rs", "begin": [14, 4] },
                    "visibility": "default",
                    "inner": {
                        "function": {
                            "decl": {
                                "inputs": [["self", { "borrowed_ref": { "mutable": false } }]],
                                "output": { "resolved_path": { "name": "String" } }
                            }
                        }
                    }
                },
                "0:10": {
                    "id": "0:10",
                    "span": { "filename": "src/user.rs", "begin": [17, 0] },
                    "inner": {
                        "impl": {
                            "trait": { "name": "Greeter" },
                            "for": { "resolved_path": { "name": "User" } },
                            "items": ["0:11"]
                        }
                    }
                },
                "0:11": {
                    "id": "0:11",
                    "name": "greet",
                    "span": { "filename": "src/user.rs", "begin": [18, 4] },
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "decl": {
                                "inputs": [["self", { "borrowed_ref": { "mutable": false } }]],
                                "output": { "resolved_path": { "name": "String" } }
                            }
                        }
                    }
                },
                "0:12": {
                    "id": "0:12",
                    "name": "create_user",
                    "span": { "filename": "src/user.rs", "begin": [22, 0] },
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "decl": {
                                "inputs": [["name", { "borrowed_ref": { "mutable": false, "type": { "primitive": "str" } } }]],
                                "output": { "resolved_path": { "name": "User" } }
                            }
                        }
                    }
                },
                "ext:1": {
                    "id": "ext:1",
                    "name": "external_fn",
                    "span": { "filename": "/cargo/registry/src/other/lib.rs", "begin": [1, 0] },
                    "visibility": "public",
                    "inner": {
                        "function": {
                            "decl": { "inputs": [] }
                        }
                    }
                }
            }
        }"#;

        let root = Path::new("/workspace");
        let target_files = vec!["src/user.rs".to_string()];
        let res = parse_rustdoc_json(root, json_data, &target_files).unwrap();

        assert_eq!(res.len(), 1);
        let user_file = &res[0];
        assert_eq!(user_file.path, "src/user.rs");

        // Verify Struct
        let struct_sym = user_file.symbols.iter().find(|s| s.kind == SymbolKind::Struct).unwrap();
        assert_eq!(struct_sym.name, "User");
        assert!(struct_sym.is_public);
        assert_eq!(struct_sym.docs.as_deref(), Some("A user in the system."));
        let sig = struct_sym.signature.as_ref().unwrap();
        assert!(sig.contains("pub id: u64"));
        assert!(sig.contains("secret: String"));

        // Verify Enum
        let enum_sym = user_file.symbols.iter().find(|s| s.kind == SymbolKind::Enum).unwrap();
        assert_eq!(enum_sym.name, "Status");
        let enum_sig = enum_sym.signature.as_ref().unwrap();
        assert!(enum_sig.contains("Active"));
        assert!(enum_sig.contains("Pending(u32)"));

        // Verify Trait with child method
        let trait_sym = user_file.symbols.iter().find(|s| s.kind == SymbolKind::Trait).unwrap();
        assert_eq!(trait_sym.name, "Greeter");
        assert_eq!(trait_sym.children.len(), 1);
        assert_eq!(trait_sym.children[0].name, "greet");
        assert!(trait_sym.children[0].signature.as_ref().unwrap().contains("fn greet(&self) -> String"));

        // Verify Impl with child method
        let impl_sym = user_file.symbols.iter().find(|s| s.kind == SymbolKind::Impl).unwrap();
        assert_eq!(impl_sym.name, "impl Greeter for User");
        assert_eq!(impl_sym.children.len(), 1);
        assert_eq!(impl_sym.children[0].name, "greet");

        // Verify Free Function
        let fn_sym = user_file.symbols.iter().find(|s| s.kind == SymbolKind::Function).unwrap();
        assert_eq!(fn_sym.name, "create_user");
        assert!(fn_sym.signature.as_ref().unwrap().contains("pub fn create_user(name: &str) -> User"));

        // Verify external crate symbol is filtered out
        assert!(!user_file.symbols.iter().any(|s| s.name == "external_fn"));
    }

    #[test]
    fn test_extract_rustdoc_symbols_missing_cargo_toml() {
        let dir = tempdir().unwrap();
        let res = extract_rustdoc_symbols(dir.path(), &["src/lib.rs".to_string()]);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("No Cargo.toml found"));
    }

    #[test]
    fn test_extract_rustdoc_symbols_missing_json_fallback() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"dummy\"\nversion = \"0.1.0\"\n").unwrap();
        let res = extract_rustdoc_symbols(dir.path(), &["src/lib.rs".to_string()]);
        assert!(res.is_err());
    }
}
