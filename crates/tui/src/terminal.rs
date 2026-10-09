use std::io::{stdout, Write};
use std::time::Duration;

use crate::config::{DesktopNotificationProtocol, NotificationConfig};

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

pub struct TerminalGuard {
    enhancement_active: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
        );
        if self.enhancement_active {
            let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub fn init_terminal() -> anyhow::Result<(Terminal<CrosstermBackend<std::io::Stdout>>, TerminalGuard)> {
    enable_raw_mode()?;
    let enhancement_supported = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);

    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange,
    )?;

    let mut enhancement_active = false;
    if enhancement_supported
        && execute!(
            stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )
        .is_ok()
    {
        enhancement_active = true;
    }

    let guard = TerminalGuard { enhancement_active };
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

pub fn format_desktop_notification(
    title: &str,
    body: &str,
    protocol: DesktopNotificationProtocol,
) -> Vec<u8> {
    let clean_title = sanitize_notification_text(title, 64);
    let clean_body = sanitize_notification_text(body, 160);
    let mut out = Vec::new();
    match protocol {
        DesktopNotificationProtocol::Osc9 => {
            let _ = write!(out, "\x1b]9;{}: {}\x1b\\", clean_title, clean_body);
        }
        DesktopNotificationProtocol::Osc777 => {
            let _ = write!(out, "\x1b]777;notify;{};{}\x1b\\", clean_title, clean_body);
        }
        DesktopNotificationProtocol::Both => {
            let _ = write!(out, "\x1b]777;notify;{};{}\x1b\\", clean_title, clean_body);
            let _ = write!(out, "\x1b]9;{}: {}\x1b\\", clean_title, clean_body);
        }
    }
    out
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
            let cmd_str = trimmed.to_string();
            #[cfg(unix)]
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd_str)
                .spawn();
            #[cfg(windows)]
            let _ = std::process::Command::new("cmd")
                .arg("/C")
                .arg(&cmd_str)
                .spawn();
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
    fn test_format_desktop_notification_protocols() {
        let osc9 = format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Osc9);
        assert_eq!(String::from_utf8(osc9).unwrap(), "\x1b]9;Title: Body\x1b\\");

        let osc777 = format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Osc777);
        assert_eq!(String::from_utf8(osc777).unwrap(), "\x1b]777;notify;Title;Body\x1b\\");

        let both = format_desktop_notification("Title", "Body", DesktopNotificationProtocol::Both);
        assert_eq!(
            String::from_utf8(both).unwrap(),
            "\x1b]777;notify;Title;Body\x1b\\\x1b]9;Title: Body\x1b\\"
        );
    }
}
