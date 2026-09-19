//! Host-driven context compaction.
//!
//! deepseek-harness keeps a durable session log and derives the model-visible
//! *surface* from it: compaction replaces an older span of that surface with a
//! summary while every original event stays in the log. abycore has no separate
//! surface, so this module adds the smallest equivalent — a list of
//! [`Compaction`] records (plus an optional [`Prune`]) on the snapshot and a
//! request view that renders the transcript with those spans replaced.
//!
//! Policy stays with the host (harness keeps it in optional packages too): when
//! to compact, how much recent history to retain verbatim, and which model
//! writes the summary. The host states its policy at every request boundary
//! through [`crate::AgentHooks::view_request`], and the SDK guarantees the
//! mechanics: a span may only be replaced when it is *balanced* (no tool call
//! separated from its result), the summary reaches the model exactly where the
//! span used to be, and the durable transcript is never rewritten.

use crate::{CancellationToken, Error, ErrorKind, Item, RequestRecord, Result, Usage};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Tags wrapping the summary inside the model-visible replacement message.
pub const SUMMARY_OPEN_TAG: &str = "<compacted-summary>";
pub const SUMMARY_CLOSE_TAG: &str = "</compacted-summary>";

/// Framing that makes the replacement read as established background, mirroring
/// the harness checkpoint preamble.
pub const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

/// The default summarization directive, delivered as the final user message
/// after the replayed span rather than as a separate summarizer system prompt —
/// keeping the conversation's own system prompt, tools and prefix in front of it
/// makes the auxiliary call a genuine prefix of the last request, so the
/// provider's cache is reused instead of invalidated. Mirrors
/// `dsh-compaction-basic`'s instruction.
pub const SUMMARIZE_INSTRUCTION: &str = "\
You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write \"(none)\" for an empty section — never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or \"(none)\"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.";

/// One host-recorded replacement of a balanced transcript span with a summary.
///
/// `start`/`end` index the durable [`crate::SessionSnapshot::items`]; the span
/// itself is never removed. Records are ordered, non-overlapping and must both
/// start and end on balanced cuts (see [`balanced_cuts`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compaction {
    /// First shadowed transcript index (inclusive).
    pub start: usize,
    /// First transcript index after the shadowed span.
    pub end: usize,
    /// Host-provided summary text, wrapped in [`SUMMARY_OPEN_TAG`] on the wire.
    pub summary: String,
}

/// Host-recorded trimming of older tool outputs in the model-visible view.
///
/// Harness ships this as `compaction-tool-result-pruner`: trimmed history is
/// cheaper to summarize, and sometimes cheap enough that no summary call is
/// needed at all. The durable transcript keeps every byte; only the view is
/// trimmed, and only for outputs older than `through`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prune {
    /// Transcript items *before* this index get their tool outputs trimmed.
    pub through: usize,
    /// Bytes kept from the start of each trimmed output.
    pub max_bytes: usize,
}

/// The smallest useful trim: below this a tool result loses its meaning.
pub const MIN_PRUNE_BYTES: usize = 64;

/// One balanced cut of the transcript, priced for retention decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewCut {
    /// Cut position: the transcript index the cut falls before.
    pub index: usize,
    /// Serialized bytes of the history from this cut onward.
    pub suffix_bytes: usize,
}

/// What a host needs at a request boundary to decide whether to reshape the
/// view: the live measurement plus every safe cut, priced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewEstimate {
    /// Serialized bytes of the request as it would be sent right now.
    pub request_bytes: usize,
    /// Serialized bytes of the model-visible history alone.
    pub history_bytes: usize,
    /// Transcript items (including shadowed ones).
    pub items: usize,
    /// Compactions already applied.
    pub compactions: usize,
    /// Transcript index after the newest compaction. A new span may also
    /// encompass entire existing records to consolidate their summaries.
    pub compacted_through: usize,
    /// Whether tool outputs are already being trimmed.
    pub pruned: bool,
    /// Current trimming boundary, so a policy can advance it as history grows.
    pub pruned_through: usize,
    /// Input tokens the provider reported for the newest measured conversation
    /// request in the ledger.
    pub last_input_tokens: Option<u64>,
    /// Serialized bytes of that same request, so the provider's count can be
    /// carried forward to the current envelope.
    pub last_request_bytes: Option<usize>,
    /// Balanced cuts, ascending: positions where the transcript can be split
    /// without separating a tool call from its result.
    pub cuts: Vec<ViewCut>,
}

impl ViewEstimate {
    /// Byte-based fallback estimate: one token per three bytes, not a bound.
    pub fn conservative_tokens(&self) -> u64 {
        (self.request_bytes as u64).div_ceil(3)
    }

    /// Provider-calibrated tokens: the measured count plus the bytes appended
    /// since, priced at the measured envelope's own bytes-per-token ratio.
    ///
    /// `None` when nothing was measured, when the current view is *smaller*
    /// than the measured envelope (a compaction or prune happened, so the
    /// measurement no longer covers this request), or when the implied ratio is
    /// implausible (outside one to eight bytes per token — CJK sits near three,
    /// English near four, and anything beyond that is a bogus counter rather
    /// than a contraction).
    pub fn calibrated_tokens(&self) -> Option<u64> {
        let measured_bytes = self.last_request_bytes?;
        let measured_tokens = self.last_input_tokens?;
        if measured_bytes == 0 || measured_tokens == 0 || self.request_bytes < measured_bytes {
            return None;
        }
        let bytes_per_token = measured_bytes as f64 / measured_tokens as f64;
        if !(1.0..=8.0).contains(&bytes_per_token) {
            return None;
        }
        let added = (self.request_bytes - measured_bytes) as f64;
        Some(measured_tokens + (added / bytes_per_token + 0.5) as u64)
    }

    /// The best current estimate: trust a valid provider measurement, even
    /// when it exceeds the byte-based fallback.
    pub fn tokens(&self) -> u64 {
        match self.calibrated_tokens() {
            Some(calibrated) => calibrated,
            None => self.conservative_tokens(),
        }
    }

    /// Bytes per token implied by the newest measurement, clamped to the
    /// plausible range and defaulting to the conservative three.
    pub fn bytes_per_token(&self) -> usize {
        let Some(measured_bytes) = self.last_request_bytes else {
            return 3;
        };
        let Some(measured_tokens) = self.last_input_tokens.filter(|tokens| *tokens > 0) else {
            return 3;
        };
        let ratio = measured_bytes as f64 / measured_tokens as f64;
        if (1.0..=8.0).contains(&ratio) {
            ratio.round() as usize
        } else {
            3
        }
    }
}

/// A host-requested change to the model-visible view, applied by the SDK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewRequest {
    /// Trim tool outputs before `through` to `max_bytes` each; no model call.
    Prune { through: usize, max_bytes: usize },
    /// Replace `[start, end)` with a summary produced by
    /// [`crate::Agent::summarize_span`].
    Condense {
        start: usize,
        end: usize,
        /// Replace the default [`SUMMARIZE_INSTRUCTION`].
        instruction: Option<String>,
        /// Output cap for this summary; `None` keeps the session model's cap.
        max_tokens: Option<u32>,
    },
}

/// What actually happened to the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewChange {
    Pruned {
        through: usize,
        max_bytes: usize,
        before_bytes: usize,
        after_bytes: usize,
    },
    Compacted {
        start: usize,
        end: usize,
        before_bytes: usize,
        after_bytes: usize,
    },
    /// The request was refused (bad span, over-long summary, failed summary
    /// call). The request still goes out with the unchanged view.
    Failed { message: String },
}

/// Byte-level context measurement for hosts deciding whether to compact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextEstimate {
    /// Serialized bytes of the next request exactly as the SDK would send it
    /// (system prompt, tool schemas, options and the compacted view).
    pub request_bytes: usize,
    /// Serialized bytes of the model-visible history alone.
    pub history_bytes: usize,
    /// Input tokens the provider reported for the newest conversation request in
    /// the ledger. It measures *that* envelope, not the current one, so it is
    /// reported for calibration rather than used as the current pressure.
    pub last_input_tokens: Option<u64>,
    /// Compactions currently applied to the request view.
    pub compactions: usize,
    /// Serialized bytes of the measured request that produced
    /// [`Self::last_input_tokens`].
    pub last_request_bytes: Option<usize>,
}

impl ContextEstimate {
    /// Byte-based fallback estimate: one token per three bytes. This often
    /// overestimates prose but is not an upper bound; valid provider counts
    /// take precedence in [`Self::tokens`].
    pub fn estimated_tokens(&self) -> u64 {
        (self.request_bytes as u64).div_ceil(3)
    }

    /// See [`ViewEstimate::calibrated_tokens`].
    pub fn calibrated_tokens(&self) -> Option<u64> {
        self.view_estimate().calibrated_tokens()
    }

    /// See [`ViewEstimate::tokens`]: calibrated when available.
    pub fn tokens(&self) -> u64 {
        self.calibrated_tokens()
            .unwrap_or_else(|| self.estimated_tokens())
    }

    /// See [`ViewEstimate::bytes_per_token`].
    pub fn bytes_per_token(&self) -> usize {
        self.view_estimate().bytes_per_token()
    }

    fn view_estimate(&self) -> ViewEstimate {
        ViewEstimate {
            request_bytes: self.request_bytes,
            history_bytes: self.history_bytes,
            items: 0,
            compactions: self.compactions,
            compacted_through: 0,
            pruned: false,
            pruned_through: 0,
            last_input_tokens: self.last_input_tokens,
            last_request_bytes: self.last_request_bytes,
            cuts: vec![],
        }
    }
}

/// One summarization call over an explicit balanced span.
#[derive(Clone, Debug)]
pub struct SummarizeOptions {
    pub cancellation: CancellationToken,
    pub timeout: Duration,
    /// Output cap; `None` keeps the session model's configured cap.
    pub max_tokens: Option<u32>,
    /// Replace the default [`SUMMARIZE_INSTRUCTION`].
    pub instruction: Option<String>,
}

impl Default for SummarizeOptions {
    fn default() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            timeout: Duration::from_secs(300),
            max_tokens: None,
            instruction: None,
        }
    }
}

/// The text a summarization call produced, with its accounting.
#[derive(Clone, Debug)]
pub struct SummarizeOutcome {
    /// Assistant text only: reasoning and tool calls are dropped.
    pub summary: String,
    pub usage: Option<Usage>,
    /// Every HTTP dispatch this call made, including retries.
    pub requests: Vec<RequestRecord>,
}

/// Which cut positions of `items` leave no tool call separated from its result.
///
/// A transcript of N items has N+1 cuts; entry `i` is the cut *before* item `i`,
/// and the final entry the cut after the last item.
pub(crate) fn balanced_cuts(items: &[Item]) -> Vec<bool> {
    let mut open = 0usize;
    let mut cuts = Vec::with_capacity(items.len() + 1);
    cuts.push(true);
    for item in items {
        match item {
            Item::FunctionCall { .. } => open += 1,
            Item::FunctionCallOutput { .. } => open = open.saturating_sub(1),
            _ => {}
        }
        cuts.push(open == 0);
    }
    cuts
}

/// Validate records against the transcript they shadow.
pub(crate) fn validate_compactions(items: &[Item], compactions: &[Compaction]) -> Result<()> {
    if compactions.is_empty() {
        return Ok(());
    }
    let cuts = balanced_cuts(items);
    let mut previous_end = 0usize;
    for compaction in compactions {
        if compaction.summary.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::Session,
                "compaction summary must not be empty",
            ));
        }
        if compaction.start >= compaction.end || compaction.end > items.len() {
            return Err(Error::new(
                ErrorKind::Session,
                "compaction span must be a non-empty range inside the transcript",
            ));
        }
        if compaction.start < previous_end {
            return Err(Error::new(
                ErrorKind::Session,
                "compaction spans must be ordered and non-overlapping",
            ));
        }
        if !cuts[compaction.start] || !cuts[compaction.end] {
            return Err(Error::new(
                ErrorKind::Session,
                "compaction span must not separate a tool call from its result",
            ));
        }
        previous_end = compaction.end;
    }
    Ok(())
}

/// Validate a prune record against the transcript it trims.
pub(crate) fn validate_prune(items: &[Item], prune: Option<&Prune>) -> Result<()> {
    let Some(prune) = prune else {
        return Ok(());
    };
    if prune.through > items.len() {
        return Err(Error::new(
            ErrorKind::Session,
            "prune boundary must lie inside the transcript",
        ));
    }
    if prune.max_bytes < MIN_PRUNE_BYTES {
        return Err(Error::new(
            ErrorKind::Session,
            "pruned tool outputs must keep at least 64 bytes",
        ));
    }
    Ok(())
}

/// The model-visible transcript: compacted spans replaced by their summaries,
/// and old tool outputs trimmed to [`Prune::max_bytes`].
pub(crate) fn request_view(
    items: &[Item],
    compactions: &[Compaction],
    prune: Option<&Prune>,
) -> Vec<Item> {
    indexed_request_view(items, compactions, prune)
        .into_iter()
        .map(|(_, item)| item)
        .collect()
}

/// View items paired with their durable start index. A summary occupies its
/// entire recorded span; no visible cut may fall inside that span.
pub(crate) fn indexed_request_view(
    items: &[Item],
    compactions: &[Compaction],
    prune: Option<&Prune>,
) -> Vec<(usize, Item)> {
    let mut view = Vec::with_capacity(items.len() + compactions.len());
    let mut cursor = 0usize;
    let push = |item: &Item, index: usize, view: &mut Vec<(usize, Item)>| {
        view.push((
            index,
            match (prune, item) {
                (Some(prune), Item::FunctionCallOutput { .. }) if index < prune.through => {
                    prune_output(item, prune.max_bytes)
                }
                _ => item.clone(),
            },
        ));
    };
    for compaction in compactions {
        for (offset, item) in items[cursor..compaction.start].iter().enumerate() {
            push(item, cursor + offset, &mut view);
        }
        view.push((compaction.start, summary_item(&compaction.summary)));
        cursor = compaction.end;
    }
    for (offset, item) in items[cursor..].iter().enumerate() {
        push(item, cursor + offset, &mut view);
    }
    view
}

/// Trim one tool result, keeping its head and naming what was dropped.
fn prune_output(item: &Item, max_bytes: usize) -> Item {
    let Item::FunctionCallOutput {
        call_id,
        output,
        is_error,
        meta,
    } = item
    else {
        return item.clone();
    };
    if output.len() <= max_bytes {
        return item.clone();
    }
    let mut end = max_bytes;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    Item::FunctionCallOutput {
        call_id: call_id.clone(),
        output: format!(
            "{}\n[tool output pruned for context: kept {end} of {} bytes]",
            &output[..end],
            output.len()
        ),
        is_error: *is_error,
        meta: meta.clone(),
    }
}

/// The replacement message a summary lands in.
pub(crate) fn summary_item(summary: &str) -> Item {
    Item::user(format!(
        "{CHECKPOINT_PREAMBLE}\n\n{SUMMARY_OPEN_TAG}\n{summary}\n{SUMMARY_CLOSE_TAG}"
    ))
}
