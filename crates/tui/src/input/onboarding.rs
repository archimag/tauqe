use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::methods;

use crate::app::{AppState, OnboardingStep, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

fn read_clipboard() -> Result<String, String> {
    arboard::Clipboard::new()
        .map_err(|e| e.to_string())?
        .get_text()
        .map_err(|e| e.to_string())
}

/// Persists the chosen workstation settings and reloads them, advancing to the
/// Ready step only when both the write and the re-read succeed.
fn save_workstation_config(st: &mut AppState, modifier: crate::config::PrimaryModifier) {
    let label = match modifier {
        crate::config::PrimaryModifier::Ctrl => "Ctrl",
        crate::config::PrimaryModifier::Alt => "Alt",
    };
    let saved = crate::config::save_tui_config_full(
        st.onboarding.chosen_layout,
        st.onboarding.chosen_langmap.clone(),
        modifier,
    );
    match saved {
        Ok(path) => match crate::config::TuiConfig::load_checked() {
            Ok(cfg) => {
                st.tui_config = cfg;
                st.onboarding.has_tui_config = true;
                st.onboarding.set_status(format!(
                    "Workstation configuration saved to {} (Primary modifier: {})",
                    path.display(),
                    label
                ));
                st.onboarding.step = OnboardingStep::Ready;
                st.onboarding.selected_index = 0;
            }
            Err(err) => {
                st.onboarding.set_error(format!(
                    "Saved {}, but it cannot be loaded: {}",
                    path.display(),
                    err
                ));
            }
        },
        Err(err) => {
            let target = st.onboarding.default_tui_config_path.clone();
            st.onboarding
                .set_error(format!("Failed to save {}: {}", target, err));
        }
    }
}

pub async fn handle_onboarding_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    if st.onboarding.input_active {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => {
                    st.onboarding.show_key = !st.onboarding.show_key;
                    return Ok(InputResult::Continue);
                }
                KeyCode::Char('v') | KeyCode::Char('y') => {
                    match read_clipboard() {
                        Ok(clip_text) => {
                            st.onboarding.input_buffer.push_str(clip_text.trim());
                        }
                        Err(err) => {
                            st.onboarding.set_error(format!(
                                "Clipboard unavailable ({}). Use your terminal's paste instead.",
                                err
                            ));
                        }
                    }
                    return Ok(InputResult::Continue);
                }
                KeyCode::Char('w') => {
                    crate::input::pop_word_backward(&mut st.onboarding.input_buffer);
                    return Ok(InputResult::Continue);
                }
                KeyCode::Char('u') => {
                    st.onboarding.input_buffer.clear();
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
            KeyCode::Char(c) if crate::input::is_char_typing(key.modifiers) => {
                st.onboarding.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                st.onboarding.input_buffer.pop();
            }
            KeyCode::Enter => {
                let input = st.onboarding.input_buffer.trim().to_string();

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
                            st.onboarding.set_error(
                                "API key cannot be empty. Please enter or paste a valid key, or press Esc to cancel.",
                            );
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
            _ => {}
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
                if st.onboarding.step == OnboardingStep::Gatekeeper
                    || st.onboarding.step == OnboardingStep::Git
                {
                    return Ok(InputResult::Exit);
                } else if st.onboarding.step == OnboardingStep::Modifier {
                    st.onboarding.step = OnboardingStep::Workstation;
                    st.onboarding.selected_index = 0;
                } else if st.onboarding.has_api_key {
                    st.view_mode = ViewMode::Develop;
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
                            if !st.onboarding.has_api_key {
                                st.onboarding.step = OnboardingStep::Credentials;
                            } else {
                                st.onboarding.step = OnboardingStep::Ready;
                            }
                            st.onboarding.selected_index = 0;
                        }
                    },
                    OnboardingStep::Credentials => match sel {
                        0 => {
                            st.onboarding.input_active = true;
                            st.onboarding.input_buffer.clear();
                            st.onboarding.show_key = false;
                            let key_in_clipboard = read_clipboard()
                                .map(|text| text.trim().starts_with("sk-or-"))
                                .unwrap_or(false);
                            if key_in_clipboard {
                                st.onboarding.set_status(
                                    "API key detected in clipboard. Press Ctrl+V to paste it.",
                                );
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
                                if !st.onboarding.has_tui_config {
                                    st.onboarding.step = OnboardingStep::Workstation;
                                } else {
                                    st.onboarding.step = OnboardingStep::Ready;
                                }
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
                            st.onboarding.step = OnboardingStep::Ready;
                            st.onboarding.selected_index = 0;
                        }
                    },
                    OnboardingStep::Modifier => {
                        let modifier = if sel == 0 {
                            crate::config::PrimaryModifier::Ctrl
                        } else {
                            crate::config::PrimaryModifier::Alt
                        };
                        save_workstation_config(&mut st, modifier);
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
                        st.view_mode = ViewMode::Develop;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(InputResult::Continue)
}
