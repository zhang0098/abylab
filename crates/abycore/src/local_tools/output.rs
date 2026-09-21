//! Per-stream bounded tails and private spill files.
use super::{
    jobs::{BashResult, BashStreamOutput, Buffer},
    workspace::Workspace,
};
use crate::ToolOutput;
use cap_std::fs::{Dir, DirBuilder, DirBuilderExt, OpenOptions, OpenOptionsExt};
use std::{io, sync::Arc};
use tokio::io::AsyncWriteExt;

pub(super) struct Capture {
    workspace: Arc<Workspace>,
    pub buffer: Buffer,
    limit: usize,
    disabled: bool,
    log: Option<Log>,
    finished: bool,
}
impl Capture {
    pub fn new(workspace: Arc<Workspace>, buffer: Buffer, limit: usize) -> Self {
        let disabled = !workspace.config.save_bash_output
            || workspace.config.permission_mode == super::PermissionMode::ReadOnly;
        Self {
            workspace,
            buffer,
            limit,
            disabled,
            log: None,
            finished: false,
        }
    }
    pub async fn push(&mut self, bytes: &[u8]) {
        let (total, tail_len, prefix) = {
            let mut buffer = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
            let tail_len = buffer.tail.len();
            let prefix = if !self.disabled
                && self.log.is_none()
                && tail_len.saturating_add(bytes.len()) > self.limit
            {
                buffer.tail.iter().copied().collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            // Collection and job cursors must progress even when log I/O stalls.
            buffer.push(bytes, self.limit);
            (buffer.total, tail_len, prefix)
        };
        if total > self.workspace.config.max_bash_log_bytes as u64 {
            self.discard();
        } else if !self.disabled
            && (tail_len.saturating_add(bytes.len()) > self.limit || self.log.is_some())
        {
            if self.log.is_none() {
                match Log::create(self.workspace.clone()).await {
                    Ok(mut log) => {
                        if log.write(&prefix).await.is_ok() {
                            self.log = Some(log);
                        } else {
                            self.discard();
                        }
                    }
                    Err(_) => self.discard(),
                }
            }
            if let Some(log) = &mut self.log {
                if log.write(bytes).await.is_err() {
                    self.discard();
                } else {
                    self.buffer
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .spill_path =
                        Some(self.workspace.config.root.join(".abycore").join(&log.name));
                }
            }
        }
    }
    fn discard(&mut self) {
        self.disabled = true;
        self.log.take();
        self.buffer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .spill_path = None;
    }
    pub async fn finish(&mut self, complete: bool) {
        if !complete {
            self.discard();
        }
        if let Some(log) = &mut self.log {
            if log.sync().await.is_err() {
                self.discard();
            } else {
                log.keep = true;
            }
        }
        self.finished = true;
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished {
            self.discard();
        }
    }
}

struct Log {
    directory: Dir,
    name: String,
    file: Option<tokio::fs::File>,
    keep: bool,
}
impl Log {
    async fn create(workspace: Arc<Workspace>) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let mut builder = DirBuilder::new();
            builder.mode(0o700);
            let log_path = workspace.workspace_child(".abycore");
            match workspace.directory.create_dir_with(&log_path, &builder) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            if !workspace.directory.symlink_metadata(&log_path)?.is_dir() {
                return Err(io::Error::other(
                    "Bash log directory must be a real directory",
                ));
            }
            let directory = workspace.directory.open_dir(&log_path)?;
            for _ in 0..8 {
                let name = format!("{:012x}.log", rand::random::<u64>() & 0xffff_ffff_ffff);
                let mut options = OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                match directory.open_with(&name, &options) {
                    Ok(file) => {
                        return Ok(Self {
                            directory,
                            name,
                            file: Some(tokio::fs::File::from_std(file.into_std())),
                            keep: false,
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::other("could not allocate a unique log"))
        })
        .await
        .map_err(io::Error::other)?
    }
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.as_mut().expect("open log").write_all(bytes).await
    }
    async fn sync(&mut self) -> io::Result<()> {
        let file = self.file.as_mut().expect("open log");
        file.flush().await?;
        file.sync_all().await
    }
}
impl Drop for Log {
    fn drop(&mut self) {
        self.file.take();
        if !self.keep {
            let _ = self.directory.remove_file(&self.name);
        }
    }
}

fn tail(text: &str, budget: usize) -> &str {
    let mut start = text.len().saturating_sub(budget);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
fn notice(stream: &BashStreamOutput, clipped: bool) -> String {
    if !(stream.truncated || clipped) {
        return String::new();
    }
    format!(
        "\n[output truncated; full output: {}]",
        stream
            .spill_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(unavailable)".into())
    )
}
pub(super) fn render(result: &BashResult, budget: usize) -> ToolOutput {
    let mut status = String::new();
    if result.timed_out {
        status.push_str(&format!(
            "\n[timed out after {}ms]",
            result.timeout_ms.unwrap_or(0)
        ));
    }
    if result.aborted {
        status.push_str("\n[aborted]");
    }
    if let Some(signal) = result.signal {
        status.push_str(&format!("\n[killed by signal: {signal}]"));
    } else if result.exit_code != Some(0) {
        status.push_str(&format!(
            "\n[exit code: {}]",
            result
                .exit_code
                .map(|n| n.to_string())
                .unwrap_or_else(|| "null".into())
        ));
    }
    if !result.output_complete {
        status.push_str("\n[output pipes did not close]");
    }
    if result.sandbox_denied {
        status.push_str(&format!(
            "\n[sandbox: permission error under {} mode — writes outside the workspace are blocked; this may also be an ordinary OS permission error]",
            result.permission_mode
        ));
    }
    let err_header = if result.stderr.text.is_empty() {
        ""
    } else {
        "\n[stderr]\n"
    };
    let mut out_notice = notice(&result.stdout, false);
    let mut err_notice = notice(&result.stderr, false);
    let total = result.stdout.text.len()
        + result.stderr.text.len()
        + out_notice.len()
        + err_notice.len()
        + err_header.len()
        + status.len();
    let clipped = total > budget;
    if clipped {
        out_notice = notice(&result.stdout, !result.stdout.text.is_empty());
        err_notice = notice(&result.stderr, !result.stderr.text.is_empty());
    }
    // Structured details retain both paths even when the model's budget cannot fit them.
    if out_notice.len() + err_notice.len() + err_header.len() + status.len() + 8 > budget {
        out_notice.clear();
        err_notice.clear();
    }
    let available = budget
        .saturating_sub(out_notice.len() + err_notice.len() + err_header.len() + status.len());
    let out_budget = if result.stderr.text.is_empty() {
        available
    } else if result.stdout.text.is_empty() {
        0
    } else {
        let half = available / 2;
        if result.stderr.text.len() < half {
            available - result.stderr.text.len()
        } else {
            half.min(result.stdout.text.len())
        }
    };
    let err_budget = available.saturating_sub(out_budget.min(result.stdout.text.len()));
    let mut content = format!(
        "{}{}{}{}{}{}",
        tail(&result.stdout.text, out_budget),
        out_notice,
        err_header,
        tail(&result.stderr.text, err_budget),
        err_notice,
        status
    );
    if content.is_empty() {
        content = "(no output)".into();
    }
    ToolOutput {
        content,
        is_error: false,
        truncated: clipped
            || result.stdout.truncated
            || result.stderr.truncated
            || !result.output_complete,
        details: Some(serde_json::to_value(result).expect("serializable Bash result")),
        meta: None,
    }
    .bounded(budget)
}
