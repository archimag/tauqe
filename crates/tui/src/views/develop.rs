use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tauqe_protocol::{ModelRef, ModelResult, ModelUsageEvent};

use crate::theme::color::RgbColor;
use crate::theme::engine::{current_engine, ThemeEngine};

pub mod input;
pub mod render;

pub use self::input::{handle_develop_key, DEVELOP_COMMANDS};

/// Minimum interval between full markdown re-parses while a response is streaming.
const MARKDOWN_THROTTLE: Duration = Duration::from_millis(80);

/// Number of trailing reasoning lines kept visible while a reasoning stream is
/// active. The live stream acts as a progress indicator; the full transcript is
/// rendered once via [`ReasoningState::finalize`].
const REASONING_TAIL_LINES: usize = 10;

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Lifecycle status of a model turn as tracked by the Develop view.
///
/// Replacing free-form status strings with an enum lets the compiler enforce
/// exhaustiveness: a typo or a newly introduced phase can no longer silently
/// desynchronize the UI (for example, leaving it busy forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnStatus {
    /// No turn has run yet, or the view was reset.
    #[default]
    Idle,
    Awaiting,
    Thinking,
    Responding,
    Editing,
    Verifying,
    Discovery,
    Staging,
    Healing,
    Done,
    Cancelled,
    Error,
}

impl TurnStatus {
    /// True while a turn is in flight, so the UI must treat the view as busy.
    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Awaiting
                | Self::Thinking
                | Self::Responding
                | Self::Editing
                | Self::Verifying
                | Self::Discovery
                | Self::Staging
                | Self::Healing
        )
    }

    /// Short human-readable label for the footer indicator.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Ready",
            Self::Awaiting => "Awaiting",
            Self::Thinking => "Thinking",
            Self::Responding => "Responding",
            Self::Editing => "Editing",
            Self::Verifying => "Verifying",
            Self::Discovery => "Discovery",
            Self::Staging => "Staging",
            Self::Healing => "Healing",
            Self::Done => "Done",
            Self::Cancelled => "Cancelled",
            Self::Error => "Error",
        }
    }
}

/// Lifecycle status of a single streamed file edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEditStatus {
    Running,
    Retrying,
    Ok,
    Error,
}

impl From<tauqe_protocol::EditFileStatus> for FileEditStatus {
    /// Maps the strict wire status onto the view lifecycle status.
    fn from(status: tauqe_protocol::EditFileStatus) -> Self {
        match status {
            tauqe_protocol::EditFileStatus::Ok => Self::Ok,
            tauqe_protocol::EditFileStatus::Error => Self::Error,
        }
    }
}

/// Kind of file operation proposed by the model.
///
/// Aliased to the protocol's strict [`tauqe_protocol::EditFileOp`] so the view
/// and the wire contract share a single source of truth, and the compiler
/// enforces exhaustive handling of every operation.
pub type EditOp = tauqe_protocol::EditFileOp;

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
    pub op_type: EditOp,
    pub status: FileEditStatus,
    pub error: Option<String>,
    pub hunks: Vec<StreamingHunk>,
    pub expanded: bool,
    pub retry_info: Option<String>,
}

/// Cache of rendered diff lines for expanded file edits.
///
/// Computing a `similar::TextDiff` is comparatively expensive and was
/// previously repeated for every expanded file on every frame and navigation
/// key. Entries are keyed by file path and invalidated by a cheap fingerprint
/// of the file's hunks, so a changed hunk always re-renders.
#[derive(Default)]
pub struct DiffRenderCache {
    entries: HashMap<String, DiffCacheEntry>,
}

struct DiffCacheEntry {
    fingerprint: u64,
    lines: Vec<Line<'static>>,
}

impl DiffRenderCache {
    /// Returns the rendered diff body for `file`, recomputing it only when the
    /// cached fingerprint no longer matches the file's hunks.
    fn lines_for(&mut self, file: &StreamingFileEdit) -> Vec<Line<'static>> {
        let fingerprint = diff_fingerprint(file);
        if let Some(lines) = self.cached_lines(&file.path, fingerprint) {
            return lines;
        }
        let lines = render_expanded_diff_lines(file);
        self.entries.insert(
            file.path.clone(),
            DiffCacheEntry {
                fingerprint,
                lines: lines.clone(),
            },
        );
        lines
    }

    fn cached_lines(&self, path: &str, fingerprint: u64) -> Option<Vec<Line<'static>>> {
        let entry = self.entries.get(path)?;
        if entry.fingerprint == fingerprint {
            Some(entry.lines.clone())
        } else {
            None
        }
    }
}

/// Cheap content fingerprint of a file's hunks, used to invalidate the diff
/// render cache without recomputing the diff itself.
fn diff_fingerprint(file: &StreamingFileEdit) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    file.path.hash(&mut hasher);
    (file.op_type as u8).hash(&mut hasher);
    for hunk in &file.hunks {
        hunk.hunk_index.hash(&mut hasher);
        hunk.old_text.hash(&mut hasher);
        hunk.new_text.hash(&mut hasher);
    }
    hasher.finish()
}

/// Filled indicator style: a semantic background role with inverse text.
///
/// Toolchain, commit, and turn-status badges resolve both their surface and
/// their label from the active palette, so a badge never disappears on a
/// terminal with a non-standard palette (c-1).
fn indicator_style(engine: &ThemeEngine, bg: RgbColor) -> Style {
    engine
        .bg(bg)
        .fg(engine.color(engine.palette().text_inverse))
        .bold()
}

/// Renders the expanded diff body for a single file edit.
///
/// Extracted from the model-line builder so the result can be cached and its
/// length reused for scroll accounting without recomputing the diff.
fn render_expanded_diff_lines(file: &StreamingFileEdit) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for hunk in &file.hunks {
        if file.op_type == EditOp::Create {
            lines.push(Line::from(vec![Span::styled(
                "  @@ new file @@",
                engine.fg(palette.diff_added).bold(),
            )]));
            for line in hunk.new_text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  + ", engine.fg(palette.diff_added).bold()),
                    Span::styled(line.to_string(), engine.fg(palette.diff_added)),
                ]));
            }
        } else if file.op_type == EditOp::Delete {
            lines.push(Line::from(vec![Span::styled(
                "  @@ deleted file @@",
                engine.fg(palette.diff_removed).bold(),
            )]));
            for line in hunk.old_text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  - ", engine.fg(palette.diff_removed).bold()),
                    Span::styled(line.to_string(), engine.fg(palette.diff_removed)),
                ]));
            }
        } else {
            let diff = similar::TextDiff::from_lines(&hunk.old_text, &hunk.new_text);
            let header_text = if file.hunks.len() > 1 {
                format!("  @@ hunk {} @@", hunk.hunk_index + 1)
            } else {
                "  @@ diff @@".to_string()
            };
            lines.push(Line::from(vec![Span::styled(
                header_text,
                engine.fg(palette.status_info).bold(),
            )]));

            for op in diff.ops() {
                for change in diff.iter_changes(op) {
                    let line_str = change.value().trim_end_matches(['\r', '\n']);
                    match change.tag() {
                        similar::ChangeTag::Delete => {
                            lines.push(Line::from(vec![
                                Span::styled("  - ", engine.fg(palette.diff_removed).bold()),
                                Span::styled(
                                    line_str.to_string(),
                                    engine.fg(palette.diff_removed),
                                ),
                            ]));
                        }
                        similar::ChangeTag::Insert => {
                            lines.push(Line::from(vec![
                                Span::styled("  + ", engine.fg(palette.diff_added).bold()),
                                Span::styled(
                                    line_str.to_string(),
                                    engine.fg(palette.diff_added),
                                ),
                            ]));
                        }
                        similar::ChangeTag::Equal => {
                            lines.push(Line::from(vec![
                                Span::styled("    ", engine.fg(palette.diff_context)),
                                Span::styled(
                                    line_str.to_string(),
                                    engine.fg(palette.diff_context),
                                ),
                            ]));
                        }
                    }
                }
            }
        }
    }
    lines
}

/// Lifecycle of the deterministic toolchain verification step.
///
/// The state is tracked structurally so the status badge never depends on
/// parsing the human-readable report: a test summary such as
/// `1 failed, 12 passed` must never select the success badge.
#[derive(Debug, Clone)]
pub enum ToolchainState {
    /// The verification command is currently executing.
    Running { command: String },
    /// The verification command completed successfully.
    Passed { command: String },
    /// The verification command failed; `message` carries the diagnostics.
    Failed {
        command: String,
        message: Option<String>,
    },
}

impl ToolchainState {
    /// True while the verification command is still running.
    pub fn is_running(&self) -> bool {
        matches!(self, ToolchainState::Running { .. })
    }

    /// The command line, exposed only while the command is running.
    pub fn running_command(&self) -> Option<&str> {
        match self {
            ToolchainState::Running { command } => Some(command),
            _ => None,
        }
    }
}

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
            engine.fg(engine.palette().text_secondary),
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
                let text_style = engine.fg(palette.text_secondary);
                for line in self.text.lines() {
                    lines.push(Line::from(Span::styled(line.to_string(), text_style)));
                }
            }
        }

        lines
    }
}

/// Which pane of the Develop tab receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevelopFocus {
    #[default]
    Editor,
    Viewport,
}

pub struct DevelopView {
    pub operation_id: Option<String>,
    pub model: Option<ModelRef>,
    pub reasoning: ReasoningState,
    pub theme_mode: crate::config::ThemeMode,
    pub text: String,
    pub markdown_lines: Vec<Line<'static>>,
    pub usage: Option<ModelUsageEvent>,
    pub prev_cost: Option<f64>,
    pub current_cost: Option<f64>,
    pub session_total_cost: f64,
    pub status: TurnStatus,
    pub error: Option<String>,
    pub result: Option<ModelResult>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub focus: DevelopFocus,

    // Structured Org-Mode Edits & Git State
    pub edits_active: bool,
    pub files: Vec<StreamingFileEdit>,
    pub selected_file_index: usize,
    pub edit_final_applied: Option<bool>,
    pub edit_final_error: Option<String>,
    pub last_commit_hash: Option<String>,
    pub last_commit_summary: Option<String>,
    pub toolchain: Option<ToolchainState>,
    pub turn_phase: Option<tauqe_protocol::TurnPhase>,
    pub turn_round: Option<usize>,
    pub turn_max_rounds: Option<usize>,
    pub turn_phase_detail: Option<String>,
    pub plan_context: Option<tauqe_protocol::ModelTurnPlanContext>,
    pub is_batch: bool,
    pub repomap_tokens: Option<usize>,
    pub spinner_frame: usize,
    pub rendered_lines_count: usize,
    pub copy_flash: Option<(usize, std::time::Instant)>,
    pub code_blocks: Vec<ModelCodeBlock>,
    pub content_rect: (u16, u16, u16, u16),
    pub content_width: usize,
    /// Vertical scroll offset of the prompt editor, retained across frames so
    /// the cursor keeps its position instead of sticking to the bottom row.
    pub input_scroll: u16,
    /// Timestamp of the first `Esc` that armed clearing a non-empty prompt.
    /// A second `Esc` within the confirmation window clears the draft; this
    /// guards long drafts from accidental `ESC`-prefixed Alt sequences.
    pub(crate) esc_clear_armed: Option<Instant>,
    pub(crate) markdown_dirty: bool,
    pub(crate) last_markdown_render: Option<Instant>,
    pub diff_cache: DiffRenderCache,
}

impl Default for DevelopView {
    fn default() -> Self {
        Self {
            operation_id: None,
            model: None,
            reasoning: ReasoningState::default(),
            theme_mode: crate::config::ThemeMode::default(),
            text: String::new(),
            markdown_lines: Vec::new(),
            usage: None,
            prev_cost: None,
            current_cost: None,
            session_total_cost: 0.0,
            status: TurnStatus::default(),
            error: None,
            result: None,
            scroll: 0,
            auto_scroll: true,
            focus: DevelopFocus::Editor,
            edits_active: false,
            files: Vec::new(),
            selected_file_index: 0,
            edit_final_applied: None,
            edit_final_error: None,
            last_commit_hash: None,
            last_commit_summary: None,
            toolchain: None,
            turn_phase: None,
            turn_round: None,
            turn_max_rounds: None,
            turn_phase_detail: None,
            plan_context: None,
            is_batch: false,
            repomap_tokens: None,
            spinner_frame: 0,
            rendered_lines_count: 0,
            copy_flash: None,
            code_blocks: Vec::new(),
            content_rect: (0, 0, 0, 0),
            content_width: 0,
            input_scroll: 0,
            esc_clear_armed: None,
            markdown_dirty: false,
            last_markdown_render: None,
            diff_cache: DiffRenderCache::default(),
        }
    }
}

impl DevelopView {
    /// Re-renders the answer markdown; while streaming, repeated calls within the
    /// throttle interval only mark the view dirty and are flushed later.
    pub fn update_markdown(&mut self) {
        if self.is_busy()
            && self
                .last_markdown_render
                .is_some_and(|t| t.elapsed() < MARKDOWN_THROTTLE)
        {
            self.markdown_dirty = true;
            return;
        }
        self.render_markdown_now();
    }

    pub fn set_theme_mode(&mut self, mode: crate::config::ThemeMode) {
        let changed = self.theme_mode != mode || self.reasoning.theme_mode != mode;
        self.theme_mode = mode;
        self.reasoning.theme_mode = mode;
        if changed {
            self.render_markdown_now();
            self.reasoning.update_markdown();
        }
    }

    /// Re-renders the answer and reasoning Markdown under the currently active
    /// theme, forcing a repaint even when the light/dark mode is unchanged.
    ///
    /// A hot theme reload (configuration reload or the interactive preset
    /// selector) swaps the global palette without changing the light/dark mode,
    /// so the throttled [`Self::set_theme_mode`] path would otherwise leave the
    /// cached Markdown painted in the previous colors.
    pub fn refresh_theme(&mut self) {
        self.render_markdown_now();
        self.reasoning.update_markdown();
    }

    fn render_markdown_now(&mut self) {
        let flashing_id = self.copy_flash.as_ref().map(|(id, _)| *id);
        let mut theme = crate::markdown::MarkdownTheme::answer_themed(self.theme_mode);
        if self.content_width > 0 {
            theme.max_width = Some(self.content_width);
        }
        let (lines, blocks) = crate::markdown::render_markdown_with_blocks(
            &self.text,
            &theme,
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
        self.markdown_dirty = false;
        self.last_markdown_render = Some(Instant::now());
    }

    pub fn update_reasoning_markdown(&mut self) {
        if self.content_width > 0 {
            self.reasoning.max_width = Some(self.content_width);
        }
        self.reasoning.update_markdown();
    }

    /// Renders markdown deferred by throttling; called once per frame before drawing.
    pub fn flush_pending_markdown(&mut self) {
        if self.markdown_dirty
            && self
                .last_markdown_render
                .is_none_or(|t| t.elapsed() >= MARKDOWN_THROTTLE)
        {
            self.render_markdown_now();
        }
    }

    /// True while any animated spinner is visible and needs frame advancement.
    pub fn needs_spinner(&self) -> bool {
        self.is_busy()
            || self.toolchain.as_ref().is_some_and(ToolchainState::is_running)
            || (self.edits_active && self.edit_final_applied.is_none())
    }

    pub fn max_scroll(&mut self, view_height: u16) -> u16 {
        let count = if self.rendered_lines_count > 0 {
            self.rendered_lines_count
        } else {
            compute_model_lines(self).len()
        };
        u16::try_from(count)
            .unwrap_or(u16::MAX)
            .saturating_sub(view_height)
    }

    pub fn clamp_scroll(&mut self, view_height: u16) {
        let max = self.max_scroll(view_height);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    pub fn file_line_offset(&mut self, target_idx: usize) -> usize {
        let mut line_idx = 0;
        if self.error.is_some() {
            line_idx += 1;
        }
        if self.toolchain.is_some() {
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
            if self.reasoning.show {
                if !self.reasoning.lines.is_empty() {
                    line_idx += self.reasoning.lines.len();
                } else {
                    line_idx += self.reasoning.text.lines().count();
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
                if file.status != FileEditStatus::Ok && file.error.is_some() {
                    line_idx += 1;
                }
                if file.expanded {
                    line_idx += self.diff_cache.lines_for(file).len();
                }
            }
        }
        line_idx
    }

    pub fn scroll_to_selected_file(&mut self, view_height: u16) {
        if self.files.is_empty() || view_height == 0 {
            return;
        }
        let target_line =
            u16::try_from(self.file_line_offset(self.selected_file_index)).unwrap_or(u16::MAX);
        let is_expanded = self
            .files
            .get(self.selected_file_index)
            .map(|f| f.expanded)
            .unwrap_or(false);

        if target_line < self.scroll {
            self.auto_scroll = false;
            self.scroll = target_line;
        } else if is_expanded {
            if target_line.saturating_add(4) >= self.scroll.saturating_add(view_height) {
                self.auto_scroll = false;
                self.scroll = target_line.saturating_sub(1);
            }
        } else if target_line >= self.scroll.saturating_add(view_height) {
            self.auto_scroll = false;
            self.scroll = target_line.saturating_add(1).saturating_sub(view_height);
        }
        self.clamp_scroll(view_height);
    }

    pub fn is_busy(&self) -> bool {
        self.status.is_busy()
    }
}

pub fn compute_model_lines(model: &mut DevelopView) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();

    let mut model_lines: Vec<Line<'static>> = Vec::new();

    if let Some(err) = &model.error {
        model_lines.push(Line::from(vec![
            Span::raw("Error: "),
            Span::styled(err.clone(), engine.fg(palette.status_error)),
        ]));
    }

    if let Some(toolchain) = &model.toolchain {
        let (badge, text) = match toolchain {
            ToolchainState::Running { command } => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                (
                    Span::styled(
                        format!(" [{}] TOOLCHAIN ", spin),
                        indicator_style(&engine, palette.status_info),
                    ),
                    format!("Running `{}`...", command),
                )
            }
            ToolchainState::Passed { command } => (
                Span::styled(
                    " [TOOLCHAIN: OK] ",
                    indicator_style(&engine, palette.status_success),
                ),
                format!("`{}` passed", command),
            ),
            ToolchainState::Failed { command, message } => (
                Span::styled(
                    " [TOOLCHAIN: FAILED] ",
                    indicator_style(&engine, palette.status_error),
                ),
                message
                    .clone()
                    .unwrap_or_else(|| format!("`{}` failed", command)),
            ),
        };

        model_lines.push(Line::from(vec![
            badge,
            Span::raw(" "),
            Span::styled(text, Style::default().bold()),
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
        model_lines.extend(model.reasoning.render_lines());
    }

    if model.edits_active || !model.files.is_empty() {
        model_lines.push(Line::raw(""));

        let total_files = model.files.len();
        let ok_files = model
            .files
            .iter()
            .filter(|f| f.status == FileEditStatus::Ok)
            .count();

        let header_badge = match model.edit_final_applied {
            Some(true) => {
                if let Some(hash) = &model.last_commit_hash {
                    Span::styled(
                        format!(" [COMMITTED: {}] ", hash),
                        indicator_style(&engine, palette.status_success),
                    )
                } else {
                    Span::styled(
                        " [APPLIED] ",
                        indicator_style(&engine, palette.status_success),
                    )
                }
            }
            Some(false) => Span::styled(
                " [REJECTED] ",
                indicator_style(&engine, palette.status_error),
            ),
            None => {
                let spin = SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()];
                Span::styled(
                    format!(" [{}] Modifying ", spin),
                    indicator_style(&engine, palette.status_warning),
                )
            }
        };

        let header_spans = vec![
            header_badge,
            Span::raw(" "),
            Span::styled(
                format!("Proposed Edits ({}/{} files)", ok_files, total_files),
                engine.fg(palette.status_info).bold(),
            ),
        ];

        model_lines.push(Line::from(header_spans));

        if let Some(summary) = &model.last_commit_summary {
            model_lines.push(Line::from(vec![
                Span::raw("  Summary: "),
                Span::styled(summary.clone(), Style::default().bold()),
            ]));
        }

        for (idx, file) in model.files.iter().enumerate() {
            let is_selected = idx == model.selected_file_index;
            let fold_icon = if file.expanded { "▼ " } else { "▶ " };
            let cursor_prefix = if is_selected { "> " } else { "  " };

            let (status_icon, status_style) = match file.status {
                FileEditStatus::Ok => ("✓", engine.fg(palette.status_success).bold()),
                FileEditStatus::Error => ("✗", engine.fg(palette.status_error).bold()),
                FileEditStatus::Running | FileEditStatus::Retrying => (
                    SPINNER_FRAMES[model.spinner_frame % SPINNER_FRAMES.len()],
                    engine.fg(palette.status_warning).bold(),
                ),
            };

            let (op_label, op_style) = match file.op_type {
                EditOp::Create => ("[NEW] ", engine.fg(palette.status_success).bold()),
                EditOp::Delete => ("[DEL] ", engine.fg(palette.status_error).bold()),
                EditOp::Move => ("[MOVE] ", engine.fg(palette.status_info).bold()),
                EditOp::Overwrite => {
                    ("[OVERWRITE] ", engine.fg(palette.status_warning).bold())
                }
                EditOp::Replace => ("[EDIT] ", engine.fg(palette.button_active_bg).bold()),
            };

            let hunk_count = file.hunks.len();
            let hunk_label = match file.op_type {
                EditOp::Move => "renamed".to_string(),
                EditOp::Overwrite => "rewritten".to_string(),
                EditOp::Delete => "deleted".to_string(),
                EditOp::Create => {
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
                }
                EditOp::Replace => {
                    if hunk_count == 1 {
                        "1 hunk".to_string()
                    } else {
                        format!("{} hunks", hunk_count)
                    }
                }
            };

            let file_line_style = if is_selected {
                engine.bg(palette.overlay).bold()
            } else {
                Style::default()
            };

            let mut file_spans = vec![
                Span::raw("  "),
                Span::styled(cursor_prefix, engine.fg(palette.status_info)),
                Span::styled(fold_icon, engine.fg(palette.text_muted)),
                Span::styled(op_label, op_style),
                Span::styled(file.path.clone(), Style::default().bold()),
                Span::raw(" "),
                Span::styled(
                    format!("({})", hunk_label),
                    engine.fg(palette.text_muted),
                ),
                Span::raw(" "),
                Span::styled(status_icon, status_style),
            ];

            if file.status != FileEditStatus::Ok {
                if let Some(retry) = &file.retry_info {
                    file_spans.push(Span::raw(" "));
                    file_spans.push(Span::styled(
                        format!("(Retrying {})", retry),
                        engine.fg(palette.status_warning).bold(),
                    ));
                }
            }

            model_lines.push(Line::from(file_spans).style(file_line_style));

            if file.status != FileEditStatus::Ok {
                if let Some(err_msg) = &file.error {
                    model_lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::styled(
                            "Validation Error: ",
                            engine.fg(palette.status_error).bold(),
                        ),
                        Span::styled(err_msg.clone(), engine.fg(palette.status_warning)),
                    ]));
                }
            }

            if file.expanded {
                model_lines.extend(model.diff_cache.lines_for(file));
            }
        }
    }

    if let Some(usage) = &model.usage {
        model_lines.push(Line::raw(""));
        let mut usage_spans = vec![
            Span::styled("Tokens: ", engine.fg(palette.text_muted).bold()),
            Span::styled(
                format!(
                    "{} prompt, {} completion",
                    usage.usage.prompt_tokens, usage.usage.completion_tokens,
                ),
                engine.fg(palette.text_muted),
            ),
        ];

        if let Some(reason_tokens) = usage.usage.reasoning_tokens {
            if reason_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} reasoning", reason_tokens),
                    engine.fg(palette.text_secondary),
                ));
            }
        }

        if let Some(cached_tokens) = usage.usage.cached_tokens {
            if cached_tokens > 0 {
                usage_spans.push(Span::styled(
                    format!(", {} cached", cached_tokens),
                    engine.fg(palette.status_info),
                ));
            }
        }

        model_lines.push(Line::from(usage_spans));
    }

    model_lines
}
