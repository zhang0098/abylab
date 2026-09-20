//! Unified event bus for the single-threaded UI loop.

/// Everything the app loop can receive.
pub enum AppEvent {
    /// Process termination signal; settle through the normal terminal guard.
    Terminate,
    /// Terminal input.
    Term(crossterm::event::Event),
    /// A protocol fact already decoded into the TUI's semantic event model.
    Ui(crate::events::UiEvent),
    /// One line of runtime stderr (kept for diagnostics).
    #[allow(dead_code)]
    RuntimeStderr(String),
    /// Runtime subprocess exited.
    #[allow(dead_code)]
    RuntimeExited(Option<i32>),
    /// Controller lifecycle updates.
    Ctl(CtlEvent),
    /// ACP `session/request_permission`: UI picks an option, then replies
    /// on the oneshot. The ACP task waits; the UI thread does not.
    PermissionAsk {
        title: String,
        options: Vec<PermissionAskOption>,
        reply: tokio::sync::oneshot::Sender<PermissionAskReply>,
    },
}

/// One option from `session/request_permission` — re-exported from the
/// abycore driver contract so the driver's asks flow through unchanged.
pub use abylab_backend::{AskOption as PermissionAskOption, PermissionReply as PermissionAskReply};

/// Empty option lists cannot be selected — cancel instead of inventing AllowOnce.
#[cfg(test)]
pub(crate) fn permission_ask_empty_outcome(
    options: &[PermissionAskOption],
) -> Option<PermissionAskReply> {
    options.is_empty().then_some(PermissionAskReply::Cancelled)
}

/// Highlight AllowOnce when the agent offered it; otherwise the first row.
pub fn permission_ask_default_sel(options: &[PermissionAskOption]) -> usize {
    options
        .iter()
        .position(|o| o.kind == "allow_once")
        .unwrap_or(0)
}

/// Controller → UI status updates.
#[derive(Debug, Clone)]
#[allow(dead_code)] // message_id: protocol fidelity; surfaced in debug logs only
pub enum CtlEvent {
    /// Spawning + initializing the runtime.
    Starting { runtime: String },
    /// initialize returned.
    Ready { server: String },
    /// session/prompt accepted into the durable inbox.
    PromptQueued { message_id: String },
    /// A Send Now request settled. Rejected concurrent prompts degrade to
    /// the client FIFO without changing the active turn lifecycle.
    SteerSettled { message_id: u64, deferred: bool },
    /// A command failed.
    Error(String),
    /// Backchat `session.cancel_requested`: user stop accepted; `session/cancel`
    /// is on the wire. The in-flight `session/prompt` has not unwound yet.
    CancelRequested,
    /// Backchat `session.cancelled`: the prompt future settled after abort.
    Interrupted,
    /// Host model catalog + advertised composition select.
    Catalog { models: Vec<CatalogModel> },
    /// Host skill catalog arrived (`available_commands_update`).
    Skills { skills: Vec<SkillInfo> },
    /// Selectable reasoning efforts for the current model.
    Efforts {
        efforts: Vec<String>,
        default: Option<String>,
    },
    /// A client-side control call succeeded.
    TuiOpDone(String),
    /// A client-side control call failed (or is unsupported on this transport).
    TuiOpFailed(String),
    /// Agent advertised `loadSession` (`session/load`, usually with `session/list`).
    AgentCaps { load_session: bool },
    /// `session/new` or `session/load` resolved; the UI must use this id.
    SessionBound {
        session_id: String,
        notice: Option<String>,
        /// The bound agent's live model id: a resumed session keeps its
        /// stored model, and a rejected `/model` never moves this row.
        model: Option<String>,
        effort: Option<String>,
    },
    /// `session/list` rows (`prefix` is the `/resume` argument, if any).
    SessionList {
        sessions: Vec<SessionListItem>,
        prefix: Option<String>,
    },
}

/// One row from ACP `session/list`.
#[derive(Debug, Clone)]
pub struct SessionListItem {
    pub id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub vision: bool,
}

/// One advertised composition choice (`agent` / `preset` / `agent-preset`,
/// One user-invocable command from `available_commands_update`: typing
/// `/name …` as a prompt makes the agent inject the skill body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}
/// One staged image on its way to the host (base64 payload).
#[derive(Debug, Clone)]
#[allow(dead_code)] // the in-process driver prompt currently carries text only
pub struct ImagePart {
    pub data: String,
    pub media_type: String,
    pub name: String,
    /// Local path when the raster came from a file; `"clipboard"` otherwise.
    pub path: String,
}

/// One content block of a composer prompt, in draft (chip) order.
#[derive(Debug, Clone)]
#[allow(dead_code)] // the image arm lands once the driver carries image input
pub enum PromptBlock {
    Text(String),
    Image(ImagePart),
}

/// UI → controller commands. The in-process driver serializes turns; the
/// addressing fields stay on the wire for multi-session diagnostics.
#[derive(Debug, Clone)]
#[allow(dead_code)] // session/message addressing fields; single-session driver
pub enum Cmd {
    Prompt {
        session_id: String,
        text: String,
    },
    /// Send another ACP `session/prompt` immediately while a turn is active.
    Steer {
        session_id: String,
        message_id: u64,
        text: String,
    },
    /// Send a prompt whose text and images stay in draft order (图文交替).
    PromptImages {
        session_id: String,
        blocks: Vec<PromptBlock>,
    },
    /// Image-capable form of [`Cmd::Steer`].
    SteerImages {
        session_id: String,
        message_id: u64,
        blocks: Vec<PromptBlock>,
    },
    /// Interrupt the active turn over ACP.
    Interrupt {
        session_id: String,
    },
    /// Select a model/config option for the ACP session.
    SelectModel {
        session_id: String,
        provider: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
    FetchCatalog,
    /// Fetch user-invocable host skills for the slash menu.
    FetchSkills,
    FetchEfforts {
        provider: String,
        model: String,
    },
    SetPermission {
        session_id: String,
        preset: String,
    },
    /// Rotate the API key for the running agent (`/login`): the driver
    /// rebuilds from its snapshot, so the next request carries the new key.
    /// `None` clears the live key (`/logout`).
    SetApiKey {
        key: Option<String>,
    },
    /// Live ACP `/new` → `session/new` (cwd = workspace).
    NewSession,
    /// Live ACP `/resume` listing (`session/list`). `prefix` is the typed id.
    ListSessions {
        prefix: Option<String>,
    },
    /// Live ACP `/resume` pick → `session/load`.
    LoadSession {
        session_id: String,
    },
    /// `/compact`: condense older history on demand.
    Compact,
    /// `/goal ...`: the session's goal and the round driver's arm switch.
    Goal {
        arg: String,
    },
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_permission_options_cancel() {
        assert_eq!(
            permission_ask_empty_outcome(&[]),
            Some(PermissionAskReply::Cancelled)
        );
        assert_eq!(
            permission_ask_empty_outcome(&[PermissionAskOption {
                option_id: "allow".into(),
                kind: "allow_once".into(),
                name: "Allow once".into(),
            }]),
            None
        );
    }

    #[test]
    fn permission_ask_defaults_to_allow_once() {
        let options = [
            PermissionAskOption {
                option_id: "reject".into(),
                kind: "reject_once".into(),
                name: "Reject".into(),
            },
            PermissionAskOption {
                option_id: "allow".into(),
                kind: "allow_once".into(),
                name: "Allow once".into(),
            },
        ];
        assert_eq!(permission_ask_default_sel(&options), 1);
        assert_eq!(permission_ask_default_sel(&options[..1]), 0);
    }
}
