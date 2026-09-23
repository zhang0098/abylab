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
    /// A model-requested task question, answered without leaving the turn.
    UserQuestion {
        question: abylab_backend::UserQuestion,
        reply: tokio::sync::oneshot::Sender<Option<abylab_backend::UserQuestionReply>>,
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

/// One change to a queued row, as the queue list's gestures express it.
#[derive(Debug, Clone)]
pub enum QueueAction {
    /// `ctrl+d`: the row leaves the queue (and its echo leaves the timeline).
    Remove,
    /// The `⌥↑` editor saved new text for the row.
    Edit(String),
}

/// One row of the session's host-owned queue, as the driver publishes it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRow {
    pub item_id: u64,
    pub text: String,
    /// `true` while the running turn has the message and has not appended it
    /// yet (the composer paints those as pending steering).
    pub steering: bool,
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
    /// The session's queue, after every change and once when a session binds.
    /// The rows are the driver's; the client renders them.
    Queue { items: Vec<QueueRow> },
    /// One row left the queue because the driver delivered it.
    QueueClaimed { item_id: u64 },
    /// One row left the queue without being delivered.
    QueueRemoved { item_id: u64 },
    /// The agent appended steered messages at a step boundary: their pending
    /// rows are ordinary parts of the conversation from here on.
    SteerAdmitted { message_ids: Vec<u64> },
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
    /// A switch failed; retain the currently displayed session and its queue.
    SessionSwitchFailed(String),
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

/// One user-invocable skill: `/skill <name>` runs it, and a hand-typed
/// `/name …` line ships as a prompt the agent expands into the skill's body.
/// Skills are never `/` menu rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
    /// The skill file this came from — `/skill`'s listing shows which one won.
    pub source: Option<String>,
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
    /// Queue one message in the session's host-owned FIFO. The driver owns
    /// delivery order: the item ships when the turn in flight ends, or right
    /// away when none is running. `item_id` names the optimistic echo the
    /// driver settles with `CtlEvent::QueueClaimed` / `QueueRemoved`.
    Queue {
        session_id: String,
        item_id: u64,
        text: String,
    },
    /// Change one queued row (remove it, or save edited text).
    UpdateQueue {
        session_id: String,
        item_id: u64,
        action: QueueAction,
    },
    /// Send Now: hand this text to the turn that is running, at its next step
    /// boundary, without cancelling it. With no running turn it is admitted as
    /// the next one — never a failure. `message_id` names the optimistic echo
    /// the driver settles with `CtlEvent::SteerSettled`.
    Steer {
        session_id: String,
        message_id: u64,
        text: String,
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
    /// Fetch user-invocable host skills for `/skill`'s catalog and candidates.
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
