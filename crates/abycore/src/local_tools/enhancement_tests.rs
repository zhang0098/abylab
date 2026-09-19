#![cfg(unix)]
use super::{tests::context, *};
use crate::{ToolContext, ToolOutput};
use serde_json::json;
use std::time::Duration;

fn bundle(root: &std::path::Path) -> LocalTools {
    let mut config = LocalToolConfig::new(root);
    config.bash_grace = Duration::from_millis(40);
    LocalTools::with_config(config).unwrap()
}
async fn bash(tools: &LocalTools, command: &str, ctx: ToolContext) -> ToolOutput {
    tools
        .bash()
        .execute(
            json!({"command":command,"description":"Test Bash behavior"}),
            ctx,
        )
        .await
        .unwrap()
}
fn result(output: ToolOutput) -> BashResult {
    serde_json::from_value(output.details.unwrap()).unwrap()
}
async fn background(tools: &LocalTools, command: &str, ctx: ToolContext) -> String {
    let output = tools.bash().execute(json!({"command":command,"description":"Start test job","timeoutMs":1,"run_in_background":true}),ctx).await.unwrap();
    output.details.unwrap()["jobId"]
        .as_str()
        .unwrap()
        .to_owned()
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bash_separates_streams_and_reports_nonzero_without_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let output = bash(
        &tools,
        "printf stdout; printf stderr >&2; exit 7",
        context(4096),
    )
    .await;
    assert!(!output.is_error);
    assert!(output.content.contains("stdout\n[stderr]\nstderr"));
    assert!(output.content.contains("[exit code: 7]"));
    let output = result(output);
    assert_eq!(output.stdout.text, "stdout");
    assert_eq!(output.stderr.text, "stderr");
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(
        bash(&tools, "true", context(4096)).await.content,
        "(no output)"
    );
}

#[tokio::test]
async fn bash_workdir_and_shell_state_are_per_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let tools = bundle(dir.path());
    let output = tools.bash().execute(json!({"command":"pwd; export LOCAL_ONLY=yes","description":"Check workdir","workdir":"sub"}),context(4096)).await.unwrap();
    assert_eq!(
        result(output).stdout.text.trim(),
        dir.path()
            .join("sub")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
    let output = bash(&tools, "pwd; printf '<%s>' \"$LOCAL_ONLY\"", context(4096)).await;
    assert_eq!(
        result(output).stdout.text,
        format!("{}\n<>", dir.path().canonicalize().unwrap().display())
    );
}

#[tokio::test]
async fn environment_scrubs_credentials_preserves_home_and_allows_explicit_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config
        .env
        .insert("EXPLICIT_SECRET".into(), "test-only-value".into());
    assert!(!format!("{config:?}").contains("test-only-value"));
    let tools = LocalTools::with_config(config).unwrap();
    let output = result(bash(&tools,r#"if [ -n "$DEEPSEEK_API_KEY" ]; then printf leaked; else printf scrubbed; fi; printf ':%s:%s:' "$EXPLICIT_SECRET" "$TERM"; if [ -n "$HOME" ]; then printf present; fi"#,context(4096)).await);
    assert!(
        output
            .stdout
            .text
            .starts_with("scrubbed:test-only-value:dumb:")
    );
    if std::env::var_os("HOME").is_some() {
        assert!(output.stdout.text.ends_with("present"));
    }
}

#[tokio::test]
async fn timeouts_allow_term_cleanup_and_report_timeout_even_when_exit_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let output = tools.bash().execute(json!({"command":"trap 'printf cleaned; exit 0' TERM; while :; do sleep 0.01; done","description":"Test graceful timeout","timeoutMs":100}),context(4096)).await.unwrap();
    assert!(!output.is_error);
    assert!(output.content.contains("timed out"));
    let output = result(output);
    assert!(output.timed_out);
    assert!(output.stdout.text.contains("cleaned"));
    assert_eq!(output.exit_code, Some(0));
}

#[tokio::test]
async fn timeout_cap_and_host_deadline_are_both_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.bash_grace = Duration::from_millis(20);
    config.bash_max_timeout = Duration::from_millis(30);
    let tools = LocalTools::with_config(config).unwrap();
    let output = tools
        .bash()
        .execute(
            json!({"command":"sleep 60","description":"Test timeout cap","timeoutMs":999999999}),
            context(4096),
        )
        .await
        .unwrap();
    assert!(result(output).timed_out);
    let tools = bundle(dir.path());
    let mut ctx = context(4096);
    ctx.deadline = tokio::time::Instant::now() + Duration::from_millis(30);
    assert!(result(bash(&tools, "sleep 60", ctx).await).timed_out);
}

#[tokio::test]
async fn separate_full_stream_logs_are_private_and_readable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    // Keep the default grace: logs are only finalized inside it, and a tight
    // grace would discard the logs instead of testing their contents.
    let tools = LocalTools::with_config(LocalToolConfig::new(dir.path())).unwrap();
    let output = bash(
        &tools,
        "for ((i=0;i<200;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done",
        context(512),
    )
    .await;
    assert!(output.truncated);
    assert!(output.content.len() <= 500);
    let output = result(output);
    let out_path = output.stdout.spill_path.unwrap();
    let err_path = output.stderr.spill_path.unwrap();
    assert_ne!(out_path, err_path);
    assert_eq!(
        std::fs::read_to_string(&out_path).unwrap(),
        "out\n".repeat(200)
    );
    assert_eq!(
        std::fs::read_to_string(&err_path).unwrap(),
        "err\n".repeat(200)
    );
    assert_eq!(
        std::fs::metadata(&out_path).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        std::fs::metadata(dir.path().join(".abycore"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let read = tools
        .read()
        .execute(json!({"file_path":out_path,"limit":2}), context(512))
        .await
        .unwrap();
    assert!(read.content.contains("1: out\n2: out"));
}

#[tokio::test]
async fn overflowing_spill_cap_discards_log_and_keeps_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.max_bash_log_bytes = 128;
    config.bash_output_bytes = 32;
    let tools = LocalTools::with_config(config).unwrap();
    let output = result(
        bash(
            &tools,
            "for ((i=0;i<100;i++)); do printf head; done; printf FINAL",
            context(4096),
        )
        .await,
    );
    assert!(output.stdout.truncated);
    assert!(output.stdout.text.ends_with("FINAL"));
    assert!(output.stdout.spill_path.is_none());
    if dir.path().join(".abycore").exists() {
        assert_eq!(
            std::fs::read_dir(dir.path().join(".abycore"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn minimum_output_budget_preserves_exit_information() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let output = bash(
        &tools,
        "for ((i=0;i<100;i++)); do printf '世界'; done; exit 9",
        context(64),
    )
    .await;
    assert!(output.content.len() <= 52);
    assert!(output.content.contains("exit code: 9"));
    assert_eq!(result(output).exit_code, Some(9));
}

#[tokio::test]
async fn logging_can_be_disabled_or_fail_without_failing_the_command() {
    for disabled in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = LocalToolConfig::new(dir.path());
        config.save_bash_output = !disabled;
        if !disabled {
            std::fs::write(dir.path().join(".abycore"), "occupied").unwrap();
        }
        let tools = LocalTools::with_config(config).unwrap();
        let output = bash(
            &tools,
            "for ((i=0;i<100;i++)); do printf value; done",
            context(64),
        )
        .await;
        assert!(!output.is_error);
        assert!(result(output).stdout.spill_path.is_none());
        if disabled {
            assert!(!dir.path().join(".abycore").exists());
        }
    }
}

#[tokio::test]
async fn background_jobs_outlive_tool_context_and_expose_incremental_output() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let ctx = context(512);
    let id = background(
        &tools,
        "printf first; sleep 0.1; printf second >&2",
        ctx.clone(),
    )
    .await;
    ctx.cancellation.cancel();
    until(|| {
        tools
            .job_output(&id, BashOutputCursor::default())
            .unwrap()
            .stdout
            == "first"
    })
    .await;
    let first = tools.job_output(&id, BashOutputCursor::default()).unwrap();
    let settled = tools.wait_job(&id).await.unwrap();
    assert_eq!(settled.status, BashJobStatus::Completed);
    assert_eq!(settled.result.unwrap().timeout_ms, None);
    let second = tools.job_output(&id, first.cursor).unwrap();
    assert!(second.stdout.is_empty());
    assert_eq!(format!("{}{}", first.stderr, second.stderr), "second");
    assert!(!second.lossy);
    assert_eq!(tools.jobs().len(), 1);
    tools.forget_job(&id).unwrap();
    assert!(tools.job(&id).is_err());
}

#[tokio::test]
async fn background_query_reports_eviction_and_rejects_invalid_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.bash_output_bytes = 16;
    let tools = LocalTools::with_config(config).unwrap();
    let id = background(
        &tools,
        "for ((i=0;i<100;i++)); do printf a; done",
        context(512),
    )
    .await;
    tools.wait_job(&id).await.unwrap();
    let output = tools.job_output(&id, BashOutputCursor::default()).unwrap();
    assert!(output.lossy);
    assert_eq!(output.stdout, "a".repeat(16));
    assert_eq!(output.cursor.stdout, 100);
    assert!(
        tools
            .job_output(
                &id,
                BashOutputCursor {
                    stdout: 101,
                    stderr: 0
                }
            )
            .is_err()
    );
    assert!(
        tools
            .job_output(&id, output.cursor)
            .unwrap()
            .stdout
            .is_empty()
    );
}

#[tokio::test]
async fn background_kill_is_idempotent_and_cleans_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let id = background(
        &tools,
        "sleep 60 & printf '%s' \"$!\" > pid; wait",
        context(512),
    )
    .await;
    until(|| dir.path().join("pid").exists()).await;
    let pid: i32 = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(tools.forget_job(&id).is_err());
    let settled = tools.kill_job(&id).await.unwrap();
    assert_eq!(settled.status, BashJobStatus::Cancelled);
    assert!(settled.result.unwrap().aborted);
    assert_eq!(
        tools.kill_job(&id).await.unwrap().status,
        BashJobStatus::Cancelled
    );
    until(|| !running(pid)).await;
}
fn running(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = text.rsplit_once(") ").and_then(|(_, s)| s.chars().next());
        !matches!(state, None | Some('Z' | 'X'))
    }
    #[cfg(not(target_os = "linux"))]
    {
        rustix::process::Pid::from_raw(pid)
            .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bash_permission_modes_are_enforced_by_descendant_sandbox() {
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = base.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let mut read_only = LocalToolConfig::new(&workspace);
    read_only.permission_mode = PermissionMode::ReadOnly;
    let read_only = LocalTools::with_config(read_only).unwrap();
    let blocked = workspace.join("read-only-blocked");
    let output = bash(
        &read_only,
        &format!("printf no > {}", blocked.display()),
        context(4096),
    )
    .await;
    let details = result(output.clone());
    assert!(details.sandbox_denied);
    assert_eq!(details.permission_mode, PermissionMode::ReadOnly);
    assert!(
        output
            .content
            .contains("file access denied under read-only mode")
    );
    assert!(!blocked.exists());

    let workspace_write = LocalTools::new(&workspace).unwrap();
    let inside = workspace.join("inside");
    assert_eq!(
        result(
            bash(
                &workspace_write,
                &format!("printf yes > {}", inside.display()),
                context(4096),
            )
            .await
        )
        .exit_code,
        Some(0)
    );
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "yes");
    let outside = base.path().join("outside");
    let denied = result(
        bash(
            &workspace_write,
            &format!("printf no > {}", outside.display()),
            context(4096),
        )
        .await,
    );
    assert!(denied.sandbox_denied);
    assert!(!outside.exists());

    let mut full = LocalToolConfig::new(&workspace);
    full.permission_mode = PermissionMode::FullAccess;
    let full = LocalTools::with_config(full).unwrap();
    let unrestricted = base.path().join("full-access");
    let allowed = result(
        bash(
            &full,
            &format!("printf full > {}", unrestricted.display()),
            context(4096),
        )
        .await,
    );
    assert_eq!(allowed.exit_code, Some(0));
    assert!(!allowed.sandbox_denied);
    assert_eq!(std::fs::read_to_string(unrestricted).unwrap(), "full");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn restricted_bash_blocks_metadata_changes_including_background_descendants() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let inside = workspace.join("inside");
    let outside = dir.path().join("outside");
    for path in [&inside, &outside] {
        std::fs::write(path, "audit fixture").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    for mode in [PermissionMode::ReadOnly, PermissionMode::WorkspaceWrite] {
        let mut config = LocalToolConfig::new(&workspace);
        config.permission_mode = mode;
        let tools = LocalTools::with_config(config).unwrap();
        for path in ["inside", "../outside"] {
            let original = std::fs::metadata(workspace.join(path)).unwrap();
            for command in [
                format!("bash -c 'chmod 000 {path}'"),
                format!("touch -m -t 200001010000 {path}"),
                format!("chown --reference={path} {path}"),
            ] {
                let denied = result(bash(&tools, &command, context(4096)).await);
                assert!(denied.sandbox_denied, "{command}: {denied:?}");
            }
            let actual = std::fs::metadata(workspace.join(path)).unwrap();
            assert_eq!(actual.permissions().mode(), original.permissions().mode());
            assert_eq!(actual.modified().unwrap(), original.modified().unwrap());
        }
        let id = background(&tools, "bash -c 'chmod 000 inside'", context(4096)).await;
        assert!(
            tools
                .wait_job(&id)
                .await
                .unwrap()
                .result
                .unwrap()
                .sandbox_denied
        );
        tools.shutdown().await;
    }
    let mut full = LocalToolConfig::new(&workspace);
    full.permission_mode = PermissionMode::FullAccess;
    let tools = LocalTools::with_config(full).unwrap();
    assert_eq!(
        result(bash(&tools, "chmod 640 inside", context(4096)).await).exit_code,
        Some(0)
    );
    assert_eq!(
        std::fs::metadata(&inside).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[tokio::test]
async fn job_limits_shutdown_and_bundle_drop_have_defined_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.max_background_jobs = 1;
    config.bash_grace = Duration::from_millis(20);
    let tools = LocalTools::with_config(config).unwrap();
    let id = background(&tools, "sleep 60", context(512)).await;
    assert!(
        tools
            .bash()
            .execute(
                json!({"command":"true","description":"exceed limit","run_in_background":true}),
                context(512)
            )
            .await
            .is_err()
    );
    tools.shutdown().await;
    assert_ne!(tools.job(&id).unwrap().status, BashJobStatus::Running);
    tools.forget_job(&id).unwrap();
    assert!(
        tools
            .bash()
            .execute(
                json!({"command":"true","description":"closed manager","run_in_background":true}),
                context(512)
            )
            .await
            .is_err()
    );
    let tools = bundle(dir.path());
    background(&tools, "printf '%s' \"$$\" > pid; sleep 60", context(512)).await;
    until(|| dir.path().join("pid").exists()).await;
    let pid = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    drop(tools);
    until(|| !running(pid)).await;
}

#[tokio::test]
async fn dropped_foreground_future_stops_its_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let tools = bundle(dir.path());
    let bash = tools.bash();
    let future = bash.execute(json!({"command":"sleep 60 & printf '%s' \"$!\" > pid; wait","description":"test drop cleanup"}),context(4096));
    let mut future = Box::pin(future);
    tokio::select! {
        _ = &mut future => panic!("command should still run"),
        _ = until(|| dir.path().join("pid").exists()) => {},
    }
    let pid = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    drop(future);
    until(|| !running(pid)).await;
}

#[test]
fn incremental_output_defers_incomplete_utf8() {
    let manager = jobs::Jobs::default();
    let (id, entry) = manager
        .insert("".into(), "".into(), PathBuf::new(), 1)
        .unwrap();
    entry.stdout.lock().unwrap().push(&[0xe4, 0xb8], 64);
    let first = manager.output(&id, BashOutputCursor::default()).unwrap();
    assert!(first.stdout.is_empty());
    assert_eq!(first.cursor.stdout, 0);
    entry.stdout.lock().unwrap().push(&[0xad], 64);
    let second = manager.output(&id, first.cursor).unwrap();
    assert_eq!(second.stdout, "中");
    assert_eq!(second.cursor.stdout, 3);
}

#[tokio::test]
async fn background_spawn_failure_settles_waiters_and_allows_forgetting() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.bash_path = dir.path().join("missing-bash");
    let tools = LocalTools::with_config(config).unwrap();
    let id = background(&tools, "true", context(512)).await;
    let job = tokio::time::timeout(Duration::from_secs(2), tools.wait_job(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.status, BashJobStatus::Failed);
    assert!(job.error.unwrap().contains("cannot start"));
    tools.forget_job(&id).unwrap();
    tools.shutdown().await;
}
