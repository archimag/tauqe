use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, ViewMode};
use crate::theme::color::RgbColor;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::theme::palette::ThemePalette;
use crate::views::develop::{FileEditStatus, ToolchainState, TurnStatus, SPINNER_FRAMES};

pub fn render_footer(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let engine = current_engine();
    render_footer_with_engine(frame, state, area, &engine);
}

fn render_footer_with_engine(
    frame: &mut ratatui::Frame,
    state: &AppState,
    area: Rect,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    let mut footer_spans = Vec::new();

    // 1. TAUQE badge
    let badge_text = " TAUQE ";
    footer_spans.push(Span::styled(
        badge_text,
        engine
            .bg(palette.button_active_bg)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    ));
    footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));

    // 2. Financial indicator (Σ session · last · now)
    if let Some(cost_str) = format_footer_cost(
        state.model.session_total_cost,
        state.model.prev_cost,
        state.model.current_cost,
        state.model.is_busy(),
    ) {
        footer_spans.push(Span::styled(
            cost_str,
            engine.fg(palette.status_warning).bold(),
        ));
        footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
    }

    // 3. Status, Round, Retry, Current file indicators AND Notifications
    let active_notif = state.active_notification();

    if state.review.running {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];
        footer_spans.push(Span::styled(
            format!("{} Reviewing", spin),
            engine.fg(palette.button_active_bg).bold(),
        ));
        if let Some(notif) = active_notif {
            footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
            let (icon, color) = format_notification(notif, &palette);
            footer_spans.push(Span::styled(
                format!("{} {}", icon, notif.text),
                engine.fg(color).bold(),
            ));
        }
    } else if state.model.is_busy() {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];

        let (status_name, status_color) = match state.model.turn_phase {
            Some(tauqe_protocol::TurnPhase::Discovery) => ("Discovery", palette.status_info),
            Some(tauqe_protocol::TurnPhase::Proposal) => {
                if state.model.status == TurnStatus::Thinking {
                    ("Thinking", palette.text_secondary)
                } else {
                    ("Proposal", palette.status_warning)
                }
            }
            Some(tauqe_protocol::TurnPhase::Staging) => ("Staging", palette.button_active_bg),
            Some(tauqe_protocol::TurnPhase::Verification) => ("Verifying", palette.status_info),
            Some(tauqe_protocol::TurnPhase::Healing) => ("Healing", palette.status_error),
            None => match state.model.status {
                TurnStatus::Thinking => ("Thinking", palette.text_secondary),
                TurnStatus::Editing => ("Editing", palette.button_active_bg),
                TurnStatus::Verifying => ("Verifying", palette.status_info),
                TurnStatus::Responding => ("Responding", palette.status_warning),
                TurnStatus::Awaiting => ("Awaiting", palette.status_warning),
                other => (other.label(), palette.status_warning),
            },
        };

        footer_spans.push(Span::styled(
            format!("{} {}", spin, status_name),
            engine.fg(status_color).bold(),
        ));

        // A graceful batch stop is in flight: the current member finishes and
        // the queue halts before the next one, so the developer sees exactly
        // which stop is pending.
        if state.batch_stopping {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                "[Stopping after current step]",
                engine.fg(palette.status_warning).bold(),
            ));
        }

        // Turn telemetry (phase pipeline, round/detail, and the active file) is
        // rendered unconditionally while a turn owns the interface, so a
        // transient notification can never erase the phase pipeline or the file
        // the model is working on. The notification follows as a compact badge
        // on the same status line instead of replacing the telemetry.
        if let Some(phase) = state.model.turn_phase {
            let inner_width = area.width.saturating_sub(2) as usize;
            if inner_width >= 80 {
                footer_spans.push(Span::raw(" "));
                footer_spans.extend(render_turn_phase_pipeline(engine, phase));
            }
        }

        if let Some(detail) = &state.model.turn_phase_detail {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                format!("[{}]", detail),
                engine.fg(palette.status_warning).bold(),
            ));
        } else if let Some(round) = state.model.turn_round {
            footer_spans.push(Span::raw(" "));
            let round_str = if let Some(max) = state.model.turn_max_rounds {
                format!("[Round {}/{}]", round, max)
            } else {
                format!("[Round {}]", round)
            };
            footer_spans.push(Span::styled(
                round_str,
                engine.fg(palette.status_info).bold(),
            ));
        } else if let Some(round_str) = extract_current_round(&state.model.text) {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                format!("[{}]", round_str),
                engine.fg(palette.status_info).bold(),
            ));
        }

        // Active retrying / modifying file
        let active_file = state
            .model
            .files
            .iter()
            .rev()
            .find(|f| matches!(f.status, FileEditStatus::Running | FileEditStatus::Retrying));

        if let Some(f) = active_file {
            if let Some(retry) = &f.retry_info {
                footer_spans.push(Span::raw(" "));
                footer_spans.push(Span::styled(
                    format!("[{}]", retry),
                    engine.fg(palette.status_warning).bold(),
                ));
            }
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                format!("→ {}", f.path),
                engine.fg(palette.text_primary).bold(),
            ));
        } else if let Some(cmd) = state
            .model
            .toolchain
            .as_ref()
            .and_then(ToolchainState::running_command)
        {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                format!("→ {}", cmd),
                engine.fg(palette.status_info).bold(),
            ));
        }

        if let Some(notif) = active_notif {
            footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
            let (icon, color) = format_notification(notif, &palette);
            footer_spans.push(Span::styled(
                format!("{} {}", icon, notif.text),
                engine.fg(color).bold(),
            ));
        }
    } else if let Some(notif) = active_notif {
        let (icon, color) = format_notification(notif, &palette);
        footer_spans.push(Span::styled(
            format!("{} {}", icon, notif.text),
            engine.fg(color).bold(),
        ));
    } else {
        let (status_lbl, status_style) = match state.model.status {
            TurnStatus::Done => ("Done", engine.fg(palette.status_success).bold()),
            TurnStatus::Cancelled => ("Cancelled", engine.fg(palette.status_error).bold()),
            TurnStatus::Error => ("Error", engine.fg(palette.status_error).bold()),
            _ => ("Ready", engine.fg(palette.status_success)),
        };
        footer_spans.push(Span::styled(status_lbl, status_style));
    }

    let inner_width = area.width.saturating_sub(2) as usize;

    let mut right_spans: Vec<Span<'static>> = Vec::new();

    if let Some(tokens) = state.model.repomap_tokens {
        if tokens > 0 {
            let (color, bold) = if state.model.is_busy() {
                (palette.status_info, true)
            } else {
                (palette.text_muted, false)
            };
            let mut style = engine.fg(color);
            if bold {
                style = style.bold();
            }
            right_spans.push(Span::styled(format_repomap_tokens(tokens), style));
        }
    }

    if state.view_mode == ViewMode::History {
        if !right_spans.is_empty() {
            right_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
        }
        let tokens = state.history_view.get_estimated_tokens();
        let total = state.history_view.total_count;
        let hist_stats = format!("{} entries (~{} tokens)", total, tokens);
        right_spans.push(Span::styled(
            hist_stats,
            engine.fg(palette.status_info).bold(),
        ));
    }

    if !right_spans.is_empty() {
        let left_len: usize = footer_spans.iter().map(|s| s.content.chars().count()).sum();
        let right_len: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
        let target_right_len = right_len + 1;
        if left_len + target_right_len < inner_width {
            let padding = inner_width - left_len - target_right_len;
            footer_spans.push(Span::raw(" ".repeat(padding)));
            footer_spans.extend(right_spans);
            footer_spans.push(Span::raw(" "));
        } else if left_len + right_len < inner_width {
            let padding = inner_width - left_len - right_len;
            footer_spans.push(Span::raw(" ".repeat(padding)));
            footer_spans.extend(right_spans);
        } else {
            footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
            footer_spans.extend(right_spans);
        }
    }

    let footer_line = Line::from(footer_spans);
    let footer = Paragraph::new(footer_line)
        .style(ratatui::style::Style::default().bg(engine.color(palette.canvas)))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(engine.fg(palette.border))
                .style(ratatui::style::Style::default().bg(engine.color(palette.canvas))),
        );
    frame.render_widget(footer, area);
}

pub fn format_notification(
    notif: &crate::app::AppNotification,
    palette: &ThemePalette,
) -> (&'static str, RgbColor) {
    match notif.level {
        crate::app::NotificationLevel::Info => ("ℹ", palette.status_info),
        crate::app::NotificationLevel::Success => ("✓", palette.status_success),
        crate::app::NotificationLevel::Warning => ("⚠", palette.status_warning),
        crate::app::NotificationLevel::Error => ("✗", palette.status_error),
    }
}

pub fn render_turn_phase_pipeline(
    engine: &ThemeEngine,
    current: tauqe_protocol::TurnPhase,
) -> Vec<Span<'static>> {
    let palette = engine.palette();
    let phases = [
        (tauqe_protocol::TurnPhase::Discovery, "Discovery"),
        (tauqe_protocol::TurnPhase::Proposal, "Proposal"),
        (tauqe_protocol::TurnPhase::Staging, "Staging"),
        (tauqe_protocol::TurnPhase::Verification, "Verify"),
        (tauqe_protocol::TurnPhase::Healing, "Heal"),
    ];

    let mut spans = Vec::new();
    spans.push(Span::styled("[", engine.fg(palette.text_muted)));
    for (idx, (phase, label)) in phases.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("→", engine.fg(palette.text_muted)));
        }
        if *phase == current {
            spans.push(Span::styled(*label, engine.fg(palette.status_info).bold()));
        } else {
            spans.push(Span::styled(*label, engine.fg(palette.text_muted)));
        }
    }
    spans.push(Span::styled("]", engine.fg(palette.text_muted)));
    spans
}

pub fn extract_current_round(text: &str) -> Option<String> {
    let last_round = text.rfind("[Round ");
    let last_retry = text.rfind("[Patch Retry ");
    match (last_round, last_retry) {
        (Some(r), Some(t)) if r >= t => extract_bracketed(&text[r..]),
        (Some(_), Some(t)) => extract_bracketed(&text[t..]),
        (Some(r), None) => extract_bracketed(&text[r..]),
        (None, Some(t)) => extract_bracketed(&text[t..]),
        (None, None) => None,
    }
}

fn extract_bracketed(slice: &str) -> Option<String> {
    slice.find(']').map(|end| slice[1..end].to_string())
}

pub fn format_repomap_tokens(tokens: usize) -> String {
    format!("map: {} tok", tokens)
}

pub fn format_footer_cost(
    session_cost: f64,
    prev_cost: Option<f64>,
    current_cost: Option<f64>,
    is_busy: bool,
) -> Option<String> {
    let prev = prev_cost.unwrap_or(0.0);
    let cur = current_cost.unwrap_or(0.0);
    let has_expenses = session_cost > 0.0 || prev > 0.0 || cur > 0.0;

    if !has_expenses {
        return None;
    }

    if is_busy {
        Some(format!(
            "Σ ${:.4} session · ${:.4} last · ${:.4} now",
            session_cost, prev, cur
        ))
    } else {
        Some(format!(
            "Σ ${:.4} session · ${:.4} last",
            session_cost, prev
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::theme::color::ColorDepth;
    use crate::theme::engine::ThemeEngine;
    use crate::theme::presets::ThemePreset;

    #[test]
    fn test_render_turn_phase_pipeline() {
        let engine = ThemeEngine::new(ThemePreset::TauqeDark, ColorDepth::Truecolor);
        let spans = render_turn_phase_pipeline(&engine, tauqe_protocol::TurnPhase::Staging);
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(combined, "[Discovery→Proposal→Staging→Verify→Heal]");
        let staging_span = spans.iter().find(|s| s.content == "Staging").unwrap();
        assert_eq!(
            staging_span.style.fg,
            Some(engine.color(engine.palette().status_info))
        );
    }

    #[test]
    fn test_extract_current_round() {
        assert_eq!(extract_current_round("Hello world"), None);
        assert_eq!(
            extract_current_round("Some text\n\n---\n**[Round 2]** Added 1 file\n\nMore text"),
            Some("Round 2".to_string())
        );
        assert_eq!(
            extract_current_round("Text [Round 2] more text [Round 3] now"),
            Some("Round 3".to_string())
        );
        assert_eq!(
            extract_current_round("Text [Round 2] [Patch Retry 1/3] fixing"),
            Some("Patch Retry 1/3".to_string())
        );
    }

    #[test]
    fn test_format_notification() {
        let palette = ThemePreset::TauqeDark.palette();
        let notif_info = crate::app::AppNotification {
            level: crate::app::NotificationLevel::Info,
            text: "info text".to_string(),
            created_at: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(3),
        };
        assert_eq!(
            format_notification(&notif_info, &palette),
            ("ℹ", palette.status_info)
        );

        let notif_err = crate::app::AppNotification {
            level: crate::app::NotificationLevel::Error,
            text: "err text".to_string(),
            created_at: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(6),
        };
        assert_eq!(
            format_notification(&notif_err, &palette),
            ("✗", palette.status_error)
        );
    }

    #[test]
    fn test_format_repomap_tokens() {
        assert_eq!(format_repomap_tokens(850), "map: 850 tok");
        assert_eq!(format_repomap_tokens(1024), "map: 1024 tok");
    }

    #[test]
    fn test_format_footer_cost() {
        assert_eq!(format_footer_cost(0.0, None, None, false), None);
        assert_eq!(format_footer_cost(0.0, Some(0.0), Some(0.0), true), None);

        assert_eq!(
            format_footer_cost(0.0150, Some(0.0050), None, false),
            Some("Σ $0.0150 session · $0.0050 last".to_string())
        );

        assert_eq!(
            format_footer_cost(0.0150, Some(0.0050), Some(0.0020), true),
            Some("Σ $0.0150 session · $0.0050 last · $0.0020 now".to_string())
        );
    }

    #[test]
    fn busy_footer_keeps_telemetry_alongside_notification() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.status = TurnStatus::Staging;
        state.model.turn_phase = Some(tauqe_protocol::TurnPhase::Staging);
        state.notify_warning("Patch applied");

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        // The autonomous-turn telemetry survives the transient notification ...
        assert!(
            content.contains("Discovery"),
            "phase pipeline must remain visible next to a notification: {content:?}"
        );
        // ... while the notification is still rendered on the same status line.
        assert!(
            content.contains("Patch applied"),
            "notification must remain visible next to the telemetry: {content:?}"
        );
    }

    #[test]
    fn busy_footer_shows_soft_stop_indicator() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(120, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.status = TurnStatus::Responding;
        state.model.is_batch = true;
        state.batch_stopping = true;

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            content.contains("Stopping after current step"),
            "the soft-stop indicator must be visible: {content:?}"
        );
    }

    #[test]
    fn footer_chrome_styles_track_theme_tokens() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        for preset in [ThemePreset::TauqeDark, ThemePreset::TauqeLight] {
            let engine = ThemeEngine::new(preset, ColorDepth::Truecolor);
            let palette = engine.palette();

            let backend = TestBackend::new(120, 3);
            let mut terminal = Terminal::new(backend).expect("terminal created");
            let mut state = crate::app::test_state();
            state.model.status = TurnStatus::Done;

            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_footer_with_engine(frame, &state, area, &engine);
                })
                .expect("draw succeeded");

            let buffer = terminal.backend().buffer();
            // The TAUQE badge is filled with the accent surface token.
            assert_eq!(buffer[(1, 1)].bg, engine.color(palette.button_active_bg));
            assert_eq!(buffer[(1, 1)].fg, engine.color(palette.text_inverse));
            // The divider next to the badge uses the muted chrome token.
            assert_eq!(buffer[(8, 1)].fg, engine.color(palette.text_muted));
            // The idle Ready/Done indicator uses the success status token.
            assert_eq!(buffer[(11, 1)].fg, engine.color(palette.status_success));
        }
    }
}
