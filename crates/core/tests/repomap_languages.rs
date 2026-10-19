//! Symbol extraction coverage for the per-language Tree-sitter extractors.

use tauqe_core::repomap::treesitter::{extract_symbols_from_str, is_supported_path};
use tauqe_core::repomap::{RepoFileSymbols, Symbol, SymbolKind};

/// Extracts the symbols of an in-memory source file.
fn extract(path: &str, source: &str) -> RepoFileSymbols {
    extract_symbols_from_str(path, source).expect("extension is registered")
}

/// Returns the symbol declared as `name`.
fn find<'a>(file: &'a RepoFileSymbols, name: &str) -> &'a Symbol {
    file.symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("symbol {name} is missing"))
}

/// Returns the rendered signature of a symbol.
fn signature(symbol: &Symbol) -> &str {
    symbol.signature.as_deref().unwrap_or("")
}

#[test]
fn registers_every_supported_extension() {
    for path in [
        "src/main.rs",
        "src/index.ts",
        "src/view.tsx",
        "src/app.js",
        "src/app.jsx",
        "bin/tool.mjs",
        "bin/tool.cjs",
        "scripts/build.py",
        "cmd/server/main.go",
        "src/Main.java",
        "src/point.c",
        "include/point.h",
        "src/renderer.cpp",
        "include/renderer.hpp",
        "src/engine.cc",
        "src/engine.cxx",
        "src/Service.cs",
        "src/Service.php",
    ] {
        assert!(is_supported_path(path), "{path} must be supported");
    }
    for path in ["lib/service.rb", "web/index.html", "Makefile"] {
        assert!(!is_supported_path(path), "{path} must stay unsupported");
    }
}

#[test]
fn extracts_rust_items() {
    let code = r#"
pub struct Service {
    pub name: String,
    retries: u8,
}

impl Service {
    pub fn new(name: &str) -> Self {
        Service { name: name.into(), retries: 0 }
    }
}
"#;
    let file = extract("src/service.rs", code);

    let service = find(&file, "Service");
    assert_eq!(service.kind, SymbolKind::Struct);
    assert!(service.is_public);
    assert!(signature(service).contains("pub name: String"));

    let implementation = file
        .symbols
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Impl)
        .expect("impl block is declared");
    assert_eq!(implementation.children.len(), 1);
    assert_eq!(implementation.children[0].name, "new");
    assert!(implementation.children[0].is_public);
}

#[test]
fn extracts_javascript_declarations() {
    let code = r#"
export function createUser(name) {
    return name;
}

const localHelper = (value) => value * 2;

export default class User {
    constructor(name) {
        this.name = name;
    }
    greet() {
        return this.name;
    }
}
"#;
    let file = extract("src/user.js", code);

    let create_user = find(&file, "createUser");
    assert_eq!(create_user.kind, SymbolKind::Function);
    assert!(create_user.is_public);
    assert!(signature(create_user).contains("createUser"));

    let helper = find(&file, "localHelper");
    assert_eq!(helper.kind, SymbolKind::Function);
    assert!(!helper.is_public);

    let user = find(&file, "User");
    assert_eq!(user.kind, SymbolKind::Struct);
    assert!(user.is_public);
    assert_eq!(user.children.len(), 2);
    assert!(user
        .children
        .iter()
        .any(|method| method.name == "greet" && method.kind == SymbolKind::Method));
}

#[test]
fn extracts_typescript_declarations() {
    let code = r#"
export interface Greeter {
    greet(name: string): string;
}

type Identifier = string;

export enum Status {
    Active,
    Pending,
}

export class Service implements Greeter {
    greet(name: string): string {
        return name;
    }
}

export const run = async () => {};
"#;
    let file = extract("src/service.ts", code);

    let greeter = find(&file, "Greeter");
    assert_eq!(greeter.kind, SymbolKind::Trait);
    assert!(greeter.is_public);
    assert_eq!(greeter.children[0].name, "greet");

    let identifier = find(&file, "Identifier");
    assert_eq!(identifier.kind, SymbolKind::Struct);
    assert!(!identifier.is_public);

    let status = find(&file, "Status");
    assert_eq!(status.kind, SymbolKind::Enum);
    assert!(signature(status).contains("Active"));

    let service = find(&file, "Service");
    assert_eq!(service.kind, SymbolKind::Struct);
    assert_eq!(service.children[0].name, "greet");

    let run = find(&file, "run");
    assert_eq!(run.kind, SymbolKind::Function);
    assert!(run.is_public);
}

#[test]
fn extracts_python_definitions() {
    let code = r#"
class Service:
    def greet(self, name):
        return name

    def _internal(self):
        return None

def build_service(path):
    return Service()

def _helper():
    return None
"#;
    let file = extract("scripts/service.py", code);

    let service = find(&file, "Service");
    assert_eq!(service.kind, SymbolKind::Struct);
    assert!(service.is_public);
    assert_eq!(service.children.len(), 2);
    assert_eq!(service.children[0].name, "greet");
    assert!(service.children[0].is_public);
    assert!(!service.children[1].is_public);

    let build = find(&file, "build_service");
    assert_eq!(build.kind, SymbolKind::Function);
    assert!(signature(build).contains("def build_service(path)"));

    let helper = find(&file, "_helper");
    assert!(!helper.is_public);
}

#[test]
fn extracts_go_declarations() {
    let code = r#"
package service

type Service struct {
    Name string
}

type Greeter interface {
    Greet(name string) string
}

func BuildService(name string) *Service {
    return &Service{Name: name}
}

func (service *Service) greet() string {
    return service.Name
}
"#;
    let file = extract("cmd/service.go", code);

    let service = find(&file, "Service");
    assert_eq!(service.kind, SymbolKind::Struct);
    assert!(service.is_public);
    assert!(signature(service).contains("Name string"));

    let greeter = find(&file, "Greeter");
    assert_eq!(greeter.kind, SymbolKind::Trait);

    let build = find(&file, "BuildService");
    assert_eq!(build.kind, SymbolKind::Function);
    assert!(build.is_public);
    assert!(signature(build).contains("func BuildService(name string) *Service"));

    let greet = find(&file, "greet");
    assert_eq!(greet.kind, SymbolKind::Method);
    assert!(!greet.is_public);
}
