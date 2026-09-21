//! Bounded line windows scanned without buffering an entire file or giant line.
use super::workspace::{
    Operation, ToolResult, Workspace, failed, inspect, io_error, open_file, verify, version,
};
use crate::ToolOutput;
use serde::Serialize;
use serde_json::json;
use std::{io::Read, path::Path};

#[derive(Serialize)]
struct Line {
    number: usize,
    text: String,
}

pub(super) fn execute(
    workspace: &Workspace,
    path: &Path,
    offset: usize,
    limit: usize,
    operation: &Operation,
) -> ToolResult<ToolOutput> {
    let key = workspace.absolute(path);
    if inspect(&workspace.directory, path)?.is_none() {
        operation.session.observe(key, None);
        return Err(failed("FS_NOT_FOUND: file does not exist"));
    }
    let (mut file, metadata) = open_file(workspace, path)?;
    let mut window = Window {
        offset,
        limit,
        max_chars: workspace.config.max_read_line_chars,
        max_bytes: workspace.config.max_read_bytes,
        lines: vec![],
        total: 0,
        bytes: 0,
        capped: false,
        line: String::new(),
        chars: 0,
        first: true,
        any_truncated: false,
    };
    let mut buffer = [0; 8192];
    let mut pending = Vec::with_capacity(8196);
    let mut bytes = 0usize;
    loop {
        operation.check()?;
        let count = file
            .read(&mut buffer)
            .map_err(|e| io_error("read file", e))?;
        if count == 0 {
            break;
        }
        bytes = bytes.saturating_add(count);
        workspace.check_size(bytes)?;
        pending.extend_from_slice(&buffer[..count]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(text) => {
                window.push(text)?;
                pending.len()
            }
            Err(error) if error.error_len().is_none() => {
                let length = error.valid_up_to();
                window
                    .push(std::str::from_utf8(&pending[..length]).expect("valid UTF-8 prefix"))?;
                length
            }
            Err(_) => return Err(failed("FS_NOT_TEXT: invalid UTF-8 text")),
        };
        pending.drain(..valid);
    }
    if !pending.is_empty() {
        return Err(failed("FS_NOT_TEXT: incomplete UTF-8 text"));
    }
    if window.chars > 0 {
        window.flush();
    }
    if offset > window.total && !(window.total == 0 && offset == 1) {
        return Err(failed(format!(
            "offset {offset} is out of range ({} lines)",
            window.total
        )));
    }
    verify(&workspace.directory, path, Some(&metadata))?;
    operation.check()?;
    operation
        .session
        .observe(key.clone(), Some(version(&metadata)));
    Ok(window.render(&key.to_string_lossy(), operation.output_limit))
}

struct Window {
    offset: usize,
    limit: usize,
    max_chars: usize,
    max_bytes: usize,
    lines: Vec<Line>,
    total: usize,
    bytes: usize,
    capped: bool,
    line: String,
    chars: usize,
    first: bool,
    any_truncated: bool,
}
impl Window {
    fn push(&mut self, text: &str) -> ToolResult<()> {
        for ch in text.chars() {
            if self.first {
                self.first = false;
                if ch == '\u{feff}' {
                    continue;
                }
            }
            if ch == '\0' {
                return Err(failed("FS_NOT_TEXT: binary file"));
            }
            if ch == '\n' {
                self.flush();
            } else {
                if self.chars <= self.max_chars {
                    self.line.push(ch);
                }
                self.chars = self.chars.saturating_add(ch.len_utf16());
            }
        }
        Ok(())
    }

    fn flush(&mut self) {
        self.total = self.total.saturating_add(1);
        if self.total >= self.offset && self.lines.len() < self.limit && !self.capped {
            if self.line.ends_with('\r') {
                self.line.pop();
                self.chars = self.chars.saturating_sub(1);
            }
            let mut text = self.line.clone();
            if self.chars > self.max_chars {
                let mut count = 0;
                text = text
                    .chars()
                    .take_while(|c| {
                        count += c.len_utf16();
                        count <= self.max_chars
                    })
                    .collect();
                text.push_str(&format!("... (line truncated to {} chars)", self.max_chars));
                self.any_truncated = true;
            }
            if text.len().saturating_add(1) > self.max_bytes.saturating_sub(self.bytes) {
                let room = self.max_bytes.saturating_sub(self.bytes);
                // The window's first line can be larger than the whole byte
                // budget. Emit a bounded preview of it anyway: the body then
                // still starts at the requested offset, and the footer's
                // continuation offset moves past the line. A window that
                // shows nothing would otherwise name the same offset again
                // and send the caller in a circle.
                if self.lines.is_empty() && room >= 2 {
                    let marker = room > 4;
                    let keep = room.saturating_sub(if marker { 4 } else { 1 });
                    let mut preview = prefix(&text, keep).to_string();
                    if marker {
                        preview.push('…');
                    }
                    self.bytes += preview.len() + 1;
                    self.lines.push(Line {
                        number: self.total,
                        text: preview,
                    });
                    self.any_truncated = true;
                }
                self.capped = true;
            } else {
                self.bytes += text.len() + 1;
                self.lines.push(Line {
                    number: self.total,
                    text,
                });
            }
        }
        self.line.clear();
        self.chars = 0;
    }

    fn footer(&self, end: usize, compact: bool) -> String {
        // Nothing from the requested window fit the byte or budget cap (the
        // first line overflowed). Naming `offset` again would point the
        // caller back at the very line that just overflowed, so point past
        // it. An empty file (total below the requested offset) keeps the
        // plain EOF footer below.
        if end < self.offset && self.total >= self.offset {
            let next = self.offset.saturating_add(1);
            return if compact {
                format!("\n[offset={next}]")
            } else {
                format!(
                    "\n(No line from offset {} fits this read; use offset={next} to continue.)",
                    self.offset
                )
            };
        }
        if compact {
            if end < self.total {
                format!("\n[offset={}]", end + 1)
            } else {
                format!("\n[EOF; {} lines]", self.total)
            }
        } else if end < self.total {
            format!(
                "\n(Showing lines {}-{end} of {}. Use offset={} to continue.)",
                self.offset,
                self.total,
                end + 1
            )
        } else {
            format!("\n(End of file - total {} lines)", self.total)
        }
    }

    fn render(&self, path: &str, budget: usize) -> ToolOutput {
        let header = format!("<path>{path}</path>\n<type>file</type>\n<content>\n");
        let compact =
            header.len() + self.footer(self.offset.saturating_sub(1), false).len() + 32 > budget;
        let (header, closing) = if compact {
            (String::new(), "")
        } else {
            (header, "\n</content>")
        };
        let mut body = String::new();
        let mut end = self.offset.saturating_sub(1);
        let mut clipped = false;
        for line in &self.lines {
            let text = format!("{}: {}\n", line.number, line.text);
            let remaining = budget.saturating_sub(
                header.len() + body.len() + self.footer(line.number, compact).len() + closing.len(),
            );
            if text.len() > remaining {
                if body.is_empty() && remaining >= 8 {
                    body.push_str(prefix(&text, remaining - 4));
                    body.push_str("...\n");
                    end = line.number;
                }
                clipped = true;
                break;
            }
            body.push_str(&text);
            end = line.number;
        }
        let content = format!("{header}{body}{}{closing}", self.footer(end, compact));
        ToolOutput { content, is_error: false,
            truncated: clipped || self.capped || self.any_truncated || end < self.total,
            details: Some(json!({"path":path,"offset":self.offset,"lines":self.lines,"totalLines":self.total})),
            meta: None,
        }.bounded(budget)
    }
}

pub(super) fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
