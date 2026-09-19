//! Optional host persistence and authorization barriers.
use crate::{PendingCall, Result, SessionSnapshot, ToolOutput};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// A safe point in the agent loop. The host must durably save before returning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CheckpointKind {
    RunStarted,
    /// Queued subagent messages/notices entered the transcript at a safe boundary.
    MessagesReceived,
    ModelResponse,
    /// A prune or compaction was committed before the next model request.
    ViewChanged,
    ToolIntent {
        call: PendingCall,
    },
    ToolResult {
        call_id: String,
        output: ToolOutput,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Deny(String),
}

/// Hooks run outside the tool execution timeout, but inside the run's deadline.
/// A failed checkpoint prevents the next network request or tool execution.
/// The host still saves `Agent::snapshot()` after run returns (including errors).
/// Hooks are not restored from a snapshot; install them before each resumed run.
pub trait AgentHooks: Send + Sync {
    /// Stable host context prepended as a user message to every conversation
    /// and summary request, including token/byte measurements. This is outside
    /// the compactable transcript and is not saved in snapshots: the host must
    /// reload it on restore. Keep it unchanged until installing new hooks with
    /// `Agent::set_hooks`, so provider calibration remains valid.
    ///
    /// Children inherit this context unless their own hooks return `Some`.
    fn request_context(&self) -> Option<&str> {
        None
    }

    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>>;

    fn authorize<'a>(&'a self, _call: PendingCall) -> BoxFuture<'a, Result<ToolDecision>> {
        Box::pin(async { Ok(ToolDecision::Allow) })
    }

    /// Asked at every request boundary, before the request is built, with the
    /// live measurement and every safe cut. Returning `Some` asks the SDK to
    /// apply that change to the model-visible view; `None` sends the request
    /// unchanged.
    ///
    /// This is harness's `agent/pre-step` compaction seam: policy (thresholds,
    /// retention, pruning) stays in the host, mechanics (validation, the
    /// summarization call, the replacement) stay in the SDK. The hook may be
    /// asked again after a change is applied, so a policy can prune and then
    /// condense in one boundary.
    fn view_request<'a>(
        &'a self,
        _estimate: &'a crate::ViewEstimate,
    ) -> BoxFuture<'a, Result<Option<crate::ViewRequest>>> {
        Box::pin(async { Ok(None) })
    }

    /// Advisory text queued after this tool result and delivered to the model
    /// with the next request, exactly like a host message.
    ///
    /// This is the SDK's seam for a loop guard: the harness ships
    /// `dsh-repeat-tool-reminder`, which nudges a model that repeats the same
    /// call instead of blocking it. Returning `Some` never changes the tool
    /// result, never delays execution and cannot re-run the tool — the text
    /// enters the transcript at the next step boundary, after the result the
    /// model would have seen anyway. `None` leaves the loop untouched.
    fn tool_reminder<'a>(
        &'a self,
        _call: &'a PendingCall,
        _output: &'a ToolOutput,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }
}
