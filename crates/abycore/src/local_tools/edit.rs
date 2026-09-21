//! Literal replacement and structured, bounded host presentation.
use super::workspace::{Operation, ToolResult, failed};
use serde_json::{Value, json};

pub(super) struct Edit {
    pub content: String,
    pub replacements: usize,
}

/// CRLF and LF spellings describe the same text: every replacement is applied
/// to the normalized form, and a pair that differs only in line endings must
/// be rejected before that (it would report success without changing a byte).
pub(super) fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub(super) fn replacement(
    source: &str,
    old: &str,
    new: &str,
    all: bool,
    operation: &Operation,
) -> ToolResult<Edit> {
    operation.check()?;
    let normalized = normalize_newlines(source);
    let old = normalize_newlines(old);
    let new = normalize_newlines(new);
    if old.is_empty() {
        return Err(failed("FS_EDIT_NOT_FOUND: old_string must not be empty"));
    }
    let mut replacements = 0usize;
    let mut content = String::new();
    let mut start = 0;
    for (offset, _) in normalized.match_indices(&old) {
        operation.check()?;
        replacements += 1;
        if replacements > 1 && !all {
            return Err(failed(
                "FS_AMBIGUOUS_EDIT: old_string matched multiple times; provide more context or set replace_all to true",
            ));
        }
        content.push_str(&normalized[start..offset]);
        content.push_str(&new);
        start = offset + old.len();
    }
    if replacements == 0 {
        return Err(failed(
            "FS_EDIT_NOT_FOUND: old_string was not found; read the file and use exact text",
        ));
    }
    content.push_str(&normalized[start..]);
    let sample: String = source.chars().take(4096).collect();
    let crlf = sample.matches("\r\n").count();
    let lf = sample.matches('\n').count().saturating_sub(crlf);
    if crlf > lf {
        content = content.replace('\n', "\r\n");
    }
    operation.check()?;
    Ok(Edit {
        content,
        replacements,
    })
}

/// Contextual before/after data is separate from the model's confirmation text.
/// Very large files omit the optional basis instead of inflating host events.
pub(super) fn details(path: &str, before: Option<&str>, after: &str, cap: usize) -> Value {
    if after.len() >= cap || before.is_some_and(|text| text.len() >= cap) {
        return json!({"path":path,"diffs":[],"diffOmitted":true});
    }
    let after = after.replace("\r\n", "\n");
    let before = before.map(|text| text.replace("\r\n", "\n"));
    let mut diffs = vec![];
    if let Some(before) = &before {
        let old: Vec<_> = before.split_inclusive('\n').collect();
        let new: Vec<_> = after.split_inclusive('\n').collect();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix != old.len() || prefix != new.len() {
            let start = prefix.saturating_sub(3);
            diffs.push(json!({"path":path,"oldText":old[start..(old.len()-suffix+3).min(old.len())].concat(),
                "newText":new[start..(new.len()-suffix+3).min(new.len())].concat(),"startLine":start+1}));
        }
    } else {
        diffs.push(json!({"path":path,"oldText":null,"newText":after}));
    }
    json!({"path":path,"before":before,"after":after,"diffs":diffs})
}
