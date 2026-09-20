//! abylab backend: the abycore-driven agent driver behind the TUI's event bus.
//!
//! The TUI speaks [`contract::Event`] / [`contract::Cmd`]; the driver maps
//! them onto the abycore Agent SDK (`Agent::run`, `AgentEvent`, hooks).

pub mod contract;
pub mod driver;
mod instructions;

pub use abycore::MIN_PRUNE_BYTES;
pub use contract::{
    AskOption, Cmd, CompactionConfig, CtlEvent, DriverConfig, Event, PermissionReply, PlanItem,
    PlanStatus, TurnLimits, UiEvent, persisted_session_id,
};
pub use driver::DriverHandle;
