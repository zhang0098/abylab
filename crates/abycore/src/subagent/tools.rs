use super::*;
use crate::{CallBudget, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture};
use serde_json::{Value, json};
use std::sync::Weak;

const NAMES: [&str; 6] = [
    "subagent",
    "subagent_fork",
    "list_agents",
    "send_message",
    "interrupt_agent",
    "wait_agent",
];

pub(super) fn definitions(manager: Weak<Manager>) -> Vec<Arc<dyn Tool>> {
    NAMES
        .into_iter()
        .map(|name| {
            Arc::new(DelegationTool {
                manager: manager.clone(),
                name,
            }) as Arc<dyn Tool>
        })
        .collect()
}

struct DelegationTool {
    // The host owns the manager; tools held by resident children must not form an Arc cycle.
    manager: Weak<Manager>,
    name: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    description: String,
    prompt: String,
    #[serde(default)]
    run_in_background: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    agent_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    agent_id: String,
    message: String,
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> std::result::Result<T, ToolError> {
    serde_json::from_value(value)
        .map_err(|_| ToolError::Failed("invalid subagent tool arguments".into()))
}

fn failed(error: Error) -> ToolError {
    ToolError::Failed(error.to_string())
}

impl Tool for DelegationTool {
    /// Waiting on a child has no timer: the child's turn is what ends it, and
    /// the child runs under its own request, tool and budget limits. A minute —
    /// the deployment's backstop for a call that declares nothing — would cut off
    /// most of the work the child was asked to do. The child keeps running after
    /// a cancelled wait; `interrupt_agent` is the only thing that stops it.
    fn call_budget(&self, _: &Value) -> CallBudget {
        match self.name {
            "subagent" | "subagent_fork" | "wait_agent" => CallBudget::Unbounded,
            _ => CallBudget::Backstop,
        }
    }
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.name {
            "subagent" | "subagent_fork" => (
                if self.name == "subagent" {
                    "Delegate a focused, self-contained task to a separate agent. It starts with no conversation history. By default wait for its final answer; run_in_background returns an agent_id for later collection with wait_agent. Background agents can receive send_message follow-ups."
                } else {
                    "Delegate a task to a separate agent seeded with this conversation's completed turns. It cannot see the current in-flight turn. By default wait for its final answer; run_in_background returns an agent_id for wait_agent and send_message."
                },
                json!({
                    "description":{"type":"string","minLength":1,"description":"Short task label."},
                    "prompt":{"type":"string","minLength":1,"description":"Complete task, including context missing from the child's conversation."},
                    "run_in_background":{"type":"boolean","default":false}
                }),
                json!(["description", "prompt"]),
            ),
            "list_agents" => (
                "List your direct children and their current status. Completion notices are delivered at conversation boundaries; use wait_agent to collect a result.",
                json!({}),
                json!([]),
            ),
            "send_message" => (
                "Send a message to your direct child. A running child receives it after its current model response and tool batch; an idle child starts another turn. This confirms delivery, not the child's answer. Pending uncertain tools require host resolution first.",
                json!({"agent_id":{"type":"string","minLength":1},"message":{"type":"string","minLength":1}}),
                json!(["agent_id", "message"]),
            ),
            "interrupt_agent" => (
                "Request cancellation of a child's or descendant's current turn. Descendants continue independently. The child stays available unless an interrupted tool requires host resolution. Already idle children are unaffected.",
                json!({"agent_id":{"type":"string","minLength":1}}),
                json!(["agent_id"]),
            ),
            _ => (
                "Wait for your direct child's current turn and return its final answer and stop reason. The wait lasts as long as the child's turn does; cancelling the wait does not stop background work.",
                json!({"agent_id":{"type":"string","minLength":1}}),
                json!(["agent_id"]),
            ),
        };
        ToolDefinition {
            name: self.name.into(),
            description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        match self.name {
            "subagent" | "subagent_fork" => {
                let args: StartArgs = parse(arguments.clone())?;
                SubagentRequest::new(args.description, args.prompt)
                    .validate()
                    .map_err(failed)?;
            }
            "list_agents" => {
                if !arguments
                    .as_object()
                    .is_some_and(|object| object.is_empty())
                {
                    return Err(ToolError::Failed("list_agents takes no arguments".into()));
                }
            }
            "send_message" => {
                let args: MessageArgs = parse(arguments.clone())?;
                validate_id(&args.agent_id)?;
                Inbox::validate_message(&args.message).map_err(failed)?;
            }
            _ => {
                validate_id(&parse::<TargetArgs>(arguments.clone())?.agent_id)?;
            }
        }
        Ok(())
    }

    fn execute<'a>(&'a self, arguments: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate(&arguments)?;
            context.request.check().map_err(failed)?;
            let manager = self
                .manager
                .upgrade()
                .ok_or_else(|| ToolError::Failed("subagent manager has been dropped".into()))?;
            let parent = context.parent.as_ref().ok_or_else(|| {
                ToolError::Failed("subagent tools require a calling Agent".into())
            })?;
            match self.name {
                "subagent" | "subagent_fork" => {
                    let args: StartArgs = parse(arguments)?;
                    let request = SubagentRequest {
                        description: args.description,
                        prompt: args.prompt,
                        mode: if self.name == "subagent_fork" {
                            SubagentMode::Fork
                        } else {
                            SubagentMode::Spawn
                        },
                    };
                    let token = context.cancellation.child_token();
                    let _cancel_on_drop = token.clone().drop_guard();
                    let info = manager
                        .start(
                            parent,
                            request,
                            (!args.run_in_background).then_some(token),
                            args.run_in_background,
                        )
                        .map_err(failed)?;
                    if args.run_in_background {
                        Ok(render_info(&info, false))
                    } else {
                        let entry = manager.entry(&info.id).map_err(failed)?;
                        drop(manager);
                        let info = entry.wait().await;
                        Ok(render_info(&info, true))
                    }
                }
                "list_agents" => {
                    let infos: Vec<_> = manager
                        .store
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .entries
                        .values()
                        .map(|entry| entry.info())
                        .filter(|info| info.parent_id == parent.id)
                        .map(|mut info| {
                            info.result = None;
                            info
                        })
                        .collect();
                    Ok(ToolOutput::text(
                        serde_json::to_string(&infos).expect("serializable agent list"),
                    ))
                }
                "send_message" => {
                    let args: MessageArgs = parse(arguments)?;
                    manager
                        .authorize(parent, &args.agent_id, false)
                        .map_err(failed)?;
                    manager
                        .send_message(&args.agent_id, args.message)
                        .map_err(failed)?;
                    Ok(ToolOutput::text(format!(
                        "message delivered to agent {}",
                        args.agent_id
                    )))
                }
                "interrupt_agent" => {
                    let args: TargetArgs = parse(arguments)?;
                    let entry = manager
                        .authorize(parent, &args.agent_id, true)
                        .map_err(failed)?;
                    entry
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .cancellation
                        .cancel();
                    Ok(ToolOutput::text(format!(
                        "interrupt requested for agent {}",
                        args.agent_id
                    )))
                }
                _ => {
                    let args: TargetArgs = parse(arguments)?;
                    let entry = manager
                        .authorize(parent, &args.agent_id, false)
                        .map_err(failed)?;
                    drop(manager);
                    Ok(render_info(&entry.wait().await, true))
                }
            }
        })
    }
}

fn validate_id(id: &str) -> std::result::Result<(), ToolError> {
    if id.is_empty() || id.len() > 128 {
        return Err(ToolError::Failed(
            "agent_id must contain 1–128 bytes".into(),
        ));
    }
    Ok(())
}

fn render_info(info: &SubagentInfo, collected: bool) -> ToolOutput {
    let content = match &info.result {
        Some(result) if collected => format!(
            "agent_id: {}\nstop_reason: {:?}\n{}{}",
            info.id,
            result.stop_reason,
            result
                .error
                .as_ref()
                .map(|error| format!("{error}\n"))
                .unwrap_or_default(),
            result.output
        ),
        _ => format!("agent_id: {}\nstatus: {:?}", info.id, info.status),
    };
    let mut output = ToolOutput::text(content);
    output.is_error = collected
        && info
            .result
            .as_ref()
            .is_none_or(|result| result.stop_reason != StopReason::Completed);
    // Persist identity/status without duplicating potentially large child output in metadata.
    let metadata = json!({"agent_id":info.id,"parent_id":info.parent_id,"depth":info.depth,"mode":info.mode,"status":info.status,
        "stop_reason":info.result.as_ref().map(|result| &result.stop_reason)});
    output.details = Some(metadata.clone());
    output.meta = Some(json!({"subagent":metadata}));
    output
}
