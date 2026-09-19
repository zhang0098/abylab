//! Whole-list task updates and a host-facing progress view, following Harness's todo_write.
use crate::{
    Error, ErrorKind, Result, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// One flat task. Updates replace the complete list; items have no stable IDs or children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TodoCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
}

/// Derived data for a checklist or compact progress strip; rendering belongs to the host.
/// An empty list represents an explicit clear, while `Agent::plan_view() == None` means
/// no list has been written in the current turn. Neither should display a task panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub todos: Vec<TodoItem>,
    pub counts: TodoCounts,
    pub total: usize,
    /// First active task in list order.
    pub active_content: Option<String>,
    /// Other concurrently active tasks, for a compact "first task + N" summary.
    pub active_extra: usize,
}

impl PlanView {
    pub(crate) fn new(todos: Vec<TodoItem>) -> Self {
        let mut counts = TodoCounts::default();
        let mut active_content = None;
        for todo in &todos {
            match todo.status {
                TodoStatus::Pending => counts.pending += 1,
                TodoStatus::InProgress => {
                    counts.in_progress += 1;
                    if active_content.is_none() {
                        active_content = Some(todo.content.clone());
                    }
                }
                TodoStatus::Completed => counts.completed += 1,
            }
        }
        Self {
            total: todos.len(),
            todos,
            counts,
            active_content,
            active_extra: counts.in_progress.saturating_sub(1),
        }
    }

    /// Read a successful committed update from `AgentEvent::ToolFinished` or a checkpoint.
    /// Uses the canonical metadata, never proposed arguments or possibly truncated text.
    /// Errors and unrelated outputs return None; malformed todo metadata is an error.
    pub fn from_tool_output(output: &ToolOutput) -> Result<Option<Self>> {
        if output.is_error {
            return Ok(None);
        }
        let Some(value) = output.meta.as_ref().and_then(|meta| meta.get("todo_write")) else {
            return Ok(None);
        };
        let update: TodoUpdate = serde_json::from_value(value.clone())
            .map_err(|_| Error::new(ErrorKind::Session, "invalid todo_write result metadata"))?;
        validate_stored(&update.todos)?;
        Ok(Some(Self::new(update.todos)))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoUpdate {
    todos: Vec<TodoItem>,
}

/// Stateless and safe to share across agents: each result updates only its calling session.
/// The parallelism policy is explicit, as in Harness. Register with `Agent::register_tool`.
#[derive(Clone, Debug)]
pub struct TodoWriteTool {
    allow_parallel_in_progress: bool,
}

impl TodoWriteTool {
    pub fn new(allow_parallel_in_progress: bool) -> Self {
        Self {
            allow_parallel_in_progress,
        }
    }

    fn parse(&self, arguments: Value) -> std::result::Result<Vec<TodoItem>, ToolError> {
        let mut update: TodoUpdate = serde_json::from_value(arguments)
            .map_err(|_| failed("todos must be an array of {content, status}; unknown fields and statuses are not accepted"))?;
        for todo in &mut update.todos {
            todo.content = todo.content.trim().to_owned();
        }
        validate_stored(&update.todos).map_err(|error| failed(&error.message))?;
        if !self.allow_parallel_in_progress
            && update
                .todos
                .iter()
                .filter(|todo| todo.status == TodoStatus::InProgress)
                .count()
                > 1
        {
            return Err(failed("at most one task may be in_progress"));
        }
        Ok(update.todos)
    }
}

impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDefinition {
        let active = if self.allow_parallel_in_progress {
            "Mark every actively worked task in_progress; several may be active when work actually runs in parallel, such as subagents or background commands. "
        } else {
            "Keep AT MOST ONE task in_progress at a time. "
        };
        ToolDefinition {
            name: "todo_write".into(),
            description: format!(
                "Record and update a structured task list for the current work. Send the ENTIRE list every call: it REPLACES the previous list, with no partial updates or per-item edits. Use one concrete task per step to plan multi-step work and show progress. {active}While work remains, keep at least one task in_progress. Mark each task completed as soon as it finishes; use no active task once all work is complete. Skip trivial single-step tasks. Statuses: pending, in_progress, completed. Send an empty list to clear the plan."
            ),
            parameters: json!({
                "type":"object", "additionalProperties":false, "required":["todos"],
                "properties":{"todos":{
                    "type":"array", "description":"The COMPLETE task list, replacing the previous list.",
                    "items":{
                        "type":"object", "additionalProperties":false, "required":["content","status"],
                        "properties":{
                            "content":{"type":"string","description":"A short, concrete task description."},
                            "status":{"type":"string","enum":["pending","in_progress","completed"]}
                        }
                    }
                }}
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        self.parse(arguments.clone()).map(|_| ())
    }

    fn execute<'a>(&'a self, arguments: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            context
                .request
                .check()
                .map_err(|error| failed(&error.to_string()))?;
            let view = PlanView::new(self.parse(arguments)?);
            let mut output = ToolOutput::text(format!(
                "Updated todo list: {} pending, {} in progress, {} completed.",
                view.counts.pending, view.counts.in_progress, view.counts.completed,
            ));
            output.details = Some(json!({"todos":view.todos,"counts":view.counts}));
            output.meta = Some(json!({"todo_write":{"todos":view.todos}}));
            // Bound the full structured result too: truncating a checklist would change its meaning.
            let bytes =
                serde_json::to_vec(&output).map_err(|_| failed("cannot encode todo result"))?;
            if bytes.len() > context.max_output_bytes {
                return Err(failed(
                    "todo list exceeds the tool output budget; shorten the list or task descriptions",
                ));
            }
            Ok(output)
        })
    }
}

/// Durable constraints exclude the current tool's parallelism policy, so tightening that
/// policy never makes an older valid parallel plan unreadable.
pub(crate) fn validate_stored(todos: &[TodoItem]) -> Result<()> {
    let mut seen = HashSet::new();
    for todo in todos {
        if todo.content.is_empty() || todo.content.trim() != todo.content {
            return Err(Error::new(
                ErrorKind::Session,
                "todo content must be non-empty and trimmed",
            ));
        }
        if !seen.insert(&todo.content) {
            return Err(Error::new(ErrorKind::Session, "duplicate todo content"));
        }
    }
    Ok(())
}

fn failed(message: &str) -> ToolError {
    ToolError::Failed(message.into())
}
