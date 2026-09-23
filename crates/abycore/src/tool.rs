use crate::{
    CancellationToken, Error, ErrorKind, Result, context::RequestContext, types::valid_tool_name,
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDefinition {
    pub(crate) fn check(&self) -> Result<()> {
        if !valid_tool_name(&self.name)
            || self.parameters.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "tools require a valid name and an object JSON schema",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub call_id: String,
    pub cancellation: CancellationToken,
    /// When this call's budget runs out. A call with no budget of its own
    /// ([`CallBudget::Unbounded`]) carries [`NO_DEADLINE`] instead: nothing arms
    /// that horizon, so a tool reading `remaining()` should treat it as "no
    /// deadline" rather than a very generous one.
    pub deadline: Instant,
    pub max_output_bytes: usize,
    #[cfg_attr(not(feature = "web-search"), allow(dead_code))]
    pub(crate) request: RequestContext,
    pub(crate) local_session: std::sync::Arc<crate::local_tools::LocalSession>,
    pub(crate) parent: Option<std::sync::Arc<crate::subagent::Parent>>,
}

/// The horizon [`ToolContext::deadline`] carries for a call with no budget: far
/// beyond any real work, close enough that the timer wheel can represent it
/// (tokio's ceiling is a little over two years).
pub const NO_DEADLINE: Duration = Duration::from_secs(365 * 24 * 60 * 60);

impl ToolContext {
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
    /// Structured host/UI result; never included in the model-facing text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Replayable host/UI metadata. Persisted with the result, never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            truncated: false,
            details: None,
            meta: None,
        }
    }
    /// Attach replayable host metadata (never sent to the model).
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            truncated: false,
            details: None,
            meta: None,
        }
    }
    pub(crate) fn bounded(mut self, limit: usize) -> Self {
        if self.content.len() > limit {
            let marker = "\n[tool output truncated]";
            let mut end = limit.saturating_sub(marker.len());
            while !self.content.is_char_boundary(end) {
                end -= 1;
            }
            self.content.truncate(end);
            self.content.push_str(marker);
            self.truncated = true;
        }
        self
    }
    pub(crate) fn wire(&self) -> String {
        if self.is_error {
            format!("Tool error: {}", self.content)
        } else {
            self.content.clone()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// A known failure; safe to report to the model as a tool result.
    #[error("{0}")]
    Failed(String),
    /// Side effects may have occurred. The host must explicitly resolve this call.
    #[error("{0}")]
    Uncertain(String),
}

pub type ToolFuture<'a> = BoxFuture<'a, std::result::Result<ToolOutput, ToolError>>;

/// How one tool call is bounded, as the tool itself declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallBudget {
    /// The tool declares nothing: the deployment's backstop applies
    /// ([`crate::RunOptions::tool_timeout`]).
    Backstop,
    /// The tool's own budget for these arguments, the way deepseek-harness's
    /// `ToolDefinition.timeoutMs` belongs to the tool.
    Own(Duration),
    /// No timer at all: the work itself is what bounds the call, because it has
    /// its own limits (a delegation waits out a child turn that runs under the
    /// child's budgets). Only cancellation ends it.
    Unbounded,
}

/// Validation must check the schema and business rules without side effects.
/// Execute is never automatically retried. Cancellation drops its future.
pub trait Tool: Send + Sync {
    /// Extra time for orderly cleanup after the tool's execution deadline.
    /// Cancellation comes first: the call is asked to stop, then given this long
    /// to settle, and only a tool that still will not settle is dropped.
    fn cleanup_grace(&self) -> Duration {
        Duration::ZERO
    }
    /// What bounds this call. A declaration belongs to the tool — the model's
    /// `timeoutMs` for `bash`, a delegation that waits as long as its child runs
    /// — and the executor enforces it by answering the expiry with a tool result
    /// the model can read, so the turn continues. The backstop applies only to
    /// calls that declare nothing.
    fn call_budget(&self, arguments: &Value) -> CallBudget {
        let _ = arguments;
        CallBudget::Backstop
    }
    fn definition(&self) -> ToolDefinition;
    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError>;
    fn execute<'a>(&'a self, arguments: Value, context: ToolContext) -> ToolFuture<'a>;
}
