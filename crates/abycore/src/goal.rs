//! One durable completion objective per session, harness's goal domain in SDK form.
//!
//! deepseek-harness keeps a single current goal per session and splits the
//! feature three ways: a goal service, model tools (`get_goal`, `create_goal`,
//! `update_goal`), and a round driver that spends configured rounds on an
//! *armed* goal. abycore carries the same state and tools; the host owns the
//! command surface (`/goal`) and decides whether to keep spending rounds, so
//! nothing in the SDK ever starts a turn on its own.
//!
//! The goal records completion state, not scheduling: `rounds_started` and
//! `max_rounds` are bookkeeping the host's round driver reads, and `revision`
//! makes every model update an explicit compare-and-set, exactly like harness's
//! "updates require the exact goal id and revision returned by a prior read".

use crate::{
    Error, ErrorKind, Result, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Lifecycle of the session's single goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "complete" => Some(Self::Complete),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }
}

/// The session's durable completion objective.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub id: String,
    /// Bumped by every accepted update; model updates must quote it.
    pub revision: u64,
    pub objective: String,
    pub status: GoalStatus,
    /// Rounds the host's driver has already spent on this goal.
    pub rounds_started: u64,
    /// Optional total round allowance. None means no round limit.
    #[serde(default)]
    pub max_rounds: Option<u64>,
    /// Optional reason, e.g. why the goal is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Goal {
    /// Rounds still available to a driver; None means unlimited.
    pub fn remaining_rounds(&self) -> Option<u64> {
        self.max_rounds
            .map(|limit| limit.saturating_sub(self.rounds_started))
    }

    /// Whether a driver may start another round right now.
    pub fn may_start_round(&self) -> bool {
        self.status == GoalStatus::Active && self.remaining_rounds() != Some(0)
    }

    /// Line the model and the transcript both read.
    pub fn summary(&self) -> String {
        let note = self
            .note
            .as_deref()
            .filter(|note| !note.trim().is_empty())
            .map(|note| format!(" · {note}"))
            .unwrap_or_default();
        format!(
            "{} · {} · round {}/{}{}",
            self.objective,
            self.status.label(),
            self.rounds_started,
            self.max_rounds
                .map(|limit| limit.to_string())
                .unwrap_or_else(|| "unlimited".into()),
            note
        )
    }
}

/// Validate a stored goal (snapshot load and every mutation path).
pub(crate) fn validate_goal(goal: Option<&Goal>) -> Result<()> {
    let Some(goal) = goal else {
        return Ok(());
    };
    if goal.id.trim().is_empty() || goal.objective.trim().is_empty() {
        return Err(Error::new(
            ErrorKind::Session,
            "goal id and objective must not be empty",
        ));
    }
    if goal.max_rounds == Some(0) {
        return Err(Error::new(
            ErrorKind::Session,
            "goal max_rounds must be a positive integer or null (unlimited)",
        ));
    }
    Ok(())
}

/// Build a fresh active goal.
pub(crate) fn new_goal(objective: String, max_rounds: Option<u64>) -> Result<Goal> {
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        return Err(Error::new(
            ErrorKind::Configuration,
            "a goal needs an objective",
        ));
    }
    if max_rounds == Some(0) {
        return Err(Error::new(
            ErrorKind::Configuration,
            "goal max_rounds must be a positive integer",
        ));
    }
    Ok(Goal {
        id: format!("goal-{:016x}", rand::random::<u64>()),
        revision: 1,
        objective,
        status: GoalStatus::Active,
        rounds_started: 0,
        max_rounds,
        note: None,
    })
}

/// A model-visible goal snapshot (the `get_goal` wire form).
fn goal_json(goal: Option<&Goal>) -> Value {
    match goal {
        Some(goal) => json!({
            "id": goal.id,
            "revision": goal.revision,
            "objective": goal.objective,
            "status": goal.status.label(),
            "rounds_started": goal.rounds_started,
            "max_rounds": goal.max_rounds,
            "note": goal.note,
        }),
        None => Value::Null,
    }
}

/// The model-facing `get_goal` answer: readable JSON, or a plain "none" line.
pub(crate) fn goal_json_text(goal: Option<&Goal>) -> String {
    match goal {
        Some(goal) => serde_json::to_string(&goal_json(Some(goal)))
            .unwrap_or_else(|_| "goal unavailable".into()),
        None => "no goal is set".into(),
    }
}

/// What a goal mutation asked for, decoded from `meta["goal_request"]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GoalRequest {
    Create {
        objective: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rounds: Option<u64>,
    },
    Update {
        goal_id: String,
        revision: u64,
        status: GoalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

impl GoalRequest {
    pub(crate) fn parse(value: &Value) -> std::result::Result<Self, String> {
        serde_json::from_value(value.clone()).map_err(|_| "invalid goal request".to_string())
    }
}

/// Apply a decoded request to the stored goal, returning the new value.
///
/// `create` refuses while a goal exists unless it is complete, and `update`
/// requires the quoted id and revision, so a stale model cannot overwrite a
/// goal that moved on.
pub(crate) fn apply_goal_request(current: Option<&Goal>, request: &GoalRequest) -> Result<Goal> {
    match request {
        GoalRequest::Create {
            objective,
            max_rounds,
        } => {
            if current.is_some_and(|goal| goal.status != GoalStatus::Complete) {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "a goal already exists; update it instead of creating another",
                ));
            }
            let mut goal = new_goal(objective.clone(), *max_rounds)?;
            goal.status = GoalStatus::Paused;
            goal.note = Some("awaiting /goal resume".into());
            Ok(goal)
        }
        GoalRequest::Update {
            goal_id,
            revision,
            status,
            note,
        } => {
            let Some(goal) = current else {
                return Err(Error::new(ErrorKind::Configuration, "no goal to update"));
            };
            if goal.id != *goal_id {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "goal id does not match the current goal",
                ));
            }
            if goal.revision != *revision {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    format!(
                        "goal revision {} is stale; read the goal again (current {})",
                        revision, goal.revision
                    ),
                ));
            }
            if *status == GoalStatus::Active {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "only the host can resume a goal",
                ));
            }
            Ok(Goal {
                revision: goal.revision.saturating_add(1),
                status: *status,
                note: note.clone().filter(|note| !note.trim().is_empty()),
                ..goal.clone()
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Model tools
// ---------------------------------------------------------------------------

fn tool_error(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}

/// `get_goal`: read the current goal, if any.
#[derive(Clone, Copy, Debug, Default)]
pub struct GetGoalTool;

impl Tool for GetGoalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "get_goal".into(),
            description: "Read the session's current completion objective (id, revision, status, rounds used). Call this before update_goal: updates must quote the exact id and revision.".into(),
            parameters: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        match arguments.as_object() {
            Some(object) if object.is_empty() => Ok(()),
            _ => Err(tool_error("get_goal takes no arguments")),
        }
    }

    fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
        // The committed snapshot is substituted by the agent, which is the only
        // place that knows the goal; this placeholder never reaches the model.
        Box::pin(async { Ok(ToolOutput::text("")) })
    }
}

/// `create_goal`: start the session's goal from a direct human request.
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateGoalTool;

impl Tool for CreateGoalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_goal".into(),
            description: "Create a paused completion objective for a direct, long-running user request. The user starts it with /goal resume; creation does not start autonomous work. It fails while an unfinished goal exists. Omit max_rounds for no round limit; set a positive total only when the user specifies a limit.".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "objective":{"type":"string","minLength":1},
                    "max_rounds":{"type":"integer","minimum":1}
                },
                "required":["objective"],
                "additionalProperties":false
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| tool_error("create_goal takes an object"))?;
        // The schema is additionalProperties:false; a count check alone let
        // `{"objective":"x","bogus":1}` through.
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "objective" | "max_rounds"))
        {
            return Err(tool_error(
                "create_goal takes objective and optional max_rounds",
            ));
        }
        if !object
            .get("objective")
            .and_then(Value::as_str)
            .is_some_and(|objective| !objective.trim().is_empty())
        {
            return Err(tool_error("objective must be a non-empty string"));
        }
        if let Some(rounds) = object.get("max_rounds")
            && !rounds.as_u64().is_some_and(|rounds| rounds > 0)
        {
            return Err(tool_error("max_rounds must be a positive integer"));
        }
        Ok(())
    }

    fn execute<'a>(&'a self, arguments: Value, _: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            // The raw arguments are the tool schema, not the tagged meta form.
            let request = GoalRequest::Create {
                objective: arguments["objective"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                max_rounds: arguments.get("max_rounds").and_then(Value::as_u64),
            };
            Ok(ToolOutput::text("goal created").with_meta(json!({ "goal_request": request })))
        })
    }
}

/// `update_goal`: move the goal's status with compare-and-set.
#[derive(Clone, Copy, Debug, Default)]
pub struct UpdateGoalTool;

impl Tool for UpdateGoalTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "update_goal".into(),
            description: "Update the session's goal: pause at the user's request, mark complete when achieved, or blocked when progress is impossible. Only the host can resume a goal. Quote the exact id and revision from get_goal; stale updates are refused. Use note to explain a block.".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "goal_id":{"type":"string","minLength":1},
                    "revision":{"type":"integer","minimum":1},
                    "status":{"type":"string","enum":["paused","blocked","complete"]},
                    "note":{"type":"string"}
                },
                "required":["goal_id","revision","status"],
                "additionalProperties":false
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| tool_error("update_goal takes an object"))?;
        // The schema is additionalProperties:false; unknown fields have to be
        // refused here too, or the declared contract is not enforced.
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "goal_id" | "revision" | "status" | "note"))
        {
            return Err(tool_error(
                "update_goal takes goal_id, revision, status and optional note",
            ));
        }
        if !object
            .get("goal_id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
        {
            return Err(tool_error("goal_id is required"));
        }
        if !object
            .get("revision")
            .and_then(Value::as_u64)
            .is_some_and(|revision| revision > 0)
        {
            return Err(tool_error("revision is required"));
        }
        if !object
            .get("status")
            .and_then(Value::as_str)
            .and_then(GoalStatus::parse)
            .is_some_and(|status| status != GoalStatus::Active)
        {
            return Err(tool_error(
                "status must be paused, blocked or complete; only the host can resume",
            ));
        }
        Ok(())
    }

    fn execute<'a>(&'a self, arguments: Value, _: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let status = arguments["status"]
                .as_str()
                .and_then(GoalStatus::parse)
                .ok_or_else(|| tool_error("status must be active, paused, blocked or complete"))?;
            let request = GoalRequest::Update {
                goal_id: arguments["goal_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                revision: arguments["revision"].as_u64().unwrap_or_default(),
                status,
                note: arguments
                    .get("note")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            Ok(ToolOutput::text("goal updated").with_meta(json!({ "goal_request": request })))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tool schemas declare `additionalProperties: false`; the validators
    /// have to refuse unknown fields, not only count them.
    #[test]
    fn goal_tools_reject_unknown_fields_like_their_schemas_say() {
        assert!(
            CreateGoalTool
                .validate(&json!({"objective": "x", "bogus": 1}))
                .is_err()
        );
        assert!(CreateGoalTool.validate(&json!({"objective": "x"})).is_ok());
        assert!(
            CreateGoalTool
                .validate(&json!({"objective": "x", "max_rounds": 3}))
                .is_ok()
        );
        assert!(
            UpdateGoalTool
                .validate(&json!({
                    "goal_id": "g", "revision": 1, "status": "active", "bogus": true
                }))
                .is_err()
        );
        assert!(
            UpdateGoalTool
                .validate(&json!({"goal_id": "g", "revision": 1, "status": "paused"}))
                .is_ok()
        );
        assert!(
            UpdateGoalTool
                .validate(&json!({
                    "goal_id": "g", "revision": 1, "status": "blocked", "note": "why"
                }))
                .is_ok()
        );
    }
}
