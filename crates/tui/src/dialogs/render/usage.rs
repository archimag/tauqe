//! Full usage telemetry dialog.
//!
//! Renders a compact comparative telemetry matrix (c-1, c-2):
//! Columns: Metric | Last Operation (or Current Operation if in flight) | Session | Project Lifetime.
//! The table delivers high information density without vertical scrolling, immediately
//! presenting project lifetime totals alongside session and last-turn metrics.

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tauqe_protocol::model::UsageTotalsSnapshot;

use crate::components::modal::{
    modal_geometry, render_dialog_footer, render_modal_container, ModalTone,
};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::centered_rect;
use crate::ui::format::{format_duration, format_money, format_tokens};
use crate::ui::table::{ColumnAlign, TableBorderStyle, TableCell, UiTable};

/// Frame of the usage summary dialog.
pub fn usage_summary_area(term: Rect) -> Rect {
    centered_rect(68, 55, term)
}

/// Renders the comparative telemetry matrix and clamps `scroll` to the content height.
pub fn render_usage_summary(
    frame: &mut ratatui::Frame,
    snapshot: &UsageTotalsSnapshot,
    repomap_tokens: Option<usize>,
    scroll: &mut u16,
) {
    let engine = current_engine();
    let title = active_t("dialog.usage.title");
    let area = usage_summary_area(frame.area());
    render_modal_container(frame, area, &title, ModalTone::Info);

    let geometry = modal_geometry(area);

    let lines = usage_summary_lines(snapshot, repomap_tokens, &engine);
    *scroll = (*scroll).min(max_summary_scroll(lines.len(), geometry.body.height));
    frame.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), geometry.body);

    render_dialog_footer(frame, geometry.hint_row, &active_t("dialog.usage.footer"));
}

/// Highest valid scroll offset for `line_count` lines in a viewport that is
/// `visible_height` rows tall; zero when the whole breakdown already fits.
fn max_summary_scroll(line_count: usize, visible_height: u16) -> u16 {
    u16::try_from(line_count.saturating_sub(usize::from(visible_height))).unwrap_or(u16::MAX)
}

/// Builds the matrix body lines: a 4-column comparative table of metrics
/// across the active/last operation, session totals, and project lifetime totals.
fn usage_summary_lines(
    snapshot: &UsageTotalsSnapshot,
    repomap_tokens: Option<usize>,
    engine: &ThemeEngine,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::raw("")];

    let current = snapshot.cur.as_ref();
    let has_session_usage = snapshot.operations != 0
        || !snapshot.session.is_empty()
        || !snapshot.prev.is_empty()
        || current.is_some_and(|totals| !totals.is_empty());
    let has_lifetime_usage = snapshot.lifetime_operations != 0 || !snapshot.lifetime.is_empty();
    let map_tokens = repomap_tokens.filter(|tokens| *tokens > 0);

    if !has_session_usage && !has_lifetime_usage && map_tokens.is_none() {
        lines.push(Line::from(Span::styled(
            format!("  {}", active_t("dialog.usage.empty")),
            engine.fg(engine.palette().text_muted),
        )));
        return lines;
    }

    // Determine target slot for column 2: in-flight operation if active, else previous turn.
    let (op_totals, op_heading_key, op_count) = if let Some(cur) = current {
        (cur, "dialog.usage.current", 1u64)
    } else if snapshot.operations > 0 || !snapshot.prev.is_empty() {
        (&snapshot.prev, "dialog.usage.last", 1u64)
    } else {
        (&snapshot.prev, "dialog.usage.last", 0u64)
    };

    let mut table = UiTable::new(TableBorderStyle::Lightweight);
    table.column_aligns(vec![
        ColumnAlign::Left,
        ColumnAlign::Right,
        ColumnAlign::Right,
        ColumnAlign::Right,
    ]);

    let palette = engine.palette();
    table.header(vec![
        TableCell::styled(vec![Span::styled(
            active_t("dialog.usage.metric"),
            engine.fg(palette.text_secondary).bold(),
        )]),
        TableCell::styled(vec![Span::styled(
            active_t(op_heading_key),
            engine.fg(palette.status_info).bold(),
        )]),
        TableCell::styled(vec![Span::styled(
            active_t("dialog.usage.session"),
            engine.fg(palette.status_info).bold(),
        )]),
        TableCell::styled(vec![Span::styled(
            active_t("dialog.usage.lifetime"),
            engine.fg(palette.status_info).bold(),
        )]),
    ]);

    // Operations count
    push_metric_row(
        &mut table,
        "dialog.usage.operations",
        if op_count > 0 {
            op_count.to_string()
        } else {
            "-".to_string()
        },
        if snapshot.operations > 0 {
            snapshot.operations.to_string()
        } else {
            "-".to_string()
        },
        if snapshot.lifetime_operations > 0 {
            snapshot.lifetime_operations.to_string()
        } else {
            "-".to_string()
        },
        engine,
    );

    // Prompt tokens
    push_metric_row(
        &mut table,
        "dialog.usage.prompt_tokens",
        format_tokens_or_dash(op_totals.prompt_tokens, op_count),
        format_tokens_or_dash(snapshot.session.prompt_tokens, snapshot.operations),
        format_tokens_or_dash(
            snapshot.lifetime.prompt_tokens,
            snapshot.lifetime_operations,
        ),
        engine,
    );

    // Completion tokens
    push_metric_row(
        &mut table,
        "dialog.usage.completion_tokens",
        format_tokens_or_dash(op_totals.completion_tokens, op_count),
        format_tokens_or_dash(snapshot.session.completion_tokens, snapshot.operations),
        format_tokens_or_dash(
            snapshot.lifetime.completion_tokens,
            snapshot.lifetime_operations,
        ),
        engine,
    );

    // Reasoning tokens (rendered if present in any scope)
    let has_reasoning = op_totals.reasoning_tokens.unwrap_or(0) > 0
        || snapshot.session.reasoning_tokens.unwrap_or(0) > 0
        || snapshot.lifetime.reasoning_tokens.unwrap_or(0) > 0;
    if has_reasoning {
        push_metric_row(
            &mut table,
            "dialog.usage.reasoning_tokens",
            format_opt_tokens_or_dash(op_totals.reasoning_tokens),
            format_opt_tokens_or_dash(snapshot.session.reasoning_tokens),
            format_opt_tokens_or_dash(snapshot.lifetime.reasoning_tokens),
            engine,
        );
    }

    // Cached tokens (rendered if present in any scope)
    let has_cached = op_totals.cached_tokens.unwrap_or(0) > 0
        || snapshot.session.cached_tokens.unwrap_or(0) > 0
        || snapshot.lifetime.cached_tokens.unwrap_or(0) > 0;
    if has_cached {
        push_metric_row(
            &mut table,
            "dialog.usage.cached_tokens",
            format_cached_or_dash(op_totals.cached_tokens, op_totals.prompt_tokens),
            format_cached_or_dash(
                snapshot.session.cached_tokens,
                snapshot.session.prompt_tokens,
            ),
            format_cached_or_dash(
                snapshot.lifetime.cached_tokens,
                snapshot.lifetime.prompt_tokens,
            ),
            engine,
        );
    }

    // Total tokens
    push_metric_row(
        &mut table,
        "dialog.usage.total_tokens",
        format_tokens_or_dash(op_totals.total_tokens, op_count),
        format_tokens_or_dash(snapshot.session.total_tokens, snapshot.operations),
        format_tokens_or_dash(snapshot.lifetime.total_tokens, snapshot.lifetime_operations),
        engine,
    );

    // Cost (rendered if positive in any scope)
    let has_cost =
        op_totals.cost > 0.0 || snapshot.session.cost > 0.0 || snapshot.lifetime.cost > 0.0;
    if has_cost {
        push_metric_row(
            &mut table,
            "dialog.usage.cost",
            format_money_or_dash(op_totals.cost),
            format_money_or_dash(snapshot.session.cost),
            format_money_or_dash(snapshot.lifetime.cost),
            engine,
        );
    }

    // Duration (rendered if positive in any scope)
    let has_duration = op_totals.duration_ms > 0
        || snapshot.session.duration_ms > 0
        || snapshot.lifetime.duration_ms > 0;
    if has_duration {
        push_metric_row(
            &mut table,
            "dialog.usage.duration",
            format_duration_or_dash(op_totals.duration_ms),
            format_duration_or_dash(snapshot.session.duration_ms),
            format_duration_or_dash(snapshot.lifetime.duration_ms),
            engine,
        );
    }

    let table_lines = table.render(engine.fg(palette.border));
    let table_width = table_lines.iter().map(Line::width).max().unwrap_or(0);
    for line in table_lines {
        lines.push(indent_line(line));
    }

    // Repository map context cost (if present)
    if let Some(tokens) = map_tokens {
        push_divider(&mut lines, table_width, engine);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}: ", active_t("dialog.usage.map")),
                engine.fg(palette.text_secondary),
            ),
            Span::styled(
                format!(
                    "{} tokens",
                    format_tokens(u32::try_from(tokens).unwrap_or(u32::MAX))
                ),
                engine.fg(palette.text_primary).bold(),
            ),
        ]));
    }

    // If an operation is in-flight and previous operation also had tokens, show a compact note
    if current.is_some() && (!snapshot.prev.is_empty() || snapshot.operations > 0) {
        if map_tokens.is_none() {
            push_divider(&mut lines, table_width, engine);
        }
        // Cost and duration are both facts of the finished operation, so the
        // note reports them together instead of letting one crowd out the
        // other: the duration of the last operation is exactly the number the
        // developer waits on while the next one runs.
        let mut facts: Vec<String> = Vec::new();
        if snapshot.prev.cost > 0.0 {
            facts.push(format_money(snapshot.prev.cost));
        }
        if snapshot.prev.duration_ms > 0 {
            facts.push(format_duration(snapshot.prev.duration_ms));
        }
        let note = if facts.is_empty() {
            format!("{} tokens", format_tokens(snapshot.prev.total_tokens))
        } else {
            format!(
                "{} tokens ({})",
                format_tokens(snapshot.prev.total_tokens),
                facts.join(" · ")
            )
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}: ", active_t("dialog.usage.last")),
                engine.fg(palette.text_secondary),
            ),
            Span::styled(note, engine.fg(palette.text_primary)),
        ]));
    }

    lines
}

fn push_metric_row(
    table: &mut UiTable,
    label_key: &str,
    val_op: String,
    val_session: String,
    val_lifetime: String,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    table.push_row(vec![
        TableCell::styled(vec![Span::styled(
            active_t(label_key),
            engine.fg(palette.text_secondary),
        )]),
        TableCell::styled(vec![Span::styled(val_op, engine.fg(palette.text_primary))]),
        TableCell::styled(vec![Span::styled(
            val_session,
            engine.fg(palette.text_primary),
        )]),
        TableCell::styled(vec![Span::styled(
            val_lifetime,
            engine.fg(palette.text_primary),
        )]),
    ]);
}

/// Prepends the shared two-column gutter to a rendered table line.
fn indent_line(mut line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw("  "));
    spans.append(&mut line.spans);
    Line::from(spans)
}

fn push_divider(lines: &mut Vec<Line<'static>>, width: usize, engine: &ThemeEngine) {
    let palette = engine.palette();
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(width)),
        engine.fg(palette.border),
    )));
}

fn format_tokens_or_dash(tokens: u32, count: u64) -> String {
    if tokens > 0 || count > 0 {
        format_tokens(tokens)
    } else {
        "-".to_string()
    }
}

fn format_opt_tokens_or_dash(tokens: Option<u32>) -> String {
    match tokens {
        Some(t) if t > 0 => format_tokens(t),
        _ => "-".to_string(),
    }
}

fn format_cached_or_dash(cached: Option<u32>, prompt: u32) -> String {
    match cached {
        Some(c) if c > 0 => format_cached_tokens(c, prompt),
        _ => "-".to_string(),
    }
}

fn format_money_or_dash(cost: f64) -> String {
    if cost > 0.0 {
        format_money(cost)
    } else {
        "-".to_string()
    }
}

fn format_duration_or_dash(duration_ms: u64) -> String {
    if duration_ms > 0 {
        format_duration(duration_ms)
    } else {
        "-".to_string()
    }
}

/// Formats cached tokens with a prompt-cache hit rate percentage when prompt
/// tokens are positive (e.g. "600 (50%)").
fn format_cached_tokens(cached: u32, prompt_tokens: u32) -> String {
    if prompt_tokens > 0 {
        let pct = ((f64::from(cached) / f64::from(prompt_tokens)) * 100.0)
            .round()
            .min(100.0) as u64;
        format!("{} ({}%)", format_tokens(cached), pct)
    } else {
        format_tokens(cached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::model::UsageTotals;

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
            ..UsageTotalsSnapshot::default()
        }
    }

    fn rendered_text(snapshot: &UsageTotalsSnapshot) -> String {
        rendered_text_with_map(snapshot, None)
    }

    fn rendered_text_with_map(
        snapshot: &UsageTotalsSnapshot,
        repomap_tokens: Option<usize>,
    ) -> String {
        let engine = current_engine();
        usage_summary_lines(snapshot, repomap_tokens, &engine)
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

    /// Flattens a rendered line into plain text.
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Reports whether the line is the horizontal rule drawn under the table header.
    fn is_horizontal_rule(line: &Line<'_>) -> bool {
        let text = line_text(line);
        let trimmed = text.trim_start_matches(' ');
        !trimmed.is_empty() && trimmed.chars().all(|ch| ch == '─')
    }

    #[test]
    fn renders_comparative_table_columns_with_the_full_breakdown() {
        let text = rendered_text(&three_slot_snapshot());

        assert!(text.contains("Metric"), "table header: {text}");
        assert!(text.contains("Session"), "session column: {text}");
        assert!(
            text.contains("Current Operation"),
            "in-flight column: {text}"
        );
        assert!(text.contains("Project Lifetime"), "lifetime column: {text}");

        assert!(text.contains("Prompt tokens"), "{text}");
        assert!(text.contains("Completion tokens"), "{text}");
        assert!(text.contains("Reasoning tokens"), "{text}");
        assert!(text.contains("Cached tokens"), "{text}");
        assert!(text.contains("Total tokens"), "{text}");
        assert!(text.contains("Cost"), "{text}");
        assert!(text.contains("Duration"), "{text}");
        assert!(text.contains("Operations"), "{text}");

        assert!(text.contains("1200"), "session prompt tokens: {text}");
        assert!(text.contains("300"), "session completion tokens: {text}");
        assert!(text.contains("1500"), "session total tokens: {text}");
    }

    #[test]
    fn switches_column_to_last_operation_when_idle() {
        let mut snapshot = three_slot_snapshot();
        snapshot.cur = None;

        let text = rendered_text(&snapshot);
        assert!(!text.contains("Current Operation"), "{text}");
        assert!(text.contains("Last Operation"), "{text}");
        assert!(text.contains("Session"), "{text}");
        assert!(text.contains("Project Lifetime"), "{text}");
    }

    #[test]
    fn empty_snapshot_renders_the_localized_placeholder() {
        let text = rendered_text(&UsageTotalsSnapshot::default());

        assert!(text.contains("No usage recorded"), "{text}");
        assert!(!text.contains("Metric"), "{text}");
    }

    #[test]
    fn renders_the_project_lifetime_column_values() {
        let mut snapshot = three_slot_snapshot();
        snapshot.lifetime = usage(9_000, 1_500, 0.42, 600_000);
        snapshot.lifetime_operations = 7;

        let text = rendered_text(&snapshot);

        assert!(text.contains("Project Lifetime"), "{text}");
        assert!(text.contains("9000"), "lifetime prompt tokens: {text}");
        assert!(text.contains("1500"), "lifetime completion tokens: {text}");
        assert!(text.contains("10500"), "lifetime total tokens: {text}");
        assert!(text.contains("7"), "lifetime operations: {text}");
        assert!(text.contains("3"), "session operations: {text}");
    }

    #[test]
    fn renders_lifetime_totals_without_any_session_usage() {
        let snapshot = UsageTotalsSnapshot {
            lifetime: usage(1_000, 200, 0.05, 30_000),
            lifetime_operations: 2,
            ..UsageTotalsSnapshot::default()
        };

        let text = rendered_text(&snapshot);

        assert!(text.contains("Project Lifetime"), "{text}");
        assert!(text.contains("1000"), "{text}");
        assert!(text.contains("2"), "{text}");
        assert!(!text.contains("No usage recorded"), "{text}");
    }

    #[test]
    fn footer_hint_advertises_the_reset_chord() {
        let footer = active_t("dialog.usage.footer");

        assert!(footer.contains("Shift+C"), "{footer}");
        assert!(footer.contains("reset"), "{footer}");
        assert!(footer.contains("Esc"), "{footer}");
    }

    #[test]
    fn renders_the_repository_map_cost_as_its_own_row() {
        let text = rendered_text_with_map(&three_slot_snapshot(), Some(850));

        assert!(text.contains("Repository Map"), "{text}");
        assert!(text.contains("850"), "{text}");
    }

    #[test]
    fn omits_the_repository_map_block_without_a_recorded_cost() {
        let absent = rendered_text(&three_slot_snapshot());
        assert!(!absent.contains("Repository Map"), "{absent}");

        let zero = rendered_text_with_map(&three_slot_snapshot(), Some(0));
        assert!(!zero.contains("Repository Map"), "{zero}");
    }

    #[test]
    fn repository_map_cost_renders_without_provider_usage() {
        let text = rendered_text_with_map(&UsageTotalsSnapshot::default(), Some(64));

        assert!(text.contains("Repository Map"), "{text}");
        assert!(text.contains("64"), "{text}");
        assert!(!text.contains("No usage recorded"), "{text}");
    }

    #[test]
    fn compact_note_pairs_cost_with_duration() {
        let text = rendered_text(&three_slot_snapshot());

        // The previous operation carries 500 tokens, $0.0060, and 12.0s: the
        // note reports money and time together instead of one crowding out
        // the other while the next operation runs.
        assert!(text.contains("500 tokens ($0.0060 · 12.0s)"), "{text}");
    }

    #[test]
    fn compact_note_falls_back_to_duration_without_cost() {
        let mut snapshot = three_slot_snapshot();
        snapshot.prev.cost = 0.0;
        let text = rendered_text(&snapshot);

        assert!(text.contains("500 tokens (12.0s)"), "{text}");
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
        assert!(text.contains("Total tokens"), "{text}");
    }

    #[test]
    fn telemetry_table_rows_share_a_single_width() {
        let mut snapshot = three_slot_snapshot();
        snapshot.cur = None;

        let engine = current_engine();
        let lines = usage_summary_lines(&snapshot, None, &engine);
        let table = lines
            .get(1..)
            .expect("table body follows the leading spacer");
        assert!(table.len() > 2, "table renders a header, a rule, and rows");

        let rule_width = table
            .iter()
            .find(|line| is_horizontal_rule(line))
            .map(Line::width)
            .expect("lightweight header rule");

        for line in table {
            assert_eq!(
                line.width(),
                rule_width,
                "ragged telemetry row: {}",
                line_text(line)
            );
        }
    }

    #[test]
    fn format_cached_tokens_includes_hit_rate_percentage() {
        assert_eq!(format_cached_tokens(600, 1200), "600 (50%)");
        assert_eq!(format_cached_tokens(100, 300), "100 (33%)");
        assert_eq!(format_cached_tokens(500, 500), "500 (100%)");
        assert_eq!(format_cached_tokens(600, 500), "600 (100%)");
        assert_eq!(format_cached_tokens(600, 0), "600");
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
        let line_count = usage_summary_lines(&snapshot, None, &engine).len();

        let term = Rect::new(0, 0, 100, 30);
        let body_height = modal_geometry(usage_summary_area(term)).body.height;
        let expected = max_summary_scroll(line_count, body_height);

        let mut scroll = u16::MAX;
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| render_usage_summary(frame, &snapshot, None, &mut scroll))
            .unwrap();

        assert_eq!(scroll, expected, "the renderer must clamp the offset");
        assert!(scroll < u16::MAX);
    }

    /// Body lines of a snapshot without the leading spacer row.
    fn table_lines(snapshot: &UsageTotalsSnapshot) -> Vec<Line<'static>> {
        let engine = current_engine();
        usage_summary_lines(snapshot, None, &engine)
            .into_iter()
            .skip(1)
            .collect()
    }

    /// Display column at which `value` ends inside a rendered line.
    fn end_column(line: &Line<'static>, value: &str) -> usize {
        let text = line_text(line);
        let end = text.find(value).expect("rendered value") + value.len();
        Span::raw(text[..end].to_string()).width()
    }

    #[test]
    fn telemetry_rows_share_one_table_width() {
        let mut snapshot = three_slot_snapshot();
        snapshot.cur = None;

        let lines = table_lines(&snapshot);
        assert!(lines.len() > 2, "the telemetry table must render rows");
        let width = lines[0].width();
        for line in &lines {
            assert_eq!(line.width(), width, "ragged row: {:?}", line_text(line));
        }
    }

    #[test]
    fn telemetry_numeric_columns_share_a_right_edge() {
        let mut snapshot = three_slot_snapshot();
        snapshot.cur = None;

        let lines = table_lines(&snapshot);
        // Session prompt, completion, and total tokens differ in digit width,
        // so a right-aligned column must still end them on the same column.
        let edges: Vec<usize> = ["1200", "300", "1500"]
            .iter()
            .map(|value| {
                let line = lines
                    .iter()
                    .find(|line| line_text(line).contains(value))
                    .expect("session value rendered");
                end_column(line, value)
            })
            .collect();
        assert!(
            edges.windows(2).all(|pair| pair[0] == pair[1]),
            "session column is not right aligned: {edges:?}"
        );
    }
}
