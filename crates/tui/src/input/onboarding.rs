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
                st.onboarding.input_buffer.clear();
                st.onboarding.show_key = false;
            }
            KeyCode::Char(c) => {
                st.onboarding.input_buffer.push(c);
            }
            KeyCode::Backspace => {
                st.onboarding.input_buffer.pop();
            }
            KeyCode::Enter => {
                let input = st.onboarding.input_buffer.trim().to_string();
                st.onboarding.input_active = false;
                st.onboarding.input_buffer.clear();

                match st.onboarding.step {
                    OnboardingStep::Config if !input.is_empty() => {
                        st.onboarding.selected_model = input.clone();
                        drop(st);
                        send_request(
                            server_writer,
                            methods::CONFIG_CREATE,
                            serde_json::json!({ "model": input }),
                        )
                        .await?;
                    }
                    OnboardingStep::Credentials if !input.is_empty() => {
                        drop(st);
                        send_request(
                            server_writer,
                            methods::CREDENTIALS_SAVE,
                            serde_json::json!({ "api_key": input }),
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    } else {
        let total_options = match st.onboarding.step {
            OnboardingStep::Git => 3,
            OnboardingStep::Config => 6,
            OnboardingStep::Credentials => 4,
            OnboardingStep::Gatekeeper => 3,
            OnboardingStep::Ready => 1,
        };

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if st.onboarding.step == OnboardingStep::Gatekeeper
                    || st.onboarding.step == OnboardingStep::Git
                {
                    return Ok(InputResult::Exit);
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
                        0..=3 => {
                            let model = st
                                .onboarding
                                .models_list
                                .get(sel)
                                .cloned()
                                .unwrap_or_else(|| "anthropic/claude-3.7-sonnet".to_string());
                            st.onboarding.selected_model = model.clone();
                            drop(st);
                            send_request(
                                server_writer,
                                methods::CONFIG_CREATE,
                                serde_json::json!({ "model": model }),
                            )
                            .await?;
                        }
                        4 => {
                            st.onboarding.input_active = true;
                            st.onboarding.input_buffer.clear();
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
                                st.onboarding.step = OnboardingStep::Ready;
                            } else {
                                st.onboarding.step = OnboardingStep::Gatekeeper;
                            }
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
