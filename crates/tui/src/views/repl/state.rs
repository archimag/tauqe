use std::time::{Duration, Instant};

use ratatui::text::Line;
use tauqe_protocol::model::UsageTotalsSnapshot;
use tauqe_protocol::{ModelRef, ModelResult};

use crate::i18n::active_t;

use super::diff_cache::DiffRenderCache;
use super::reasoning::ReasoningState;
use super::render::compute_model_lines;

/// Minimum interval between full markdown re-parses while a response is streaming.
const MARKDOWN_THROTTLE: Duration = Duration::from_millis(80);

/// Wall-clock budget of the kinetic glyph assembly phase.
///
/// The glyphs of the product name converge toward their final slots within
/// this window; the typewriter stays silent until the monolith has fully
/// assembled, so the two phases never overlap (c-kinetic-phases).
const SPLASH_ASSEMBLY_DURATION: Duration = Duration::from_millis(700);

/// Total wall-clock budget of the splash: assembly followed by the motto.
///
/// Once this budget is exhausted both phases are complete and the view stops
/// requesting animation frames, so an idle REPL costs no redraws (c-2).
const SPLASH_TOTAL_DURATION: Duration = Duration::from_millis(2200);

/// Wall-clock budget of the typewriter phase alone.
///
/// It is derived from the total budget so the two amounts can never drift
/// apart, and it is zero-safe because the total always exceeds the assembly.
const SPLASH_TYPING_BUDGET: Duration =
    SPLASH_TOTAL_DURATION.saturating_sub(SPLASH_ASSEMBLY_DURATION);

/// Normalizes `elapsed` against `budget` into a clamped `0.0..=1.0` fraction.
///
/// A zero budget would divide by zero, so it degrades to a completed phase
/// instead of producing a non-finite progress value.
fn elapsed_fraction(elapsed: Duration, budget: Duration) -> f32 {
    let budget = budget.as_secs_f32();
    if budget <= 0.0 {
        return 1.0;
    }
    (elapsed.as_secs_f32() / budget).clamp(0.0, 1.0)
}

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

/// Live reconnect telemetry reported by the daemon while it backs off between
/// two provider attempts.
///
/// The daemon owns the countdown: every second of the pause is published as a
/// fresh snapshot, so the view mirrors the reported remaining seconds instead of
/// running its own timer and drifting away from the real deadline (c-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryingState {
    /// The attempt that just failed, one-based.
    pub attempt: u32,
    /// Total attempts the transport is allowed to make for one request.
    pub max_attempts: u32,
    /// Whole seconds left before the next attempt starts.
    pub delay_secs: u64,
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
    /// Authoritative usage snapshot mirrored from `model/usageTotals`.
    ///
    /// The daemon is the single accumulator of tokens, money, and time, so the
    /// view stores the reported snapshot verbatim instead of folding counters
    /// locally (c-1, c-2).
    pub usage_totals: UsageTotalsSnapshot,
    pub status: TurnStatus,
    pub error: Option<String>,
    pub result: Option<ModelResult>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub focus: ReplFocus,
    /// Instant the application started, anchoring the splash animation clock.
    pub splash_started: Instant,

    // Structured Org-Mode Edits & Git State
    pub edits_active: bool,
    pub files: Vec<StreamingFileEdit>,
    pub selected_file_index: usize,
    pub edit_final_applied: Option<bool>,
    pub edit_final_error: Option<String>,
    pub last_commit_hash: Option<String>,
    pub last_commit_summary: Option<String>,
    pub toolchain: Option<ToolchainState>,
    /// Pending provider reconnect, mirrored from the daemon's countdown.
    pub retrying: Option<RetryingState>,
    /// True when the last submitted prompt never reached the model history.
    ///
    /// The harness commits a prompt to the model context only when the turn
    /// settles, so a provider failure or a cancellation rolls the record back
    /// and leaves the editor history as the only place the text still lives.
    /// The flag drives the recall hint shown above the prompt until the next
    /// turn starts (c-3).
    pub prompt_rolled_back: bool,
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
            usage_totals: UsageTotalsSnapshot::default(),
            status: TurnStatus::default(),
            error: None,
            result: None,
            scroll: 0,
            auto_scroll: true,
            focus: ReplFocus::Editor,
            splash_started: Instant::now(),
            edits_active: false,
            files: Vec::new(),
            selected_file_index: 0,
            edit_final_applied: None,
            edit_final_error: None,
            last_commit_hash: None,
            last_commit_summary: None,
            toolchain: None,
            retrying: None,
            prompt_rolled_back: false,
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
    /// Releases every per-turn buffer so a new turn starts from a clean slate.
    ///
    /// The interface is long-lived: without this reset the reasoning transcript,
    /// the rendered answer, the streamed edits, and the rendered diff fragments
    /// of every previous round would stay alive for the whole session, and a
    /// batch of plan steps would accumulate them one step after another. Only
    /// the per-turn payload is dropped; the turn identity (`operation_id`,
    /// `model`, `status`, `plan_context`, `is_batch`), the scroll position, and
    /// the accumulator-owned usage totals belong to the caller that opens the
    /// turn and are set right after this reset (c-3).
    pub fn reset_turn_buffers(&mut self) {
        self.reasoning.clear();
        self.text.clear();
        self.markdown_lines.clear();
        self.code_blocks.clear();
        self.diff_cache = DiffRenderCache::default();
        self.markdown_dirty = false;
        self.last_markdown_render = None;
        self.error = None;
        self.result = None;
        self.edits_active = false;
        self.files.clear();
        self.selected_file_index = 0;
        self.edit_final_applied = None;
        self.edit_final_error = None;
        self.last_commit_hash = None;
        self.last_commit_summary = None;
        self.toolchain = None;
        self.retrying = None;
        self.prompt_rolled_back = false;
        self.turn_phase = None;
        self.turn_round = None;
        self.turn_max_rounds = None;
        self.turn_phase_detail = None;
        self.repomap_tokens = None;
        self.copy_flash = None;
    }

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
        let (lines, blocks) =
            crate::markdown::render_markdown_with_blocks(&self.text, &theme, flashing_id);
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

    /// True while the response area is empty and the splash greeting owns it.
    ///
    /// The greeting never competes with real output: as soon as the turn
    /// produces text, an error, a toolchain report, or a streamed edit, the
    /// regular viewport takes over on the very next frame (c-1).
    pub fn splash_visible(&self) -> bool {
        !self.is_busy()
            && self.text.is_empty()
            && self.error.is_none()
            && self.toolchain.is_none()
            && self.files.is_empty()
            && !self.edits_active
    }

    /// Fraction of the kinetic glyph assembly already played, clamped to
    /// `0.0..=1.0`.
    pub fn splash_assembly_progress(&self) -> f32 {
        elapsed_fraction(self.splash_started.elapsed(), SPLASH_ASSEMBLY_DURATION)
    }

    /// True once the glyphs have reached their final slots.
    pub fn splash_assembly_done(&self) -> bool {
        self.splash_assembly_progress() >= 1.0
    }

    /// Fraction of the splash motto already revealed, clamped to `0.0..=1.0`.
    ///
    /// The typewriter starts strictly after the assembly phase completes, so
    /// the motto never competes with the converging logo (c-kinetic-phases).
    pub fn splash_typing_progress(&self) -> f32 {
        let typing_elapsed = self
            .splash_started
            .elapsed()
            .saturating_sub(SPLASH_ASSEMBLY_DURATION);
        elapsed_fraction(typing_elapsed, SPLASH_TYPING_BUDGET)
    }

    /// True once the typewriter has revealed the whole motto.
    pub fn splash_typing_done(&self) -> bool {
        self.splash_typing_progress() >= 1.0
    }

    /// True while the splash still has frames left in either phase.
    fn splash_animating(&self) -> bool {
        self.splash_visible() && !(self.splash_assembly_done() && self.splash_typing_done())
    }

    /// True when a turn or background operation is actively executing.
    pub fn background_active(&self) -> bool {
        self.is_busy()
            || self
                .toolchain
                .as_ref()
                .is_some_and(ToolchainState::is_running)
            || (self.edits_active && self.edit_final_applied.is_none())
    }

    /// True while any animated spinner or the splash typewriter needs a frame.
    ///
    /// The splash keeps requesting frames only until the typewriter completes,
    /// so the shared timer loop settles into its idle cadence afterwards (c-2).
    pub fn needs_spinner(&self) -> bool {
        self.splash_animating() || self.background_active()
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

    /// Closes the in-flight usage window when a turn reaches a terminal status.
    ///
    /// The daemon owns accumulation and publishes the flushed snapshot, so this
    /// only drops a window a cancelled or failed turn left open; without it the
    /// footer would keep presenting a finished operation as still running (c-1).
    pub fn end_in_flight_usage(&mut self) {
        self.usage_totals.cur = None;
    }
}

#[cfg(test)]
mod turn_reset_tests {
    use super::*;

    fn stale_view() -> ReplView {
        let mut view = ReplView::default();
        view.reasoning.append_delta("stale reasoning");
        view.reasoning.toggle_fold();
        view.text = "stale answer".to_string();
        view.update_markdown();
        view.code_blocks.push(ModelCodeBlock {
            id: 0,
            lang: "rust".to_string(),
            code: "fn stale() {}".to_string(),
            raw_start_line: 0,
            raw_end_line: 1,
            visual_start_line: 0,
            visual_end_line: 1,
        });
        view.files.push(StreamingFileEdit {
            path: "crates/tui/src/lib.rs".to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: Vec::new(),
            expanded: false,
            retry_info: None,
        });
        view.edits_active = true;
        view.edit_final_applied = Some(true);
        view.edit_final_error = Some("stale".to_string());
        view.last_commit_hash = Some("abc1234".to_string());
        view.last_commit_summary = Some("feat: stale".to_string());
        view.toolchain = Some(ToolchainState::Passed {
            command: "cargo check".to_string(),
        });
        view.retrying = Some(RetryingState {
            attempt: 1,
            max_attempts: 3,
            delay_secs: 2,
        });
        view.prompt_rolled_back = true;
        view.turn_phase = Some(tauqe_protocol::TurnPhase::Healing);
        view.turn_round = Some(2);
        view.turn_max_rounds = Some(4);
        view.turn_phase_detail = Some("healing".to_string());
        view.repomap_tokens = Some(4096);
        view.error = Some("stale error".to_string());
        view.copy_flash = Some((0, Instant::now()));
        view
    }

    #[test]
    fn reset_turn_buffers_releases_every_per_turn_buffer() {
        let mut view = stale_view();
        assert!(!view.reasoning.is_empty());
        assert!(!view.markdown_lines.is_empty());

        view.reset_turn_buffers();

        assert!(view.reasoning.is_empty());
        assert!(view.reasoning.show, "the reasoning block reopens folded");
        assert!(!view.reasoning.is_streaming());
        assert!(view.text.is_empty());
        assert!(view.markdown_lines.is_empty());
        assert!(view.code_blocks.is_empty());
        assert!(view.files.is_empty());
        assert!(!view.edits_active);
        assert!(view.edit_final_applied.is_none());
        assert!(view.edit_final_error.is_none());
        assert!(view.last_commit_hash.is_none());
        assert!(view.last_commit_summary.is_none());
        assert!(view.toolchain.is_none());
        assert!(view.retrying.is_none());
        assert!(
            !view.prompt_rolled_back,
            "a new turn clears the recall hint of the rolled-back prompt"
        );
        assert!(view.turn_phase.is_none());
        assert!(view.turn_round.is_none());
        assert!(view.turn_max_rounds.is_none());
        assert!(view.turn_phase_detail.is_none());
        assert!(view.repomap_tokens.is_none());
        assert!(view.error.is_none());
        assert!(view.result.is_none());
        assert!(view.copy_flash.is_none());
        assert!(!view.markdown_dirty);
        assert!(
            view.splash_visible(),
            "a released turn returns the viewport to its opening state"
        );
    }

    #[test]
    fn reset_turn_buffers_preserves_turn_identity_and_usage() {
        let mut view = stale_view();
        view.operation_id = Some("op-7".to_string());
        view.model = Some(tauqe_protocol::ModelRef::openrouter("coder"));
        view.plan_context = Some(tauqe_protocol::ModelTurnPlanContext {
            plan_id: "plan-1".to_string(),
            step_id: "3".to_string(),
            batch_step_ids: vec!["3".to_string(), "4".to_string()],
        });
        view.is_batch = true;
        view.usage_totals.session.total_tokens = 128;

        view.reset_turn_buffers();

        assert_eq!(view.operation_id.as_deref(), Some("op-7"));
        assert_eq!(
            view.model,
            Some(tauqe_protocol::ModelRef::openrouter("coder"))
        );
        assert_eq!(
            view.plan_context
                .as_ref()
                .map(|context| context.step_id.as_str()),
            Some("3")
        );
        assert!(view.is_batch);
        assert_eq!(
            view.usage_totals.session.total_tokens, 128,
            "the accumulator-owned usage totals survive the reset"
        );
    }
}

#[cfg(test)]
mod splash_tests {
    use super::*;

    #[test]
    fn empty_view_opens_with_pending_typing() {
        let view = ReplView::default();
        assert!(view.splash_visible());
        assert!(!view.splash_typing_done());
        assert!(view.splash_typing_progress() < 1.0);
        assert!(view.needs_spinner());
    }

    #[test]
    fn any_output_or_busy_turn_hides_the_splash() {
        let view = ReplView {
            text: "answer".to_string(),
            ..ReplView::default()
        };
        assert!(!view.splash_visible());

        let busy = ReplView {
            status: TurnStatus::Thinking,
            ..ReplView::default()
        };
        assert!(!busy.splash_visible());
    }

    #[test]
    fn completed_typing_stops_the_redraw_loop() {
        let view = ReplView {
            splash_started: Instant::now()
                .checked_sub(SPLASH_TOTAL_DURATION + Duration::from_millis(1))
                .expect("system uptime exceeds the splash duration"),
            ..ReplView::default()
        };

        assert!(view.splash_typing_done());
        assert!((view.splash_typing_progress() - 1.0).abs() < f32::EPSILON);
        assert!(!view.needs_spinner());
    }

    #[test]
    fn all_hiding_conditions_displace_splash_immediately() {
        // 1. Error message
        let view_err = ReplView {
            error: Some("Turn failed".to_string()),
            ..ReplView::default()
        };
        assert!(!view_err.splash_visible());
        assert!(!view_err.splash_animating());

        // 2. Running toolchain
        let view_toolchain = ReplView {
            toolchain: Some(ToolchainState::Running {
                command: "cargo test".to_string(),
            }),
            ..ReplView::default()
        };
        assert!(!view_toolchain.splash_visible());

        // 3. Active file edits
        let view_edits = ReplView {
            edits_active: true,
            ..ReplView::default()
        };
        assert!(!view_edits.splash_visible());

        // 4. Streamed files list
        let view_files = ReplView {
            files: vec![StreamingFileEdit {
                path: "crates/tui/src/main.rs".to_string(),
                op_type: EditOp::Replace,
                status: FileEditStatus::Ok,
                error: None,
                hunks: Vec::new(),
                expanded: false,
                retry_info: None,
            }],
            ..ReplView::default()
        };
        assert!(!view_files.splash_visible());

        // 5. All busy TurnStatus variants
        let busy_statuses = [
            TurnStatus::Awaiting,
            TurnStatus::Thinking,
            TurnStatus::Responding,
            TurnStatus::Editing,
            TurnStatus::Verifying,
            TurnStatus::Discovery,
            TurnStatus::Staging,
            TurnStatus::Healing,
        ];
        for status in busy_statuses {
            let busy_view = ReplView {
                status,
                ..ReplView::default()
            };
            assert!(
                !busy_view.splash_visible(),
                "status {:?} must hide splash",
                status
            );
        }
    }

    #[test]
    fn assembly_and_typing_phases_run_strictly_in_sequence() {
        let now = Instant::now();

        // Phase 1 start: the glyphs are still apart, the motto stays hidden.
        let view_start = ReplView {
            splash_started: now,
            ..ReplView::default()
        };
        assert!(view_start.splash_assembly_progress() <= 0.05);
        assert!(!view_start.splash_assembly_done());
        assert!(view_start.splash_typing_progress() <= 0.05);
        assert!(!view_start.splash_typing_done());
        assert!(view_start.splash_animating());

        // Phase 1 middle: assembly is half played, typing has not started.
        let view_half = ReplView {
            splash_started: now
                .checked_sub(SPLASH_ASSEMBLY_DURATION / 2)
                .expect("valid uptime"),
            ..ReplView::default()
        };
        let assembly = view_half.splash_assembly_progress();
        assert!((assembly - 0.5).abs() < 0.05);
        assert!(!view_half.splash_assembly_done());
        assert!(view_half.splash_typing_progress() <= 0.0);
        assert!(view_half.splash_animating());

        // Phase 1 complete: the typewriter starts from its very beginning.
        let view_assembled = ReplView {
            splash_started: now
                .checked_sub(SPLASH_ASSEMBLY_DURATION)
                .expect("valid uptime"),
            ..ReplView::default()
        };
        assert!(view_assembled.splash_assembly_done());
        assert!(view_assembled.splash_typing_progress() <= 0.05);
        assert!(!view_assembled.splash_typing_done());
        assert!(view_assembled.splash_animating());

        // Phase 2 middle: the motto is half revealed.
        let view_typing_half = ReplView {
            splash_started: now
                .checked_sub(SPLASH_ASSEMBLY_DURATION + SPLASH_TYPING_BUDGET / 2)
                .expect("valid uptime"),
            ..ReplView::default()
        };
        let typing = view_typing_half.splash_typing_progress();
        assert!((typing - 0.5).abs() < 0.05);
        assert!(!view_typing_half.splash_typing_done());
        assert!(view_typing_half.splash_animating());

        // Both phases complete: no frames are requested any more.
        let view_done = ReplView {
            splash_started: now
                .checked_sub(SPLASH_TOTAL_DURATION + Duration::from_millis(50))
                .expect("valid uptime"),
            ..ReplView::default()
        };
        assert_eq!(view_done.splash_assembly_progress(), 1.0);
        assert_eq!(view_done.splash_typing_progress(), 1.0);
        assert!(view_done.splash_typing_done());
        assert!(!view_done.splash_animating());
        assert!(!view_done.needs_spinner());

        // Mid-typing displacement stops the animation immediately.
        let mut interrupted = view_typing_half;
        interrupted.text.push_str("first byte of stream");
        assert!(!interrupted.splash_visible());
        assert!(!interrupted.splash_animating());
    }

    #[test]
    fn needs_spinner_stays_active_until_the_second_phase_ends() {
        let now = Instant::now();

        let assembling = ReplView {
            splash_started: now
                .checked_sub(SPLASH_ASSEMBLY_DURATION / 2)
                .expect("valid uptime"),
            ..ReplView::default()
        };
        assert!(!assembling.background_active());
        assert!(assembling.needs_spinner());

        let typing = ReplView {
            splash_started: now
                .checked_sub(SPLASH_ASSEMBLY_DURATION + SPLASH_TYPING_BUDGET / 4)
                .expect("valid uptime"),
            ..ReplView::default()
        };
        assert!(typing.splash_assembly_done());
        assert!(!typing.splash_typing_done());
        assert!(typing.needs_spinner());
    }

    #[test]
    fn background_active_vs_needs_spinner_separation() {
        let empty_view = ReplView::default();
        // Background is idle, but typewriter is animating
        assert!(!empty_view.background_active());
        assert!(empty_view.needs_spinner());

        // When typing finishes and view is idle, neither requires spinner
        let finished_view = ReplView {
            splash_started: Instant::now()
                .checked_sub(SPLASH_TOTAL_DURATION + Duration::from_millis(10))
                .expect("valid uptime"),
            ..ReplView::default()
        };
        assert!(!finished_view.background_active());
        assert!(!finished_view.needs_spinner());

        // When a turn runs, background_active and needs_spinner both activate
        let busy_view = ReplView {
            status: TurnStatus::Thinking,
            ..finished_view
        };
        assert!(busy_view.background_active());
        assert!(busy_view.needs_spinner());
    }
}
