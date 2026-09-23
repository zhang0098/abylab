use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    InputText { text: String },
    InputImage { media_type: String, data: String },
    OutputText { text: String },
    ReasoningText { text: String },
}

impl ContentPart {
    pub fn text(&self) -> &str {
        match self {
            Self::InputText { text } | Self::OutputText { text } | Self::ReasoningText { text } => {
                text
            }
            Self::InputImage { .. } => "",
        }
    }
}

/// Provider-neutral transcript blocks. Item IDs are local; only call IDs go on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        role: MessageRole,
        content: Vec<ContentPart>,
    },
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<ThinkingSignature>,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
        /// Replayable tool metadata; excluded from provider requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
}

/// Opaque native thinking metadata, replayed only to the model that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinkingSignature {
    pub model: String,
    pub value: String,
}

impl Item {
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_parts(vec![ContentPart::InputText { text: text.into() }])
    }

    pub fn user_parts(content: Vec<ContentPart>) -> Self {
        Self::Message {
            id: None,
            role: MessageRole::User,
            content,
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Message { id, .. } | Self::Reasoning { id, .. } => id.as_deref(),
            Self::FunctionCall { id, .. } => Some(id),
            Self::FunctionCallOutput { .. } => None,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let valid = match self {
            Self::Message { role, content, .. } => !content.is_empty() && content.iter().all(|part| {
                matches!(
                    (role, part),
                    (MessageRole::User, ContentPart::InputText { .. })
                        | (MessageRole::Assistant, ContentPart::OutputText { .. })
                )
                || matches!((role, part),
                    (MessageRole::User, ContentPart::InputImage { media_type, data })
                        if matches!(media_type.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
                            && !data.is_empty()
                            && data.len() <= 44_739_244
                )
            }),
            Self::Reasoning { content, .. } => content
                .iter()
                .all(|p| matches!(p, ContentPart::ReasoningText { .. })),
            Self::FunctionCall {
                id, call_id, name, ..
            } => !id.is_empty() && !call_id.is_empty() && valid_tool_name(name),
            Self::FunctionCallOutput { call_id, .. } => !call_id.is_empty(),
        };
        if !valid {
            return Err(Error::protocol("invalid transcript item"));
        }
        Ok(())
    }
}

pub(crate) fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Total input, including cache reads. Unknown when it cannot be derived safely.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// Native Messages counters; input_tokens is their checked sum.
    pub uncached_input_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// False when the provider counters were missing or inconsistent.
    pub consistent: bool,
}

impl Usage {
    /// Messages counters are disjoint. Absent cache counters count as zero —
    /// the provider's `input_tokens` alone still prices the request (DeepSeek
    /// may omit them); unknown counters stay unknown.
    pub(crate) fn from_wire(value: &Value) -> Self {
        let uncached = value.get("input_tokens").and_then(Value::as_u64);
        let cached = value.get("cache_read_input_tokens").and_then(Value::as_u64);
        let created = value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64);
        let output = value.get("output_tokens").and_then(Value::as_u64);
        let input = uncached.and_then(|uncached| {
            uncached
                .checked_add(cached.unwrap_or(0))
                .and_then(|input| input.checked_add(created.unwrap_or(0)))
        });
        let sum = input.zip(output).and_then(|(i, o)| i.checked_add(o));
        let counters_valid = [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ]
        .iter()
        .all(|key| value.get(key).is_none_or(|v| v.as_u64().is_some()));
        let consistent = counters_valid && sum.is_some();
        Self {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: None,
            uncached_input_tokens: uncached,
            cache_creation_tokens: created,
            total_tokens: if consistent { sum } else { None },
            consistent,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub status: ResponseStatus,
    pub output: Vec<Item>,
    pub usage: Option<Usage>,
    pub incomplete_reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

impl Response {
    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|i| match i {
                Item::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                } => Some(content.iter().map(ContentPart::text).collect::<String>()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestPurpose {
    Conversation,
    WebSearch,
    /// A host-driven summarization call for context compaction.
    Compaction,
}

/// One model a provider currently advertises (`GET /models`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    /// The provider's owner label, when the listing carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// One HTTP dispatch. Missing usage remains unknown, including interrupted attempts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub purpose: RequestPurpose,
    pub attempt: u32,
    pub response_id: Option<String>,
    pub status: Option<String>,
    pub usage: Option<Usage>,
    /// Serialized bytes of the request body this dispatch sent. Recorded so a
    /// host can calibrate byte-based context estimates against the provider's
    /// own token count for the same envelope; absent on records written before
    /// this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_bytes: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_usage_never_invents_or_double_counts_tokens() {
        let valid = json!({"input_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":4});
        let usage = Usage::from_wire(&valid);
        assert!(usage.consistent);
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.total_tokens, Some(16));
        assert_eq!(usage.reasoning_tokens, None);
        // Absent cache counters count as zero (DeepSeek omits them).
        let without_cache = json!({"input_tokens":12,"output_tokens":5});
        let usage = Usage::from_wire(&without_cache);
        assert!(usage.consistent);
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.total_tokens, Some(17));
        for patch in [
            json!({"input_tokens":-1}),
            json!({"output_tokens":1.2}),
            json!({"cache_read_input_tokens":"3"}),
            json!({"cache_creation_input_tokens":null}),
            json!({"input_tokens":u64::MAX}),
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let usage = Usage::from_wire(&value);
            assert!(!usage.consistent);
            assert_eq!(usage.total_tokens, None);
        }
        for key in ["input_tokens", "output_tokens"] {
            let mut value = valid.clone();
            value.as_object_mut().unwrap().remove(key);
            assert_eq!(Usage::from_wire(&value).total_tokens, None);
        }
    }
}
