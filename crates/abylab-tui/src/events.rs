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
    /// Incremental tool-call arguments: the call's card renders its title while
    /// the model is still writing it.
    ToolCallDelta {
        session: String,
        call_id: String,
        name: String,
        delta: String,
    },
    /// Execution begins for a call whose card the streamed call created; the
    /// tool timer starts here, not at the first argument delta.
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
    Usage {
        session: String,
        input: u64,
        output: u64,
        cached: u64,
        reasoning: u64,
    },
    #[allow(dead_code)] // host-injected user context; awaiting a direct driver producer
    UserInjected {
        session: String,
        source: String,
        preview: String,
    },
    /// abycore subagent lifecycle (driver broadcast forwarder).
    SubagentStarted {
        parent: String,
        child: String,
        /// Spawn description — the child view's human label.
        label: Option<String>,
    },
    /// abycore subagent lifecycle (driver broadcast forwarder).
    SubagentFinished {
        child: String,
    },
    /// ACP `user_message_chunk` (session/load replay needs user lines).
    UserMessage {
        session: String,
        text: String,
    },
    /// ACP `session_info_update` title.
    #[allow(dead_code)] // dsh `session_info_update` title; awaiting a direct driver producer
    SessionTitle {
        session: String,
        title: String,
    },
    /// ACP `plan` / `plan_update` snapshot (todo entries, not `plan/mode`).
    /// `summary` feeds the transcript's plan cell; the structured fields feed
    /// the composer's live todo line and its clickable progress dialog, so the
    /// TUI owns their wording. An empty `summary` hides every surface.
    Plan {
        session: String,
        summary: String,
        /// The whole checklist, in list order — the dialog lists every row.
        todos: Vec<PlanItem>,
        /// First in-progress task, in list order.
        active: Option<String>,
        /// Additional concurrently in-progress tasks.
        active_extra: usize,
        completed: usize,
        total: usize,
    },
    /// `plan/mode` — dsh-plan-mode collaboration state (last one wins);
    /// awaiting a direct driver producer (plan mode facts currently ride
    /// the permission facts emission).
    #[allow(dead_code)]
    PlanMode {
        session: String,
        active: bool,
    },
    /// `sandbox/mode` — file policy: read-only | workspace-write | danger-full-access.
    SandboxMode {
        session: String,
        mode: String,
    },
    /// `approval/policy` — ask | never.
    ApprovalPolicy {
        session: String,
        policy: String,
    },
    /// `permission/preset` — bundled permission preset name.
    PermissionPreset {
        session: String,
        preset: String,
    },
    /// `approval/asked` — one pending approval request; awaiting a direct
    /// driver producer (the driver's permission ask uses the overlay).
    #[allow(dead_code)]
    ApprovalAsked {
        session: String,
        tool: String,
        reason: Option<String>,
    },
    /// `approval/decided` — its outcome; awaiting a direct driver producer.
    #[allow(dead_code)]
    ApprovalDecided {
        session: String,
        outcome: String,
    },
}

impl UiEvent {
    /// Live todo progress behind the composer's todo line and its clickable
    /// progress dialog, or `None` when a plan event carries no checklist
    /// (`UiEvent::Plan` with an empty summary). The transcript keeps the full
    /// digest; the cap row shows only the task in progress plus completed/total,
    /// worded by the active locale.
    pub fn plan_progress(&self) -> Option<PlanProgress> {
        match self {
            UiEvent::Plan {
                summary,
                todos,
                active,
                active_extra,
                completed,
                total,
                ..
            } if !summary.is_empty() && *total > 0 => Some(PlanProgress {
                todos: todos.clone(),
                active: active.clone(),
                active_extra: *active_extra,
                completed: *completed,
                total: *total,
            }),
            _ => None,
        }
    }
}

/// The checklist facts the composer's todo line and progress dialog render
/// (see [`UiEvent::plan_progress`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanProgress {
    /// The whole checklist, in list order.
    pub todos: Vec<PlanItem>,
    /// First in-progress task, in list order.
    pub active: Option<String>,
    /// Additional concurrently in-progress tasks.
    pub active_extra: usize,
    pub completed: usize,
    pub total: usize,
}

/// One checklist row, in the order the agent wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub status: PlanStatus,
}

/// Lifecycle of one [`PlanItem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}
