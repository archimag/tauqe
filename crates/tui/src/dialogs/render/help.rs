use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::ViewMode;

use crate::ui::centered_rect;

pub const GLOBAL_COMMANDS: &[crate::app::KeyCommand] = &[
    // Core Actions: the primary work cycle.
    crate::app::KeyCommand { key: "F6 / C-S", description: "Squash commits dialog" },
    crate::app::KeyCommand { key: "Ctrl+C", description: "Cancel active generation" },
    // Navigation & Inspection.
    crate::app::KeyCommand { key: "C-1..5 / F1..F5", description: "Switch views: REPL │ Plans │ Review │ Context │ History" },
    crate::app::KeyCommand { key: "C-R", description: "Fold / unfold thinking (reasoning) stream" },
    crate::app::KeyCommand { key: crate::app::HELP_OPEN_CHORD, description: "Open this help dialog (? also opens it outside text input); Esc / q close it" },
    // Auxiliary & Destructive.
    crate::app::KeyCommand { key: "C-M / Alt+M", description: "Select active model" },
    crate::app::KeyCommand { key: "C-O", description: "Reload configuration from disk" },
    crate::app::KeyCommand { key: "Ctrl+Q", description: "Quit TAUQE" },
];

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

pub fn get_mode_key_commands(mode: ViewMode) -> (&'static str, &'static [crate::app::KeyCommand]) {
    match mode {
        ViewMode::Develop => (" Help: REPL ", crate::views::develop::DEVELOP_COMMANDS),
        ViewMode::Context => (" Help: Context ", crate::views::context::CONTEXT_COMMANDS),
        ViewMode::History => (" Help: History ", crate::views::history::HISTORY_COMMANDS),
        ViewMode::Review => (" Help: Review ", crate::views::review::REVIEW_COMMANDS),
        ViewMode::Plans => (" Help: Plans ", crate::views::plans::PLANS_COMMANDS),
        ViewMode::Onboarding => (" Help: Setup ", ONBOARDING_COMMANDS),
    }
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let current_len = current.chars().count();
        let word_len = word.chars().count();
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

fn format_command_key(raw_key: &str, primary_modifier: crate::config::PrimaryModifier) -> String {
    let mod_prefix = match primary_modifier {
        crate::config::PrimaryModifier::Ctrl => "Ctrl+",
        crate::config::PrimaryModifier::Alt => "Alt+",
    };
    raw_key.replace("C-", mod_prefix)
}

fn push_command_lines(
    lines: &mut Vec<Line<'static>>,
    commands: &[crate::app::KeyCommand],
    col_w: usize,
    desc_w: usize,
    primary_modifier: crate::config::PrimaryModifier,
) {
    for cmd in commands {
        let display_key = format_command_key(cmd.key, primary_modifier);
        let mut wrapped = wrap_words(cmd.description, desc_w).into_iter();
        let first = wrapped.next().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<col_w$}", display_key), Style::default().fg(Color::White).bold()),
            Span::styled(first, Style::default().fg(Color::Gray)),
        ]));
        for rest in wrapped {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(2 + col_w)),
                Span::styled(rest, Style::default().fg(Color::Gray)),
            ]));
        }
    }
}

pub fn render_help_popup(
    frame: &mut ratatui::Frame,
    mode: ViewMode,
    scroll: &mut u16,
    primary_modifier: crate::config::PrimaryModifier,
) {
    let (title, mode_commands) = get_mode_key_commands(mode);
    let area = centered_rect(76, 75, frame.area());
    frame.render_widget(Clear, area);

    let key_w = GLOBAL_COMMANDS
        .iter()
        .chain(mode_commands.iter())
        .map(|c| format_command_key(c.key, primary_modifier).chars().count())
        .max()
        .unwrap_or(0);
    let col_w = key_w + 2;
    let inner_w = area.width.saturating_sub(2) as usize;
    let desc_w = inner_w.saturating_sub(2 + col_w).max(10);

    let mut help_lines: Vec<Line<'static>> = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Global Navigation",
            Style::default().fg(Color::Cyan).bold(),
        )),
    ];
    push_command_lines(&mut help_lines, GLOBAL_COMMANDS, col_w, desc_w, primary_modifier);

    help_lines.push(Line::raw(""));
    help_lines.push(Line::from(Span::styled(
        "Active Mode Shortcuts",
        Style::default().fg(Color::Yellow).bold(),
    )));
    push_command_lines(&mut help_lines, mode_commands, col_w, desc_w, primary_modifier);

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(3)])
        .split(inner_area);

    let inner_h = chunks[0].height as usize;
    let max_scroll = help_lines
        .len()
        .saturating_sub(inner_h)
        .min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);

    let help_widget = Paragraph::new(help_lines).scroll((*scroll, 0));
    frame.render_widget(help_widget, chunks[0]);

    let actions = vec![
        Line::from(Span::styled(
            " [ Close Help (Esc / q / ?) ] ",
            Style::default().bg(Color::DarkGray).fg(Color::White).bold(),
        )),
        Line::from(Span::styled(
            "n/p or j/k scroll • PgUp/PgDn page scroll • Home top • Esc/q close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_help_command_advertises_real_binding() {
        let entry = GLOBAL_COMMANDS
            .iter()
            .find(|c| c.description.contains("help dialog"))
            .expect("the Help dialog command must be documented");
        // Help opens with the primary-modifier chord, never with F1 (which switches
        // to the Develop tab) and never with a bare `?` inside a text input.
        assert_eq!(entry.key, crate::app::HELP_OPEN_CHORD);
        assert!(!entry.key.contains("F1"));
    }
}
