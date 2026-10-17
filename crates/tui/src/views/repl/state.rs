use std::time::{Duration, Instant};

use ratatui::text::Line;
use tauqe_protocol::{ModelRef, ModelResult, ModelUsageEvent};

use crate::i18n::active_t;

use super::diff_cache::DiffRenderCache;
use super::reasoning::ReasoningState;
use super::render::compute_model_lines;

/// Minimum interval between full markdown re-parses while a response is streaming.
const MARKDOWN_THROTTLE: Duration = Duration::from_millis(80);

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

    /// Localized short label for the footer status indicator.
    pub fn label(self) -> String {
        match self {
            Self::Idle => active_t("status.idle"),
            Self::Awaiting => active_t("status.awaiting"),
            Self::Thinking => active_t("status.thinking"),
            Self::Responding => active_t("status.responding"),
            Self::Editing => active_t("status.editing"),
            Self::Verifying => active_t("status.verifying"),
            Self::Discovery => active_t("status.discovery"),
            Self::Staging => active_t("status.staging"),
            Self::Healing => active_t("status.healing"),
            Self::Done => active_t("status.done"),
            Self::Cancelled => active_t("status.cancelled"),
            Self::Error => active_t("status.error"),
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

/// Which pane of the REPL tab receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplFocus {
    #[default]
    Editor,
    Viewport,
}

pub struct ReplView {
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
    pub focus: ReplFocus,

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

impl Default for ReplView {
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
            focus: ReplFocus::Editor,
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

impl ReplView {
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

    pub(crate) fn render_markdown_now(&mut self) {
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
