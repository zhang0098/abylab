//! Native Messages serialization and decoding, matching Harness's DeepSeek adapter.
use crate::{
    ContentPart, Error, ErrorKind, Item, MessageRole, ModelOptions, ReasoningEffort, Response,
    ResponseStatus, Result, ThinkingSignature, ToolDefinition, Usage, session::validate_history,
};
use serde_json::{Value, json};
use std::collections::HashSet;

pub(super) fn request(
    system: &str,
    history: &[Item],
    options: &ModelOptions,
    tools: &[ToolDefinition],
    stream: bool,
) -> Result<Value> {
    let body = request_body(system, history, options, tools, stream)?;
    if body["messages"]
        .as_array()
        .is_none_or(|messages| messages.is_empty())
    {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "Messages history must not be empty",
        ));
    }
    Ok(body)
}

/// Same serialization without the "history must not be empty" dispatch rule, so
/// a host can price a fresh session's envelope before its first prompt.
pub(super) fn measurement_body(
    system: &str,
    history: &[Item],
    options: &ModelOptions,
    tools: &[ToolDefinition],
) -> Result<Value> {
    request_body(system, history, options, tools, true)
}

fn request_body(
    system: &str,
    history: &[Item],
    options: &ModelOptions,
    tools: &[ToolDefinition],
    stream: bool,
) -> Result<Value> {
    options.validate()?;
    if !validate_history(history)?.is_empty() {
        return Err(Error::new(
            ErrorKind::Session,
            "every tool call needs an immediate result before the next request",
        ));
    }
    let mut names = HashSet::new();
    for tool in tools {
        tool.check()?;
        if !names.insert(&tool.name) {
            return Err(Error::new(ErrorKind::Configuration, "duplicate tool name"));
        }
    }
    let mut messages: Vec<Value> = vec![];
    for item in history {
        let (role, blocks) = match item {
            Item::Message { role, content, .. } => (
                if *role == MessageRole::User {
                    "user"
                } else {
                    "assistant"
                },
                content
                    .iter()
                    .filter(|p| *role == MessageRole::Assistant || matches!(p, ContentPart::InputImage { .. }) || !p.text().is_empty())
                    .map(|p| match p {
                        ContentPart::InputImage { media_type, data } => json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),
                        _ => json!({"type":"text","text":p.text()}),
                    })
                    .collect(),
            ),
            Item::Reasoning {
                content, signature, ..
            } => {
                let mut block = json!({"type":"thinking", "thinking":content.iter().map(ContentPart::text).collect::<String>()});
                if let Some(signature) = signature.as_ref().filter(|s| s.model == options.model) {
                    block["signature"] = json!(signature.value);
                }
                ("assistant", vec![block])
            }
            Item::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                let input: Value = serde_json::from_str(arguments).map_err(|_| {
                    Error::new(ErrorKind::Session, "historical tool input is invalid JSON")
                })?;
                if !input.is_object() {
                    return Err(Error::new(
                        ErrorKind::Session,
                        "historical tool input must be an object",
                    ));
                }
                (
                    "assistant",
                    vec![json!({"type":"tool_use","id":call_id,"name":name,"input":input})],
                )
            }
            Item::FunctionCallOutput {
                call_id,
                output,
                is_error,
                ..
            } => {
                let content: Vec<Value> = if output.is_empty() {
                    vec![]
                } else {
                    vec![json!({"type":"text","text":output})]
                };
                (
                    "user",
                    vec![
                        json!({"type":"tool_result","tool_use_id":call_id,"content":content,"is_error":is_error}),
                    ],
                )
            }
        };
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
            previous["content"]
                .as_array_mut()
                .expect("message content")
                .extend(blocks);
        } else {
            messages.push(json!({"role":role,"content":blocks}));
        }
    }
    // Native tool_result blocks precede other user content in the same turn.
    for message in &mut messages {
        if message["role"] == "user" {
            message["content"]
                .as_array_mut()
                .unwrap()
                .sort_by_key(|b| b["type"] != "tool_result");
        }
    }
    let mut body = json!({"model":options.model,"messages":messages,"max_tokens":options.max_tokens,
        "thinking":{"type":if options.reasoning == ReasoningEffort::Off { "disabled" } else { "enabled" }},"stream":stream});
    if options.reasoning != ReasoningEffort::Off {
        body["output_config"] = json!({"effort":options.reasoning});
    }
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.parameters}))
            .collect();
        body["tool_choice"] = json!({"type":options.tool_choice});
    }
    Ok(body)
}

pub(super) fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::protocol(format!("Messages field {key} must be a string")))
}

pub(super) fn identity(value: &Value) -> Result<(String, String)> {
    if value["type"] != "message" || value["role"] != "assistant" {
        return Err(Error::protocol("expected an assistant Messages response"));
    }
    let id = string(value, "id")?;
    let model = string(value, "model")?;
    if id.is_empty() || model.is_empty() {
        return Err(Error::protocol("empty message identity"));
    }
    Ok((id.into(), model.into()))
}

pub(super) fn block(value: &Value, id: &str, index: usize, model: &str) -> Result<Item> {
    let item_id = format!("{id}:{index}");
    let item = match string(value, "type")? {
        "text" => Item::Message {
            id: Some(item_id),
            role: MessageRole::Assistant,
            content: vec![ContentPart::OutputText {
                text: string(value, "text")?.into(),
            }],
        },
        "thinking" => Item::Reasoning {
            id: Some(item_id),
            content: vec![ContentPart::ReasoningText {
                text: string(value, "thinking")?.into(),
            }],
            signature: value
                .get("signature")
                .map(|_| {
                    string(value, "signature").map(|s| ThinkingSignature {
                        model: model.into(),
                        value: s.into(),
                    })
                })
                .transpose()?,
        },
        "tool_use" => {
            let input = value
                .get("input")
                .filter(|v| v.is_object())
                .ok_or_else(|| Error::protocol("tool_use input must be an object"))?;
            Item::FunctionCall {
                id: item_id,
                call_id: string(value, "id")?.into(),
                name: string(value, "name")?.into(),
                arguments: input.to_string(),
            }
        }
        _ => return Err(Error::protocol("unsupported Messages content block")),
    };
    item.validate()?;
    Ok(item)
}

pub(super) fn settle(response: &mut Response, reason: &str) -> Result<()> {
    response.status = match reason {
        "end_turn" | "stop_sequence" | "tool_use" => ResponseStatus::Completed,
        "max_tokens" => ResponseStatus::Incomplete,
        _ => return Err(Error::protocol("unsupported Messages stop reason")),
    };
    response.incomplete_reason =
        (response.status == ResponseStatus::Incomplete).then(|| reason.into());
    let mut calls = HashSet::new();
    for item in &response.output {
        item.validate()?;
        if let Item::FunctionCall {
            call_id, arguments, ..
        } = item
        {
            if !calls.insert(call_id) {
                return Err(Error::protocol("duplicate tool call id"));
            }
            if response.status == ResponseStatus::Completed
                && !serde_json::from_str::<Value>(arguments).is_ok_and(|v| v.is_object())
            {
                return Err(Error::protocol(
                    "completed tool input must be a JSON object",
                ));
            }
        }
    }
    if response.status == ResponseStatus::Completed {
        if response.output.is_empty() {
            return Err(Error::new(
                ErrorKind::EmptyResponse,
                "completed message has no content",
            ));
        }
        if (reason == "tool_use") != !calls.is_empty() {
            return Err(Error::protocol("stop reason disagrees with tool calls"));
        }
    }
    Ok(())
}

pub(super) fn decode(wire: Value, requested_model: &str) -> Result<Response> {
    if wire["type"] == "error" {
        return Err(provider_error(&wire));
    }
    let (id, model) = identity(&wire)?;
    let blocks = wire["content"]
        .as_array()
        .ok_or_else(|| Error::protocol("message has no content array"))?;
    let mut response = Response {
        output: blocks
            .iter()
            .enumerate()
            .map(|(i, b)| block(b, &id, i, requested_model))
            .collect::<Result<_>>()?,
        id,
        model,
        status: ResponseStatus::InProgress,
        usage: usage(&wire)?,
        incomplete_reason: None,
        error_code: None,
        error_message: None,
    };
    settle(&mut response, string(&wire, "stop_reason")?)?;
    Ok(response)
}

pub(super) fn usage(value: &Value) -> Result<Option<Usage>> {
    match value.get("usage") {
        None | Some(Value::Null) => Ok(None),
        Some(value) if value.is_object() => Ok(Some(Usage::from_wire(value))),
        _ => Err(Error::protocol("Messages usage must be an object")),
    }
}

pub(super) fn provider_error(value: &Value) -> Error {
    let kind = match value.pointer("/error/type").and_then(Value::as_str) {
        Some("authentication_error" | "permission_error") => ErrorKind::Authentication,
        Some("rate_limit_error") => ErrorKind::RateLimit,
        Some("invalid_request_error") => ErrorKind::InvalidRequest,
        Some("context_length_exceeded" | "context_limit_exceeded") => {
            ErrorKind::ContextLimitExceeded
        }
        Some("insufficient_quota" | "quota_exceeded") => ErrorKind::Quota,
        _ => ErrorKind::Server,
    };
    // Provider text can contain credentials or prompts; never echo it.
    Error::new(kind, "Messages API returned an in-band error")
}
