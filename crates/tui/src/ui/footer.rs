use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use tauqe_protocol::model::UsageTotalsSnapshot;

use crate::app::{AppState, ViewMode};
use crate::i18n::active_t;
use crate::theme::color::RgbColor;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::theme::palette::ThemePalette;
use crate::ui::format::{format_money, format_tokens};
use crate::views::repl::{FileEditStatus, ToolchainState, TurnStatus, SPINNER_FRAMES};

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

    // 2. Usage indicator (Σ session · last · now), degrading by width instead
    // of ever cutting a number in half (c-4).
    let usage_budget = (area.width.saturating_sub(2) as usize).saturating_sub(24);
    if let Some(usage_str) = format_footer_usage(&state.model.usage_totals, usage_budget) {
        footer_spans.push(Span::styled(
            usage_str,
            engine.fg(palette.status_warning).bold(),
        ));
        footer_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
    }

    // 3. Status, Round, Retry, Current file indicators AND Notifications
    let active_notif = state.active_notification();

    if state.review.running {
        let spin = SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()];
        footer_spans.push(Span::styled(
            format!("{} {}", spin, active_t("footer.reviewing")),
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
            Some(tauqe_protocol::TurnPhase::Discovery) => {
                (active_t("status.discovery"), palette.status_info)
            }
            Some(tauqe_protocol::TurnPhase::Proposal) => {
                if state.model.status == TurnStatus::Thinking {
                    (active_t("status.thinking"), palette.text_secondary)
                } else {
                    (active_t("phase.proposal"), palette.status_warning)
                }
            }
            Some(tauqe_protocol::TurnPhase::Staging) => {
                (active_t("status.staging"), palette.button_active_bg)
            }
            Some(tauqe_protocol::TurnPhase::Verification) => {
                (active_t("status.verifying"), palette.status_info)
            }
            Some(tauqe_protocol::TurnPhase::Healing) => {
                (active_t("status.healing"), palette.status_error)
            }
            None => match state.model.status {
                TurnStatus::Thinking => (active_t("status.thinking"), palette.text_secondary),
                TurnStatus::Editing => (active_t("status.editing"), palette.button_active_bg),
                TurnStatus::Verifying => (active_t("status.verifying"), palette.status_info),
                TurnStatus::Responding => (active_t("status.responding"), palette.status_warning),
                TurnStatus::Awaiting => (active_t("status.awaiting"), palette.status_warning),
                other => (other.label(), palette.status_warning),
            },
        };

        footer_spans.push(Span::styled(
            format!("{} {}", spin, status_name),
            engine.fg(status_color).bold(),
        ));

        // A reconnect pause is in flight: the daemon publishes the remaining
        // seconds, so the footer mirrors that countdown and names the chord
        // that aborts the pause instead of waiting it out (c-4).
        if let Some(retrying) = &state.model.retrying {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                active_t("footer.retrying")
                    .replacen("{}", &retrying.attempt.to_string(), 1)
                    .replacen("{}", &retrying.max_attempts.to_string(), 1)
                    .replacen("{}", &retrying.delay_secs.to_string(), 1),
                engine.fg(palette.status_warning).bold(),
            ));
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                format!("({})", active_t("footer.retrying_cancel")),
                engine.fg(palette.status_warning),
            ));
        }

        // A graceful batch stop is in flight: the current member finishes and
        // the queue halts before the next one, so the developer sees exactly
        // which stop is pending.
        if state.batch_stopping {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(
                active_t("footer.stopping"),
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
            TurnStatus::Done => (
                active_t("footer.done"),
                engine.fg(palette.status_success).bold(),
            ),
            TurnStatus::Cancelled => (
                active_t("footer.cancelled"),
                engine.fg(palette.status_error).bold(),
            ),
            TurnStatus::Error => (
                active_t("footer.error"),
                engine.fg(palette.status_error).bold(),
            ),
            _ => (active_t("footer.ready"), engine.fg(palette.status_success)),
        };
        footer_spans.push(Span::styled(status_lbl, status_style));
    }

    let inner_width = area.width.saturating_sub(2) as usize;

    // The repository map cost is telemetry, not a turn indicator: it lives in
    // the usage dialog (Alt+U), so the status line never spends columns on it
    // and stays free for turn phases and the active file (c-4).
    let mut right_spans: Vec<Span<'static>> = Vec::new();

    if state.view_mode == ViewMode::History {
        if !right_spans.is_empty() {
            right_spans.push(Span::styled(" │ ", engine.fg(palette.text_muted)));
        }
        let tokens = state.history_view.get_estimated_tokens();
        let total = state.history_view.total_count;
        let hist_stats = active_t("footer.history_stats")
            .replacen("{}", &total.to_string(), 1)
            .replacen("{}", &tokens.to_string(), 1);
        right_spans.push(Span::styled(
            hist_stats,
            engine.fg(palette.status_info).bold(),
        ));
    }

    if !right_spans.is_empty() {
        let left_len: usize = footer_spans.iter().map(|s| s.width()).sum();
        let right_len: usize = right_spans.iter().map(|s| s.width()).sum();
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
        (tauqe_protocol::TurnPhase::Discovery, "phase.discovery"),
        (tauqe_protocol::TurnPhase::Proposal, "phase.proposal"),
        (tauqe_protocol::TurnPhase::Staging, "phase.staging"),
        (
            tauqe_protocol::TurnPhase::Verification,
            "phase.verification",
        ),
        (tauqe_protocol::TurnPhase::Healing, "phase.healing"),
    ];

    let mut spans = Vec::new();
    spans.push(Span::styled("[", engine.fg(palette.text_muted)));
    for (idx, (phase, key)) in phases.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("→", engine.fg(palette.text_muted)));
        }
        let label = active_t(key);
        if *phase == current {
            spans.push(Span::styled(label, engine.fg(palette.status_info).bold()));
        } else {
            spans.push(Span::styled(label, engine.fg(palette.text_muted)));
        }
    }
    spans.push(Span::styled("]", engine.fg(palette.text_muted)));
    spans
}

/// Renders every tier of the compact usage summary, richest first.
///
/// Money outranks tokens: as soon as any slot carries a known cost the footer
/// shows currency, otherwise the token counts stand in under an explicit `tok`
/// unit. The list runs from the three labelled slots down to the session total
/// alone, so a caller picks the richest variant that fits instead of truncating
/// a number mid-digit (c-4, c-5).
pub fn footer_usage_variants(snapshot: &UsageTotalsSnapshot) -> Vec<String> {
    let cur = snapshot.cur.as_ref();
    let has_money = snapshot.session.cost > 0.0
        || snapshot.prev.cost > 0.0
        || cur.is_some_and(|totals| totals.cost > 0.0);
    let has_tokens = snapshot.session.total_tokens > 0
        || snapshot.prev.total_tokens > 0
        || cur.is_some_and(|totals| totals.total_tokens > 0);

    if has_money {
        usage_variants(
            format_money(snapshot.session.cost),
            format_money(snapshot.prev.cost),
            cur.map(|totals| format_money(totals.cost)),
            None,
        )
    } else if has_tokens {
        usage_variants(
            format_tokens(snapshot.session.total_tokens),
            format_tokens(snapshot.prev.total_tokens),
            cur.map(|totals| format_tokens(totals.total_tokens)),
            Some(active_t("footer.usage.tok")),
        )
    } else {
        Vec::new()
    }
}

/// Selects the richest usage summary that fits `budget` terminal columns.
///
/// Returns `None` when the snapshot carries neither money nor tokens, and when
/// even the session-only form does not fit: the footer then drops the field
/// entirely rather than showing a severed value (c-4).
pub fn format_footer_usage(snapshot: &UsageTotalsSnapshot, budget: usize) -> Option<String> {
    footer_usage_variants(snapshot)
        .into_iter()
        .find(|variant| UnicodeWidthStr::width(variant.as_str()) <= budget)
}

/// Builds the three degradation tiers of one usage summary.
fn usage_variants(
    session: String,
    prev: String,
    cur: Option<String>,
    unit: Option<String>,
) -> Vec<String> {
    let with_unit = |value: &String| match &unit {
        Some(unit) => format!("{} {}", value, unit),
        None => value.clone(),
    };
    let session = with_unit(&session);
    let prev = with_unit(&prev);
    let cur = cur.as_ref().map(with_unit);

    let mut full = format!("Σ {} {}", session, active_t("footer.usage.session"));
    full.push_str(&format!(" · {} {}", prev, active_t("footer.usage.last")));
    if let Some(cur) = &cur {
        full.push_str(&format!(" · {} {}", cur, active_t("footer.usage.now")));
    }

    let mut compact = format!("Σ {} · {}", session, prev);
    if let Some(cur) = &cur {
        compact.push_str(&format!(" · {}", cur));
    }

    vec![full, compact, format!("Σ {}", session)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::model::UsageTotals;

    fn money_snapshot(cur_cost: Option<f64>) -> UsageTotalsSnapshot {
        UsageTotalsSnapshot {
            session: UsageTotals {
                cost: 0.015,
                ..UsageTotals::default()
            },
            prev: UsageTotals {
                cost: 0.005,
                ..UsageTotals::default()
            },
            cur: cur_cost.map(|cost| UsageTotals {
                cost,
                ..UsageTotals::default()
            }),
            operations: 2,
            ..UsageTotalsSnapshot::default()
        }
    }

    fn token_snapshot() -> UsageTotalsSnapshot {
        UsageTotalsSnapshot {
            session: UsageTotals {
                total_tokens: 1200,
                ..UsageTotals::default()
            },
            prev: UsageTotals {
                total_tokens: 300,
                ..UsageTotals::default()
            },
            cur: Some(UsageTotals {
                total_tokens: 50,
                ..UsageTotals::default()
            }),
            operations: 2,
            ..UsageTotalsSnapshot::default()
        }
    }

    #[test]
    fn footer_usage_prefers_money_and_labels_every_slot() {
        let variants = footer_usage_variants(&money_snapshot(Some(0.002)));
        assert_eq!(
            variants,
            vec![
                "Σ $0.0150 session · $0.0050 last · $0.0020 now".to_string(),
                "Σ $0.0150 · $0.0050 · $0.0020".to_string(),
                "Σ $0.0150".to_string(),
            ]
        );
    }

    #[test]
    fn footer_usage_drops_the_in_flight_slot_when_no_operation_runs() {
        let variants = footer_usage_variants(&money_snapshot(None));
        assert_eq!(variants[0], "Σ $0.0150 session · $0.0050 last");
        assert_eq!(variants[1], "Σ $0.0150 · $0.0050");
    }

    #[test]
    fn footer_usage_falls_back_to_tokens_when_no_cost_is_known() {
        let variants = footer_usage_variants(&token_snapshot());
        assert_eq!(
            variants[0],
            "Σ 1200 tok session · 300 tok last · 50 tok now"
        );
        assert_eq!(variants[1], "Σ 1200 tok · 300 tok · 50 tok");
        assert_eq!(variants[2], "Σ 1200 tok");
    }

    #[test]
    fn footer_usage_is_absent_without_money_or_tokens() {
        let empty = UsageTotalsSnapshot::default();
        assert!(footer_usage_variants(&empty).is_empty());
        assert_eq!(format_footer_usage(&empty, 200), None);
    }

    #[test]
    fn footer_usage_degrades_by_budget_without_cutting_numbers() {
        let snapshot = money_snapshot(Some(0.002));
        assert_eq!(
            format_footer_usage(&snapshot, 46).as_deref(),
            Some("Σ $0.0150 session · $0.0050 last · $0.0020 now")
        );
        assert_eq!(
            format_footer_usage(&snapshot, 45).as_deref(),
            Some("Σ $0.0150 · $0.0050 · $0.0020")
        );
        assert_eq!(
            format_footer_usage(&snapshot, 9).as_deref(),
            Some("Σ $0.0150")
        );
        assert_eq!(format_footer_usage(&snapshot, 8), None);
    }

    #[test]
    fn busy_footer_renders_the_round_badge_from_typed_state() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(140, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.status = TurnStatus::Responding;
        state.model.turn_round = Some(2);
        state.model.turn_max_rounds = Some(5);

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            content.contains("[Round 2/5]"),
            "the typed round counter must drive the badge: {content:?}"
        );
    }

    #[test]
    fn busy_footer_never_scrapes_a_round_badge_from_answer_text() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(140, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.status = TurnStatus::Responding;
        state.model.turn_round = None;
        state.model.turn_max_rounds = None;
        state.model.turn_phase_detail = None;
        state.model.text = "Quoted log follows:\n[Round 9] unrelated prose".to_string();

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            !content.contains("Round 9"),
            "model prose must never fabricate a round badge: {content:?}"
        );
    }

    #[test]
    fn footer_never_renders_the_repository_map_cost() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(140, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.repomap_tokens = Some(1234);

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        // The map cost is telemetry confined to the usage dialog (c-4) ...
        assert!(
            !content.contains("1234") && !content.contains("map:"),
            "the footer must not spend columns on the map cost: {content:?}"
        );
        // ... while the regular status line stays intact.
        assert!(content.contains("Ready"), "{content:?}");
    }

    #[test]
    fn test_format_notification() {
        let palette = crate::theme::presets::ThemePreset::TauqeDark.palette();
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
    fn busy_footer_shows_the_reconnect_countdown() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(140, 3);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.model.status = TurnStatus::Awaiting;
        state.model.retrying = Some(crate::views::repl::RetryingState {
            attempt: 2,
            max_attempts: 4,
            delay_secs: 3,
        });

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_footer(frame, &state, area);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(
            content.contains("Reconnecting 2/4 in 3s"),
            "the countdown must name the attempt and the remaining seconds: {content:?}"
        );
        assert!(
            content.contains("C-g to cancel"),
            "the indicator must name the chord that aborts the pause: {content:?}"
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
}
