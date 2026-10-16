use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Upper bound (250 ms) for a single native clipboard operation.
///
/// `arboard` talks to an external X11/Wayland/OS clipboard process that can
/// stall for seconds when its owner is hung or slow. The TUI drives clipboard
/// access from its async event loop, so an unbounded call would freeze terminal
/// rendering. Native work is dispatched to a dedicated worker thread and is
/// awaited for at most this long; on expiry the operation degrades safely — a
/// copy reports the native path as unavailable, a paste returns no text.
pub const NATIVE_CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(250);

/// Maximum payload size (100 KB) for terminal OSC 52 clipboard sequences
/// to avoid terminal multiplexer buffer overflows.
pub const MAX_OSC52_BYTES: usize = 100 * 1024;

/// The `arboard` clipboard handle, owned exclusively by the worker thread.
///
/// It is created on first use and rebuilt whenever an operation reports a stale
/// handle, preserving the resilience of the previous design without sharing the
/// handle behind a lock.
type ClipboardHandle = Option<arboard::Clipboard>;

/// A unit of native clipboard work executed on the worker thread with exclusive
/// access to the shared [`ClipboardHandle`].
type ClipboardJob = Box<dyn FnOnce(&mut ClipboardHandle) + Send + 'static>;

/// Long-lived worker that serializes every native clipboard operation and is the
/// sole owner of the `arboard` handle.
///
/// A dedicated worker exists so that a stalled X11/Wayland clipboard owner can
/// neither leak one OS thread per user action nor leave several threads blocked
/// on a shared mutex: at most one request is ever in flight, and further
/// requests are rejected immediately while the worker is busy.
struct ClipboardWorker {
    sender: mpsc::Sender<ClipboardJob>,
    busy: Arc<AtomicBool>,
}

/// Lazily started, process-wide clipboard worker.
///
/// `None` means the worker thread could not be spawned, in which case every
/// operation degrades straight to its fallback.
static CLIPBOARD_WORKER: OnceLock<Option<ClipboardWorker>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyResult {
    /// Copied to native OS clipboard (X11 / Wayland / Win / macOS).
    Native,
    /// Copied via terminal escape sequence (OSC 52).
    Osc52Only,
    /// Failed to copy to either native clipboard or terminal.
    Failed,
}

impl CopyResult {
    pub fn is_success(&self) -> bool {
        !matches!(self, CopyResult::Failed)
    }
}

pub fn is_ssh_session() -> bool {
    std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_CLIENT").is_some()
}

/// Returns the process-wide clipboard worker, spawning it on first use.
fn worker() -> Option<&'static ClipboardWorker> {
    CLIPBOARD_WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            let busy = Arc::new(AtomicBool::new(false));
            let worker_busy = Arc::clone(&busy);
            match std::thread::Builder::new()
                .name("tauqe-clipboard".to_string())
                .spawn(move || worker_loop(receiver, worker_busy))
            {
                Ok(_) => Some(ClipboardWorker { sender, busy }),
                Err(_) => None,
            }
        })
        .as_ref()
}

/// Serializes clipboard requests on the worker thread, which owns the `arboard`
/// handle exclusively and therefore never holds a lock across a blocking call.
fn worker_loop(receiver: mpsc::Receiver<ClipboardJob>, busy: Arc<AtomicBool>) {
    let mut clipboard: ClipboardHandle = None;
    while let Ok(job) = receiver.recv() {
        // A panicking job must not take the shared worker down with it: catch
        // the unwind, discard the possibly tainted handle, and keep serving.
        if catch_unwind(AssertUnwindSafe(|| job(&mut clipboard))).is_err() {
            clipboard = None;
        }
        busy.store(false, Ordering::Release);
    }
}

/// Runs `work` on the worker thread and returns its result, or `fallback` when
/// the worker is unavailable, busy, panics, or does not answer within
/// [`NATIVE_CLIPBOARD_TIMEOUT`].
///
/// Dispatching to a single worker thread is what keeps this bounded: callers
/// never spawn their own OS threads and never contend on a shared lock. While
/// the worker is busy (for example, blocked inside `arboard` because the display
/// server is hung) every further request is dropped immediately instead of
/// queueing another detached thread.
fn run_bounded<T, F>(work: F, fallback: T) -> T
where
    T: Send + 'static,
    F: FnOnce(&mut ClipboardHandle) -> T + Send + 'static,
{
    match worker() {
        Some(clipboard_worker) => run_worker_job(clipboard_worker, work, fallback),
        None => fallback,
    }
}

/// Claims the worker for a single request and awaits its reply.
fn run_worker_job<T, F>(worker: &ClipboardWorker, work: F, fallback: T) -> T
where
    T: Send + 'static,
    F: FnOnce(&mut ClipboardHandle) -> T + Send + 'static,
{
    // Reject immediately while a request is already in flight so a stalled
    // display server cannot accumulate queued work.
    if worker.busy.swap(true, Ordering::AcqRel) {
        return fallback;
    }

    let (reply_tx, reply_rx) = mpsc::channel();
    let job: ClipboardJob = Box::new(move |clipboard| {
        let _ = reply_tx.send(work(clipboard));
    });

    if worker.sender.send(job).is_err() {
        worker.busy.store(false, Ordering::Release);
        return fallback;
    }

    reply_rx.recv_timeout(NATIVE_CLIPBOARD_TIMEOUT).unwrap_or(fallback)
}

/// Reads plain text for a standard paste, re-initializing the shared handle if
/// necessary. When the standard clipboard holds no text, the X11/Wayland
/// Primary Selection is consulted as a fallback so that a recent mouse selection
/// remains pasteable.
pub fn get_from_clipboard() -> Option<String> {
    run_bounded(
        |clipboard: &mut ClipboardHandle| {
            if let Some(text) = read_standard_clipboard_now(clipboard) {
                Some(text)
            } else {
                read_primary_selection_now(clipboard)
            }
        },
        None,
    )
}

/// Reads text from the X11/Wayland Primary Selection buffer.
///
/// The Primary Selection is the ephemeral buffer that the most recent mouse
/// selection populates and that terminals paste on a middle-click. It has no
/// equivalent on macOS, Windows, or Android, where this always returns `None`.
pub fn get_from_primary_selection() -> Option<String> {
    run_bounded(read_primary_selection_now, None)
}

/// Reads the best available paste buffer for a middle-click, preferring the
/// Primary Selection and falling back to the standard clipboard.
pub fn get_primary_or_clipboard() -> Option<String> {
    run_bounded(
        |clipboard: &mut ClipboardHandle| {
            if let Some(text) = read_primary_selection_now(clipboard) {
                Some(text)
            } else {
                read_standard_clipboard_now(clipboard)
            }
        },
        None,
    )
}

/// Reads text from the standard OS clipboard only, re-initializing the shared
/// handle once if it has gone stale.
fn read_standard_clipboard_now(clipboard: &mut ClipboardHandle) -> Option<String> {
    if clipboard.is_none() {
        *clipboard = arboard::Clipboard::new().ok();
    }

    if let Some(handle) = clipboard.as_mut() {
        match handle.get_text() {
            Ok(text) if !text.is_empty() => return Some(text),
            Ok(_) => return None,
            Err(_) => {}
        }
    }

    if let Ok(mut new_clipboard) = arboard::Clipboard::new() {
        let text = new_clipboard.get_text().ok();
        *clipboard = Some(new_clipboard);
        if let Some(text) = text {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    None
}

/// Reads text from the X11/Wayland Primary Selection buffer on platforms that
/// expose one, returning `None` everywhere else.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
fn read_primary_selection_now(clipboard: &mut ClipboardHandle) -> Option<String> {
    use arboard::GetExtLinux;

    if clipboard.is_none() {
        *clipboard = arboard::Clipboard::new().ok();
    }

    if let Some(handle) = clipboard.as_mut() {
        return handle
            .get()
            .clipboard(arboard::LinuxClipboardKind::Primary)
            .text()
            .ok()
            .filter(|text| !text.is_empty());
    }

    None
}

/// Reads text from the X11/Wayland Primary Selection buffer on platforms that
/// expose one, returning `None` everywhere else.
#[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten"))))]
fn read_primary_selection_now(_clipboard: &mut ClipboardHandle) -> Option<String> {
    None
}

pub fn copy_to_clipboard(text: &str) -> CopyResult {
    let clean_text = text.trim_end_matches(['\r', '\n']);

    // In an SSH session the native clipboard belongs to the remote host and is
    // unreachable from the user's terminal, so only the OSC 52 sequence is
    // meaningful. Skip `arboard` entirely: with X11 forwarding a native write can
    // stall over the network even though its result is discarded below.
    let in_ssh = is_ssh_session();

    let native_ok = if in_ssh {
        false
    } else {
        let owned = clean_text.to_string();
        run_bounded(
            move |clipboard: &mut ClipboardHandle| write_native_clipboard(clipboard, &owned),
            false,
        )
    };

    let should_send_osc52 = !native_ok || in_ssh;
    let mut osc52_ok = false;
    if should_send_osc52 && clean_text.len() <= MAX_OSC52_BYTES {
        let b64 = base64_encode(clean_text.as_bytes());
        let osc52 = format!("\x1b]52;c;{b64}\x07\x1b]52;p;{b64}\x07");
        let mut out = std::io::stdout();
        if out.write_all(osc52.as_bytes()).is_ok() && out.flush().is_ok() {
            osc52_ok = true;
        }
    }

    // In an SSH session the native clipboard belongs to the remote host and is
    // unreachable from the user's terminal, so only the OSC 52 sequence is
    // meaningful. Report success solely when it was actually emitted, instead of
    // claiming a native copy the user cannot observe.
    if in_ssh {
        if osc52_ok {
            CopyResult::Osc52Only
        } else {
            CopyResult::Failed
        }
    } else if native_ok {
        CopyResult::Native
    } else if osc52_ok {
        CopyResult::Osc52Only
    } else {
        CopyResult::Failed
    }
}

/// Writes `text` to the native OS clipboard, re-initializing the shared handle
/// once when it has gone stale, and mirrors it into the X11/Wayland Primary
/// Selection on Linux. Returns whether the standard clipboard was updated.
fn write_native_clipboard(clipboard: &mut ClipboardHandle, text: &str) -> bool {
    let mut native_ok = false;

    if clipboard.is_none() {
        *clipboard = arboard::Clipboard::new().ok();
    }

    let mut needs_reinit = false;
    if let Some(handle) = clipboard.as_mut() {
        if handle.set_text(text.to_string()).is_ok() {
            native_ok = true;
            mirror_into_primary(handle, text);
        } else {
            needs_reinit = true;
        }
    } else {
        needs_reinit = true;
    }

    if needs_reinit {
        if let Ok(mut new_clipboard) = arboard::Clipboard::new() {
            if new_clipboard.set_text(text.to_string()).is_ok() {
                native_ok = true;
                mirror_into_primary(&mut new_clipboard, text);
            }
            *clipboard = Some(new_clipboard);
        }
    }

    native_ok
}

/// Mirrors `text` into the X11/Wayland Primary Selection on Linux so that a
/// middle-click paste stays consistent with the standard clipboard.
#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
fn mirror_into_primary(clipboard: &mut arboard::Clipboard, text: &str) {
    use arboard::SetExtLinux;
    let _ = clipboard
        .set()
        .clipboard(arboard::LinuxClipboardKind::Primary)
        .text(text.to_string());
}

/// No-op on platforms without an X11/Wayland Primary Selection.
#[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten"))))]
fn mirror_into_primary(_clipboard: &mut arboard::Clipboard, _text: &str) {}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_test_worker() -> ClipboardWorker {
        let (sender, receiver) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        std::thread::Builder::new()
            .name("tauqe-clipboard-test".to_string())
            .spawn(move || worker_loop(receiver, worker_busy))
            .expect("failed to spawn test clipboard worker");
        ClipboardWorker { sender, busy }
    }

    #[test]
    fn test_selection_accessors_degrade_gracefully() {
        // A headless environment has no display server, so the accessors must
        // return `None` instead of panicking. Where a display exists they read
        // whatever the developer currently has selected. The worker guarantees
        // the call returns even if the selection owner is hung.
        let _ = get_from_clipboard();
        let _ = get_from_primary_selection();
        let _ = get_primary_or_clipboard();
    }

    #[test]
    fn test_worker_dispatches_work_and_returns_the_result() {
        let worker = spawn_test_worker();
        assert_eq!(
            run_worker_job(&worker, |_clipboard: &mut ClipboardHandle| 42, 0),
            42
        );
    }

    #[test]
    fn test_worker_falls_back_after_timeout() {
        let worker = spawn_test_worker();
        let start = std::time::Instant::now();
        let got = run_worker_job(
            &worker,
            |_clipboard: &mut ClipboardHandle| {
                std::thread::sleep(NATIVE_CLIPBOARD_TIMEOUT + Duration::from_millis(200));
                1
            },
            0,
        );
        assert_eq!(got, 0, "a slow worker must yield the fallback value");
        assert!(
            start.elapsed() >= NATIVE_CLIPBOARD_TIMEOUT,
            "the call must wait at least the timeout before falling back"
        );
    }

    #[test]
    fn test_worker_rejects_requests_while_busy() {
        let worker = spawn_test_worker();

        // Occupy the worker for longer than the caller deadline.
        assert_eq!(
            run_worker_job(
                &worker,
                |_clipboard: &mut ClipboardHandle| {
                    std::thread::sleep(NATIVE_CLIPBOARD_TIMEOUT + Duration::from_millis(200));
                    1
                },
                0,
            ),
            0
        );

        // A second request must be dropped immediately instead of queueing
        // another OS thread or blocking on a shared lock.
        let start = std::time::Instant::now();
        assert_eq!(
            run_worker_job(&worker, |_clipboard: &mut ClipboardHandle| 5, 9),
            9
        );
        assert!(
            start.elapsed() < NATIVE_CLIPBOARD_TIMEOUT,
            "a busy worker must reject the request without waiting"
        );
    }

    #[test]
    fn test_worker_survives_a_panicking_job() {
        let worker = spawn_test_worker();

        // A panicking job must degrade to the fallback rather than propagate.
        let got = run_worker_job(
            &worker,
            |_clipboard: &mut ClipboardHandle| -> i32 {
                panic!("worker failure");
            },
            7,
        );
        assert_eq!(got, 7);

        // The worker catches the unwind and clears its busy flag shortly after;
        // wait for it to become idle again, then confirm it still serves work.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while worker.busy.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            run_worker_job(&worker, |_clipboard: &mut ClipboardHandle| 99, 0),
            99
        );
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"Hello, World!"), "SGVsbG8sIFdvcmxkIQ==");
    }
}
