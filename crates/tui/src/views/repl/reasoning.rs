use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::theme::engine::current_engine;

use super::render::indicator_style;

/// Number of trailing reasoning lines kept visible while a reasoning stream is
/// active. The live stream acts as a progress indicator; the full transcript is
/// rendered once via [`ReasoningState::finalize`].
const REASONING_TAIL_LINES: usize = 10;

#[derive(Debug, Clone)]
pub struct ReasoningState {
    pub text: String,
    pub lines: Vec<Line<'static>>,
    pub show: bool,
    pub theme_mode: crate::config::ThemeMode,
    pub max_width: Option<usize>,
    streaming: bool,
    tail: Vec<Line<'static>>,
    pending: String,
    newline_count: usize,
    tail_consumed: usize,
}

impl Default for ReasoningState {
    fn default() -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            show: true,
            theme_mode: crate::config::ThemeMode::default(),
            max_width: None,
            streaming: false,
            tail: Vec::new(),
            pending: String::new(),
            newline_count: 0,
            tail_consumed: 0,
        }
    }
}

impl ReasoningState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.lines.clear();
        self.show = true;
        self.streaming = false;
        self.tail.clear();
        self.pending.clear();
        self.newline_count = 0;
        self.tail_consumed = 0;
    }

    /// Appends a streamed reasoning delta.
    ///
    /// While the stream is active only a lightweight tail window is maintained;
    /// the heavy Markdown render is deferred to [`Self::finalize`].
    pub fn append_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.streaming = true;
        self.fold_tail();
    }

    /// Finalizes an active reasoning stream by performing the full Markdown
    /// render exactly once. No-op when the stream has already settled.
    pub fn finalize(&mut self) {
        if !self.streaming {
            return;
        }
        self.streaming = false;
        self.show = false;
        self.update_markdown();
    }

    pub fn update_markdown(&mut self) {
        let mut theme = crate::markdown::MarkdownTheme::reasoning_themed(self.theme_mode);
        theme.max_width = self.max_width;
        self.lines = crate::markdown::render_markdown(&self.text, &theme);
    }

    /// Folds newly appended text into the rolling tail window.
    ///
    /// Text is consumed starting at [`Self::tail_consumed`], which tolerates
    /// deltas inserted directly into [`Self::text`] by event handlers.
    fn fold_tail(&mut self) {
        if self.tail_consumed >= self.text.len() {
            return;
        }
        let segment = self
            .text
            .get(self.tail_consumed..)
            .unwrap_or(self.text.as_str())
            .to_string();
        self.tail_consumed = self.text.len();

        let mut rest = segment.as_str();
        while let Some(idx) = rest.find('\n') {
            let (head, after) = rest.split_at(idx);
            self.pending.push_str(head);
            self.commit_pending_line();
            rest = &after[1..];
        }
        self.pending.push_str(rest);
    }

    fn commit_pending_line(&mut self) {
        let line = std::mem::take(&mut self.pending);
        self.tail.push(Self::tail_line(&line));
        if self.tail.len() > REASONING_TAIL_LINES {
            self.tail.remove(0);
        }
        self.newline_count += 1;
    }

    fn tail_line(line: &str) -> Line<'static> {
        let engine = current_engine();
        Line::from(Span::styled(
            line.to_string(),
            engine.fg(engine.palette().text_reasoning),
        ))
    }

    fn total_lines(&self) -> usize {
        self.newline_count + usize::from(!self.pending.is_empty())
    }

    /// True while a reasoning stream is active and not yet finalized.
    ///
    /// Text channels use this to settle and fold the reasoning block exactly
    /// once when the answer begins, so the lifecycle stays identical everywhere.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn toggle_fold(&mut self) {
        self.show = !self.show;
    }

    pub fn render_lines(&self) -> Vec<Line<'static>> {
        if self.text.is_empty() {
            return Vec::new();
        }

        let engine = current_engine();
        let palette = engine.palette();

        let mut lines = Vec::new();
        let reasoning_badge = Span::styled(
            " [REASONING] ",
            indicator_style(&engine, palette.button_active_bg),
        );

        if !self.show {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    "▶ Thinking hidden",
                    engine.fg(palette.text_muted).bold(),
                ),
            ]));
            return lines;
        }

        if self.streaming {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled(
                    format!("Thinking… ({} lines)", self.total_lines()),
                    engine.fg(palette.status_info).bold(),
                ),
            ]));

            lines.extend(self.tail.iter().cloned());
            if !self.pending.is_empty() {
                lines.push(Self::tail_line(&self.pending));
            }
        } else {
            lines.push(Line::from(vec![
                reasoning_badge,
                Span::raw(" "),
                Span::styled("Thinking", engine.fg(palette.status_info).bold()),
            ]));

            if !self.lines.is_empty() {
                lines.extend(self.lines.iter().cloned());
            } else {
                let text_style = engine.fg(palette.text_reasoning);
                for line in self.text.lines() {
                    lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
        }

        lines
    }
}
