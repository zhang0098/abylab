use super::{messages, sse::Frame};
use crate::{
    ContentPart, Error, Item, Response, ResponseStatus, Result, StreamEvent, ThinkingSignature,
    Usage,
};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(super) struct Assembler {
    requested_model: String,
    response: Option<Response>,
    blocks: Vec<Draft>,
    indexes: HashMap<usize, usize>,
    usage: Value,
    reason: Option<String>,
    sequence: u64,
    finished: bool,
}

struct Draft {
    item: Item,
    closed: bool,
    arguments: Option<String>,
}

impl Assembler {
    pub fn new(model: String) -> Self {
        Self {
            requested_model: model,
            response: None,
            blocks: vec![],
            indexes: HashMap::new(),
            usage: json!({}),
            reason: None,
            sequence: 0,
            finished: false,
        }
    }

    pub fn accept(&mut self, frame: Frame) -> Result<Option<StreamEvent>> {
        if self.finished {
            return Err(Error::protocol("event after message_stop"));
        }
        let value: Value = serde_json::from_str(&frame.data)
            .map_err(|_| Error::protocol("invalid Messages SSE JSON"))?;
        let kind = messages::string(&value, "type")?;
        if frame.event.as_deref().is_some_and(|event| event != kind) {
            return Err(Error::protocol("SSE event name disagrees with data type"));
        }
        let sequence = self.sequence;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| Error::protocol("event counter overflow"))?;
        if kind == "error" {
            return Err(messages::provider_error(&value));
        }
        if kind == "message_start" {
            if self.response.is_some() {
                return Err(Error::protocol("duplicate message_start"));
            }
            let message = &value["message"];
            let (id, model) = messages::identity(message)?;
            if !message["content"].as_array().is_some_and(Vec::is_empty)
                || !message["stop_reason"].is_null()
            {
                return Err(Error::protocol(
                    "message_start must have empty, unfinished content",
                ));
            }
            self.update_usage(message)?;
            self.response = Some(Response {
                id: id.clone(),
                model,
                status: ResponseStatus::InProgress,
                output: vec![],
                usage: None,
                incomplete_reason: None,
                error_code: None,
                error_message: None,
            });
            return Ok(Some(StreamEvent::Started {
                response_id: id,
                sequence,
            }));
        }
        if !matches!(
            kind,
            "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        ) {
            // Messages allows new non-content event types (including ping).
            if kind.starts_with("content_block_")
                || kind.starts_with("message_")
                || kind.starts_with("response.")
            {
                return Err(Error::protocol(
                    "unsupported content-bearing Messages event",
                ));
            }
            return Ok(None);
        }
        if self.response.is_none() {
            return Err(Error::protocol("event precedes message_start"));
        }
        match kind {
            "content_block_start" => {
                let wire_index = position(&value)?;
                if self.indexes.contains_key(&wire_index) || self.reason.is_some() {
                    return Err(Error::protocol(
                        "duplicate block or block after message settlement",
                    ));
                }
                let index = self.blocks.len();
                let item = messages::block(
                    &value["content_block"],
                    &self.response.as_ref().unwrap().id,
                    index,
                    &self.requested_model,
                )?;
                if let Item::FunctionCall { call_id, .. } = &item
                    && self.blocks.iter().any(|b| matches!(&b.item, Item::FunctionCall{call_id:other,..} if other == call_id)) {
                    return Err(Error::protocol("duplicate tool call id"));
                }
                // Text carried in a start event is just as visible as a delta.
                let event = match &item {
                    Item::Message { id, content, .. } if !content[0].text().is_empty() => {
                        Some(StreamEvent::TextDelta {
                            output_index: index,
                            content_index: 0,
                            item_id: id.clone().unwrap(),
                            delta: content[0].text().into(),
                            sequence,
                        })
                    }
                    Item::Reasoning { id, content, .. } if !content[0].text().is_empty() => {
                        Some(StreamEvent::ReasoningDelta {
                            output_index: index,
                            content_index: 0,
                            item_id: id.clone().unwrap(),
                            delta: content[0].text().into(),
                            sequence,
                        })
                    }
                    _ => None,
                };
                self.indexes.insert(wire_index, index);
                self.blocks.push(Draft {
                    item,
                    closed: false,
                    arguments: None,
                });
                Ok(event)
            }
            "content_block_delta" | "content_block_stop" => {
                if self.reason.is_some() {
                    return Err(Error::protocol("block event after message settlement"));
                }
                let index = *self
                    .indexes
                    .get(&position(&value)?)
                    .ok_or_else(|| Error::protocol("unknown content block"))?;
                let draft = &mut self.blocks[index];
                if draft.closed {
                    return Err(Error::protocol("delta/stop on a closed content block"));
                }
                if kind == "content_block_stop" {
                    draft.closed = true;
                    if let (Item::FunctionCall { arguments, .. }, Some(delta)) =
                        (&mut draft.item, draft.arguments.take())
                    {
                        *arguments = delta;
                    }
                    return Ok(Some(StreamEvent::ItemDone {
                        output_index: index,
                        item: draft.item.clone(),
                        sequence,
                    }));
                }
                let delta = &value["delta"];
                let item_id = draft.item.id().unwrap().to_owned();
                let event = match (messages::string(delta, "type")?, &mut draft.item) {
                    ("text_delta", Item::Message { content, .. }) => {
                        let text = messages::string(delta, "text")?;
                        let ContentPart::OutputText { text: current } = &mut content[0] else {
                            unreachable!()
                        };
                        current.push_str(text);
                        Some(StreamEvent::TextDelta {
                            output_index: index,
                            content_index: 0,
                            item_id,
                            delta: text.into(),
                            sequence,
                        })
                    }
                    ("thinking_delta", Item::Reasoning { content, .. }) => {
                        let text = messages::string(delta, "thinking")?;
                        let ContentPart::ReasoningText { text: current } = &mut content[0] else {
                            unreachable!()
                        };
                        current.push_str(text);
                        Some(StreamEvent::ReasoningDelta {
                            output_index: index,
                            content_index: 0,
                            item_id,
                            delta: text.into(),
                            sequence,
                        })
                    }
                    ("signature_delta", Item::Reasoning { signature, .. }) => {
                        let text = messages::string(delta, "signature")?;
                        signature
                            .get_or_insert_with(|| ThinkingSignature {
                                model: self.requested_model.clone(),
                                value: String::new(),
                            })
                            .value
                            .push_str(text);
                        None
                    }
                    ("input_json_delta", Item::FunctionCall { call_id, name, .. }) => {
                        let text = messages::string(delta, "partial_json")?;
                        if !text.is_empty() {
                            draft.arguments.get_or_insert_default().push_str(text);
                        }
                        Some(StreamEvent::ToolArgumentsDelta {
                            output_index: index,
                            item_id,
                            call_id: call_id.clone(),
                            name: name.clone(),
                            delta: text.into(),
                            sequence,
                        })
                    }
                    _ => return Err(Error::protocol("delta type does not match content block")),
                };
                Ok(event)
            }
            "message_delta" => {
                if !value["delta"].is_object() {
                    return Err(Error::protocol("invalid message_delta"));
                }
                if let Some(reason) = value["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                    let reason = reason
                        .as_str()
                        .ok_or_else(|| Error::protocol("invalid stop reason"))?;
                    if self
                        .reason
                        .as_deref()
                        .is_some_and(|previous| previous != reason)
                        || self.blocks.iter().any(|b| !b.closed)
                    {
                        return Err(Error::protocol(
                            "message settles before blocks close or changes stop reason",
                        ));
                    }
                    self.reason = Some(reason.into());
                }
                self.update_usage(&value)?;
                Ok(None)
            }
            "message_stop" => {
                if self.blocks.iter().any(|b| !b.closed) {
                    return Err(Error::protocol("message_stop with open blocks"));
                }
                let reason = self
                    .reason
                    .as_deref()
                    .ok_or_else(|| Error::protocol("message_stop without a stop reason"))?;
                let mut response = self.response.take().unwrap();
                response.output = self.blocks.iter().map(|b| b.item.clone()).collect();
                response.usage = (!self.usage.as_object().unwrap().is_empty())
                    .then(|| Usage::from_wire(&self.usage));
                messages::settle(&mut response, reason)?;
                self.finished = true;
                Ok(Some(StreamEvent::Finished {
                    response: Box::new(response),
                    sequence,
                }))
            }
            _ => unreachable!(),
        }
    }

    fn update_usage(&mut self, value: &Value) -> Result<()> {
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            let usage = usage
                .as_object()
                .ok_or_else(|| Error::protocol("Messages usage must be an object"))?;
            for key in [
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ] {
                if usage.get(key).is_some_and(|v| v.as_u64().is_none()) {
                    return Err(Error::protocol("invalid Messages token counter"));
                }
            }
            // Cumulative fields replace previous values; absent fields retain starts.
            self.usage.as_object_mut().unwrap().extend(usage.clone());
        }
        Ok(())
    }
}

fn position(value: &Value) -> Result<usize> {
    value["index"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| Error::protocol("invalid content block index"))
}
