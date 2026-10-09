use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{AppState, ViewMode};
use crate::ui::develop::SPINNER_FRAMES;

pub fn render_footer(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let mut footer_spans = Vec::new();

    // 1. TAUQE badge
    let badge_text = " TAUQE ";
    footer_spans.push(Span::styled(
        badge_text,
        Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
    ));
    footer_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));

    // 2. Financial indicator (Σ session · last · now)
    if let Some(cost_str) = format_footer_cost(
        state.model.session_total_cost,
        state.model.prev_cost,
        state.model.current_cost,
        state.model.is_busy(),
    ) {
        footer_spans.push(Span::styled(
            cost_str,
            Style::default().fg(Color::Yellow).bold(),
        ));
        footer_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    }

    // 3. Status, Round, Retry, Current file indicators AND Notifications
    let active_notif = state.active_notification();

    if state.review.running {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];
        footer_spans.push(Span::styled(
            format!("{} Reviewing", spin),
            Style::default().fg(Color::Magenta).bold(),
        ));
        if let Some(notif) = active_notif {
            footer_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            let (icon, color) = format_notification(notif);
            footer_spans.push(Span::styled(
                format!("{} {}", icon, notif.text),
                Style::default().fg(color).bold(),
            ));
        }
    } else if state.model.is_busy() {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];

        let (status_name, status_color) = match state.model.turn_phase {
            Some(tauqe_protocol::TurnPhase::Discovery) => ("Discovery", Color::Cyan),
            Some(tauqe_protocol::TurnPhase::Proposal) => {
                if state.model.status == "thinking" {
                    ("Thinking", Color::Rgb(130, 170, 220))
                } else {
                    ("Proposal", Color::Yellow)
                }
            }
            Some(tauqe_protocol::TurnPhase::Staging) => ("Staging", Color::Magenta),
            Some(tauqe_protocol::TurnPhase::Verification) => ("Verifying", Color::Cyan),
            Some(tauqe_protocol::TurnPhase::Healing) => ("Healing", Color::LightRed),
            None => match state.model.status.as_str() {
                "thinking" => ("Thinking", Color::Rgb(130, 170, 220)),
                "editing" => ("Editing", Color::Magenta),
                "verifying" => ("Verifying", Color::Cyan),
                "responding" | "streaming" => ("Responding", Color::Yellow),
                "awaiting" | "starting" => ("Awaiting", Color::Yellow),
                _ => (state.model.status.as_str(), Color::Yellow),
            },
        };

        footer_spans.push(Span::styled(
            format!("{} {}", spin, status_name),
            Style::default().fg(status_color).bold(),
        ));

        if let Some(notif) = active_notif {
            footer_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            let (icon, color) = format_notification(notif);
            footer_spans.push(Span::styled(
                format!("{} {}", icon, notif.text),
                Style::default().fg(color).bold(),
            ));
        } else {
            if let Some(phase) = state.model.turn_phase {
                let inner_width = area.width.saturating_sub(2) as usize;
                if inner_width >= 80 {
                    footer_spans.push(Span::raw(" "));
                    footer_spans.extend(render_turn_phase_pipeline(phase));
                }
            }

            if let Some(detail) = &state.model.turn_phase_detail {
                footer_spans.push(Span::raw(" "));
                footer_spans.push(Span::styled(
                    format!("[{}]", detail),
                    Style::default().fg(Color::Yellow).bold(),
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
                    Style::default().fg(Color::Cyan).bold(),
                ));
            } else if let Some(round_str) = extract_current_round(&state.model.text) {
                footer_spans.push(Span::raw(" "));
                footer_spans.push(Span::styled(
                    format!("[{}]", round_str),
                    Style::default().fg(Color::Cyan).bold(),
                ));
            }

            // Active retrying / modifying file
            let active_file = state
                .model
                .files
                .iter()
                .rev()
                .find(|f| f.status == "running" || f.status == "retrying");

            if let Some(f) = active_file {
                if let Some(retry) = &f.retry_info {
                    footer_spans.push(Span::raw(" "));
                    footer_spans.push(Span::styled(
                        format!("[{}]", retry),
                        Style::default().fg(Color::Yellow).bold(),
                    ));
                }
                footer_spans.push(Span::raw(" "));
                footer_spans.push(Span::styled(
                    format!("→ {}", f.path),
                    Style::default().fg(Color::White).bold(),
                ));
            } else if let Some(cmd) = &state.model.toolchain_command {
                footer_spans.push(Span::raw(" "));
                footer_spans.push(Span::styled(
                    format!("→ {}", cmd),
                    Style::default().fg(Color::Cyan).bold(),
                ));
            }
        }
    } else if let Some(notif) = active_notif {
        let (icon, color) = format_notification(notif);
        footer_spans.push(Span::styled(
            format!("{} {}", icon, notif.text),
            Style::default().fg(color).bold(),
        ));
    } else {
        let (status_lbl, status_style) = match state.model.status.as_str() {
            "done" => ("Done", Style::default().fg(Color::Green).bold()),
            "cancelled" => ("Cancelled", Style::default().fg(Color::Red).bold()),
            "error" => ("Error", Style::default().fg(Color::Red).bold()),
            _ => ("Ready", Style::default().fg(Color::Green)),
        };
        footer_spans.push(Span::styled(status_lbl, status_style));
    }

    let inner_width = area.width.saturating_sub(2) as usize;

    if state.view_mode == ViewMode::History {
        let tokens = state.history_view.get_estimated_tokens();
        let total = state.history_view.total_count;
        let hist_stats = format!(" {} entries (~{} tokens) ", total, tokens);
        let left_len: usize = footer_spans.iter().map(|s| s.content.chars().count()).sum();
        let right_len = hist_stats.chars().count();
        if left_len + right_len < inner_width {
            let padding = inner_width - left_len - right_len;
            footer_spans.push(Span::raw(" ".repeat(padding)));
            footer_spans.push(Span::styled(
                hist_stats,
                Style::default().fg(Color::Cyan).bold(),
            ));
        } else {
            footer_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            footer_spans.push(Span::styled(
                hist_stats,
                Style::default().fg(Color::Cyan).bold(),
            ));
        }
    }

    let footer_line = Line::from(footer_spans);
    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

pub fn format_notification(notif: &crate::app::AppNotification) -> (&'static str, Color) {
    match notif.level {
        crate::app::NotificationLevel::Info => ("ℹ", Color::Cyan),
        crate::app::NotificationLevel::Success => ("✓", Color::Green),
        crate::app::NotificationLevel::Warning => ("⚠", Color::Yellow),
        crate::app::NotificationLevel::Error => ("✗", Color::Red),
    }
}

pub fn render_turn_phase_pipeline(current: tauqe_protocol::TurnPhase) -> Vec<Span<'static>> {
    let phases = [
        (tauqe_protocol::TurnPhase::Discovery, "Discovery"),
        (tauqe_protocol::TurnPhase::Proposal, "Proposal"),
        (tauqe_protocol::TurnPhase::Staging, "Staging"),
        (tauqe_protocol::TurnPhase::Verification, "Verify"),
        (tauqe_protocol::TurnPhase::Healing, "Heal"),
    ];

    let mut spans = Vec::new();
    spans.push(Span::styled("[", Style::default().fg(Color::DarkGray)));
    for (idx, (phase, label)) in phases.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("→", Style::default().fg(Color::DarkGray)));
        }
        if *phase == current {
            spans.push(Span::styled(
                *label,
                Style::default().fg(Color::Cyan).bold(),
            ));
        } else {
            spans.push(Span::styled(
                *label,
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    spans.push(Span::styled("]", Style::default().fg(Color::DarkGray)));
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

    #[test]
    fn test_render_turn_phase_pipeline() {
        let spans = render_turn_phase_pipeline(tauqe_protocol::TurnPhase::Staging);
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(combined, "[Discovery→Proposal→Staging→Verify→Heal]");
        let staging_span = spans.iter().find(|s| s.content == "Staging").unwrap();
        assert_eq!(staging_span.style.fg, Some(Color::Cyan));
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
        let notif_info = crate::app::AppNotification {
            level: crate::app::NotificationLevel::Info,
            text: "info text".to_string(),
            created_at: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(3),
        };
        assert_eq!(format_notification(&notif_info), ("ℹ", Color::Cyan));

        let notif_err = crate::app::AppNotification {
            level: crate::app::NotificationLevel::Error,
            text: "err text".to_string(),
            created_at: std::time::Instant::now(),
            ttl: std::time::Duration::from_secs(6),
        };
        assert_eq!(format_notification(&notif_err), ("✗", Color::Red));
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
}
