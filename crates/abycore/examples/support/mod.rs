use abycore::{Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput};
use serde_json::{Value, json};

pub struct Uppercase;
impl Tool for Uppercase {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "uppercase".into(),
            description: "Convert text to uppercase".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string","maxLength":1000}},"required":["text"],"additionalProperties":false}),
        }
    }
    fn validate(&self, args: &Value) -> Result<(), ToolError> {
        if args.as_object().is_none_or(|o| o.len() != 1)
            || args["text"]
                .as_str()
                .is_none_or(|s| s.chars().count() > 1000)
        {
            return Err(ToolError::Failed(
                "provide only text, up to 1000 characters".into(),
            ));
        }
        Ok(())
    }
    fn execute<'a>(&'a self, args: Value, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            Ok(ToolOutput::text(
                args["text"].as_str().unwrap().to_uppercase(),
            ))
        })
    }
}
