use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::AppState;
use crate::i18n::active_t;

use super::{format_masked_key, ONBOARDING_STEPS};

pub fn render_onboarding_step_badge(current_step: crate::app::OnboardingStep) -> Line<'static> {
    let engine = crate::theme::engine::current_engine();
    let palette = engine.palette();
    if current_step == crate::app::OnboardingStep::Gatekeeper {
        return Line::from(vec![
            Span::styled(
                " 0. Git [OK] ─── 1. Configuration [OK] ─── ",
                engine.fg(palette.status_success),
            ),
            Span::styled(
                "[Warning: API Key Missing]",
                engine
                    .bg(palette.status_error)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
            ),
            Span::styled(
                " ─── 3. Layout ─── 4. Modifier ─── 5. Ready",
                engine.fg(palette.text_muted),
            ),
        ]);
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
                format!("{}{} [OK]", if idx == 0 { " " } else { "" }, meta.name),
                engine.fg(palette.status_success),
            ));
        } else if idx == current_idx {
            spans.push(Span::styled(
                format!(" [{}] ", meta.name),
                meta.accent.badge_style(&engine).bold(),
            ));
        } else {
            spans.push(Span::styled(meta.name, engine.fg(palette.text_muted)));
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
            lines.push(Line::from("The current working directory is not a Git repository."));
            lines.push(Line::from("Tauqe strictly requires Git: isolated pre-edit checkpoints, atomic turn commits,"));
            lines.push(Line::from("instant rollback (undo), and feature commit squashing."));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select an action (arrows ↑/↓, then Enter):",
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                "Initialize Git repository (git init)".to_string(),
                "Initialize and create initial commit (git init && git commit)".to_string(),
                "Exit Tauqe".to_string(),
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
            lines.push(Line::from("Configuration file not found in project root."));
            lines.push(Line::from(format!(
                "It will be created at: {}",
                ob.default_config_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select configuration option (arrows ↑/↓, then Enter):",
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                "Create default tauqe.toml (recommended configuration)".to_string(),
                "Skip (use built-in defaults without creating tauqe.toml)".to_string(),
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
            lines.push(Line::from("OpenRouter API key was not detected in environment (OPENROUTER_API_KEY) or credentials files."));
            lines.push(Line::from("An API key is required to query models (obtain at https://openrouter.ai/keys)."));
            lines.push(Line::from(format!(
                "Recommended secure file location: {}",
                ob.default_credentials_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select key setup method (arrows ↑/↓, then Enter):",
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                format!("Enter API key now (save with 0600 permissions to {})", ob.default_credentials_path),
                "Create stub credentials.toml file (fill manually in editor)".to_string(),
                "Configured via OPENROUTER_API_KEY environment variable (Check again)".to_string(),
                "Skip".to_string(),
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
            lines.push(Line::from("Configure shortcut key translation and navigation keys for your system layout."));
            lines.push(Line::from(format!(
                "Configuration file: {}",
                ob.default_tui_config_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select keyboard layout option (arrows ↑/↓, then Enter):",
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                "Standard (No translation, default QWERTY / layout-neutral)".to_string(),
                "Russian JCUKEN (Translate Cyrillic ЙЦУКЕН to QWERTY for shortcuts & navigation)".to_string(),
                "Custom Langmap (Input custom character mapping: e.g. Greek, Hebrew, etc.)".to_string(),
                "Skip (Use defaults without creating tui.toml)".to_string(),
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
            lines.push(Line::from("In text input mode (prompt buffer, commit message editor), every regular key types text."));
            lines.push(Line::from("Functional commands (send prompt, undo, squash, model picker, tabs) use the primary modifier."));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select primary command modifier (arrows ↑/↓, then Enter):",
                engine.fg(palette.text_primary).bold(),
            )));
            lines.push(Line::raw(""));

            let options = [
                "Ctrl (Default — standard Emacs C- shortcuts: Ctrl+Enter, Ctrl+Z, Ctrl+S, Ctrl+M)".to_string(),
                "Alt (Meta modifier — Alt+Enter, Alt+Z, Alt+S, Alt+M; robust on macOS and legacy terminals)".to_string(),
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
            lines.push(Line::from("Language model calls cannot proceed without an OpenRouter API key."));
            lines.push(Line::from("You can input the key now, create credentials.toml, or export:"));
            lines.push(Line::from(Span::styled("  export OPENROUTER_API_KEY=\"sk-or-v1-...\"", engine.fg(palette.status_info).bold())));
            lines.push(Line::raw(""));

            let options = [
                "Go back and configure OpenRouter API key".to_string(),
                "Check again (reload environment and credentials files)".to_string(),
                "Exit Tauqe".to_string(),
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
                Span::styled("• Model: ", engine.fg(palette.text_primary).bold()),
                Span::styled(state.active_model.to_string(), engine.fg(palette.status_info)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Configuration: ", engine.fg(palette.text_primary).bold()),
                Span::styled(
                    ob.config_path.as_deref().unwrap_or("Built-in defaults"),
                    engine.fg(palette.text_primary),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• OpenRouter Key: ", engine.fg(palette.text_primary).bold()),
                Span::styled("[OK] Configured", engine.fg(palette.status_success).bold()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Command Modifier: ", engine.fg(palette.text_primary).bold()),
                Span::styled(
                    match state.tui_config.input.primary_modifier {
                        crate::config::PrimaryModifier::Ctrl => "Ctrl (Default)",
                        crate::config::PrimaryModifier::Alt => "Alt",
                    },
                    engine.fg(palette.status_info).bold(),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from("Press Enter to open the development workspace."));
            lines.push(Line::from(Span::styled(
                "Press '?' at any time to view the hotkey reference.",
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
                Span::styled(" Start Session (Open Terminal) ", style),
            ]));
        }
    }

    if ob.input_active {
        lines.push(Line::raw(""));
        if ob.input_langmap {
            let prompt = "Enter Custom Langmap (e.g. 'йцукен;qwerty' or 'йq,цw'): ";
            lines.push(Line::from(vec![
                Span::styled(prompt, engine.fg(palette.button_active_bg).bold()),
                Span::styled(ob.input_buffer.get_text(), engine.fg(palette.text_primary).bold()),
                Span::styled("█", engine.fg(palette.button_active_bg)),
                Span::styled("  (Enter: Save, Esc: Cancel)", engine.fg(palette.text_muted)),
            ]));
        } else {
            let prompt = "Enter OpenRouter API Key: ";

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
            let hint = if ob.show_key {
                format!(
                    "  ({}R: Hide, {}V: Paste, Enter: Save, Esc: Cancel)",
                    prefix, prefix
                )
            } else {
                format!(
                    "  ({}R: Show, {}V: Paste, Enter: Save, Esc: Cancel)",
                    prefix, prefix
                )
            };

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
            format!("Error: {}", err),
            engine.fg(palette.status_error).bold(),
        )));
    }
    if let Some(msg) = &ob.status_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("✓ {}", msg),
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
        crate::app::OnboardingStep::Git => "Tauqe relies on Git for checkpoints, atomic step commits, and safe undo.",
        crate::app::OnboardingStep::Config => "Create default tauqe.toml or skip to use built-in defaults.",
        crate::app::OnboardingStep::Credentials => "Secure credential storage: created with 0600 file permissions (owner-only read/write).",
        crate::app::OnboardingStep::Workstation => "Workstation configuration: personal settings stored in ~/.config/tauqe/tui.toml.",
        crate::app::OnboardingStep::Modifier => "Primary command modifier for text input: choose Ctrl (default) or Alt for legacy/macOS terminals.",
        crate::app::OnboardingStep::Gatekeeper => "OpenRouter API key is required for LLM calls. Select an action.",
        crate::app::OnboardingStep::Ready => "All parameters verified. Press Enter to start.",
    };
    let info_block = Paragraph::new(Line::from(Span::styled(info_text, engine.fg(palette.text_muted))))
        .block(Block::default().title(" Information ").borders(Borders::ALL));
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
