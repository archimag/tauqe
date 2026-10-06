use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use workbench_protocol::{ModelResult, ModelUsageEvent};

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone)]
pub struct StreamingHunk {
    pub hunk_index: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct StreamingFileEdit {
    pub path: String,
    pub op_type: String, // "replace", "create", "delete"
    pub status: String,  // "running", "ok", "error"
    pub error: Option<String>,
    pub hunks: Vec<StreamingHunk>,
    pub expanded: bool,
}

pub struct ModelView {
    pub operation_id: Option<String>,
    pub model: Option<String>,
    pub reasoning: String,
    pub text: String,
    pub markdown_lines: Vec<Line<'static>>,
    pub reasoning_lines: Vec<Line<'static>>,
    pub usage: Option<ModelUsageEvent>,
    pub last_op_cost: Option<f64>,
    pub session_total_cost: f64,
    pub status: String,
    pub error: Option<String>,
    pub result: Option<ModelResult>,
    pub scroll: u16,
    pub show_reasoning: bool,
    pub auto_scroll: bool,

    // Structured Org-Mode Edits & Git State
    pub edits_active: bool,
    pub files: Vec<StreamingFileEdit>,
    pub selected_file_index: usize,
    pub edit_final_applied: Option<bool>,
    pub edit_final_error: Option<String>,
    pub last_commit_hash: Option<String>,
    pub last_commit_summary: Option<String>,
    pub git_notification: Option<String>,
    pub intent_notification: Option<String>,
    pub toolchain_command: Option<String>,
    pub toolchain_status: Option<String>,
    pub spinner_frame: usize,
}

impl Default for ModelView {
    fn default() -> Self {
        Self {
            operation_id: None,
            model: None,
            reasoning: String::new(),
            text: String::new(),
            markdown_lines: Vec::new(),
            reasoning_lines: Vec::new(),
            usage: None,
            last_op_cost: None,
            session_total_cost: 0.0,
            status: String::new(),
            error: None,
            result: None,
            scroll: 0,
            show_reasoning: true,
            auto_scroll: true,
            edits_active: false,
            files: Vec::new(),
            selected_file_index: 0,
            edit_final_applied: None,
            edit_final_error: None,
            last_commit_hash: None,
            last_commit_summary: None,
            git_notification: None,
            intent_notification: None,
            toolchain_command: None,
            toolchain_status: None,
            spinner_frame: 0,
        }
    }
}

impl ModelView {
    pub fn update_markdown(&mut self) {
        self.markdown_lines = crate::markdown::render_markdown(&self.text, &crate::markdown::MarkdownTheme::answer());
    }

    pub fn update_reasoning_markdown(&mut self) {
        self.reasoning_lines = crate::markdown::render_markdown(&self.reasoning, &crate::markdown::MarkdownTheme::reasoning());
    }

    pub fn max_scroll(&self, view_height: u16) -> u16 {
        let lines = compute_model_lines(self);
        let count = lines.len() as u16;
        count.saturating_sub(view_height)
    }

    pub fn clamp_scroll(&mut self, view_height: u16) {
        let max = self.max_scroll(view_height);
        if self.scroll > max {
            self.scroll = max;
        }
    }
}

pub fn compute_model_lines(model: &ModelView) -> Vec<Line<'static>> {
    let mut model_lines: Vec<Line<'static>> = Vec::new();



    if let Some(err) = &model.error {
        model_lines.push(Line::from(vec![
            Span::raw("Error: "),
            Span::styled(err.clone(), Style::default().fg(Color::Red)),
        ]));
    }

    if let Some(intent_notif) = &model.intent_notification {
        model_lines.push(Line::from(vec![
            Span::styled(" [INTENT MEMORY] ", Style::default().bg(Color::Blue).fg(Color::White).bold()),
            Span::raw(" "),
            Span::styled(intent_notif.clone(), Style::default().fg(Color::Cyan)),
        ]));
    }

    if let Some(notif) = &model.git_notification {
        model_lines.push(Line::from(vec![
            Span::styled(" [GIT] ", Style::default().bg(Color::Cyan).fg(Color::Black).bold()),
            Span::raw(" "),
            Span::styled(notif.clone(), Style::default().fg(Color::Cyan).bold()),
        ]));
    }

    if let Some(toolchain_info) = &model.toolchain_status {
        let is_running = model.toolchain_command.is_some();
        let badge = if is_running {
            let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
            Span::styled(format!(" [{}] TOOLCHAIN ", spin), Style::default().bg(Color::Cyan).fg(Color::Black).bold())
        } else if toolchain_info.contains("passed") {
            Span::styled(" [TOOLCHAIN: OK] ", Style::default().bg(Color::Green).fg(Color::Black).bold())
        } else {
            Span::styled(" [TOOLCHAIN: FAILED] ", Style::default().bg(Color::Red).fg(Color::White).bold())
        };

        model_lines.push(Line::from(vec![
            badge,
            Span::raw(" "),
            Span::styled(toolchain_info.clone(), Style::default().bold().fg(Color::White)),
        ]));
    }

    if !model.reasoning.is_empty() {
        if model.show_reasoning {
            model_lines.push(Line::from(Span::styled(
                "Thinking (Ctrl+R to hide):",
                Style::default()
                    .fg(Color::Rgb(100, 140, 190))
                    .add_modifier(ratatui::style::Modifier::DIM),
            )));
            if !model.reasoning_lines.is_empty() {
                model_lines.extend(model.reasoning_lines.iter().cloned());
            } else {
                let text_style = Style::default().fg(Color::Rgb(125, 160, 205)).add_modifier(ratatui::style::Modifier::DIM);
                for line in model.reasoning.lines() {
                    model_lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
        } else {
            model_lines.push(Line::from(Span::styled(
                "[+] Thinking hidden (Ctrl+R to show)",
                Style::default().bold().fg(Color::DarkGray),
            )));
        }
    }

    if !model.markdown_lines.is_empty() {
        model_lines.push(Line::raw(""));
        model_lines.extend(model.markdown_lines.iter().cloned());
    } else if !model.text.is_empty() {
        model_lines.push(Line::raw(""));
        for line in model.text.lines() {
            model_lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    if model.edits_active || !model.files.is_empty() {
        model_lines.push(Line::raw(""));

        let total_files = model.files.len();
        let ok_files = model.files.iter().filter(|f| f.status == "ok").count();

        let header_badge = match model.edit_final_applied {
            Some(true) => {
                if let Some(hash) = &model.last_commit_hash {
                    Span::styled(format!(" [COMMITTED: {}] ", hash), Style::default().bg(Color::Green).fg(Color::Black).bold())
                } else {
                    Span::styled(" [APPLIED] ", Style::default().bg(Color::Green).fg(Color::Black).bold())
                }
            }
            Some(false) => Span::styled(" [REJECTED] ", Style::default().bg(Color::Red).fg(Color::White).bold()),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(format!(" [{}] Modifying ", spin), Style::default().bg(Color::Yellow).fg(Color::Black).bold())
            }
        };

        let mut header_spans = vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!("Proposed Edits ({}/{} files) - [ / ] Navigate, Space/Enter to Fold/Unfold", ok_files, total_files),
                Style::default().bold().fg(Color::Cyan),
            ),
        ];

        if model.last_commit_hash.is_some() {
            header_spans.push(Span::raw(" | "));
            header_spans.push(Span::styled("Press 'u' to Undo AI commit", Style::default().fg(Color::Yellow)));
        }

        model_lines.push(Line::from(header_spans));

        if let Some(summary) = &model.last_commit_summary {
            model_lines.push(Line::from(vec![
                Span::raw("  Summary: "),
                Span::styled(summary.clone(), Style::default().bold().fg(Color::White)),
            ]));
        }

        for (idx, file) in model.files.iter().enumerate() {
            let is_selected = idx == model.selected_file_index;
            let fold_icon = if file.expanded { "▼ " } else { "▶ " };
            let cursor_prefix = if is_selected { "● " } else { "  " };

            let (status_icon, status_style) = match file.status.as_str() {
                "ok" => ("✓", Style::default().fg(Color::Green).bold()),
                "error" => ("✗", Style::default().fg(Color::Red).bold()),
                _ => (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    Style::default().fg(Color::Yellow).bold(),
                ),
            };

            let (op_label, op_style) = match file.op_type.as_str() {
                "create" => ("[NEW] ", Style::default().fg(Color::Green).bold()),
                "delete" => ("[DEL] ", Style::default().fg(Color::Red).bold()),
                _ => ("[EDIT] ", Style::default().fg(Color::Magenta).bold()),
            };

            let hunk_count = file.hunks.len();
            let hunk_label = if file.op_type == "create" {
                let line_count = file
                    .hunks
                    .first()
                    .map(|h| h.new_text.lines().count())
                    .unwrap_or(0);
                if line_count == 1 {
                    "1 line".to_string()
                } else {
                    format!("{} lines", line_count)
                }
            } else if hunk_count == 1 {
                "1 hunk".to_string()
            } else {
                format!("{} hunks", hunk_count)
            };

            let file_line_style = if is_selected {
                Style::default().bg(Color::DarkGray).bold()
            } else {
                Style::default()
            };

            model_lines.push(
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)),
                    Span::styled(fold_icon, Style::default().fg(Color::DarkGray)),
                    Span::styled(op_label, op_style),
                    Span::styled(file.path.clone(), Style::default().bold()),
                    Span::raw(" "),
                    Span::styled(format!("({})", hunk_label), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(status_icon, status_style),
                ])
                .style(file_line_style),
            );

            if let Some(err_msg) = &file.error {
                model_lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled("Validation Error: ", Style::default().fg(Color::Red).bold()),
                    Span::styled(err_msg.clone(), Style::default().fg(Color::Yellow)),
                ]));
            }

            if file.expanded {
                for hunk in &file.hunks {
                    if file.op_type == "create" {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(
                                "@@ new file @@",
                                Style::default().fg(Color::Green).italic(),
                            ),
                        ]));
                    } else {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(
                                format!("@@ hunk {} @@", hunk.hunk_index + 1),
                                Style::default().fg(Color::DarkGray).italic(),
                            ),
                        ]));
                    }

                    for line in hunk.old_text.lines() {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(format!("- {}", line), Style::default().fg(Color::Red)),
                        ]));
                    }
                    for line in hunk.new_text.lines() {
                        model_lines.push(Line::from(vec![
                            Span::raw("      "),
                            Span::styled(format!("+ {}", line), Style::default().fg(Color::Green)),
                        ]));
                    }
                }
            }
        }
    }

    if let Some(usage) = &model.usage {
        model_lines.push(Line::raw(""));
        let cost_val = usage.usage.cost.unwrap_or(0.0);
        let cost_str = format!("${:.5}", cost_val);
        let cost_style = if cost_val > 0.0 {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let mut usage_spans = vec![
            Span::styled("Tokens: ", Style::default().fg(Color::DarkGray).bold()),
            Span::styled(
                format!(
                    "{} prompt, {} completion",
                    usage.usage.prompt_tokens,
                    usage.usage.completion_tokens,
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ];

        if let Some(reason_tokens) = usage.usage.reasoning_tokens {
            if reason_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} reasoning", reason_tokens),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }

        if let Some(cached_tokens) = usage.usage.cached_tokens {
            if cached_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} cached", cached_tokens),
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        usage_spans.push(Span::raw(" | "));
        usage_spans.push(Span::styled("Operation cost: ", Style::default().fg(Color::Yellow).bold()));
        usage_spans.push(Span::styled(cost_str, cost_style));

        model_lines.push(Line::from(usage_spans));
    }

    model_lines
}
