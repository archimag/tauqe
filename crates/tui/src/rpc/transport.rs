use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tauqe_protocol::{Message, ModelRef, Request, RequestId};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(10);
static PENDING_REQUESTS: Mutex<Option<HashMap<u64, String>>> = Mutex::new(None);
static PENDING_ROLLBACKS: Mutex<Option<HashMap<u64, OptimisticRollback>>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub enum OptimisticRollback {
    ReviewItemStatus {
        item_id: u32,
        prev_status: tauqe_protocol::ReviewStatus,
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

pub fn allocate_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn record_pending_request(id: u64, method: &str) {
    if let Ok(mut lock) = PENDING_REQUESTS.lock() {
        lock.get_or_insert_with(HashMap::new)
            .insert(id, method.to_string());
    }
}

pub fn take_pending_request(id: u64) -> Option<String> {
    if let Ok(mut lock) = PENDING_REQUESTS.lock() {
        lock.as_mut().and_then(|m| m.remove(&id))
    } else {
        None
    }
}

pub fn record_optimistic_rollback(id: u64, rollback: OptimisticRollback) {
    if let Ok(mut lock) = PENDING_ROLLBACKS.lock() {
        lock.get_or_insert_with(HashMap::new).insert(id, rollback);
    }
}

pub fn take_optimistic_rollback(id: u64) -> Option<OptimisticRollback> {
    if let Ok(mut lock) = PENDING_ROLLBACKS.lock() {
        lock.as_mut().and_then(|m| m.remove(&id))
    } else {
        None
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

pub async fn start_server() -> anyhow::Result<(Child, ChildStdin, Lines<BufReader<ChildStdout>>, PathBuf)> {
    let server_path = find_server_binary();
    let log_path = match std::fs::create_dir_all(".tauqe") {
        Ok(_) => PathBuf::from(".tauqe/server.log"),
        Err(_) => std::env::temp_dir().join("tauqe-server.log"),
    };
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open server log file at '{}'", log_path.display()))?;

    let mut server_child = tokio::process::Command::new(&server_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
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
    writer: &mut ChildStdin,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<()> {
    record_pending_request(id, method);
    let req = Request {
        id: RequestId::Number(id),
        method: method.to_string(),
        params: Some(params),
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn send_request(
    writer: &mut ChildStdin,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<u64> {
    let id = allocate_request_id();
    send_request_with_id(writer, id, method, params).await?;
    Ok(id)
}
