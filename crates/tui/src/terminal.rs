use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::{DesktopNotificationProtocol, NotificationConfig};

use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

/// Whether [`init_terminal`] pushed the keyboard enhancement flags.
///
/// The flag outlives the guard: the panic hook and the termination handler
/// restore the terminal without ever holding it.
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Undoes every change [`init_terminal`] made to the terminal.
///
/// A run ends either through the guard, through the panic hook, or through the
/// termination handler, and all three must leave the same terminal behind. The
/// steps therefore live here once and in one order; a copy per exit path lets
/// one of them silently miss a step the others undo and strand the developer in
/// a raw alternate screen. The routine is idempotent, because more than one
/// path can fire for a single run.
pub fn restore_terminal() {
    reset_terminal_background();
    let _ = execute!(
        stdout(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        Show,
    );
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Restores the terminal and exits when the process is asked to terminate.
///
/// A termination signal unwinds the process without dropping the guard, so an
/// external `SIGTERM` from a supervisor or a closed session would leave the
/// developer's terminal in raw mode and inside the alternate screen. The
/// handler only restores the terminal; it never interprets the signal as a TUI
/// command. The keyboard's `Ctrl+C` does not reach it either: raw mode disables
/// signal generation and the input layer normalizes the chord itself.
pub fn install_termination_restore() {
    tokio::spawn(async {
        wait_for_termination().await;
        restore_terminal();
        std::process::exit(130);
    });
}

async fn wait_for_termination() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub fn init_terminal(
) -> anyhow::Result<(Terminal<CrosstermBackend<std::io::Stdout>>, TerminalGuard)> {
    enable_raw_mode()?;
    let enhancement_supported =
        crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);

    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange,
    )?;

    if enhancement_supported
        && execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )
        .is_ok()
    {
        KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::SeqCst);
    }

    let guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok((terminal, guard))
}

pub fn spawn_event_reader() -> mpsc::Receiver<crossterm::event::Event> {
    let (event_tx, event_rx) = mpsc::channel::<crossterm::event::Event>(100);
    tokio::task::spawn_blocking(move || {
        while !event_tx.is_closed() {
            match event::poll(Duration::from_millis(10)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if event_tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    event_rx
}

/// Filters out control characters and delimiters, limiting length to safely pass through terminal OSC.
pub fn sanitize_notification_text(raw: &str, max_len: usize) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(max_len));
    for c in raw.chars() {
        if cleaned.chars().count() >= max_len {
            break;
        }
        if c.is_control() {
            if !cleaned.ends_with(' ') {
                cleaned.push(' ');
            }
        } else if c == ';' {
            cleaned.push(':');
        } else {
            cleaned.push(c);
        }
    }
    cleaned.trim().to_string()
}

pub fn wrap_osc_payload(payload: &str) -> Vec<u8> {
    if std::env::var_os("TMUX").is_some() {
        let escaped = payload.replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;{}\x1b\\", escaped).into_bytes()
    } else {
        payload.as_bytes().to_vec()
    }
}

pub fn format_osc11_background(color: crate::theme::color::RgbColor) -> Vec<u8> {
    let payload = format!(
        "\x1b]11;#{:02x}{:02x}{:02x}\x07\x1b]11;#{:02x}{:02x}{:02x}\x1b\\",
        color.r, color.g, color.b, color.r, color.g, color.b
    );
    wrap_osc_payload(&payload)
}

pub fn format_osc111_reset() -> Vec<u8> {
    let payload = "\x1b]111\x07\x1b]111\x1b\\";
    wrap_osc_payload(payload)
}

/// Updates the host terminal's default window background color via OSC 11 to match
/// the active canvas palette, coloring any external window margins/padding.
pub fn update_terminal_background(color: crate::theme::color::RgbColor) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CURRENT_COLOR: AtomicU32 = AtomicU32::new(u32::MAX);
    let packed = ((color.r as u32) << 16) | ((color.g as u32) << 8) | (color.b as u32);
    if CURRENT_COLOR.swap(packed, Ordering::Relaxed) != packed {
        let payload = format_osc11_background(color);
        let mut out = stdout();
        let _ = out.write_all(&payload);
        let _ = out.flush();
    }
}

/// Resets the host terminal's window background color via OSC 111.
pub fn reset_terminal_background() {
    let payload = format_osc111_reset();
    let mut out = stdout();
    let _ = out.write_all(&payload);
    let _ = out.flush();
}

pub fn format_desktop_notification(
    title: &str,
    body: &str,
    protocol: DesktopNotificationProtocol,
) -> Vec<u8> {
    let clean_title = sanitize_notification_text(title, 64);
    let clean_body = sanitize_notification_text(body, 160);
    let payload = match protocol {
        DesktopNotificationProtocol::Osc9 => {
            format!("\x1b]9;{}: {}\x1b\\", clean_title, clean_body)
        }
        DesktopNotificationProtocol::Osc777 => {
            format!("\x1b]777;notify;{};{}\x1b\\", clean_title, clean_body)
        }
        DesktopNotificationProtocol::Both => format!(
            "\x1b]777;notify;{};{}\x1b\\\x1b]9;{}: {}\x1b\\",
            clean_title, clean_body, clean_title, clean_body
        ),
    };
    // Under `tmux` the emitted OSC sequence must travel inside a DCS passthrough
    // frame, otherwise the multiplexer swallows it before reaching the terminal.
    wrap_osc_payload(&payload)
}

pub fn trigger_turn_notification(
    title: &str,
    body: &str,
    config: &NotificationConfig,
    focused: bool,
) {
    if config.only_unfocused && focused {
        return;
    }

    let mut out = stdout();
    if config.sound {
        let _ = out.write_all(b"\x07");
    }
    if config.desktop {
        let payload = format_desktop_notification(title, body, config.desktop_protocol);
        let _ = out.write_all(&payload);
    }
    let _ = out.flush();

    if let Some(ref cmd) = config.command {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            spawn_notification_command(trimmed);
        }
    }
}

/// Launches the configured notification command hook and reaps it in a
/// background task.
///
/// The child is waited on asynchronously so a finished hook never lingers as a
/// zombie for the lifetime of the TUI process. The hook is best-effort: a
/// spawn failure is silently ignored rather than surfacing into the turn
/// lifecycle.
fn spawn_notification_command(command: &str) {
    #[cfg(unix)]
    {
        if let Ok(mut child) = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .spawn()
        {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
    #[cfg(windows)]
    {
        if let Ok(mut child) = tokio::process::Command::new("cmd")
            .arg("/C")
            .arg(command)
            .spawn()
        {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_notification_text_strips_controls_and_semicolons() {
        let input = "feat(tui): \x1b[31mred\x1b[0m; \x07alert\nnewline\tfoo";
        let cleaned = sanitize_notification_text(input, 100);
        assert!(!cleaned.contains('\x1b'));
        assert!(!cleaned.contains('\x07'));
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\t'));
        assert!(!cleaned.contains(';'));
        assert_eq!(cleaned, "feat(tui): [31mred [0m: alert newline foo");
    }

    #[test]
    fn test_sanitize_notification_text_respects_char_limit_utf8() {
        let input = "Проверка длинного описания коммита для уведомления";
        let cleaned = sanitize_notification_text(input, 15);
        assert_eq!(cleaned.chars().count(), 15);
        assert_eq!(cleaned, "Проверка длинно");
    }

    #[test]
    fn test_format_osc11_background_and_reset() {
        let color = crate::theme::color::RgbColor::new(26, 27, 38);
        let osc11 = format_osc11_background(color);
        let s = String::from_utf8(osc11).unwrap();
        assert!(s.contains("]11;#1a1b26"));

        let osc111 = format_osc111_reset();
        let s111 = String::from_utf8(osc111).unwrap();
        assert!(s111.contains("]111"));
    }

    #[test]
    fn test_format_desktop_notification_protocols() {
        // Compare against `wrap_osc_payload` so the assertion holds whether or
        // not the test process itself runs inside `tmux`.
        let osc9 = format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Osc9);
        assert_eq!(osc9, wrap_osc_payload("\x1b]9;Title: Body\x1b\\"));

        let osc777 =
            format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Osc777);
        assert_eq!(osc777, wrap_osc_payload("\x1b]777;notify;Title;Body\x1b\\"));

        let both = format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Both);
        assert_eq!(
            both,
            wrap_osc_payload("\x1b]777;notify;Title;Body\x1b\\\x1b]9;Title: Body\x1b\\")
        );
    }

    /// The guard, the panic hook, and the termination handler can all fire for
    /// one run, so a repeated restore must not pop a keyboard enhancement the
    /// process never pushed.
    #[test]
    fn test_restore_terminal_is_idempotent() {
        KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::SeqCst);
        restore_terminal();
        assert!(!KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::SeqCst));
        restore_terminal();
        assert!(!KEYBOARD_ENHANCEMENT_ACTIVE.load(Ordering::SeqCst));
    }
}
