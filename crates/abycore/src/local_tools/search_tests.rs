use super::*;
use crate::local_tools::tests::context;
use crate::{ToolContext, ToolError, ToolOutput};
use serde_json::json;

async fn glob(
    tools: &LocalTools,
    pattern: &str,
    path: Option<&str>,
    ctx: &ToolContext,
) -> ToolOutput {
    let mut args = json!({"pattern": pattern});
    if let Some(path) = path {
        args["path"] = json!(path);
    }
    tools.glob().execute(args, ctx.clone()).await.unwrap()
}

async fn grep(
    tools: &LocalTools,
    pattern: &str,
    path: Option<&str>,
    include: Option<&str>,
    ctx: &ToolContext,
) -> ToolOutput {
    let mut args = json!({"pattern": pattern});
    if let Some(path) = path {
        args["path"] = json!(path);
    }
    if let Some(include) = include {
        args["include"] = json!(include);
    }
    tools.grep().execute(args, ctx.clone()).await.unwrap()
}

/// Newest-first ordering, basename patterns at any depth, hidden files
/// included, and VCS metadata pruned.
#[tokio::test]
async fn glob_orders_newest_first_and_matches_any_depth() {
    let dir = tempfile::tempdir().unwrap();
    // Oldest first, one sleep per file: the sort is newest-first with a path
    // tie-break, and file timestamps share the kernel's coarse tick — two
    // files written back-to-back used to tie and flip the last row from run
    // to run.
    std::fs::write(dir.path().join("a.txt"), "oldest\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(dir.path().join(".hidden.txt"), "hidden\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(dir.path().join("b.txt"), "newer\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::create_dir_all(dir.path().join("deep/nested")).unwrap();
    std::fs::write(dir.path().join("deep/nested/c.txt"), "newest\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/object.txt"), "vcs\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let output = glob(&tools, "*.txt", None, &ctx).await;
    let lines: Vec<&str> = output.content.lines().collect();
    assert_eq!(lines.first(), Some(&"deep/nested/c.txt"));
    assert_eq!(lines.last(), Some(&"a.txt"));
    assert!(lines.contains(&".hidden.txt"));
    assert!(!lines.iter().any(|line| line.contains(".git/")));
    assert_eq!(output.details.unwrap()["total"].as_u64().unwrap(), 4);
}

/// A pattern with a separator anchors the depth; the search root limits
/// scope; the root .gitignore excludes matches even under a whitelist pattern.
#[tokio::test]
async fn glob_anchors_path_shaped_patterns_and_respects_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(dir.path().join("src/nested/b.rs"), "").unwrap();
    std::fs::write(dir.path().join("lib/c.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/generated.rs"), "").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "generated.rs\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let anchored = glob(&tools, "src/**/*.rs", None, &ctx).await;
    let paths: Vec<&str> = anchored.content.lines().collect();
    assert_eq!(paths, ["src/nested/b.rs", "src/a.rs"]);
    let scoped = glob(&tools, "*.rs", Some("lib"), &ctx).await;
    assert_eq!(scoped.content.trim(), "lib/c.rs");
    let ignored = glob(&tools, "**/generated.rs", None, &ctx).await;
    assert_eq!(ignored.content, "No files found");
}

/// The inline cap truncates to a newest-first head with a total footer.
#[tokio::test]
async fn glob_caps_at_max_results() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..5 {
        std::fs::write(dir.path().join(format!("{index:02}.txt")), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut config = LocalToolConfig::new(dir.path());
    config.glob_max_results = 3;
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(8192);
    let output = glob(&tools, "*.txt", None, &ctx).await;
    assert!(output.truncated);
    assert!(output.content.contains("04.txt"));
    assert!(!output.content.contains("00.txt"));
    assert!(output.content.contains("(Showing 3 of 5 paths"));
    assert_eq!(output.details.unwrap()["total"].as_u64().unwrap(), 5);
}

/// Matches are grouped by file with 1-based line numbers and the found count.
#[tokio::test]
async fn grep_finds_and_groups_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("one.rs"), "alpha\nbeta alpha\ngamma\n").unwrap();
    std::fs::write(dir.path().join("two.rs"), "ALPHA\n").unwrap();
    std::fs::write(dir.path().join("other.txt"), "alpha\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let output = grep(&tools, "alpha", None, None, &ctx).await;
    assert_eq!(output.content.lines().next(), Some("Found 3 matches"));
    assert!(
        output
            .content
            .contains("one.rs\nLine 1: alpha\nLine 2: beta alpha")
    );
    // Search is case-sensitive by default, so two.rs's "ALPHA" never matches.
    assert!(!output.content.contains("two.rs"));
    assert!(output.details.unwrap()["total"].as_u64().unwrap() == 3);
    let single = grep(&tools, "alpha", Some("one.rs"), None, &ctx).await;
    assert_eq!(single.content.lines().next(), Some("Found 2 matches"));
}

/// The include filter admits only matching files; regex errors and malformed
/// filters are argument errors.
#[tokio::test]
async fn grep_include_filter_and_argument_validation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("code.rs"), "needle\n").unwrap();
    std::fs::write(dir.path().join("note.md"), "needle\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let filtered = grep(&tools, "needle", None, Some("*.rs"), &ctx).await;
    assert_eq!(filtered.content.lines().next(), Some("Found 1 match"));
    assert!(filtered.content.contains("code.rs"));
    assert!(!filtered.content.contains("note.md"));
    for include in ["!", "*.ts,*.md", "a{b,c},d", "  "] {
        let error = tools
            .grep()
            .execute(json!({"pattern":"x","include":include}), ctx.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Failed(_)));
    }
    let bad_regex = tools
        .grep()
        .execute(json!({"pattern":"[unclosed"}), ctx.clone())
        .await
        .unwrap_err();
    assert!(matches!(bad_regex, ToolError::Failed(_)));
}

/// Binary files (NUL before the match) are skipped; invalid UTF-8 lines get a
/// placeholder; a long line is preview-capped.
#[tokio::test]
async fn grep_skips_binary_and_caps_line_previews() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("binary.rs"), b"\0needle\n").unwrap();
    let mut long = "needle ".repeat(400);
    long.push_str("tail");
    std::fs::write(dir.path().join("long.txt"), &long).unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.grep_max_line_bytes = 32;
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(8192);
    let output = grep(&tools, "needle", None, None, &ctx).await;
    assert_eq!(output.content.lines().next(), Some("Found 1 match"));
    assert!(output.content.contains("long.txt"));
    assert!(output.content.contains("(line truncated)"));
    assert!(!output.content.contains("binary.rs"));
}

/// The inline match cap stops the walk and reports the limit.
#[tokio::test]
async fn grep_reports_limit_reached() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("many.txt"), "a\na\na\n").unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.grep_max_matches = 2;
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(8192);
    let output = grep(&tools, "a", None, None, &ctx).await;
    assert!(output.truncated);
    assert_eq!(
        output.content.lines().next(),
        Some("Found 2 matches (limit reached)")
    );
    assert!(output.content.contains("(Showing the first 2 matches;"));
    assert!(output.details.unwrap()["total"].as_u64().unwrap() == 3);
}

/// Pattern edge cases: empty patterns fail; `.` and `^` anchor per line; the
/// `.gitignore`-ignored and hidden-vcs files stay out of grep discovery.
#[tokio::test]
async fn grep_requires_pattern_and_respects_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("kept.txt"), "needle\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/ignore-me.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "skipped.txt\n").unwrap();
    std::fs::write(dir.path().join("skipped.txt"), "needle\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let output = grep(&tools, "needle", None, None, &ctx).await;
    assert!(output.content.contains("kept.txt"));
    assert!(!output.content.contains("skipped.txt"));
    assert!(!output.content.contains(".git/"));
    let empty = tools
        .grep()
        .execute(json!({"pattern":""}), ctx.clone())
        .await
        .unwrap_err();
    assert!(matches!(empty, ToolError::Failed(_)));
}

/// An explicit file operand obeys the host's file-size policy exactly like
/// the walk does, instead of searching past it.
#[tokio::test]
async fn grep_single_file_honors_the_file_size_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "needle here\n").unwrap();
    let mut config = LocalToolConfig::new(dir.path());
    config.max_file_bytes = Some(4);
    let tools = LocalTools::with_config(config).unwrap();
    let ctx = context(8192);
    let error = tools
        .grep()
        .execute(json!({"pattern":"needle","path":"big.txt"}), ctx.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_file_bytes"), "{error}");
    // The directory walk keeps skipping oversized files, as before.
    let output = grep(&tools, "needle", None, None, &ctx).await;
    assert!(
        output.content.contains("No matches found"),
        "{}",
        output.content
    );
}

/// A file whose line trips the searcher heap cap is skipped, but the result
/// says so: a silent skip reads as a definitive "No matches found".
#[tokio::test]
async fn grep_reports_files_it_could_not_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut line = String::with_capacity(17 * 1024 * 1024);
    line.push_str("needle ");
    line.push_str(&"a".repeat(17 * 1024 * 1024));
    line.push('\n');
    std::fs::write(dir.path().join("minified.js"), line).unwrap();
    std::fs::write(dir.path().join("small.txt"), "needle here\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let output = grep(&tools, "needle", None, None, &ctx).await;
    assert!(
        output.content.contains("Found 1 match"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("could not be searched"),
        "{}",
        output.content
    );
    assert_eq!(output.details.unwrap()["unsearched"], 1);
}

/// A leading `!` excludes, ripgrep's `--glob` semantics: `!*.txt` lists every
/// file except the txt ones instead of silently admitting nothing.
#[tokio::test]
async fn glob_leading_bang_excludes_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.rs"), "x\n").unwrap();
    std::fs::write(dir.path().join("drop.txt"), "x\n").unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let ctx = context(8192);
    let output = glob(&tools, "!*.txt", None, &ctx).await;
    assert!(output.content.contains("keep.rs"), "{}", output.content);
    assert!(!output.content.contains("drop.txt"), "{}", output.content);
}
