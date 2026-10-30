//! Keyboard routing for the fullscreen diff inspector.
//!
//! The inspector is a reading surface: it owns the keyboard while it is open and
//! never mutates the staged change set. Every chord is declared once in
//! [`INSPECTOR_COMMAND_MAP`], so the navigation the footer documents and the
//! navigation the dialog intercepts come from a single declaration (c-3, c-4).
//!
//! The card has two panes and one explicit focus: `Tab` / `Shift+Tab` switch it,
//! the vertical motion then walks the audited-file list or scrolls the isolated
//! diff, and the bracket chords switch files from either pane, so reading a
//! change set never requires leaving the pane the developer works in (c-2).
use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{AppState, DiffInspectorFocus, DiffInspectorState};
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
    /// Switch the active pane between the file list and the diff read area.
    ToggleFocus,
    /// Move down inside the focused pane.
    MoveForward,
    /// Move up inside the focused pane.
    MoveBackward,
    /// Highlight the next audited file, whatever pane has focus.
    NextFile,
    /// Highlight the previous audited file, whatever pane has focus.
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
    // The pane switch is the only structural chord of the card: both spellings a
    // terminal reports for `Shift+Tab` reach the same reversible toggle (c-2).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        InspectorCommand::ToggleFocus,
        Interface,
        "cmd.diff_inspector.toggle_focus",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Tab),
        InspectorCommand::ToggleFocus,
        Interface,
        "cmd.diff_inspector.toggle_focus",
    ),
    // Vertical motion is focus aware: the same chord walks the audited-file list
    // while it owns the keyboard and scrolls the patch once `Tab` hands it over.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        InspectorCommand::MoveForward,
        Interface,
        "cmd.diff_inspector.move_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        InspectorCommand::MoveBackward,
        Interface,
        "cmd.diff_inspector.move_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        InspectorCommand::MoveForward,
        Interface,
        "cmd.diff_inspector.move_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        InspectorCommand::MoveBackward,
        Interface,
        "cmd.diff_inspector.move_up",
    ),
    // `j` / `k` belong to the patch alone, so the read area stays reachable from
    // either pane without switching focus.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('j')),
        InspectorCommand::ScrollDown,
        Interface,
        "cmd.diff_inspector.scroll_down",
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
    // The bracket chords switch files at any time, so the audited set stays
    // walkable without leaving the pane that owns the keyboard (c-2).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('[')),
        InspectorCommand::PrevFile,
        Interface,
        "cmd.diff_inspector.prev_file",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(']')),
        InspectorCommand::NextFile,
        Interface,
        "cmd.diff_inspector.next_file",
    ),
]);

/// Applies one key press to the open inspector.
///
/// The reading position of the Develop viewport is never touched, so closing the
/// dialog returns the developer to the exact spot the audit was opened from
/// (c-3). The selected file keeps its own scroll offset, and moving the
/// highlight resets it so a shorter patch never inherits a stale offset.
///
/// The vertical motion is focus aware: the file list walks its highlight while it
/// owns the keyboard, and the diff read area scrolls its own body once `Tab`
/// hands the keyboard over (c-2). The bracket chords switch files from either
/// pane, so reading a change set never requires switching focus first.
pub(super) fn handle_diff_inspector_key(key: KeyEvent, st: &mut AppState) {
    // The whole workstation input configuration travels with the key, so the
    // configured layout and langmap reach both the bare and the modified chords
    // (Conventions §5.1).
    let input: InputConfig = st.tui_config.input.clone();
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
        InspectorCommand::ToggleFocus => inspector.focus.toggle(),
        InspectorCommand::MoveForward => match inspector.focus {
            DiffInspectorFocus::Files => step_file(inspector, true),
            DiffInspectorFocus::Diff => scroll_diff(inspector, 1, true),
        },
        InspectorCommand::MoveBackward => match inspector.focus {
            DiffInspectorFocus::Files => step_file(inspector, false),
            DiffInspectorFocus::Diff => scroll_diff(inspector, 1, false),
        },
        InspectorCommand::NextFile => step_file(inspector, true),
        InspectorCommand::PrevFile => step_file(inspector, false),
        InspectorCommand::ScrollUp => scroll_diff(inspector, 1, false),
        InspectorCommand::ScrollDown => scroll_diff(inspector, 1, true),
        InspectorCommand::PageUp => scroll_diff(inspector, INSPECTOR_PAGE_STEP, false),
        InspectorCommand::PageDown => scroll_diff(inspector, INSPECTOR_PAGE_STEP, true),
    }
}

/// Moves the audited-file highlight by one row, resetting the isolated diff read
/// area so a shorter patch never inherits a stale offset.
fn step_file(inspector: &mut DiffInspectorState, forward: bool) {
    let last = inspector.files.len().saturating_sub(1);
    let target = if forward {
        (inspector.selected_file + 1).min(last)
    } else {
        inspector.selected_file.saturating_sub(1)
    };
    if target != inspector.selected_file {
        inspector.selected_file = target;
        inspector.diff_scroll = 0;
    }
}

/// Moves the isolated diff read area by `lines` in the requested direction.
///
/// The renderer clamps the resulting offset to the body it actually draws, so the
/// scroll can never run past the last rendered row (c-3).
fn scroll_diff(inspector: &mut DiffInspectorState, lines: u16, down: bool) {
    inspector.diff_scroll = if down {
        inspector.diff_scroll.saturating_add(lines)
    } else {
        inspector.diff_scroll.saturating_sub(lines)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DiffInspectorFocus, DiffInspectorState};
    use crate::views::repl::{EditOp, FileEditStatus, StreamingFileEdit};
    use crossterm::event::KeyModifiers;

    fn file(path: &str) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: Vec::new(),
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
            focus: DiffInspectorFocus::Files,
        });
        st
    }

    fn press(st: &mut AppState, code: KeyCode) {
        handle_diff_inspector_key(KeyEvent::new(code, KeyModifiers::NONE), st);
    }

    fn press_shifted(st: &mut AppState, code: KeyCode) {
        handle_diff_inspector_key(KeyEvent::new(code, KeyModifiers::SHIFT), st);
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

    /// `Tab` and both spellings of `Shift+Tab` hand the keyboard to the other
    /// pane, so the active pane is always an explicit, reversible state (c-2).
    #[test]
    fn tab_switches_the_active_pane() {
        let mut st = state_with_inspector();
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.focus),
            Some(DiffInspectorFocus::Files),
            "the file list owns the keyboard when the audit opens"
        );

        press(&mut st, KeyCode::Tab);
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.focus),
            Some(DiffInspectorFocus::Diff)
        );

        press_shifted(&mut st, KeyCode::Tab);
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.focus),
            Some(DiffInspectorFocus::Files)
        );

        // A terminal reporting the dedicated `BackTab` code reaches the same pane.
        press_shifted(&mut st, KeyCode::BackTab);
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.focus),
            Some(DiffInspectorFocus::Diff)
        );
    }

    /// The vertical motion follows the focused pane: the list walks its highlight
    /// on the left, and the read area scrolls its own body on the right without
    /// moving the highlight (c-2).
    #[test]
    fn arrows_and_paging_scroll_the_isolated_diff_while_it_has_focus() {
        let mut st = state_with_inspector();
        press(&mut st, KeyCode::Tab);

        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Char('j'));
        assert_eq!(
            st.diff_inspector.as_ref().map(|dialog| dialog.diff_scroll),
            Some(2)
        );
        assert_eq!(
            st.diff_inspector
                .as_ref()
                .map(|dialog| dialog.selected_file),
            Some(1),
            "scrolling the patch never moves the highlight behind it"
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

    /// The bracket chords switch files from either pane, so reading a change set
    /// never requires switching focus first (c-2).
    #[test]
    fn brackets_switch_files_from_either_pane() {
        let mut st = state_with_inspector();

        press(&mut st, KeyCode::Char('['));
        assert_eq!(
            st.diff_inspector
                .as_ref()
                .map(|dialog| dialog.selected_file),
            Some(0)
        );
        press(&mut st, KeyCode::Char(']'));
        assert_eq!(
            st.diff_inspector
                .as_ref()
                .map(|dialog| dialog.selected_file),
            Some(1)
        );

        // The patch keeps the keyboard, and the file chords keep working.
        press(&mut st, KeyCode::Tab);
        press(&mut st, KeyCode::Down);
        press(&mut st, KeyCode::Char('['));
        let inspector = st.diff_inspector.as_ref().expect("the dialog stays open");
        assert_eq!(inspector.selected_file, 0);
        assert_eq!(
            inspector.diff_scroll, 0,
            "a new file opens at its first line"
        );
        assert_eq!(inspector.focus, DiffInspectorFocus::Diff);
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
