//! Integration coverage for the project usage telemetry owned by the daemon.
//!
//! The telemetry scopes of one running session are covered by the unit tests
//! next to `crates/server/src/usage.rs`. What only a real process can prove is
//! that the project lifetime is persisted into, and read back from,
//! `<repo_root>/.tauqe/usage.json` across the process boundary. These tests
//! therefore start the compiled `tauqe-server` binary, drive it through
//! JSON-RPC over stdin and stdout the way the client does, and inspect the
//! document the daemon leaves behind.
//!
//! A model turn is deliberately absent from the contract tested here: it needs
//! a provider key and a network round-trip, while the lifetime document is
//! rewritten by the deterministic request plumbing alone.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// Request that clears the persisted project lifetime.
const USAGE_RESET: &str = "model/usageReset";

/// Handshake request establishing the repository the daemon works on.
const CLIENT_INITIALIZE: &str = "client/initialize";

/// One running daemon driven over its JSON-RPC pipes.
struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Daemon {
    /// Spawns the daemon with `repo` as its working directory.
    ///
    /// The working directory is what binds the daemon to a repository, so two
    /// daemons started over the same directory share one telemetry document
    /// exactly as two consecutive launches of the tool do.
    fn start(repo: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tauqe-server"))
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon binary starts");
        let stdin = child.stdin.take().expect("the daemon stdin is captured");
        let stdout = child.stdout.take().expect("the daemon stdout is captured");
        Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 0,
        }
    }

    /// Sends one request and returns the matching response object.
    ///
    /// The daemon interleaves events with responses on the same channel, so
    /// lines are read until the one carrying this request's id arrives.
    fn call(&mut self, method: &str, params: Option<Value>) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({ "id": id, "method": method });
        if let Some(params) = params {
            request["params"] = params;
        }
        {
            let stdin = self.stdin.as_mut().expect("the daemon is still running");
            writeln!(stdin, "{}", request).expect("the daemon request is written");
            stdin.flush().expect("the daemon request is flushed");
        }
        self.read_response(id)
    }

    /// Reads output lines until the response of `id` arrives.
    fn read_response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("the daemon output is readable");
            assert!(
                read > 0,
                "the daemon closed its stdout before answering request {id}"
            );
            let value: Value =
                serde_json::from_str(line.trim()).expect("the daemon writes valid JSON");
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }

    /// Completes the handshake, binding the daemon to its repository.
    fn initialize(&mut self) {
        let result = expect_ok(self.call(CLIENT_INITIALIZE, None));
        assert!(
            result.get("server_name").is_some(),
            "initialize must answer with the server description: {result}"
        );
    }

    /// Closes stdin and waits for a clean exit.
    ///
    /// The daemon treats stdin EOF as the shutdown signal, drains in-flight
    /// work, and exits successfully, so a lost document would surface here.
    fn shutdown(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("the daemon exits");
        assert!(
            status.success(),
            "the daemon must exit cleanly, got {status:?}"
        );
    }
}

/// Unwraps a successful response into its result payload.
fn expect_ok(response: Value) -> Value {
    if let Some(error) = response.get("error") {
        assert!(
            error.is_null(),
            "the daemon rejected the request: {response}"
        );
    }
    response
        .get("result")
        .filter(|result| !result.is_null())
        .cloned()
        .unwrap_or_else(|| panic!("the daemon answered without a result: {response}"))
}

/// Temporary git repository hosting one daemon run.
struct TempRepo(PathBuf);

impl TempRepo {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tauqe-usage-it-{}-{}-{}",
            std::process::id(),
            label,
            unique
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the temporary repository is created");
        tauqe_core::git::init_repository(Some(&root), true).expect("the repository is initialized");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn telemetry_path(&self) -> PathBuf {
        self.0.join(".tauqe").join("usage.json")
    }

    /// Writes a telemetry document before the daemon reads it.
    fn seed_telemetry(&self, document: &str) {
        let path = self.telemetry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the telemetry directory is created");
        }
        std::fs::write(&path, document).expect("the telemetry document is seeded");
    }

    /// Reads the telemetry document back as JSON.
    fn read_telemetry(&self) -> Value {
        let raw = std::fs::read_to_string(self.telemetry_path())
            .expect("the daemon wrote the telemetry document");
        serde_json::from_str(&raw).expect("the telemetry document is valid JSON")
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A recorded project lifetime is loaded by the daemon, left untouched while no
/// operation completes, and rewritten from the cleared totals after a reset.
#[test]
fn resetting_the_project_lifetime_rewrites_the_persisted_document() {
    let repo = TempRepo::new("reset");
    repo.seed_telemetry(
        r#"{"lifetime":{"prompt_tokens":1200,"completion_tokens":280,"total_tokens":1480,"cost":0.42,"duration_ms":600000},"lifetime_operations":3}"#,
    );

    let mut daemon = Daemon::start(repo.path());
    daemon.initialize();

    // Starting the daemon must not disturb a recorded lifetime: the document is
    // only rewritten when a scope actually changes.
    let untouched = repo.read_telemetry();
    assert_eq!(untouched["lifetime"]["total_tokens"].as_u64(), Some(1480));
    assert_eq!(untouched["lifetime_operations"].as_u64(), Some(3));

    let snapshot = expect_ok(daemon.call(USAGE_RESET, None));
    assert_eq!(
        snapshot["lifetime"]["total_tokens"].as_u64(),
        Some(0),
        "the answered snapshot reports the cleared scope"
    );
    assert_eq!(snapshot["lifetime_operations"].as_u64(), Some(0));
    assert_eq!(
        snapshot["session"]["total_tokens"].as_u64(),
        Some(0),
        "a fresh process starts with an empty session scope"
    );

    let stored = repo.read_telemetry();
    assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(0));
    assert_eq!(stored["lifetime"]["cost"].as_f64(), Some(0.0));
    assert_eq!(stored["lifetime_operations"].as_u64(), Some(0));

    daemon.shutdown();
}

/// An unreadable document never blocks startup, and the next write replaces it
/// with a complete valid one.
#[test]
fn a_corrupt_telemetry_document_never_blocks_startup() {
    let repo = TempRepo::new("corrupt");
    repo.seed_telemetry("{ this is not a telemetry document");

    let mut daemon = Daemon::start(repo.path());
    daemon.initialize();

    let snapshot = expect_ok(daemon.call(USAGE_RESET, None));
    assert_eq!(
        snapshot["lifetime_operations"].as_u64(),
        Some(0),
        "an unreadable document degrades to zero totals"
    );

    let stored = repo.read_telemetry();
    assert_eq!(stored["lifetime"]["total_tokens"].as_u64(), Some(0));
    assert_eq!(stored["lifetime_operations"].as_u64(), Some(0));

    daemon.shutdown();
}
