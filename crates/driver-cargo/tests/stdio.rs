//! End-to-end checks of the driver protocol over a real child process.
//!
//! These tests dogfood the wire contract: they spawn the built binary, feed it
//! request lines, and decode the response lines it writes back.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

/// Manifest declaring one in-memory formatter and one failing check suite.
const MANIFEST: &str = r#"
[formatters.upper]
command = "sh"
args = ["-c", "tr a-z A-Z"]
extensions = ["txt"]

[checks.sample.steps.greet]
command = "sh"
args = ['-c', 'printf "main.go:3:1: error: boom\n"; exit 1']
parser = "go"
"#;

/// A temporary repository root removed when the test ends.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates an empty directory with a unique name.
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tauqe-driver-stdio-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// Root of the temporary repository.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one file relative to the root.
    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path.join(name), content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawns the driver with the repository root as its working directory.
fn spawn(root: &Path) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tauqe-driver-cargo"))
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

/// Sends one request line and decodes the single response line it answers with.
fn roundtrip(stdin: &mut ChildStdin, reader: &mut BufReader<ChildStdout>, request: &str) -> Value {
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn test_stdio_serves_every_phase() {
    let dir = TempDir::new();
    dir.write("tauqe-project.toml", MANIFEST);

    let (mut child, mut stdin, mut stdout) = spawn(dir.path());

    let capabilities = roundtrip(&mut stdin, &mut stdout, r#"{"type":"capabilities"}"#);
    assert_eq!(capabilities["type"], "capabilities");
    assert_eq!(capabilities["capabilities"]["format"], json!(true));
    assert_eq!(capabilities["capabilities"]["post_hook"], json!(false));
    assert_eq!(capabilities["capabilities"]["diagnostics"], json!(true));
    assert_eq!(capabilities["capabilities"]["suites"], json!(["sample"]));

    let formatted = roundtrip(
        &mut stdin,
        &mut stdout,
        r#"{"type":"format","path":"notes.txt","content":"hello"}"#,
    );
    assert_eq!(formatted["type"], "format");
    assert_eq!(formatted["formatted"], json!(true));
    assert_eq!(formatted["content"], "HELLO");
    assert_eq!(formatted["formatter"], "upper");

    // A path no formatter claims keeps its authored content.
    let unclaimed = roundtrip(
        &mut stdin,
        &mut stdout,
        r##"{"type":"format","path":"notes.md","content":"# Notes\n"}"##,
    );
    assert_eq!(unclaimed["formatted"], json!(false));
    assert_eq!(unclaimed["content"], "# Notes\n");

    // No post-hook is declared, so the phase reports an empty success.
    let hooks = roundtrip(
        &mut stdin,
        &mut stdout,
        r#"{"type":"post_hook","changed_files":["src/lib.rs"]}"#,
    );
    assert_eq!(hooks["type"], "post_hook");
    assert_eq!(hooks["success"], json!(true));
    assert!(hooks.get("executed_hooks").is_none());

    let checked = roundtrip(
        &mut stdin,
        &mut stdout,
        r#"{"type":"check","suite":"sample","changed_files":[]}"#,
    );
    assert_eq!(checked["type"], "check");
    assert_eq!(checked["success"], json!(false));
    assert_eq!(checked["steps"][0]["name"], "greet");
    assert_eq!(checked["diagnostics"][0]["file"], "main.go");
    assert_eq!(checked["diagnostics"][0]["line"], json!(3));
    assert_eq!(checked["diagnostics"][0]["severity"], "error");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn test_stdio_synthesizes_a_manifest_for_a_rust_repository() {
    let dir = TempDir::new();
    dir.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );

    let (mut child, mut stdin, mut stdout) = spawn(dir.path());

    let capabilities = roundtrip(&mut stdin, &mut stdout, r#"{"type":"capabilities"}"#);
    assert_eq!(capabilities["capabilities"]["format"], json!(true));
    assert_eq!(
        capabilities["capabilities"]["suites"],
        json!(["all", "check", "lint", "test"])
    );

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn test_stdio_rejects_a_malformed_request() {
    let dir = TempDir::new();
    let (mut child, mut stdin, _stdout) = spawn(dir.path());

    writeln!(stdin, "{{not a request").unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    // A corrupt request stream must fail fast instead of waiting forever.
    assert_eq!(child.wait().unwrap().code(), Some(2));
}
