//! Full usage telemetry dialog.
//!
//! The footer keeps the usage summary to three compact values so turn telemetry
//! is never displaced (c-4). The complete per-slot breakdown — prompt,
//! completion, reasoning, and cached tokens, money, the harness-measured
//! duration, and the number of completed operations — lives here instead, and
//! it renders strictly from the daemon-owned snapshot the footer mirrors (c-1,
//! c-2). Every label resolves through the locale catalog, so neither the body
//! nor the footer hardcodes an English literal (c-5).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tauqe_protocol::model::{UsageTotals, UsageTotalsSnapshot};

use crate::components::modal::{render_dialog_footer, render_modal_container, ModalTone};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::centered_rect;
use crate::ui::format::{format_duration, format_money, format_tokens};

/// Frame of the usage summary dialog.
pub fn usage_summary_area(term: Rect) -> Rect {
    centered_rect(66, 60, term)
}

/// Renders the breakdown and clamps `scroll` to the content height.
pub fn render_usage_summary(
    frame: &mut ratatui::Frame,
    snapshot: &UsageTotalsSnapshot,
    scroll: &mut u16,
) {
    let engine = current_engine();
    let title = active_t("dialog.usage.title");
    let inner = render_modal_container(
        frame,
        usage_summary_area(frame.area()),
        &title,
        ModalTone::Info,
    );

    // The body scrolls; the hint stays a one-row strip docked to the card's
    // bottom edge, exactly like every other modal footer (Conventions §6).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    let body_area = chunks[0];
    let footer_area = chunks[1];

    let lines = usage_summary_lines(snapshot, &engine);
    *scroll = (*scroll).min(max_summary_scroll(lines.len(), body_area.height));
    frame.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), body_area);

    render_dialog_footer(frame, footer_area, &active_t("dialog.usage.footer"));
}

/// Highest valid scroll offset for `line_count` lines in a viewport that is
/// `visible_height` rows tall; zero when the whole breakdown already fits.
fn max_summary_scroll(line_count: usize, visible_height: u16) -> u16 {
    u16::try_from(line_count.saturating_sub(usize::from(visible_height))).unwrap_or(u16::MAX)
}

/// Builds the scrollable body: the session total, the last completed operation,
/// and — strictly while one is running — the operation in flight.
fn usage_summary_lines(
    snapshot: &UsageTotalsSnapshot,
    engine: &ThemeEngine,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw("")];

    let current = snapshot.cur.as_ref();
    let empty = snapshot.operations == 0
        && snapshot.session.is_empty()
        && snapshot.prev.is_empty()
        && current.is_none_or(UsageTotals::is_empty);
    if empty {
        lines.push(Line::from(Span::styled(
            format!("  {}", active_t("dialog.usage.empty")),
            engine.fg(engine.palette().text_muted),
        )));
        return lines;
    }

    push_totals_block(
        &mut lines,
        "dialog.usage.session",
        &snapshot.session,
        Some(snapshot.operations),
        engine,
    );
    push_totals_block(&mut lines, "dialog.usage.last", &snapshot.prev, None, engine);
    if let Some(current) = current {
        push_totals_block(&mut lines, "dialog.usage.current", current, None, engine);
    }
    lines
}

/// Appends one titled block of usage rows, separated from the previous block by
/// a blank line. Optional breakdowns appear only when the provider reported
/// them, so an absent value is never rendered as a misleading zero.
fn push_totals_block(
    lines: &mut Vec<Line<'static>>,
    heading_key: &str,
    totals: &UsageTotals,
    operations: Option<u64>,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    if lines.len() > 1 {
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled(
        active_t(heading_key),
        engine.fg(palette.status_info).bold(),
    )));

    push_row(
        lines,
        "dialog.usage.prompt_tokens",
        format_tokens(totals.prompt_tokens),
        engine,
    );
    push_row(
        lines,
        "dialog.usage.completion_tokens",
        format_tokens(totals.completion_tokens),
        engine,
    );
    if let Some(reasoning) = totals.reasoning_tokens.filter(|tokens| *tokens > 0) {
        push_row(
            lines,
            "dialog.usage.reasoning_tokens",
            format_tokens(reasoning),
            engine,
        );
    }
    if let Some(cached) = totals.cached_tokens.filter(|tokens| *tokens > 0) {
        push_row(
            lines,
            "dialog.usage.cached_tokens",
            format_tokens(cached),
            engine,
        );
    }
    push_row(
        lines,
        "dialog.usage.total_tokens",
        format_tokens(totals.total_tokens),
        engine,
    );
    if totals.cost > 0.0 {
        push_row(lines, "dialog.usage.cost", format_money(totals.cost), engine);
    }
    if totals.duration_ms > 0 {
        push_row(
            lines,
            "dialog.usage.duration",
            format_duration(totals.duration_ms),
            engine,
        );
    }
    if let Some(operations) = operations {
        push_row(
            lines,
            "dialog.usage.operations",
            operations.to_string(),
            engine,
        );
    }
}

/// Appends one `Label: value` row with a localized label.
fn push_row(
    lines: &mut Vec<Line<'static>>,
    label_key: &str,
    value: String,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}: ", active_t(label_key)),
            engine.fg(palette.text_secondary),
        ),
        Span::styled(value, engine.fg(palette.text_primary).bold()),
    ]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn usage(prompt: u32, completion: u32, cost: f64, duration_ms: u64) -> UsageTotals {
        UsageTotals {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            reasoning_tokens: Some(prompt / 4),
            cached_tokens: Some(prompt / 2),
            cost,
            duration_ms,
        }
    }

    fn three_slot_snapshot() -> UsageTotalsSnapshot {
        UsageTotalsSnapshot {
            session: usage(1200, 300, 0.018, 45_000),
            prev: usage(400, 100, 0.006, 12_000),
            cur: Some(usage(50, 10, 0.001, 2_000)),
            operations: 3,
        }
    }

    fn rendered_text(snapshot: &UsageTotalsSnapshot) -> String {
        let engine = current_engine();
        usage_summary_lines(snapshot, &engine)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_session_last_and_current_blocks_with_the_full_breakdown() {
        let text = rendered_text(&three_slot_snapshot());

        assert!(text.contains("Session"), "session block: {text}");
        assert!(text.contains("Last Operation"), "last block: {text}");
        assert!(text.contains("Current Operation"), "current block: {text}");

        assert!(text.contains("Prompt tokens: 1200"), "{text}");
        assert!(text.contains("Completion tokens: 300"), "{text}");
        assert!(text.contains("Reasoning tokens: 300"), "{text}");
        assert!(text.contains("Cached tokens: 600"), "{text}");
        assert!(text.contains("Total tokens: 1500"), "{text}");
        assert!(text.contains("Cost: $0.0180"), "{text}");
        assert!(text.contains("Duration: 45.0s"), "{text}");
        assert!(text.contains("Operations: 3"), "{text}");
    }

    #[test]
    fn omits_the_current_block_when_no_operation_is_in_flight() {
        let mut snapshot = three_slot_snapshot();
        snapshot.cur = None;

        let text = rendered_text(&snapshot);
        assert!(!text.contains("Current Operation"), "{text}");
        assert!(text.contains("Session"), "{text}");
        assert!(text.contains("Last Operation"), "{text}");
    }

    #[test]
    fn empty_snapshot_renders_the_localized_placeholder() {
        let text = rendered_text(&UsageTotalsSnapshot::default());

        assert!(text.contains("No usage recorded"), "{text}");
        assert!(!text.contains("Session"), "{text}");
        assert!(!text.contains("Last Operation"), "{text}");
    }

    #[test]
    fn optional_breakdowns_are_hidden_when_absent() {
        let snapshot = UsageTotalsSnapshot {
            session: UsageTotals {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..UsageTotals::default()
            },
            ..UsageTotalsSnapshot::default()
        };

        let text = rendered_text(&snapshot);
        assert!(!text.contains("Reasoning tokens"), "{text}");
        assert!(!text.contains("Cached tokens"), "{text}");
        assert!(!text.contains("Cost"), "{text}");
        assert!(!text.contains("Duration"), "{text}");
        assert!(text.contains("Total tokens: 15"), "{text}");
    }

    #[test]
    fn max_scroll_clamps_and_never_underflows() {
        assert_eq!(max_summary_scroll(10, 4), 6);
        assert_eq!(max_summary_scroll(3, 5), 0);
        assert_eq!(max_summary_scroll(0, 0), 0);
        assert_eq!(max_summary_scroll(usize::MAX, 1), u16::MAX);
    }

    #[test]
    fn rendering_clamps_an_out_of_range_scroll_offset() {
        let snapshot = three_slot_snapshot();
        let engine = current_engine();
        let line_count = usage_summary_lines(&snapshot, &engine).len();

        let term = Rect::new(0, 0, 100, 30);
        // Frame border (two rows) and the one-row docked hint leave the rest to
        // the scrollable body, mirroring the render path exactly.
        let body_height = usage_summary_area(term)
            .height
            .saturating_sub(2)
            .saturating_sub(1);
        let expected = max_summary_scroll(line_count, body_height);

        let mut scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| render_usage_summary(frame, &snapshot, &mut scroll))
            .unwrap();

        assert_eq!(scroll, expected, "the renderer must clamp the offset");
        assert!(scroll < u16::MAX);
    }
}
