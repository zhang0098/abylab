use super::{
    LocalTools, edit, read,
    workspace::{
        Operation, ToolResult, Workspace, failed, inspect, parse, read_file, read_file_limited,
        replace, run_filesystem,
    },
};
use crate::{Result, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

#[derive(Clone)]
pub struct ReadTool {
    pub(super) workspace: Arc<Workspace>,
}
#[derive(Clone)]
pub struct WriteTool {
    pub(super) workspace: Arc<Workspace>,
}
#[derive(Clone)]
pub struct EditTool {
    pub(super) workspace: Arc<Workspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    file_path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl ReadTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.read())
    }
    fn input(&self, value: &Value) -> ToolResult<ReadInput> {
        let input: ReadInput = parse(value)?;
        self.workspace.path(&input.file_path)?;
        if ["offset", "limit"]
            .iter()
            .any(|key| value.get(key).is_some_and(Value::is_null))
            || input.offset == Some(0)
            || input
                .limit
                .is_some_and(|n| n == 0 || n > self.workspace.config.max_read_lines)
        {
            return Err(failed(
                "offset and limit must be positive integers; limit must not exceed max_read_lines",
            ));
        }
        Ok(input)
    }
}
impl WriteTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.write())
    }
    fn input(&self, value: &Value) -> ToolResult<WriteInput> {
        let input: WriteInput = parse(value)?;
        self.workspace.path(&input.file_path)?;
        self.workspace.check_size(input.content.len())?;
        Ok(input)
    }
}
impl EditTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.edit())
    }
    fn input(&self, value: &Value) -> ToolResult<EditInput> {
        let input: EditInput = parse(value)?;
        self.workspace.path(&input.file_path)?;
        if input.old_string.is_empty() || input.old_string == input.new_string {
            return Err(failed(
                "old_string must be nonempty and differ from new_string",
            ));
        }
        self.workspace.check_size(input.old_string.len())?;
        self.workspace.check_size(input.new_string.len())?;
        Ok(input)
    }
}

impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name:"read".into(), description:"Read a UTF-8 text file and return line-numbered content. offset is 1-based. Use the continuation offset for more lines. Read an existing file before write or edit; reading records its current version for this session.".into(),
            parameters:json!({"type":"object","properties":{"file_path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":self.workspace.config.max_read_lines}},"required":["file_path"],"additionalProperties":false}) }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        self.input(value).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            run_filesystem(
                self.workspace.clone(),
                context,
                input.file_path,
                false,
                move |workspace, path, operation| {
                    read::execute(
                        workspace,
                        path,
                        input.offset.unwrap_or(1),
                        input.limit.unwrap_or(workspace.config.max_read_lines),
                        operation,
                    )
                },
            )
            .await
        })
    }
}
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name:"write".into(), description:"Create or fully replace a UTF-8 text file. Parent directories are created automatically. Read an existing file before overwriting it, unless this session just wrote or edited it. Changes since the last read are rejected.".into(),
            parameters:json!({"type":"object","properties":{"file_path":{"type":"string","minLength":1},"content":{"type":"string"}},"required":["file_path","content"],"additionalProperties":false}) }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        self.input(value).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            run_filesystem(self.workspace.clone(), context, input.file_path, true, move |workspace, path, operation| {
                let previous = inspect(&workspace.directory, path)?;
                let key = workspace.absolute(path);
                operation.session.guard(&key, previous.as_ref(), false)?;
                let before = previous.as_ref().filter(|m| m.len() < workspace.config.max_diff_bytes as u64)
                    .and_then(|_| read_file_limited(workspace, path, operation, Some(workspace.config.max_diff_bytes)).ok());
                let committed = replace(workspace, path, &input.content, previous.as_ref(), operation)?;
                operation.session.observe(key.clone(), Some(committed));
                let verb = if previous.is_some() { "Updated" } else { "Created" };
                let mut output = confirmation(format!("<path>{}</path>\n<type>file</type>\n<content>\n{verb} file\n</content>",key.display()), operation);
                let mut details = edit::details(&key.to_string_lossy(), before.as_deref(), &input.content, workspace.config.max_diff_bytes);
                details["operation"] = json!(if previous.is_some() {"update"} else {"create"});
                output.details = Some(details);
                Ok(output)
            }).await
        })
    }
}
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name:"edit".into(), description:"Edit an existing UTF-8 text file by replacing literal old_string with new_string. Read it first unless just created or edited in this session. By default old_string must occur exactly once; set replace_all to replace all occurrences. Formatting must match exactly (CRLF/LF are normalized).".into(),
            parameters:json!({"type":"object","properties":{"file_path":{"type":"string","minLength":1},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["file_path","old_string","new_string"],"additionalProperties":false}) }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        self.input(value).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            run_filesystem(self.workspace.clone(), context, input.file_path, true, move |workspace, path, operation| {
                let previous = inspect(&workspace.directory, path)?;
                let key = workspace.absolute(path);
                operation.session.guard(&key, previous.as_ref(), true)?;
                let source = read_file(workspace, path, operation)?;
                let edited = edit::replacement(&source, &input.old_string, &input.new_string, input.replace_all, operation)?;
                let committed = replace(workspace, path, &edited.content, previous.as_ref(), operation)?;
                operation.session.observe(key.clone(), Some(committed));
                let message = if input.replace_all { format!("The file {} has been updated. All occurrences were successfully replaced.", key.display()) }
                    else { format!("The file {} has been updated successfully.", key.display()) };
                let mut output = confirmation(message, operation);
                let mut details = edit::details(&key.to_string_lossy(), Some(&source), &edited.content, workspace.config.max_diff_bytes);
                details["replacements"] = json!(edited.replacements);
                output.details = Some(details);
                Ok(output)
            }).await
        })
    }
}
fn confirmation(message: String, operation: &Operation) -> ToolOutput {
    ToolOutput::text(message).bounded(operation.output_limit)
}
