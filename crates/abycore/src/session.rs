use crate::{Error, ErrorKind, Item, ModelOptions, RequestRecord, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const FORMAT_ERROR: &str = "unsupported session version or protocol; expected version 2 / deepseek-messages (legacy Responses sessions are not migrated automatically)";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingState {
    Ready,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub state: PendingState,
}

/// Portable data only: no credentials, clients, executable tools or callbacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub version: u32,
    pub protocol: String,
    pub system_prompt: String,
    pub model: ModelOptions,
    pub items: Vec<Item>,
    pub pending: Vec<PendingCall>,
    /// A user message or resolved tool batch still needs a model response.
    pub needs_response: bool,
    pub run_sequence: u64,
    pub requests: Vec<RequestRecord>,
    /// Current turn's canonical task list. None before its first write; [] explicitly clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todos: Option<Vec<crate::TodoItem>>,
    /// Host-recorded replacements for the model-visible view. The transcript
    /// above is never rewritten; see [`crate::Compaction`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compactions: Vec<crate::Compaction>,
    /// Host-recorded trimming of older tool outputs in the model-visible view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<crate::Prune>,
    /// The session's single completion objective.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<crate::Goal>,
}

impl SessionSnapshot {
    pub fn new(system_prompt: impl Into<String>, model: ModelOptions) -> Self {
        Self {
            version: 2,
            protocol: "deepseek-messages".into(),
            system_prompt: system_prompt.into(),
            model,
            items: vec![],
            pending: vec![],
            needs_response: false,
            run_sequence: 0,
            requests: vec![],
            todos: None,
            compactions: vec![],
            prune: None,
            goal: None,
        }
    }
    /// Progress projection for a host checklist. Available without registering executable tools.
    pub fn plan_view(&self) -> Option<crate::PlanView> {
        self.todos.clone().map(crate::PlanView::new)
    }
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        serde_json::to_string_pretty(self)
            .map_err(|_| Error::new(ErrorKind::Session, "cannot serialize session"))
    }
    pub fn from_json(json: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|_| Error::new(ErrorKind::Session, "invalid session JSON"))?;
        // Diagnose the protocol before parsing fields whose schema changed.
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2)
            || value.get("protocol").and_then(serde_json::Value::as_str)
                != Some("deepseek-messages")
        {
            return Err(Error::new(ErrorKind::Session, FORMAT_ERROR));
        }
        let snapshot: Self = serde_json::from_value(value)
            .map_err(|_| Error::new(ErrorKind::Session, "invalid session JSON"))?;
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 2 || self.protocol != "deepseek-messages" {
            return Err(Error::new(ErrorKind::Session, FORMAT_ERROR));
        }
        self.model.validate()?;
        if let Some(todos) = &self.todos {
            crate::todo::validate_stored(todos)?;
        }
        crate::compaction::validate_compactions(&self.items, &self.compactions)?;
        crate::compaction::validate_prune(&self.items, self.prune.as_ref())?;
        crate::goal::validate_goal(self.goal.as_ref())?;
        let unmatched = validate_history(&self.items)?;
        let mut pending = HashSet::new();
        for call in &self.pending {
            if !pending.insert(&call.call_id)
                || unmatched
                    .get(&call.call_id)
                    .is_none_or(|(name, args)| *name != &call.name || *args != &call.arguments)
            {
                return Err(Error::new(
                    ErrorKind::Session,
                    "pending calls do not match transcript",
                ));
            }
        }
        if unmatched.len() != pending.len() || (!self.needs_response && !self.pending.is_empty()) {
            return Err(Error::new(
                ErrorKind::Session,
                "unresolved transcript calls",
            ));
        }
        let ordered = self.items.iter().filter_map(|item| match item {
            Item::FunctionCall { call_id, .. } if unmatched.contains_key(call_id) => Some(call_id),
            _ => None,
        });
        if !ordered.eq(self.pending.iter().map(|call| &call.call_id)) {
            return Err(Error::new(
                ErrorKind::Session,
                "pending call order differs from transcript",
            ));
        }
        let waiting_input = matches!(
            self.items.last(),
            Some(
                Item::Message {
                    role: crate::MessageRole::User,
                    ..
                } | Item::FunctionCallOutput { .. }
            )
        );
        if self.pending.is_empty() && self.needs_response != waiting_input {
            return Err(Error::new(
                ErrorKind::Session,
                "turn state does not match transcript boundary",
            ));
        }
        Ok(())
    }
}

/// Returns the unfinished final batch; no new message may follow an unfinished batch.
pub(crate) fn validate_history(items: &[Item]) -> Result<HashMap<&String, (&String, &String)>> {
    let mut ids = HashSet::new();
    let mut calls = HashSet::new();
    let mut pending = HashMap::new();
    let mut outputs_started = false;
    for item in items {
        item.validate()?;
        if let Some(id) = item.id()
            && (id.is_empty() || !ids.insert(id))
        {
            return Err(Error::new(
                ErrorKind::Session,
                "duplicate or empty transcript item id",
            ));
        }
        match item {
            Item::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                if outputs_started || !calls.insert(call_id) {
                    return Err(Error::new(
                        ErrorKind::Session,
                        "invalid function call ordering or duplicate call id",
                    ));
                }
                pending.insert(call_id, (name, arguments));
            }
            Item::FunctionCallOutput { call_id, .. } => {
                if pending.remove(call_id).is_none() {
                    return Err(Error::new(
                        ErrorKind::Session,
                        "orphan or duplicate tool output",
                    ));
                }
                outputs_started = !pending.is_empty();
            }
            _ => {
                // A response can interleave reasoning/messages and calls before its tool outputs.
                if outputs_started
                    || (!pending.is_empty()
                        && matches!(
                            item,
                            Item::Message {
                                role: crate::MessageRole::User,
                                ..
                            }
                        ))
                {
                    return Err(Error::new(
                        ErrorKind::Session,
                        "message interrupts an unfinished tool batch",
                    ));
                }
            }
        }
    }
    Ok(pending)
}
