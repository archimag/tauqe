//! Keyboard routing for the fullscreen diff inspector.
//!
//! The inspector is a reading surface: it owns the keyboard while it is open and
//! never mutates the staged change set. Every chord is declared once in
//! [`INSPECTOR_COMMAND_MAP`], so the navigation the footer documents and the
//! navigation the dialog intercepts come from a single declaration (c-3, c-4).

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::AppState;
use crate::command::CommandCategory::Interface;
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::config::InputConfig;

/// Lines a single page scroll moves the isolated diff read area.
const INSPECTOR_PAGE_STEP: u16 = 10;

/// Operations of the diff inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InspectorCommand {
    /// Close the inspector and return to the Develop view.
    Close,
    /// Highlight the next audited file.
    NextFile,
    /// Highlight the previous audited file.
    PrevFile,
    /// Scroll the diff read area up one line.
    ScrollUp,
    /// Scroll the diff read area down one line.
    ScrollDown,
    /// Scroll the diff read area up one page.
    PageUp,
    /// Scroll the diff read area down one page.
    PageDown,
}

/// Single source of truth for the inspector keybindings.
///
/// The map is scoped to this dialog because no other surface accepts a bare
/// `n` / `p` or a bare arrow without a text buffer owning those keys.
pub(super) static INSPECTOR_COMMAND_MAP: CommandMap<InspectorCommand> = CommandMap::new(&[
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        InspectorCommand::Close,
        Interface,
        "cmd.diff_inspector.close",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        InspectorCommand::Close,
        Interface,
        "cmd.diff_inspector.close",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        InspectorCommand::NextFile,
        Interface,
        "cmd.diff_inspector.next_file",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        InspectorCommand::PrevFile,
        Interface,
        "cmd.diff_inspector.prev_file",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        InspectorCommand::ScrollDown,
        Interface,
        "cmd.diff_inspector.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        InspectorCommand::ScrollDown,
        Interface,
        "cmd.diff_inspector.scroll_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        InspectorCommand::ScrollUp,
        Interface,
        "cmd.diff_inspector.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('k')),
        InspectorCommand::ScrollUp,
        Interface,
        "cmd.diff_inspector.scroll_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        InspectorCommand::PageDown,
        Interface,
        "cmd.diff_inspector.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        InspectorCommand::PageUp,
        Interface,
        "cmd.diff_inspector.page_up",
    ),
]);

/// Applies one key press to the open inspector.
///
/// The reading position of the Develop viewport is never touched, so closing the
/// dialog returns the developer to the exact spot the audit was opened from
/// (c-3). The selected file keeps its own scroll offset, and moving the
/// highlight resets it so a shorter patch never inherits a stale offset.
pub(super) fn handle_diff_inspector_key(key: KeyEvent, st: &mut AppState) {
    let input = InputConfig {
        primary_modifier: st.tui_config.input.primary_modifier,
        meta_modifier: st.tui_config.input.meta_modifier,
        ..InputConfig::default()
    };
    let Some(command) = INSPECTOR_COMMAND_MAP
        .resolve(key, &input, false)
        .map(|spec| spec.action)
    else {
        return;
    };

    if command == InspectorCommand::Close {
        st.diff_inspector = None;
        return;
    }

    let Some(inspector) = st.diff_inspector.as_mut() else {
        return;
    };

    match command {
        // Handled above, kept explicit so a new command can never fall through.
        InspectorCommand::Close => {}
        InspectorCommand::NextFile => {
            if inspector.selected_file + 1 < inspector.files.len() {
                inspector.selected_file += 1;
                inspector.diff_scroll = 0;
            }
        }
        InspectorCommand::PrevFile => {
            if inspector.selected_file > 0 {
                inspector.selected_file -= 1;
                inspector.diff_scroll = 0;
            }
        }
        InspectorCommand::ScrollUp => {
            inspector.diff_scroll = inspector.diff_scroll.saturating_sub(1);
        }
        InspectorCommand::ScrollDown => {
            inspector.diff_scroll = inspector.diff_scroll.saturating_add(1);
        }
        InspectorCommand::PageUp => {
            inspector.diff_scroll = inspector.diff_scroll.saturating_sub(INSPECTOR_PAGE_STEP);
        }
        InspectorCommand::PageDown => {
            inspector.diff_scroll = inspector.diff_scroll.saturating_add(INSPECTOR_PAGE_STEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DiffInspectorState;
    use crate::views::repl::{EditOp, FileEditStatus, StreamingFileEdit};
    use crossterm::event::KeyModifiers;

    fn file(path: &str) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: Vec::new(),
            expanded: true,
            retry_info: None,
        }
    }

    fn state_with_inspector() -> AppState {
        let mut st = crate::app::test_state();
        st.model.files = vec![file("crates/tui/src/a.rs"), file("crates/tui/src/b.rs")];
        st.model.selected_file_index = 1;
        st.model.scroll = 7;
        st.diff_inspector = Some(DiffInspectorState {
            files: st.model.files.clone(),
            selected_file: st.model.selected_file_index,
            diff_scroll: 0,
        });
        st
    }

    fn press(st: &mut AppState, code: KeyCode) {
        handle_diff_inspector_key(KeyEvent::new(code, KeyModifiers::NONE), st);
    }

    #[test]
    fn n_and_p_walk_the_audited_files_and_reset_the_scroll() {
        let mut st = state_with_inspector();
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Down);

        press(&mut st, KeyCode::Char('p'));
        let inspector = st.diff_inspector.as_ref().expect("the dialog stays open");
        assert_eq!(inspector.selected_file, 0);
        assert_eq!(inspector.diff_scroll, 0);

        press(&mut st, KeyCode::Char('n'));
        press(&mut st, KeyCode::Char('n'));
        let inspector = st.diff_inspector.as_ref().expect("the dialog stays open");
        assert_eq!(inspector.selected_file, 1);
        assert_eq!(inspector.diff_scroll, 0, "the last file caps the walk");
    }

    #[test]
    fn arrows_and_paging_scroll_the_isolated_diff() {
        let mut st = state_with_inspector();

        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Char('j'));
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.diff_scroll),
            Some(2)
        );

        press(&mut st, KeyCode::PageDown);
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.diff_scroll),
            Some(2 + INSPECTOR_PAGE_STEP)
        );

        press(&mut st, KeyCode::PageUp);
        press(&mut st, KeyCode::Up);
        press(&mut st, KeyCode::Char('k'));
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.diff_scroll),
            Some(0)
        );
    }

    #[test]
    fn esc_and_q_close_the_inspector_without_moving_the_viewport() {
        for close in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut st = state_with_inspector();
            press(&mut st, close);

            assert!(st.diff_inspector.is_none());
            assert_eq!(st.model.scroll, 7, "the reading position is preserved");
            assert_eq!(st.model.selected_file_index, 1);
        }
    }
}
