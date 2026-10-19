//! Input handling for the interface-language (locale) dialog.
//!
//! The developer names a target language in free form and confirms; the request
//! is forwarded to the daemon over JSON-RPC because the client never contacts a
//! provider directly (c-4). Choosing "Reset to English" only deletes the single
//! custom-locale file and reverts the engine, a purely client-side operation
//! (c-4, c-5).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, LocaleGenerateParams};
use tokio::sync::MutexGuard;

use crate::app::{AppState, LocaleDialogButton, LocaleDialogFocus, LocaleDialogState};
use crate::i18n::{active_t, canonical_catalog, delete_custom_locale};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

/// Routes key events for the locale dialog.
///
/// `Tab` / `Shift+Tab` move between the language field and the action bar,
/// `Enter` in the field or on the focused `Synthesize` button launches
/// synthesis, `←` / `→` step between action buttons, and `Esc` dismisses the
/// dialog without a change.
pub(super) async fn handle_locale_dialog(
    key: KeyEvent,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let mut dialog = match st.locale_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(None),
    };

    let is_ctrl_alt =
        key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::ALT);
    let is_cmd = !is_ctrl_alt
        && (st.tui_config.input.primary_modifier.matches(key.modifiers)
            || key.modifiers.contains(KeyModifiers::CONTROL));

    // When generation is in flight, block input except for explicit cancellation via Esc.
    if dialog.generating {
        match key.code {
            KeyCode::Esc => {
                cancel_synthesis(st, server_writer).await?;
                return Ok(Some(InputResult::Continue));
            }
            _ => {
                st.locale_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    if dialog.focus.is_actions() {
        match key.code {
            // Dismissal: the dialog stays dropped (safe cancellation).
            KeyCode::Esc | KeyCode::Char('q') => return Ok(Some(InputResult::Continue)),
            KeyCode::Tab => {
                dialog.focus = LocaleDialogFocus::Editor;
                st.locale_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::BackTab | KeyCode::Left => {
                dialog.button.cycle_prev();
                st.locale_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Right => {
                dialog.button.cycle_next();
                st.locale_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return activate(st, dialog, server_writer).await;
            }
            _ => {
                st.locale_dialog = Some(dialog);
                return Ok(Some(InputResult::Continue));
            }
        }
    }

    // The single-line language field owns the keyboard.
    match key.code {
        // Dismissal without a change.
        KeyCode::Esc => Ok(Some(InputResult::Continue)),
        KeyCode::Tab | KeyCode::BackTab => {
            dialog.focus = LocaleDialogFocus::Actions;
            st.locale_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
        // A bare `Enter` synthesizes, mirroring the discussion prompt modal;
        // `Shift+Enter` and `Alt+Enter` stay free for future multi-line input.
        KeyCode::Enter
            if !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            synthesize(st, dialog, server_writer).await
        }
        _ => {
            crate::input::handle_editor_key(&mut dialog.language_editor, key, is_cmd, false);
            st.locale_dialog = Some(dialog);
            Ok(Some(InputResult::Continue))
        }
    }
}

/// Executes the action button that currently holds focus.
async fn activate(
    st: MutexGuard<'_, AppState>,
    dialog: LocaleDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    match dialog.button {
        LocaleDialogButton::Synthesize => synthesize(st, dialog, server_writer).await,
        LocaleDialogButton::ResetEnglish => reset_to_english(st),
        LocaleDialogButton::Cancel => Ok(Some(InputResult::Continue)),
    }
}

/// Sends `locale/generate` carrying the canonical English catalog (c-1, c-2, c-4).
async fn synthesize(
    mut st: MutexGuard<'_, AppState>,
    mut dialog: LocaleDialogState,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<Option<InputResult>> {
    let language = dialog.language_editor.get_text().trim().to_string();
    if language.is_empty() {
        st.notify_warning(active_t("notify.locale_empty_target"));
        st.locale_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    if st.model.is_busy() || st.review.running {
        st.notify_warning(active_t("notify.model_busy"));
        st.locale_dialog = Some(dialog);
        return Ok(Some(InputResult::Continue));
    }

    let params = LocaleGenerateParams {
        language: language.clone(),
        items: canonical_catalog(),
    };
    st.notify_info(active_t("notify.locale_synthesizing").replace("{}", &language));
    dialog.generating = true;
    st.locale_dialog = Some(dialog);
    drop(st);
    send_request(
        server_writer,
        methods::LOCALE_GENERATE,
        serde_json::to_value(params)?,
    )
    .await?;
    Ok(Some(InputResult::Continue))
}

/// Cancels an in-flight locale synthesis and dismisses the dialog (c-4).
///
/// Shared by the keyboard `Esc` path and the mouse click on `Cancel`, so both
/// interrupt the Scribe request identically. Never sends `MODEL_CANCEL` if an
/// unrelated model turn or review is active, preventing accidental abortion
/// of concurrent operations.
async fn cancel_synthesis(
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let unrelated_busy = st.model.is_busy() || st.review.running;
    st.locale_dialog = None;
    st.notify_info(active_t("notify.locale_cancelled"));
    drop(st);
    if !unrelated_busy {
        let _ = send_request(
            server_writer,
            methods::MODEL_CANCEL,
            serde_json::to_value(tauqe_protocol::ModelCancelParams {
                mode: Some(tauqe_protocol::CancelMode::Hard),
            })?,
        )
        .await;
    }
    Ok(())
}

/// Activates a locale dialog action button from the mouse path.
///
/// The mouse peer of focusing a button and pressing `Enter`: clicking
/// `Synthesize`, `Reset to English`, or `Cancel` runs the same transition the
/// keyboard does, so the dialog can be driven entirely by the pointer
/// (Conventions §6.6). While synthesis is in flight only `Cancel` acts,
/// mirroring the keyboard gate on input.
pub async fn click_locale_button(
    mut st: MutexGuard<'_, AppState>,
    button: LocaleDialogButton,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let Some(generating) = st.locale_dialog.as_ref().map(|dialog| dialog.generating) else {
        return Ok(());
    };

    if generating {
        if matches!(button, LocaleDialogButton::Cancel) {
            cancel_synthesis(st, server_writer).await?;
        }
        return Ok(());
    }

    let mut dialog = match st.locale_dialog.take() {
        Some(dialog) => dialog,
        None => return Ok(()),
    };
    dialog.button = button;
    let _ = activate(st, dialog, server_writer).await?;
    Ok(())
}

/// Deletes the single custom-locale file and reverts the engine to English (c-4, c-5).
fn reset_to_english(mut st: MutexGuard<'_, AppState>) -> anyhow::Result<Option<InputResult>> {
    st.reset_locale_to_english();
    match delete_custom_locale() {
        Ok(()) => st.notify_success(active_t("notify.locale_reset_english")),
        Err(err) => st.notify_warning(
            active_t("notify.locale_reset_unsaved").replace("{}", &err.to_string()),
        ),
    }
    Ok(Some(InputResult::Continue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    async fn dispatch(
        state: &Arc<Mutex<AppState>>,
        key: KeyEvent,
    ) -> anyhow::Result<Option<InputResult>> {
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_locale_dialog(key, guard, &mut conn).await
    }

    #[tokio::test]
    async fn empty_language_keeps_the_dialog_and_warns() {
        let mut st = crate::app::test_state();
        st.locale_dialog = Some(LocaleDialogState::default());
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Enter)).await.unwrap();

        let st = state.lock().await;
        assert!(
            st.locale_dialog.is_some(),
            "the dialog stays open for correction"
        );
        let notif = st.active_notification().expect("a warning is surfaced");
        assert!(notif.text.contains("target language"));
    }

    #[tokio::test]
    async fn enter_with_a_language_dispatches_and_keeps_dialog_in_generating_state() {
        let mut st = crate::app::test_state();
        let mut dialog = LocaleDialogState::default();
        dialog.language_editor.insert_str("Elvish");
        st.locale_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Enter)).await.unwrap();

        let st = state.lock().await;
        let d = st
            .locale_dialog
            .as_ref()
            .expect("dialog stays open while generating");
        assert!(d.generating, "dialog transitions to generating state");
        let notif = st
            .active_notification()
            .expect("a status notification is surfaced");
        assert!(notif.text.contains("Elvish"));
    }

    #[tokio::test]
    async fn escape_dismisses_without_a_request() {
        let mut st = crate::app::test_state();
        st.locale_dialog = Some(LocaleDialogState::default());
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Esc)).await.unwrap();

        let st = state.lock().await;
        assert!(st.locale_dialog.is_none());
        assert!(st.active_notification().is_none());
    }

    #[tokio::test]
    async fn pressing_q_while_generating_does_not_cancel() {
        let mut st = crate::app::test_state();
        let dialog = LocaleDialogState {
            generating: true,
            ..Default::default()
        };
        st.locale_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Char('q'))).await.unwrap();

        let st = state.lock().await;
        let d = st.locale_dialog.as_ref().expect("dialog remains present");
        assert!(d.generating, "generating state preserved");
    }

    #[tokio::test]
    async fn pressing_esc_while_generating_cancels_synthesis() {
        let mut st = crate::app::test_state();
        let dialog = LocaleDialogState {
            generating: true,
            ..Default::default()
        };
        st.locale_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Esc)).await.unwrap();

        let st = state.lock().await;
        assert!(st.locale_dialog.is_none(), "dialog dismissed on Esc");
        let notif = st.active_notification().expect("notification present");
        assert!(notif.text.contains("cancelled"));
    }

    #[tokio::test]
    async fn typing_a_character_lands_in_the_language_field() {
        let mut st = crate::app::test_state();
        st.locale_dialog = Some(LocaleDialogState::default());
        let state = Arc::new(Mutex::new(st));

        dispatch(&state, key(KeyCode::Char('Р'))).await.unwrap();

        let st = state.lock().await;
        let dialog = st.locale_dialog.as_ref().expect("dialog stays open");
        assert_eq!(dialog.language_editor.get_text(), "Р");
    }
}
