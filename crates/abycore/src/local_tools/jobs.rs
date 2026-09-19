use super::{
    PermissionMode,
    workspace::{ToolResult, failed},
};
use crate::CancellationToken;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BashStreamOutput {
    pub text: String,
    pub truncated: bool,
    pub spill_path: Option<PathBuf>,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BashResult {
    pub stdout: BashStreamOutput,
    pub stderr: BashStreamOutput,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub aborted: bool,
    pub timeout_ms: Option<u64>,
    pub output_complete: bool,
    pub permission_mode: PermissionMode,
    pub sandbox_denied: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BashJobStatus {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashJob {
    pub id: String,
    pub command: String,
    pub description: String,
    pub workdir: PathBuf,
    pub status: BashJobStatus,
    pub result: Option<BashResult>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashOutputCursor {
    pub stdout: u64,
    pub stderr: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashJobOutput {
    pub status: BashJobStatus,
    pub stdout: String,
    pub stderr: String,
    pub cursor: BashOutputCursor,
    pub lossy: bool,
    pub stdout_spill_path: Option<PathBuf>,
    pub stderr_spill_path: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct StreamBuffer {
    pub tail: VecDeque<u8>,
    pub total: u64,
    pub spill_path: Option<PathBuf>,
}
impl StreamBuffer {
    pub fn push(&mut self, bytes: &[u8], limit: usize) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let excess = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(limit);
        self.tail.drain(..excess.min(self.tail.len()));
        self.tail
            .extend(&bytes[bytes.len().saturating_sub(limit)..]);
    }
    pub fn snapshot(&self) -> BashStreamOutput {
        let bytes: Vec<_> = self.tail.iter().copied().collect();
        BashStreamOutput {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated: self.total > self.tail.len() as u64,
            spill_path: self.spill_path.clone(),
            total_bytes: self.total,
        }
    }
    fn delta(&self, from: u64, complete: bool) -> ToolResult<(String, u64, bool)> {
        if from > self.total {
            return Err(failed("output cursor is ahead of the stream"));
        }
        let start = self.total - self.tail.len() as u64;
        let lossy = from < start;
        let bytes: Vec<_> = self
            .tail
            .iter()
            .skip(from.saturating_sub(start) as usize)
            .copied()
            .collect();
        // Defer an incomplete UTF-8 suffix until the next read, preserving byte cursors.
        let mut end = bytes.len();
        if !complete {
            let mut offset = 0;
            while offset < bytes.len() {
                match std::str::from_utf8(&bytes[offset..]) {
                    Ok(_) => break,
                    Err(e) => {
                        offset += e.valid_up_to();
                        match e.error_len() {
                            Some(len) => offset += len,
                            None => {
                                end = offset;
                                break;
                            }
                        }
                    }
                }
            }
        }
        Ok((
            String::from_utf8_lossy(&bytes[..end]).into_owned(),
            from.max(start) + end as u64,
            lossy,
        ))
    }
}

pub(super) type Buffer = Arc<Mutex<StreamBuffer>>;
pub(super) struct Entry {
    state: Mutex<BashJob>,
    pub cancellation: CancellationToken,
    pub stdout: Buffer,
    pub stderr: Buffer,
    done: Notify,
}
impl Entry {
    fn snapshot(&self) -> BashJob {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    #[cfg(unix)]
    pub fn finish(&self, result: ToolResult<BashResult>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.status != BashJobStatus::Running {
            return;
        }
        match result {
            Ok(result) => {
                state.status = if result.aborted {
                    BashJobStatus::Cancelled
                } else {
                    BashJobStatus::Completed
                };
                state.result = Some(result);
            }
            Err(error) => {
                state.status = if self.cancellation.is_cancelled() {
                    BashJobStatus::Cancelled
                } else {
                    BashJobStatus::Failed
                };
                state.error = Some(error.to_string());
            }
        }
        drop(state);
        self.done.notify_waiters();
    }
    async fn wait(&self) -> BashJob {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.snapshot();
            if state.status != BashJobStatus::Running {
                return state;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct Store {
    entries: BTreeMap<String, Arc<Entry>>,
    closed: bool,
}
#[derive(Default)]
pub(super) struct Jobs {
    store: Mutex<Store>,
}
impl Jobs {
    #[cfg(unix)]
    pub fn insert(
        &self,
        command: String,
        description: String,
        workdir: PathBuf,
        cap: usize,
    ) -> ToolResult<(String, Arc<Entry>)> {
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if store.closed {
            return Err(failed("background job manager has shut down"));
        }
        if store.entries.len() >= cap {
            return Err(failed(
                "background job limit reached; the host must forget completed jobs",
            ));
        }
        let id = loop {
            let candidate = format!("job-{:016x}", rand::random::<u64>());
            if !store.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let entry = Arc::new(Entry {
            state: Mutex::new(BashJob {
                id: id.clone(),
                command,
                description,
                workdir,
                status: BashJobStatus::Running,
                result: None,
                error: None,
            }),
            cancellation: CancellationToken::new(),
            stdout: Arc::default(),
            stderr: Arc::default(),
            done: Notify::new(),
        });
        store.entries.insert(id.clone(), entry.clone());
        Ok((id, entry))
    }
    fn entry(&self, id: &str) -> ToolResult<Arc<Entry>> {
        self.store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| failed("unknown background job"))
    }
    pub fn list(&self) -> Vec<BashJob> {
        self.store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .values()
            .map(|entry| entry.snapshot())
            .collect()
    }
    pub fn get(&self, id: &str) -> ToolResult<BashJob> {
        Ok(self.entry(id)?.snapshot())
    }
    pub fn output(&self, id: &str, cursor: BashOutputCursor) -> ToolResult<BashJobOutput> {
        let entry = self.entry(id)?;
        let status = entry.snapshot().status;
        let complete = status != BashJobStatus::Running;
        let stdout = entry.stdout.lock().unwrap_or_else(|p| p.into_inner());
        let stderr = entry.stderr.lock().unwrap_or_else(|p| p.into_inner());
        let (out, out_cursor, out_lossy) = stdout.delta(cursor.stdout, complete)?;
        let (err, err_cursor, err_lossy) = stderr.delta(cursor.stderr, complete)?;
        Ok(BashJobOutput {
            status,
            stdout: out,
            stderr: err,
            cursor: BashOutputCursor {
                stdout: out_cursor,
                stderr: err_cursor,
            },
            lossy: out_lossy || err_lossy,
            stdout_spill_path: stdout.spill_path.clone(),
            stderr_spill_path: stderr.spill_path.clone(),
        })
    }
    pub async fn wait(&self, id: &str) -> ToolResult<BashJob> {
        Ok(self.entry(id)?.wait().await)
    }
    pub async fn kill(&self, id: &str) -> ToolResult<BashJob> {
        let entry = self.entry(id)?;
        entry.cancellation.cancel();
        Ok(entry.wait().await)
    }
    pub fn forget(&self, id: &str) -> ToolResult<()> {
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let entry = store
            .entries
            .get(id)
            .ok_or_else(|| failed("unknown background job"))?;
        if entry.snapshot().status == BashJobStatus::Running {
            return Err(failed("stop or wait for the job before forgetting it"));
        }
        store.entries.remove(id);
        Ok(())
    }
    pub async fn shutdown(&self) {
        let entries: Vec<_> = {
            let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            store.closed = true;
            store.entries.values().cloned().collect()
        };
        for entry in &entries {
            entry.cancellation.cancel();
        }
        for entry in entries {
            entry.wait().await;
        }
    }
}
impl Drop for Jobs {
    fn drop(&mut self) {
        for entry in self
            .store
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .values()
        {
            entry.cancellation.cancel();
        }
    }
}

/// A dropped/panicked background future must still settle the host's waiters.
#[cfg(unix)]
pub(super) struct CompletionGuard(pub Arc<Entry>);
#[cfg(unix)]
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.0.finish(Err(failed(
            "background executor stopped before reporting its result",
        )));
    }
}
