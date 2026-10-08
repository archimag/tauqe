use std::io::{stdout, Write};
use std::time::Duration;

use crate::config::NotificationConfig;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

pub fn trigger_turn_notification(title: &str, body: &str, config: &NotificationConfig) {
    let mut out = stdout();
    if config.sound {
        let _ = out.write_all(b"\x07");
    }
    if config.desktop {
        let _ = write!(out, "\x1b]777;notify;{};{}\x1b\\", title, body);
        let _ = write!(out, "\x1b]9;{}: {}\x1b\\", title, body);
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
