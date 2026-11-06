use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use tauqe_protocol::{Message, ModelRef, Request, RequestId};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(10);

#[derive(Debug, Clone)]
pub enum OptimisticRollback {
    ReviewIssueStatus {
        review_id: Option<String>,
        item_id: u32,
        prev_status: tauqe_protocol::IssueStatus,
    },
    PlanItemStatus {
        plan_id: String,
        item_id: String,
        prev_status: tauqe_protocol::PlanItemStatus,
    },
    ActiveModel {
        prev_model: ModelRef,
        prev_selection: tauqe_protocol::ModelSelection,
    },
}

/// Monotonic source of client-side request identifiers.
pub fn allocate_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Correlates in-flight requests with their method name and the optimistic
/// rollback they carry.
///
/// The tracker is owned by an [`RpcConnection`] rather than a process-global
/// static, so independent connections (including unit tests) never observe each
/// other's bookkeeping. Entries are removed as soon as the matching response
/// arrives, and any entry still pending when the connection is dropped is
/// released with it.
#[derive(Debug, Default)]
struct RpcTracker {
    pending_requests: HashMap<u64, String>,
    pending_rollbacks: HashMap<u64, OptimisticRollback>,
}

impl RpcTracker {
    fn record_request(&mut self, id: u64, method: &str) {
        self.pending_requests.insert(id, method.to_string());
    }

    fn take_request(&mut self, id: u64) -> Option<String> {
        self.pending_requests.remove(&id)
    }

    fn record_rollback(&mut self, id: u64, rollback: OptimisticRollback) {
        self.pending_rollbacks.insert(id, rollback);
    }

    fn take_rollback(&mut self, id: u64) -> Option<OptimisticRollback> {
        self.pending_rollbacks.remove(&id)
    }
}

/// The client's write half of the server connection.
///
/// The connection bundles the byte sink with the request/rollback tracker so the
/// whole input and dialog stack can be driven by any async writer. Production
/// wraps the `ChildStdin` of the spawned server process; tests wrap an in-memory
/// sink such as a `Vec<u8>`. This removes the concrete `ChildStdin` dependency
/// (and the need to spawn a helper process) from every handler.
pub struct RpcConnection {
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    tracker: RpcTracker,
    #[cfg(test)]
    written: Vec<u8>,
}

impl RpcConnection {
    /// Wraps an async byte sink (or a test double) as an RPC connection.
    pub fn new<W: AsyncWrite + Unpin + Send + 'static>(writer: W) -> Self {
        Self {
            writer: Box::new(writer),
            tracker: RpcTracker::default(),
            #[cfg(test)]
            written: Vec::new(),
        }
    }

    /// Writes a raw, pre-serialized line without tracking it. The handshake uses
    /// this before any request/response correlation exists.
    pub async fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        let writer = self.writer.as_mut();
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        #[cfg(test)]
        self.written.extend_from_slice(&bytes);
        Ok(())
    }

    /// Returns the bytes written across the connection during tests.
    #[cfg(test)]
    pub fn written(&self) -> &[u8] {
        &self.written
    }

    /// Removes and returns the method recorded for a request id.
    pub fn take_pending_request(&mut self, id: u64) -> Option<String> {
        self.tracker.take_request(id)
    }

    /// Removes and returns the optimistic rollback recorded for a request id.
    pub fn take_optimistic_rollback(&mut self, id: u64) -> Option<OptimisticRollback> {
        self.tracker.take_rollback(id)
    }
    /// Registers an optimistic rollback to apply if the matching request fails.
    ///
    /// This is the internal recorder used by [`send_request_with_rollback`] and
    /// by the tracker unit tests. Production code records a rollback exclusively
    /// through [`send_request_with_rollback`], so an entry exists only after the
    /// request actually reached the server.
    pub(crate) fn record_optimistic_rollback(&mut self, id: u64, rollback: OptimisticRollback) {
        self.tracker.record_rollback(id, rollback);
    }
}

pub fn find_server_binary() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "tauqe-server.exe"
    } else {
        "tauqe-server"
    };

    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(exe_name);
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from(exe_name)
}

/// Resolves the repository root by walking up from the current working
/// directory until a `.git` entry is found.
///
/// The server keeps its journal in `.tauqe` at the repository root. The client
/// only derives that path to report where the log lives; it never opens the
/// file, so the server stays the sole writer. When no repository is detected,
/// the current directory is used so the TUI still runs outside version control.
fn resolve_repo_root() -> PathBuf {
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut current = start.as_path();
    loop {
        if current.join(".git").exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return start,
        }
    }
}

pub async fn start_server(
) -> anyhow::Result<(Child, ChildStdin, Lines<BufReader<ChildStdout>>, PathBuf)> {
    let server_path = find_server_binary();
    // The server owns its journal: it opens `.tauqe/server.log` through the
    // rotating file appender in `logging`. The client only derives the same
    // path to report where the log lives in disconnection notifications and
    // never opens the file, so no second descriptor competes with the server's
    // rotation.
    let log_path = resolve_repo_root().join(".tauqe").join("server.log");

    let mut cmd = tokio::process::Command::new(&server_path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    // Put the daemon in its own process group on Unix so that a signal sent to
    // the TUI's foreground group (Ctrl+C, SIGHUP, or a closed terminal) never
    // reaches the server and leaves it running as an orphan. The server is torn
    // down explicitly on exit, or implicitly through `kill_on_drop` when the
    // child handle is dropped.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut server_child = cmd.spawn().with_context(|| {
        format!(
            "Failed to spawn tauqe-server at '{}'. Make sure to run 'cargo build' first.",
            server_path.display()
        )
    })?;

    let child_stdin = server_child
        .stdin
        .take()
        .context("Failed to open child server stdin")?;
    let child_stdout = server_child
        .stdout
        .take()
        .context("Failed to open child server stdout")?;

    let reader = BufReader::new(child_stdout).lines();
    Ok((server_child, child_stdin, reader, log_path))
}

pub fn spawn_message_reader(mut reader: Lines<BufReader<ChildStdout>>) -> mpsc::Receiver<Message> {
    let (msg_tx, msg_rx) = mpsc::channel::<Message>(100);
    tokio::spawn(async move {
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(msg) = serde_json::from_str::<Message>(&line) {
                        if msg_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
    msg_rx
}

/// Writes one request and records it as in flight.
///
/// # Locking invariant
///
/// Callers must not hold the `AppState` guard across this `await`. The event loop
/// is the only task that locks the state, so a write parked on a daemon that
/// stopped draining its stdin would freeze rendering and server-message
/// processing at once. Complete the state mutation, drop the guard, and only
/// then await this function — as `start_turn` and every dialog handler do (c-1).
pub async fn send_request_with_id(
    conn: &mut RpcConnection,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    let req = Request {
        id: RequestId::Number(id),
        method: method.to_string(),
        params: Some(params),
    };
    let line = serde_json::to_string(&req)?;
    // Track the in-flight request only once it has actually reached the server.
    // A failed write must not leave a stale entry that would later misroute an
    // unrelated response. The event loop dispatches this write and the matching
    // response sequentially, so no response can be handled before the entry is
    // recorded here.
    conn.write_line(&line).await?;
    conn.tracker.record_request(id, method);
    Ok(())
}

/// Writes a request under a freshly allocated identifier.
///
/// The same locking invariant as [`send_request_with_id`] applies: the
/// `AppState` guard must already be released when this function is awaited.
pub async fn send_request(
    conn: &mut RpcConnection,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<u64> {
    let id = allocate_request_id();
    send_request_with_id(conn, id, method, params).await?;
    Ok(id)
}
/// Writes one request and records the optimistic rollback it may need.
///
/// Both tracker entries — the in-flight method and the rollback — are recorded
/// only after the request actually reached the server, mirroring the ordering
/// rule of [`send_request_with_id`]: a failed write leaves no tracker state
/// behind, because no response will ever arrive for that id. The caller keeps
/// ownership of the rollback value (hence the reference) and applies it inline
/// when this returns an error, so the optimistic mutation it already made is
/// reverted at the site that owns it.
///
/// # Locking invariant
///
/// Callers must not hold the `AppState` guard across this `await`, exactly as
/// for [`send_request_with_id`].
pub async fn send_request_with_rollback(
    conn: &mut RpcConnection,
    id: u64,
    method: &str,
    params: serde_json::Value,
    rollback: &OptimisticRollback,
) -> anyhow::Result<()> {
    send_request_with_id(conn, id, method, params).await?;
    conn.record_optimistic_rollback(id, rollback.clone());
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_connection() -> RpcConnection {
        RpcConnection::new(Vec::<u8>::new())
    }

    #[tokio::test]
    async fn send_request_records_and_take_removes_pending_request() {
        let mut conn = test_connection();
        let id = send_request(&mut conn, "model/ask", serde_json::json!({}))
            .await
            .expect("an in-memory sink never fails to write");
        assert_eq!(conn.take_pending_request(id).as_deref(), Some("model/ask"));
        assert!(conn.take_pending_request(id).is_none());
        assert!(!conn.written().is_empty());
    }

    #[test]
    fn optimistic_rollback_is_tracked_then_removed() {
        let mut conn = test_connection();
        conn.record_optimistic_rollback(
            9002,
            OptimisticRollback::ActiveModel {
                prev_model: ModelRef::openrouter("previous-model"),
                prev_selection: tauqe_protocol::ModelSelection::default(),
            },
        );
        assert!(conn.take_optimistic_rollback(9002).is_some());
        assert!(conn.take_optimistic_rollback(9002).is_none());
    }

    #[test]
    fn independent_connections_do_not_share_tracker_state() {
        let mut a = test_connection();
        let mut b = test_connection();
        a.record_optimistic_rollback(
            1,
            OptimisticRollback::ActiveModel {
                prev_model: ModelRef::openrouter("a"),
                prev_selection: tauqe_protocol::ModelSelection::default(),
            },
        );
        assert!(a.take_optimistic_rollback(1).is_some());
        assert!(b.take_optimistic_rollback(1).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_child_handle_kills_the_spawned_process() {
        use std::os::unix::process::CommandExt;

        let probe_dir = crate::testing::scratch_dir("spawn-probe");
        let heartbeat = probe_dir.join("heartbeat.log");

        // The probe appends a heartbeat every 50ms until it is terminated.
        let script = format!(
            "while true; do echo tick >> '{}'; sleep 0.05; done",
            heartbeat.display()
        );
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);

        let child = cmd.spawn().expect("spawn probe process");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Dropping the handle must terminate the process so it stops writing.
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let size_at_drop = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let size_later = std::fs::metadata(&heartbeat)
            .map(|meta| meta.len())
            .unwrap_or(0);

        let _ = std::fs::remove_dir_all(&probe_dir);
        assert_eq!(
            size_at_drop, size_later,
            "the spawned process kept running after its handle was dropped"
        );
    }
    /// Async sink whose every write fails, standing in for a server pipe that
    /// closed before the request reached it.
    struct BrokenPipeWriter;

    impl tokio::io::AsyncWrite for BrokenPipeWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "server pipe closed",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn active_model_rollback(name: &str) -> OptimisticRollback {
        OptimisticRollback::ActiveModel {
            prev_model: ModelRef::openrouter(name),
            prev_selection: tauqe_protocol::ModelSelection::default(),
        }
    }

    /// A rollback registered for a request that never left the client would leak
    /// in the tracker for the whole process lifetime, and the in-flight entry
    /// would misroute a later response: both exist only after a successful write
    /// (c-3).
    #[tokio::test]
    async fn send_request_with_rollback_records_nothing_when_the_write_fails() {
        let mut conn = RpcConnection::new(BrokenPipeWriter);
        let rollback = active_model_rollback("previous-model");

        let result = send_request_with_rollback(
            &mut conn,
            4100,
            "model/select",
            serde_json::json!({}),
            &rollback,
        )
        .await;

        assert!(result.is_err(), "a closed pipe fails the write");
        assert!(conn.take_pending_request(4100).is_none());
        assert!(conn.take_optimistic_rollback(4100).is_none());
        assert!(conn.written().is_empty(), "the transport accepted no bytes");
    }

    /// A successful write records the in-flight request together with the
    /// rollback the response path would apply (c-3).
    #[tokio::test]
    async fn send_request_with_rollback_records_both_entries_after_a_successful_write() {
        let mut conn = test_connection();
        let rollback = active_model_rollback("previous-model");

        send_request_with_rollback(
            &mut conn,
            4101,
            "model/select",
            serde_json::json!({}),
            &rollback,
        )
        .await
        .expect("an in-memory sink never fails to write");

        assert_eq!(
            conn.take_pending_request(4101).as_deref(),
            Some("model/select")
        );
        assert!(conn.take_optimistic_rollback(4101).is_some());
        assert!(!conn.written().is_empty());
    }
}
