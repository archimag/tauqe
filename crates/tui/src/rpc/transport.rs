use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tauqe_protocol::{Message, ModelRef, Request, RequestId};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(10);

#[derive(Debug, Clone)]
pub enum OptimisticRollback {
    ReviewItemStatus {
        review_id: Option<String>,
        item_id: u32,
        prev_status: tauqe_protocol::ReviewItemStatus,
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
}

impl RpcConnection {
    /// Wraps an async byte sink (or a test double) as an RPC connection.
    pub fn new<W: AsyncWrite + Unpin + Send + 'static>(writer: W) -> Self {
        Self {
            writer: Box::new(writer),
            tracker: RpcTracker::default(),
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
        Ok(())
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
    pub fn record_optimistic_rollback(&mut self, id: u64, rollback: OptimisticRollback) {
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

pub async fn start_server() -> anyhow::Result<(Child, ChildStdin, Lines<BufReader<ChildStdout>>, PathBuf)> {
    let server_path = find_server_binary();
    // The server owns its journal: it opens `.tauqe/server.log` through the
    // rotating file appender in `logging`. The client only derives the same
    // path to report where the log lives in disconnection notifications and
    // never opens the file, so no second descriptor competes with the server's
    // rotation.
    let log_path = resolve_repo_root().join(".tauqe").join("server.log");

    let mut server_child = tokio::process::Command::new(&server_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
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

pub async fn send_request(
    conn: &mut RpcConnection,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<u64> {
    let id = allocate_request_id();
    send_request_with_id(conn, id, method, params).await?;
    Ok(id)
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
}
