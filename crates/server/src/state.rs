use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{watch, Mutex};
use tauqe_core::config::AppConfig;
use tauqe_core::context::ContextManager;
use tauqe_core::history::HistoryManager;
use tauqe_protocol::UiHistoryItem;

use crate::transport::OutChannel;

pub struct ActiveOperation {
    pub cancel_tx: watch::Sender<bool>,
    pub abort_handle: Option<tokio::task::AbortHandle>,
}

pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub context: tokio::sync::RwLock<ContextManager>,
    pub history: Mutex<HistoryManager>,
    pub session_lock: Mutex<()>,
    pub active_cancel: Mutex<Option<ActiveOperation>>,
    pub total_cost: Mutex<f64>,
    pub history_sender: tokio::sync::mpsc::UnboundedSender<UiHistoryItem>,
    pub out: OutChannel,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        repo_path: PathBuf,
        history_sender: tokio::sync::mpsc::UnboundedSender<UiHistoryItem>,
        out: OutChannel,
    ) -> Arc<Self> {
        // Crash recovery: check and recover any dangling checkpoint or step commits from an interrupted workflow
        if let Ok(Some(recovery_msg)) =
            tauqe_core::git::checkpoint::recover_interrupted_workflow(&repo_path)
        {
            eprintln!("[tauqe-recovery] {}", recovery_msg);
        }

        let history_sender_init = history_sender.clone();
        let mut initial_history_manager = HistoryManager::new(repo_path.clone());
        initial_history_manager.set_listener(move |item| {
            let _ = history_sender_init.send(item);
        });

        Arc::new(Self {
            config: Mutex::new(config),
            context: tokio::sync::RwLock::new(ContextManager::new(repo_path)),
            history: Mutex::new(initial_history_manager),
            session_lock: Mutex::new(()),
            active_cancel: Mutex::new(None),
            total_cost: Mutex::new(0.0),
            history_sender,
            out,
        })
    }
}

static OP_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn next_operation_id() -> u64 {
    OP_COUNTER.fetch_add(1, Ordering::SeqCst)
}
