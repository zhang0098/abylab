//! abylab backend: the abycore-driven agent driver behind the TUI's event bus.
//!
//! The TUI speaks [`contract::Event`] / [`contract::Cmd`]; the driver maps
//! them onto the abycore Agent SDK (`Agent::run`, `AgentEvent`, hooks).

pub mod contract;
pub mod driver;
mod instructions;
mod queue_store;
mod user_questions;

pub use abycore::MIN_PRUNE_BYTES;
pub use contract::{
    AskOption, Cmd, CompactionConfig, CtlEvent, DriverConfig, Event, PermissionReply, PlanItem,
    PlanStatus, PromptPart, QueueAction, QueuePlacement, QueueRow, SteerRequest, TurnLimits,
    UiEvent, persisted_session_id,
};
pub use contract::{UserQuestion, UserQuestionOption, UserQuestionReply};
pub use driver::DriverHandle;
