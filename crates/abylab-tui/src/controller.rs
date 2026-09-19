//! Controller thread: executes UI commands. Live sessions delegate to the
//! abycore driver in `abylab-backend`; that is the only transport.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc};

use crate::bus::{AppEvent, CatalogPreset, Cmd, CtlEvent};
use crate::runtime::RuntimeConfig;

pub struct Controller {
    cmd_tx: Sender<Cmd>,
    aby: Option<Arc<abylab_backend::DriverHandle>>,
}

impl Controller {
    /// Live abycore: the agent SDK drives turns in-process. This is the
    /// primary transport.
    pub fn start_aby(
        cfg: RuntimeConfig,
        session_id: String,
        bus: Sender<AppEvent>,
        limits: abylab_backend::TurnLimits,
        compaction: Option<abylab_backend::CompactionConfig>,
    ) -> Controller {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        // Persisted UI settings supply the launch defaults with no CLI flag
        // (reasoning effort, permission preset); flags win where they exist.
        let settings = crate::locale::UiSettings::load(&cfg.home);
        let driver_cfg = abylab_backend::DriverConfig {
            session_id: session_id.clone(),
            // The app's session id is the single source of truth: a shell
            // `--session-id` resumes its persisted snapshot when one exists
            // in the shared session store.
            resume: abylab_backend::persisted_session_id(
                &cfg.sessions_root,
                &cfg.workspace,
                &session_id,
            ),
            sessions_root: Some(cfg.sessions_root.clone()),
            home: Some(cfg.home.clone()),
            workspace: cfg.workspace.clone(),
            model: if cfg.model.is_empty() {
                "deepseek-flash".into()
            } else {
                cfg.model.clone()
            },
            reasoning: settings.effort.clone().unwrap_or_else(|| "high".into()),
            permission: settings.permission.clone(),
            max_tokens: cfg.max_tokens,
            api_key: cfg.api_key.clone(),
            base_url: cfg.base_url.clone(),
            limits,
            compaction,
        };
        let sink_bus = bus.clone();
        let spawn_result =
            abylab_backend::driver::spawn(driver_cfg, move |event: abylab_backend::Event| {
                for app_event in translate_backend(event) {
                    let _ = sink_bus.send(app_event);
                }
            });
        match spawn_result {
            Ok(handle) => {
                let handle = Arc::new(handle);
                let loop_handle = Arc::clone(&handle);
                std::thread::Builder::new()
                    .name("dsh-controller".into())
                    .spawn(move || aby_loop(bus, cmd_rx, loop_handle))
                    .expect("spawn aby controller");
                Controller {
                    cmd_tx,
                    aby: Some(handle),
                }
            }
            Err(err) => {
                let _ = bus.send(AppEvent::Ctl(CtlEvent::Error(err)));
                Controller { cmd_tx, aby: None }
            }
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Interrupt from the UI thread; the driver delivers the cancel to the
    /// active turn.
    pub fn interrupt_now(&self) -> bool {
        if let Some(aby) = &self.aby {
            aby.interrupt();
            return true;
        }
        false
    }
}

/// The abycore command loop: tui `Cmd`s → driver commands, plus catalog /
/// efforts stubs the composer chrome expects.
fn aby_loop(
    bus: Sender<AppEvent>,
    cmd_rx: Receiver<Cmd>,
    handle: Arc<abylab_backend::DriverHandle>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Prompt { text, .. } => handle.send(abylab_backend::Cmd::Prompt { text }),
            Cmd::Steer {
                message_id, text, ..
            } => {
                // Send Now: cancel the active turn; the prompt queues behind
                // it inside the driver.
                handle.interrupt();
                handle.send(abylab_backend::Cmd::Prompt { text });
                let _ = bus.send(AppEvent::Ctl(CtlEvent::SteerSettled {
                    message_id,
                    deferred: false,
                }));
            }
            Cmd::PromptImages { blocks, .. } | Cmd::SteerImages { blocks, .. } => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        crate::bus::PromptBlock::Text(text) => Some(text.as_str()),
                        crate::bus::PromptBlock::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                handle.interrupt();
                handle.send(abylab_backend::Cmd::Prompt { text });
            }
            Cmd::Interrupt { .. } => {
                // The cancel itself happened in interrupt_now(); the driver
                // reports the settled turn on its own.
            }
            Cmd::SelectModel { model, effort, .. } => {
                handle.send(abylab_backend::Cmd::SetModel { model, effort });
            }
            Cmd::FetchCatalog => {
                // Seed the stock composition presets immediately, then ask
                // the driver for the provider's live model listing. A failed
                // or empty fetch keeps the picker on its stock presets
                // (`MODEL_PRESETS` + the configured model).
                let _ = bus.send(AppEvent::Ctl(CtlEvent::Catalog {
                    models: Vec::new(),
                    presets: stock_presets(),
                }));
                handle.send(abylab_backend::Cmd::FetchCatalog);
            }
            Cmd::FetchSkills => {
                let _ = bus.send(AppEvent::Ctl(CtlEvent::Skills { skills: Vec::new() }));
            }
            Cmd::FetchEfforts { .. } => {
                let _ = bus.send(AppEvent::Ctl(CtlEvent::Efforts {
                    efforts: vec!["off".into(), "low".into(), "high".into(), "max".into()],
                    default: Some("high".into()),
                }));
            }
            Cmd::NewSession => {
                let id = format!("aby-{}", crate::app::timestamp());
                handle.send(abylab_backend::Cmd::NewSession { session_id: id });
            }
            Cmd::ListSessions { prefix } => {
                handle.send(abylab_backend::Cmd::ListSessions { prefix });
            }
            Cmd::LoadSession { session_id } => {
                handle.send(abylab_backend::Cmd::Resume { session_id });
            }
            Cmd::SetPermission { preset, .. } => {
                handle.send(abylab_backend::Cmd::SetPermission { preset });
            }
            Cmd::SetApiKey { key } => {
                handle.send(abylab_backend::Cmd::SetApiKey { key });
            }
            Cmd::SetPreset { .. } | Cmd::SetConfigOption { .. } => {
                let _ = bus.send(AppEvent::Ctl(CtlEvent::TuiOpFailed(
                    "not supported by the abycore driver yet".into(),
                )));
            }
            Cmd::Compact => handle.send(abylab_backend::Cmd::Compact),
            Cmd::Goal { arg } => handle.send(abylab_backend::Cmd::Goal { arg }),
            Cmd::Shutdown => {
                handle.shutdown();
                break;
            }
        }
    }
}

/// Backend events → TUI `AppEvent`s (the seam between the two contracts).
fn translate_backend(event: abylab_backend::Event) -> Vec<AppEvent> {
    match event {
        abylab_backend::Event::Ui(ui) => vec![AppEvent::Ui(translate_ui(ui))],
        abylab_backend::Event::Ctl(ctl) => {
            if let abylab_backend::CtlEvent::Ready { server } = ctl {
                return vec![
                    AppEvent::Ctl(CtlEvent::Ready { server }),
                    // abycore owns the workspace snapshot store: session/list,
                    // session/load and resume are always available on this
                    // transport, so `/resume` must take the driver-backed path.
                    AppEvent::Ctl(CtlEvent::AgentCaps { load_session: true }),
                ];
            }
            vec![AppEvent::Ctl(match ctl {
                abylab_backend::CtlEvent::Starting { runtime } => CtlEvent::Starting { runtime },
                abylab_backend::CtlEvent::PromptQueued { message_id } => {
                    CtlEvent::PromptQueued { message_id }
                }
                abylab_backend::CtlEvent::Error(err) => CtlEvent::Error(err),
                abylab_backend::CtlEvent::CancelRequested => CtlEvent::CancelRequested,
                abylab_backend::CtlEvent::Interrupted => CtlEvent::Interrupted,
                abylab_backend::CtlEvent::TuiOpDone(message) => CtlEvent::TuiOpDone(message),
                abylab_backend::CtlEvent::TuiOpFailed(message) => CtlEvent::TuiOpFailed(message),
                abylab_backend::CtlEvent::SessionBound {
                    session_id,
                    notice,
                    model,
                    effort,
                } => CtlEvent::SessionBound {
                    session_id,
                    notice,
                    model,
                    effort,
                },
                abylab_backend::CtlEvent::Efforts { efforts, default } => {
                    CtlEvent::Efforts { efforts, default }
                }
                abylab_backend::CtlEvent::Catalog { models } => CtlEvent::Catalog {
                    models: models
                        .into_iter()
                        .map(|model| crate::bus::CatalogModel {
                            provider: model.provider,
                            id: model.id,
                            name: model.name,
                            vision: model.vision,
                        })
                        .collect(),
                    // The live listing replaces models only; stock composition
                    // presets were seeded when the picker opened.
                    presets: Vec::new(),
                },
                abylab_backend::CtlEvent::SessionList { sessions, prefix } => {
                    CtlEvent::SessionList {
                        sessions: sessions
                            .into_iter()
                            .map(|row| crate::bus::SessionListItem {
                                id: row.id,
                                title: row.title,
                                updated_at: row.updated_at,
                            })
                            .collect(),
                        prefix,
                    }
                }
                // Handled above (emits Ready + AgentCaps together).
                abylab_backend::CtlEvent::Ready { .. } => unreachable!(),
            })]
        }
        abylab_backend::Event::PermissionAsk {
            title,
            options,
            reply,
        } => vec![AppEvent::PermissionAsk {
            title,
            options,
            reply,
        }],
    }
}

fn translate_ui(ui: abylab_backend::UiEvent) -> crate::events::UiEvent {
    use crate::events::UiEvent;
    match ui {
        abylab_backend::UiEvent::SessionStatus { session, running } => {
            UiEvent::SessionStatus { session, running }
        }
        abylab_backend::UiEvent::TurnStart { session, turn } => {
            UiEvent::TurnStart { session, turn }
        }
        abylab_backend::UiEvent::TurnEnd { session, kind } => UiEvent::TurnEnd { session, kind },
        abylab_backend::UiEvent::TextDelta { session, text } => {
            UiEvent::TextDelta { session, text }
        }
        abylab_backend::UiEvent::ReasoningDelta { session, text } => {
            UiEvent::ReasoningDelta { session, text }
        }
        abylab_backend::UiEvent::AssistantFinal {
            session,
            text,
            model,
        } => UiEvent::AssistantFinal {
            session,
            text,
            model,
        },
        abylab_backend::UiEvent::ToolCall {
            session,
            call_id,
            name,
            arguments,
        } => UiEvent::ToolCall {
            session,
            call_id,
            name,
            arguments,
        },
        abylab_backend::UiEvent::ToolCallDelta {
            session,
            call_id,
            name,
            delta,
        } => UiEvent::ToolCallDelta {
            session,
            call_id,
            name,
            delta,
        },
        abylab_backend::UiEvent::ToolStarted {
            session,
            call_id,
            name,
        } => UiEvent::ToolStarted {
            session,
            call_id,
            name,
        },
        abylab_backend::UiEvent::ToolResult {
            session,
            call_id,
            is_error,
            text,
            error,
        } => UiEvent::ToolResult {
            session,
            call_id,
            is_error,
            text,
            error,
        },
        abylab_backend::UiEvent::Plan { session, summary } => UiEvent::Plan { session, summary },
        abylab_backend::UiEvent::SubagentStarted {
            parent,
            child,
            label,
        } => UiEvent::SubagentStarted {
            parent,
            child,
            label,
        },
        abylab_backend::UiEvent::SubagentFinished { child } => UiEvent::SubagentFinished { child },
        abylab_backend::UiEvent::Usage {
            session,
            input,
            output,
            cached,
            reasoning,
        } => UiEvent::Usage {
            session,
            input,
            output,
            cached,
            reasoning,
        },
        abylab_backend::UiEvent::UserMessage { session, text } => {
            UiEvent::UserMessage { session, text }
        }
        abylab_backend::UiEvent::SandboxMode { session, mode } => {
            UiEvent::SandboxMode { session, mode }
        }
        abylab_backend::UiEvent::PermissionPreset { session, preset } => {
            UiEvent::PermissionPreset { session, preset }
        }
        abylab_backend::UiEvent::ApprovalPolicy { session, policy } => {
            UiEvent::ApprovalPolicy { session, policy }
        }
    }
}

#[cfg(test)]
pub(crate) fn test_controller() -> (Controller, Receiver<Cmd>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    (Controller { cmd_tx, aby: None }, cmd_rx)
}

#[cfg(test)]
pub(crate) fn test_interruptible_controller() -> (Controller, Receiver<Cmd>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    (Controller { cmd_tx, aby: None }, cmd_rx)
}

fn stock_presets() -> Vec<CatalogPreset> {
    crate::app::AGENT_MODES
        .iter()
        .map(|(id, name, desc)| CatalogPreset {
            id: id.to_string(),
            name: name.to_string(),
            description: desc.to_string(),
            broken: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_presets_cover_the_four_web_ui_modes() {
        let presets = stock_presets();
        let ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["standard", "code", "minimal", "cordis"]);
    }

    #[test]
    fn backend_permission_facts_reach_the_tui() {
        let events = translate_backend(abylab_backend::Event::Ui(
            abylab_backend::UiEvent::PermissionPreset {
                session: "s1".into(),
                preset: "read-only".into(),
            },
        ));
        assert!(matches!(
            &events[0],
            AppEvent::Ui(crate::events::UiEvent::PermissionPreset { session, preset })
                if session == "s1" && preset == "read-only"
        ));

        let events = translate_backend(abylab_backend::Event::Ui(
            abylab_backend::UiEvent::SandboxMode {
                session: "s1".into(),
                mode: "read-only".into(),
            },
        ));
        assert!(matches!(
            &events[0],
            AppEvent::Ui(crate::events::UiEvent::SandboxMode { session, mode })
                if session == "s1" && mode == "read-only"
        ));
    }
}
