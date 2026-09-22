//! Explicitly registered local tools with a host-selected file-effect policy.
mod bash;
mod edit;
mod files;
mod jobs;
#[cfg(unix)]
mod output;
mod read;
mod workspace;

use crate::{Agent, Error, ErrorKind, Result, Tool};
pub use bash::BashTool;
use cap_std::{ambient_authority, fs::Dir};
pub use files::{EditTool, ReadTool, WriteTool};
pub use jobs::{
    BashJob, BashJobOutput, BashJobStatus, BashOutputCursor, BashResult, BashStreamOutput,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
pub(crate) use workspace::LocalSession;
use workspace::Workspace;

/// File-effect policy shared by the file tools and every Bash child.
///
/// Reads keep the host process's ambient visibility. `WorkspaceWrite` also
/// permits writes beneath the workspace and platform temporary roots, matching
/// DeepSeek Harness. `FullAccess` is Harness's `danger-full-access` mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    FullAccess,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::FullAccess => "full-access",
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Host-owned limits shared by a set of local tools.
#[derive(Clone)]
pub struct LocalToolConfig {
    pub root: PathBuf,
    /// Defaults to `WorkspaceWrite`; applies to file mutations and Bash.
    pub permission_mode: PermissionMode,
    /// Optional host file-size cap. None matches Harness's unrestricted text-file size.
    pub max_file_bytes: Option<usize>,
    pub max_read_lines: usize,
    pub max_read_line_chars: usize,
    pub max_read_bytes: usize,
    /// Maximum before/after bytes retained for host diff presentation.
    pub max_diff_bytes: usize,
    /// Also limited by the current run's tool output budget.
    pub max_output_bytes: usize,
    pub bash_path: PathBuf,
    pub bash_timeout: Duration,
    pub bash_max_timeout: Duration,
    pub bash_grace: Duration,
    /// In-memory tail bytes per stdout/stderr stream.
    pub bash_output_bytes: usize,
    /// Maximum retained background jobs, including completed jobs; forget completed jobs to free slots.
    pub max_background_jobs: usize,
    pub max_command_bytes: usize,
    /// Save overflowing Bash output under root/.abycore (default true).
    pub save_bash_output: bool,
    /// Maximum bytes per stream log; overflowing logs are discarded.
    pub max_bash_log_bytes: usize,
    /// Inherit the parent environment after credential-name filtering (default true).
    pub inherit_env: bool,
    /// Explicit additions/overrides; values are omitted from Debug.
    pub env: BTreeMap<String, String>,
}

impl LocalToolConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            permission_mode: PermissionMode::WorkspaceWrite,
            max_file_bytes: None,
            max_read_lines: 2000,
            max_read_line_chars: 2000,
            max_read_bytes: 50 * 1024,
            max_diff_bytes: 10 * 1024 * 1024,
            max_output_bytes: 64 * 1024,
            bash_path: "bash".into(),
            bash_timeout: Duration::from_secs(60),
            bash_max_timeout: Duration::from_secs(600),
            bash_grace: Duration::from_secs(3),
            bash_output_bytes: 64_000,
            max_background_jobs: 64,
            max_command_bytes: 64 * 1024,
            save_bash_output: true,
            max_bash_log_bytes: 64 * 1024 * 1024,
            inherit_env: true,
            env: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for LocalToolConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalToolConfig")
            .field("root", &self.root)
            .field("permission_mode", &self.permission_mode)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("max_read_lines", &self.max_read_lines)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("max_read_line_chars", &self.max_read_line_chars)
            .field("max_read_bytes", &self.max_read_bytes)
            .field("max_diff_bytes", &self.max_diff_bytes)
            .field("bash_path", &self.bash_path)
            .field("bash_timeout", &self.bash_timeout)
            .field("bash_max_timeout", &self.bash_max_timeout)
            .field("bash_grace", &self.bash_grace)
            .field("bash_output_bytes", &self.bash_output_bytes)
            .field("max_background_jobs", &self.max_background_jobs)
            .field("max_command_bytes", &self.max_command_bytes)
            .field("save_bash_output", &self.save_bash_output)
            .field("max_bash_log_bytes", &self.max_bash_log_bytes)
            .field("inherit_env", &self.inherit_env)
            .field("env", &"[REDACTED]")
            .finish()
    }
}

/// A directory capability and shared mutation lock. Clones share both.
#[derive(Clone)]
pub struct LocalTools {
    workspace: Arc<Workspace>,
    jobs: Arc<jobs::Jobs>,
}

impl LocalTools {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_config(LocalToolConfig::new(root.as_ref()))
    }

    pub fn with_config(mut config: LocalToolConfig) -> Result<Self> {
        if config.max_file_bytes == Some(0)
            || config.max_read_lines == 0
            || config.max_read_line_chars == 0
            || config.max_read_bytes == 0
            || config.max_diff_bytes == 0
            || config.max_output_bytes < 64
            || config.max_command_bytes == 0
            || config.max_bash_log_bytes == 0
            || config.bash_path.as_os_str().is_empty()
            || config.bash_timeout.is_zero()
            || config.bash_max_timeout.is_zero()
            || config.bash_grace.is_zero()
            || config.bash_output_bytes == 0
            || config.max_background_jobs == 0
            || config.bash_max_timeout > Duration::from_millis(i32::MAX as u64)
            || config.bash_grace > Duration::from_millis(i32::MAX as u64)
            || config
                .env
                .iter()
                .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "invalid local tool limits, shell or environment",
            ));
        }
        config.root = std::fs::canonicalize(&config.root).map_err(|_| {
            Error::new(
                ErrorKind::Configuration,
                "local tool root must be an accessible existing directory",
            )
        })?;
        #[cfg(unix)]
        let capability_root = PathBuf::from("/");
        #[cfg(not(unix))]
        let capability_root = config.root.clone();
        let directory =
            Dir::open_ambient_dir(&capability_root, ambient_authority()).map_err(|_| {
                Error::new(
                    ErrorKind::Configuration,
                    "cannot open local filesystem capability root",
                )
            })?;
        let workspace_path = config
            .root
            .strip_prefix(&capability_root)
            .map(Path::to_path_buf)
            .map_err(|_| {
                Error::new(
                    ErrorKind::Configuration,
                    "workspace is outside the local filesystem capability root",
                )
            })?;
        let mut writable_roots = vec![];
        for candidate in [PathBuf::from("/tmp"), std::env::temp_dir()] {
            if let Ok(root) = std::fs::canonicalize(candidate)
                && !writable_roots.contains(&root)
            {
                writable_roots.push(root);
            }
        }
        Ok(Self {
            jobs: Arc::default(),
            workspace: Arc::new(Workspace {
                config,
                capability_root,
                workspace_path,
                writable_roots,
                directory,
                mutations: Default::default(),
            }),
        })
    }

    pub fn read(&self) -> ReadTool {
        ReadTool {
            workspace: self.workspace.clone(),
        }
    }
    pub fn write(&self) -> WriteTool {
        WriteTool {
            workspace: self.workspace.clone(),
        }
    }
    pub fn edit(&self) -> EditTool {
        EditTool {
            workspace: self.workspace.clone(),
        }
    }
    pub fn bash(&self) -> BashTool {
        BashTool {
            workspace: self.workspace.clone(),
            jobs: self.jobs.clone(),
        }
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.workspace.config.permission_mode
    }

    /// List jobs owned by this bundle; completed jobs remain until forgotten.
    pub fn jobs(&self) -> Vec<BashJob> {
        self.jobs.list()
    }
    pub fn job(&self, id: &str) -> std::result::Result<BashJob, crate::ToolError> {
        self.jobs.get(id)
    }
    /// Byte cursors are independent for stdout and stderr. A lossy read reports evicted bytes.
    pub fn job_output(
        &self,
        id: &str,
        cursor: BashOutputCursor,
    ) -> std::result::Result<BashJobOutput, crate::ToolError> {
        self.jobs.output(id, cursor)
    }
    /// Cancel a job and await process cleanup. Repeated calls are safe.
    pub async fn kill_job(&self, id: &str) -> std::result::Result<BashJob, crate::ToolError> {
        self.jobs.kill(id).await
    }
    pub async fn wait_job(&self, id: &str) -> std::result::Result<BashJob, crate::ToolError> {
        self.jobs.wait(id).await
    }
    /// Remove a completed job's in-memory record. Retained log files are host-owned.
    pub fn forget_job(&self, id: &str) -> std::result::Result<(), crate::ToolError> {
        self.jobs.forget(id)
    }
    /// Stop all background jobs and join cleanup before shutting down the host runtime.
    pub async fn shutdown(&self) {
        self.jobs.shutdown().await;
    }

    /// Register all four tools atomically. To grant fewer capabilities, register individual tools.
    pub fn register(&self, agent: &mut Agent) -> Result<()> {
        agent.register_tools(self.tools())
    }

    /// Apply a new local permission policy without replacing the agent's
    /// runtime identity. Existing children retain their delegated tools.
    pub fn replace(&self, agent: &mut Agent) -> Result<()> {
        agent.replace_tools(self.tools())
    }

    fn tools(&self) -> [Arc<dyn Tool>; 4] {
        let tools: [Arc<dyn Tool>; 4] = [
            Arc::new(self.read()),
            Arc::new(self.write()),
            Arc::new(self.edit()),
            Arc::new(self.bash()),
        ];
        tools
    }
}

#[cfg(test)]
mod enhancement_tests;
#[cfg(test)]
mod tests;
