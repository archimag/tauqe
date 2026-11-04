use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::methods;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, OnboardingStep, ViewMode};
use crate::i18n::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

/// Fills the `{}` placeholders of a localized template in order.
fn fill(template: &str, args: &[&str]) -> String {
    args.iter()
        .fold(template.to_string(), |acc, arg| acc.replacen("{}", arg, 1))
}

/// Outcome of the blocking workstation persistence job.
///
/// The job owns both filesystem steps — the `tui.toml` rewrite and the re-read
/// of the written file — so the apply phase performs no I/O and only selects
/// the message the outcome resolves to (c-1).
enum WorkstationSaveOutcome {
    /// The configuration was written and the written file re-read cleanly.
    Saved {
        path: String,
        config: crate::config::TuiConfig,
    },
    /// The write itself failed; `target` is the path it would have replaced.
    SaveFailed { target: String, error: String },
    /// The write succeeded but the written file could not be re-read.
    LoadFailed { path: String, error: String },
}

/// Persists the chosen workstation settings and reloads them, advancing to the
/// Ready step only when both the write and the re-read succeed.
///
/// The guard is consumed here because the persistence is blocking filesystem
/// work: `save_tui_config_full` re-reads, re-parses, and atomically rewrites
/// `tui.toml`, and `TuiConfig::load_checked` reads and parses the written file
/// again. Both run as one blocking-pool job with the application state guard
/// released, and the result is applied after the guard is re-acquired, so
/// rendering, RPC message processing, and every other locker of the state never
/// queue behind the disk (c-1).
async fn save_workstation_config(
    state: &Arc<Mutex<AppState>>,
    st: MutexGuard<'_, AppState>,
    modifier: crate::config::PrimaryModifier,
) {
    // The two command chords are paired: whichever key becomes the primary
    // modifier leaves the other key as the meta modifier, so `C-x` and `M-x`
    // never collide on one physical key. The rule lives on the modifier type
    // itself, so every writer of `tui.toml` states it identically
    // (Conventions §5.2).
    let meta = modifier.paired_meta();
    let layout = st.onboarding.chosen_layout;
    let langmap = st.onboarding.chosen_langmap.clone();
    let target = st.onboarding.default_tui_config_path.clone();
    drop(st);

    let saved = tokio::task::spawn_blocking(move || {
        match crate::config::save_tui_config_full(layout, langmap, modifier, meta) {
            Ok(path) => match crate::config::TuiConfig::load_checked() {
                Ok(config) => WorkstationSaveOutcome::Saved {
                    path: path.display().to_string(),
                    config,
                },
                Err(err) => WorkstationSaveOutcome::LoadFailed {
                    path: path.display().to_string(),
                    error: err,
                },
            },
            Err(err) => WorkstationSaveOutcome::SaveFailed {
                target,
                error: err.to_string(),
            },
        }
    })
    .await;

    let mut st = state.lock().await;
    let outcome = match saved {
        Ok(outcome) => outcome,
        // A panicking persistence task still reports a failure instead of
        // leaving the step silently unresolved.
        Err(err) => WorkstationSaveOutcome::SaveFailed {
            target: st.onboarding.default_tui_config_path.clone(),
            error: err.to_string(),
        },
    };

    match outcome {
        WorkstationSaveOutcome::Saved { path, config } => {
            st.tui_config = config;
            st.onboarding.has_tui_config = true;
            st.onboarding.set_status(fill(
                &active_t("notify.onboarding_saved"),
                &[&path, modifier.emacs_prefix(), meta.emacs_prefix()],
            ));
            st.onboarding.step = st.onboarding.next_step();
            st.onboarding.selected_index = 0;
        }
        WorkstationSaveOutcome::LoadFailed { path, error } => {
            st.onboarding.set_error(fill(
                &active_t("notify.onboarding_load_failed"),
                &[&path, &error],
            ));
        }
        WorkstationSaveOutcome::SaveFailed { target, error } => {
            st.onboarding.set_error(fill(
                &active_t("notify.onboarding_save_failed"),
                &[&target, &error],
            ));
        }
    }
}

pub async fn handle_onboarding_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    if st.onboarding.input_active {
        // Clipboard and reveal commands follow the abstract command modifiers:
        // either the primary chord (`C-`) or the meta chord (`M-`) drives them.
        // The AltGr / compose chord is excluded inside the abstract checks, and
        // the paired meta modifier keeps a Ctrl fallback working when the
        // primary modifier is configured as Alt (Conventions §5.2).
        let is_cmd = st.tui_config.input.is_primary(key.modifiers)
            || st.tui_config.input.is_meta(key.modifiers);

        if is_cmd {
            match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    st.onboarding.show_key = !st.onboarding.show_key;
                    return Ok(InputResult::Continue);
                }
                KeyCode::Char('v')
                | KeyCode::Char('V')
                | KeyCode::Char('y')
                | KeyCode::Char('Y') => {
                    // The paste buffer is a native round trip bounded at
                    // NATIVE_CLIPBOARD_TIMEOUT. The guard is released while it
                    // is read, so a hung clipboard owner can never freeze
                    // rendering or block another locker of the state (c-1).
                    drop(st);
                    let clipboard = crate::clipboard::get_from_clipboard_async().await;
                    let mut st = state.lock().await;
                    match clipboard {
                        Some(clip_text) => {
                            st.onboarding.input_buffer.insert_str(clip_text.trim());
                        }
                        None => {
                            st.onboarding
                                .set_error(active_t("notify.onboarding_clipboard_empty"));
                        }
                    }
                    return Ok(InputResult::Continue);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                st.onboarding.input_active = false;
                st.onboarding.input_langmap = false;
                st.onboarding.input_buffer.clear();
                st.onboarding.show_key = false;
                st.onboarding.clear_messages();
            }
            KeyCode::Enter => {
                let input = st.onboarding.input_buffer.get_text().trim().to_string();

                if st.onboarding.input_langmap {
                    st.onboarding.input_active = false;
                    st.onboarding.input_buffer.clear();
                    st.onboarding.input_langmap = false;
                    if !input.is_empty() {
                        st.onboarding.chosen_layout = crate::config::LayoutPreset::None;
                        st.onboarding.chosen_langmap = Some(input);
                    }
                    st.onboarding.step = OnboardingStep::Modifier;
                    st.onboarding.selected_index = 0;
                    st.onboarding.clear_messages();
                    return Ok(InputResult::Continue);
                }

                match st.onboarding.step {
                    OnboardingStep::Credentials => {
                        if input.is_empty() {
                            st.onboarding
                                .set_error(active_t("notify.onboarding_key_empty"));
                            return Ok(InputResult::Continue);
                        }
                        st.onboarding.input_active = false;
                        st.onboarding.input_buffer.clear();
                        st.onboarding.clear_messages();
                        drop(st);
                        send_request(
                            server_writer,
                            methods::CREDENTIALS_SAVE,
                            serde_json::json!({ "api_key": input }),
                        )
                        .await?;
                    }
                    _ => {
                        st.onboarding.input_active = false;
                        st.onboarding.input_buffer.clear();
                    }
                }
            }
            _ => {
                if st.onboarding.input_buffer.takes_yank_chord(key, is_cmd) {
                    // The paste buffer is a native round trip bounded at
                    // NATIVE_CLIPBOARD_TIMEOUT, so it is read with the state
                    // guard released and the answer is applied afterwards
                    // (c-1).
                    drop(st);
                    let clipboard = crate::clipboard::get_from_clipboard_async().await;
                    let mut st = state.lock().await;
                    st.onboarding.input_buffer.yank_with(clipboard);
                    return Ok(InputResult::Continue);
                }
                st.onboarding.input_buffer.handle_key(key, is_cmd);
            }
        }
    } else {
        let total_options = match st.onboarding.step {
            OnboardingStep::Git => 3,
            OnboardingStep::Config => 2,
            OnboardingStep::Credentials => 4,
            OnboardingStep::Workstation => 4,
            OnboardingStep::Modifier => 2,
            OnboardingStep::Gatekeeper => 3,
            OnboardingStep::Ready => 1,
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                st.onboarding.clear_messages();
                if st.onboarding.step == OnboardingStep::Config {
                    st.onboarding.config_skipped = true;
                }
                if st.onboarding.step == OnboardingStep::Gatekeeper
                    || st.onboarding.step == OnboardingStep::Git
                {
                    return Ok(InputResult::Exit);
                } else if st.onboarding.step == OnboardingStep::Modifier {
                    st.onboarding.step = OnboardingStep::Workstation;
                    st.onboarding.selected_index = 0;
                } else if st.onboarding.has_api_key {
                    st.view_mode = ViewMode::Repl;
                } else {
                    st.onboarding.step = OnboardingStep::Gatekeeper;
                    st.onboarding.selected_index = 0;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                st.onboarding.selected_index = st.onboarding.selected_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if st.onboarding.selected_index + 1 < total_options {
                    st.onboarding.selected_index += 1;
                }
            }
            KeyCode::Enter => {
                st.onboarding.clear_messages();
                let sel = st.onboarding.selected_index;
                match st.onboarding.step {
                    OnboardingStep::Git => match sel {
                        0 => {
                            drop(st);
                            let params = tauqe_protocol::RepositoryInitParams {
                                initial_commit: false,
                            };
                            send_request(
                                server_writer,
                                methods::REPOSITORY_INIT,
                                serde_json::to_value(params)?,
                            )
                            .await?;
                        }
                        1 => {
                            drop(st);
                            let params = tauqe_protocol::RepositoryInitParams {
                                initial_commit: true,
                            };
                            send_request(
                                server_writer,
                                methods::REPOSITORY_INIT,
                                serde_json::to_value(params)?,
                            )
                            .await?;
                        }
                        _ => {
                            return Ok(InputResult::Exit);
                        }
                    },
                    OnboardingStep::Config => match sel {
                        0 => {
                            drop(st);
                            send_request(
                                server_writer,
                                methods::CONFIG_CREATE,
                                serde_json::json!({}),
                            )
                            .await?;
                        }
                        _ => {
                            st.onboarding.config_skipped = true;
                            st.onboarding.step = st.onboarding.next_step();
                            st.onboarding.selected_index = 0;
                        }
                    },
                    OnboardingStep::Credentials => match sel {
                        0 => {
                            st.onboarding.input_active = true;
                            st.onboarding.input_buffer.clear();
                            st.onboarding.show_key = false;
                            // The paste buffer is a native round trip bounded at
                            // NATIVE_CLIPBOARD_TIMEOUT, so it is read with the guard
                            // released and the answer is applied afterwards (c-1).
                            drop(st);
                            let key_in_clipboard = crate::clipboard::get_from_clipboard_async()
                                .await
                                .is_some_and(|text| text.trim().starts_with("sk-or-"));
                            let mut st = state.lock().await;
                            if key_in_clipboard {
                                let primary_chord = format!(
                                    "{}V",
                                    st.tui_config.input.primary_modifier.prefix_str()
                                );
                                let meta_chord =
                                    format!("{}V", st.tui_config.input.meta_modifier.prefix_str());
                                st.onboarding.set_status(fill(
                                    &active_t("notify.onboarding_key_detected"),
                                    &[&primary_chord, &meta_chord],
                                ));
                            }
                        }
                        1 => {
                            drop(st);
                            send_request(
                                server_writer,
                                methods::CREDENTIALS_CREATE_STUB,
                                serde_json::json!({}),
                            )
                            .await?;
                        }
                        2 => {
                            drop(st);
                            send_request(
                                server_writer,
                                methods::CONFIG_RELOAD,
                                serde_json::json!({}),
                            )
                            .await?;
                        }
                        _ => {
                            if st.onboarding.has_api_key {
                                st.onboarding.step = st.onboarding.next_step();
                            } else {
                                st.onboarding.step = OnboardingStep::Gatekeeper;
                            }
                            st.onboarding.selected_index = 0;
                        }
                    },
                    OnboardingStep::Workstation => match sel {
                        0 => {
                            st.onboarding.chosen_layout = crate::config::LayoutPreset::None;
                            st.onboarding.chosen_langmap = None;
                            st.onboarding.step = OnboardingStep::Modifier;
                            st.onboarding.selected_index = 0;
                        }
                        1 => {
                            st.onboarding.chosen_layout = crate::config::LayoutPreset::RuJcuken;
                            st.onboarding.chosen_langmap = None;
                            st.onboarding.step = OnboardingStep::Modifier;
                            st.onboarding.selected_index = 0;
                        }
                        2 => {
                            st.onboarding.input_active = true;
                            st.onboarding.input_langmap = true;
                            st.onboarding.input_buffer.clear();
                        }
                        _ => {
                            st.onboarding.workstation_skipped = true;
                            st.onboarding.step = st.onboarding.next_step();
                            st.onboarding.selected_index = 0;
                        }
                    },
                    OnboardingStep::Modifier => {
                        let modifier = if sel == 0 {
                            crate::config::PrimaryModifier::Ctrl
                        } else {
                            crate::config::PrimaryModifier::Alt
                        };
                        // The persistence job owns the guard until it settles,
                        // so this arm returns instead of touching the state
                        // again (c-1).
                        save_workstation_config(state, st, modifier).await;
                        return Ok(InputResult::Continue);
                    }
                    OnboardingStep::Gatekeeper => match sel {
                        0 => {
                            st.onboarding.step = OnboardingStep::Credentials;
                            st.onboarding.selected_index = 0;
                        }
                        1 => {
                            drop(st);
                            send_request(
                                server_writer,
                                methods::CONFIG_RELOAD,
                                serde_json::json!({}),
                            )
                            .await?;
                        }
                        _ => {
                            return Ok(InputResult::Exit);
                        }
                    },
                    OnboardingStep::Ready => {
                        st.view_mode = ViewMode::Repl;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(InputResult::Continue)
}
