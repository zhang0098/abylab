use super::*;
use crate::{CancellationToken, ToolContext, ToolError, context::RequestContext};
use serde_json::json;
use std::sync::Mutex;

pub(super) fn context(output_limit: usize) -> ToolContext {
    let cancellation = CancellationToken::new();
    let request =
        RequestContext::new(cancellation.clone(), None, 16, Arc::new(Mutex::new(vec![]))).unwrap();
    ToolContext {
        call_id: "local-test".into(),
        cancellation,
        deadline: tokio::time::Instant::now() + Duration::from_secs(30),
        max_output_bytes: output_limit,
        request,
        local_session: Arc::default(),
        parent: None,
    }
}
async fn read(tools: &LocalTools, path: &str, ctx: &ToolContext) -> crate::ToolOutput {
    tools
        .read()
        .execute(json!({"file_path":path}), ctx.clone())
        .await
        .unwrap()
}
async fn write(
    tools: &LocalTools,
    path: &str,
    content: &str,
    ctx: &ToolContext,
) -> crate::ToolOutput {
    tools
        .write()
        .execute(json!({"file_path":path,"content":content}), ctx.clone())
        .await
        .unwrap()
}
async fn edit(
    tools: &LocalTools,
    path: &str,
    old: &str,
    new: &str,
    all: bool,
    ctx: &ToolContext,
) -> std::result::Result<crate::ToolOutput, ToolError> {
    tools
        .edit()
        .execute(
            json!({"file_path":path,"old_string":old,"new_string":new,"replace_all":all}),
            ctx.clone(),
        )
        .await
}

#[tokio::test]
async fn create_read_edit_overwrite_and_empty_files() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    assert!(
        write(&tools, "nested/a", "one\ntwo\nthree\n", &ctx)
            .await
            .content
            .contains("Created file")
    );
    let page = tools
        .read()
        .execute(
            json!({"file_path":"nested/a","offset":2,"limit":1}),
            ctx.clone(),
        )
        .await
        .unwrap();
    assert!(page.content.contains("2: two"));
    assert!(page.content.contains("offset=3"));
    let output = edit(&tools, "nested/a", "two", "second", false, &ctx)
        .await
        .unwrap();
    assert!(!output.content.contains("-two"));
    assert!(
        output.details.unwrap()["diffs"][0]["newText"]
            .as_str()
            .unwrap()
            .contains("second")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested/a")).unwrap(),
        "one\nsecond\nthree\n"
    );
    assert!(
        write(&tools, "nested/a", "", &ctx)
            .await
            .content
            .contains("Updated file")
    );
    assert!(
        read(&tools, "nested/a", &ctx)
            .await
            .content
            .contains("total 0 lines")
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("nested"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn validates_harness_argument_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    for value in [
        json!({"path":"a"}),
        json!({"file_path":"a","offset":0}),
        json!({"file_path":"a","limit":2001}),
        json!({"file_path":"a","limit":null}),
        json!({"file_path":"a","offset":1.5}),
        json!({"file_path":"a","extra":true}),
    ] {
        assert!(tools.read().validate(&value).is_err(), "{value}");
    }
    for value in [
        json!({"file_path":"a"}),
        json!({"file_path":"a","content":12}),
        json!({"path":"a","content":""}),
    ] {
        assert!(tools.write().validate(&value).is_err());
    }
    for value in [
        json!({"file_path":"a","oldText":"x","newText":"y"}),
        json!({"file_path":"a","old_string":"","new_string":"y"}),
        json!({"file_path":"a","old_string":"x","new_string":"x"}),
        json!({"file_path":"a","old_string":"x","new_string":"y","replace_all":null}),
    ] {
        assert!(tools.edit().validate(&value).is_err());
    }
    for value in [
        json!({"command":"true"}),
        json!({"command":"true","description":""}),
        json!({"command":"","description":"test"}),
        json!({"command":"true","description":"test","timeoutMs":0}),
        json!({"command":"true","description":"test","timeout":1}),
        json!({"command":"true","description":"test","timeoutMs":null}),
    ] {
        assert!(tools.bash().validate(&value).is_err());
    }
    assert!(tools.bash().validate(&json!({"command":"true","description":"test","timeoutMs":999999999,"run_in_background":true})).is_ok());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn guards_unseen_and_stale_files_before_write_or_literal_matching() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), "original").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"a","content":"blind"}), ctx.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_NOT_OBSERVED")
    );
    assert!(
        edit(&tools, "a", "original", "changed", false, &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_NOT_OBSERVED")
    );
    read(&tools, "a", &ctx).await;
    // Like an editor's atomic save, guarantee a distinct inode even on coarse timestamp filesystems.
    std::fs::write(dir.path().join("external-save"), "external").unwrap();
    std::fs::rename(dir.path().join("external-save"), dir.path().join("a")).unwrap();
    assert!(
        edit(&tools, "a", "original", "changed", false, &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_STALE_VERSION")
    );
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"a","content":"blind"}), ctx.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_STALE_VERSION")
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "external"
    );
    read(&tools, "a", &ctx).await;
    edit(&tools, "a", "external", "updated", false, &ctx)
        .await
        .unwrap();
}

#[tokio::test]
async fn observations_are_per_session_even_with_one_shared_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let first = context(4096);
    let second = context(4096);
    write(&tools, "a", "old", &first).await;
    assert!(
        edit(&tools, "a", "old", "new", false, &second)
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_NOT_OBSERVED")
    );
    read(&tools, "a", &second).await;
    edit(&tools, "a", "old", "new", false, &first)
        .await
        .unwrap();
    assert!(
        edit(&tools, "a", "old", "different", false, &second)
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_STALE_VERSION")
    );
}

#[tokio::test]
async fn an_absence_observation_does_not_authorize_later_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    assert!(
        tools
            .read()
            .execute(json!({"file_path":"a"}), ctx.clone())
            .await
            .is_err()
    );
    std::fs::write(dir.path().join("a"), "external").unwrap();
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"a","content":"bad"}), ctx.clone())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "external"
    );
}

#[tokio::test]
async fn literal_matching_replace_all_and_nonoverlapping_occurrences_match_harness() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "alpha alpha", &ctx).await;
    assert!(
        edit(&tools, "a", "alpha", "beta", false, &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("FS_AMBIGUOUS_EDIT")
    );
    let output = edit(&tools, "a", "alpha", "beta", true, &ctx)
        .await
        .unwrap();
    assert_eq!(output.details.unwrap()["replacements"], 2);
    write(&tools, "a", "aaa", &ctx).await;
    edit(&tools, "a", "aa", "x", false, &ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("a")).unwrap(), "xa");
    edit(&tools, "a", "x", "", false, &ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("a")).unwrap(), "a");
    for (source, old) in [
        ("“yes”", "\"yes\""),
        ("cafe\u{301}", "café"),
        ("hello  \n", "hello\n"),
    ] {
        write(&tools, "a", source, &ctx).await;
        assert!(
            edit(&tools, "a", old, "changed", false, &ctx)
                .await
                .unwrap_err()
                .to_string()
                .contains("FS_EDIT_NOT_FOUND")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a")).unwrap(),
            source
        );
    }
}

#[tokio::test]
async fn edits_use_harness_bom_and_line_ending_normalization() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "\u{feff}one\r\ntwo\r\nthree\n", &ctx).await;
    edit(&tools, "a", "two\n", "changed\n", false, &ctx)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "one\r\nchanged\r\nthree\r\n"
    );
}

#[tokio::test]
async fn read_streams_large_files_and_caps_giant_lines() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mut file = std::fs::File::create(dir.path().join("large")).unwrap();
    for _ in 0..(17 * 128) {
        file.write_all(&[b'x'; 8192]).unwrap();
    }
    file.write_all(b"\nlast\n").unwrap();
    drop(file);
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    let output = read(&tools, "large", &ctx).await;
    assert!(output.content.contains("line truncated to 2000 chars"));
    assert!(output.content.contains("2: last"));
    assert_eq!(output.details.unwrap()["totalLines"], 2);
}

#[tokio::test]
async fn utf8_split_at_chunk_boundary_and_small_budget_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let content = format!("{}中\nsecond\nthird\n", "a".repeat(8191));
    std::fs::write(dir.path().join("a"), content).unwrap();
    let ctx = context(64);
    let output = tools
        .read()
        .execute(json!({"file_path":"a","offset":2,"limit":1}), ctx.clone())
        .await
        .unwrap();
    assert!(output.content.contains("2: second"), "{}", output.content);
    assert!(output.content.contains("offset=3"));
    assert!(output.content.len() <= 52);
    assert!(
        tools
            .read()
            .execute(json!({"file_path":"a","offset":4}), ctx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_utf8_binary_and_configured_file_caps_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.max_file_bytes = Some(16);
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(4096);
    for bytes in [vec![0xff], vec![b'a', 0], vec![0xe4, 0xb8], vec![b'x'; 17]] {
        std::fs::write(dir.path().join("a"), bytes).unwrap();
        assert!(
            tools
                .read()
                .execute(json!({"file_path":"a"}), ctx.clone())
                .await
                .is_err()
        );
    }
    assert!(
        tools
            .write()
            .validate(&json!({"file_path":"a","content":"x".repeat(17)}))
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn byte_caps_and_diff_caps_do_not_break_file_edits() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.max_read_bytes = 10;
    config.max_diff_bytes = 8;
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "hello\nworld\n", &ctx).await;
    let page = read(&tools, "a", &ctx).await;
    assert!(page.content.contains("1: hello"));
    assert!(!page.content.contains("2: world"));
    assert!(page.content.contains("offset=2"));
    let edited = edit(&tools, "a", "hello", "changed", false, &ctx)
        .await
        .unwrap();
    assert_eq!(edited.details.unwrap()["diffOmitted"], true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "changed\nworld\n"
    );
}

/// A first line larger than the whole read budget must not produce a footer
/// that points back at the same offset: the window emits a bounded preview
/// and the continuation moves past the line (a self-referential offset loops
/// a caller that follows it).
#[tokio::test]
async fn an_oversized_first_line_never_points_back_at_itself() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(4096);
    let text = format!("{}\nsecond\n", "a".repeat(64));

    let mut config = LocalToolConfig::new(dir.path());
    config.max_read_bytes = 24;
    let tools = LocalTools::with_config(config).unwrap();
    write(&tools, "a", &text, &ctx).await;
    let page = read(&tools, "a", &ctx).await;
    assert!(page.content.contains("1: a"), "{}", page.content);
    assert!(
        page.content.contains("offset=2"),
        "the continuation moves past the oversized line: {}",
        page.content
    );
    assert!(!page.content.contains("offset=1"), "{}", page.content);
    assert!(page.truncated, "{}", page.content);

    // A budget too small even for a preview still moves the caller on.
    let mut config = LocalToolConfig::new(dir.path());
    config.max_read_bytes = 1;
    let tools = LocalTools::with_config(config).unwrap();
    let page = read(&tools, "a", &ctx).await;
    assert!(page.content.contains("use offset=2"), "{}", page.content);
}

/// A pair that differs only in line endings normalizes to the same text: it
/// must be rejected up front, not reported as a successful no-op.
#[tokio::test]
async fn an_edit_that_only_changes_line_endings_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "a\r\nb\n", &ctx).await;
    let error = edit(&tools, "a", "a\r\n", "a\n", false, &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("differ"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "a\r\nb\n"
    );
}

/// Literal matching buffers the whole file, so `edit` refuses a file over the
/// host's `max_file_bytes` cap instead of growing without bound (a built-in
/// ceiling applies when the host sets none).
#[tokio::test]
async fn edit_refuses_a_file_over_the_host_file_cap() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(4096);
    let lenient = LocalTools::new(dir.path()).unwrap();
    write(&lenient, "a", "0123456789abcdefg", &ctx).await; // 17 bytes
    let mut config = LocalToolConfig::new(dir.path());
    config.max_file_bytes = Some(16);
    let strict = LocalTools::with_config(config).unwrap();
    let error = edit(&strict, "a", "0", "x", false, &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("max_file_bytes"), "{error}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "0123456789abcdefg"
    );
}

#[tokio::test]
async fn cancelled_filesystem_operations_have_no_new_effects() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    ctx.cancellation.cancel();
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"nested/a","content":"no"}), ctx)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn canonical_paths_share_guards_and_workspace_write_is_contained() {
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = base.path().join("workspace");
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let tools = LocalTools::new(&workspace).unwrap();
    let ctx = context(4096);
    for path in [
        outside.join("a").to_string_lossy().into_owned(),
        "../outside".into(),
        "missing/../../outside".into(),
    ] {
        assert!(
            tools
                .write()
                .execute(json!({"file_path":path,"content":"no"}), ctx.clone())
                .await
                .is_err()
        );
    }
    std::fs::create_dir(workspace.join("sub")).unwrap();
    write(&tools, "a", "old", &ctx).await;
    edit(&tools, "sub/../a", "old", "new", false, &ctx)
        .await
        .unwrap();
    assert!(
        tools
            .read()
            .execute(json!({"file_path":workspace.join("a")}), ctx)
            .await
            .unwrap()
            .content
            .contains("1: new")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn internal_symlinks_preserve_links_and_external_writes_are_rejected() {
    use std::os::unix::fs::symlink;
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = base.path().join("workspace");
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let tools = LocalTools::new(&workspace).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "old", &ctx).await;
    symlink("a", workspace.join("alias")).unwrap();
    read(&tools, "alias", &ctx).await;
    edit(&tools, "alias", "old", "new", false, &ctx)
        .await
        .unwrap();
    assert!(
        std::fs::symlink_metadata(workspace.join("alias"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(workspace.join("a")).unwrap(), "new");
    symlink(workspace.join("a"), workspace.join("absolute-alias")).unwrap();
    edit(&tools, "absolute-alias", "new", "absolute", false, &ctx)
        .await
        .unwrap();
    assert!(
        std::fs::symlink_metadata(workspace.join("absolute-alias"))
            .unwrap()
            .is_symlink()
    );
    symlink("missing", workspace.join("dangling")).unwrap();
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"dangling","content":"no"}), ctx.clone())
            .await
            .is_err()
    );
    symlink("loop", workspace.join("loop")).unwrap();
    assert!(
        tools
            .read()
            .execute(json!({"file_path":"loop"}), ctx.clone())
            .await
            .is_err()
    );
    symlink(&outside, workspace.join("escape")).unwrap();
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"escape/a","content":"no"}), ctx)
            .await
            .is_err()
    );
    assert!(!outside.join("a").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn private_new_files_preserved_permissions_and_special_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(4096);
    write(&tools, "a", "old", &ctx).await;
    assert_eq!(
        std::fs::metadata(dir.path().join("a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    std::fs::set_permissions(dir.path().join("a"), std::fs::Permissions::from_mode(0o751)).unwrap();
    read(&tools, "a", &ctx).await;
    edit(&tools, "a", "old", "new", false, &ctx).await.unwrap();
    assert_eq!(
        std::fs::metadata(dir.path().join("a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    rustix::fs::mknodat(
        &tools.workspace.directory,
        tools.workspace.workspace_child("pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR,
        0,
    )
    .unwrap();
    assert!(
        tools
            .read()
            .execute(json!({"file_path":"pipe"}), ctx.clone())
            .await
            .is_err()
    );
    assert!(
        tools
            .write()
            .execute(json!({"file_path":"pipe","content":"no"}), ctx)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_sessions_cannot_both_commit_based_on_one_version() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let first = context(4096);
    let second = context(4096);
    write(&tools, "a", "old", &first).await;
    read(&tools, "a", &second).await;
    let (a, b) = tokio::join!(
        edit(&tools, "a", "old", "first", false, &first),
        edit(&tools, "a", "old", "second", false, &second)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn atomic_create_rechecks_absence_without_clobbering_or_leaking_staging() {
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a"), "external").unwrap();
    let ctx = context(4096);
    let operation = workspace::Operation {
        cancellation: ctx.cancellation,
        deadline: ctx.deadline,
        output_limit: 4096,
        session: ctx.local_session,
    };
    assert!(
        workspace::replace(
            &tools.workspace,
            &tools.workspace.workspace_child("a"),
            "bad",
            None,
            &operation
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a")).unwrap(),
        "external"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn an_aliased_root_accepts_its_original_absolute_paths() {
    let parent = tempfile::tempdir().unwrap();
    let actual = parent.path().join("actual");
    let alias = parent.path().join("alias");
    std::fs::create_dir(&actual).unwrap();
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let tools = LocalTools::new(&alias).unwrap();
    let ctx = context(4096);
    write(&tools, alias.join("a").to_str().unwrap(), "old", &ctx).await;
    std::os::unix::fs::symlink(alias.join("a"), actual.join("link")).unwrap();
    edit(&tools, "link", "old", "new", false, &ctx)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(actual.join("a")).unwrap(), "new");
}

#[cfg(unix)]
#[tokio::test]
async fn permission_modes_control_file_effects_and_keep_ambient_reads() {
    let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = base.path().join("workspace");
    let outside = base.path().join("outside.txt");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(&outside, "ambient").unwrap();

    let mut read_only = LocalToolConfig::new(&workspace);
    read_only.permission_mode = PermissionMode::ReadOnly;
    let read_only = LocalTools::with_config(read_only).unwrap();
    assert_eq!(read_only.permission_mode(), PermissionMode::ReadOnly);
    assert!(
        read_only
            .read()
            .execute(json!({"file_path":outside}), context(4096))
            .await
            .unwrap()
            .content
            .contains("1: ambient")
    );
    let denied = read_only
        .write()
        .execute(
            json!({"file_path":"blocked.txt","content":"no"}),
            context(4096),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("FS_SANDBOX_DENIED"));
    assert!(!workspace.join("blocked.txt").exists());

    let workspace_write = LocalTools::new(&workspace).unwrap();
    assert_eq!(
        workspace_write.permission_mode(),
        PermissionMode::WorkspaceWrite
    );
    let outside_workspace = base.path().join("denied.txt");
    let denied = workspace_write
        .write()
        .execute(
            json!({"file_path":outside_workspace,"content":"no"}),
            context(4096),
        )
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("workspace-write mode"));
    let temporary = tempfile::tempdir().unwrap();
    workspace_write
        .write()
        .execute(
            json!({"file_path":temporary.path().join("allowed.txt"),"content":"yes"}),
            context(4096),
        )
        .await
        .unwrap();

    let mut full = LocalToolConfig::new(&workspace);
    full.permission_mode = PermissionMode::FullAccess;
    let full = LocalTools::with_config(full).unwrap();
    let allowed = base.path().join("allowed.txt");
    full.write()
        .execute(json!({"file_path":allowed,"content":"full"}), context(4096))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(allowed).unwrap(), "full");
}
