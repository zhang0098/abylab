use crate::{ErrorKind, Item, RequestRecord, Response, ToolOutput};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
/// Normalized model events. `sequence` is a local SSE frame counter, not a provider field.
pub enum StreamEvent {
    Started {
        response_id: String,
        sequence: u64,
    },
    TextDelta {
        output_index: usize,
        content_index: usize,
        item_id: String,
        delta: String,
        sequence: u64,
    },
    ReasoningDelta {
        output_index: usize,
        content_index: usize,
        item_id: String,
        delta: String,
        sequence: u64,
    },
    ToolArgumentsDelta {
        output_index: usize,
        item_id: String,
        /// Provider tool-call id, so a host can render the call while it streams.
        call_id: String,
        name: String,
        delta: String,
        sequence: u64,
    },
    ItemDone {
        output_index: usize,
        item: Item,
        sequence: u64,
    },
    Finished {
        response: Box<Response>,
        sequence: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    Completed,
    Incomplete,
    Failed,
    Error(ErrorKind),
}

#[derive(Clone, Debug)]
pub enum AgentEvent {
    RunStarted {
        run: u64,
    },
    /// Current checklist at run entry, and after each committed todo_write update.
    /// None clears the view; continue_run republishes the unfinished turn's existing plan.
    PlanChanged {
        plan: Option<crate::PlanView>,
    },
    Model(StreamEvent),
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolFinished {
        call_id: String,
        output: ToolOutput,
    },
    RunFinished {
        run: u64,
        reason: StopReason,
    },
    /// The session's goal at run entry, and after every committed goal update.
    GoalChanged {
        goal: Option<crate::Goal>,
    },
    /// The host's view policy reshaped the model-visible history at a request
    /// boundary (harness's `agent/pre-step` seam). The transcript is unchanged;
    /// only what the next request carries moved.
    ViewChanged {
        change: crate::ViewChange,
    },
}

#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub stop_reason: StopReason,
    pub response: Response,
    pub new_items: Vec<Item>,
    /// Every dispatch from this run; unknown usage is never treated as zero.
    pub requests: Vec<RequestRecord>,
}
