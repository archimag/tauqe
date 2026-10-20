//! Tree-line rendering of the Review view.
//!
//! The Review tree is a document of state rather than a widget tree: every row,
//! its detail block, the embedded subplan steps, and the section headers are
//! rendered into one flat line stream whose leading indentation is derived from
//! the numeric nesting depth. Hosting that scale here keeps `review.rs` focused
//! on tree state and navigation, and keeps the strictly increasing indent
//! invariant testable in one place (Conventions §4.8).

use std::collections::HashSet;

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use tauqe_protocol::{IssueStatus, IssueType, ModelSelection, PlanItem, PlanItemStatus};

use crate::theme::engine::current_engine;
use crate::ui::wrap_lines;
use crate::views::plans::architect_badge;

use super::VisibleReviewRow;

/// Expansion key of one subplan step inside a finding's embedded plan.
///
/// The `s:` segment keeps step keys disjoint from the `session:item` keys that
/// mark an expanded finding, so the two expansion scopes never collide.
pub(crate) fn subplan_key(session_id: &str, item_id: u32, step_id: &str) -> String {
    format!("{}:{}:s:{}", session_id, item_id, step_id)
}

/// Records every subplan step of a finding as expanded.
pub(crate) fn collect_subplan_keys(
    session_id: &str,
    item_id: u32,
    steps: &[PlanItem],
    out: &mut HashSet<String>,
) {
    for step in steps {
        out.insert(subplan_key(session_id, item_id, &step.id));
        collect_subplan_keys(session_id, item_id, &step.children, out);
    }
}

/// Whether every subplan step of a finding is expanded.
pub(crate) fn subplan_all_expanded(
    expanded: &HashSet<String>,
    session_id: &str,
    item_id: u32,
    steps: &[PlanItem],
) -> bool {
    steps.iter().all(|step| {
        expanded.contains(&subplan_key(session_id, item_id, &step.id))
            && subplan_all_expanded(expanded, session_id, item_id, &step.children)
    })
}

/// Indent step, in columns, of one nesting level of the Review tree.
const REVIEW_INDENT_STEP: usize = 2;

/// Nesting level of a session header row.
const REVIEW_SESSION_LEVEL: usize = 0;

/// Nesting level of the Executive Summary row of a session.
const REVIEW_SUMMARY_LEVEL: usize = 1;

/// Nesting level of a finding row.
///
/// A finding's own detail block and its top-level subplan steps both sit one
/// level deeper, so the body never reads as deeper than the children.
const REVIEW_ITEM_LEVEL: usize = 2;

/// Leading indentation of a node rendered at `level`.
///
/// Every hierarchical row derives its leading indent from its numeric depth
/// through this single helper, so a parent row, its body, and its children can
/// never drift apart and no renderer needs a literal whitespace run
/// (Conventions §4.8).
fn review_indent(level: usize) -> String {
    " ".repeat(REVIEW_INDENT_STEP * level)
}

/// Leading indentation of the detail block of a node rendered at `level`.
///
/// A summary row starts with a one-column selection gutter, while a detail
/// block owns no gutter of its own. The gutter column is therefore reproduced
/// as a leading space, so a node's body lines up exactly with the rows around
/// it and the body never reads as shallower than its children.
fn review_body_indent(level: usize) -> String {
    format!(" {}", review_indent(level))
}

/// Nesting level of a subplan step lying `depth` levels below its finding.
///
/// `depth` keeps its established meaning (0 is the top-level step); the shift
/// that places a step strictly deeper than its finding lives here, in the
/// render scale, and never in the flatten logic.
fn subplan_step_level(depth: usize) -> usize {
    REVIEW_ITEM_LEVEL + 1 + depth
}

/// Short badge text for a classified problem type.
pub(crate) fn issue_type_badge(issue_type: IssueType) -> &'static str {
    match issue_type {
        IssueType::Bug => "BUG",
        IssueType::Security => "SEC",
        IssueType::Architecture => "ARCH",
        IssueType::Performance => "PERF",
        IssueType::Quality => "QUAL",
    }
}

/// Emphasized color for a finding's problem type badge.
pub(crate) fn issue_type_style(issue_type: IssueType) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    match issue_type {
        IssueType::Bug | IssueType::Security => engine.fg(palette.status_error).bold(),
        IssueType::Architecture | IssueType::Performance => {
            engine.fg(palette.status_warning).bold()
        }
        IssueType::Quality => engine.fg(palette.status_info).bold(),
    }
}

/// Filled badge style for a finding's lifecycle status, including `Refine`.
pub(crate) fn issue_status_style(status: IssueStatus) -> Style {
    let engine = current_engine();
    let palette = engine.palette();
    let inverse = engine.color(palette.text_inverse);
    match status {
        IssueStatus::Discussion => engine.bg(palette.button_active_bg).fg(inverse).bold(),
        IssueStatus::Refine => engine.bg(palette.status_error).fg(inverse).bold(),
        IssueStatus::Todo => engine.bg(palette.status_warning).fg(inverse).bold(),
        IssueStatus::InProgress => engine.bg(palette.status_info).fg(inverse).bold(),
        IssueStatus::Fixed => engine.bg(palette.status_success).fg(inverse).bold(),
        IssueStatus::Skipped | IssueStatus::Rejected => {
            engine.bg(palette.text_muted).fg(inverse).bold()
        }
    }
}

/// Formats a Unix timestamp (seconds) as a compact UTC calendar instant
/// (`YYYY-MM-DD HH:MM UTC`) for the session header.
pub(crate) fn format_created_at(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (hour, minute) = (rem / 3_600, (rem % 3_600) / 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year, month, day, hour, minute
    )
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

/// Status badge style for a subplan step, mirroring the Plans view vocabulary so
/// a decomposed finding reads exactly like an implementation step.
pub(crate) fn plan_status_style(status: PlanItemStatus) -> Style {
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

/// Counts executable leaves in an embedded subplan as `(completed, total)`.
///
/// Mirrors `ReviewItem::subplan_stats`: only leaves carry an explicit status,
/// cancelled leaves are excluded from the total, and `completed` counts the
/// done leaves.
fn subplan_progress(items: &[PlanItem]) -> (usize, usize) {
    fn walk(items: &[PlanItem], total: &mut usize, completed: &mut usize) {
        for item in items {
            if item.is_leaf() {
                if item.status == PlanItemStatus::Cancelled {
                    continue;
                }
                *total += 1;
                if item.status == PlanItemStatus::Done {
                    *completed += 1;
                }
            } else {
                walk(&item.children, total, completed);
            }
        }
    }
    let mut total = 0;
    let mut completed = 0;
    walk(items, &mut total, &mut completed);
    (completed, total)
}

pub(crate) fn model_badge(selection: &ModelSelection) -> Span<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    match selection {
        ModelSelection::Role(role) => match role {
            tauqe_protocol::ModelRole::Scribe => {
                Span::styled(" [Scr]", engine.fg(palette.status_success))
            }
            tauqe_protocol::ModelRole::Coder => {
                Span::styled(" [Cod]", engine.fg(palette.status_warning))
            }
            tauqe_protocol::ModelRole::Architect => {
                Span::styled(" [Arc]", engine.fg(palette.status_error))
            }
            tauqe_protocol::ModelRole::Primary => {
                Span::styled(" [Pri]", engine.fg(palette.status_info))
            }
            tauqe_protocol::ModelRole::Reviewer => {
                Span::styled(" [Rev]", engine.fg(palette.button_active_bg))
            }
        },
        ModelSelection::Specific(model_ref) => Span::styled(
            format!(" [{}]", model_ref.name),
            engine.fg(palette.status_info),
        ),
    }
}

pub(crate) fn issue_status_label(status: IssueStatus) -> &'static str {
    match status {
        IssueStatus::Discussion => "DISCUSSION",
        IssueStatus::Todo => "TODO",
        IssueStatus::InProgress => "IN_PROGRESS",
        IssueStatus::Refine => "REFINE",
        IssueStatus::Fixed => "FIXED",
        IssueStatus::Skipped => "SKIP",
        IssueStatus::Rejected => "REJECTED",
    }
}

pub(crate) fn location(files: &[String]) -> Option<String> {
    if files.is_empty() {
        None
    } else {
        Some(files.join(", "))
    }
}

fn split_title_and_file_path(title: &str) -> (&str, Option<&str>) {
    let trimmed = title.trim();
    if let Some(idx) = trimmed.rfind(" (") {
        if trimmed.ends_with(')') {
            let inside = &trimmed[idx + 2..trimmed.len() - 1];
            if inside.contains('/')
                || inside.contains('\\')
                || inside.contains(':')
                || inside.contains('.')
            {
                return (trimmed[..idx].trim(), Some(inside));
            }
        }
    }
    (trimmed, None)
}

fn pad_line_to_width(mut line: Line<'static>, width: usize, fill_style: Style) -> Line<'static> {
    let current_width: usize = line.spans.iter().map(|s| s.width()).sum();
    if current_width < width {
        line.spans
            .push(Span::styled(" ".repeat(width - current_width), fill_style));
    }
    line
}

pub(crate) fn compute_unified_review_lines(
    rows: &[VisibleReviewRow],
    selected_idx: usize,
    width: usize,
    theme_mode: crate::config::ThemeMode,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let engine = current_engine();
    let palette = engine.palette();
    let mut all_lines = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());

    for (idx, row) in rows.iter().enumerate() {
        let is_selected = idx == selected_idx;
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
            VisibleReviewRow::SessionHeader {
                session_id,
                title,
                model,
                created_at,
                total,
                fixed,
                todo,
                in_progress,
                discussion,
                skipped,
                rejected,
                is_expanded,
                focused,
                ..
            } => {
                if idx > 0 {
                    all_lines.push(Line::raw(""));
                }

                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let stats_str = format!(
                    "{}/{} fixed ({} todo, {} prog, {} disc, {} skip, {} rej)",
                    fixed, total, todo, in_progress, discussion, skipped, rejected
                );

                let mut spans = vec![
                    gutter,
                    Span::raw(review_indent(REVIEW_SESSION_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        format!("[{}] ", session_id),
                        engine.fg(palette.status_info).bold(),
                    ),
                    Span::styled(title.clone(), engine.fg(palette.text_primary).bold()),
                    Span::styled(format!(" ({}) ", model), engine.fg(palette.text_muted)),
                    Span::styled(
                        format!("{}  ", format_created_at(*created_at)),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        format!("({})", stats_str),
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

                let mut header_line = Line::from(spans).style(row_style);
                if is_selected {
                    header_line = pad_line_to_width(header_line, width, row_style);
                }
                all_lines.extend(wrap_lines(vec![header_line], width));
            }
            VisibleReviewRow::SessionSummary {
                description,
                is_expanded,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let spans = vec![
                    gutter,
                    Span::raw(review_indent(REVIEW_SUMMARY_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        format!("{}:", crate::i18n::active_t("review.executive_summary")),
                        engine.fg(palette.status_success).bold(),
                    ),
                ];

                let mut summary_line = Line::from(spans).style(row_style);
                if is_selected {
                    summary_line = pad_line_to_width(summary_line, width, row_style);
                }
                let mut row_lines = vec![summary_line];

                if *is_expanded {
                    let trimmed = description.trim();
                    if !trimmed.is_empty() {
                        let body_prefix = Span::styled(
                            format!("{}│ ", review_body_indent(REVIEW_SUMMARY_LEVEL + 1)),
                            engine.fg(palette.text_muted),
                        );
                        let mut theme = crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = engine.fg(palette.text_secondary);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(body_prefix.clone());
                        let md = crate::markdown::render_markdown(trimmed, &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        } else {
                            for line in trimmed.lines() {
                                row_lines.push(Line::from(vec![
                                    body_prefix.clone(),
                                    Span::styled(
                                        line.to_string(),
                                        engine.fg(palette.text_secondary),
                                    ),
                                ]));
                            }
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
            VisibleReviewRow::ReviewItem {
                item_id,
                title,
                issue_type,
                status,
                model,
                files,
                readonly_files,
                description,
                recommendations,
                note,
                plan,
                subplan_architect,
                is_expanded,
                focused,
                ..
            } => {
                let fold_icon = if *is_expanded { "▼ " } else { "▶ " };
                let label = issue_status_label(*status);

                let title_style = if is_selected {
                    engine.fg(palette.text_primary).bold()
                } else if *status == IssueStatus::Fixed
                    || *status == IssueStatus::Skipped
                    || *status == IssueStatus::Rejected
                {
                    engine.fg(palette.text_secondary)
                } else {
                    Style::default().bold()
                };

                let raw_title = title.replace(['\r', '\n'], " ");
                let (display_title, extra_path) = split_title_and_file_path(&raw_title);

                let mut spans = vec![
                    gutter,
                    Span::raw(review_indent(REVIEW_ITEM_LEVEL)),
                    Span::styled(fold_icon, engine.fg(palette.status_warning).bold()),
                    Span::styled(
                        format!(" {} ", issue_type_badge(*issue_type)),
                        issue_type_style(*issue_type),
                    ),
                    Span::raw(" "),
                    Span::styled(format!(" {} ", label), issue_status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", item_id, display_title), title_style),
                ];
                if let Some(selection) = model {
                    spans.push(model_badge(selection));
                }
                if !files.is_empty() {
                    spans.push(Span::styled(
                        format!(
                            " [{} file{}]",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" }
                        ),
                        engine.fg(palette.text_muted),
                    ));
                }
                if !readonly_files.is_empty() {
                    spans.push(Span::styled(
                        format!(" [{} readonly]", readonly_files.len()),
                        engine.fg(palette.text_muted),
                    ));
                }
                if !plan.is_empty() {
                    let (done, total) = subplan_progress(plan.as_slice());
                    if total > 0 {
                        spans.push(Span::styled(
                            format!(" [{}/{} steps]", done, total),
                            engine.fg(palette.status_info),
                        ));
                    }
                }
                if let Some(tier) = subplan_architect {
                    spans.push(architect_badge(tier));
                }
                if *focused {
                    spans.push(focus_badge());
                }

                let mut item_line = Line::from(spans).style(row_style);
                if is_selected {
                    item_line = pad_line_to_width(item_line, width, row_style);
                }
                let mut row_lines = vec![item_line];
                // The finding's own detail block renders at the level of its
                // children, never deeper (Conventions §4.8).
                let body_indent = review_body_indent(REVIEW_ITEM_LEVEL + 1);

                if *is_expanded {
                    let loc_str = location(files).or_else(|| extra_path.map(|s| s.to_string()));
                    if let Some(loc) = loc_str {
                        let clean_loc = loc.trim();
                        if !clean_loc.is_empty() {
                            row_lines.push(Line::from(vec![
                                Span::styled(
                                    format!(
                                        "{body_indent}{}",
                                        crate::i18n::active_t("review.files_label")
                                    ),
                                    engine.fg(palette.status_info).bold(),
                                ),
                                Span::styled(
                                    clean_loc.to_string(),
                                    engine.fg(palette.text_primary),
                                ),
                            ]));
                        }
                    }

                    if !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(
                                format!(
                                    "{body_indent}{}",
                                    crate::i18n::active_t("review.readonly_label")
                                ),
                                engine.fg(palette.status_warning).bold(),
                            ),
                            Span::styled(
                                readonly_files.join(", "),
                                engine.fg(palette.text_primary),
                            ),
                        ]));
                    }

                    if let Some(notes) = note.as_deref() {
                        let clean_notes = notes.trim();
                        if !clean_notes.is_empty() {
                            let note_header = if *status == IssueStatus::Skipped {
                                format!(
                                    "{body_indent}{} ",
                                    crate::i18n::active_t("review.skip_reason_label")
                                )
                            } else {
                                format!(
                                    "{body_indent}{} ",
                                    crate::i18n::active_t("review.note_label")
                                )
                            };
                            let mut note_lines = clean_notes.lines();
                            if let Some(first) = note_lines.next() {
                                row_lines.push(Line::from(vec![
                                    Span::styled(
                                        note_header,
                                        engine.fg(palette.status_warning).bold(),
                                    ),
                                    Span::styled(
                                        first.to_string(),
                                        engine.fg(palette.text_primary),
                                    ),
                                ]));
                                for line in note_lines {
                                    row_lines.push(Line::from(vec![
                                        Span::styled(
                                            format!("{body_indent}│ "),
                                            engine.fg(palette.text_muted),
                                        ),
                                        Span::styled(
                                            line.to_string(),
                                            engine.fg(palette.text_primary),
                                        ),
                                    ]));
                                }
                            }
                        }
                    }

                    if !description.trim().is_empty() {
                        row_lines.push(Line::from(Span::styled(
                            format!(
                                "{body_indent}{}",
                                crate::i18n::active_t("review.description_label")
                            ),
                            engine.fg(palette.status_info).bold(),
                        )));
                        let mut theme = crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                        theme.text_style = engine.fg(palette.text_secondary);
                        theme.hard_breaks = true;
                        theme.compact = true;
                        theme.line_prefix = Some(Span::styled(
                            format!("{body_indent}│ "),
                            engine.fg(palette.text_muted),
                        ));
                        let md = crate::markdown::render_markdown(description.trim(), &theme);
                        if !md.is_empty() {
                            row_lines.extend(md);
                        }
                    }

                    if !recommendations.is_empty() {
                        row_lines.push(Line::from(Span::styled(
                            format!(
                                "{body_indent}{}",
                                crate::i18n::active_t("review.recommendations_label")
                            ),
                            engine.fg(palette.status_success).bold(),
                        )));
                        for rec in recommendations {
                            row_lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{body_indent}│ • "),
                                    engine.fg(palette.text_muted),
                                ),
                                Span::styled(rec.clone(), engine.fg(palette.text_primary)),
                            ]));
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
            VisibleReviewRow::SubplanItem {
                step_id,
                title,
                details,
                status,
                files,
                readonly_files,
                depends_on,
                depth,
                is_expandable,
                is_expanded,
                ..
            } => {
                let row_level = subplan_step_level(*depth);
                let indent = review_indent(row_level);
                let body_indent = review_body_indent(row_level + 1);
                let meta_prefix = format!("{body_indent}│ ");
                let fold_icon = if *is_expandable {
                    if *is_expanded {
                        "▼ "
                    } else {
                        "▶ "
                    }
                } else if is_selected {
                    "▶ "
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
                    Span::styled(format!(" {} ", status), plan_status_style(*status)),
                    Span::raw(" "),
                    Span::styled(format!("#{} {}", step_id, clean_title), title_style),
                ];
                if !files.is_empty() || !readonly_files.is_empty() {
                    let mut parts = Vec::new();
                    if !files.is_empty() {
                        parts.push(format!(
                            "{} file{}",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" }
                        ));
                    }
                    if !readonly_files.is_empty() {
                        parts.push(format!("{} ref", readonly_files.len()));
                    }
                    spans.push(Span::styled(
                        format!(" [{}]", parts.join(" + ")),
                        engine.fg(palette.text_muted),
                    ));
                }

                let mut row_line = Line::from(spans).style(row_style);
                if is_selected {
                    row_line = pad_line_to_width(row_line, width, row_style);
                }
                let mut row_lines = vec![row_line];

                if *is_expanded {
                    if !files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("files: ", engine.fg(palette.text_muted)),
                            Span::styled(files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !readonly_files.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("ref: ", engine.fg(palette.text_muted)),
                            Span::styled(readonly_files.join(", "), engine.fg(palette.status_info)),
                        ]));
                    }
                    if !depends_on.is_empty() {
                        row_lines.push(Line::from(vec![
                            Span::styled(meta_prefix.clone(), engine.fg(palette.text_muted)),
                            Span::styled("depends_on: ", engine.fg(palette.text_muted)),
                            Span::styled(depends_on.join(", "), engine.fg(palette.status_warning)),
                        ]));
                    }
                    if let Some(det) = details {
                        let trimmed = det.trim();
                        if !trimmed.is_empty() {
                            let mut theme =
                                crate::markdown::MarkdownTheme::answer_themed(theme_mode);
                            theme.text_style = engine.fg(palette.text_secondary);
                            theme.hard_breaks = true;
                            theme.compact = true;
                            theme.line_prefix =
                                Some(Span::styled(meta_prefix, engine.fg(palette.text_muted)));
                            let md = crate::markdown::render_markdown(trimmed, &theme);
                            if !md.is_empty() {
                                row_lines.extend(md);
                            }
                        }
                    }
                }

                all_lines.extend(wrap_lines(row_lines, width));
            }
        }
    }

    (all_lines, offsets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::review::ReviewViewState;
    use tauqe_protocol::{IssueStatus, IssueType, ModelRef, ReviewIssue, ReviewItem, ReviewSession};

    fn make_item(id: u32) -> ReviewIssue {
        ReviewIssue {
            id,
            title: format!("Finding {}", id),
            model: None,
            issue_type: IssueType::Architecture,
            status: IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: vec![],
            note: None,
        }
    }

    fn make_session(id: &str, item_ids: &[u32]) -> ReviewSession {
        ReviewSession {
            id: id.to_string(),
            title: format!("Session {}", id),
            created_at: 0,
            model: ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: item_ids
                .iter()
                .map(|&i| ReviewItem::from(make_item(i)))
                .collect(),
            raw_markdown: String::new(),
        }
    }

    fn make_session_with_description(
        id: &str,
        description: &str,
        item_ids: &[u32],
    ) -> ReviewSession {
        let mut session = make_session(id, item_ids);
        session.description = Some(description.to_string());
        session
    }

    fn subplan_step(id: &str, title: &str, status: PlanItemStatus) -> PlanItem {
        PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Default::default()
        }
    }

    fn step_rows(rows: &[VisibleReviewRow]) -> Vec<(String, usize)> {
        rows.iter()
            .filter_map(|row| match row {
                VisibleReviewRow::SubplanItem { step_id, depth, .. } => {
                    Some((step_id.clone(), *depth))
                }
                _ => None,
            })
            .collect()
    }

    /// Leading indent of a rendered line, counting the selection gutter.
    ///
    /// Every row carries a one-column gutter (`▌` when selected, a space
    /// otherwise), so counting both characters yields indents that are
    /// comparable no matter where the cursor rests.
    fn leading_indent(line: &Line<'static>) -> usize {
        line.spans
            .iter()
            .flat_map(|span| span.content.chars())
            .take_while(|c| *c == ' ' || *c == '▌')
            .count()
    }

    /// Full text of a rendered line, joined across all of its spans.
    ///
    /// A label and its value are rendered as separate spans, so a probe must
    /// match the concatenated line rather than any single span.
    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn find_line(lines: &[Line<'static>], needle: &str) -> usize {
        lines
            .iter()
            .position(|line| line_text(line).contains(needle))
            .unwrap_or_else(|| panic!("missing rendered line containing {needle}"))
    }

    #[test]
    fn test_expanded_session_header_renders_executive_summary_block() {
        let session = make_session_with_description(
            "rev-summary",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        let summary_label = format!("{}:", crate::i18n::active_t("review.executive_summary"));
        assert!(
            texts.iter().any(|t| t.contains(&summary_label)),
            "expected the Executive Summary header, got: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("The architecture is sound.")),
            "expected the rendered summary body, got: {texts:?}"
        );
    }

    #[test]
    fn test_collapsed_session_header_hides_executive_summary() {
        let session = make_session_with_description(
            "rev-collapsed",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.collapsed_sessions.insert("rev-collapsed".to_string());
        let rows = view.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();

        let summary_label = format!("{}:", crate::i18n::active_t("review.executive_summary"));
        assert!(
            !texts.iter().any(|t| t.contains(&summary_label)),
            "a collapsed session must not render the Executive Summary block, got: {texts:?}"
        );
    }

    #[test]
    fn test_summary_fold_is_independent_of_findings() {
        let session = make_session_with_description(
            "rev-summary",
            "## Overview\n\nThe architecture is sound.",
            &[1],
        );
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // Expanded session exposes an expanded summary row by default.
        let rows = view.flatten_rows();
        assert!(matches!(
            rows.get(1),
            Some(VisibleReviewRow::SessionSummary {
                is_expanded: true,
                ..
            })
        ));

        let (lines, _) = compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let summary_label = format!("{}:", crate::i18n::active_t("review.executive_summary"));
        assert!(texts.iter().any(|t| t.contains(&summary_label)));
        assert!(texts
            .iter()
            .any(|t| t.contains("The architecture is sound.")));

        // Folding the summary hides only its body; findings stay visible.
        view.toggle_summary_expanded("rev-summary");
        assert!(!view.is_summary_expanded("rev-summary"));

        let rows = view.flatten_rows();
        assert!(matches!(
            rows.get(1),
            Some(VisibleReviewRow::SessionSummary {
                is_expanded: false,
                ..
            })
        ));
        let (lines, _) = compute_unified_review_lines(&rows, 0, 80, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains(&summary_label)));
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("The architecture is sound.")),
            "a folded summary must hide its body, got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("#1")),
            "findings must stay visible when the summary is folded, got: {texts:?}"
        );
    }

    #[test]
    fn test_toggle_fold_all_is_bound_to_summary_state() {
        let session = make_session_with_description("rev-summary", "Summary body.", &[1]);
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // A folded summary makes the tree "not fully expanded".
        view.collapsed_summaries.insert("rev-summary".to_string());
        assert!(!view.is_all_expanded());

        // Expanding all also unfolds summaries.
        view.toggle_fold_all();
        assert!(view.is_summary_expanded("rev-summary"));
        assert!(view.is_all_expanded());

        // Collapsing all hides the session and its summary again.
        view.toggle_fold_all();
        assert!(!view.is_all_expanded());
    }

    #[test]
    fn test_review_item_multiple_files_badge_and_expanded_rendering() {
        let mut session = make_session("rev-files", &[1]);
        session.items[0].files = vec![
            "crates/core/src/review.rs".to_string(),
            "crates/protocol/src/review.rs".to_string(),
        ];
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // Collapsed item shows [2 files] badge
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 100, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(
            texts.iter().any(|t| t.contains("[2 files]")),
            "collapsed finding should display file count badge, got: {texts:?}"
        );

        // Expanded item renders full files line
        let mut view_expanded = view;
        view_expanded.toggle_item_expanded("rev-files", 1);
        let rows_exp = view_expanded.flatten_rows();
        let (lines_exp, _) =
            compute_unified_review_lines(&rows_exp, 1, 100, crate::config::ThemeMode::Dark);
        let texts_exp: Vec<String> = lines_exp.iter().map(|l| l.to_string()).collect();
        assert!(
            texts_exp.iter().any(|t| t.contains("Files: ")
                && t.contains("crates/core/src/review.rs, crates/protocol/src/review.rs")),
            "expanded finding should render full file paths, got: {texts_exp:?}"
        );
    }

    #[test]
    fn test_review_item_skipped_badge_and_skip_reason_notes() {
        let mut session = make_session("rev-skip", &[1, 2]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            status: IssueStatus::Skipped,
            note: Some(
                "Already addressed in previous turn\nVerified no code change needed".to_string(),
            ),
            ..make_item(1)
        });
        session.items[1] = ReviewItem::from(ReviewIssue {
            status: IssueStatus::Fixed,
            note: Some("Fixed race condition".to_string()),
            ..make_item(2)
        });

        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-skip", 1);
        view.toggle_item_expanded("rev-skip", 2);

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 100, crate::config::ThemeMode::Dark);
        let texts: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

        // 1. Badge displays compact SKIP
        assert!(
            texts.iter().any(|t| t.contains(" SKIP ")),
            "expected compact SKIP status badge, got: {texts:?}"
        );

        // 2. Multiline skip reason renders with vertical prefix for subsequent lines
        let skip_label = crate::i18n::active_t("review.skip_reason_label");
        let note_label = crate::i18n::active_t("review.note_label");
        assert!(
            texts.iter().any(|t| t.contains(&format!("{skip_label} "))
                && t.contains("Already addressed in previous turn")),
            "expected Skip reason header and first line, got: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("│ Verified no code change needed")),
            "expected indented subsequent line with vertical bar prefix, got: {texts:?}"
        );

        // 3. Regular non-skipped item renders the note header
        assert!(
            texts
                .iter()
                .any(|t| t.contains(&format!("{note_label} Fixed race condition"))),
            "expected regular note header for fixed item, got: {texts:?}"
        );
    }

    #[test]
    fn test_session_header_statistics_with_skipped() {
        let mut session = make_session("rev-stats", &[1, 2, 3, 4, 5, 6, 7]);
        session.items[0].status = IssueStatus::Fixed;
        session.items[1].status = IssueStatus::Todo;
        session.items[2].status = IssueStatus::InProgress;
        session.items[3].status = IssueStatus::Discussion;
        session.items[4].status = IssueStatus::Skipped;
        session.items[5].status = IssueStatus::Skipped;
        session.items[6].status = IssueStatus::Rejected;

        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();

        if let VisibleReviewRow::SessionHeader {
            total,
            fixed,
            todo,
            in_progress,
            discussion,
            skipped,
            rejected,
            ..
        } = &rows[0]
        {
            assert_eq!(*total, 7);
            assert_eq!(*fixed, 1);
            assert_eq!(*todo, 1);
            assert_eq!(*in_progress, 1);
            assert_eq!(*discussion, 1);
            assert_eq!(*skipped, 2);
            assert_eq!(*rejected, 1);
        } else {
            panic!("expected SessionHeader row");
        }

        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 200, crate::config::ThemeMode::Dark);
        let header_text = lines[0].to_string();
        assert!(
            header_text.contains("1/7 fixed (1 todo, 1 prog, 1 disc, 2 skip, 1 rej)"),
            "expected session statistics formatted with skip count, got: {header_text}"
        );
    }

    #[test]
    fn test_issue_type_badges_and_status_labels() {
        assert_eq!(issue_type_badge(IssueType::Bug), "BUG");
        assert_eq!(issue_type_badge(IssueType::Security), "SEC");
        assert_eq!(issue_type_badge(IssueType::Architecture), "ARCH");
        assert_eq!(issue_type_badge(IssueType::Performance), "PERF");
        assert_eq!(issue_type_badge(IssueType::Quality), "QUAL");

        assert_eq!(issue_status_label(IssueStatus::Refine), "REFINE");
        assert_eq!(issue_status_label(IssueStatus::Skipped), "SKIP");
        assert_eq!(issue_status_label(IssueStatus::InProgress), "IN_PROGRESS");
    }

    #[test]
    fn test_format_created_at_is_utc_and_stable() {
        assert_eq!(format_created_at(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_created_at(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn test_session_header_renders_created_date() {
        let session = make_session("rev-date", &[1]);
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 0, 120, crate::config::ThemeMode::Dark);
        let header_text = lines[0].to_string();
        assert!(
            header_text.contains("1970-01-01 00:00 UTC"),
            "expected the formatted creation date in the header, got: {header_text}"
        );
    }

    #[test]
    fn test_review_item_renders_issue_type_and_refine_status() {
        let mut session = make_session("rev-refine-badge", &[1]);
        session.items[0].issue_type = IssueType::Bug;
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-refine-badge", 1);

        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 120, crate::config::ThemeMode::Dark);
        let text: String = lines.iter().map(|line| line.to_string()).collect();

        // The authoritative category drives the badge directly.
        assert!(
            text.contains(" BUG "),
            "expected the issue-type badge: {text}"
        );
    }

    #[test]
    fn test_review_focus_badge_and_subplan_rendering() {
        let mut session = make_session("rev-focus", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![
                PlanItem {
                    id: "1.1".to_string(),
                    title: "First subtask".to_string(),
                    status: PlanItemStatus::Done,
                    ..Default::default()
                },
                PlanItem {
                    id: "1.2".to_string(),
                    title: "Second subtask".to_string(),
                    status: PlanItemStatus::InProgress,
                    ..Default::default()
                },
            ],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.discussion_focus =
            tauqe_protocol::DiscussionFocus::review("rev-focus", vec!["1".to_string()]);
        view.toggle_item_expanded("rev-focus", 1);

        let rows = view.flatten_rows();
        assert!(matches!(
            &rows[0],
            VisibleReviewRow::SessionHeader { focused: false, .. }
        ));
        assert!(rows.iter().any(|r| matches!(
            r,
            VisibleReviewRow::ReviewItem {
                item_id: 1,
                focused: true,
                ..
            }
        )));

        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 120, crate::config::ThemeMode::Dark);
        let mut text = String::new();
        for line in &lines {
            for span in &line.spans {
                text.push_str(&span.content);
            }
        }
        assert!(text.contains("[FOCUS]"), "focus badge expected: {text}");
        assert!(
            text.contains("[1/2 steps]"),
            "subplan badge expected: {text}"
        );
        assert!(
            text.contains("#1.1 First subtask"),
            "subtask expected: {text}"
        );
        assert!(
            text.contains("#1.2 Second subtask"),
            "subtask expected: {text}"
        );
    }

    #[test]
    fn test_subplan_steps_flatten_into_interactive_rows() {
        let mut session = make_session("rev-sub", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![PlanItem {
                children: vec![
                    subplan_step("1.1", "First leaf", PlanItemStatus::Todo),
                    subplan_step("1.2", "Second leaf", PlanItemStatus::Done),
                ],
                ..subplan_step("1", "Parent step", PlanItemStatus::InProgress)
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // A collapsed finding exposes no subplan rows at all.
        assert!(step_rows(&view.flatten_rows()).is_empty());

        // An expanded finding exposes its top-level step only; children stay
        // folded until the container step is expanded.
        view.toggle_item_expanded("rev-sub", 1);
        assert_eq!(step_rows(&view.flatten_rows()), vec![("1".to_string(), 0)]);

        view.toggle_subplan_item_expanded("rev-sub", 1, "1");
        assert_eq!(
            step_rows(&view.flatten_rows()),
            vec![
                ("1".to_string(), 0),
                ("1.1".to_string(), 1),
                ("1.2".to_string(), 1),
            ]
        );

        // A row carries the owning finding, the step status, and its path.
        assert!(view.flatten_rows().iter().any(|row| matches!(
            row,
            VisibleReviewRow::SubplanItem {
                item_id: 1,
                step_id,
                status: PlanItemStatus::Done,
                ..
            } if step_id == "1.2"
        )));
    }

    #[test]
    fn test_subplan_step_details_and_files_render_when_expanded() {
        let mut session = make_session("rev-sub", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![PlanItem {
                id: "1.1".to_string(),
                title: "Wire the auth guard".to_string(),
                details: Some("Guard must reject anonymous callers.".to_string()),
                status: PlanItemStatus::InProgress,
                files: vec!["src/auth.rs".to_string()],
                readonly_files: vec!["contracts/api.md".to_string()],
                depends_on: vec!["1".to_string()],
                ..Default::default()
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-sub", 1);

        // Folded step shows only its title and the file badge.
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 2, 160, crate::config::ThemeMode::Dark);
        let folded: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(folded.contains("#1.1 Wire the auth guard"), "{folded}");
        assert!(folded.contains("[1 file + 1 ref]"), "{folded}");
        assert!(!folded.contains("depends_on: 1"), "{folded}");

        // Expanded step reveals target files, references, dependencies, and the
        // formatted description block.
        view.toggle_subplan_item_expanded("rev-sub", 1, "1.1");
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, 2, 160, crate::config::ThemeMode::Dark);
        let expanded: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(expanded.contains("files: src/auth.rs"), "{expanded}");
        assert!(expanded.contains("ref: contracts/api.md"), "{expanded}");
        assert!(expanded.contains("depends_on: 1"), "{expanded}");
        assert!(
            expanded.contains("Guard must reject anonymous callers."),
            "{expanded}"
        );
    }

    #[test]
    fn test_review_item_renders_the_architect_provenance() {
        let mut session = make_session("rev-arch", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            plan: vec![subplan_step("1.1", "First step", PlanItemStatus::Todo)],
            ..make_item(1)
        });
        session.items[0].subplan_architect = Some(tauqe_protocol::ArchitectTier::new(
            tauqe_protocol::ModelRef::openrouter("planner/model"),
            Some(tauqe_protocol::ReasoningEffort::XHigh),
        ));
        let view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        let rows = view.flatten_rows();
        assert!(rows.iter().any(|row| matches!(
            row,
            VisibleReviewRow::ReviewItem {
                subplan_architect: Some(_),
                ..
            }
        )));

        let (lines, _) =
            compute_unified_review_lines(&rows, 1, 160, crate::config::ThemeMode::Dark);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("[ARC openrouter:planner/model xhigh]"),
            "the finding must name the Architect model and depth: {text}"
        );
    }

    #[test]
    fn test_hierarchy_indent_strictly_increases_and_body_stays_within_children() {
        let mut session = make_session_with_description("rev-indent", "Summary body.", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            files: vec!["src/auth.rs".to_string()],
            plan: vec![PlanItem {
                children: vec![subplan_step("1.1", "Leaf step", PlanItemStatus::Todo)],
                ..subplan_step("1", "Container step", PlanItemStatus::Todo)
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };
        view.toggle_item_expanded("rev-indent", 1);
        view.toggle_subplan_item_expanded("rev-indent", 1, "1");

        let rows = view.flatten_rows();
        let (lines, _offsets) =
            compute_unified_review_lines(&rows, 0, 160, crate::config::ThemeMode::Dark);

        let header = leading_indent(&lines[find_line(&lines, "[rev-indent]")]);
        let summary = leading_indent(&lines[find_line(&lines, "Executive Summary:")]);
        let item = leading_indent(&lines[find_line(&lines, "#1 Finding 1")]);
        let step = leading_indent(&lines[find_line(&lines, "#1 Container step")]);
        let leaf = leading_indent(&lines[find_line(&lines, "#1.1 Leaf step")]);

        // Every nested node renders strictly deeper than its visual parent.
        assert!(
            header < summary,
            "the session summary must sit deeper than its session header: \
             {header} >= {summary}"
        );
        assert!(
            summary < item,
            "a finding must sit deeper than the session summary: {summary} >= {item}"
        );
        assert!(
            item < step,
            "a top-level subplan step must sit strictly deeper than its finding: \
             {item} >= {step}"
        );
        assert!(
            step < leaf,
            "a nested step must sit deeper than its container: {step} >= {leaf}"
        );

        // A node body occupies the level of its children, never a deeper one.
        let finding_body = leading_indent(&lines[find_line(&lines, "Files: src/auth.rs")]);
        assert_eq!(
            finding_body, step,
            "the finding detail block must sit at the level of its children"
        );
        let summary_body = leading_indent(&lines[find_line(&lines, "Summary body.")]);
        assert!(
            summary_body <= item,
            "the session summary body must never sit deeper than its findings: \
             {summary_body} > {item}"
        );
    }

    #[test]
    fn test_hierarchy_indent_covers_collapsed_and_expanded_states() {
        let mut session = make_session_with_description("rev-state", "Summary body.", &[1]);
        session.items[0] = ReviewItem::from(ReviewIssue {
            files: vec!["src/auth.rs".to_string()],
            plan: vec![PlanItem {
                children: vec![subplan_step("1.1", "Leaf step", PlanItemStatus::Todo)],
                ..subplan_step("1", "Container step", PlanItemStatus::Todo)
            }],
            ..make_item(1)
        });
        let mut view = ReviewViewState {
            sessions: vec![session],
            ..Default::default()
        };

        // A collapsed session exposes only its header, anchored at the root.
        view.collapsed_sessions.insert("rev-state".to_string());
        let rows = view.flatten_rows();
        assert_eq!(rows.len(), 1, "a collapsed session exposes only its header");
        let (lines, _) =
            compute_unified_review_lines(&rows, usize::MAX, 160, crate::config::ThemeMode::Dark);
        let header = leading_indent(&lines[find_line(&lines, "[rev-state]")]);
        assert_eq!(header, 1, "the session header sits at the root level");
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !rendered.contains("Summary body."),
            "a collapsed session hides its summary: {rendered}"
        );
        assert!(
            !rendered.contains("#1 Finding 1"),
            "a collapsed session hides its findings: {rendered}"
        );

        // An expanded session with a collapsed finding shows the header, the
        // summary, and the finding, but no subplan rows; each nested row is
        // strictly deeper than its parent.
        view.collapsed_sessions.clear();
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, usize::MAX, 160, crate::config::ThemeMode::Dark);
        let header = leading_indent(&lines[find_line(&lines, "[rev-state]")]);
        let summary = leading_indent(&lines[find_line(&lines, "Executive Summary:")]);
        let item = leading_indent(&lines[find_line(&lines, "#1 Finding 1")]);
        assert!(header < summary, "{header} >= {summary}");
        assert!(summary < item, "{summary} >= {item}");
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !rendered.contains("#1 Container step"),
            "a collapsed finding hides its subplan: {rendered}"
        );

        // A fully expanded finding places its steps strictly deeper than the
        // finding and keeps the finding body at the children level.
        view.toggle_item_expanded("rev-state", 1);
        view.toggle_subplan_item_expanded("rev-state", 1, "1");
        let rows = view.flatten_rows();
        let (lines, _) =
            compute_unified_review_lines(&rows, usize::MAX, 160, crate::config::ThemeMode::Dark);
        let item = leading_indent(&lines[find_line(&lines, "#1 Finding 1")]);
        let step = leading_indent(&lines[find_line(&lines, "#1 Container step")]);
        let leaf = leading_indent(&lines[find_line(&lines, "#1.1 Leaf step")]);
        assert!(item < step, "{item} >= {step}");
        assert!(step < leaf, "{step} >= {leaf}");

        let finding_body = leading_indent(&lines[find_line(&lines, "Files: src/auth.rs")]);
        assert_eq!(
            finding_body, step,
            "the finding detail block must sit at the level of its children"
        );
        let summary_body = leading_indent(&lines[find_line(&lines, "Summary body.")]);
        assert!(
            summary_body <= item,
            "the session summary body must never sit deeper than its findings: \
             {summary_body} > {item}"
        );
    }
}
