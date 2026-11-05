use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ratatui::text::Line;
use tauqe_protocol::model::{TurnWorkContext, UsageTotalsSnapshot};
use tauqe_protocol::{ModelRef, ModelResult};

use super::diff_cache::DiffRenderCache;
use super::reasoning::ReasoningState;
use super::render::measure_canvas;
use crate::app::{DiffInspectorFocus, DiffInspectorState};
use crate::i18n::active_t;
use crate::views::recenter::RecenterState;

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

/// Lifecycle status of a model turn as tracked by the REPL view.
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
    Research,
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
                | Self::Research
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
            Self::Research => active_t("status.research"),
            Self::Staging => active_t("status.staging"),
            Self::Healing => active_t("status.healing"),
            Self::Done => active_t("status.done"),
            Self::Cancelled => active_t("status.cancelled"),
            Self::Error => active_t("status.error"),
        }
    }
}

/// Lifecycle status of a single streamed file edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// One hunk of a file patch, anchored to the lines it changes.
///
/// The coordinates are the ones a unified diff hunk header states
/// (`@@ -old_start,old_lines +new_start,new_lines @@ hint`): they name the real
/// lines of the file before the patch and after it, so a diff surface reports
/// where a change lands instead of counting the rows of the patch
/// (c-hunk-coordinates-model).
///
/// A hunk whose coordinates are unknown keeps zeros, so `0` means "unknown"
/// rather than a real line, and a surface that renders coordinates must treat
/// it as such.
#[derive(Debug, Clone)]
pub struct StreamingHunk {
    pub hunk_index: usize,
    /// One-based first line of the hunk in the file before the patch.
    pub old_start: usize,
    /// Number of source lines the hunk covers.
    pub old_lines: usize,
    /// One-based first line of the hunk in the file after the patch.
    pub new_start: usize,
    /// Number of result lines the hunk covers.
    pub new_lines: usize,
    /// Section the header names after its ranges, usually the enclosing
    /// function signature, as git prints it.
    pub context_hint: Option<String>,
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

/// Row offsets of the canvas the viewport drew last.
///
/// The renderer already wraps every raw line to draw it, so it records the span
/// each raw line occupies together with the raw index of every staged-file
/// summary row. File navigation, recentering, and the scroll bound then resolve
/// against the drawn layout instead of rebuilding and re-wrapping the whole
/// canvas per keystroke (c-1, c-4).
#[derive(Default)]
pub(crate) struct CanvasProjection {
    /// Fingerprint of the state the projection was measured from.
    pub(crate) signature: u64,
    /// First visual row of every raw canvas line.
    pub(crate) visual_start: Vec<usize>,
    /// Last visual row of every raw canvas line.
    pub(crate) visual_end: Vec<usize>,
    /// Raw canvas index of every staged-file summary row.
    pub(crate) file_rows: Vec<usize>,
    /// Visual rows the whole canvas occupies.
    pub(crate) total_lines: usize,
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
    /// Three-beat recenter state of the staged-file selection.
    ///
    /// The shared [`RecenterState`] component owns only the cycle bookkeeping;
    /// the view keeps owning its selection, its scroll, and its layout.
    pub recenter: RecenterState,
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
    /// Work the model executes in this turn, declared by the daemon.
    ///
    /// The daemon states the running work — the owning plan or review, the
    /// active member, and its position in the batch queue — so the REPL names
    /// the declared task instead of reconstructing it from the stored plans and
    /// reviews (c-1). The field belongs to the turn identity: a payload reset
    /// keeps it, and only a new `MODEL_STARTED` declaration replaces it.
    pub work_context: Option<TurnWorkContext>,
    /// Detail level of the universal work marker above the response pane.
    ///
    /// `false` keeps the single dense breadcrumb row, `true` adds the sliding
    /// window of the queued members (c-3, c-4). The choice belongs to the
    /// session, so it survives a turn reset until the developer switches it
    /// again.
    pub work_marker_expanded: bool,
    pub is_batch: bool,
    pub repomap_tokens: Option<usize>,
    pub spinner_frame: usize,
    pub rendered_lines_count: usize,
    pub copy_flash: Option<(usize, std::time::Instant)>,
    pub code_blocks: Vec<ModelCodeBlock>,
    /// Hyperlinks of the rendered answer, in the columns the Markdown renderer
    /// painted them at.
    ///
    /// The answer is parsed once, so the registry reads the links of that parse
    /// instead of re-parsing the body on every frame; the viewport resolves them
    /// onto the rows it draws (c-1, c-3).
    pub links: Vec<crate::markdown::ExtractedLink>,
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
    /// Wrap projection of the canvas the viewport drew last (c-1, c-4).
    ///
    /// Navigation reads it instead of rebuilding the canvas per keystroke, and
    /// its recorded signature decides whether it still describes the live state.
    pub(crate) canvas: CanvasProjection,
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
            recenter: RecenterState::default(),
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
            work_context: None,
            work_marker_expanded: false,
            is_batch: false,
            repomap_tokens: None,
            spinner_frame: 0,
            rendered_lines_count: 0,
            copy_flash: None,
            code_blocks: Vec::new(),
            links: Vec::new(),
            content_width: 0,
            input_scroll: 0,
            esc_clear_armed: None,
            markdown_dirty: false,
            last_markdown_render: None,
            diff_cache: DiffRenderCache::default(),
            canvas: CanvasProjection::default(),
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
    /// `model`, `status`, `plan_context`, `work_context`, `is_batch`), the
    /// scroll position, and the accumulator-owned usage totals belong to the
    /// caller that opens the turn and are set right after this reset (c-3).
    pub fn reset_turn_buffers(&mut self) {
        self.reasoning.clear();
        self.text.clear();
        self.markdown_lines.clear();
        self.code_blocks.clear();
        self.links.clear();
        self.diff_cache = DiffRenderCache::default();
        self.markdown_dirty = false;
        self.last_markdown_render = None;
        self.error = None;
        self.result = None;
        self.edits_active = false;
        self.files.clear();
        self.selected_file_index = 0;
        // The recenter cycle is scoped to the selection, and a fresh turn
        // starts it over from its first beat.
        self.recenter = RecenterState::default();
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
        // The projection describes the canvas of the finished turn; the next
        // turn records its own before anything reads it (c-1).
        self.canvas = CanvasProjection::default();
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
        let (lines, blocks, links) =
            crate::markdown::render_markdown_with_blocks(&self.text, &theme, flashing_id);
        self.markdown_lines = lines;
        self.links = links;
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

    /// Maximum scroll offset of the viewport for a given visible height.
    ///
    /// The row count the last frame reported wins, because it is the canvas the
    /// developer is looking at. Before the first frame of a turn the recorded
    /// projection answers instead, and only a stale recording falls back to the
    /// borrowed measuring pass, so the bound never drifts from the drawn rows
    /// (c-1, c-4).
    pub fn max_scroll(&mut self, view_height: u16) -> u16 {
        let count = if self.rendered_lines_count > 0 {
            self.rendered_lines_count
        } else {
            self.refresh_canvas_projection();
            self.canvas.total_lines
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

    /// Fingerprint of every input the wrapped canvas is built from.
    ///
    /// The projection the renderer records is reused only while this fingerprint
    /// still describes the live state, so a stale recording can never misposition
    /// the viewport. The folded inputs are exactly the ones the canvas renders:
    /// the wrap width, the rendered answer Markdown or the raw text behind it,
    /// the reasoning transcript with its fold, stream lifecycle, and dropped-line
    /// counter, the diagnostic and toolchain prefixes, the whole staged change
    /// set, the commit trail, the usage counters, and the copy flash (c-1, c-4).
    ///
    /// The animated spinner is deliberately absent: every glyph of the frame set
    /// occupies exactly one column, so a tick can change the content of a badge
    /// but never the number of rows it wraps into. The fingerprint hashes a few
    /// dozen bytes and never clones a line, a message, or a staged body.
    pub(crate) fn canvas_signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.content_width.max(10).hash(&mut hasher);
        self.markdown_lines.len().hash(&mut hasher);
        self.text.len().hash(&mut hasher);
        self.reasoning.lines.len().hash(&mut hasher);
        self.reasoning.text.len().hash(&mut hasher);
        self.reasoning.truncated_lines().hash(&mut hasher);
        self.reasoning.show.hash(&mut hasher);
        self.reasoning.is_streaming().hash(&mut hasher);
        self.error.hash(&mut hasher);
        match self.toolchain.as_ref() {
            Some(ToolchainState::Running { command }) => {
                0u8.hash(&mut hasher);
                command.hash(&mut hasher);
            }
            Some(ToolchainState::Passed { command }) => {
                1u8.hash(&mut hasher);
                command.hash(&mut hasher);
            }
            Some(ToolchainState::Failed { command, message }) => {
                2u8.hash(&mut hasher);
                command.hash(&mut hasher);
                message.hash(&mut hasher);
            }
            None => 3u8.hash(&mut hasher),
        }
        self.edits_active.hash(&mut hasher);
        self.edit_final_applied.hash(&mut hasher);
        self.selected_file_index.hash(&mut hasher);
        self.last_commit_hash.hash(&mut hasher);
        self.last_commit_summary.hash(&mut hasher);
        for file in &self.files {
            file.path.hash(&mut hasher);
            file.op_type.as_str().hash(&mut hasher);
            file.status.hash(&mut hasher);
            file.hunks.len().hash(&mut hasher);
            file.error.hash(&mut hasher);
            file.retry_info.hash(&mut hasher);
        }
        let totals = self.usage_totals.operation_totals();
        totals.prompt_tokens.hash(&mut hasher);
        totals.completion_tokens.hash(&mut hasher);
        totals.total_tokens.hash(&mut hasher);
        totals.reasoning_tokens.hash(&mut hasher);
        totals.cached_tokens.hash(&mut hasher);
        self.copy_flash
            .as_ref()
            .map(|(block, _)| *block)
            .hash(&mut hasher);
        hasher.finish()
    }

    /// Records the wrap projection of the canvas the renderer just laid out.
    pub(crate) fn record_canvas_projection(
        &mut self,
        visual_start: Vec<usize>,
        visual_end: Vec<usize>,
        file_rows: Vec<usize>,
        total_lines: usize,
    ) {
        self.canvas = CanvasProjection {
            signature: self.canvas_signature(),
            visual_start,
            visual_end,
            file_rows,
            total_lines,
        };
    }

    /// Refreshes the recorded projection when the live canvas no longer matches it.
    fn refresh_canvas_projection(&mut self) {
        if self.canvas.signature != self.canvas_signature() {
            self.canvas = measure_canvas(self);
        }
    }

    /// Visual span of every staged file summary row, with the canvas height.
    ///
    /// The spans are read from the projection the renderer recorded, and measured
    /// by the borrowed wrapping pass (see [`measure_canvas`]) only when that
    /// recording no longer describes the live state. Either way they name the rows
    /// the viewport draws, so a summary wider than the canvas still occupies
    /// several visual rows (c-1, c-4).
    fn file_summary_layout(&mut self) -> (Vec<(usize, usize)>, usize) {
        self.refresh_canvas_projection();
        let projection = &self.canvas;
        let spans = projection
            .file_rows
            .iter()
            .filter_map(|row| {
                let start = projection.visual_start.get(*row).copied()?;
                let end = projection.visual_end.get(*row).copied().unwrap_or(start);
                Some((start, end))
            })
            .collect();
        (spans, projection.total_lines)
    }

    /// Materializes the wrapped canvas the viewport would draw (test-only).
    ///
    /// Production paths read the recorded projection; the regression tests pin
    /// that derivation to the rows the renderer draws, so they keep materializing
    /// the canvas the shared wrapping pass produces.
    #[cfg(test)]
    fn wrapped_canvas(&mut self) -> (Vec<Line<'static>>, Vec<usize>, Vec<usize>) {
        let width = self.content_width.max(10);
        super::render::wrap_model_lines(super::render::compute_model_lines(self), width)
    }

    /// Visual span of every staged file summary row, in `files` order.
    ///
    /// Selecting a file and keeping it visible resolve through this single
    /// derivation, so no caller reproduces the canvas arithmetic by hand
    /// (c-1, c-4).
    pub fn file_summary_visual_rows(&mut self) -> Vec<(usize, usize)> {
        self.file_summary_layout().0
    }

    /// Scrolls the viewport just far enough to reveal the selected staged file.
    ///
    /// The target is the visual start of the file's summary span in the wrapped
    /// canvas, so a wrapped summary or a bounded reasoning tail cannot park the
    /// viewport past the row it is meant to reveal (c-1, c-4).
    pub fn scroll_to_selected_file(&mut self, view_height: u16) {
        if self.files.is_empty() || view_height == 0 {
            return;
        }
        let rows = self.file_summary_visual_rows();
        let Some(&(row, _)) = rows.get(self.selected_file_index) else {
            return;
        };
        let row = u16::try_from(row).unwrap_or(u16::MAX);
        if row < self.scroll {
            self.auto_scroll = false;
            self.scroll = row;
        } else if row >= self.scroll.saturating_add(view_height) {
            self.auto_scroll = false;
            self.scroll = row.saturating_add(1).saturating_sub(view_height);
        }
        self.clamp_scroll(view_height);
    }

    /// Recenters the Viewport on the visual span of the selected staged file.
    ///
    /// Repeated presses walk the shared three-beat cycle — center, top, bottom —
    /// and moving the selection restarts the cycle, exactly like the recenter
    /// command of the cursor-driven views (Conventions.md §5.2). The span comes
    /// from the same wrapped canvas the viewport draws, so a summary that wraps
    /// recenters over its whole height and no patch body or reasoning row can
    /// shift the beats (c-1, c-4).
    pub fn recenter_selected_file(&mut self, view_height: u16) {
        if self.files.is_empty() || view_height == 0 {
            return;
        }
        let selected = self.selected_file_index;
        let (rows, total_lines) = self.file_summary_layout();
        let Some(&(start, end)) = rows.get(selected) else {
            return;
        };
        let scroll = self.recenter.recenter(
            selected,
            start,
            end.saturating_add(1),
            total_lines,
            view_height,
        );
        self.auto_scroll = false;
        self.scroll = scroll;
    }

    /// Builds the reading position of a fullscreen audit over the staged set.
    ///
    /// The audit is opened by one command chord reachable from the prompt and
    /// from the Viewport alike, so every entry point builds its reading position
    /// here: the same press over the same staged set always opens the same audit
    /// (c-1). The highlight starts on the file the Viewport had selected,
    /// clamped into the live set, and the isolated diff pane opens at its first
    /// line. The change set itself is never captured: the audit reads the single
    /// owned `files` slice in place, so an empty staged set yields `None` and the
    /// caller reports the missing target instead of drawing an empty card.
    pub fn inspector_snapshot(&self) -> Option<DiffInspectorState> {
        if self.files.is_empty() {
            return None;
        }
        let selected_file = self
            .selected_file_index
            .min(self.files.len().saturating_sub(1));
        Some(DiffInspectorState {
            selected_file,
            diff_scroll: 0,
            focus: DiffInspectorFocus::Files,
        })
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
    use tauqe_protocol::model::{TurnWorkItem, TurnWorkQueue};
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
        view.links.push(crate::markdown::ExtractedLink {
            id: 0,
            url: "https://example.com".to_string(),
            line_idx: 0,
            start_col: 4,
            end_col: 21,
        });
        view.files.push(StreamingFileEdit {
            path: "crates/tui/src/lib.rs".to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: Vec::new(),
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
        view.work_context = Some(TurnWorkContext::Interactive);
        view.error = Some("stale error".to_string());
        view.copy_flash = Some((0, Instant::now()));
        view
    }

    #[test]
    fn reset_turn_buffers_releases_every_per_turn_buffer() {
        let mut view = stale_view();
        assert!(!view.reasoning.is_empty());
        assert!(!view.markdown_lines.is_empty());

        // The cycle is driven once, so the reset has a non-default beat to clear.
        view.recenter_selected_file(10);
        assert_ne!(view.recenter, RecenterState::default());

        view.reset_turn_buffers();
        assert!(!view.reasoning.is_streaming());
        assert!(view.text.is_empty());
        assert!(view.markdown_lines.is_empty());
        assert!(view.code_blocks.is_empty());
        assert!(view.links.is_empty());
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
        assert!(
            view.work_context.is_some(),
            "the declared work belongs to the turn identity and survives the reset"
        );
        assert_eq!(
            view.recenter,
            RecenterState::default(),
            "a fresh turn starts the recenter cycle over"
        );
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
        view.work_context = Some(TurnWorkContext::PlanStep {
            plan_id: "plan-1".to_string(),
            plan_title: "Plan One".to_string(),
            queue: TurnWorkQueue {
                items: vec![
                    TurnWorkItem {
                        id: "3".to_string(),
                        title: "Declare the contract".to_string(),
                    },
                    TurnWorkItem {
                        id: "4".to_string(),
                        title: "Wire the clients".to_string(),
                    },
                ],
                active_index: 1,
            },
        });
        view.is_batch = true;
        view.work_marker_expanded = true;
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
        assert!(
            view.work_marker_expanded,
            "the marker detail level survives a turn reset"
        );
        assert!(
            matches!(
                view.work_context.as_ref(),
                Some(TurnWorkContext::PlanStep { plan_id, queue, .. })
                    if plan_id == "plan-1" && queue.active_index == 1
            ),
            "the declared work survives the reset until the next declaration"
        );
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
            TurnStatus::Research,
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

#[cfg(test)]
mod compact_file_list_tests {
    use super::super::render::compute_model_lines;
    use super::*;

    /// Wide canvas: a rendered summary never wraps, so its visual row equals the
    /// raw canvas index the layout records.
    const WIDE: usize = 200;

    fn file(path: &str, status: FileEditStatus) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status,
            error: None,
            hunks: vec![StreamingHunk {
                hunk_index: 0,
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                context_hint: None,
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            }],
            retry_info: None,
        }
    }

    /// One answer line, the proposed-edits header, and one summary row per file:
    ///
    /// ```text
    /// 0  answer
    /// 1  blank
    /// 2  proposed edits header
    /// 3  file 0 summary
    /// 4  file 1 summary
    /// ```
    fn view() -> ReplView {
        ReplView {
            text: "answer".to_string(),
            content_width: WIDE,
            files: vec![
                file("crates/tui/src/views/repl/a.rs", FileEditStatus::Ok),
                file("crates/tui/src/views/repl/b.rs", FileEditStatus::Ok),
            ],
            ..ReplView::default()
        }
    }

    /// Full text of one rendered row.
    fn row_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The single rendered row whose text contains `needle`.
    fn rendered_row_of(wrapped: &[Line<'static>], needle: &str) -> usize {
        let rows: Vec<usize> = wrapped
            .iter()
            .enumerate()
            .filter(|(_, line)| row_text(line).contains(needle))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(rows.len(), 1, "exactly one row must carry {needle}");
        rows[0]
    }

    /// Every derived span starts on the row that renders its file summary, stays
    /// inside the canvas, and never runs backwards.
    fn assert_derivation_matches_the_canvas(view: &mut ReplView) {
        let rows = view.file_summary_visual_rows();
        let (wrapped, _, _) = view.wrapped_canvas();
        assert_eq!(
            rows.len(),
            view.files.len(),
            "every staged file owns exactly one span"
        );
        for (staged, (start, end)) in view.files.iter().zip(rows) {
            assert!(start <= end, "a span never runs backwards");
            assert!(end < wrapped.len(), "a span stays inside the canvas");
            let rendered: String = wrapped[start..=end].iter().map(row_text).collect();
            assert!(
                rendered.contains(&staged.path),
                "the span must start on the rendered summary row: {rendered}"
            );
        }
    }

    #[test]
    fn a_file_with_hunks_occupies_a_single_summary_row() {
        let mut view = view();
        assert!(
            view.files.iter().all(|file| !file.hunks.is_empty()),
            "the fixture carries patch bodies"
        );
        assert_eq!(
            compute_model_lines(&mut view).len(),
            5,
            "the patch bodies stay out of the canvas and every file keeps one row (c-4)"
        );
    }

    #[test]
    fn the_summary_rows_are_addressable_in_the_wrapped_canvas() {
        let mut view = view();
        assert_eq!(
            view.file_summary_visual_rows(),
            vec![(3, 3), (4, 4)],
            "a wide canvas keeps one visual row per summary"
        );
    }

    /// The renderer records the projection it just wrapped, and navigation reuses
    /// it verbatim while its fingerprint describes the live state. The sentinel
    /// recording proves the reuse: the spans below can only come from the
    /// recording, never from a fresh measurement. Any change to a canvas input
    /// invalidates the fingerprint, so the next derivation measures the live
    /// canvas instead of the stale rows (c-1, c-4).
    #[test]
    fn a_matching_recording_is_reused_and_a_stale_one_is_measured_again() {
        let mut view = view();
        view.record_canvas_projection(vec![0, 1, 2, 3, 4], vec![0, 1, 2, 3, 4], vec![0, 1], 5);

        assert_eq!(view.file_summary_visual_rows(), vec![(0, 0), (1, 1)]);
        assert_eq!(view.max_scroll(4), 1, "the bound reads the recorded height");

        view.files[0].status = FileEditStatus::Error;
        view.files[0].error = Some("hunk did not match the file".to_string());

        assert_eq!(
            view.file_summary_visual_rows(),
            vec![(3, 3), (5, 5)],
            "a changed change set invalidates the recording"
        );
        assert_eq!(view.max_scroll(2), 4);
    }

    #[test]
    fn a_narrow_canvas_reports_a_multi_row_span_for_a_wrapped_summary() {
        let mut view = view();
        view.content_width = 16;
        view.files[0].path = "src/very long module name for the signer.rs".to_string();

        let rows = view.file_summary_visual_rows();
        let (start, end) = rows[0];
        assert!(
            end > start,
            "a summary wider than the canvas wraps over several rows"
        );

        let (wrapped, _, _) = view.wrapped_canvas();
        assert!(end < wrapped.len(), "the span stays inside the canvas");
        let rendered: String = wrapped[start..=end]
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            rendered.contains("signer.rs"),
            "the span must reach the end of the summary: {rendered}"
        );

        let outside: String = wrapped
            .iter()
            .enumerate()
            .filter(|(index, _)| *index < start || *index > end)
            .map(|(_, line)| row_text(line))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !outside.contains("signer.rs"),
            "the span must cover every row of the summary: {outside}"
        );
    }

    #[test]
    fn a_failed_file_keeps_its_diagnostic_row_below_the_summary() {
        let mut view = view();
        view.files[0].status = FileEditStatus::Error;
        view.files[0].error = Some("hunk did not match the file".to_string());
        let rows = view.file_summary_visual_rows();
        assert_eq!(rows[0], (3, 3));
        assert_eq!(
            rows[1],
            (5, 5),
            "the rendered diagnostic row shifts the next summary row"
        );
    }

    #[test]
    fn selecting_a_file_keeps_its_summary_row_visible() {
        let mut view = view();

        view.selected_file_index = 1;
        view.scroll_to_selected_file(2);
        assert_eq!(view.scroll, 3, "the last row is pulled above the edge");

        view.selected_file_index = 0;
        view.scroll = 5;
        view.scroll_to_selected_file(2);
        assert_eq!(view.scroll, 3);
        assert!(!view.auto_scroll);
    }

    #[test]
    fn recenter_keeps_the_selected_row_inside_the_document() {
        let mut view = view();
        view.selected_file_index = 1;

        view.recenter_selected_file(2);

        assert!(!view.auto_scroll);
        let max = view.max_scroll(2);
        assert!(
            view.scroll <= max,
            "the recentered row stays in the document"
        );

        // Moving the selection restarts the shared recenter cycle.
        view.selected_file_index = 0;
        view.recenter_selected_file(2);
        let max = view.max_scroll(2);
        assert!(view.scroll <= max);
    }

    /// The regression the finding documents: while a reasoning stream is active
    /// the renderer shows only the bounded tail, so the derived row must not
    /// track the retained transcript.
    #[test]
    fn a_streaming_transcript_never_shifts_the_derived_file_rows() {
        let mut view = view();
        view.selected_file_index = 1;
        for index in 0..40 {
            view.reasoning
                .append_delta(&format!("reasoning line {index}\n"));
        }
        assert!(
            view.reasoning.is_streaming(),
            "the fixture keeps the reasoning stream open"
        );

        let (wrapped, _, _) = view.wrapped_canvas();
        let derived = view.file_summary_visual_rows()[1].0;
        assert_eq!(
            derived,
            rendered_row_of(&wrapped, "repl/b.rs"),
            "the derived row must name the row the canvas draws"
        );

        // The streaming renderer shows a bounded tail, so a longer transcript
        // cannot move the row. The full-transcript arithmetic drifted here.
        for index in 40..120 {
            view.reasoning
                .append_delta(&format!("reasoning line {index}\n"));
        }
        assert_eq!(
            view.file_summary_visual_rows()[1].0,
            derived,
            "the streaming renderer shows only the bounded tail"
        );
    }

    /// A transcript past the retention cap renders one extra marker row, which
    /// the deleted mirror never counted.
    #[test]
    fn a_trimmed_transcript_keeps_the_derived_rows_on_the_drawn_canvas() {
        let mut view = view();
        let chunk = "reasoning line with a stable length\n";
        let appends = (192 * 1024) / chunk.len();
        for _ in 0..appends {
            view.reasoning.append_delta(chunk);
        }
        assert!(
            view.reasoning.text.len() < appends * chunk.len(),
            "the fixture must cross the transcript retention cap"
        );

        assert_derivation_matches_the_canvas(&mut view);
    }

    /// The settled, expanded reasoning block renders its whole transcript, and
    /// the derivation must follow it.
    #[test]
    fn a_settled_expanded_transcript_keeps_the_derived_rows_on_the_drawn_canvas() {
        let mut view = view();
        for index in 0..30 {
            view.reasoning
                .append_delta(&format!("reasoning line {index}\n"));
        }
        view.reasoning.finalize();
        view.reasoning.toggle_fold();
        assert!(view.reasoning.show, "the settled block is expanded");

        assert_derivation_matches_the_canvas(&mut view);
    }

    /// Crossing the transcript retention cap is a discrete state transition: the
    /// retained window keeps streaming while a truncation-marker row joins the
    /// canvas. The fingerprint must observe it, or a recording measured before
    /// the trim answers after the window grew back to the same length (c-1, c-4).
    #[test]
    fn a_tripped_retention_cap_invalidates_the_recorded_projection() {
        let mut view = view();
        let line = "reasoning line with a stable length\n";
        while view.reasoning.text.len() + line.len() <= 100 * 1024 {
            view.reasoning.append_delta(line);
        }
        let recorded_len = view.reasoning.text.len();
        assert_eq!(
            view.reasoning.truncated_lines(),
            0,
            "the recorded state must precede the retention cap"
        );

        view.record_canvas_projection(vec![0, 1, 2, 3, 4], vec![0, 1, 2, 3, 4], vec![0, 1], 5);
        assert_eq!(
            view.file_summary_visual_rows(),
            vec![(0, 0), (1, 1)],
            "the recording is reused while it describes the live state"
        );

        let appends = (192 * 1024) / line.len();
        for _ in 0..appends {
            view.reasoning.append_delta(line);
        }
        assert!(
            view.reasoning.truncated_lines() > 0,
            "the fixture must trip the retention cap"
        );

        let restored = recorded_len.saturating_sub(view.reasoning.text.len());
        view.reasoning
            .append_delta(&line.repeat(restored / line.len()));
        view.reasoning
            .append_delta(&"x".repeat(restored % line.len()));
        assert_eq!(
            view.reasoning.text.len(),
            recorded_len,
            "only the truncation counter differs from the recorded state"
        );

        assert_ne!(
            view.file_summary_visual_rows(),
            vec![(0, 0), (1, 1)],
            "a tripped cap must invalidate the stale recording"
        );
        assert_derivation_matches_the_canvas(&mut view);
    }
}
