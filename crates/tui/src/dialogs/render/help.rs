use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{HelpTab, ViewMode};
use crate::command::editor::EDITOR_COMMAND_MAP;
use crate::command::global::GLOBAL_COMMAND_MAP;
use crate::command::{CommandCategory, CommandMap};
use crate::components::modal::{button_style, render_modal_container, ModalTone};
use crate::config::{MetaModifier, PrimaryModifier};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::centered_rect;

/// Commands shown on the Setup (onboarding) tab.
pub const ONBOARDING_COMMANDS: &[crate::app::KeyCommand] = &[
    // Core Actions: the primary work cycle.
    crate::app::KeyCommand { key: "Enter", description: "Execute selected onboarding action" },
    // Navigation & Inspection.
    crate::app::KeyCommand { key: "Esc", description: "Exit help dialog" },
    crate::app::KeyCommand { key: "C-R", description: "Show / hide entered key" },
    // Editing & Clipboard.
    crate::app::KeyCommand { key: "C-V", description: "Paste API key from clipboard" },
    crate::app::KeyCommand { key: "C-U", description: "Clear key input buffer" },
    // Auxiliary & Destructive.
    crate::app::KeyCommand { key: "C-O", description: "Reload configuration from disk" },
];

/// One rendered help command: the projected chord label and its description.
pub struct HelpCommand {
    pub key: String,
    pub description: String,
}

/// One titled block of help commands: a cognitive tier or a single mode's list.
struct HelpSection {
    heading: String,
    commands: Vec<HelpCommand>,
}

/// Localized display title of a Help tab, resolved from the canonical catalog so
/// the tab bar and the tab hit-test geometry share one source of truth
/// (Conventions §9, c-1).
fn help_tab_title(tab: HelpTab) -> String {
    active_t(match tab {
        HelpTab::Global => "help.tab.global",
        HelpTab::CurrentMode => "help.tab.current_mode",
        HelpTab::TextEditor => "help.tab.text_editor",
    })
}

/// Localized display title of a command cognitive tier.
fn category_title(category: CommandCategory) -> String {
    active_t(match category {
        CommandCategory::Essential => "help.category.essential",
        CommandCategory::Interface => "help.category.interface",
        CommandCategory::Helpers => "help.category.helpers",
    })
}

/// Renders an abstract command chord for display: every `C-` (primary) and `M-`
/// (meta) prefix is projected through the developer's configured modifiers, so
/// the Help dialog always advertises the real keys (concept #c-1).
fn format_command_key(
    raw_key: &str,
    primary_modifier: PrimaryModifier,
    meta_modifier: MetaModifier,
) -> String {
    raw_key
        .replace("C-", primary_modifier.prefix_str())
        .replace("M-", meta_modifier.prefix_str())
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let current_len = UnicodeWidthStr::width(current.as_str());
        let word_len = UnicodeWidthStr::width(word);
        if !current.is_empty() && current_len + 1 + word_len > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Projects one categorized [`CommandMap`] into the three cognitive tiers.
///
/// Tiers keep their canonical order (Essential → Interface → Helpers) and any
/// empty tier is omitted, so a map without costly operations simply starts at
/// Interface (Conventions §3, concept #c-2).
fn command_map_sections<A: 'static>(
    map: &CommandMap<A>,
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> Vec<HelpSection> {
    command_map_filtered_sections(map, primary, meta, |_| true)
}

/// Projects a categorized [`CommandMap`] into cognitive tiers, filtering specs
/// via `keep` to exclude commands belonging to other surfaces (e.g. text editor).
fn command_map_filtered_sections<A: 'static>(
    map: &CommandMap<A>,
    primary: PrimaryModifier,
    meta: MetaModifier,
    keep: impl Fn(&A) -> bool,
) -> Vec<HelpSection> {
    CommandCategory::ALL
        .iter()
        .filter_map(|category| {
            let mut seen = std::collections::HashSet::new();
            let mut commands = Vec::new();
            for spec in map.by_category(*category) {
                if !keep(&spec.action) {
                    continue;
                }
                let key = spec.chord.display(primary, meta);
                if seen.insert(key.clone()) {
                    commands.push(HelpCommand {
                        key,
                        description: spec.description(),
                    });
                }
            }
            if commands.is_empty() {
                None
            } else {
                Some(HelpSection {
                    heading: category_title(*category),
                    commands,
                })
            }
        })
        .collect()
}

/// Builds the cognitive-ordered sections for the active view mode.
fn mode_help_sections(
    mode: ViewMode,
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> Vec<HelpSection> {
    match mode {
        ViewMode::Repl => command_map_filtered_sections(
            &crate::views::repl::REPL_COMMAND_MAP,
            primary,
            meta,
            |action| !crate::views::repl::input::is_editor_action(*action),
        ),
        ViewMode::Plans => command_map_filtered_sections(
            &crate::views::plans::PLANS_COMMAND_MAP,
            primary,
            meta,
            |_| true,
        ),
        ViewMode::Context => command_map_filtered_sections(
            &crate::views::context::CONTEXT_COMMAND_MAP,
            primary,
            meta,
            |_| true,
        ),
        ViewMode::History => command_map_filtered_sections(
            &crate::views::history::HISTORY_COMMAND_MAP,
            primary,
            meta,
            |_| true,
        ),
        ViewMode::Review => command_map_filtered_sections(
            &crate::views::review::REVIEW_COMMAND_MAP,
            primary,
            meta,
            |_| true,
        ),
        ViewMode::Onboarding => vec![HelpSection {
            heading: active_t("tab.onboarding"),
            commands: list_help_commands(ONBOARDING_COMMANDS, primary, meta),
        }],
    }
}

/// Builds the sections rendered on the active Help tab.
///
/// - `Global`: application-wide shortcuts in three cognitive tiers.
/// - `CurrentMode`: view-specific commands in three cognitive tiers.
/// - `TextEditor`: shared Emacs/Readline editor bindings.
fn build_sections(
    tab: HelpTab,
    mode: ViewMode,
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> Vec<HelpSection> {
    match tab {
        HelpTab::Global => command_map_sections(&GLOBAL_COMMAND_MAP, primary, meta),
        HelpTab::CurrentMode => mode_help_sections(mode, primary, meta),
        HelpTab::TextEditor => command_map_sections(&EDITOR_COMMAND_MAP, primary, meta),
    }
}

/// Appends one command row (and any wrapped description continuations).
fn push_command(
    lines: &mut Vec<Line<'static>>,
    command: &HelpCommand,
    col_w: usize,
    desc_w: usize,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    let mut wrapped = wrap_words(&command.description, desc_w).into_iter();
    let first = wrapped.next().unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {:<col_w$}", command.key),
            engine.fg(palette.text_primary).bold(),
        ),
        Span::styled(first, engine.fg(palette.text_secondary)),
    ]));
    for rest in wrapped {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(2 + col_w)),
            Span::styled(rest, engine.fg(palette.text_secondary)),
        ]));
    }
}

/// Builds the complete scrollable body for one Help tab at the given width.
fn build_help_body(
    tab: HelpTab,
    mode: ViewMode,
    primary: PrimaryModifier,
    meta: MetaModifier,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    let sections = build_sections(tab, mode, primary, meta);

    let key_w = sections
        .iter()
        .flat_map(|section| section.commands.iter())
        .map(|command| UnicodeWidthStr::width(command.key.as_str()))
        .max()
        .unwrap_or(0);
    let col_w = key_w + 2;
    let desc_w = inner_w.saturating_sub(2 + col_w).max(10);

    let mut lines: Vec<Line<'static>> = vec![Line::raw("")];
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            section.heading.clone(),
            engine.fg(palette.status_info).bold(),
        )));
        for command in &section.commands {
            push_command(&mut lines, command, col_w, desc_w, &engine);
        }
    }
    lines
}

/// Renders the tab bar, highlighting the active tab with the accent button style.
fn render_help_tab_bar(
    frame: &mut ratatui::Frame,
    area: Rect,
    active: HelpTab,
    engine: &ThemeEngine,
) {
    let palette = engine.palette();
    let mut spans: Vec<Span> = Vec::new();
    for (index, tab) in [HelpTab::Global, HelpTab::CurrentMode, HelpTab::TextEditor].iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("    "));
        }
        let style = if *tab == active {
            button_style(engine, palette.button_active_bg, palette.text_inverse, true)
        } else {
            button_style(engine, palette.button_bg, palette.button_fg, false)
        };
        spans.push(Span::styled(format!(" {} ", help_tab_title(*tab)), style));
    }
    let line = Line::from(spans).alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(line), area);
}

/// Clickable rectangles of the Help modal header bar.
///
/// Rendering and mouse hit-testing derive the tabs from the same layout, so a
/// click on a tab switches it instead of dismissing the dialog
/// (Conventions §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpTabGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// The `Global` tab button.
    pub global_tab: Rect,
    /// The `Current Mode` tab button.
    pub current_mode_tab: Rect,
    /// The `Text Editor` tab button.
    pub text_editor_tab: Rect,
}

/// Computes the Help dialog frame and the header tab rectangles from the same
/// layout [`render_help_popup`] draws.
pub fn help_tab_geometry(term: Rect) -> HelpTabGeometry {
    let area = centered_rect(76, 75, term);
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(inner);
    let bar = chunks[0];

    // The tab bar is one centered line of ` {title} ` chips separated by four
    // spaces, matching `render_help_tab_bar`.
    let titles = [
        help_tab_title(HelpTab::Global),
        help_tab_title(HelpTab::CurrentMode),
        help_tab_title(HelpTab::TextEditor),
    ];
    let widths: [u16; 3] = [
        UnicodeWidthStr::width(titles[0].as_str()) as u16 + 2,
        UnicodeWidthStr::width(titles[1].as_str()) as u16 + 2,
        UnicodeWidthStr::width(titles[2].as_str()) as u16 + 2,
    ];
    let gap = 4u16;
    let total = widths[0] + widths[1] + widths[2] + gap * 2;
    let start = bar.x + bar.width.saturating_sub(total) / 2;
    let y = bar.y;

    let global_tab = Rect::new(start, y, widths[0], 1);
    let current_mode_tab = Rect::new(start + widths[0] + gap, y, widths[1], 1);
    let text_editor_tab = Rect::new(
        start + widths[0] + gap + widths[1] + gap,
        y,
        widths[2],
        1,
    );

    HelpTabGeometry {
        area,
        global_tab,
        current_mode_tab,
        text_editor_tab,
    }
}

/// Converts a legacy key-command list into help rows, projecting `C-` / `M-`.
fn list_help_commands(
    commands: &'static [crate::app::KeyCommand],
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> Vec<HelpCommand> {
    commands
        .iter()
        .map(|command| HelpCommand {
            key: format_command_key(command.key, primary, meta),
            description: onboarding_description(command.key)
                .unwrap_or_else(|| command.description.to_string()),
        })
        .collect()
}

/// Resolves the localized description of a Setup (onboarding) command from the
/// canonical catalog, keyed by the abstract chord declared in
/// [`ONBOARDING_COMMANDS`]. The command list owns the chords and the catalog owns
/// the text, so a hot-swapped locale reaches the Setup Help tab without
/// duplicating either. An unmapped chord falls back to the static English
/// description, so the tab never renders an empty row (c-1, c-5).
fn onboarding_description(raw_key: &str) -> Option<String> {
    let catalog_key = match raw_key {
        "Enter" => "cmd.onboarding.execute",
        "Esc" => "cmd.onboarding.exit_help",
        "C-R" => "cmd.onboarding.toggle_key",
        "C-V" => "cmd.onboarding.paste_key",
        "C-U" => "cmd.onboarding.clear_key_input",
        "C-O" => "cmd.onboarding.reload_config",
        _ => return None,
    };
    Some(active_t(catalog_key))
}

/// Returns the heading and cognitive-ordered rows for the active mode.
///
/// Modes backed by a typed [`CommandMap`] derive their rows from that single
/// source of truth; modes still using a legacy list are projected from it.
#[cfg(test)]
pub fn get_mode_key_commands(
    mode: ViewMode,
    primary: PrimaryModifier,
    meta: MetaModifier,
) -> (&'static str, Vec<HelpCommand>) {
    let title = match mode {
        ViewMode::Repl => "REPL",
        ViewMode::Plans => "Plans",
        ViewMode::Context => "Context",
        ViewMode::History => "History",
        ViewMode::Review => "Review",
        ViewMode::Onboarding => "Setup",
    };
    let sections = mode_help_sections(mode, primary, meta);
    let commands = sections.into_iter().flat_map(|s| s.commands).collect();
    (title, commands)
}

pub fn render_help_popup(
    frame: &mut ratatui::Frame,
    mode: ViewMode,
    tab: HelpTab,
    scroll: &mut u16,
    primary_modifier: PrimaryModifier,
    meta_modifier: MetaModifier,
) {
    let area = centered_rect(76, 75, frame.area());
    let engine = current_engine();
    let palette = engine.palette();

    let title = active_t("dialog.help.title");
    let inner_area = render_modal_container(frame, area, &title, ModalTone::Warning);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(3),
        ])
        .split(inner_area);

    render_help_tab_bar(frame, chunks[0], tab, &engine);

    let body_w = chunks[1].width.max(1) as usize;
    let help_lines = build_help_body(tab, mode, primary_modifier, meta_modifier, body_w);
    let inner_h = chunks[1].height as usize;
    let max_scroll = help_lines
        .len()
        .saturating_sub(inner_h)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);

    let help_widget = Paragraph::new(help_lines).scroll((*scroll, 0));
    frame.render_widget(help_widget, chunks[1]);

    let close_label = format!(" [ {} (Esc / q / ?) ] ", active_t("help.close"));
    let actions = vec![
        Line::from(Span::styled(
            close_label,
            button_style(&engine, palette.button_bg, palette.button_fg, true),
        )),
        Line::from(Span::styled(
            active_t("help.footer"),
            engine.fg(palette.text_muted),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(Alignment::Center);
    frame.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        let mut text = String::new();
        for span in &line.spans {
            text.push_str(&span.content);
        }
        text
    }

    fn rendered_text(tab: HelpTab, mode: ViewMode) -> String {
        build_help_body(tab, mode, PrimaryModifier::Ctrl, MetaModifier::Alt, 100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_tab_defaults_to_global_and_cycles_across_three_tabs() {
        assert_eq!(HelpTab::default(), HelpTab::Global);
        assert_eq!(HelpTab::Global.next(), HelpTab::CurrentMode);
        assert_eq!(HelpTab::CurrentMode.next(), HelpTab::TextEditor);
        assert_eq!(HelpTab::TextEditor.next(), HelpTab::Global);
        assert_eq!(HelpTab::Global.prev(), HelpTab::TextEditor);
        assert_eq!(HelpTab::CurrentMode.prev(), HelpTab::Global);
        assert_eq!(HelpTab::TextEditor.prev(), HelpTab::CurrentMode);
    }

    #[test]
    fn global_tab_renders_the_three_cognitive_tiers() {
        let text = rendered_text(HelpTab::Global, ViewMode::Plans);
        assert!(text.contains("Essential"));
        assert!(text.contains("Interface"));
        assert!(text.contains("Helpers"));
        // Global commands are advertised with their projected chords.
        assert!(text.contains("Cancel the active generation"));
        assert!(text.contains("Ctrl+C"));
        // Mode-specific commands must not be mixed into Global.
        assert!(!text.contains("Execute the selected step"));
    }

    #[test]
    fn current_mode_tab_renders_mode_commands_without_global_leakage() {
        let text = rendered_text(HelpTab::CurrentMode, ViewMode::Plans);
        assert!(text.contains("Essential"));
        assert!(text.contains("Interface"));
        assert!(text.contains("Execute the selected step"));
        // Global commands must not be mixed into CurrentMode.
        assert!(!text.contains("Cancel the active generation"));
    }

    #[test]
    fn text_editor_tab_renders_editor_commands() {
        let text = rendered_text(HelpTab::TextEditor, ViewMode::Plans);
        assert!(text.contains("Kill the word before the cursor"));
        assert!(text.contains("Yank from the kill ring or the clipboard"));
        assert!(!text.contains("Cancel the active generation"));
        assert!(!text.contains("Execute the selected step"));
    }

    #[test]
    fn help_tabs_do_not_leak_commands_into_each_other() {
        let global = rendered_text(HelpTab::Global, ViewMode::Repl);
        assert!(global.contains("Cancel the active generation"));
        assert!(!global.contains("Send the prompt"));
        assert!(!global.contains("Yank from the kill ring or the clipboard"));

        let mode = rendered_text(HelpTab::CurrentMode, ViewMode::Repl);
        assert!(!mode.contains("Cancel the active generation"));
        assert!(mode.contains("Send the prompt"));
        assert!(!mode.contains("Yank from the kill ring or the clipboard"));

        let editor = rendered_text(HelpTab::TextEditor, ViewMode::Repl);
        assert!(!editor.contains("Cancel the active generation"));
        assert!(!editor.contains("Send the prompt"));
        assert!(editor.contains("Yank from the kill ring or the clipboard"));
    }

    #[test]
    fn global_map_binds_the_help_dialog_to_a_command_chord() {
        use crate::command::global::GlobalCommand;

        let spec = GLOBAL_COMMAND_MAP
            .specs()
            .iter()
            .find(|spec| spec.action == GlobalCommand::OpenHelp)
            .expect("the Help dialog must be reachable from the command map");
        let chord = spec.chord.display(PrimaryModifier::Ctrl, MetaModifier::Alt);
        assert!(!chord.contains('F'));
    }

    #[test]
    fn no_command_advertises_a_function_key() {
        let (primary, meta) = (PrimaryModifier::Ctrl, MetaModifier::Alt);
        let mut keys: Vec<String> = GLOBAL_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord.display(primary, meta))
            .collect();
        keys.extend(
            EDITOR_COMMAND_MAP
                .specs()
                .iter()
                .map(|spec| spec.chord.display(primary, meta)),
        );
        for mode in [
            ViewMode::Repl,
            ViewMode::Context,
            ViewMode::History,
            ViewMode::Review,
            ViewMode::Plans,
            ViewMode::Onboarding,
        ] {
            let (_, commands) = get_mode_key_commands(mode, primary, meta);
            keys.extend(commands.iter().map(|command| command.key.clone()));
        }
        for key in keys {
            for index in 1..=10 {
                let fkey = format!("F{}", index);
                assert!(
                    !key.contains(fkey.as_str()),
                    "function key {} must never be bound (found in {:?})",
                    fkey,
                    key,
                );
            }
        }
    }

    /// The tab rectangles used for mouse hit-testing must land exactly on the
    /// drawn tabs.
    #[test]
    fn help_tab_buttons_match_hit_geometry() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let term = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        let mut scroll = 0u16;
        terminal
            .draw(|f| {
                render_help_popup(
                    f,
                    ViewMode::Plans,
                    HelpTab::Global,
                    &mut scroll,
                    PrimaryModifier::Ctrl,
                    MetaModifier::Alt,
                )
            })
            .unwrap();

        let geom = help_tab_geometry(term);
        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };

        assert!(read(geom.global_tab).contains("Global"));
        assert!(read(geom.current_mode_tab).contains("Current Mode"));
        assert!(read(geom.text_editor_tab).contains("Text Editor"));
    }
}
