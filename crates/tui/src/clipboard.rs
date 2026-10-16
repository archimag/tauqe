use std::io::Write;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

/// Upper bound (250 ms) for a single native clipboard operation.
///
/// `arboard` talks to an external X11/Wayland/OS clipboard process that can
/// stall for seconds when its owner is hung or slow. The TUI drives clipboard
/// access from its async event loop, so an unbounded call would freeze terminal
/// rendering. Native work runs on a short-lived worker thread and is awaited
/// for at most this long; on expiry the operation degrades safely — a copy
/// reports the native path as unavailable, a paste returns no text.
pub const NATIVE_CLIPBOARD_TIMEOUT: Duration = Duration::from_millis(250);

/// Maximum payload size (100 KB) for terminal OSC 52 clipboard sequences
/// to avoid terminal multiplexer buffer overflows.
pub const MAX_OSC52_BYTES: usize = 100 * 1024;

static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

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

/// Runs a blocking clipboard operation on a short-lived worker thread and
/// returns its result, or `fallback` if the worker does not finish within
/// [`NATIVE_CLIPBOARD_TIMEOUT`].
///
/// The external clipboard process can stall indefinitely; offloading the call
/// keeps that stall off the TUI event loop. A worker that outlives the timeout
/// is detached and its eventual result is discarded. A worker panic (which
/// drops its sender) is indistinguishable from a timeout and yields `fallback`.
fn run_bounded<T, F>(work: F, fallback: T) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("tauqe-clipboard".to_string())
        .spawn(move || {
            let _ = tx.send(work());
        });
    if spawned.is_ok() {
        rx.recv_timeout(NATIVE_CLIPBOARD_TIMEOUT)
            .unwrap_or(fallback)
    } else {
        fallback
    }
}

/// Reads plain text for a standard paste, re-initializing the shared handle if
/// necessary. When the standard clipboard holds no text, the X11/Wayland
/// Primary Selection is consulted as a fallback so that a recent mouse
/// selection remains pasteable.
pub fn get_from_clipboard() -> Option<String> {
    run_bounded(
        || read_standard_clipboard_now().or_else(read_primary_selection_now),
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
        || read_primary_selection_now().or_else(read_standard_clipboard_now),
        None,
    )
}

/// Reads text from the standard OS clipboard only, re-initializing the shared
/// handle once if it has gone stale.
fn read_standard_clipboard_now() -> Option<String> {
    if let Ok(mut guard) = CLIPBOARD.lock() {
        if guard.is_none() {
            *guard = arboard::Clipboard::new().ok();
        }

        if let Some(clipboard) = guard.as_mut() {
            match clipboard.get_text() {
                Ok(text) if !text.is_empty() => return Some(text),
                Ok(_) => return None,
                Err(_) => {}
            }
        }

        if let Ok(mut new_cb) = arboard::Clipboard::new() {
            let res = new_cb.get_text().ok();
            *guard = Some(new_cb);
            if let Some(text) = res {
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Reads text from the X11/Wayland Primary Selection buffer on platforms that
/// expose one, returning `None` everywhere else.
fn read_primary_selection_now() -> Option<String> {
    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
    {
        use arboard::GetExtLinux;

        if let Ok(mut guard) = CLIPBOARD.lock() {
            if guard.is_none() {
                *guard = arboard::Clipboard::new().ok();
            }

            if let Some(clipboard) = guard.as_mut() {
                return clipboard
                    .get()
                    .clipboard(arboard::LinuxClipboardKind::Primary)
                    .text()
                    .ok()
                    .filter(|text| !text.is_empty());
            }
        }
    }
    None
}

pub fn copy_to_clipboard(text: &str) -> CopyResult {
    let clean_text = text.trim_end_matches(['\r', '\n']);

    // In an SSH session the native clipboard belongs to the remote host and is
    // unreachable from the user's terminal, so only the OSC 52 sequence is
    // meaningful. Skip `arboard` entirely: with X11 forwarding a native write
    // can stall over the network even though its result is discarded below.
    let in_ssh = is_ssh_session();

    let native_ok = if in_ssh {
        false
    } else {
        let owned = clean_text.to_string();
        run_bounded(move || write_native_clipboard(&owned), false)
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
    // meaningful. Report success solely when it was actually emitted, instead
    // of claiming a native copy the user cannot observe.
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
fn write_native_clipboard(text: &str) -> bool {
    let mut native_ok = false;

    if let Ok(mut guard) = CLIPBOARD.lock() {
        if guard.is_none() {
            *guard = arboard::Clipboard::new().ok();
        }

        let mut needs_reinit = false;
        if let Some(clipboard) = guard.as_mut() {
            if clipboard.set_text(text.to_string()).is_ok() {
                native_ok = true;
                #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
                {
                    use arboard::SetExtLinux;
                    let _ = clipboard
                        .set()
                        .clipboard(arboard::LinuxClipboardKind::Primary)
                        .text(text.to_string());
                }
            } else {
                needs_reinit = true;
            }
        } else {
            needs_reinit = true;
        }

        if needs_reinit {
            if let Ok(mut new_cb) = arboard::Clipboard::new() {
                if new_cb.set_text(text.to_string()).is_ok() {
                    native_ok = true;
                    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android"), not(target_os = "emscripten")))]
                    {
                        use arboard::SetExtLinux;
                        let _ = new_cb
                            .set()
                            .clipboard(arboard::LinuxClipboardKind::Primary)
                            .text(text.to_string());
                    }
                }
                *guard = Some(new_cb);
            }
        }
    }

    native_ok
}

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

    #[test]
    fn test_selection_accessors_degrade_gracefully() {
        // A headless environment has no display server, so the accessors must
        // return `None` instead of panicking. Where a display exists they read
        // whatever the developer currently has selected. The bounded worker
        // guarantees the call returns even if the selection owner is hung.
        let _ = get_from_clipboard();
        let _ = get_from_primary_selection();
        let _ = get_primary_or_clipboard();
    }

    #[test]
    fn test_run_bounded_returns_worker_value_when_prompt() {
        assert_eq!(run_bounded(|| 42, 0), 42);
    }

    #[test]
    fn test_run_bounded_falls_back_on_timeout() {
        let start = std::time::Instant::now();
        let got = run_bounded(
            || {
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
    fn test_run_bounded_falls_back_on_worker_panic() {
        // A panicking worker drops its sender, which surfaces as a disconnect
        // and must degrade to the fallback instead of propagating the panic.
        let got = run_bounded(
            || -> i32 {
                panic!("worker failure");
            },
            7,
        );
        assert_eq!(got, 7);
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
