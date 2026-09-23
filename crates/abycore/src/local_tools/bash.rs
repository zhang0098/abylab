use super::{
    LocalTools, PermissionMode,
    jobs::Jobs,
    workspace::{ToolResult, Workspace, failed, parse},
};
use crate::{CallBudget, Result, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct BashTool {
    pub(super) workspace: Arc<Workspace>,
    pub(super) jobs: Arc<Jobs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BashInput {
    command: String,
    description: String,
    timeout_ms: Option<f64>,
    workdir: Option<String>,
    #[serde(default, rename = "run_in_background")]
    background: bool,
}
impl BashTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.bash())
    }
    fn input(&self, value: &Value) -> ToolResult<BashInput> {
        let input: BashInput = parse(value)?;
        if input.command.trim().is_empty()
            || input.command.contains('\0')
            || input.command.len() > self.workspace.config.max_command_bytes
        {
            return Err(failed(
                "command must be nonempty, without NUL, and fit max_command_bytes",
            ));
        }
        if input.description.trim().is_empty() {
            return Err(failed("description must be a nonempty string"));
        }
        if ["timeoutMs", "workdir"]
            .iter()
            .any(|key| value.get(key).is_some_and(Value::is_null))
            || input.timeout_ms.is_some_and(|n| !n.is_finite() || n <= 0.0)
        {
            return Err(failed(
                "timeoutMs must be a positive number; optional arguments cannot be null",
            ));
        }
        if input
            .workdir
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.contains('\0'))
        {
            return Err(failed("workdir must be a nonempty directory path"));
        }
        Ok(input)
    }

    /// How long one foreground command may run: the caller's `timeoutMs` when
    /// it gave one, else the configured `bash_timeout`, both capped by
    /// `bash_max_timeout`.
    fn command_timeout(&self, timeout_ms: Option<f64>) -> Duration {
        let cap = self.workspace.config.bash_max_timeout;
        timeout_ms
            .map(|ms| Duration::from_secs_f64((ms / 1000.0).min(cap.as_secs_f64())))
            .unwrap_or(self.workspace.config.bash_timeout)
            .min(cap)
    }
}
impl Tool for BashTool {
    fn cleanup_grace(&self) -> Duration {
        self.workspace
            .config
            .bash_grace
            .saturating_mul(2)
            .saturating_add(Duration::from_secs(1))
    }
    /// The model's `timeoutMs` is this call's budget: the executor's per-call
    /// backstop must not clip a command the caller deliberately gave more time,
    /// and the command is killed and reported by this tool, not by the executor.
    fn call_budget(&self, value: &Value) -> CallBudget {
        match self.input(value) {
            Ok(input) => input.timeout_ms.map_or(CallBudget::Backstop, |ms| {
                CallBudget::Own(self.command_timeout(Some(ms)))
            }),
            Err(_) => CallBudget::Backstop,
        }
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description: format!(
                "Execute a Bash command in a fresh shell under {} file permissions. description briefly explains the command. workdir defaults to the workspace; relative paths resolve against it. timeoutMs is in milliseconds and capped by the executor. stdout and stderr are returned separately with exit/timeout/sandbox markers. Set run_in_background=true to return a job id immediately without an execution timeout; the host manages job output and termination. Shell variables and cwd do not persist across calls.",
                self.workspace.config.permission_mode
            ),
            parameters: json!({"type":"object","properties":{"command":{"type":"string","minLength":1},"description":{"type":"string","minLength":1},"timeoutMs":{"type":"number","exclusiveMinimum":0},"workdir":{"type":"string","minLength":1},"run_in_background":{"type":"boolean"}},"required":["command","description"],"additionalProperties":false}),
        }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        self.input(value).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            #[cfg(not(unix))]
            {
                let _ = (input, context);
                Err(failed(
                    "bash currently requires Unix process groups (Linux/macOS)",
                ))
            }
            #[cfg(unix)]
            {
                use super::jobs::CompletionGuard;
                if context.cancellation.is_cancelled() || context.remaining().is_zero() {
                    return Err(failed("Bash cancelled before execution"));
                }
                let workdir = input
                    .workdir
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.workspace.config.root.clone());
                let workdir = if workdir.is_absolute() {
                    workdir
                } else {
                    self.workspace.config.root.join(workdir)
                };
                let workdir = tokio::fs::canonicalize(workdir)
                    .await
                    .map_err(|_| failed("cannot resolve Bash workdir"))?;
                if !tokio::fs::metadata(&workdir)
                    .await
                    .map_err(|_| failed("cannot inspect Bash workdir"))?
                    .is_dir()
                {
                    return Err(failed("workdir must be a directory"));
                }
                if context.cancellation.is_cancelled() || context.remaining().is_zero() {
                    return Err(failed("Bash cancelled before execution"));
                }
                if input.background {
                    let (id, entry) = self.jobs.insert(
                        input.command.clone(),
                        input.description,
                        workdir.clone(),
                        self.workspace.config.max_background_jobs,
                    )?;
                    let workspace = self.workspace.clone();
                    let spec = Spec {
                        command: input.command,
                        workdir,
                        timeout: None,
                        deadline: None,
                        cancellation: entry.cancellation.clone(),
                        limit: workspace.config.bash_output_bytes,
                    };
                    let guard = CompletionGuard(entry.clone());
                    tokio::spawn(async move {
                        let _guard = guard;
                        entry.finish(
                            run(workspace, spec, entry.stdout.clone(), entry.stderr.clone()).await,
                        );
                    });
                    let mut output =
                        crate::ToolOutput::text(format!("started background job {id}"));
                    output.details = Some(json!({"kind":"background","jobId":id}));
                    Ok(output)
                } else {
                    let limit = self.workspace.output_limit(&context);
                    let timeout = self.command_timeout(input.timeout_ms);
                    let spec = Spec {
                        command: input.command,
                        workdir,
                        timeout: Some(timeout),
                        deadline: Some(context.deadline),
                        cancellation: context.cancellation,
                        limit: self
                            .workspace
                            .config
                            .bash_output_bytes
                            .min((limit / 2).max(1)),
                    };
                    let result =
                        run(self.workspace.clone(), spec, Arc::default(), Arc::default()).await?;
                    Ok(super::output::render(&result, limit))
                }
            }
        })
    }
}

#[cfg(unix)]
struct Spec {
    command: String,
    workdir: PathBuf,
    timeout: Option<Duration>,
    deadline: Option<tokio::time::Instant>,
    cancellation: crate::CancellationToken,
    limit: usize,
}

#[cfg(unix)]
async fn run(
    workspace: Arc<Workspace>,
    spec: Spec,
    stdout_buffer: super::jobs::Buffer,
    stderr_buffer: super::jobs::Buffer,
) -> ToolResult<super::jobs::BashResult> {
    use super::{jobs::BashResult, output::Capture};
    use std::process::Stdio;
    use tokio::{io::AsyncReadExt, time::Instant};
    if spec.cancellation.is_cancelled() {
        return Err(failed("Bash cancelled before execution"));
    }
    let started = Instant::now();
    let deadline = spec
        .timeout
        .and_then(|timeout| started.checked_add(timeout));
    let deadline = match (deadline, spec.deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let timeout_ms = deadline.map(|d| {
        d.saturating_duration_since(started)
            .as_millis()
            .min(u64::MAX as u128) as u64
    });
    let mut command = bash_command(&workspace, &spec.command)?;
    command
        .current_dir(&spec.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .env_clear();
    if workspace.config.inherit_env {
        for (key, value) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if !["KEY", "PASSWORD", "SECRET", "TOKEN"]
                .iter()
                .any(|word| upper.contains(word))
                && !upper.starts_with("DSH_")
                && !upper.starts_with("ABYCORE_")
            {
                command.env(key, value);
            }
        }
    }
    command
        .envs([
            ("NO_COLOR", "1"),
            ("TERM", "dumb"),
            ("PAGER", "cat"),
            ("GIT_PAGER", "cat"),
        ])
        .envs(&workspace.config.env);
    configure_sandbox(&mut command, &workspace)?;
    let mut child = command.spawn().map_err(|error| {
        if workspace.config.permission_mode == PermissionMode::FullAccess {
            failed(format!("cannot start configured Bash executable: {error}"))
        } else {
            failed(format!(
                "SANDBOX_UNAVAILABLE: cannot start Bash under {} mode: {error}",
                workspace.config.permission_mode
            ))
        }
    })?;
    let pid = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| ToolError::Uncertain("cannot track Bash process group".into()))?;
    let group = ProcessGroup(pid);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Uncertain("Bash stdout is missing".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Uncertain("Bash stderr is missing".into()))?;
    let mut out = Capture::new(workspace.clone(), stdout_buffer.clone(), spec.limit);
    let mut err = Capture::new(workspace.clone(), stderr_buffer.clone(), spec.limit);
    let mut out_bytes = [0; 8192];
    let mut err_bytes = [0; 8192];
    let (mut out_open, mut err_open) = (true, true);
    let mut status = None;
    let mut stopping = false;
    let mut killed = false;
    let mut timed_out = false;
    let mut aborted = false;
    let mut complete = true;
    let mut stop_at = None;
    let mut cleanup_end = None;
    loop {
        if status.is_some() && !out_open && !err_open && (killed || !group.exists()) {
            break;
        }
        tokio::select! {
            biased;
            _ = spec.cancellation.cancelled(), if !stopping => {
                aborted = true; stopping = true;
                group.signal(rustix::process::Signal::TERM)?;
                stop_at = Some(Instant::now() + workspace.config.bash_grace);
            },
            _ = async { tokio::time::sleep_until(deadline.expect("deadline")).await }, if deadline.is_some() && !stopping => {
                timed_out = true; stopping = true;
                group.signal(rustix::process::Signal::TERM)?;
                stop_at = Some(Instant::now() + workspace.config.bash_grace);
            },
            _ = async { tokio::time::sleep_until(stop_at.expect("stop deadline")).await }, if stop_at.is_some() && !killed => {
                group.signal(rustix::process::Signal::KILL)?;
                killed = true;
                cleanup_end = Some(Instant::now() + workspace.config.bash_grace);
            },
            _ = async { tokio::time::sleep_until(cleanup_end.expect("cleanup deadline")).await }, if cleanup_end.is_some() => {
                if status.is_none() { return Err(ToolError::Uncertain("cannot reap Bash after SIGKILL".into())); }
                complete = false; break;
            },
            result = child.wait(), if status.is_none() => {
                status = Some(result.map_err(|_| ToolError::Uncertain("cannot collect Bash exit status".into()))?);
                if !stopping {
                    stopping = true;
                    group.signal(rustix::process::Signal::TERM)?;
                    stop_at = Some(Instant::now() + workspace.config.bash_grace);
                }
            },
            result = stdout.read(&mut out_bytes), if out_open => {
                let count = result.map_err(|_| ToolError::Uncertain("cannot read Bash stdout".into()))?;
                if count == 0 { out_open = false; }
                else if tokio::time::timeout(workspace.config.bash_grace,out.push(&out_bytes[..count])).await.is_err() { complete = false; }
            },
            result = stderr.read(&mut err_bytes), if err_open => {
                let count = result.map_err(|_| ToolError::Uncertain("cannot read Bash stderr".into()))?;
                if count == 0 { err_open = false; }
                else if tokio::time::timeout(workspace.config.bash_grace,err.push(&err_bytes[..count])).await.is_err() { complete = false; }
            },
        }
    }
    group.signal(rustix::process::Signal::KILL)?;
    let status =
        status.ok_or_else(|| ToolError::Uncertain("Bash ended without an exit status".into()))?;
    let finalized = tokio::time::timeout(workspace.config.bash_grace, async {
        tokio::join!(out.finish(complete), err.finish(complete));
    })
    .await
    .is_ok();
    if !finalized {
        complete = false;
    }
    drop(out);
    drop(err);
    use std::os::unix::process::ExitStatusExt;
    let stdout = stdout_buffer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot();
    let stderr = stderr_buffer
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .snapshot();
    // A permission-error signature under a sandboxed preset. The sandbox does
    // not report its denials and ordinary OS EACCES looks the same on stderr,
    // so this is a heuristic: hosts must not present it as proof that the
    // sandbox blocked the command.
    let denied = status.code() != Some(0)
        && workspace.config.permission_mode != PermissionMode::FullAccess
        && [
            "permission denied",
            "read-only file system",
            "operation not permitted",
        ]
        .iter()
        .any(|signature| stderr.text.to_ascii_lowercase().contains(signature));
    Ok(BashResult {
        stdout,
        stderr,
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        aborted,
        timeout_ms,
        output_complete: complete,
        permission_mode: workspace.config.permission_mode,
        sandbox_denied: denied,
    })
}

#[cfg(unix)]
fn bash_command(workspace: &Workspace, script: &str) -> ToolResult<tokio::process::Command> {
    #[cfg(target_os = "macos")]
    if workspace.config.permission_mode != PermissionMode::FullAccess {
        let quote = |path: &Path| {
            path.to_string_lossy()
                .replace('\\', r"\\")
                .replace('"', r#"\""#)
        };
        let mut profile = String::from(
            r#"(version 1) (allow default) (deny file-write*) (allow file-write* (literal "/dev/null"))"#,
        );
        if workspace.config.permission_mode == PermissionMode::WorkspaceWrite {
            profile.push_str(&format!(
                " (allow file-write* (subpath \"{}\"))",
                quote(&workspace.config.root)
            ));
            for root in &workspace.writable_roots {
                profile.push_str(&format!(
                    " (allow file-write* (subpath \"{}\"))",
                    quote(root)
                ));
            }
        }
        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", &profile, "--"])
            .arg(&workspace.config.bash_path)
            .args(["-c", script]);
        return Ok(command);
    }

    let mut command = tokio::process::Command::new(&workspace.config.bash_path);
    command.args(["-c", script]);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn configure_sandbox(
    command: &mut tokio::process::Command,
    workspace: &Workspace,
) -> ToolResult<()> {
    let mode = match workspace.config.permission_mode {
        PermissionMode::FullAccess => return Ok(()),
        PermissionMode::ReadOnly => crate::subprocess_sandbox::ConfinedMode::ReadOnly,
        PermissionMode::WorkspaceWrite => crate::subprocess_sandbox::ConfinedMode::WorkspaceWrite,
    };
    crate::subprocess_sandbox::confine(
        command.as_std_mut(),
        mode,
        &workspace.config.root,
        &workspace.writable_roots,
    )
    .map_err(|error| {
        failed(format!(
            "SANDBOX_UNAVAILABLE: {} mode requires Linux Landlock ABI v3 or newer and the metadata seccomp policy: {error}",
            workspace.config.permission_mode
        ))
    })
}

#[cfg(target_os = "macos")]
fn configure_sandbox(
    _command: &mut tokio::process::Command,
    _workspace: &Workspace,
) -> ToolResult<()> {
    // Restricted commands were wrapped by the system Seatbelt runner in
    // `bash_command`; full-access commands need no setup.
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn configure_sandbox(
    _command: &mut tokio::process::Command,
    workspace: &Workspace,
) -> ToolResult<()> {
    (workspace.config.permission_mode == PermissionMode::FullAccess)
        .then_some(())
        .ok_or_else(|| {
            failed(format!(
                "SANDBOX_UNAVAILABLE: no Bash filesystem sandbox backend is available for {} mode on this platform",
                workspace.config.permission_mode
            ))
        })
}

#[cfg(unix)]
struct ProcessGroup(rustix::process::Pid);
#[cfg(unix)]
impl ProcessGroup {
    fn exists(&self) -> bool {
        rustix::process::test_kill_process_group(self.0).is_ok()
    }
    fn signal(&self, signal: rustix::process::Signal) -> ToolResult<()> {
        match rustix::process::kill_process_group(self.0, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(_) => Err(ToolError::Uncertain(
                "cannot signal Bash process group".into(),
            )),
        }
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = self.signal(rustix::process::Signal::KILL);
    }
}
