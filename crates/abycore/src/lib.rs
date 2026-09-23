//! An asynchronous, in-process DeepSeek agent SDK.
//!
//! The host owns its Tokio runtime, tools, event handling and snapshot storage.
//! No requests are made until a client or agent operation is awaited.
#![deny(unsafe_code)]
#![doc = include_str!("../README.md")]

mod agent;
mod compaction;
mod config;
mod context;
mod deepseek;
mod error;
mod event;
mod goal;
mod hooks;
mod local_tools;
mod persist;
mod session;
mod skills;
mod subagent;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod subprocess_sandbox;
mod todo;
mod tool;
mod types;
#[cfg(feature = "web-search")]
mod web_search;

pub use agent::Agent;
pub use compaction::{
    CHECKPOINT_PREAMBLE, Compaction, ContextEstimate, MIN_PRUNE_BYTES, Prune,
    SUMMARIZE_INSTRUCTION, SUMMARY_CLOSE_TAG, SUMMARY_OPEN_TAG, SummarizeOptions, SummarizeOutcome,
    ViewChange, ViewCut, ViewEstimate, ViewRequest,
};
pub use config::{
    ClientConfig, ModelOptions, ReasoningEffort, RequestOptions, RetryPolicy, RunOptions,
    ToolChoice,
};
pub use deepseek::{DeepSeekClient, MessageRequest, MessageStream};
pub use error::{Error, ErrorKind, Result};
pub use event::{AgentEvent, RunOutcome, StopReason, StreamEvent};
pub use goal::{
    CreateGoalTool, DEFAULT_MAX_ROUNDS, GetGoalTool, Goal, GoalStatus, MAX_MAX_ROUNDS,
    UpdateGoalTool,
};
pub use hooks::{AgentHooks, CheckpointKind, ToolDecision};
pub use local_tools::{
    BashJob, BashJobOutput, BashJobStatus, BashOutputCursor, BashResult, BashStreamOutput,
    BashTool, EditTool, LocalToolConfig, LocalTools, PermissionMode, ReadTool, WriteTool,
};
pub use persist::{SessionStore, SessionSummary, SessionWriter};
pub use session::{PendingCall, PendingState, SessionSnapshot};
pub use skills::{
    MAX_SKILL_BYTES, SKILL_FILENAME, SKILLS_DIR, Skill, SkillCatalog, SkillInvocation, SkillTool,
};
pub use subagent::{
    SteerHandle, SubagentConfig, SubagentEvent, SubagentHookFactory, SubagentInfo, SubagentMode,
    SubagentRequest, SubagentResult, SubagentStatus, Subagents,
};
pub use todo::{PlanView, TodoCounts, TodoItem, TodoStatus, TodoWriteTool};
pub use tokio_util::sync::CancellationToken;
pub use tool::{
    CallBudget, NO_DEADLINE, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput,
};
pub use types::{
    ContentPart, Item, MessageRole, ModelInfo, RequestPurpose, RequestRecord, Response,
    ResponseStatus, ThinkingSignature, Usage,
};
#[cfg(feature = "web-search")]
pub use web_search::{DeepSeekWebSearch, SearchConfig, SearchRequest, SearchResult, SearchSource};
