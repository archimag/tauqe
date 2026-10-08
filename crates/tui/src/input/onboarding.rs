use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::methods;

use crate::app::{AppState, OnboardingStep, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

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
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        if let Ok(clip_text) = cb.get_text() {
                            st.onboarding.input_buffer.push_str(clip_text.trim());
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
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Ok(clip_text) = cb.get_text() {
                                    let trimmed = clip_text.trim();
                                    if trimmed.starts_with("sk-or-") {
                                        st.onboarding.input_buffer = trimmed.to_string();
                                    }
                                }
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
                    OnboardingStep::Modifier => match sel {
                        0 => {
                            let _ = crate::config::save_tui_config_full(
                                st.onboarding.chosen_layout,
                                st.onboarding.chosen_langmap.clone(),
                                crate::config::PrimaryModifier::Ctrl,
                            );
                            st.tui_config = crate::config::TuiConfig::load();
                            st.onboarding.has_tui_config = true;
                            st.onboarding.set_status(
                                "Workstation configuration saved (Primary modifier: Ctrl)",
                            );
                            st.onboarding.step = OnboardingStep::Ready;
                            st.onboarding.selected_index = 0;
                        }
                        _ => {
                            let _ = crate::config::save_tui_config_full(
                                st.onboarding.chosen_layout,
                                st.onboarding.chosen_langmap.clone(),
                                crate::config::PrimaryModifier::Alt,
                            );
                            st.tui_config = crate::config::TuiConfig::load();
                            st.onboarding.has_tui_config = true;
                            st.onboarding.set_status(
                                "Workstation configuration saved (Primary modifier: Alt)",
                            );
                            st.onboarding.step = OnboardingStep::Ready;
                            st.onboarding.selected_index = 0;
                        }
                    },
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
