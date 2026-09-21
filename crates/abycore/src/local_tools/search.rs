//! In-process workspace search: path discovery (`glob`) and content search
//! (`grep`), the Rust ports of harness's `tool-fs-search` implemented the way
//! pi_agent_rust implements them: the ripgrep-family crates run in-process
//! (`ignore` for traversal, `grep-regex` for the matcher, `grep-searcher` for
//! line scanning with rg's default binary handling), so the model sees
//! harness-compatible behavior without spawning a binary.
use super::{
    LocalTools,
    workspace::{Operation, ToolResult, Workspace, failed, parse, run_filesystem},
};
use crate::{Result, Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Default inline cap on `glob` paths (pi's `DEFAULT_FIND_LIMIT`).
pub(super) const GLOB_MAX_RESULTS_DEFAULT: usize = 1000;
/// Safety bound on collected glob candidates before sorting; a pattern that
/// matches everything stops the walk here instead of buffering unboundedly.
pub(super) const GLOB_SCAN_LIMIT: usize = 20_000;
/// Default inline cap on `grep` matches (pi's `DEFAULT_GREP_LIMIT`).
pub(super) const GREP_MAX_MATCHES_DEFAULT: usize = 100;
/// Default per-line preview cap in bytes (harness `GREP_MAX_LINE_BYTES`).
pub(super) const GREP_MAX_LINE_BYTES_DEFAULT: usize = 2000;
/// Per-searcher heap cap so one giant minified line cannot spike memory; a
/// file that trips it is skipped and the walk continues.
const GREP_HEAP_LIMIT: usize = 16 * 1024 * 1024;
/// VCS metadata directories that never appear in discovery output, pruned
/// during traversal (harness `GLOB_VCS_EXCLUDES`).
const VCS_DIRECTORIES: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

#[derive(Clone)]
pub struct GlobTool {
    pub(super) workspace: Arc<Workspace>,
}
#[derive(Clone)]
pub struct GrepTool {
    pub(super) workspace: Arc<Workspace>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
}

/// Build the whitelist override matcher for one glob. Patterns with no path
/// separator match basenames at any depth (globset crosses separators), and
/// slash-anchored patterns resolve relative to the workspace root, so a
/// search rooted in a subdirectory keeps the same semantics as one rooted at
/// the workspace (rg's `--glob` from the workdir).
fn whitelist(workspace: &Workspace, pattern: &str) -> ToolResult<ignore::overrides::Override> {
    let mut overrides = ignore::overrides::OverrideBuilder::new(&workspace.config.root);
    overrides
        .add(pattern)
        .map_err(|error| failed(format!("invalid glob pattern: {error}")))?;
    overrides
        .build()
        .map_err(|error| failed(format!("invalid glob pattern: {error}")))
}

/// Whitelist plus the VCS metadata exclusions shared by both tools.
fn with_vcs_excludes(
    workspace: &Workspace,
    pattern: Option<&str>,
) -> ToolResult<ignore::overrides::Override> {
    let mut overrides = ignore::overrides::OverrideBuilder::new(&workspace.config.root);
    for name in VCS_DIRECTORIES {
        let _ = overrides.add(&format!("!**/{name}"));
        let _ = overrides.add(&format!("!**/{name}/**"));
    }
    if let Some(pattern) = pattern {
        overrides
            .add(pattern)
            .map_err(|error| failed(format!("invalid glob pattern: {error}")))?;
    }
    overrides
        .build()
        .map_err(|error| failed(format!("invalid glob pattern: {error}")))
}

/// Display path relative to the workspace root; external paths stay absolute.
fn display(workspace: &Workspace, path: &Path) -> String {
    match path.strip_prefix(&workspace.config.root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Cut one matched line to the preview cap on a UTF-8 boundary.
fn preview(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let mut end = max_bytes;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} (line truncated)", &line[..end])
}

/// One content match: file path, 1-based line number, previewed line text.
#[derive(Clone, serde::Serialize)]
struct Match {
    path: String,
    #[serde(rename = "lineNumber")]
    line_number: usize,
    line: String,
}

/// Group matches by file in first-seen order: each section is the display
/// path, then one `Line N: <text>` row per match (harness `formatGrepMatches`).
fn grouped(matches: &[Match]) -> String {
    let mut sections: Vec<String> = vec![];
    let mut lines: Vec<String> = vec![];
    let mut current: Option<&str> = None;
    for entry in matches {
        if current != Some(entry.path.as_str()) && !lines.is_empty() {
            if let Some(path) = current.take() {
                sections.push(format!("{path}\n{}", lines.join("\n")));
            }
            lines.clear();
        }
        current = Some(&entry.path);
        lines.push(format!("Line {}: {}", entry.line_number, entry.line));
    }
    if let Some(path) = current {
        sections.push(format!("{path}\n{}", lines.join("\n")));
    }
    sections.join("\n\n")
}

impl GlobTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.glob())
    }
    fn input(&self, value: &Value) -> ToolResult<GlobInput> {
        let input: GlobInput = parse(value)?;
        if input.pattern.trim().is_empty() {
            return Err(failed("pattern must be a non-empty string"));
        }
        if input
            .path
            .as_ref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(failed("path must be a non-empty string when given"));
        }
        Ok(input)
    }
}
impl GrepTool {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Ok(LocalTools::new(root)?.grep())
    }
    fn input(&self, value: &Value) -> ToolResult<GrepInput> {
        let input: GrepInput = parse(value)?;
        if input.pattern.is_empty() {
            return Err(failed("pattern must be a non-empty string"));
        }
        if input
            .path
            .as_ref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Err(failed("path must be a non-empty string when given"));
        }
        if let Some(include) = &input.include {
            if include.trim().is_empty() {
                return Err(failed("include must be a non-empty glob when given"));
            }
            if include.starts_with('!') {
                return Err(failed(
                    "include must be a positive glob filter; negated patterns (\"!\") are not supported",
                ));
            }
            let mut braces = 0usize;
            for character in include.chars() {
                match character {
                    '{' => braces += 1,
                    '}' => braces = braces.saturating_sub(1),
                    ',' if braces == 0 => {
                        return Err(failed(
                            "include must be one glob, not a comma-separated list (use {a,b} alternation instead)",
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(input)
    }
    fn matcher(&self, pattern: &str) -> ToolResult<grep_regex::RegexMatcher> {
        grep_regex::RegexMatcherBuilder::new()
            .build(pattern)
            .map_err(|error| failed(format!("invalid search pattern: {error}")))
    }
}

impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        let limit = self.workspace.config.glob_max_results;
        ToolDefinition {
            name: "glob".into(),
            description: format!(
                "Find files whose paths match a glob pattern. Returns matching file paths — \
                 never directories — newest first by modification time, up to {limit} paths. \
                 Respects .gitignore; hidden files are included but VCS metadata directories \
                 are excluded. A pattern with no \"/\" matches basenames at any depth; include \
                 a separator to anchor the depth."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1,
                        "description": "Glob pattern to match file paths against (e.g. \"**/*.rs\", \"src/**/*.test.js\"). A leading \"!\" excludes matches (e.g. \"!*.min.js\")."},
                    "path": {"type": "string",
                        "description": "Directory to search in. Defaults to the workspace; a relative path resolves against it."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        let input = self.input(value)?;
        whitelist(&self.workspace, &input.pattern).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            let overrides = with_vcs_excludes(&self.workspace, None)?;
            let pattern = whitelist(&self.workspace, &input.pattern)?;
            let negated = input.pattern.trim_start().starts_with('!');
            run_filesystem(
                self.workspace.clone(),
                context,
                input.path.clone().unwrap_or_else(|| ".".into()),
                false,
                move |workspace, root, operation| {
                    glob_search(
                        workspace,
                        root,
                        &overrides,
                        Some(&pattern),
                        negated,
                        operation,
                    )
                },
            )
            .await
        })
    }
}

impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        let matches = self.workspace.config.grep_max_matches;
        ToolDefinition {
            name: "grep".into(),
            description: format!(
                "Search file contents with a ripgrep-compatible regular expression. Returns \
                 matching lines with line numbers, grouped by file, up to {matches} matches; \
                 a larger result stops at the limit and says so. Respects .gitignore. Use read \
                 on a matched file for surrounding context."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1,
                        "description": "Regular expression to search for (ripgrep syntax)."},
                    "path": {"type": "string",
                        "description": "File or directory to search. Defaults to the workspace; a relative path resolves against it."},
                    "include": {"type": "string", "minLength": 1,
                        "description": "One glob filter for which files to search (e.g. \"*.rs\", \"*.{js,jsx}\"). Not a list; negation is not supported."}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }
    fn validate(&self, value: &Value) -> std::result::Result<(), ToolError> {
        let input = self.input(value)?;
        if let Some(include) = &input.include {
            whitelist(&self.workspace, include)?;
        }
        self.matcher(&input.pattern).map(|_| ())
    }
    fn execute<'a>(&'a self, value: Value, context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = self.input(&value)?;
            let matcher = self.matcher(&input.pattern)?;
            let include = match &input.include {
                Some(pattern) => Some(whitelist(&self.workspace, pattern)?),
                None => None,
            };
            run_filesystem(
                self.workspace.clone(),
                context,
                input.path.clone().unwrap_or_else(|| ".".into()),
                false,
                move |workspace, root, operation| {
                    grep_search(workspace, root, include.as_ref(), &matcher, operation)
                },
            )
            .await
        })
    }
}

/// Walk settings shared by both tools, mirroring pi's scan builder: hidden
/// files are listed, .gitignore is respected anywhere (no git requirement),
/// parent rules chain, and symlinks are never followed. The override matcher
/// adds the whitelist filter and prunes VCS metadata.
fn walk_builder(
    workspace: &Workspace,
    root: &Path,
    overrides: ignore::overrides::Override,
) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(workspace.absolute(root));
    builder.hidden(false);
    builder.parents(true);
    builder.require_git(false);
    builder.follow_links(false);
    builder.overrides(overrides);
    builder
}

/// `rg --files --sort=modified` semantics, adapted: the newest-first head of
/// the matched files, bounded by the inline cap and the scan safety limit.
/// Discovery filters (.gitignore, hidden, VCS) prune the walk natively; the
/// glob pattern itself is a post-filter, like pi's find backend. A leading
/// `!` follows ripgrep's `--glob` meaning (exclude what it matches) instead of
/// admitting nothing.
fn glob_search(
    workspace: &Workspace,
    root: &Path,
    overrides: &ignore::overrides::Override,
    pattern: Option<&ignore::overrides::Override>,
    negated: bool,
    operation: &Operation,
) -> ToolResult<ToolOutput> {
    let mut collected: Vec<(std::time::SystemTime, PathBuf)> = vec![];
    let mut total = 0usize;
    let mut unbounded = false;
    for entry in walk_builder(workspace, root, overrides.clone()).build() {
        operation.check()?;
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        let path = entry.into_path();
        if let Some(pattern) = pattern {
            let matched = pattern.matched(workspace_child(workspace, &path), false);
            let admitted = if negated {
                !matched.is_ignore()
            } else {
                matched.is_whitelist()
            };
            if !admitted {
                continue;
            }
        }
        total += 1;
        if collected.len() >= GLOB_SCAN_LIMIT {
            unbounded = true;
            break;
        }
        collected.push((modified, path));
    }
    collected.sort_by(|(left, left_path), (right, right_path)| {
        right.cmp(left).then_with(|| left_path.cmp(right_path))
    });
    let limit = workspace.config.glob_max_results;
    let shown: Vec<String> = collected[..collected.len().min(limit)]
        .iter()
        .map(|(_, path)| display(workspace, path))
        .collect();
    let footer = if shown.len() == total {
        String::new()
    } else if unbounded {
        format!(
            "\n\n(Showing {} of more than {total} paths — the walk stopped at its \
             {GLOB_SCAN_LIMIT}-entry safety limit. Narrow the pattern or path.)",
            shown.len()
        )
    } else {
        format!(
            "\n\n(Showing {} of {total} paths, newest first. Narrow the pattern or path to see more.)",
            shown.len()
        )
    };
    let content = if shown.is_empty() {
        "No files found".to_owned()
    } else {
        format!("{}\n{footer}", shown.join("\n"))
    };
    let truncated = shown.len() < total;
    operation.check()?;
    Ok(ToolOutput {
        content,
        is_error: false,
        truncated,
        details: Some(json!({
            "paths": shown[..shown.len().min(limit)],
            "total": total,
        })),
        meta: None,
    }
    .bounded(operation.output_limit))
}

/// pi's in-process grep backend: walk with discovery filters, scan each file
/// with a `grep-searcher` line sink (binary files quit at the first NUL byte,
/// like rg's default), retain matches up to the inline cap, and keep counting
/// until one match past the cap so overflow is detected exactly.
fn grep_search(
    workspace: &Workspace,
    root: &Path,
    include: Option<&ignore::overrides::Override>,
    matcher: &grep_regex::RegexMatcher,
    operation: &Operation,
) -> ToolResult<ToolOutput> {
    let absolute = workspace.absolute(root);
    let mut searcher = grep_searcher::SearcherBuilder::new()
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .heap_limit(Some(GREP_HEAP_LIMIT))
        .build();
    let cap = workspace.config.grep_max_matches;
    let limit = workspace.config.grep_max_line_bytes;
    let mut matches: Vec<Match> = vec![];
    let mut count = 0usize;
    let mut overflow = false;
    let mut unsearched = 0usize;
    if single_file(&absolute) {
        // An explicit file operand bypasses traversal; the include filter
        // still applies to its workspace-relative path (pi's single-file rule),
        // and so does the host's file-size policy that the walk enforces.
        if workspace.config.max_file_bytes.is_some_and(|max| {
            std::fs::metadata(&absolute).is_ok_and(|meta| meta.len() > max as u64)
        }) {
            return Err(failed("file exceeds the host max_file_bytes limit"));
        }
        operation.check()?;
        let admitted = include.as_ref().is_none_or(|filter| {
            filter
                .matched(workspace_child(workspace, &absolute), false)
                .is_whitelist()
        });
        if admitted {
            let mut file_overflow = false;
            let collector = Collector {
                path: display(workspace, &absolute),
                matches: &mut matches,
                count: &mut count,
                overflow: &mut file_overflow,
                cap,
                limit,
            };
            if let Err(error) = search_file(&mut searcher, matcher, &absolute, operation, collector)
            {
                // The scan stopped because the caller cancelled or the
                // deadline passed: surface that, not a search failure.
                if operation.interrupted() {
                    operation.check()?;
                }
                return Err(failed(format!(
                    "cannot search {}: {error}",
                    display(workspace, &absolute)
                )));
            }
            overflow = file_overflow;
        }
    } else {
        for entry in walk_builder(workspace, root, with_vcs_excludes(workspace, None)?).build() {
            operation.check()?;
            if overflow {
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.into_path();
            if workspace.config.max_file_bytes.is_some_and(|max| {
                std::fs::metadata(&path).is_ok_and(|meta| meta.len() > max as u64)
            }) {
                continue;
            }
            if let Some(filter) = include
                && !filter
                    .matched(workspace_child(workspace, &path), false)
                    .is_whitelist()
            {
                continue;
            }
            let mut file_overflow = false;
            let collector = Collector {
                path: display(workspace, &path),
                matches: &mut matches,
                count: &mut count,
                overflow: &mut file_overflow,
                cap,
                limit,
            };
            // An unreadable or binary-quitting file is skipped, like rg's
            // stderr warning: the search continues with the remaining files.
            // The skip is counted, because a missing file can mean the answer
            // is incomplete (a line over the searcher heap cap, for example).
            match search_file(&mut searcher, matcher, &path, operation, collector) {
                Ok(()) => overflow = file_overflow,
                Err(_) if operation.interrupted() => operation.check()?,
                Err(_) => unsearched += 1,
            }
        }
    }
    let truncated = overflow;
    let notice = if unsearched > 0 {
        format!(
            "\n\n({unsearched} file{} could not be searched; the results may be incomplete.)",
            if unsearched == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let content = if count == 0 {
        format!("No matches found{notice}")
    } else {
        let noun = if count == 1 { "match" } else { "matches" };
        let header = if truncated {
            format!("Found {} matches (limit reached)", matches.len())
        } else {
            format!("Found {count} {noun}")
        };
        let footer = if truncated {
            format!(
                "\n\n(Showing the first {} matches; narrow the pattern, path, or include to see more.)",
                matches.len()
            )
        } else {
            String::new()
        };
        format!("{header}\n\n{}\n{footer}{notice}", grouped(&matches))
    };
    operation.check()?;
    Ok(ToolOutput {
        content,
        is_error: false,
        truncated,
        details: Some(json!({
            "matches": matches,
            "total": count,
            "unsearched": unsearched,
        })),
        meta: None,
    }
    .bounded(operation.output_limit))
}

/// The workspace-relative path a workspace-rooted override matcher matches.
fn workspace_child(workspace: &Workspace, path: &Path) -> PathBuf {
    path.strip_prefix(&workspace.config.root)
        .unwrap_or(path)
        .to_path_buf()
}

fn single_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Scan one file through an interruptible reader. `search_path` opens its own
/// reader, so the only way to stop a long non-matching scan at cancellation or
/// the deadline is `search_reader` with a wrapper: the searcher checks nothing
/// between reads itself.
fn search_file(
    searcher: &mut grep_searcher::Searcher,
    matcher: &grep_regex::RegexMatcher,
    path: &Path,
    operation: &Operation,
    collector: Collector<'_>,
) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    searcher.search_reader(
        matcher,
        InterruptibleReader {
            inner: file,
            operation,
        },
        collector,
    )
}

/// A `Read` that fails once the tool call is cancelled or past its deadline.
/// The error kind is intentionally not `Interrupted`: the searcher retries
/// `Interrupted` reads in its own buffering loop, which would spin forever.
struct InterruptibleReader<'a, R> {
    inner: R,
    operation: &'a Operation,
}
impl<R: std::io::Read> std::io::Read for InterruptibleReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.operation.interrupted() {
            return Err(std::io::Error::other("local operation interrupted"));
        }
        self.inner.read(buffer)
    }
}

/// grep-searcher line sink: validate UTF-8 per matched line, cap the preview,
/// keep counting matched lines past the inline buffer, and stop the file's
/// search once the buffer is full.
struct Collector<'a> {
    path: String,
    matches: &'a mut Vec<Match>,
    count: &'a mut usize,
    overflow: &'a mut bool,
    cap: usize,
    limit: usize,
}
impl grep_searcher::Sink for Collector<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        *self.count += 1;
        if self.matches.len() >= self.cap {
            *self.overflow = true;
            return Ok(false);
        }
        // SinkMatch bytes include the line terminator.
        let text = match std::str::from_utf8(mat.bytes()) {
            Ok(text) => {
                let text = text.strip_suffix('\n').unwrap_or(text);
                text.strip_suffix('\r').unwrap_or(text).to_owned()
            }
            Err(_) => "(line is not valid UTF-8)".to_owned(),
        };
        self.matches.push(Match {
            path: self.path.clone(),
            line_number: mat
                .line_number()
                .and_then(|number| usize::try_from(number).ok())
                .unwrap_or(0),
            line: preview(&text, self.limit),
        });
        Ok(true)
    }
}
