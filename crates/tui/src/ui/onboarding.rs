use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::AppState;

pub fn format_masked_key(key: &str) -> String {
    let len = key.chars().count();
    if len == 0 {
        return String::new();
    }
    let bullets = "•".repeat(len.min(16));
    format!("{} ({} chars)", bullets, len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingStepMeta {
    pub step: crate::app::OnboardingStep,
    pub name: &'static str,
    pub active_bg: Color,
    pub active_fg: Color,
}

pub const ONBOARDING_STEPS: &[OnboardingStepMeta] = &[
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Git,
        name: "0. Git",
        active_bg: Color::Red,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Config,
        name: "1. Configuration",
        active_bg: Color::Cyan,
        active_fg: Color::Black,
    },
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Credentials,
        name: "2. API Key",
        active_bg: Color::Yellow,
        active_fg: Color::Black,
    },
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Workstation,
        name: "3. Layout",
        active_bg: Color::Magenta,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Modifier,
        name: "4. Modifier",
        active_bg: Color::Blue,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: crate::app::OnboardingStep::Ready,
        name: "5. Ready",
        active_bg: Color::Green,
        active_fg: Color::Black,
    },
];

pub fn render_onboarding_step_badge(current_step: crate::app::OnboardingStep) -> Line<'static> {
    if current_step == crate::app::OnboardingStep::Gatekeeper {
        return Line::from(vec![
            Span::styled(" 0. Git [OK] ─── 1. Configuration [OK] ─── ", Style::default().fg(Color::Green)),
            Span::styled("[Warning: API Key Missing]", Style::default().bg(Color::Red).fg(Color::White).bold()),
            Span::styled(" ─── 3. Layout ─── 4. Modifier ─── 5. Ready", Style::default().fg(Color::DarkGray)),
        ]);
    }

    let current_idx = ONBOARDING_STEPS
        .iter()
        .position(|meta| meta.step == current_step)
        .unwrap_or(0);

    let mut spans = Vec::new();
    for (idx, meta) in ONBOARDING_STEPS.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(" ─── ", Style::default().fg(Color::DarkGray)));
        }
        if idx < current_idx {
            spans.push(Span::styled(
                format!("{}{} [OK]", if idx == 0 { " " } else { "" }, meta.name),
                Style::default().fg(Color::Green),
            ));
        } else if idx == current_idx {
            spans.push(Span::styled(
                format!(" [{}] ", meta.name),
                Style::default().bg(meta.active_bg).fg(meta.active_fg).bold(),
            ));
        } else {
            spans.push(Span::styled(
                meta.name,
                Style::default().fg(Color::DarkGray),
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

    let step_badge = render_onboarding_step_badge(ob.step);

    let mut lines = Vec::new();
    lines.push(Line::raw(""));
    lines.push(step_badge);
    lines.push(Line::raw(""));

    match ob.step {
        crate::app::OnboardingStep::Git => {
            lines.push(Line::from(Span::styled(
                "Step 0: Initialize Git Repository",
                Style::default().bold().fg(Color::Red),
            )));
            lines.push(Line::from("The current working directory is not a Git repository."));
            lines.push(Line::from("Tauqe strictly requires Git: isolated pre-edit checkpoints, atomic turn commits,"));
            lines.push(Line::from("instant rollback (undo), and feature commit squashing."));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select an action (arrows ↑/↓, then Enter):",
                Style::default().bold().fg(Color::White),
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Config => {
            lines.push(Line::from(Span::styled(
                "Step 1: Project Configuration (tauqe.toml)",
                Style::default().bold().fg(Color::Cyan),
            )));
            lines.push(Line::from("Configuration file not found in project root."));
            lines.push(Line::from(format!(
                "It will be created at: {}",
                ob.default_config_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select configuration option (arrows ↑/↓, then Enter):",
                Style::default().bold().fg(Color::White),
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Credentials => {
            lines.push(Line::from(Span::styled(
                "Step 2: Credentials and OpenRouter API Key",
                Style::default().bold().fg(Color::Yellow),
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
                Style::default().bold().fg(Color::White),
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Yellow).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Workstation => {
            lines.push(Line::from(Span::styled(
                "Step 3: Workstation Keyboard Layout and Mapping (tui.toml)",
                Style::default().bold().fg(Color::Magenta),
            )));
            lines.push(Line::from("Configure shortcut key translation and navigation keys for your system layout."));
            lines.push(Line::from(format!(
                "Configuration file: {}",
                ob.default_tui_config_path
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select keyboard layout option (arrows ↑/↓, then Enter):",
                Style::default().bold().fg(Color::White),
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Magenta).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Modifier => {
            lines.push(Line::from(Span::styled(
                "Step 4: Primary Command Modifier (C-)",
                Style::default().bold().fg(Color::Blue),
            )));
            lines.push(Line::from("In text input mode (prompt buffer, commit message editor), every regular key types text."));
            lines.push(Line::from("Functional commands (send prompt, undo, squash, model picker, tabs) use the primary modifier."));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Select primary command modifier (arrows ↑/↓, then Enter):",
                Style::default().bold().fg(Color::White),
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Blue).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Gatekeeper => {
            lines.push(Line::from(Span::styled(
                "Warning: System Not Ready",
                Style::default().bold().fg(Color::Red),
            )));
            lines.push(Line::from("Language model calls cannot proceed without an OpenRouter API key."));
            lines.push(Line::from("You can input the key now, create credentials.toml, or export:"));
            lines.push(Line::from(Span::styled("  export OPENROUTER_API_KEY=\"sk-or-v1-...\"", Style::default().fg(Color::Cyan).bold())));
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
                    Style::default().bg(Color::DarkGray).fg(Color::White).bold()
                } else {
                    Style::default().fg(Color::Gray)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Red).bold()),
                    Span::styled(opt.clone(), style),
                ]));
            }
        }
        crate::app::OnboardingStep::Ready => {
            lines.push(Line::from(Span::styled(
                "Step 5: System is successfully configured and ready to use!",
                Style::default().bold().fg(Color::Green),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("• Model: ", Style::default().bold()),
                Span::styled(state.active_model.to_string(), Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Configuration: ", Style::default().bold()),
                Span::styled(
                    ob.config_path.as_deref().unwrap_or("Built-in defaults"),
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• OpenRouter Key: ", Style::default().bold()),
                Span::styled("[OK] Configured", Style::default().fg(Color::Green).bold()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("• Command Modifier: ", Style::default().bold()),
                Span::styled(
                    match state.tui_config.input.primary_modifier {
                        crate::config::PrimaryModifier::Ctrl => "Ctrl (Default)",
                        crate::config::PrimaryModifier::Alt => "Alt",
                    },
                    Style::default().fg(Color::Blue).bold(),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from("Press Enter to open the development workspace."));
            lines.push(Line::from(Span::styled(
                "Press '?' at any time to view the hotkey reference.",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::raw(""));

            let prefix = "  ▶ [●] ";
            let style = Style::default().bg(Color::Green).fg(Color::Black).bold();
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green).bold()),
                Span::styled(" Start Session (Open Terminal) ", style),
            ]));
        }
    }

    if ob.input_active {
        lines.push(Line::raw(""));
        if ob.input_langmap {
            let prompt = "Enter Custom Langmap (e.g. 'йцукен;qwerty' or 'йq,цw'): ";
            lines.push(Line::from(vec![
                Span::styled(prompt, Style::default().fg(Color::Magenta).bold()),
                Span::styled(&ob.input_buffer, Style::default().fg(Color::White).bold()),
                Span::styled("█", Style::default().fg(Color::Magenta)),
                Span::styled("  (Enter: Save, Esc: Cancel)", Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            let prompt = "Enter OpenRouter API Key: ";

            let displayed = if ob.show_key {
                if ob.input_buffer.is_empty() {
                    String::new()
                } else {
                    format!("{} ({} chars)", ob.input_buffer, ob.input_buffer.chars().count())
                }
            } else {
                format_masked_key(&ob.input_buffer)
            };

            let hint = if ob.show_key {
                "  (Ctrl+R: Hide, Ctrl+V: Paste, Enter: Save, Esc: Cancel)"
            } else {
                "  (Ctrl+R: Show, Ctrl+V: Paste, Enter: Save, Esc: Cancel)"
            };

            lines.push(Line::from(vec![
                Span::styled(prompt, Style::default().fg(Color::Yellow).bold()),
                Span::styled(displayed, Style::default().fg(Color::White).bold()),
                Span::styled("█", Style::default().fg(Color::Yellow)),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    if let Some(err) = &ob.error_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("Error: {}", err),
            Style::default().fg(Color::Red).bold(),
        )));
    }
    if let Some(msg) = &ob.status_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("✓ {}", msg),
            Style::default().fg(Color::Green).bold(),
        )));
    }

    let main_block = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Tauqe Initial Setup (Onboarding) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
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
    let info_block = Paragraph::new(Line::from(Span::styled(info_text, Style::default().fg(Color::DarkGray))))
        .block(Block::default().title(" Information ").borders(Borders::ALL));
    frame.render_widget(info_block, info_area);

    let footer_line = Line::from(vec![
        Span::styled(
            " ↑ / ↓ (k / j) ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Select Item  "),
        Span::styled(
            " Enter ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Confirm  "),
        Span::styled(
            " Esc / q ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Back / Exit  "),
        Span::styled(
            " Ctrl+O ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        ),
        Span::raw(" Reload Config"),
    ]);
    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_masked_key() {
        assert_eq!(format_masked_key(""), "");
        assert_eq!(format_masked_key("short-key"), "••••••••• (9 chars)");
        assert_eq!(
            format_masked_key("sk-or-v1-0123456789abcdef0123456789abcdef"),
            "•••••••••••••••• (41 chars)"
        );
    }

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
}
