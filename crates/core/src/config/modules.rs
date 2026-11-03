//! Modular project configuration: the `tauqe.d/*.toml` layers of a repository.
//!
//! A project may split its configuration across `tauqe.d/*.toml` modules. The
//! layers are read in lexical file-name order and merged into the base
//! document, so every reader of the repository observes one configuration:
//!
//! - tables merge recursively, so a module may declare `[context]` while the
//!   base document declares a key inside it;
//! - arrays of tables concatenate in layer order, so `[[outer_docs]]` and
//!   `[[review.presets]]` accumulate instead of replacing each other;
//! - a key two layers both declare as a value is a configuration error naming
//!   both files, because a silent override would make the effective
//!   configuration depend on file names alone.
//!
//! The module directory always belongs to the repository root: an explicit
//! `--config` moves the base document without moving the modules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::escape::single_line;

use super::paths::{modules_dir, modules_root, MODULES_DIR_NAME, MODULE_EXTENSION};

/// Dotted configuration keys declared by the `tauqe.d` modules of a repository.
///
/// The map names the module that owns a key, so a client editing the
/// configuration can be refused instead of mutating a value the next load
/// would overwrite.
pub type ModuleOwners = BTreeMap<String, PathBuf>;

/// One configuration layer and the document it declares.
struct Layer {
    path: PathBuf,
    table: toml::Table,
}

/// The layer each dotted key path of a merged document came from.
#[derive(Debug, Default)]
struct Provenance {
    /// The layer that declared each dotted key path first.
    layers: BTreeMap<String, PathBuf>,
    /// The module that declared each dotted key path first, if any.
    modules: BTreeMap<String, PathBuf>,
}

impl Provenance {
    /// Records `layer` as the first declarer of `key`.
    fn declare(&mut self, key: &str, layer: &Path, is_module: bool) {
        self.layers
            .entry(key.to_string())
            .or_insert_with(|| layer.to_path_buf());
        if is_module {
            self.modules
                .entry(key.to_string())
                .or_insert_with(|| layer.to_path_buf());
        }
    }

    /// The layer that declared `key` first.
    fn layer_of(&self, key: &str) -> Option<&Path> {
        self.layers.get(key).map(PathBuf::as_path)
    }
}

/// Dotted configuration keys declared by the `tauqe.d` modules of `repo_root`.
pub fn module_owners(repo_root: &Path) -> Result<ModuleOwners, String> {
    crate::blocking::run_blocking(|| {
        let mut base = toml::Table::new();
        merge_modules(&mut base, None, Some(repo_root))
    })
}

/// Reports a collision between a configuration document and the repository modules.
///
/// `document` is the TOML text a caller is about to install as the base
/// configuration, and `label` names it in the diagnostic. A collision means the
/// document declares a key a `tauqe.d` module already declares: the merge would
/// refuse the pair, so installing the document would leave the project
/// unloadable.
pub fn ensure_no_module_conflict(
    document: &str,
    label: &Path,
    repo_root: Option<&Path>,
) -> Result<(), String> {
    crate::blocking::run_blocking(|| {
        let mut table = parse_table(label, document)?;
        merge_modules(&mut table, Some(label), repo_root).map(|_| ())
    })
}

/// Reads the layered configuration document governing `repo_root`.
///
/// `base` is the base document: the file named by `--config`, or the one the
/// repository discovery resolved. The document is `None` when the repository
/// declares no configuration at all — no base file was found and no module
/// exists — so the built-in defaults govern it.
pub(crate) fn read_document(
    base: Option<&Path>,
    repo_root: Option<&Path>,
) -> Result<Option<toml::Table>, String> {
    let mut table = match base {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|err| format!("Failed to read {}: {}", path.display(), err))?;
            parse_table(path, &content)?
        }
        None => toml::Table::new(),
    };
    merge_modules(&mut table, base, repo_root)?;
    if base.is_none() && table.is_empty() {
        return Ok(None);
    }
    Ok(Some(table))
}

/// Names the configuration document a diagnostic is attributed to.
///
/// A layered document is attributed to the base file that anchors it; a
/// repository that declares modules only is attributed to its module directory.
pub(crate) fn document_label(base: Option<&Path>, repo_root: Option<&Path>) -> String {
    if let Some(path) = base {
        return path.display().to_string();
    }
    match repo_root {
        Some(root) => modules_dir(root).display().to_string(),
        None => MODULES_DIR_NAME.to_string(),
    }
}

/// Parses one configuration document, naming the file and the line it broke on.
fn parse_table(path: &Path, content: &str) -> Result<toml::Table, String> {
    toml::from_str::<toml::Table>(content).map_err(|err| {
        let message = single_line(err.message());
        match err.span() {
            Some(span) => {
                let (line, column) = line_col(content, span.start);
                format!(
                    "Failed to parse {} at line {}, column {}: {}",
                    path.display(),
                    line,
                    column,
                    message
                )
            }
            None => format!("Failed to parse {}: {}", path.display(), message),
        }
    })
}

/// Resolves the one-based line and column of a byte offset in `content`.
///
/// The TOML decoder reports the location of a syntax error as a byte span, so
/// the diagnostic translates its opening offset into the coordinates an editor
/// shows. An offset that is not a character boundary stops at the next
/// character start instead of slicing the text, and an offset past the end of
/// the document clamps to its length.
fn line_col(content: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(content.len());
    let mut line = 1;
    let mut column = 1;
    for (index, character) in content.char_indices() {
        if index >= offset {
            break;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Reads every `tauqe.d/*.toml` layer of a repository, in lexical order.
fn load_layers(repo_root: Option<&Path>) -> Result<Vec<Layer>, String> {
    let Some(root) = modules_root(repo_root) else {
        return Ok(Vec::new());
    };
    let dir = modules_dir(&root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A repository without a module directory declares everything in its
        // base document, which is the ordinary layout.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("Failed to read {}: {}", dir.display(), err)),
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read {}: {}", dir.display(), err))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        // `tauqe.d/*.toml` never matches a hidden file, so a dotted name stays
        // out of the merge even when it carries the extension.
        let hidden = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if hidden || path.extension().and_then(|ext| ext.to_str()) != Some(MODULE_EXTENSION) {
            continue;
        }
        paths.push(path);
    }
    paths.sort();

    let mut layers = Vec::with_capacity(paths.len());
    for path in paths {
        let content = std::fs::read_to_string(&path)
            .map_err(|err| format!("Failed to read {}: {}", path.display(), err))?;
        layers.push(Layer {
            table: parse_table(&path, &content)?,
            path,
        });
    }
    Ok(layers)
}

/// Merges every `tauqe.d/*.toml` layer of `repo_root` into `base`.
///
/// `base_label` names the document `base` was read from, so a collision can
/// report both files. A caller that merges modules into a document which is not
/// on disk yet passes the path that document is about to take; a caller with no
/// base document passes `None` and an empty `base`.
///
/// Returns the dotted keys the modules own.
fn merge_modules(
    base: &mut toml::Table,
    base_label: Option<&Path>,
    repo_root: Option<&Path>,
) -> Result<ModuleOwners, String> {
    let layers = load_layers(repo_root)?;
    let mut provenance = Provenance::default();
    if let Some(label) = base_label {
        for (key, value) in base.iter() {
            declare_value(value, label, key, false, &mut provenance);
        }
    }
    for layer in layers {
        merge_layer(base, layer.table, &layer.path, "", &mut provenance)?;
    }
    Ok(provenance.modules)
}

/// Merges one layer into `target`.
fn merge_layer(
    target: &mut toml::Table,
    incoming: toml::Table,
    layer: &Path,
    prefix: &str,
    provenance: &mut Provenance,
) -> Result<(), String> {
    for (key, value) in incoming {
        let dotted = dotted_key(prefix, &key);
        match target.get_mut(&key) {
            None => {
                declare_value(&value, layer, &dotted, true, provenance);
                target.insert(key, value);
            }
            Some(existing) => merge_value(existing, value, layer, &dotted, provenance)?,
        }
    }
    Ok(())
}

/// Merges one value into the value the same key already holds.
fn merge_value(
    existing: &mut toml::Value,
    incoming: toml::Value,
    layer: &Path,
    dotted: &str,
    provenance: &mut Provenance,
) -> Result<(), String> {
    match (existing, incoming) {
        (toml::Value::Table(nested), toml::Value::Table(more)) => {
            merge_layer(nested, more, layer, dotted, provenance)
        }
        (toml::Value::Array(items), toml::Value::Array(mut more))
            if is_array_of_tables(items.as_slice()) && is_array_of_tables(more.as_slice()) =>
        {
            // Arrays of tables accumulate in layer order, so `[[outer_docs]]`
            // and `[[review.presets]]` add up instead of replacing each other.
            provenance.declare(dotted, layer, true);
            items.append(&mut more);
            Ok(())
        }
        (_, _) => Err(collision(dotted, provenance.layer_of(dotted), layer)),
    }
}

/// Builds the diagnostic of a key two layers both declare as a value.
fn collision(dotted: &str, first: Option<&Path>, second: &Path) -> String {
    let first = first
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "an earlier layer".to_string());
    format!(
        "Conflicting configuration: the key `{}` is declared by both {} and {}; \
         declare it in one layer only",
        dotted,
        first,
        second.display()
    )
}

/// Records every value of `value` as declared by `layer` under `dotted`.
fn declare_value(
    value: &toml::Value,
    layer: &Path,
    dotted: &str,
    is_module: bool,
    provenance: &mut Provenance,
) {
    match value {
        toml::Value::Table(nested) => {
            for (key, value) in nested {
                declare_value(
                    value,
                    layer,
                    &dotted_key(dotted, key),
                    is_module,
                    provenance,
                );
            }
        }
        _ => provenance.declare(dotted, layer, is_module),
    }
}

/// Reports whether `items` is an array of tables, the shape `[[section]]` yields.
fn is_array_of_tables(items: &[toml::Value]) -> bool {
    !items.is_empty() && items.iter().all(toml::Value::is_table)
}

/// Joins a table prefix and a key into the dotted path a diagnostic reports.
fn dotted_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{}.{}", prefix, key)
    }
}
