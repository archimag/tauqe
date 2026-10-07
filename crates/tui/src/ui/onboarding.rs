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
    if len <= 14 {
        key.to_string()
    } else {
        let chars: Vec<char> = key.chars().collect();
        let prefix: String = chars[..8.min(len)].iter().collect();
        let suffix: String = chars[len.saturating_sub(4)..].iter().collect();
        format!("{}...{} ({} chars)", prefix, suffix, len)
    }
}

pub fn render_onboarding_view(
    frame: &mut ratatui::Frame,
    state: &AppState,
    main_area: Rect,
    info_area: Rect,
    footer_area: Rect,
) {
    let ob = &state.onboarding;

    let step_badge = match ob.step {
        crate::app::OnboardingStep::Git => Line::from(vec![
            Span::styled(" [0. Git Repository] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
            Span::styled(" ─── 1. Configuration ─── 2. API Key ─── 3. Ready", Style::default().fg(Color::DarkGray)),
        ]),
        crate::app::OnboardingStep::Config => Line::from(vec![
            Span::styled(" 0. Git [OK] ─── ", Style::default().fg(Color::Green)),
            Span::styled("[1. Configuration]", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::styled(" ─── 2. API Key ─── 3. Ready", Style::default().fg(Color::DarkGray)),
        ]),
        crate::app::OnboardingStep::Credentials => Line::from(vec![
            Span::styled(" 0. Git [OK] ─── 1. Configuration ─── ", Style::default().fg(Color::Green)),
            Span::styled("[2. API Key]", Style::default().bg(Color::Yellow).fg(Color::Black).bold()),
            Span::styled(" ─── 3. Ready", Style::default().fg(Color::DarkGray)),
        ]),
        crate::app::OnboardingStep::Gatekeeper => Line::from(vec![
            Span::styled(" 0. Git [OK] ─── 1. Configuration ─── 2. API Key ─── ", Style::default().fg(Color::DarkGray)),
            Span::styled("[Warning: API Key Missing]", Style::default().bg(Color::Red).fg(Color::White).bold()),
        ]),
        crate::app::OnboardingStep::Ready => Line::from(vec![
            Span::styled(" 0. Git [OK] ─── 1. Configuration ─── 2. API Key ─── ", Style::default().fg(Color::Green)),
            Span::styled("[3. Ready to Start]", Style::default().bg(Color::Green).fg(Color::Black).bold()),
        ]),
    };

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
                "Select default model for the project (arrows ↑/↓, then Enter):",
                Style::default().bold().fg(Color::White),
            )));
            lines.push(Line::raw(""));

            let options = [
                format!("Create tauqe.toml with model {} (Recommended)", ob.models_list[0]),
                format!("Create tauqe.toml with model {}", ob.models_list[1]),
                format!("Create tauqe.toml with model {}", ob.models_list[2]),
                format!("Create tauqe.toml with model {}", ob.models_list[3]),
                "Enter custom model identifier manually...".to_string(),
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
                "System is successfully configured and ready to use!",
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
        let (prompt, masked) = match ob.step {
            crate::app::OnboardingStep::Config => ("Enter model identifier: ", false),
            _ => ("Enter OpenRouter API Key: ", true),
        };

        let displayed = if masked {
            if ob.show_key {
                if ob.input_buffer.is_empty() {
                    String::new()
                } else {
                    format!("{} ({} chars)", ob.input_buffer, ob.input_buffer.chars().count())
                }
            } else {
                format_masked_key(&ob.input_buffer)
            }
        } else {
            ob.input_buffer.clone()
        };

        let hint = if masked {
            if ob.show_key {
                "  (Ctrl+R: Hide, Ctrl+V: Paste, Enter: Save, Esc: Cancel)"
            } else {
                "  (Ctrl+R: Show, Ctrl+V: Paste, Enter: Save, Esc: Cancel)"
            }
        } else {
            "  (Enter: Save, Esc: Cancel)"
        };

        lines.push(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow).bold()),
            Span::styled(displayed, Style::default().fg(Color::White).bold()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
        ]));
    }

    if let Some(err) = &ob.error_message {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("Error: {}", err),
            Style::default().fg(Color::Red).bold(),
        )));
    } else if let Some(msg) = &ob.status_message {
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
        crate::app::OnboardingStep::Config => "Use ↑/↓ arrow keys to select a model and Enter to confirm.",
        crate::app::OnboardingStep::Credentials => "Secure credential storage: created with 0600 file permissions (owner-only read/write).",
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
        assert_eq!(format_masked_key("short-key"), "short-key");
        assert_eq!(
            format_masked_key("sk-or-v1-0123456789abcdef0123456789abcdef"),
            "sk-or-v1...cdef (41 chars)"
        );
    }
}
