use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use tauqe_protocol::model::{
    ModelTurnPlanContext, ReviewWorkPhase, TurnWorkContext, TurnWorkItem, TurnWorkQueue,
};
use tauqe_protocol::{DiscussionFocus, DiscussionTarget};

use crate::app::AppState;
use crate::i18n::active_t;
use crate::theme::color::RgbColor;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::geometry;
use crate::ui::{wrap_line, UiAction};

use super::state::ReplFocus;

mod model;
mod splash;

pub(crate) use self::model::file_op_badge;
use self::splash::render_splash;

pub use self::model::compute_model_lines;

/// Frame glyphs cycled while any animated spinner is visible.
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Filled indicator style: a semantic background role with inverse text.
///
/// Toolchain, commit, and turn-status badges resolve both their surface and
/// their label from the active palette, so a badge never disappears on a
/// terminal with a non-standard palette (c-1).
pub(crate) fn indicator_style(engine: &ThemeEngine, bg: RgbColor) -> Style {
    engine
        .bg(bg)
        .fg(engine.color(engine.palette().text_inverse))
        .bold()
}

/// Fixed capacity of the sliding step window drawn around the active member.
const MAX_STEPPER_WINDOW: usize = 5;

/// Response rows the work marker never takes, so model output stays readable.
const MIN_MODEL_ROWS: u16 = 4;

pub fn render_repl_view(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    model_area: Rect,
    input_area: Rect,
    prompt_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    // A banner is a pure indicator, so it is carved out of the response area
    // rather than the prompt itself: the rows directly above the prompt host the
    // rollback recall hint and the discussion focus anchor (c-visual-anchor,
    // c-3).
    let banners = banner_rows(state);
    let banner_count = banners
        .len()
        .min(usize::from(model_area.height.saturating_sub(1)));
    let (model_area, banner_area) = if banner_count > 0 {
        let height = u16::try_from(banner_count).unwrap_or(u16::MAX);
        let chunks =
            Layout::vertical([Constraint::Min(1), Constraint::Length(height)]).split(model_area);
        (chunks[0], Some(chunks[1]))
    } else {
        (model_area, None)
    };

    // The universal work marker states what the turn executes and never
    // displaces a readable response pane: without room for the expanded card
    // it keeps its single dense row (c-3, c-4).
    let marker = work_marker(state);
    let marker_layout = marker
        .as_ref()
        .map(|marker| marker.layout(model_area.height))
        .unwrap_or(MarkerLayout::Hidden);
    let (marker_area, model_area) = match marker.as_ref() {
        Some(marker) if marker_layout != MarkerLayout::Hidden => {
            let chunks = Layout::vertical([
                Constraint::Length(marker.height(marker_layout)),
                Constraint::Min(0),
            ])
            .split(model_area);
            (Some(chunks[0]), chunks[1])
        }
        _ => (None, model_area),
    };

    render_model_pane(frame, state, model_area);

    if let (Some(marker), Some(area)) = (marker.as_ref(), marker_area) {
        marker.render(frame, area, marker_layout);
    }

    if let Some(banner_area) = banner_area {
        for (index, banner) in banners.into_iter().take(banner_count).enumerate() {
            let row = Rect::new(
                banner_area.x,
                banner_area
                    .y
                    .saturating_add(u16::try_from(index).unwrap_or(u16::MAX)),
                banner_area.width,
                1,
            );
            match banner {
                ReplBanner::RolledBackPrompt => render_rolled_back_banner(frame, state, row),
                ReplBanner::DiscussionFocus => render_discussion_focus_banner(frame, state, row),
            }
        }
    }

    render_prompt_input(frame, state, input_area, prompt_lines, cursor_visual_line);
}

/// Indicator rows reserved directly above the prompt; they never overlay it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplBanner {
    /// The last prompt never reached the model history; name the recall chords.
    RolledBackPrompt,
    /// The active discussion focus anchor.
    DiscussionFocus,
}

/// Selects the indicator rows shown above the prompt, ordered top to bottom.
///
/// The discussion focus anchor stays in the row closest to the editor, so the
/// recall hint of a rolled-back prompt reads as the older event above it (c-3).
fn banner_rows(state: &AppState) -> Vec<ReplBanner> {
    let mut rows = Vec::new();
    if state.model.prompt_rolled_back {
        rows.push(ReplBanner::RolledBackPrompt);
    }
    if state.discussion_focus.is_active() {
        rows.push(ReplBanner::DiscussionFocus);
    }
    rows
}

/// Renders the recall hint for a prompt the harness rolled back out of the
/// model history.
///
/// The row is a pure indicator: it states that the prompt was not recorded and
/// names the editor chords that restore it into the buffer, so a failed or
/// cancelled turn never costs the developer the text they just wrote (c-3).
fn render_rolled_back_banner(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let engine = current_engine();
    let palette = engine.palette();
    let line = Line::from(vec![
        Span::styled(" ⚠ ", engine.fg(palette.status_warning).bold()),
        Span::styled(
            state.prompt_recall_hint(),
            engine.fg(palette.status_warning),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Renders the active discussion focus as a one-line status banner above the prompt.
///
/// The banner is a pure indicator (Conventions.md §2): it names the focused
/// entity, its title, the number of targeted items, and the detach shortcut, but
/// carries no command of its own. The server owns the authoritative focus; this
/// only mirrors it for display.
fn render_discussion_focus_banner(frame: &mut ratatui::Frame, state: &AppState, area: Rect) {
    let engine = current_engine();
    let palette = engine.palette();
    let (badge, badge_bg, id, title, focused_count) = match &state.discussion_focus {
        DiscussionFocus::Focused {
            target,
            focused_items,
        } => match target {
            DiscussionTarget::Plan { id } => {
                let title = state
                    .plans_view
                    .plans
                    .iter()
                    .find(|plan| &plan.id == id)
                    .map(|plan| plan.title.clone())
                    .unwrap_or_else(|| id.clone());
                (
                    active_t("repl.focus_badge_plan"),
                    palette.status_info,
                    id.clone(),
                    title,
                    focused_items.len(),
                )
            }
            DiscussionTarget::Review { id } => {
                let title = state
                    .review
                    .sessions
                    .iter()
                    .find(|session| &session.id == id)
                    .map(|session| session.title.clone())
                    .unwrap_or_else(|| id.clone());
                (
                    active_t("repl.focus_badge_review"),
                    palette.button_active_bg,
                    id.clone(),
                    title,
                    focused_items.len(),
                )
            }
        },
        DiscussionFocus::None => return,
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", badge),
            engine
                .bg(badge_bg)
                .fg(engine.color(palette.text_inverse))
                .bold(),
        ),
        Span::raw(" "),
        Span::styled(id, engine.fg(palette.status_info).bold()),
        Span::raw(" \u{2014} "),
        Span::styled(
            title.replace(['\r', '\n'], " "),
            engine.fg(palette.text_primary),
        ),
    ];

    if focused_count > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("({} focused)", focused_count),
            engine.fg(palette.text_muted),
        ));
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        active_t("repl.detach_hint"),
        engine.fg(palette.text_muted).bold(),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Layout of the universal work marker above the response pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerLayout {
    /// No marker: the running turn executes no declared work.
    Hidden,
    /// The single dense breadcrumb row (c-3).
    Compact,
    /// The breadcrumb plus the sliding step window in a bordered card (c-4).
    Expanded,
}

/// One member of the ordered work queue, resolved for rendering.
struct WorkMember {
    id: String,
    title: String,
}

impl WorkMember {
    /// Resolves a member the daemon declared as typed work.
    fn from_item(item: &TurnWorkItem) -> Self {
        Self {
            id: item.id.clone(),
            title: item.title.clone(),
        }
    }

    /// Resolves a member a legacy producer declared by its identifier alone.
    fn from_id(id: &str) -> Self {
        Self {
            id: id.to_string(),
            title: String::new(),
        }
    }
}

/// Breadcrumb content of the universal work marker.
///
/// Every field is resolved once from the declaration the daemon sent, so the
/// compact row and the expanded window can never name two different tasks and
/// the viewport never reconstructs the running work from the stored plans and
/// reviews (c-1).
struct WorkBreadcrumb {
    /// Catalog key of the work-kind badge.
    badge_key: &'static str,
    /// Name of the entity that owns the work.
    entity: String,
    /// Ordered work members, empty when the work is not a batch.
    members: Vec<WorkMember>,
    /// Zero-based index of the member the turn executes.
    active_index: Option<usize>,
    /// Catalog key of a phase that executes no work member.
    phase_key: Option<&'static str>,
}

impl WorkBreadcrumb {
    /// Active member together with its one-based position and the queue length.
    fn active(&self) -> Option<(&WorkMember, usize, usize)> {
        let index = self.active_index?;
        let member = self.members.get(index)?;
        Some((member, index + 1, self.members.len()))
    }
}

/// Fixed-capacity slice of the work queue rendered around the active member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepWindow {
    start: usize,
    len: usize,
}

impl StepWindow {
    /// One past the last member the window renders.
    fn end(self) -> usize {
        self.start + self.len
    }
}

/// Positions the sliding window so the active member always stays visible.
///
/// The window keeps a fixed capacity and centers on the active member, clamped
/// to the queue so it never runs past either end. Members outside the window
/// are reported by count instead of being rendered, which keeps a plan or a
/// group plan of any length inside a bounded marker (c-4). A queue of at most
/// one member needs no window at all.
fn step_window(total: usize, active: usize, capacity: usize) -> Option<StepWindow> {
    if capacity == 0 || total <= 1 || active >= total {
        return None;
    }
    let len = capacity.min(total);
    let start = active.saturating_sub(len / 2).min(total - len);
    Some(StepWindow { start, len })
}

/// Universal work marker rendered above the response pane.
///
/// The marker names the work the running turn declares — its kind, the owning
/// entity, the position inside the batch queue, the active member, and the
/// phase — from the typed work contract, so the Develop viewport never derives
/// the task from the stored plans and reviews (c-1). It occupies one dense row
/// by default (c-3) and adds the sliding window of the queued members once the
/// developer expands it (c-4).
struct WorkMarker {
    /// The always-visible single-row breadcrumb.
    header: Line<'static>,
    /// Member rows of the sliding window, empty while the marker is compact.
    window: Vec<Line<'static>>,
}

impl WorkMarker {
    /// Chooses how much of the marker fits above a readable response pane.
    ///
    /// The breadcrumb is never dropped for lack of space: when the expanded
    /// card cannot fit, the marker keeps its single dense row instead of
    /// displacing the model output (c-3).
    fn layout(&self, available: u16) -> MarkerLayout {
        if available <= 1 {
            return MarkerLayout::Hidden;
        }
        if self.window.is_empty() {
            return MarkerLayout::Compact;
        }
        let rows = u16::try_from(self.window.len()).unwrap_or(u16::MAX);
        if rows.saturating_add(3) <= available.saturating_sub(MIN_MODEL_ROWS) {
            MarkerLayout::Expanded
        } else {
            MarkerLayout::Compact
        }
    }

    /// Vertical rows the marker occupies in `layout`.
    fn height(&self, layout: MarkerLayout) -> u16 {
        let rows = u16::try_from(self.window.len()).unwrap_or(u16::MAX);
        match layout {
            MarkerLayout::Hidden => 0,
            MarkerLayout::Compact => 1,
            MarkerLayout::Expanded => rows.saturating_add(3),
        }
    }

    /// Draws the marker into the rows carved for it.
    fn render(&self, frame: &mut ratatui::Frame, area: Rect, layout: MarkerLayout) {
        if layout == MarkerLayout::Hidden {
            return;
        }
        let engine = current_engine();
        let mut block = Block::default().padding(Padding::horizontal(1));
        let mut lines = Vec::with_capacity(self.window.len() + 1);
        lines.push(self.header.clone());
        if layout == MarkerLayout::Expanded {
            block = block
                .borders(Borders::ALL)
                .border_style(engine.fg(engine.palette().border));
            lines.extend(self.window.iter().cloned());
        }
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// Builds the universal work marker from the declared work contract.
///
/// A turn that executes no declared work yields `None`, so an interactive
/// answer keeps the whole response area and Develop pays nothing (c-1, c-3).
fn work_marker(state: &AppState) -> Option<WorkMarker> {
    let breadcrumb = work_breadcrumb(state)?;
    let engine = current_engine();
    let spinner = state
        .model
        .is_busy()
        .then(|| SPINNER_FRAMES[state.model.spinner_frame % SPINNER_FRAMES.len()]);
    let window = if state.model.work_marker_expanded {
        step_window_lines(&breadcrumb, spinner, &engine)
    } else {
        Vec::new()
    };
    Some(WorkMarker {
        header: breadcrumb_header(&breadcrumb, spinner, &engine),
        window,
    })
}

/// Resolves the marker content from the work the turn declares.
///
/// The typed [`TurnWorkContext`] is the primary source. A producer that
/// predates the contract declares only the plan coordinates of the turn, so
/// those coordinates are rendered as they are instead of inferring a work kind
/// the client was never told (c-1).
fn work_breadcrumb(state: &AppState) -> Option<WorkBreadcrumb> {
    if let Some(context) = state.model.work_context.as_ref() {
        return work_context_breadcrumb(context);
    }
    let context = state.model.plan_context.as_ref()?;
    Some(plan_context_breadcrumb(context))
}

/// Resolves the breadcrumb of a declared work context.
fn work_context_breadcrumb(context: &TurnWorkContext) -> Option<WorkBreadcrumb> {
    let breadcrumb = match context {
        TurnWorkContext::Interactive => return None,
        TurnWorkContext::PlanStep {
            plan_title, queue, ..
        } => queue_breadcrumb("repl.work.badge_plan", plan_title, queue, None),
        TurnWorkContext::ReviewGroupStep {
            review_title,
            group_title,
            queue,
            ..
        } => {
            let entity = format!("{review_title} \u{203a} {group_title}");
            queue_breadcrumb("repl.work.badge_review_group", &entity, queue, None)
        }
        TurnWorkContext::ReviewStep {
            review_title,
            queue,
            ..
        } => queue_breadcrumb("repl.work.badge_review", review_title, queue, None),
        TurnWorkContext::ReviewPhase {
            review_title,
            phase,
            queue,
            ..
        } => {
            let phase = review_phase_key(*phase);
            queue_breadcrumb("repl.work.badge_review", review_title, queue, Some(phase))
        }
    };
    Some(breadcrumb)
}

/// Builds the breadcrumb of one ordered work queue.
fn queue_breadcrumb(
    badge_key: &'static str,
    entity: &str,
    queue: &TurnWorkQueue,
    phase: Option<&'static str>,
) -> WorkBreadcrumb {
    WorkBreadcrumb {
        badge_key,
        entity: entity.to_string(),
        members: queue.items.iter().map(WorkMember::from_item).collect(),
        active_index: queue.active().map(|_| queue.active_index),
        phase_key: phase,
    }
}

/// Builds the breadcrumb of a producer that declares plan coordinates alone.
fn plan_context_breadcrumb(context: &ModelTurnPlanContext) -> WorkBreadcrumb {
    let members: Vec<WorkMember> = if context.batch_step_ids.is_empty() {
        vec![WorkMember::from_id(&context.step_id)]
    } else {
        context
            .batch_step_ids
            .iter()
            .map(|id| WorkMember::from_id(id.as_str()))
            .collect()
    };
    let active_index = members
        .iter()
        .position(|member| member.id == context.step_id);
    WorkBreadcrumb {
        badge_key: "repl.work.badge_plan",
        entity: context.plan_id.clone(),
        members,
        active_index,
        phase_key: None,
    }
}

/// Catalog key of the phase a review turn executes without a work member.
fn review_phase_key(phase: ReviewWorkPhase) -> &'static str {
    match phase {
        ReviewWorkPhase::Audit => "repl.work.phase_audit",
        ReviewWorkPhase::Decomposition => "repl.work.phase_decomposition",
        ReviewWorkPhase::PostAnalysis => "repl.work.phase_post_analysis",
    }
}

/// Builds the single-row breadcrumb: badge, entity, queue position, the active
/// member, and the phase, in that order (c-3).
fn breadcrumb_header(
    breadcrumb: &WorkBreadcrumb,
    spinner: Option<&'static str>,
    engine: &ThemeEngine,
) -> Line<'static> {
    let palette = engine.palette();
    let mut spans = vec![
        Span::styled(
            format!(" {} ", active_t(breadcrumb.badge_key)),
            indicator_style(engine, palette.status_info),
        ),
        Span::raw(" "),
        Span::styled(
            breadcrumb.entity.clone(),
            engine.fg(palette.text_primary).bold(),
        ),
    ];

    if let Some((active, position, total)) = breadcrumb.active() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[{position}/{total}]"),
            engine.fg(palette.status_info),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("#{}", active.id),
            engine.fg(palette.text_muted),
        ));
        if !active.title.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                active.title.clone(),
                engine.fg(palette.text_secondary),
            ));
        }
    }

    if let Some(phase_key) = breadcrumb.phase_key {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            active_t(phase_key),
            engine.fg(palette.status_warning).bold(),
        ));
    }

    if let Some(spinner) = spinner {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(spinner, engine.fg(palette.status_warning)));
    }

    Line::from(spans)
}

/// Renders the sliding step window around the active work member (c-4).
fn step_window_lines(
    breadcrumb: &WorkBreadcrumb,
    spinner: Option<&'static str>,
    engine: &ThemeEngine,
) -> Vec<Line<'static>> {
    let Some((_, position, total)) = breadcrumb.active() else {
        return Vec::new();
    };
    let active_index = position - 1;
    let Some(window) = step_window(total, active_index, MAX_STEPPER_WINDOW) else {
        return Vec::new();
    };

    let palette = engine.palette();
    let mut rows = Vec::with_capacity(window.len + 2);

    if window.start > 0 {
        rows.push(hidden_members_row(
            engine,
            "repl.work.hidden_before",
            window.start,
        ));
    }

    for index in window.start..window.end() {
        let Some(member) = breadcrumb.members.get(index) else {
            continue;
        };
        let is_active = index == active_index;
        let prefix = match (is_active, spinner) {
            (true, Some(spinner)) => format!(" {spinner} "),
            _ => "   ".to_string(),
        };
        let title_style = if is_active {
            engine.fg(palette.text_primary).bold()
        } else {
            engine.fg(palette.text_secondary)
        };
        let mut spans = vec![Span::styled(prefix, engine.fg(palette.status_info).bold())];
        spans.push(Span::styled(
            format!("#{}", member.id),
            engine.fg(palette.text_muted),
        ));
        if !member.title.is_empty() {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(member.title.clone(), title_style));
        }
        rows.push(Line::from(spans));
    }

    let hidden_after = total.saturating_sub(window.end());
    if hidden_after > 0 {
        rows.push(hidden_members_row(
            engine,
            "repl.work.hidden_after",
            hidden_after,
        ));
    }

    rows
}

/// Renders one hidden-members indicator row of the sliding window.
fn hidden_members_row(engine: &ThemeEngine, key: &str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw("   "),
        Span::styled(
            active_t(key).replacen("{}", &count.to_string(), 1),
            engine.fg(engine.palette().text_muted),
        ),
    ])
}

fn render_model_pane(frame: &mut ratatui::Frame, state: &mut AppState, area: Rect) {
    let model_block = Block::default().padding(Padding::horizontal(1));
    let inner = model_block.inner(area);

    let content_height = inner.height;
    let content_width = (inner.width as usize).max(10);
    state.last_model_height = content_height;

    if state.model.content_width != content_width {
        state.model.content_width = content_width;
        state.model.reasoning.max_width = Some(content_width);
        state.model.render_markdown_now();
        // The reasoning block is a lazy surface: this re-parses only while the
        // transcript is expanded and settled, and releases the rendered lines
        // otherwise (c-2).
        state.model.reasoning.update_markdown();
    }

    if let Some((_, inst)) = state.model.copy_flash {
        if inst.elapsed().as_secs_f32() >= 2.0 {
            state.model.copy_flash = None;
            state.model.update_markdown();
        }
    }

    // An empty viewport is owned by the transient startup greeting. It yields
    // to the regular viewport on the first byte of output, so the greeting
    // never competes with real work (c-1).
    if state.model.splash_visible() {
        render_splash(frame, &state.model, inner);
        return;
    }

    let raw_lines = compute_model_lines(&mut state.model);

    let prefix_count =
        (state.model.error.is_some() as usize) + (state.model.toolchain.is_some() as usize);

    let mut raw_to_visual_start = Vec::with_capacity(raw_lines.len());
    let mut raw_to_visual_end = Vec::with_capacity(raw_lines.len());
    let mut model_lines = Vec::with_capacity(raw_lines.len());
    let mut current_visual = 0;
    for line in raw_lines {
        let wrapped = wrap_line(line, content_width);
        let wrapped_count = wrapped.len();
        raw_to_visual_start.push(current_visual);
        raw_to_visual_end.push(current_visual + wrapped_count.saturating_sub(1));
        current_visual += wrapped_count;
        model_lines.extend(wrapped);
    }

    for block in &mut state.model.code_blocks {
        let b_raw_start = prefix_count + block.raw_start_line;
        let b_raw_end = prefix_count + block.raw_end_line;
        if b_raw_start < raw_to_visual_start.len() && b_raw_end < raw_to_visual_end.len() {
            block.visual_start_line = raw_to_visual_start[b_raw_start];
            block.visual_end_line = raw_to_visual_end[b_raw_end];
        }
    }

    let total_lines = model_lines.len() as u16;
    state.model.rendered_lines_count = model_lines.len();

    let max_scroll = total_lines.saturating_sub(content_height);
    if state.model.auto_scroll || state.model.scroll > max_scroll {
        state.model.scroll = max_scroll;
    }

    // Every code block the viewport just laid out declares its own clickable
    // regions: the informational body first, then the copy button of its header
    // row, because a later registration wins over the surface it covers. The
    // viewport therefore owns its Markdown geometry and the pointer dispatcher
    // never recomputes a column or a scroll offset (c-1, c-2).
    let mut block_regions: Vec<(Rect, UiAction)> = Vec::new();
    for block in &state.model.code_blocks {
        if let Some(rect) = geometry::code_block_body_rect(
            inner,
            block.visual_start_line,
            block.visual_end_line,
            state.model.scroll,
            content_height,
        ) {
            block_regions.push((rect, UiAction::CodeBlockBody));
        }
        if let Some(rect) = geometry::code_copy_button_rect(
            inner,
            0,
            &block.lang,
            block.visual_start_line,
            state.model.scroll,
            content_height,
        ) {
            block_regions.push((
                rect,
                UiAction::CopyCodeBlock {
                    item: None,
                    block: block.id,
                },
            ));
        }
    }
    for (rect, action) in block_regions {
        state.hit_regions.register(rect, action);
    }

    let scroll_start = state.model.scroll as usize;
    let visible_count = content_height as usize;
    let visible_lines: Vec<Line<'static>> = model_lines
        .into_iter()
        .skip(scroll_start)
        .take(visible_count)
        .collect();

    let model_paragraph = Paragraph::new(visible_lines).block(model_block);
    frame.render_widget(model_paragraph, area);
}

/// Computes the prompt editor scroll offset so the cursor stays visible,
/// moving the viewport only when the cursor leaves the visible window.
///
/// The offset is retained across frames: recomputing it from scratch pinned
/// the cursor to the bottom row and made upward navigation jitter and stick
/// at the lower edge. Scrolling now happens only at the window boundaries
/// (visibility hysteresis), matching the Emacs-style `recenter` behavior
/// users expect from a multiline prompt.
fn ensure_visible_input_scroll(
    current_scroll: usize,
    cursor_visual_line: usize,
    visible_input_lines: usize,
    total_lines: usize,
) -> usize {
    if visible_input_lines == 0 || total_lines <= visible_input_lines {
        return 0;
    }

    let max_scroll = total_lines - visible_input_lines;
    let mut scroll = current_scroll.min(max_scroll);

    if cursor_visual_line < scroll {
        scroll = cursor_visual_line;
    } else if cursor_visual_line >= scroll + visible_input_lines {
        scroll = cursor_visual_line + 1 - visible_input_lines;
    }

    scroll.min(max_scroll)
}

fn render_prompt_input(
    frame: &mut ratatui::Frame,
    state: &mut AppState,
    area: Rect,
    input_lines: Vec<Line<'static>>,
    cursor_visual_line: usize,
) {
    let is_busy = state.model.is_busy();
    let visible_input_lines = area.height.saturating_sub(2) as usize;

    // Keep the prompt scroll stateful: the viewport moves only when the
    // cursor leaves the visible window, instead of snapping to the bottom
    // row on every frame (c-prompt-input-scroll-hysteresis).
    let input_scroll = ensure_visible_input_scroll(
        state.model.input_scroll as usize,
        cursor_visual_line,
        visible_input_lines,
        input_lines.len(),
    ) as u16;
    state.model.input_scroll = input_scroll;

    let engine = current_engine();
    let palette = engine.palette();
    let in_viewport = state.model.focus == ReplFocus::Viewport;
    let border_style = if is_busy {
        engine.fg(palette.status_warning)
    } else if in_viewport {
        engine.fg(palette.text_muted)
    } else {
        Style::default()
    };

    let mut input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    if in_viewport {
        input_block = input_block.title(format!(" {} ", active_t("repl.viewport_title")));
    }

    let input_paragraph = Paragraph::new(input_lines)
        .block(input_block)
        .scroll((input_scroll, 0));
    frame.render_widget(input_paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_within_the_window_keeps_the_scroll_stable() {
        // Moving the cursor up inside the visible window must not shift the view.
        assert_eq!(ensure_visible_input_scroll(10, 14, 5, 40), 10);
        assert_eq!(ensure_visible_input_scroll(10, 10, 5, 40), 10);
    }

    #[test]
    fn cursor_above_the_window_scrolls_up_to_reveal_it() {
        assert_eq!(ensure_visible_input_scroll(10, 7, 5, 40), 7);
        assert_eq!(ensure_visible_input_scroll(10, 0, 5, 40), 0);
    }

    #[test]
    fn cursor_below_the_window_scrolls_down_just_enough() {
        assert_eq!(ensure_visible_input_scroll(0, 5, 5, 40), 1);
        assert_eq!(ensure_visible_input_scroll(3, 20, 5, 40), 16);
    }

    #[test]
    fn short_content_or_empty_window_pins_scroll_to_zero() {
        assert_eq!(ensure_visible_input_scroll(4, 2, 10, 6), 0);
        assert_eq!(ensure_visible_input_scroll(4, 2, 0, 10), 0);
        assert_eq!(ensure_visible_input_scroll(0, 0, 5, 5), 0);
    }

    #[test]
    fn scroll_never_exceeds_maximum() {
        // A stale offset larger than the content clamps to the last page.
        assert_eq!(ensure_visible_input_scroll(100, 39, 5, 40), 35);
    }

    #[test]
    fn upward_navigation_is_monotonic_without_jitter() {
        let visible = 5usize;
        let total = 20usize;
        let mut scroll = 0usize;

        for cursor in 0..total {
            scroll = ensure_visible_input_scroll(scroll, cursor, visible, total);
        }
        assert_eq!(scroll, total - visible);

        let mut observed = Vec::new();
        for cursor in (0..total).rev() {
            scroll = ensure_visible_input_scroll(scroll, cursor, visible, total);
            observed.push(scroll);
        }

        // While the cursor stays inside the window the scroll must not move;
        // it only follows once the cursor rises above the top row.
        assert_eq!(observed[0], total - visible);
        assert_eq!(observed[1], total - visible);
        assert!(observed.iter().all(|&s| s <= total - visible));
        assert_eq!(*observed.last().unwrap(), 0);
    }
    #[test]
    fn the_step_window_centers_on_the_active_member_and_clamps_at_both_ends() {
        assert_eq!(step_window(10, 5, 5), Some(StepWindow { start: 3, len: 5 }));
        // The window clamps to both ends of the queue instead of running past
        // them.
        assert_eq!(step_window(10, 0, 5), Some(StepWindow { start: 0, len: 5 }));
        assert_eq!(step_window(10, 9, 5), Some(StepWindow { start: 5, len: 5 }));
        // A queue shorter than the capacity is rendered whole.
        assert_eq!(step_window(3, 1, 5), Some(StepWindow { start: 0, len: 3 }));
        // A single member, an out-of-range index, and an empty capacity need
        // no window at all.
        assert_eq!(step_window(1, 0, 5), None);
        assert_eq!(step_window(10, 10, 5), None);
        assert_eq!(step_window(10, 5, 0), None);
    }

    #[test]
    fn the_active_member_always_lands_inside_the_window() {
        for total in 1..=24usize {
            for active in 0..total {
                let Some(window) = step_window(total, active, MAX_STEPPER_WINDOW) else {
                    assert_eq!(total, 1, "only a one-member queue needs no window");
                    continue;
                };
                assert!(window.len <= MAX_STEPPER_WINDOW);
                assert!(
                    window.start <= active,
                    "the window never starts below the active member"
                );
                assert!(active < window.end(), "the active member stays visible");
                assert!(
                    window.end() <= total,
                    "the window never runs past the queue"
                );
            }
        }
    }

    #[test]
    fn the_expanded_card_degrades_to_the_compact_row_on_a_short_pane() {
        let marker = WorkMarker {
            header: Line::from("breadcrumb"),
            window: vec![Line::from("member"); 5],
        };
        // Five member rows plus the breadcrumb and the two borders need eight
        // rows while the response pane keeps its own minimum.
        assert_eq!(marker.layout(12), MarkerLayout::Expanded);
        assert_eq!(marker.height(MarkerLayout::Expanded), 8);
        assert_eq!(marker.layout(11), MarkerLayout::Compact);
        assert_eq!(marker.height(MarkerLayout::Compact), 1);
        assert_eq!(marker.layout(1), MarkerLayout::Hidden);

        let collapsed = WorkMarker {
            header: Line::from("breadcrumb"),
            window: Vec::new(),
        };
        assert_eq!(collapsed.layout(2), MarkerLayout::Compact);
    }

    #[test]
    fn a_legacy_plan_producer_still_names_its_declared_step() {
        let context = ModelTurnPlanContext {
            plan_id: "plan-1".to_string(),
            step_id: "2".to_string(),
            batch_step_ids: vec!["1".to_string(), "2".to_string(), "3".to_string()],
        };
        let breadcrumb = plan_context_breadcrumb(&context);
        assert_eq!(breadcrumb.badge_key, "repl.work.badge_plan");
        assert_eq!(breadcrumb.entity, "plan-1");
        let (member, position, total) = breadcrumb.active().expect("the declared step is active");
        assert_eq!(member.id, "2");
        assert_eq!((position, total), (2, 3));

        // A turn outside a batch declares a single coordinate and needs no
        // window.
        let single = ModelTurnPlanContext {
            batch_step_ids: Vec::new(),
            ..context
        };
        let breadcrumb = plan_context_breadcrumb(&single);
        assert_eq!(
            breadcrumb
                .active()
                .map(|(_, position, total)| (position, total)),
            Some((1, 1))
        );
        assert_eq!(step_window(1, 0, MAX_STEPPER_WINDOW), None);
    }
}
