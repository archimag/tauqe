use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::AppState;
use crate::i18n::active_t;

use super::{format_masked_key, ONBOARDING_STEPS};

/// Fills the single `{}` placeholder of a localized template with `value`.
fn fill_one(template: &str, value: &str) -> String {
    template.replacen("{}", value, 1)
}

pub fn render_onboarding_step_badge(current_step: crate::app::OnboardingStep) -> Line<'static> {
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    if current_step == crate::app::OnboardingStep::Gatekeeper {
        // The warning badge replaces the credentials step instead of inventing a
        // parallel pipeline, so the chain always mirrors `ONBOARDING_STEPS` and
        // stays localized as one unit (Conventions §9).
        let mut spans = Vec::new();
        for (idx, meta) in ONBOARDING_STEPS.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled(" ─── ", engine.fg(palette.text_muted)));
            }
            if meta.step == crate::app::OnboardingStep::Credentials {
                spans.push(Span::styled(
                    format!(" {} ", active_t("onboarding.gatekeeper_badge")),
                    engine
                        .bg(palette.status_error)
                        .fg(engine.color(palette.text_inverse))
                        .bold(),
                ));
            } else if meta.step == crate::app::OnboardingStep::Git
                || meta.step == crate::app::OnboardingStep::Config
            {
                spans.push(Span::styled(
                    format!(
                        "{}{} [OK]",
                        if idx == 0 { " " } else { "" },
                        active_t(meta.name_key)
                    ),
                    engine.fg(palette.status_success),
                ));
            } else {
                spans.push(Span::styled(
                    active_t(meta.name_key),
                    engine.fg(palette.text_muted),
                ));
            }
        }
        return Line::from(spans);
    }

    let current_idx = ONBOARDING_STEPS
        .iter()
        .position(|meta| meta.step == current_step)
        .unwrap_or(0);

    let mut spans = Vec::new();
    for (idx, meta) in ONBOARDING_STEPS.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" ─── ", engine.fg(palette.text_muted)));
        }
        if idx < current_idx {
            spans.push(Span::styled(
                format!(
                    "{}{} [OK]",
                    if idx == 0 { " " } else { "" },
                    active_t(meta.name_key)
                ),
                engine.fg(palette.status_success),
            ));
        } else if idx == current_idx {
            spans.push(Span::styled(
                format!(" [{}] ", active_t(meta.name_key)),
                meta.accent.badge_style(&engine).bold(),
            ));
        } else {
            spans.push(Span::styled(
                active_t(meta.name_key),
                engine.fg(palette.text_muted),
            ));
        }
    }
    Line::from(spans)
}

pub fn render_onboarding_view(
    frame: &mut ratatui::Frame,
    state: &AppState,
    main_area: Rect,
    info_area: Rect,
    footer_area: Rect,
) {
    let ob = &state.onboarding;
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();

    let step_badge = render_onboarding_step_badge(ob.step);

    let mut lines = Vec::new();
    lines.push(Line::raw(""));
    lines.push(step_badge);
    lines.push(Line::raw(""));

    match ob.step {
        crate::app::OnboardingStep::Git => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.git_title"),
                engine.fg(palette.status_error).bold(),
            )));
            lines.push(Line::from(active_t("onboarding.git.body_not_repo")));
            lines.push(Line::from(active_t("onboarding.git.body_requires")));
            lines.push(Line::from(active_t("onboarding.git.body_rollback")));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.git.select_prompt"),
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                active_t("onboarding.git.option_init"),
                active_t("onboarding.git.option_init_commit"),
                active_t("onboarding.git.option_exit"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.status_error).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Config => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.config_title"),
                engine.fg(palette.status_info).bold(),
            )));
            lines.push(Line::from(active_t("onboarding.config.body_not_found")));
            lines.push(Line::from(fill_one(
                &active_t("onboarding.config.body_will_create"),
                &ob.default_config_path.to_string(),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.config.select_prompt"),
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                active_t("onboarding.config.option_create"),
                active_t("onboarding.config.option_skip"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.status_info).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Credentials => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.credentials_title"),
                engine.fg(palette.status_warning).bold(),
            )));
            lines.push(Line::from(active_t(
                "onboarding.credentials.body_not_detected",
            )));
            lines.push(Line::from(active_t("onboarding.credentials.body_required")));
            lines.push(Line::from(fill_one(
                &active_t("onboarding.credentials.body_location"),
                &ob.default_credentials_path.to_string(),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.credentials.select_prompt"),
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                fill_one(
                    &active_t("onboarding.credentials.option_enter"),
                    &ob.default_credentials_path.to_string(),
                ),
                active_t("onboarding.credentials.option_stub"),
                active_t("onboarding.credentials.option_env"),
                active_t("onboarding.credentials.option_skip"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.status_warning).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Workstation => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.workstation_title"),
                engine.fg(palette.button_active_bg).bold(),
            )));
            lines.push(Line::from(active_t(
                "onboarding.workstation.body_configure",
            )));
            lines.push(Line::from(fill_one(
                &active_t("onboarding.workstation.body_file"),
                &ob.default_tui_config_path.to_string(),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.workstation.select_prompt"),
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                active_t("onboarding.workstation.option_standard"),
                active_t("onboarding.workstation.option_jcuken"),
                active_t("onboarding.workstation.option_custom"),
                active_t("onboarding.workstation.option_skip"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.button_active_bg).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Modifier => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.modifier_title"),
                engine.fg(palette.overlay).bold(),
            )));
            lines.push(Line::from(active_t("onboarding.modifier.body_input")));
            lines.push(Line::from(active_t("onboarding.modifier.body_commands")));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.modifier.select_prompt"),
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                active_t("onboarding.modifier.option_ctrl"),
                active_t("onboarding.modifier.option_alt"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.overlay).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Gatekeeper => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.gatekeeper_title"),
                engine.fg(palette.status_error).bold(),
            )));
            lines.push(Line::from(active_t("onboarding.gatekeeper.body_no_key")));
            lines.push(Line::from(active_t("onboarding.gatekeeper.body_options")));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.gatekeeper.body_export"),
                engine.fg(palette.status_info).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                active_t("onboarding.gatekeeper.option_go_back"),
                active_t("onboarding.gatekeeper.option_recheck"),
                active_t("onboarding.gatekeeper.option_exit"),
            ];

            for (idx, opt) in options.iter().enumerate() {
                let is_sel = idx == ob.selected_index;
                let prefix = if is_sel { "  ▶ [●] " } else { "    [ ] " };
                let style = if is_sel {
                    engine
                        .bg(palette.overlay)
                        .fg(engine.color(palette.text_primary))
                        .bold()
                } else {
                    engine.fg(palette.text_secondary)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, engine.fg(palette.status_error).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Ready => {
            lines.push(Line::from(Span::styled(
                active_t("onboarding.step.ready_title"),
                engine.fg(palette.status_success).bold(),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("• {} ", active_t("onboarding.ready.model_label")),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(
                    state.active_model.to_string(),
                    engine.fg(palette.status_info),
                ),
            ]));

            let config_value = ob
                .config_path
                .clone()
                .unwrap_or_else(|| active_t("onboarding.ready.builtin_defaults"));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("• {} ", active_t("onboarding.ready.config_label")),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(config_value, engine.fg(palette.text_primary)),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("• {} ", active_t("onboarding.ready.key_label")),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(
                    active_t("onboarding.ready.key_configured"),
                    engine.fg(palette.status_success).bold(),
                ),
            ]));

            let modifier_value = match state.tui_config.input.primary_modifier {
                crate::config::PrimaryModifier::Ctrl => active_t("onboarding.ready.modifier_ctrl"),
                crate::config::PrimaryModifier::Alt => active_t("onboarding.ready.modifier_alt"),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("• {} ", active_t("onboarding.ready.modifier_label")),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled(modifier_value, engine.fg(palette.status_info).bold()),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(active_t("onboarding.ready.press_enter")));
            lines.push(Line::from(Span::styled(
                active_t("onboarding.ready.help_hint"),
                engine.fg(palette.text_muted),
            )));
            lines.push(Line::raw(""));

            let prefix = "  ▶ [●] ";
            let style = engine
                .bg(palette.status_success)
                .fg(engine.color(palette.text_inverse))
                .bold();
            lines.push(Line::from(vec![
                Span::styled(prefix, engine.fg(palette.status_success).bold()),
                Span::styled(
                    format!(" {} ", active_t("onboarding.ready.start_session")),
                    style,
                ),
            ]));
        }
    }

    if ob.input_active {
        lines.push(Line::raw(""));
        if ob.input_langmap {
            let prompt = active_t("onboarding.input.langmap_prompt");
            lines.push(Line::from(vec![
                Span::styled(prompt, engine.fg(palette.button_active_bg).bold()),
                Span::styled(
                    ob.input_buffer.get_text(),
                    engine.fg(palette.text_primary).bold(),
                ),
                Span::styled("█", engine.fg(palette.button_active_bg)),
                Span::styled(
                    active_t("onboarding.input.langmap_hint"),
                    engine.fg(palette.text_muted),
                ),
            ]));
        } else {
            let prompt = active_t("onboarding.input.api_key_prompt");

            let displayed = if ob.show_key {
                if ob.input_buffer.is_empty() {
                    String::new()
                } else {
                    format!(
                        "{} ({} chars)",
                        ob.input_buffer.get_text(),
                        ob.input_buffer.get_text().chars().count()
                    )
                }
            } else {
                format_masked_key(ob.input_buffer.get_text())
            };

            let prefix = state.tui_config.input.primary_modifier.prefix_str();
            let hint_key = if ob.show_key {
                "onboarding.input.key_hint_show"
            } else {
                "onboarding.input.key_hint_hide"
            };
            let hint = active_t(hint_key)
                .replacen("{}", prefix, 1)
                .replacen("{}", prefix, 1);

            lines.push(Line::from(vec![
                Span::styled(prompt, engine.fg(palette.status_warning).bold()),
                Span::styled(displayed, engine.fg(palette.text_primary).bold()),
                Span::styled("█", engine.fg(palette.status_warning)),
                Span::styled(hint, engine.fg(palette.text_muted)),
            ]));
        }
    }

    if let Some(err) = &ob.error_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            fill_one(&active_t("onboarding.error_prefix"), err),
            engine.fg(palette.status_error).bold(),
        )));
    }
    if let Some(msg) = &ob.status_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            fill_one(&active_t("onboarding.status_prefix"), msg),
            engine.fg(palette.status_success).bold(),
        )));
    }

    let main_block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!(" {} ", active_t("onboarding.title")))
                .borders(Borders::ALL)
                .border_style(engine.fg(palette.status_info)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(main_block, main_area);

    let info_text = match ob.step {
        crate::app::OnboardingStep::Git => active_t("onboarding.info.git"),
        crate::app::OnboardingStep::Config => active_t("onboarding.info.config"),
        crate::app::OnboardingStep::Credentials => active_t("onboarding.info.credentials"),
        crate::app::OnboardingStep::Workstation => active_t("onboarding.info.workstation"),
        crate::app::OnboardingStep::Modifier => active_t("onboarding.info.modifier"),
        crate::app::OnboardingStep::Gatekeeper => active_t("onboarding.info.gatekeeper"),
        crate::app::OnboardingStep::Ready => active_t("onboarding.info.ready"),
    };
    let info_block = Paragraph::new(Line::from(Span::styled(
        info_text,
        engine.fg(palette.text_muted),
    )))
    .block(
        Block::default()
            .title(format!(" {} ", active_t("onboarding.info_title")))
            .borders(Borders::ALL),
    );
    frame.render_widget(info_block, info_area);

    // The footer is reserved for operational telemetry (Conventions §1.2, §3.2):
    // onboarding keybindings are documented by the Help modal (`ONBOARDING_COMMANDS`),
    // never by a persistent hint bar. Reuse the shared telemetry footer so the
    // onboarding chrome stays theme-correct and the reload chord follows the
    // configured primary modifier instead of a hardcoded `Ctrl+O` (Conventions §5.2).
    crate::ui::render_footer(frame, state, footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_onboarding_step_badge_consistency() {
        assert_eq!(ONBOARDING_STEPS.len(), 6);

        let badge_git = render_onboarding_step_badge(crate::app::OnboardingStep::Git);
        let text_git = badge_git.to_string();
        assert!(text_git.contains("[0. Git]"));
        assert!(text_git.contains("1. Configuration"));
        assert!(text_git.contains("5. Ready"));

        let badge_ready = render_onboarding_step_badge(crate::app::OnboardingStep::Ready);
        let text_ready = badge_ready.to_string();
        assert!(text_ready.contains("0. Git [OK]"));
        assert!(text_ready.contains("4. Modifier [OK]"));
        assert!(text_ready.contains("[5. Ready]"));

        let badge_gatekeeper = render_onboarding_step_badge(crate::app::OnboardingStep::Gatekeeper);
        let text_gatekeeper = badge_gatekeeper.to_string();
        assert!(text_gatekeeper.contains("Warning: API Key Missing"));
        assert!(text_gatekeeper.contains("0. Git [OK]"));
        assert!(text_gatekeeper.contains("5. Ready"));
    }

    #[test]
    fn onboarding_footer_shows_telemetry_not_keyboard_hints() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::{Constraint, Direction, Layout};
        use ratatui::Terminal;

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal created");
        let mut state = crate::app::test_state();
        state.view_mode = crate::app::ViewMode::Onboarding;

        terminal
            .draw(|frame| {
                let area = frame.area();
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(5),
                        Constraint::Length(3),
                        Constraint::Length(3),
                    ])
                    .split(area);
                render_onboarding_view(frame, &state, chunks[1], chunks[2], chunks[3]);
            })
            .expect("draw succeeded");

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        // §3.2: a primary view never renders a persistent shortcut hint bar.
        assert!(
            !content.contains("Reload Config") && !content.contains("Select Item"),
            "onboarding footer must not render shortcut hints: {content:?}"
        );
        // The shared telemetry footer identifies the environment instead.
        assert!(
            content.contains("TAUQE"),
            "onboarding footer must render the shared telemetry footer: {content:?}"
        );
    }
}
