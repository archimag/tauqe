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

    // 2. Financial indicator (🪙)
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

    // 3. Status, Round, Retry, Current file indicators
    if state.review.running {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];
        footer_spans.push(Span::styled(
            format!("{} Reviewing", spin),
            Style::default().fg(Color::Magenta).bold(),
        ));
    } else if state.model.is_busy() {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];

        let (status_name, status_color) = match state.model.status.as_str() {
            "thinking" => ("Thinking", Color::Rgb(130, 170, 220)),
            "editing" => ("Editing", Color::Magenta),
            "verifying" => ("Verifying", Color::Cyan),
            "responding" | "streaming" => ("Responding", Color::Yellow),
            "awaiting" | "starting" => ("Awaiting", Color::Yellow),
            _ => (state.model.status.as_str(), Color::Yellow),
        };

        footer_spans.push(Span::styled(
            format!("{} {}", spin, status_name),
            Style::default().fg(status_color).bold(),
        ));

        if let Some(round_str) = extract_current_round(&state.model.text) {
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
    } else if let Some(notif) = &state.model.git_notification {
        footer_spans.push(Span::styled(
            notif.clone(),
            Style::default().fg(Color::Green).bold(),
        ));
    } else if let (ViewMode::Context, Some(msg)) = (state.view_mode, &state.context_view.status_message) {
        footer_spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Green).bold(),
        ));
    } else if let Some((notif, inst)) = &state.model.copy_notification {
        if inst.elapsed().as_secs_f32() < 2.5 {
            footer_spans.push(Span::styled(
                format!("✓ {}", notif),
                Style::default().fg(Color::Green).bold(),
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
    } else {
        let (status_lbl, status_style) = match state.model.status.as_str() {
            "done" => ("Done", Style::default().fg(Color::Green).bold()),
            "cancelled" => ("Cancelled", Style::default().fg(Color::Red).bold()),
            "error" => ("Error", Style::default().fg(Color::Red).bold()),
            _ => ("Ready", Style::default().fg(Color::Green)),
        };
        footer_spans.push(Span::styled(status_lbl, status_style));
    }

    let footer_line = Line::from(footer_spans);
    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
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
        Some(format!("🪙 ${:.4} | ${:.4} | ${:.4}", session_cost, prev, cur))
    } else {
        Some(format!("🪙 ${:.4} | ${:.4}", session_cost, prev))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_format_footer_cost() {
        assert_eq!(format_footer_cost(0.0, None, None, false), None);
        assert_eq!(format_footer_cost(0.0, Some(0.0), Some(0.0), true), None);

        assert_eq!(
            format_footer_cost(0.0150, Some(0.0050), None, false),
            Some("🪙 $0.0150 | $0.0050".to_string())
        );

        assert_eq!(
            format_footer_cost(0.0150, Some(0.0050), Some(0.0020), true),
            Some("🪙 $0.0150 | $0.0050 | $0.0020".to_string())
        );
    }
}
