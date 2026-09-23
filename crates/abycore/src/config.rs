use crate::{CancellationToken, Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

/// HTTP configuration. Debug deliberately excludes credentials and proxy URLs.
#[derive(Clone)]
pub struct ClientConfig {
    pub api_key: String,
    /// Messages API root, like Harness: append `/v1/messages` unless already ending in `/v1`.
    pub base_url: String,
    pub proxy: Option<String>,
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub max_response_bytes: usize,
    pub max_event_bytes: usize,
    pub retry: RetryPolicy,
}

impl ClientConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            proxy: None,
            connect_timeout: Duration::from_secs(15),
            first_byte_timeout: Duration::from_secs(120),
            stream_idle_timeout: Duration::from_secs(60),
            max_response_bytes: 16 * 1024 * 1024,
            max_event_bytes: 1024 * 1024,
            retry: RetryPolicy::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.api_key.trim().is_empty() || self.api_key.chars().any(char::is_control) {
            return Err(Error::new(
                ErrorKind::Configuration,
                "API key is empty or contains control characters",
            ));
        }
        if self.connect_timeout.is_zero()
            || self.first_byte_timeout.is_zero()
            || self.stream_idle_timeout.is_zero()
            || self.max_response_bytes == 0
            || self.max_event_bytes == 0
            || self.retry.initial_delay.is_zero()
            || self.retry.max_delay < self.retry.initial_delay
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "timeouts, size limits and retry delays must be positive and ordered",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("credentials", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Additional attempts, before a response stream has been delivered.
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(15),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Off,
    Low,
    #[default]
    High,
    Max,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptions {
    pub model: String,
    pub reasoning: ReasoningEffort,
    pub max_tokens: u32,
    pub tool_choice: ToolChoice,
}

impl Default for ModelOptions {
    fn default() -> Self {
        Self {
            model: "deepseek-flash".into(),
            reasoning: ReasoningEffort::High,
            max_tokens: 256_000,
            tool_choice: ToolChoice::Auto,
        }
    }
}

impl ModelOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() || self.max_tokens == 0 {
            return Err(Error::new(
                ErrorKind::Configuration,
                "model and max_tokens are required",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RequestOptions {
    pub cancellation: CancellationToken,
    /// How long this call may go without progress. `None` leaves it to the
    /// transport's own timeouts, which is where a stalled request belongs.
    pub timeout: Option<Duration>,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            timeout: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub cancellation: CancellationToken,
    /// An optional cap on how long a run may go **without progress**: no bytes
    /// from the provider, no request or tool call finishing, no committed step.
    /// It is a gap between two moments of work, never a cap on the segment's
    /// total duration.
    ///
    /// `None` — the default — adds nothing, matching deepseek-harness's agent
    /// loop, which has no deadline over a step at all. What bounds a run then:
    /// each request under the transport's own connect/first-byte/stream-idle
    /// timeouts, each tool call under its budget (declared, or
    /// [`RunOptions::tool_timeout`]), and the request/tool budgets above. A host
    /// that wants a stalled run to fail on its own can set one here.
    pub timeout: Option<Duration>,
    /// This agent turn's HTTP dispatches, including retries and auxiliary searches.
    /// Subagent turns have independent budgets configured by `SubagentConfig`.
    pub max_requests: usize,
    pub max_tool_calls: usize,
    /// The backstop for one tool call that declares no budget of its own:
    /// [`Tool::call_budget`](crate::Tool::call_budget) is what a tool that knows
    /// its own work (a command with `timeoutMs`, a delegation waiting on a child)
    /// answers with, and that value is the call's budget. Expiry is reported to
    /// the model as a tool error, not as a failed turn.
    pub tool_timeout: Duration,
    pub max_tool_output_bytes: usize,
    /// Serialized input budget in bytes, not an estimate of model tokens.
    pub max_input_bytes: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            timeout: None,
            max_requests: 16,
            max_tool_calls: 32,
            tool_timeout: Duration::from_secs(60),
            max_tool_output_bytes: 64 * 1024,
            max_input_bytes: 4 * 1024 * 1024,
        }
    }
}

impl RunOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.timeout.is_some_and(|timeout| timeout.is_zero())
            || self.tool_timeout.is_zero()
            || self.max_requests == 0
            || self.max_tool_calls == 0
            || self.max_tool_output_bytes < 64
            || self.max_input_bytes == 0
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "run limits must be positive; tool output limit must be at least 64 bytes",
            ));
        }
        Ok(())
    }
}
