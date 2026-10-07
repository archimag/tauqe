use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{ModelResult, ModelUsageEvent};

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone)]
pub struct ModelCodeBlock {
    pub id: usize,
    pub lang: String,
    pub code: String,
    pub raw_start_line: usize,
    pub raw_end_line: usize,
    pub visual_start_line: usize,
    pub visual_end_line: usize,
}

#[derive(Debug, Clone)]
pub struct StreamingHunk {
    pub hunk_index: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone)]
pub struct StreamingFileEdit {
    pub path: String,
    pub op_type: String, // "replace", "create", "delete", "retrying"
    pub status: String,  // "running", "ok", "error", "retrying"
    pub error: Option<String>,
    pub hunks: Vec<StreamingHunk>,
    pub expanded: bool,
    pub retry_info: Option<String>,
}

pub struct ModelView {
    pub operation_id: Option<String>,
    pub model: Option<String>,
    pub reasoning: String,
    pub text: String,
    pub markdown_lines: Vec<Line<'static>>,
    pub reasoning_lines: Vec<Line<'static>>,
    pub usage: Option<ModelUsageEvent>,
    pub prev_cost: Option<f64>,
    pub current_cost: Option<f64>,
    pub session_total_cost: f64,
    pub round_usage_received: bool,
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
    pub toolchain_command: Option<String>,
    pub toolchain_status: Option<String>,
    pub spinner_frame: usize,
    pub rendered_lines_count: usize,
    pub copy_flash: Option<(usize, std::time::Instant)>,
    pub copy_notification: Option<(String, std::time::Instant)>,
    pub code_blocks: Vec<ModelCodeBlock>,
    pub content_rect: (u16, u16, u16, u16),
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
            prev_cost: None,
            current_cost: None,
            session_total_cost: 0.0,
            round_usage_received: false,
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
            toolchain_command: None,
            toolchain_status: None,
            spinner_frame: 0,
            rendered_lines_count: 0,
            copy_flash: None,
            copy_notification: None,
            code_blocks: Vec::new(),
            content_rect: (0, 0, 0, 0),
        }
    }
}

impl ModelView {
    pub fn start_new_round_if_needed(&mut self) {
        if self.round_usage_received {
            self.prev_cost = self.current_cost;
            self.current_cost = Some(0.0);
            self.round_usage_received = false;
        }
    }

    pub fn update_markdown(&mut self) {
        let flashing_id = self.copy_flash.as_ref().map(|(id, _)| *id);
        let (lines, blocks) = crate::markdown::render_markdown_with_blocks(
            &self.text,
            &crate::markdown::MarkdownTheme::answer(),
            flashing_id,
        );
        self.markdown_lines = lines;
        self.code_blocks = blocks
            .into_iter()
            .map(|b| ModelCodeBlock {
                id: b.id,
                lang: b.lang,
                code: b.content,
                raw_start_line: b.start_line,
                raw_end_line: b.end_line,
                visual_start_line: b.start_line,
                visual_end_line: b.end_line,
            })
            .collect();
    }

    pub fn update_reasoning_markdown(&mut self) {
        self.reasoning_lines = crate::markdown::render_markdown(
            &self.reasoning,
            &crate::markdown::MarkdownTheme::reasoning(),
        );
    }

    pub fn max_scroll(&self, view_height: u16) -> u16 {
        let count = if self.rendered_lines_count > 0 {
            self.rendered_lines_count as u16
        } else {
            compute_model_lines(self).len() as u16
        };
        count.saturating_sub(view_height)
    }

    pub fn clamp_scroll(&mut self, view_height: u16) {
        let max = self.max_scroll(view_height);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    pub fn expanded_file_lines_count(&self, file: &StreamingFileEdit) -> usize {
        let mut count = 0;
        for hunk in &file.hunks {
            if file.op_type == "create" {
                count += 1 + hunk.new_text.lines().count();
            } else if file.op_type == "delete" {
                count += 1 + hunk.old_text.lines().count();
            } else {
                let diff = similar::TextDiff::from_lines(&hunk.old_text, &hunk.new_text);
                count += 1;
                for op in diff.ops() {
                    count += diff.iter_changes(op).count();
                }
            }
        }
        count
    }

    pub fn file_line_offset(&self, target_idx: usize) -> usize {
        let mut line_idx = 0;
        if self.error.is_some() {
            line_idx += 1;
        }
        if self.git_notification.is_some() {
            line_idx += 1;
        }
        if self.toolchain_status.is_some() {
            line_idx += 1;
        }

        if !self.markdown_lines.is_empty() {
            line_idx += self.markdown_lines.len();
        } else if !self.text.is_empty() {
            line_idx += self.text.lines().count();
        }

        if !self.reasoning.is_empty() {
            if !self.text.is_empty() || line_idx > 0 {
                line_idx += 1;
            }
            line_idx += 1;
            if self.show_reasoning {
                if !self.reasoning_lines.is_empty() {
                    line_idx += self.reasoning_lines.len();
                } else {
                    line_idx += self.reasoning.lines().count();
                }
            }
        }

        if self.edits_active || !self.files.is_empty() {
            line_idx += 1;
            line_idx += 1;
            if self.last_commit_summary.is_some() {
                line_idx += 1;
            }

            for (idx, file) in self.files.iter().enumerate() {
                if idx == target_idx {
                    return line_idx;
                }
                line_idx += 1;
                if file.status != "ok" && file.error.is_some() {
                    line_idx += 1;
                }
                if file.expanded {
                    line_idx += self.expanded_file_lines_count(file);
                }
            }
        }
        line_idx
    }

    pub fn scroll_to_selected_file(&mut self, view_height: u16) {
        if self.files.is_empty() || view_height == 0 {
            return;
        }
        let target_line = self.file_line_offset(self.selected_file_index) as u16;
        let is_expanded = self
            .files
            .get(self.selected_file_index)
            .map(|f| f.expanded)
            .unwrap_or(false);

        if target_line < self.scroll {
            self.auto_scroll = false;
            self.scroll = target_line;
        } else if is_expanded {
            if target_line + 4 >= self.scroll + view_height {
                self.auto_scroll = false;
                self.scroll = target_line.saturating_sub(1);
            }
        } else if target_line >= self.scroll + view_height {
            self.auto_scroll = false;
            self.scroll = (target_line + 1).saturating_sub(view_height);
        }
        self.clamp_scroll(view_height);
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.status.as_str(),
            "awaiting"
                | "thinking"
                | "responding"
                | "editing"
                | "verifying"
                | "starting"
                | "streaming"
        )
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

    if let Some(notif) = &model.git_notification {
        model_lines.push(Line::from(vec![
            Span::styled(
                " [GIT] ",
                Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            ),
            Span::raw(" "),
            Span::styled(notif.clone(), Style::default().fg(Color::Cyan).bold()),
        ]));
    }

    if let Some(toolchain_info) = &model.toolchain_status {
        let is_running = model.toolchain_command.is_some();
        let badge = if is_running {
            let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
            Span::styled(
                format!(" [{}] TOOLCHAIN ", spin),
                Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
            )
        } else if toolchain_info.contains("passed") {
            Span::styled(
                " [TOOLCHAIN: OK] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            )
        } else {
            Span::styled(
                " [TOOLCHAIN: FAILED] ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            )
        };

        model_lines.push(Line::from(vec![
            badge,
            Span::raw(" "),
            Span::styled(
                toolchain_info.clone(),
                Style::default().bold().fg(Color::White),
            ),
        ]));
    }

    // 1. Model response text is always displayed at the top
    if !model.markdown_lines.is_empty() {
        model_lines.extend(model.markdown_lines.iter().cloned());
    } else if !model.text.is_empty() {
        for line in model.text.lines() {
            model_lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    // 2. Reasoning stream is displayed below the response text
    if !model.reasoning.is_empty() {
        if !model.text.is_empty() || !model_lines.is_empty() {
            model_lines.push(Line::raw(""));
        }

        let reasoning_badge = Span::styled(
            " [REASONING] ",
            Style::default().bg(Color::Rgb(60, 90, 140)).fg(Color::White).bold(),
        );

        if model.show_reasoning {
            model_lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    "Thinking (Ctrl+R to fold)",
                    Style::default().fg(Color::Rgb(130, 170, 220)).bold(),
                ),
            ]));

            if !model.reasoning_lines.is_empty() {
                model_lines.extend(model.reasoning_lines.iter().cloned());
            } else {
                let text_style = Style::default()
                    .fg(Color::Rgb(130, 165, 210))
                    .add_modifier(ratatui::style::Modifier::DIM);
                for line in model.reasoning.lines() {
                    model_lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
        } else {
            model_lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    "▶ Thinking hidden (Ctrl+R to expand)",
                    Style::default().fg(Color::DarkGray).bold(),
                ),
            ]));
        }
    }

    if model.edits_active || !model.files.is_empty() {
        model_lines.push(Line::raw(""));

        let total_files = model.files.len();
        let ok_files = model.files.iter().filter(|f| f.status == "ok").count();

        let header_badge = match model.edit_final_applied {
            Some(true) => {
                if let Some(hash) = &model.last_commit_hash {
                    Span::styled(
                        format!(" [COMMITTED: {}] ", hash),
                        Style::default().bg(Color::Green).fg(Color::Black).bold(),
                    )
                } else {
                    Span::styled(
                        " [APPLIED] ",
                        Style::default().bg(Color::Green).fg(Color::Black).bold(),
                    )
                }
            }
            Some(false) => Span::styled(
                " [REJECTED] ",
                Style::default().bg(Color::Red).fg(Color::White).bold(),
            ),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(
                    format!(" [{}] Modifying ", spin),
                    Style::default().bg(Color::Yellow).fg(Color::Black).bold(),
                )
            }
        };

        let mut header_spans = vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!(
                    "Proposed Edits ({}/{} files) - [ / ] Navigate, Space/Enter to Fold/Unfold",
                    ok_files, total_files
                ),
                Style::default().bold().fg(Color::Cyan),
            ),
        ];

        if model.last_commit_hash.is_some() {
            header_spans.push(Span::raw(" | "));
            header_spans.push(Span::styled(
                "Press 'u' to Undo AI commit",
                Style::default().fg(Color::Yellow),
            ));
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
            let cursor_prefix = if is_selected { "> " } else { "  " };

            let is_retrying = file.status == "retrying" || (file.status != "ok" && file.retry_info.is_some());
            let (status_icon, status_style) = if file.status == "ok" {
                ("✓", Style::default().fg(Color::Green).bold())
            } else if is_retrying {
                (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    Style::default().fg(Color::Yellow).bold(),
                )
            } else if file.status == "error" {
                ("✗", Style::default().fg(Color::Red).bold())
            } else {
                (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    Style::default().fg(Color::Yellow).bold(),
                )
            };

            let (op_label, op_style) = match file.op_type.as_str() {
                "create" => ("[NEW] ", Style::default().fg(Color::Green).bold()),
                "delete" => ("[DEL] ", Style::default().fg(Color::Red).bold()),
                "move" => ("[MOVE] ", Style::default().fg(Color::Cyan).bold()),
                _ => ("[EDIT] ", Style::default().fg(Color::Magenta).bold()),
            };

            let hunk_count = file.hunks.len();
            let hunk_label = if file.op_type == "move" {
                "renamed".to_string()
            } else if file.op_type == "delete" {
                "deleted".to_string()
            } else if file.op_type == "create" {
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

            let mut file_spans = vec![
                Span::raw("  "),
                Span::styled(cursor_prefix, Style::default().fg(Color::Cyan)),
                Span::styled(fold_icon, Style::default().fg(Color::DarkGray)),
                Span::styled(op_label, op_style),
                Span::styled(file.path.clone(), Style::default().bold()),
                Span::raw(" "),
                Span::styled(
                    format!("({})", hunk_label),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(status_icon, status_style),
            ];

            if file.status != "ok" {
                if let Some(retry) = &file.retry_info {
                    file_spans.push(Span::raw(" "));
                    file_spans.push(Span::styled(
                        format!("(Retrying {})", retry),
                        Style::default().fg(Color::Yellow).bold(),
                    ));
                }
            }

            model_lines.push(Line::from(file_spans).style(file_line_style));

            if file.status != "ok" {
                if let Some(err_msg) = &file.error {
                    model_lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled("Validation Error: ", Style::default().fg(Color::Red).bold()),
                        Span::styled(err_msg.clone(), Style::default().fg(Color::Yellow)),
                    ]));
                }
            }

            if file.expanded {
                for hunk in &file.hunks {
                    if file.op_type == "create" {
                        model_lines.push(Line::from(vec![
                            Span::styled(
                                "  @@ new file @@",
                                Style::default().fg(Color::Green).bold(),
                            ),
                        ]));
                        for line in hunk.new_text.lines() {
                            model_lines.push(Line::from(vec![
                                Span::styled("  + ", Style::default().fg(Color::Rgb(120, 240, 120)).bold()),
                                Span::styled(line.to_string(), Style::default().fg(Color::Rgb(140, 240, 140))),
                            ]));
                        }
                    } else if file.op_type == "delete" {
                        model_lines.push(Line::from(vec![
                            Span::styled(
                                "  @@ deleted file @@",
                                Style::default().fg(Color::Red).bold(),
                            ),
                        ]));
                        for line in hunk.old_text.lines() {
                            model_lines.push(Line::from(vec![
                                Span::styled("  - ", Style::default().fg(Color::Rgb(255, 120, 120)).bold()),
                                Span::styled(line.to_string(), Style::default().fg(Color::Rgb(255, 140, 140))),
                            ]));
                        }
                    } else {
                        let diff = similar::TextDiff::from_lines(&hunk.old_text, &hunk.new_text);
                        let header_text = if file.hunks.len() > 1 {
                            format!("  @@ hunk {} @@", hunk.hunk_index + 1)
                        } else {
                            "  @@ diff @@".to_string()
                        };
                        model_lines.push(Line::from(vec![
                            Span::styled(header_text, Style::default().fg(Color::Cyan).bold()),
                        ]));

                        for op in diff.ops() {
                            for change in diff.iter_changes(op) {
                                let line_str = change.value().trim_end_matches(['\r', '\n']);
                                match change.tag() {
                                    similar::ChangeTag::Delete => {
                                        model_lines.push(Line::from(vec![
                                            Span::styled("  - ", Style::default().fg(Color::Rgb(255, 120, 120)).bold()),
                                            Span::styled(line_str.to_string(), Style::default().fg(Color::Rgb(255, 140, 140))),
                                        ]));
                                    }
                                    similar::ChangeTag::Insert => {
                                        model_lines.push(Line::from(vec![
                                            Span::styled("  + ", Style::default().fg(Color::Rgb(120, 240, 120)).bold()),
                                            Span::styled(line_str.to_string(), Style::default().fg(Color::Rgb(140, 240, 140))),
                                        ]));
                                    }
                                    similar::ChangeTag::Equal => {
                                        model_lines.push(Line::from(vec![
                                            Span::styled("    ", Style::default().fg(Color::DarkGray)),
                                            Span::styled(line_str.to_string(), Style::default().fg(Color::Rgb(170, 175, 185))),
                                        ]));
                                    }
                                }
                            }
                        }
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
                    usage.usage.prompt_tokens, usage.usage.completion_tokens,
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
        usage_spans.push(Span::styled(
            "Operation cost: ",
            Style::default().fg(Color::Yellow).bold(),
        ));
        usage_spans.push(Span::styled(cost_str, cost_style));

        model_lines.push(Line::from(usage_spans));
    }

    model_lines
}
