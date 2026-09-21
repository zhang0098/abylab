//! Shared semantic contract between the abylab TUI and the abycore driver.
//!
//! The driver emits [`Event`]s; the TUI's controller thread translates them
//! into `AppEvent`s. The types here are deliberately tui-independent.

use tokio::sync::oneshot;

use abycore::SessionStore;

/// One semantic UI fact, mirroring the subset of the TUI's `UiEvent` the
/// transcript renderer consumes. `session` names the owning session so the
/// app can route events across tabs.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    SessionStatus {
        session: String,
        running: bool,
    },
    TurnStart {
        session: String,
        turn: u64,
    },
    TurnEnd {
        session: String,
        kind: String,
    },
    TextDelta {
        session: String,
        text: String,
    },
    ReasoningDelta {
        session: String,
        text: String,
    },
    AssistantFinal {
        session: String,
        text: String,
        model: Option<String>,
    },
    ToolCall {
        session: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Incremental tool-call arguments: the call's card can render its title
    /// while the model is still writing it.
    ToolCallDelta {
        session: String,
        call_id: String,
        name: String,
        delta: String,
    },
    /// Execution begins for a tool call whose card the streamed call created.
    /// The tool timer starts here, not at the first argument delta.
    ToolStarted {
        session: String,
        call_id: String,
        name: String,
    },
    ToolResult {
        session: String,
        call_id: String,
        is_error: bool,
        text: String,
        error: Option<String>,
    },
    /// Todo-plan snapshot: `summary` is the transcript plan cell's digest,
    /// while the structured fields let the TUI paint its own live todo line
    /// and the clickable progress dialog. An empty `summary` (no list this
    /// turn, or an explicit clear) hides every plan surface.
    Plan {
        session: String,
        summary: String,
        /// The whole checklist, in list order — the dialog shows every row.
        todos: Vec<PlanItem>,
        /// First in-progress task, in list order.
        active: Option<String>,
        /// Additional concurrently in-progress tasks.
        active_extra: usize,
        completed: usize,
        total: usize,
    },
    /// Subagent lifecycle under the parent's session id.
    SubagentStarted {
        parent: String,
        child: String,
        label: Option<String>,
    },
    SubagentFinished {
        child: String,
    },
    Usage {
        session: String,
        input: u64,
        output: u64,
        cached: u64,
        reasoning: u64,
    },
    /// Replayed user line from a resumed session.
    UserMessage {
        session: String,
        text: String,
    },
    /// Effective filesystem/process sandbox for this session.
    SandboxMode {
        session: String,
        mode: String,
    },
    /// User-facing permission preset selected for this session.
    PermissionPreset {
        session: String,
        preset: String,
    },
    /// Whether individual tool calls require host approval.
    ApprovalPolicy {
        session: String,
        policy: String,
    },
}

/// One checklist row, in the order the agent wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub status: PlanStatus,
}

/// Lifecycle of one [`PlanItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// Driver lifecycle facts, mirroring the TUI's `CtlEvent` subset.
#[derive(Debug, Clone)]
pub enum CtlEvent {
    Starting {
        runtime: String,
    },
    Ready {
        server: String,
    },
    PromptQueued {
        message_id: String,
    },
    /// One Send Now request settled. `deferred` means the driver could not take
    /// the message into a running turn (no agent, a session switch, a full
    /// inbox), so the composer keeps it queued instead of claiming delivery.
    SteerSettled {
        message_id: u64,
        deferred: bool,
    },
    /// The agent appended steered messages to the transcript at a step boundary:
    /// the composer's pending-steering rows are ordinary user rows from here on.
    /// One event per boundary, however many messages it drained.
    SteerAdmitted {
        message_ids: Vec<u64>,
    },
    Error(String),
    CancelRequested,
    Interrupted,
    TuiOpDone(String),
    TuiOpFailed(String),
    /// A requested switch failed; the previously bound session is unchanged.
    SessionSwitchFailed(String),
    SessionBound {
        session_id: String,
        notice: Option<String>,
        /// The bound agent's live model id, so `/model`, `/status` and the
        /// composer meta row track the driver instead of launch assumptions
        /// (a resumed session keeps its stored model).
        model: Option<String>,
        effort: Option<String>,
    },
    /// The provider's live model listing (`/model` picker). Empty when the
    /// fetch failed; the picker keeps its stock presets then.
    Catalog {
        models: Vec<CatalogModel>,
    },
    Efforts {
        efforts: Vec<String>,
        default: Option<String>,
    },
    /// `/resume` discovery rows (the workspace store, newest first).
    SessionList {
        sessions: Vec<SessionRow>,
        /// Echoed `/resume <prefix>` argument, if any.
        prefix: Option<String>,
    },
    /// The skills discovered at launch (the `/skill` listing and its argument
    /// candidates). Same snapshot the driver injects on `/<name>` and hands the
    /// `skill` tool, so the listing never offers a skill the driver would not run.
    Skills {
        skills: Vec<SkillRow>,
    },
}

/// One skill: a markdown instruction document the user invokes with `/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRow {
    pub name: String,
    pub description: String,
    /// Frontmatter `input-hint`: the `/name <hint>` argument placeholder.
    pub input_hint: Option<String>,
    /// Absolute path of the skill file — `/skill`'s listing shows where it came from.
    pub path: String,
}

/// One model advertised by the provider's live catalog (`GET /models`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub vision: bool,
}

/// One resumable session row for the `/resume` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    /// Title, falling back to the first user prompt preview.
    pub title: Option<String>,
    /// Epoch seconds string of the last modification.
    pub updated_at: Option<String>,
}

/// One option rendered in the TUI's permission overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOption {
    pub option_id: String,
    pub kind: String,
    pub name: String,
}

/// The UI's decision for a pending permission ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionReply {
    Selected(String),
    Cancelled,
}

/// Per-turn execution limits for the in-process agent.
///
/// deepseek-harness — the design abylab mirrors — has **no built-in turn
/// budget**: `packages/core/agent-loop` keeps running steps until the model
/// stops, the inbox empties, or the user aborts, and its README says any
/// runaway policy belongs to a lifecycle extension point of the host. Where
/// the harness does cap something (`workflow-ptc`'s `maxTotalAgents`, the goal
/// round driver's `maxGoalRounds`), the cap is a generous, explicit backstop
/// whose message tells the caller to raise it when the scale is intentional.
///
/// abycore instead always enforces `RunOptions::max_requests`/`max_tool_calls`
/// per run (SDK defaults 16/32), so abylab treats them the same way: a far
/// backstop, never a normal turn ending. [`TurnLimits::UNLIMITED`] removes the
/// backstop entirely — the closest match to harness behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnLimits {
    /// HTTP dispatches per run segment: conversation, retries and web search.
    /// [`TurnLimits::UNLIMITED`] removes the cap.
    pub max_requests: usize,
    /// Tool executions per run segment. [`TurnLimits::UNLIMITED`] removes the cap.
    pub max_tool_calls: usize,
    /// Extra run segments granted to an unfinished turn before the failure
    /// surfaces: budget stops, timeouts and transient request failures resume the
    /// same open turn through the SDK's `continue_run` (harness's
    /// `dsh-llm-retry` re-runs a failed step in the same open turn). `0`
    /// disables it, so every failure ends the turn.
    pub continuations: usize,
    /// Whole-segment deadline. Harness has no turn deadline at all; this is
    /// abylab's own bound. Expiry can grant a fresh segment while continuation
    /// headroom remains; it does not immediately end a long, unfinished turn.
    pub run_timeout: std::time::Duration,
    /// Per-tool deadline (a tool that ignores cancellation can exceed it).
    pub tool_timeout: std::time::Duration,
}

impl TurnLimits {
    /// No cap: the turn runs until the model stops, the user interrupts, the
    /// timeout continuations are exhausted, or the context limit is reached.
    pub const UNLIMITED: usize = usize::MAX;

    /// The harness-style default: wide enough that a legitimate turn never
    /// reaches it (abycore has no advisory loop guard yet), plus continuation
    /// headroom so tripping it still does not drop the turn.
    pub const fn watchdog() -> Self {
        Self {
            max_requests: 1000,
            max_tool_calls: 1000,
            continuations: 3,
            // abycore's SDK defaults: ten minutes per segment, one per tool.
            run_timeout: std::time::Duration::from_secs(600),
            tool_timeout: std::time::Duration::from_secs(60),
        }
    }
}

impl Default for TurnLimits {
    fn default() -> Self {
        Self::watchdog()
    }
}

/// Host context policy for long sessions.
///
/// deepseek-harness condenses at 80% of the routed model's window and keeps the
/// newest 16% verbatim (`dsh-compaction-basic` defaults); abycore ships the
/// mechanics (`Agent::context_estimate`, `summarize_span`, `compact`) and
/// abylab owns the numbers and the trigger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Context window of the routed model, in tokens. The host must know it —
    /// abycore has no model table.
    pub context_window: u64,
    /// Start compacting once the conservative estimate reaches this fraction of
    /// the window.
    pub compact_at: f64,
    /// Recent history kept verbatim, as a fraction of the window.
    pub keep_recent: f64,
    /// Output cap for the summarization call.
    pub max_tokens: u32,
    /// Before summarizing, trim each tool result older than the retained tail
    /// to this many bytes (0 disables trimming). Harness ships the same step as
    /// `compaction-tool-result-pruner`: it can free enough context that no
    /// summary call is needed at all.
    pub prune_tool_bytes: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window: 128_000,
            compact_at: 0.8,
            keep_recent: 0.16,
            max_tokens: 8192,
            prune_tool_bytes: 4096,
        }
    }
}

impl CompactionConfig {
    /// Token ceiling that triggers compaction. `compact_at == 0.0` disables
    /// the threshold (and with it the prune/condense pass at request
    /// boundaries), matching the environment knob's documented meaning;
    /// clamping it to one token would do the opposite.
    pub fn threshold_tokens(&self) -> u64 {
        if self.compact_at <= 0.0 {
            return u64::MAX;
        }
        (self.context_window as f64 * self.compact_at).max(1.0) as u64
    }

    /// Tokens of recent history that must stay verbatim.
    pub fn retained_tokens(&self) -> u64 {
        (self.context_window as f64 * self.keep_recent).max(0.0) as u64
    }
}

/// Launch configuration for one abycore agent session.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub session_id: String,
    /// Session id to restore from the workspace's session store, if any.
    pub resume: Option<String>,
    /// Shared abycore session store root (e.g. `~/.abylab/sessions`).
    /// `None` keeps the workspace-scoped default
    /// (`<workspace>/.abycore/sessions`).
    pub sessions_root: Option<String>,
    /// aby home directory (`~/.abylab`): host-level state that is not
    /// session data, e.g. global AGENTS.md.
    /// `None` disables global instruction discovery; the TUI resolves ABYLAB_HOME.
    pub home: Option<String>,
    pub workspace: String,
    pub model: String,
    pub reasoning: String,
    /// Startup permission preset (`read-only`, `workspace-write`,
    /// `danger-full-access`). `None` keeps the trusted-directory default
    /// (`danger-full-access`).
    pub permission: Option<String>,
    /// Explicit output-token cap for fresh and resumed sessions. None keeps
    /// the saved value on resume; a fresh session uses the SDK default.
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Per-turn budgets and auto-continuation headroom.
    pub limits: TurnLimits,
    /// Context compaction policy; `None` disables automatic compaction.
    pub compaction: Option<CompactionConfig>,
}

/// `Some(session_id)` when the shared store holds a committed snapshot for
/// that session — lets the TUI decide the startup `resume` without
/// depending on abycore.
pub fn persisted_session_id(
    sessions_root: &str,
    workspace: &str,
    session_id: &str,
) -> Option<String> {
    let store = (SessionStore::at(sessions_root, workspace).ok())?;
    // A log can hold a header and a title with no checkpoint yet (a crash
    // between `set_title` and the first `append_checkpoint`): resuming it
    // fails, while `create_new` can reclaim it under the writer lock. Only a
    // committed, loadable snapshot resumes.
    store.load(session_id).ok()?;
    Some(session_id.to_string())
}

/// One Send Now request on its own channel, so a running turn can take it
/// while it is streaming. Kept out of [`Cmd`] on purpose: commands are strictly
/// serialized behind the turn, and a steer that waits for the turn to end is
/// exactly the behavior this replaces.
#[derive(Debug, Clone)]
pub struct SteerRequest {
    /// Reject the steer if a session switch changed the intended owner.
    pub session_id: String,
    /// The composer's optimistic echo, settled by [`CtlEvent::SteerSettled`].
    pub message_id: u64,
    pub text: String,
}

/// UI → driver commands. The driver serializes turns; a `Prompt` that arrives
/// while a turn is running queues behind it.
#[derive(Debug, Clone)]
pub enum Cmd {
    Prompt {
        text: String,
    },
    /// Reject a prompt if a queued session switch changed its intended owner.
    PromptForSession {
        session_id: String,
        text: String,
    },
    /// Send Now: inject a message into the turn that is already running, at its
    /// next complete step boundary, **without cancelling anything**.
    ///
    /// The composer routes the gesture through [`SteerRequest`] instead, whose
    /// channel a running turn polls; the driver only produces this variant for
    /// itself when no turn was running. Such a steer is normalized into
    /// [`Cmd::Prompt`] — admitted as the next waking turn, never a failure
    /// (the harness's best-effort steer contract).
    SteerForSession {
        session_id: String,
        /// The composer's optimistic echo id, settled by
        /// [`CtlEvent::SteerSettled`].
        message_id: u64,
        text: String,
    },
    /// Switch model/effort. Applied only while the session has no history;
    /// otherwise the driver refuses with a `TuiOpFailed`.
    SetModel {
        model: Option<String>,
        effort: Option<String>,
    },
    /// Drop the current history and bind a fresh session id.
    NewSession {
        session_id: String,
    },
    /// Load a persisted session (abycore snapshot) and continue it.
    Resume {
        session_id: String,
    },
    /// List persisted sessions under `workspace/.abycore/sessions`.
    ListSessions {
        /// `/resume <prefix>` id prefix, echoed back in `SessionList`.
        prefix: Option<String>,
    },
    /// Fetch the provider's live model listing for the `/model` picker.
    FetchCatalog,
    /// Fetch the skills discovered at launch (`/skills`). A pure read of the
    /// launch snapshot, so it answers while a turn is running.
    FetchSkills,
    /// Change the local tool sandbox without dropping conversation history.
    SetPermission {
        preset: String,
    },
    /// Rotate the API key for the running agent (`/login`): the active agent
    /// rebuilds from its snapshot, so the next request carries the new key
    /// without a restart. `None` clears the key (`/logout`).
    SetApiKey {
        key: Option<String>,
    },
    /// Condense older history on demand (`/compact`), even below pressure.
    Compact,
    /// `/goal [objective|pause|resume|complete|clear|status]`: the session's one
    /// durable completion objective, and the round driver's arm switch.
    Goal {
        arg: String,
    },
    Shutdown,
}

/// Driver → UI events.
#[derive(Debug)]
pub enum Event {
    Ui(UiEvent),
    Ctl(CtlEvent),
    /// A tool call awaits authorization. The driver task blocks on the
    /// oneshot; the UI thread does not.
    PermissionAsk {
        title: String,
        options: Vec<AskOption>,
        reply: oneshot::Sender<PermissionReply>,
    },
}
