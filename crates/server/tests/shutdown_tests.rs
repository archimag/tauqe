//! Integration coverage for the daemon shutdown handshake.
//!
//! The daemon stops when its client closes the stdin pipe, but stopping is not
//! instantaneous: it cancels the active operation, drains in-flight work, and
//! rolls its Git transaction back before it returns from `main`. Both sides
//! read that timeline from `tauqe_protocol::lifecycle`, and the assertions next
//! to those constants already pin the arithmetic.
//!
//! What only a real process can prove is that the compiled daemon honours the
//! contract: an idle daemon exits cleanly and well inside the budget the client
//! grants it, and it records in its journal why it stopped, so a look at
//! `<repo_root>/.tauqe/server.log` never shows an unexplained disappearance.
//!
//! A model turn is deliberately absent from this contract: it needs a provider
//! key and a network round-trip, while the shutdown path is exercised by the
//! deterministic request plumbing alone.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauqe_protocol::{methods, CLIENT_SHUTDOWN_WAIT};

/// Cadence of the exit poll while the daemon drains.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Extra time granted on top of the shared client budget before the test
/// declares a wedged daemon. It only widens the diagnostic window: a daemon
/// that needs more than the budget has already broken the contract, and the
/// assertion reports that instead of hanging the test run.
const WAIT_HEADROOM: Duration = Duration::from_secs(5);

/// One running daemon driven over its JSON-RPC pipes.
struct Daemon {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Daemon {
    /// Spawns the daemon with `repo` as its working directory.
    ///
    /// The working directory is what binds the daemon to a repository, so the
    /// journal it writes belongs to that temporary repository and cannot mix
    /// with another test's output.
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
            child: Some(child),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 0,
        }
    }

    /// Completes the handshake, binding the daemon to its repository.
    ///
    /// A daemon that never answered the handshake would make an early exit
    /// indistinguishable from a rejected request, so the client becomes a real
    /// session before it asks the daemon to stop.
    fn initialize(&mut self) {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "id": id, "method": methods::CLIENT_INITIALIZE });
        {
            let stdin = self.stdin.as_mut().expect("the daemon is still running");
            writeln!(stdin, "{request}").expect("the daemon request is written");
            stdin.flush().expect("the daemon request is flushed");
        }

        let response = self.read_response(id);
        assert!(
            response
                .get("result")
                .is_some_and(|result| !result.is_null()),
            "initialize must answer with the server description: {response}"
        );
    }

    /// Reads output lines until the response of `id` arrives.
    ///
    /// The daemon interleaves events with responses on the same channel, so
    /// lines are skipped until the one carrying this request's id appears.
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

    /// Closes the client end of the pipe and waits for the daemon to exit.
    ///
    /// Closing stdin is the contract that asks the daemon to stop, so this is
    /// the moment the shared shutdown timeline starts. The wait is bounded by
    /// the budget the client itself grants, plus a small headroom, so a daemon
    /// that outlives its own drain deadline is reported instead of stalling the
    /// test run.
    fn close_stdin_and_wait(mut self) -> (ExitStatus, Duration) {
        self.stdin = None;

        let mut child = self.child.take().expect("the daemon was started");
        let timeout = CLIENT_SHUTDOWN_WAIT + WAIT_HEADROOM;
        match wait_for_exit(&mut child, timeout) {
            Some(outcome) => outcome,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the daemon did not exit within {timeout:?}");
            }
        }
    }
}

impl Drop for Daemon {
    /// Kills a daemon that outlived a failing assertion.
    ///
    /// Without this guard a panicking test would leave a spawned process
    /// holding the temporary repository open, and the next run would fail for
    /// reasons that have nothing to do with the contract.
    fn drop(&mut self) {
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Polls the daemon until it exits or the timeout expires.
///
/// Polling through `try_wait` keeps the child handle in the caller, so a
/// timeout can still kill and reap the process instead of abandoning it.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<(ExitStatus, Duration)> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some((status, started.elapsed())),
            Ok(None) => {}
            Err(err) => panic!("the daemon status is unreadable: {err}"),
        }
        if started.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(EXIT_POLL_INTERVAL);
    }
}

/// Temporary git repository hosting one daemon run.
struct TempRepo(PathBuf);

impl TempRepo {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tauqe-shutdown-it-{}-{}-{}",
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

    /// Collects every journal record the daemon left in `<repo_root>/.tauqe`.
    ///
    /// The whole rotated family is read, and the name prefix is matched instead
    /// of one exact file, so a rotation policy change cannot silently empty the
    /// assertion.
    fn journal(&self) -> String {
        let directory = self.0.join(".tauqe");
        let mut collected = String::new();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => return collected,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("server.log") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                collected.push_str(&text);
            }
        }
        collected
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An idle daemon stops on stdin EOF, exits cleanly, and does not consume the
/// whole budget the client grants its drain.
#[test]
fn closing_stdin_stops_the_daemon_cleanly_and_promptly() {
    let repo = TempRepo::new("graceful");
    let mut daemon = Daemon::start(repo.path());
    daemon.initialize();

    let (status, elapsed) = daemon.close_stdin_and_wait();

    assert!(
        status.success(),
        "the daemon must exit cleanly on stdin EOF, got {status:?}"
    );
    assert!(
        elapsed < CLIENT_SHUTDOWN_WAIT,
        "an idle daemon spent {elapsed:?} of the {CLIENT_SHUTDOWN_WAIT:?} client budget"
    );
}

/// The daemon names the cause in its journal, so the log answers why it left
/// on its own instead of showing an unexplained disappearance.
#[test]
fn the_daemon_records_why_it_stopped() {
    let repo = TempRepo::new("journal");
    let mut daemon = Daemon::start(repo.path());
    daemon.initialize();

    let (status, _) = daemon.close_stdin_and_wait();
    assert!(
        status.success(),
        "the daemon must exit cleanly on stdin EOF, got {status:?}"
    );

    let journal = repo.journal();
    assert!(
        !journal.is_empty(),
        "the daemon left no journal in {}",
        repo.path().join(".tauqe").display()
    );
    assert!(
        journal.contains("stdin-eof"),
        "the journal does not name the shutdown cause:\n{journal}"
    );
    assert!(
        journal.contains("graceful shutdown"),
        "the journal does not record the graceful shutdown path:\n{journal}"
    );
}
