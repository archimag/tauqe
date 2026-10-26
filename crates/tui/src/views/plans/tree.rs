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
use crate::ui::wrap_lines;

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

/// Filled badge style for a step lifecycle status.
pub(crate) fn status_style(status: PlanItemStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        PlanItemStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        PlanItemStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        PlanItemStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        PlanItemStatus::Done => engine.bg(palette.status_success).fg(inverse).bold(),
        PlanItemStatus::Blocked => engine.bg(palette.status_error).fg(inverse).bold(),
        PlanItemStatus::Cancelled => engine.bg(palette.text_muted).fg(inverse).bold(),
    }
}

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
pub(crate) fn plan_row_gap(index: usize, row: &VisiblePlanRow) -> bool {
    index > 0 && matches!(row, VisiblePlanRow::PlanHeader { .. })
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
///
/// A slice holding a single row renders exactly that row, which is how the view
/// measures the height of one row without materializing the whole tree (c-2).
pub(crate) fn compute_unified_plan_lines(
    rows: &[VisiblePlanRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
    cache: &mut MarkdownBodyCache,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let engine = current_engine();
    let palette = engine.palette();
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
        // The blank separator between plans belongs to the gap before a plan
        // header, so it is emitted before the offset of that row is recorded.
        // Otherwise the offset anchors the gap and scrolling stops one line
        // short of the header it is meant to reveal (c-offset-accuracy).
        if plan_row_gap(idx, row) {
            all_lines.push(Line::raw(""));
        }
        offsets.push(all_lines.len());

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
                all_lines.extend(wrap_lines(vec![header_line], width));

                if *is_expanded {
                    if let Some(desc) = description {
                        let prepared = prepare_plan_markdown(desc);
                        if !prepared.is_empty() {
                            // The plan body renders at the level of its
                            // children, never deeper (Conventions §4.8).
                            let body_prefix = Span::styled(
                                format!("{}│ ", plan_body_indent(PLAN_HEADER_LEVEL + 1)),
                                engine.fg(palette.text_muted),
                            );
                            let mut desc_lines = Vec::new();
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(body_prefix.clone());
                            let md = crate::markdown::render_markdown(&prepared, &theme);
                            if !md.is_empty() {
                                desc_lines.extend(md);
                            } else {
                                for line in prepared.lines() {
                                    desc_lines.push(Line::from(vec![
                                        body_prefix.clone(),
                                        Span::styled(
                                            line.to_string(),
                                            engine.fg(palette.text_secondary),
                                        ),
                                    ]));
                                }
                            }
                            all_lines.extend(wrap_lines(desc_lines, width));
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
                all_lines.extend(wrap_lines(vec![line], width));
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
                    Span::raw(indent.clone()),
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
                let mut row_lines = wrap_lines(vec![row_line], width);

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
                            let key = concept_body_key(plan_id, item_id);
                            let body =
                                cache.lines_for(&key, &prepared, &theme, Some(width), theme_mode);
                            row_lines.extend(body.iter().cloned());
                        }
                    }
                }
                all_lines.extend(row_lines);
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
                all_lines.extend(wrap_lines(vec![line], width));
            }
            VisiblePlanRow::PlanItem {
                plan_id,
                item_id,
                title,
                details,
                status,
                files,
                readonly_files,
                concept_refs,
                blocked_reason,
                depth,
                has_children,
                has_details: _,
                is_expandable,
                is_expanded,
                focused,
                ..
            } => {
                let indent = plan_indent(*depth);
                // The step body renders at the level of its children, never
                // deeper (Conventions §4.8).
                let body_prefix = format!("{}│ ", plan_body_indent(*depth + 1));

                let fold_icon = if *is_expandable {
                    if *is_expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                } else if is_selected {
                    "▶ "
                } else if *status == PlanItemStatus::Discussion {
                    "? "
                } else if *status == PlanItemStatus::Blocked {
                    "! "
                } else {
                    "• "
                };

                let fold_style = if *is_expandable {
                    engine.fg(palette.status_warning).bold()
                } else if is_selected {
                    engine.fg(palette.status_info).bold()
                } else {
                    engine.fg(palette.text_muted)
                };

                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == PlanItemStatus::Done || *status == PlanItemStatus::Cancelled {
                    engine.fg(palette.text_secondary)
                } else {
                    Style::default().bold()
                };

                let clean_title = title.replace(['\r', '\n'], " ").trim().to_string();

                let mut spans = vec![
                    gutter,
                    Span::raw(indent.clone()),
                    Span::styled(fold_icon, fold_style),
                    Span::styled(format!(" {} ", status), status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", item_id, clean_title), title_style),
                ];

                if !*has_children && (!files.is_empty() || !readonly_files.is_empty()) {
                    let mut badge_parts = Vec::new();
                    if !files.is_empty() {
                        badge_parts.push(crate::i18n::active_t("plans.file_count").replacen(
                            "{}",
                            &files.len().to_string(),
                            1,
                        ));
                    }
                    if !readonly_files.is_empty() {
                        badge_parts.push(crate::i18n::active_t("plans.ref_count").replacen(
                            "{}",
                            &readonly_files.len().to_string(),
                            1,
                        ));
                    }
                    spans.push(Span::styled(
                        format!(" [{}]", badge_parts.join(" + ")),
                        engine.fg(palette.text_muted),
                    ));
                }
                if *focused {
                    spans.push(focus_badge());
                }

                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];
                // A cached Markdown body arrives already wrapped, so it is
                // collected apart and appended after the row summary and its
                // metadata, which are short and wrapped every frame (c-1).
                let mut detail_lines: Vec<Line<'static>> = Vec::new();

                if *is_expanded {
                    if !*has_children && !files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(body_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled(
                                format!("{} ", crate::i18n::active_t("plans.files_prefix")),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled(files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !*has_children && !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(body_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled(
                                format!("{} ", crate::i18n::active_t("plans.ref_prefix")),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled(readonly_files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !concept_refs.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(body_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled(
                                crate::i18n::active_t("plans.concept_prefix"),
                                engine.fg(palette.text_muted),
                            ),
                            Span::styled(
                                concept_refs.join(", "),
                                engine.fg(palette.button_active_bg),
                            ),
                        ]));
                    }
                    if let Some(reason) = blocked_reason {
                        let prepared = prepare_plan_markdown(reason);
                        if !prepared.is_empty() {
                            // The blocker is the step's most important detail,
                            // so it leads the body of a blocked step and keeps
                            // the semantic error role (Conventions §8.4).
                            let reason_style = engine.fg(palette.status_error);
                            let prefix = format!(
                                "{}{} ",
                                body_prefix,
                                crate::i18n::active_t("plans.blocked_prefix")
                            );
                            for (index, text) in prepared.lines().enumerate() {
                                let lead = if index == 0 {
                                    Span::styled(prefix.clone(), reason_style.bold())
                                } else {
                                    Span::styled(body_prefix.clone(), engine.fg(palette.text_muted))
                                };
                                row_lines.push(Line::from(vec![
                                    lead,
                                    Span::styled(text.to_string(), reason_style),
                                ]));
                            }
                        }
                    }
                    if let Some(det) = details {
                        let prepared = prepare_plan_markdown(det);
                        if !prepared.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix = Some(Span::styled(
                                body_prefix.clone(),
                                engine.fg(palette.text_muted),
                            ));
                            let key = step_body_key(plan_id, item_id);
                            let body =
                                cache.lines_for(&key, &prepared, &theme, Some(width), theme_mode);
                            detail_lines.extend(body.iter().cloned());
                        }
                    }
                }

                let mut rendered = wrap_lines(row_lines, width);
                rendered.extend(detail_lines);
                all_lines.extend(rendered);
            }
        }
    }

    (all_lines, offsets)
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
            files: vec![],
            readonly_files: vec![],
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
                description: None,
                concept: vec![ConceptItem {
                    details: Some(concept_details.to_string()),
                    ..concept_leaf("c-1", "Invariant")
                }],
                items: vec![PlanItem {
                    details: Some(step_details.to_string()),
                    ..leaf_item("1", "Step")
                }],
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
            first_text.contains("Tokens must be signed."),
            "the concept body must render: {first_text}"
        );
        assert!(
            first_text.contains("Implement the signer."),
            "the step body must render: {first_text}"
        );
        assert_eq!(
            view.markdown_cache.len(),
            2,
            "one cached body per rendered description"
        );

        // Moving the cursor re-renders every row but must reuse both bodies.
        let (second, _) = render_view(&mut view, step_row, 200);
        assert_eq!(
            view.markdown_cache.len(),
            2,
            "navigation must neither add nor drop a cached body"
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
            2,
            "a new wrap width drops and re-renders both bodies"
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
        let step = plan.items.first_mut().expect("the fixture step is present");
        step.details = Some("Second wording.".to_string());

        let (after, _) = render_view(&mut view, usize::MAX, 200);
        let text = joined_text(&after);
        assert!(text.contains("Second wording."), "{text}");
        assert!(!text.contains("First wording."), "{text}");
        assert_eq!(
            view.markdown_cache.len(),
            2,
            "a rewritten description replaces its own body"
        );
    }

    #[test]
    fn test_file_badges_render_only_for_leaf_items() {
        let mut view = PlansViewState::default();
        let plan = Plan {
            id: "leaf-files".to_string(),
            title: "Leaf Files".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            last_replanned_step: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Phase".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                files: vec!["parent.rs".to_string()],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![PlanItem {
                    id: "1.1".to_string(),
                    title: "Leaf Task".to_string(),
                    details: None,
                    status: PlanItemStatus::Todo,
                    blocked_reason: None,
                    files: vec!["leaf.rs".to_string()],
                    readonly_files: vec![],
                    concept_refs: vec![],
                    depends_on: vec![],
                    children: vec![],
                }],
            }],
        };
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
        let plan = Plan {
            id: "ref-files".to_string(),
            title: "Reference Files".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            last_replanned_step: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Wire Auth".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                files: vec!["src/auth.rs".to_string()],
                readonly_files: vec![
                    "contracts/api.md".to_string(),
                    "contracts/types.rs".to_string(),
                ],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
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
            plans: vec![Plan {
                id: "blocked-plan".to_string(),
                title: "Blocked Plan".to_string(),
                items: vec![item],
                ..Default::default()
            }],
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
            plans: vec![Plan {
                id: "expandable-plan".to_string(),
                title: "Expandable Plan".to_string(),
                items: vec![item],
                ..Default::default()
            }],
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
        let plan = Plan {
            id: "focus-plan".to_string(),
            title: "Focus Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            last_replanned_step: None,
            concept: vec![],
            items: vec![PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: PlanItemStatus::Todo,
                blocked_reason: None,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        };
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
            plans: vec![Plan {
                id: "arch-plan".to_string(),
                title: "Arch Plan".to_string(),
                architect: Some(tauqe_protocol::ArchitectTier::new(
                    tauqe_protocol::ModelRef::openrouter("planner/model"),
                    Some(tauqe_protocol::ReasoningEffort::High),
                )),
                ..Default::default()
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
                Plan {
                    id: "plan-a".to_string(),
                    title: "Plan A".to_string(),
                    ..Default::default()
                },
                Plan {
                    id: "plan-b".to_string(),
                    title: "Plan B".to_string(),
                    ..Default::default()
                },
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
            plans: vec![Plan {
                id: "indent-plan".to_string(),
                title: "Indent Plan".to_string(),
                description: Some("Plan description.".to_string()),
                concept: vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "Invariant".to_string(),
                    details: Some("Tokens must be signed.".to_string()),
                    children: vec![concept_leaf("c-1.1", "Nested Invariant")],
                }],
                items: vec![parent],
                ..Default::default()
            }],
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
            plans: vec![Plan {
                id: "state-plan".to_string(),
                title: "State Plan".to_string(),
                description: Some("Plan description.".to_string()),
                concept: vec![ConceptItem {
                    id: "c-1".to_string(),
                    title: "Invariant".to_string(),
                    details: Some("Tokens must be signed.".to_string()),
                    children: vec![concept_leaf("c-1.1", "Nested Invariant")],
                }],
                items: vec![parent],
                ..Default::default()
            }],
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
}
