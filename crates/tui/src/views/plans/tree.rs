//! Tree-line rendering of the Plans view.
//!
//! The Plans tree is a document of state rather than a widget tree: every row,
//! its detail block, and the section headers are rendered into one flat line
//! stream whose leading indentation is derived from the numeric nesting depth.
//! Hosting that scale here keeps `plans.rs` focused on tree state and
//! navigation, and keeps the strictly increasing indent invariant testable in
//! one place (Conventions §4.8).

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{ArchitectTier, PlanItemStatus};

use crate::markdown::MarkdownBodyCache;
use crate::theme::engine::current_engine;
use crate::ui::wrap::{wrap_line, wrapped_line_count};

use super::VisiblePlanRow;

/// Indent step, in columns, of one nesting level of the Plans tree.
const PLAN_INDENT_STEP: usize = 2;

/// Nesting level of a plan header row.
const PLAN_HEADER_LEVEL: usize = 0;

/// Nesting level of a section header (`CONCEPT`, `IMPLEMENTATION`) row.
///
/// A section header is a child of its plan header, so it sits exactly one level
/// below the plan.
const PLAN_SECTION_LEVEL: usize = 1;

/// Leading indentation of a node rendered at `level`.
///
/// Every hierarchical row derives its leading indent from its numeric depth
/// through this single helper, so a parent row, its body, and its children can
/// never drift apart and no renderer needs a literal whitespace run
/// (Conventions §4.8).
fn plan_indent(level: usize) -> String {
    " ".repeat(PLAN_INDENT_STEP * level)
}

/// Leading indentation of the detail block of a node rendered at `level`.
///
/// A summary row starts with a one-column selection gutter, while a detail
/// block owns no gutter of its own. The gutter column is therefore reproduced
/// as a leading space, so a node's body lines up exactly with the rows around
/// it and the body never reads as shallower than its children.
fn plan_body_indent(level: usize) -> String {
    format!(" {}", plan_indent(level))
}

/// Cache key of the body of the plan header description of `plan_id`.
///
/// The `p:` prefix and the `:d` suffix keep a plan body disjoint from the
/// two-segment step keys (`{plan}:{item}`) and the `c:`-prefixed concept keys
/// that share the same cache.
fn plan_description_key(plan_id: &str) -> String {
    format!("p:{}:d", plan_id)
}

/// Cache key of the body of the concept item `item_id` in `plan_id`.
///
/// The `c:` prefix keeps a concept body apart from a step body that reuses the
/// same identifier, mirroring the expanded-item keys the view state already
/// carries (c-1).
fn concept_body_key(plan_id: &str, item_id: &str) -> String {
    format!("c:{}:{}", plan_id, item_id)
}

/// Cache key of the body of the step `item_id` in `plan_id` (c-1).
fn step_body_key(plan_id: &str, item_id: &str) -> String {
    format!("{}:{}", plan_id, item_id)
}

// A step badge is one implementation-step projection, so the Plans tree
// reuses the single shared palette mapping instead of restating it (c-5,
// Conventions §8.4).
pub(crate) use crate::views::implementation::status_style;

/// Compact indicator marking the row anchored by the active discussion focus.
fn focus_badge() -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Span::styled(
        " [FOCUS]",
        engine
            .bg(palette.status_success)
            .fg(engine.color(palette.text_inverse))
            .bold(),
    )
}

/// Compact badge naming the Architect configuration that produced a
/// decomposition.
///
/// A plan and a review finding both record which Architect model, at which
/// reasoning depth, decomposed them. The badge marks the provenance kind and
/// prints the model through the single interface formatter, so the alias, or
/// the provider name without its transport prefix, and the reasoning depth are
/// never re-derived here (c-1, c-2). Plans and Review share this one helper so
/// the two surfaces can never name the provenance differently.
pub(crate) fn architect_badge(tier: &ArchitectTier) -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    let label = format!(
        " [ARC] {}",
        crate::model_display::model_label(&tier.model, tier.effort)
    );
    Span::styled(label, engine.fg(palette.status_error))
}

/// Extends a selected row line to the full content width with its fill style.
fn pad_line_to_width(mut line: Line<'static>, width: usize, fill_style: Style) -> Line<'static> {
    let current_width: usize = line.spans.iter().map(|s| s.width()).sum();
    if current_width < width {
        line.spans
            .push(Span::styled(" ".repeat(width - current_width), fill_style));
    }
    line
}

/// Normalizes a Markdown detail block before rendering.
///
/// The first line keeps its original indentation, while every following line
/// sheds the smallest common leading indent, so a block quoted from a document
/// renders flush instead of drifting to the right.
pub(crate) fn prepare_plan_markdown(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() <= 1 {
        return trimmed.to_string();
    }

    let min_tail_indent = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);

    if min_tail_indent > 0 {
        let prefix = " ".repeat(min_tail_indent);
        let mut out = String::with_capacity(trimmed.len());
        out.push_str(lines[0]);
        for line in &lines[1..] {
            out.push('\n');
            // Whitespace-only lines (including multi-byte characters such
            // as NBSP U+00A0 or U+3000) are excluded from the indent
            // computation, so they must never be sliced by a byte offset.
            if line.trim().is_empty() {
                continue;
            }
            if let Some(stripped) = line.strip_prefix(prefix.as_str()) {
                out.push_str(stripped);
            } else {
                out.push_str(line.trim_start());
            }
        }
        out
    } else {
        trimmed.to_string()
    }
}

/// Whether the row at `index` opens a new plan, so it is preceded by the blank
/// separator that visually separates two plans.
///
/// The renderer and the viewport measurement consult this single predicate, so a
/// measured row height can never disagree with the lines actually drawn (c-2).
fn plan_row_gap(index: usize, row: &VisiblePlanRow) -> bool {
    index > 0 && matches!(row, VisiblePlanRow::PlanHeader { .. })
}

/// Compact badge naming the git commit a finished node recorded.
///
/// The hash is abbreviated the way the Git tooling abbreviates it — the first
/// seven characters — so a row names the change it produced without widening the
/// line. The Plans steps and the Review findings resolve the same badge, so the
/// two surfaces can never abbreviate one commit differently (c-1, c-4).
pub(crate) fn commit_badge(hash: &str) -> Option<Span<'static>> {
    let short: String = hash.trim().chars().take(7).collect();
    if short.is_empty() {
        return None;
    }
    let engine = current_engine();
    Some(Span::styled(
        format!(" {}", short),
        engine.fg(engine.palette().status_success),
    ))
}

/// Identity and lifecycle of one plan step rendered by the shared row renderer.
///
/// The Plans tree and the plans embedded in the Review tree both render
/// implementation steps, so a step is described once here and drawn once by
/// [`render_plan_step`]: a surface supplies the values it projects and never
/// restates the layout (c-1).
pub(crate) struct PlanStepRow<'a> {
    /// Identifier printed after `#`.
    pub item_id: &'a str,
    /// Title printed after the identifier.
    pub title: &'a str,
    /// Lifecycle status driving the status badge and the fold icon.
    pub status: PlanItemStatus,
    /// Git commit the step recorded when it finished, if any.
    pub commit: Option<&'a str>,
    /// Diagnostic report of the semantic blocker that stopped the step.
    pub blocked_reason: Option<&'a str>,
    /// Markdown details of the step, rendered as the body of an expanded row.
    pub details: Option<&'a str>,
    /// Whether the step owns nested child steps.
    pub has_children: bool,
    /// Whether the row can be folded.
    pub is_expandable: bool,
    /// Whether the row is unfolded.
    pub is_expanded: bool,
    /// Whether the active discussion focus anchors on this row.
    pub focused: bool,
}

/// File and reference metadata printed around one plan step.
///
/// Every list is taken exactly as the calling surface projects it: a surface
/// that does not carry one of them passes an empty slice, and the corresponding
/// line simply never appears.
pub(crate) struct PlanStepRefs<'a> {
    /// Files the step modifies.
    pub files: &'a [String],
    /// Read-only contracts the step must rely on but never modify.
    pub readonly_files: &'a [String],
    /// Project-declared `[[outer_docs]]` entries the step is implemented against.
    pub outer_docs: &'a [String],
    /// Concept nodes the step implements.
    pub concept_refs: &'a [String],
    /// Other steps this step depends on.
    pub depends_on: &'a [String],
}

/// Placement of one plan step row inside the tree that renders it.
///
/// Indentation is a property of the tree, not of the step, so the nesting scale
/// of each view is passed in rather than assumed here (Conventions §4.8).
pub(crate) struct PlanStepContext<'a> {
    /// Leading indentation of the summary row, without the selection gutter.
    pub indent: &'a str,
    /// Leading indentation of the detail block of the row.
    pub body_indent: &'a str,
    /// Key of the rendered detail body inside the shared Markdown cache.
    pub cache_key: &'a str,
    /// Width the detail block is wrapped to.
    pub width: usize,
    /// Active light/dark mode the Markdown body is rendered with.
    pub theme_mode: crate::config::ThemeMode,
    /// Fill style of the row, carrying the selection surface when selected.
    pub row_style: Style,
    /// Whether the row is the selected one.
    pub is_selected: bool,
}

/// Resolves a localized metadata label and keeps its trailing separator.
fn metadata_label(key: &str) -> String {
    format!("{} ", crate::i18n::active_t(key))
}

/// Fills the placeholder of a localized count template with one number.
fn metadata_count(key: &str, count: usize) -> String {
    crate::i18n::active_t(key).replacen("{}", &count.to_string(), 1)
}

/// Emits one labelled metadata line of a step body.
///
/// The label and its values are separate spans so each keeps its own semantic
/// role, and the body prefix is repeated on every line so a long value keeps one
/// left margin (Conventions §4.8, §8.4).
fn push_metadata<V: TreeRowSink>(
    sink: &mut V,
    prefix: &Span<'static>,
    label: String,
    values: &[String],
    value_style: Style,
    width: usize,
) {
    let engine = current_engine();
    let muted = engine.fg(engine.palette().text_muted);
    let line = Line::from(vec![
        prefix.clone(),
        Span::styled(label, muted),
        Span::styled(values.join(", "), value_style),
    ]);
    sink.summary(line, width);
}

/// Renders one plan step row and, while it is expanded, its detail block.
///
/// The row is described once by [`PlanStepRow`] and placed once by
/// [`PlanStepContext`], so the Plans tree and the plans embedded in the Review
/// tree draw an implementation step identically: the same fold icon, the same
/// status badge, the same file and document badges, the same short commit hash,
/// the same blocker block, and the same cached Markdown body. A surface supplies
/// only the values it projects and never restates the layout (c-1, c-3).
pub(crate) fn render_plan_step<V: TreeRowSink>(
    sink: &mut V,
    row: &PlanStepRow<'_>,
    refs: &PlanStepRefs<'_>,
    context: &PlanStepContext<'_>,
    cache: &mut MarkdownBodyCache,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let width = context.width;
    let muted = engine.fg(palette.text_muted);
    let info = engine.fg(palette.status_info);

    let fold_icon = if row.is_expandable {
        if row.is_expanded {
            "▼ "
        } else {
            "▶ "
        }
    } else if context.is_selected {
        "▶ "
    } else if row.status == PlanItemStatus::Discussion {
        "? "
    } else if row.status == PlanItemStatus::Blocked {
        "! "
    } else {
        "• "
    };

    let fold_style = if row.is_expandable {
        engine.fg(palette.status_warning).bold()
    } else if context.is_selected {
        info.bold()
    } else {
        muted
    };

    let title_style = if context.is_selected {
        engine.fg(palette.text_primary).bold()
    } else if row.status == PlanItemStatus::Done || row.status == PlanItemStatus::Cancelled {
        engine.fg(palette.text_secondary)
    } else {
        Style::default().bold()
    };

    let clean_title = row.title.replace(['\r', '\n'], " ").trim().to_string();
    let gutter = if context.is_selected {
        Span::styled("▌", info.bold())
    } else {
        Span::raw(" ")
    };

    let mut spans = vec![
        gutter,
        Span::raw(context.indent.to_string()),
        Span::styled(fold_icon, fold_style),
        Span::styled(format!(" {} ", row.status), status_style(row.status)),
        Span::raw(" "),
        Span::styled(format!("#{} {}", row.item_id, clean_title), title_style),
    ];

    if !row.has_children {
        let mut badges = Vec::new();
        if !refs.files.is_empty() {
            badges.push(metadata_count("plans.file_count", refs.files.len()));
        }
        if !refs.readonly_files.is_empty() {
            badges.push(metadata_count("plans.ref_count", refs.readonly_files.len()));
        }
        if !refs.outer_docs.is_empty() {
            badges.push(metadata_count("plans.docs_count", refs.outer_docs.len()));
        }
        if !badges.is_empty() {
            spans.push(Span::styled(format!(" [{}]", badges.join(" + ")), muted));
        }
    }
    if row.status == PlanItemStatus::Done {
        if let Some(badge) = row.commit.and_then(commit_badge) {
            spans.push(badge);
        }
    }
    if row.focused {
        spans.push(focus_badge());
    }

    let mut summary_line = Line::from(spans).style(context.row_style);
    if context.is_selected {
        summary_line = pad_line_to_width(summary_line, width, context.row_style);
    }
    sink.summary(summary_line, width);

    if !row.is_expanded {
        return;
    }

    let body_prefix = format!("{}│ ", context.body_indent);
    let meta_prefix = Span::styled(body_prefix.clone(), muted);

    if !row.has_children && !refs.files.is_empty() {
        let label = metadata_label("plans.files_prefix");
        push_metadata(sink, &meta_prefix, label, refs.files, info, width);
    }
    if !row.has_children && !refs.readonly_files.is_empty() {
        let label = metadata_label("plans.ref_prefix");
        push_metadata(sink, &meta_prefix, label, refs.readonly_files, info, width);
    }
    if !row.has_children && !refs.outer_docs.is_empty() {
        let label = metadata_label("plans.docs_prefix");
        push_metadata(sink, &meta_prefix, label, refs.outer_docs, info, width);
    }
    if !refs.depends_on.is_empty() {
        let label = crate::i18n::active_t("review.depends_on_label");
        let style = engine.fg(palette.status_warning);
        push_metadata(sink, &meta_prefix, label, refs.depends_on, style, width);
    }
    if !refs.concept_refs.is_empty() {
        let label = crate::i18n::active_t("plans.concept_prefix");
        let style = engine.fg(palette.button_active_bg);
        push_metadata(sink, &meta_prefix, label, refs.concept_refs, style, width);
    }

    if let Some(reason) = row.blocked_reason {
        let prepared = prepare_plan_markdown(reason);
        if !prepared.is_empty() {
            // The blocker is the step's most important detail, so it leads the
            // body of a blocked step and keeps the semantic error role
            // (Conventions §8.4).
            let reason_style = engine.fg(palette.status_error);
            let blocked_label = crate::i18n::active_t("plans.blocked_prefix");
            let lead_prefix = format!("{}{} ", body_prefix, blocked_label);
            for (index, text) in prepared.lines().enumerate() {
                let lead = if index == 0 {
                    Span::styled(lead_prefix.clone(), reason_style.bold())
                } else {
                    Span::styled(body_prefix.clone(), muted)
                };
                let line = Line::from(vec![lead, Span::styled(text.to_string(), reason_style)]);
                sink.summary(line, width);
            }
        }
    }

    if let Some(details) = row.details {
        let prepared = prepare_plan_markdown(details);
        if !prepared.is_empty() {
            let mut theme = crate::markdown::MarkdownTheme::answer_themed(context.theme_mode);
            theme.text_style = engine.fg(palette.text_secondary);
            theme.hard_breaks = true;
            theme.compact = true;
            theme.line_prefix = Some(Span::styled(body_prefix, muted));
            theme.constrain_table_width(width);
            let key = context.cache_key.to_string();
            let body = cache.lines_for(&key, &prepared, &theme, Some(width), context.theme_mode);
            sink.body(body);
        }
    }
}

/// Receives the rows of a hierarchical tree in the order the shared row walk
/// emits them.
///
/// The Plans and Review trees both render and measure their rows through this
/// one contract: the walk owns the row order, the blank separator between
/// top-level nodes, and the match over every row variant, while a sink only
/// decides what to do with each piece it is handed — materialize it, or count
/// it. The shared step renderer draws into the same contract, so a step row is
/// built once for both surfaces (c-1, c-2).
pub(crate) trait TreeRowSink {
    /// Emits the blank separator that opens a new top-level node.
    fn separator(&mut self);

    /// Records the first rendered line of the row that follows.
    fn row_start(&mut self);

    /// Emits one row-summary line, which still has to be wrapped to `width`.
    fn summary(&mut self, line: Line<'static>, width: usize);

    /// Emits a detail body the Markdown cache already wrapped to the target
    /// width, so its lines are taken exactly as they are.
    fn body(&mut self, lines: &[Line<'static>]);
}

/// Rendering visitor: materializes the wrapped lines and the row offsets.
#[derive(Default)]
struct PlanLineRenderer {
    lines: Vec<Line<'static>>,
    offsets: Vec<usize>,
}
impl TreeRowSink for PlanLineRenderer {
    fn separator(&mut self) {
        self.lines.push(Line::raw(""));
    }

    fn row_start(&mut self) {
        self.offsets.push(self.lines.len());
    }

    fn summary(&mut self, line: Line<'static>, width: usize) {
        self.lines.extend(wrap_line(line, width));
    }

    fn body(&mut self, lines: &[Line<'static>]) {
        self.lines.extend(lines.iter().cloned());
    }
}

/// Measuring visitor: reproduces the row offsets and the total height of the
/// tree without materializing a single line.
#[derive(Default)]
struct PlanLineMeasure {
    offsets: Vec<usize>,
    height: usize,
}

impl TreeRowSink for PlanLineMeasure {
    fn separator(&mut self) {
        self.height += 1;
    }

    fn row_start(&mut self) {
        self.offsets.push(self.height);
    }

    fn summary(&mut self, line: Line<'static>, width: usize) {
        self.height += wrapped_line_count(&line, width);
    }

    fn body(&mut self, lines: &[Line<'static>]) {
        self.height += lines.len();
    }
}

/// Renders the flattened Plans rows into lines and their row offsets.
///
/// Each row contributes its summary line plus, when expanded, a detail block
/// that renders at the level of its children. The returned offsets map a row
/// index onto its first rendered line, so scrolling and selection stay aligned
/// with the wrapping of a variable-height tree.
///
/// The Markdown detail blocks of concepts and steps come from `cache` instead of
/// being parsed afresh: a frame that only moved the cursor or scrolled reuses the
/// lines already rendered for the unchanged description, while a changed
/// description, indent, width, or palette re-renders exactly that one body
/// (c-1, c-3).
pub(crate) fn compute_unified_plan_lines(
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut renderer = PlanLineRenderer::default();
    walk_plan_rows(rows, selected_idx, width, theme_mode, cache, &mut renderer);
    (renderer.lines, renderer.offsets)
}

/// Measures the row offsets and the total height of the flattened Plans rows
/// without materializing a single line.
///
/// Every break decision is shared with [`compute_unified_plan_lines`] through one
/// row walk, and the short summary lines are counted through
/// [`wrapped_line_count`] instead of being wrapped into owned lines. A cached
/// detail body already arrives wrapped to the target width, so it contributes its
/// own length. Measured offsets therefore match the rendered frame exactly while
/// positioning the viewport costs no line allocation (c-2).
pub(crate) fn measure_plan_rows(
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache,
) -> (Vec<usize>, usize) {
    let mut measure = PlanLineMeasure::default();
    walk_plan_rows(rows, selected_idx, width, theme_mode, cache, &mut measure);
    (measure.offsets, measure.height)
}

/// Walks the visible rows in render order and hands every rendered piece to
/// `visitor`.
///
/// This walk is the single source of truth for the shape of the tree: it emits
/// the separator between plans, records where each row starts, builds every
/// summary line, and resolves every detail body through the Markdown cache.
/// Rendering and measuring therefore observe one identical layout, and no
/// consumer can drift from another (c-2).
fn walk_plan_rows<V: TreeRowSink>(
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache,
    visitor: &mut V,
) {
    let engine = current_engine();
    let palette = engine.palette();

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        // The blank separator between plans belongs to the gap before a plan
        // header, so it is emitted before the offset of that row is recorded.
        // Otherwise the offset anchors the gap and scrolling stops one line
        // short of the header it is meant to reveal (c-offset-accuracy).
        if plan_row_gap(idx, row) {
            visitor.separator();
        }
        visitor.row_start();

        let row_style = if is_selected {
            engine.bg(palette.overlay)
        } else {
            Style::default()
        };

        let gutter = if is_selected {
            Span::styled("▌", engine.fg(palette.status_info).bold())
        } else {
            Span::raw(" ")
        };

        match row {
            VisiblePlanRow::PlanHeader {
                plan_id,
                title,
                description,
                total,
                completed,
                status,
                is_expanded,
                focused,
                architect,
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = crate::i18n::active_t("plans.done_progress")
                    .replacen("{}", &completed.to_string(), 1)
                    .replacen("{}", &total.to_string(), 1);

                let mut spans = vec![
                    gutter,
                    Span::raw(plan_indent(PLAN_HEADER_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(format!(" {} ", status), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(
                        format!("[{}] ", plan_id),
                        engine.fg(palette.status_info).bold(),
                    ),
                    Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
                    Span::styled(
                        format!(" ({})", stats_str),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                if *focused {
                    spans.push(focus_badge());
                }
                if let Some(tier) = architect {
                    spans.push(architect_badge(tier));
                }

                let mut header_line = Line::from(spans).style(row_style);
                if is_selected {
                    header_line = pad_line_to_width(header_line, width, row_style);
                }
                visitor.summary(header_line, width);

                if *is_expanded {
                    if let Some(desc) = description {
                        let prepared = prepare_plan_markdown(desc);
                        if !prepared.is_empty() {
                            // The plan body renders at the level of its
                            // children, never deeper (Conventions §4.8).
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}│ ", plan_body_indent(PLAN_HEADER_LEVEL + 1)),
                                engine.fg(palette.text_muted),
                            ));
                            theme.constrain_table_width(width);
                            let key = plan_description_key(plan_id);
                            let body =
                                cache.lines_for(&key, &prepared, &theme, Some(width), theme_mode);
                            visitor.body(body);
                        }
                    }
                }
            }
            VisiblePlanRow::ConceptHeader {
                count, is_expanded, ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let spans = vec![
                    gutter,
                    Span::raw(plan_indent(PLAN_SECTION_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.button_active_bg).bold()),
                    Span::styled(
                        format!(
                            " {} ",
                            crate::i18n::active_t("plans.concept_title").to_uppercase()
                        ),
                        engine
                            .bg(palette.button_active_bg)
                            .fg(engine.color(palette.text_inverse))
                            .bold(),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        crate::i18n::active_t("plans.concept_subtitle"),
                        engine.fg(palette.text_primary).bold(),
                    ),
                    Span::styled(
                        format!(
                            " ({})",
                            crate::i18n::active_t("plans.concept_item_count").replacen(
                                "{}",
                                &count.to_string(),
                                1
                            )
                        ),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                let mut line = Line::from(spans).style(row_style);
                if is_selected {
                    line = pad_line_to_width(line, width, row_style);
                }
                visitor.summary(line, width);
            }
            VisiblePlanRow::ConceptItem {
                plan_id,
                item_id,
                title,
                details,
                depth,
                is_expandable,
                is_expanded,
                focused,
                ..
            } => {
                let indent = plan_indent(*depth);
                let fold_icon = if *is_expandable {
                    if *is_expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                } else if is_selected {
                    "▶ "
                } else {
                    "◆ "
                };
                let fold_style = if *is_expandable {
                    engine.fg(palette.button_active_bg).bold()
                } else if is_selected {
                    engine.fg(palette.status_info).bold()
                } else {
                    engine.fg(palette.text_muted)
                };
                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else {
                    engine.fg(palette.button_active_bg)
                };
                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!("#{} {}", item_id, clean_title), title_style),
                ];
                if *focused {
                    spans.push(focus_badge());
                }
                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                visitor.summary(row_line, width);

                if *is_expanded {
                    if let Some(det) = details {
                        let prepared = prepare_plan_markdown(det);
                        if !prepared.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                format!("{}│ ", plan_body_indent(*depth + 1)),
                                engine.fg(palette.text_muted),
                            ));
                            theme.constrain_table_width(width);
                            let key = concept_body_key(plan_id, item_id);
                            let body =
                                cache.lines_for(&key, &prepared, &theme, Some(width), theme_mode);
                            visitor.body(body);
                        }
                    }
                }
            }
            VisiblePlanRow::ImplementationHeader {
                total,
                completed,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = crate::i18n::active_t("plans.done_progress")
                    .replacen("{}", &completed.to_string(), 1)
                    .replacen("{}", &total.to_string(), 1);
                let spans = vec![
                    gutter,
                    Span::raw(plan_indent(PLAN_SECTION_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.status_info).bold()),
                    Span::styled(
                        format!(
                            " {} ",
                            crate::i18n::active_t("plans.impl_title").to_uppercase()
                        ),
                        engine
                            .bg(palette.status_info)
                            .fg(engine.color(palette.text_inverse))
                            .bold(),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        crate::i18n::active_t("plans.impl_subtitle"),
                        engine.fg(palette.text_primary).bold(),
                    ),
                    Span::styled(
                        format!(" ({})", stats_str),
                        if is_selected {
                            engine.fg(palette.text_primary)
                        } else {
                            engine.fg(palette.text_muted)
                        },
                    ),
                ];
                let mut line = Line::from(spans).style(row_style);
                if is_selected {
                    line = pad_line_to_width(line, width, row_style);
                }
                visitor.summary(line, width);
            }
            VisiblePlanRow::PlanItem {
                plan_id,
                item_id,
                title,
                details,
                status,
                commit,
                files,
                readonly_files,
                outer_docs,
                concept_refs,
                blocked_reason,
                depth,
                has_children,
                is_expandable,
                is_expanded,
                focused,
                ..
            } => {
                // The step row and its metadata are drawn by the one shared
                // plan-step renderer, so the Plans tree and the plans embedded
                // in the Review tree can never render an implementation step
                // differently (c-1).
                let indent = plan_indent(*depth);
                // The step body renders at the level of its children, never
                // deeper (Conventions §4.8).
                let body_indent = plan_body_indent(*depth + 1);
                let cache_key = step_body_key(plan_id, item_id);
                render_plan_step(
                    visitor,
                    &PlanStepRow {
                        item_id,
                        title,
                        status: *status,
                        commit: commit.as_deref(),
                        blocked_reason: blocked_reason.as_deref(),
                        details: details.as_deref(),
                        has_children: *has_children,
                        is_expandable: *is_expandable,
                        is_expanded: *is_expanded,
                        focused: *focused,
                    },
                    &PlanStepRefs {
                        files,
                        readonly_files,
                        outer_docs,
                        concept_refs,
                        // The Plans projection carries no dependency list of its
                        // own, so it offers none to the shared renderer.
                        depends_on: &[],
                    },
                    &PlanStepContext {
                        indent: &indent,
                        body_indent: &body_indent,
                        cache_key: &cache_key,
                        width,
                        theme_mode,
                        row_style,
                        is_selected,
                    },
                    cache,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::PlansViewState;
    use super::*;
    use tauqe_protocol::{ConceptItem, Plan, PlanItem};

    fn concept_leaf(id: &str, title: &str) -> ConceptItem {
        ConceptItem {
            id: id.to_string(),
            title: title.to_string(),
            details: None,
            children: vec![],
        }
    }

    fn leaf_item(id: &str, title: &str) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            details: None,
            status: PlanItemStatus::Todo,
            blocked_reason: None,
            commit: None,
            files: vec![],
            readonly_files: vec![],
            outer_docs: vec![],
            concept_refs: vec![],
            depends_on: vec![],
            children: vec![],
        }
    }

    fn parent_item(id: &str, title: &str, children: Vec<PlanItem>) -> PlanItem {
        PlanItem {
            children,
            ..leaf_item(id, title)
        }
    }

    /// Leading indent of a rendered line, counting the selection gutter.
    ///
    /// Every row carries a one-column gutter (`▌` when selected, a space
    /// otherwise), so counting that column yields indents that are comparable
    /// no matter where the cursor rests.
    fn leading_indent(line: &Line<'static>) -> usize {
        line.spans
            .iter()
            .flat_map(|span| span.content.chars())
            .take_while(|c| *c == ' ' || *c == '▌')
            .count()
    }

    /// Index of the first rendered line whose concatenated text contains
    /// `needle`.
    ///
    /// A label and its value are rendered as separate spans, so a probe must
    /// match the joined line rather than any single span.
    fn find_line(lines: &[Line<'static>], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .contains(needle)
            })
            .unwrap_or_else(|| panic!("missing rendered line containing {needle}"))
    }

    /// Full text of every rendered line, joined for substring probes.
    fn joined_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders the tree the way the view does, against the view's own body
    /// cache, and returns the lines with their row offsets.
    fn render_view(
        view: &mut PlansViewState,
        selected: usize,
        width: usize,
    ) -> (Vec<Line<'static>>, Vec<usize>) {
        let rows = view.flatten_rows();
        compute_unified_plan_lines(
            &rows,
            selected,
            width,
            view.theme_mode,
            &mut view.markdown_cache,
        )
    }

    /// Body lines of a rendered tree: the detail block lines that carry the `│`
    /// body prefix, as opposed to the summary line of a row.
    fn body_lines(lines: &[Line<'static>]) -> Vec<Line<'static>> {
        lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .contains('│')
            })
            .cloned()
            .collect()
    }

    /// Fixture with one expanded concept body and one expanded step body.
    fn cached_bodies_fixture(step_details: &str, concept_details: &str) -> PlansViewState {
        let mut view = PlansViewState {
            plans: vec![Plan {
                id: "cache-plan".to_string(),
                title: "Cache Plan".to_string(),
                description: Some("One coherent change set per bounded turn.".to_string()),
                concept: vec![ConceptItem {
                    details: Some(concept_details.to_string()),
                    ..concept_leaf("c-1", "Invariant")
                }],
                implementation: tauqe_protocol::PlanImplementation {
                    items: vec![PlanItem {
                        details: Some(step_details.to_string()),
                        ..leaf_item("1", "Step")
                    }],
                    ..Default::default()
                },
                ..Default::default()
            }],
            content_width: 200,
            ..Default::default()
        };
        view.expand_all();
        view
    }

    #[test]
    fn test_navigation_reuses_cached_bodies_and_keeps_their_lines() {
        let mut view = cached_bodies_fixture("Implement the signer.", "Tokens must be signed.");
        let step_row = view
            .flatten_rows()
            .iter()
            .position(|row| matches!(row, VisiblePlanRow::PlanItem { .. }))
            .expect("the step row is visible");

        let (first, _) = render_view(&mut view, 0, 200);
        let first_text = joined_text(&first);
        assert!(
            first_text.contains("One coherent change set per bounded turn."),
            "the plan header body must render: {first_text}"
        );
        assert!(
            first_text.contains("Tokens must be signed."),
            "the concept body must render: {first_text}"
        );
        assert!(
            first_text.contains("Implement the signer."),
            "the step body must render: {first_text}"
        );
        assert_eq!(
            view.markdown_cache.len(),
            3,
            "one cached body per rendered description"
        );
        let renders_after_first_pass = view.markdown_cache.render_count();

        // Moving the cursor re-renders every row but must reuse all three bodies.
        let (second, _) = render_view(&mut view, step_row, 200);
        assert_eq!(
            view.markdown_cache.len(),
            3,
            "navigation must neither add nor drop a cached body"
        );
        assert_eq!(
            view.markdown_cache.render_count(),
            renders_after_first_pass,
            "navigation must not re-parse a body it already rendered"
        );
        assert_eq!(
            body_lines(&second),
            body_lines(&first),
            "navigation must not change a rendered body"
        );
    }

    #[test]
    fn test_width_change_rerenders_the_cached_bodies() {
        let mut view = cached_bodies_fixture(
            "Implement the signer over a rotating key so verification stays deterministic.",
            "Tokens must be signed.",
        );

        let (wide, _) = render_view(&mut view, usize::MAX, 200);
        let (narrow, _) = render_view(&mut view, usize::MAX, 40);
        assert_eq!(
            view.markdown_cache.len(),
            3,
            "a new wrap width drops and re-renders every body"
        );
        assert!(
            body_lines(&narrow).len() > body_lines(&wide).len(),
            "the narrow body must wrap into more lines"
        );

        // Returning to the original width re-renders the identical lines.
        let (wide_again, _) = render_view(&mut view, usize::MAX, 200);
        assert_eq!(body_lines(&wide_again), body_lines(&wide));
    }

    #[test]
    fn test_rewritten_description_rerenders_its_own_body() {
        let mut view = cached_bodies_fixture("First wording.", "Tokens must be signed.");

        let (before, _) = render_view(&mut view, usize::MAX, 200);
        assert!(joined_text(&before).contains("First wording."));

        let plan = view.plans.first_mut().expect("the fixture plan is present");
        let step = plan
            .implementation
            .items
            .first_mut()
            .expect("the fixture step is present");
        step.details = Some("Second wording.".to_string());

        let (after, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&after);
        assert!(text.contains("Second wording."), "{text}");
        assert!(!text.contains("First wording."), "{text}");
        assert_eq!(
            view.markdown_cache.len(),
            3,
            "a rewritten description replaces its own body"
        );
    }

    #[test]
    fn test_file_badges_render_only_for_leaf_items() {
        let mut view = PlansViewState::default();
        let plan = Plan::from_parts(
            "leaf-files".to_string(),
            "Leaf Files".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Phase".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec!["parent.rs".to_string()],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Leaf Task".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    blocked_reason: None,
                    commit: None,
                    files: vec!["leaf.rs".to_string()],
                    readonly_files: vec![],
                    outer_docs: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
            }],
        );
        view.plans = vec![plan];
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);

        assert!(
            text.contains("Leaf Task"),
            "leaf row should render: {}",
            text
        );
        // Exactly one file badge: the leaf task. The parent phase carries a
        // non-empty `files` list in memory but must never render a badge.
        assert_eq!(
            text.matches("file(s)]").count(),
            1,
            "only leaf rows may show file badges: {}",
            text
        );
    }

    #[test]
    fn test_reference_files_badge_and_details() {
        let mut view = PlansViewState::default();
        let plan = Plan::from_parts(
            "ref-files".to_string(),
            "Reference Files".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Wire Auth".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec!["src/auth.rs".to_string()],
                readonly_files: vec![
                    "contracts/api.md".to_string(),
                    "contracts/types.rs".to_string(),
                ],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        );
        view.plans = vec![plan];
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);

        assert!(
            text.contains("[1 file(s) + 2 ref(s)]"),
            "badge should count target and reference files: {}",
            text
        );
        assert!(
            text.contains("files: src/auth.rs"),
            "target files line: {}",
            text
        );
        assert!(
            text.contains("ref: contracts/api.md, contracts/types.rs"),
            "reference files line: {}",
            text
        );
    }

    /// A blocked step reveals its blocker reason once expanded, renders that
    /// reason with the semantic error style, and hides it again while folded.
    #[test]
    fn test_blocked_step_renders_its_reason_when_expanded() {
        let mut item = leaf_item("1", "Wire Auth");
        item.status = PlanItemStatus::Blocked;
        item.blocked_reason = Some("Invariant c-model cannot be satisfied.".to_string());

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "blocked-plan".to_string(),
                "Blocked Plan".to_string(),
                None,
                0,
                0,
                vec![],
                vec![item],
            )],
            ..Default::default()
        };
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);
        assert!(
            text.contains("blocked: Invariant c-model cannot be satisfied."),
            "the reason line must render with its catalog prefix: {text}"
        );

        let engine = current_engine();
        let palette = engine.palette();
        let reason_line = &lines[find_line(&lines, "Invariant c-model cannot be satisfied.")];
        let reason_text: String = reason_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            reason_line
                .spans
                .iter()
                .any(|span| span.style.fg == Some(engine.color(palette.status_error))),
            "the reason must use the semantic error style: {reason_text}"
        );

        // Folding the step hides its reason again.
        view.toggle_item_expanded("blocked-plan", "1");
        let (folded, _) = render_view(&mut view, usize::MAX, 200);
        assert!(
            !joined_text(&folded).contains("Invariant c-model cannot be satisfied."),
            "a folded step must hide its blocker reason"
        );
    }

    /// A blocked step carries no description or file metadata, so the reason
    /// alone must make the step expandable; otherwise it could never be read.
    #[test]
    fn test_blocked_step_without_details_is_expandable() {
        let mut item = leaf_item("1", "Blocked Step");
        item.status = PlanItemStatus::Blocked;
        item.blocked_reason = Some("Waiting on an external contract.".to_string());

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "expandable-plan".to_string(),
                "Expandable Plan".to_string(),
                None,
                0,
                0,
                vec![],
                vec![item],
            )],
            ..Default::default()
        };
        view.expand_all();

        let rows = view.flatten_rows();
        assert!(
            rows.iter().any(|row| matches!(
                row,
                VisiblePlanRow::PlanItem {
                    item_id,
                    is_expandable: true,
                    ..
                } if item_id == "1"
            )),
            "a blocked step must be expandable so its reason can be revealed"
        );
    }

    #[test]
    fn test_prepare_plan_markdown_does_not_slice_multibyte_whitespace() {
        // A line made only of NBSP (U+00A0) is Unicode whitespace and is
        // excluded from the indent computation, but its two-byte width
        // must never be sliced by a one-byte offset.
        let raw = "Title\n line one\n\u{00A0}\n line three";
        let prepared = prepare_plan_markdown(raw);
        assert_eq!(prepared, "Title\nline one\n\nline three");
    }

    #[test]
    fn test_focus_badge_marks_the_anchored_plan_item() {
        let mut view = PlansViewState::default();
        let plan = Plan::from_parts(
            "focus-plan".to_string(),
            "Focus Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec![],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        );
        view.plans = vec![plan];
        view.expand_all();
        view.discussion_focus =
            tauqe_protocol::DiscussionFocus::plan("focus-plan", vec!["1".to_string()]);

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisiblePlanRow::PlanHeader { focused: false, .. }
        ));
        assert!(rows.iter().any(|r| matches!(
            r,
            VisiblePlanRow::PlanItem { item_id, focused: true, .. } if item_id == "1"
        )));

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);
        assert!(
            text.contains("[FOCUS]"),
            "focus badge should render: {}",
            text
        );
    }

    #[test]
    fn test_plan_header_renders_the_architect_provenance() {
        let mut view = PlansViewState {
            plans: vec![{
                let mut plan = Plan::from_parts(
                    "arch-plan".to_string(),
                    "Arch Plan".to_string(),
                    None,
                    0,
                    0,
                    vec![],
                    vec![],
                );
                plan.implementation.architect = Some(tauqe_protocol::ArchitectTier::new(
                    tauqe_protocol::ModelRef::openrouter("planner/model"),
                    Some(tauqe_protocol::ReasoningEffort::High),
                ));
                plan
            }],
            ..Default::default()
        };

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisiblePlanRow::PlanHeader {
                architect: Some(_),
                ..
            }
        ));

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let rendered = joined_text(&lines);
        assert!(
            rendered.contains("[ARC] planner/model [high]"),
            "the header must name the Architect model and depth: {rendered}"
        );
    }

    /// The blank separator between two plans belongs to the gap before the
    /// second header, so the recorded offset of every plan points straight at
    /// its header line instead of the empty row above it (c-offset-accuracy).
    #[test]
    fn test_plan_offsets_point_at_their_header_not_the_separator_gap() {
        let mut view = PlansViewState {
            plans: vec![
                Plan::from_parts(
                    "plan-a".to_string(),
                    "Plan A".to_string(),
                    None,
                    0,
                    0,
                    vec![],
                    vec![],
                ),
                Plan::from_parts(
                    "plan-b".to_string(),
                    "Plan B".to_string(),
                    None,
                    0,
                    0,
                    vec![],
                    vec![],
                ),
            ],
            ..Default::default()
        };

        let rows = view.flatten_rows();
        let headers: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, VisiblePlanRow::PlanHeader { .. }))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(headers.len(), 2, "both plan headers are visible");

        let (lines, offsets) = render_view(&mut view, usize::MAX, 200);
        assert_eq!(offsets.len(), rows.len());

        // The first plan starts the document, so its offset is line zero.
        assert_eq!(offsets[headers[0]], 0);

        // The second plan's offset skips the separator and anchors its header.
        let second = offsets[headers[1]];
        assert!(
            lines[second].to_string().contains("[plan-b]"),
            "the offset must anchor the header line: {}",
            lines[second]
        );
        assert!(
            lines[second - 1].to_string().trim().is_empty(),
            "the separator must sit above the anchored header: {}",
            lines[second - 1]
        );
    }

    #[test]
    fn test_hierarchy_indent_strictly_increases_and_body_stays_within_children() {
        let mut parent = parent_item("1", "Parent Step", vec![leaf_item("1.1", "Leaf Step")]);
        parent.details = Some("Parent details.".to_string());

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "indent-plan".to_string(),
                "Indent Plan".to_string(),
                Some("Plan description.".to_string()),
                0,
                0,
                vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "Invariant".to_string(),
                    details: Some("Tokens must be signed.".to_string()),
                    children: vec![concept_leaf("c-1.1", "Nested Invariant")],
                }],
                vec![parent],
            )],
            ..Default::default()
        };
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);

        let header = leading_indent(&lines[find_line(&lines, "[indent-plan]")]);
        let concept_section = leading_indent(&lines[find_line(&lines, "Architectural Intent")]);
        let impl_section = leading_indent(&lines[find_line(&lines, "Execution Blueprint")]);
        let concept = leading_indent(&lines[find_line(&lines, "#c-1 Invariant")]);
        let nested_concept = leading_indent(&lines[find_line(&lines, "#c-1.1 Nested Invariant")]);
        let step = leading_indent(&lines[find_line(&lines, "#1 Parent Step")]);
        let leaf = leading_indent(&lines[find_line(&lines, "#1.1 Leaf Step")]);

        // Every nested node renders strictly deeper than its visual parent.
        assert!(
            header < concept_section,
            "a section must sit deeper than the plan header: {header} >= {concept_section}"
        );
        assert!(
            concept_section < concept,
            "a concept must sit deeper than its section: {concept_section} >= {concept}"
        );
        assert!(
            concept < nested_concept,
            "a nested concept must sit deeper than its parent: {concept} >= {nested_concept}"
        );
        assert_eq!(
            concept_section, impl_section,
            "both sections are siblings and share one level"
        );
        assert!(
            impl_section < step,
            "a step must sit deeper than its section: {impl_section} >= {step}"
        );
        assert!(
            step < leaf,
            "a nested step must sit deeper than its parent: {step} >= {leaf}"
        );

        // A node body occupies the level of its children, never a deeper one.
        let plan_body = leading_indent(&lines[find_line(&lines, "Plan description.")]);
        assert_eq!(
            plan_body, concept_section,
            "the plan description must sit at the level of its sections"
        );
        let concept_body = leading_indent(&lines[find_line(&lines, "Tokens must be signed.")]);
        assert_eq!(
            concept_body, nested_concept,
            "a concept body must sit at the level of its children"
        );
        let step_body = leading_indent(&lines[find_line(&lines, "Parent details.")]);
        assert_eq!(
            step_body, leaf,
            "a step body must sit at the level of its children"
        );
    }

    #[test]
    fn test_hierarchy_indent_covers_collapsed_and_expanded_states() {
        let mut parent = parent_item("1", "Parent Step", vec![leaf_item("1.1", "Leaf Step")]);
        parent.details = Some("Parent details.".to_string());

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "state-plan".to_string(),
                "State Plan".to_string(),
                Some("Plan description.".to_string()),
                0,
                0,
                vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "Invariant".to_string(),
                    details: Some("Tokens must be signed.".to_string()),
                    children: vec![concept_leaf("c-1.1", "Nested Invariant")],
                }],
                vec![parent],
            )],
            ..Default::default()
        };

        // A collapsed plan exposes only its header, anchored at the root level.
        let rows = view.flatten_rows();
        assert_eq!(rows.len(), 1, "a collapsed plan exposes only its header");
        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let rendered = joined_text(&lines);
        assert!(
            !rendered.contains("Invariant"),
            "a collapsed plan hides its items: {rendered}"
        );
        let header = leading_indent(&lines[find_line(&lines, "[state-plan]")]);
        assert_eq!(header, 1, "the plan header sits at the root level");

        // Expanding the plan while folding both sections reveals the section
        // headers only; every child stays hidden and strictly deeper than the
        // plan header.
        view.toggle_plan_expanded("state-plan");
        view.toggle_concept_section_expanded("state-plan");
        view.toggle_impl_section_expanded("state-plan");
        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let header = leading_indent(&lines[find_line(&lines, "[state-plan]")]);
        let concept_section = leading_indent(&lines[find_line(&lines, "Architectural Intent")]);
        let impl_section = leading_indent(&lines[find_line(&lines, "Execution Blueprint")]);
        assert!(header < concept_section, "{header} >= {concept_section}");
        assert_eq!(
            concept_section, impl_section,
            "both sections are siblings and share one level"
        );
        let rendered = joined_text(&lines);
        assert!(
            !rendered.contains("#c-1 "),
            "a folded concept section hides its items: {rendered}"
        );
        assert!(
            !rendered.contains("#1 "),
            "a folded implementation section hides its steps: {rendered}"
        );

        // Fully expanding the tree keeps every child strictly deeper than its
        // parent and every node body at the level of its children.
        view.expand_all();
        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let header = leading_indent(&lines[find_line(&lines, "[state-plan]")]);
        let concept_section = leading_indent(&lines[find_line(&lines, "Architectural Intent")]);
        let concept = leading_indent(&lines[find_line(&lines, "#c-1 Invariant")]);
        let nested_concept = leading_indent(&lines[find_line(&lines, "#c-1.1 Nested Invariant")]);
        let step = leading_indent(&lines[find_line(&lines, "#1 Parent Step")]);
        let leaf = leading_indent(&lines[find_line(&lines, "#1.1 Leaf Step")]);

        assert!(header < concept_section, "{header} >= {concept_section}");
        assert!(concept_section < concept, "{concept_section} >= {concept}");
        assert!(concept < nested_concept, "{concept} >= {nested_concept}");
        assert!(concept_section < step, "{concept_section} >= {step}");
        assert!(step < leaf, "{step} >= {leaf}");

        let plan_body = leading_indent(&lines[find_line(&lines, "Plan description.")]);
        assert_eq!(
            plan_body, concept_section,
            "the plan description must sit at the level of its sections"
        );
        let concept_body = leading_indent(&lines[find_line(&lines, "Tokens must be signed.")]);
        assert_eq!(
            concept_body, nested_concept,
            "a concept body must sit at the level of its children"
        );
        let step_body = leading_indent(&lines[find_line(&lines, "Parent details.")]);
        assert_eq!(
            step_body, leaf,
            "a step body must sit at the level of its children"
        );
    }
    /// A finished step names the commit it recorded in the compact form the Git
    /// tooling prints, while a step that never finished names nothing.
    #[test]
    fn test_finished_step_renders_its_short_commit_hash() {
        let mut done = leaf_item("1", "Done Step");
        done.status = PlanItemStatus::Done;
        done.commit = Some("0123456789abcdef".to_string());

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "commit-plan".to_string(),
                "Commit Plan".to_string(),
                None,
                0,
                0,
                vec![],
                vec![done, leaf_item("2", "Todo Step")],
            )],
            ..Default::default()
        };
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);
        assert!(
            text.contains(" 0123456"),
            "the short hash must render: {text}"
        );
        assert!(
            !text.contains("0123456789abcdef"),
            "the badge must stay short: {text}"
        );
        assert!(
            !text.contains(" #2 Todo Step  0123456"),
            "a step that recorded no commit carries no hash: {text}"
        );
    }
    /// A leaf step implemented against an external specification renders the
    /// document count in its compact badge and the identifiers themselves in
    /// an expanded `docs:` line (c-plans-tree-docs-visibility).
    #[test]
    fn test_outer_docs_badge_and_details() {
        let mut item = leaf_item("1", "Wire Chat");
        item.files = vec!["src/chat.rs".to_string()];
        item.readonly_files = vec!["contracts/api.md".to_string()];
        item.outer_docs = vec![
            "openrouter/chat-completion".to_string(),
            "openrouter/limits".to_string(),
        ];

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "docs-plan".to_string(),
                "Docs Plan".to_string(),
                None,
                0,
                0,
                vec![],
                vec![item],
            )],
            ..Default::default()
        };
        view.expand_all();

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);
        assert!(
            text.contains("[1 file(s) + 1 ref(s) + 2 doc(s)]"),
            "the badge must count the external documents: {text}"
        );
        assert!(
            text.contains("docs: openrouter/chat-completion, openrouter/limits"),
            "the expanded step must list its documents: {text}"
        );
    }

    /// External documentation alone makes a step expandable, so its identifiers
    /// stay readable even when the step carries no description or file metadata.
    #[test]
    fn test_outer_docs_alone_expand_the_step() {
        let mut item = leaf_item("1", "Read the spec");
        item.outer_docs = vec!["openrouter/models".to_string()];

        let mut view = PlansViewState {
            plans: vec![Plan::from_parts(
                "docs-only".to_string(),
                "Docs Only".to_string(),
                None,
                0,
                0,
                vec![],
                vec![item],
            )],
            ..Default::default()
        };
        view.expand_all();

        let rows = view.flatten_rows();
        assert!(
            rows.iter().any(|row| matches!(
                row,
                VisiblePlanRow::PlanItem {
                    item_id,
                    is_expandable: true,
                    ..
                } if item_id == "1"
            )),
            "external documentation must make the step expandable"
        );

        let (lines, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&lines);
        assert!(
            text.contains("[1 doc(s)]"),
            "a docs-only step carries its own badge: {text}"
        );
        assert!(
            text.contains("docs: openrouter/models"),
            "the expanded step must list its documents: {text}"
        );
    }
}
