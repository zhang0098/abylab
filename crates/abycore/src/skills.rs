//! User-authored skills: markdown instruction documents the host discovers and
//! hands to the model on demand.
//!
//! One skill is either `<workspace>/.agents/skills/<name>/SKILL.md` or
//! `<workspace>/.agents/skills/<name>.md`. The workspace is the only root:
//! skills are checked in with the project, they travel with it, and they never
//! leak between projects.
//!
//! Frontmatter is optional and carries `name`, `description` and an optional
//! `input-hint`; anything else in the body is the instruction text. Nothing
//! here executes: a skill is text the host injects when the user types
//! `/<name>`, or text the model reads on demand through [`SkillTool`].

use crate::{Tool, ToolContext, ToolDefinition, ToolError, ToolFuture, ToolOutput};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Directory under the workspace that holds its skills.
pub const SKILLS_DIR: &str = ".agents/skills";
/// Filename of a directory-form skill.
pub const SKILL_FILENAME: &str = "SKILL.md";
/// Markdown bytes one skill may carry, frontmatter excluded — the same 64 KiB
/// the workspace instruction baseline gets. A skill is injected once per
/// invocation rather than every turn, so it may be a long document; an
/// oversized body is truncated (with a visible marker and a warning) instead of
/// skipped, because a half skill still beats a skill that silently vanished
/// from the catalog.
pub const MAX_SKILL_BYTES: usize = 64 * 1024;
/// Skills discovered per session; later files are skipped with a warning.
pub const MAX_SKILLS: usize = 128;
/// Skills named in the host context index (the tool still lists every one).
pub const MAX_INDEX_SKILLS: usize = 32;
/// Longest name a skill may carry.
const MAX_NAME_BYTES: usize = 64;
/// Longest description rendered in a UI listing and the index.
const MAX_DESCRIPTION_CHARS: usize = 200;

/// One discovered skill: the metadata the UI shows and the body the model runs
/// against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Frontmatter `input-hint`: the argument placeholder `/name <hint>` shows.
    pub input_hint: Option<String>,
    pub body: String,
    /// Absolute path of the skill file.
    pub path: PathBuf,
}

impl Skill {
    /// The skill's directory — the anchor for supporting files it names.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// One parsed `/<name> [args]` line that named a discovered skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillInvocation<'a> {
    pub skill: &'a Skill,
    /// The command line as typed, trimmed.
    pub line: String,
    /// Everything after the name, trimmed; empty when the user passed none.
    pub args: String,
}

impl SkillInvocation<'_> {
    /// The model-facing user message: the typed line, a frame that says what
    /// follows, then the body verbatim. Keeping the line first leaves the
    /// transcript (and the derived session title) readable.
    pub fn render(&self) -> String {
        let skill = self.skill;
        let args = if self.args.is_empty() {
            String::new()
        } else {
            " Text after the command on the first line is the user's argument to it.".to_string()
        };
        format!(
            "{line}\n\n\
             <system-reminder>\n\
             The user invoked the skill \"{name}\" ({path}). Its instructions follow and apply to \
             this turn; supporting files it names resolve relative to {dir}.{args}\n\
             The skill does not override system, developer, or direct user instructions.\n\
             </system-reminder>\n\n\
             {body}",
            line = self.line,
            name = skill.name,
            path = skill.path.display(),
            dir = skill.dir().display(),
            body = skill.body,
        )
    }
}

/// The discovered skills of one session, plus the nonfatal diagnostics
/// discovery produced (unreadable files, invalid names, duplicates).
#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    warnings: Vec<String>,
}

impl SkillCatalog {
    /// Discover the session's skills from the workspace's `.agents/skills`.
    /// A missing directory is normal; a duplicate name keeps the first file in
    /// path order and warns.
    pub fn discover(workspace: &Path) -> Self {
        let mut catalog = Self::default();
        catalog.scan(&workspace.join(SKILLS_DIR));
        catalog.skills.sort_by(|a, b| a.name.cmp(&b.name));
        catalog
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// Parse a composer line. `/<name>` and `/<name> <args>` resolve; any other
    /// line — including one naming no known skill — is `None`, so the caller
    /// ships it unchanged.
    pub fn invocation<'a>(&'a self, line: &'a str) -> Option<SkillInvocation<'a>> {
        let line = line.trim();
        let rest = line.strip_prefix('/')?;
        // `/` alone and `//…` are not names; neither is a path like `/usr/bin`.
        if rest.is_empty() || rest.starts_with('/') {
            return None;
        }
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (rest, ""),
        };
        let skill = self.get(name)?;
        Some(SkillInvocation {
            skill,
            line: line.to_string(),
            args: args.to_string(),
        })
    }

    /// The model-facing user message for a skill line, if it names one.
    pub fn expand(&self, line: &str) -> Option<String> {
        Some(self.invocation(line)?.render())
    }

    /// The host context index: which skills this session has. Stable for the
    /// session's lifetime, so it can ride `AgentHooks::request_context`.
    pub fn index_block(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let listed = self.skills.iter().take(MAX_INDEX_SKILLS);
        let mut block = String::from(
            "<system-reminder>\n\
             Skills available in this session. The user invokes one with /&lt;name&gt;; the \
             `skill` tool reads one on demand and lists them all when called without a name.\n",
        );
        for skill in listed {
            block.push_str(&format!("- {} · {}\n", skill.name, skill.description));
        }
        let hidden = self.skills.len().saturating_sub(MAX_INDEX_SKILLS);
        if hidden > 0 {
            block.push_str(&format!(
                "- … and {hidden} more — call the `skill` tool with no name to list them.\n"
            ));
        }
        block.push_str("</system-reminder>");
        Some(block)
    }

    /// This catalog as the model-facing tool that reads one skill on demand.
    pub fn tool(self: &Arc<Self>) -> SkillTool {
        SkillTool::new(Arc::clone(self))
    }

    fn scan(&mut self, root: &Path) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            // No skills directory is the normal case, not a diagnostic.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return,
            Err(err) => {
                self.warnings
                    .push(format!("cannot read {}: {err}", root.display()));
                return;
            }
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if self.skills.len() >= MAX_SKILLS {
                self.warnings.push(format!(
                    "more than {MAX_SKILLS} skills found — the rest of {} was ignored",
                    root.display()
                ));
                break;
            }
            let (entry, file) = if path.is_dir() {
                (path.clone(), path.join(SKILL_FILENAME))
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                (path.clone(), path)
            } else {
                continue;
            };
            match std::fs::read_to_string(&file) {
                // A directory without a SKILL.md is not a skill.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    self.warnings
                        .push(format!("cannot read {}: {err}", file.display()));
                    continue;
                }
                Ok(text) => {
                    if let Some(skill) = self.parse(&entry, &file, &text) {
                        self.insert(skill);
                    }
                }
            }
        }
    }

    fn parse(&mut self, entry: &Path, file: &Path, text: &str) -> Option<Skill> {
        let (fields, body) = split_frontmatter(text);
        let name = fields
            .get("name")
            .cloned()
            .unwrap_or_else(|| default_name(entry));
        if !valid_name(&name) {
            self.warnings.push(format!(
                "{}: `{name}` is not a valid skill name — use 1-{MAX_NAME_BYTES} characters of \
                 [A-Za-z0-9-_], starting with a letter or digit",
                file.display()
            ));
            return None;
        }
        let body = body.trim();
        // Over the cap the body is cut at a char boundary and marked, so the
        // model knows the instructions may stop short of the file.
        let (body, oversized) = match body.len() > MAX_SKILL_BYTES {
            true => (
                format!(
                    "{}\n\n[Truncated: the skill file is {} bytes; this session keeps the first \
                     {MAX_SKILL_BYTES}.]",
                    truncate_bytes(body, MAX_SKILL_BYTES),
                    body.len()
                ),
                true,
            ),
            false => (body.to_string(), false),
        };
        if oversized {
            self.warnings.push(format!(
                "{}: skill `{name}` is over the {MAX_SKILL_BYTES}-byte limit — truncated to the \
                 first {MAX_SKILL_BYTES} bytes",
                file.display()
            ));
        }
        let description = fields
            .get("description")
            .cloned()
            .unwrap_or_else(|| first_line(body.as_str()))
            .trim()
            .to_string();
        let input_hint = fields
            .get("input-hint")
            .map(|hint| hint.trim())
            .filter(|hint| !hint.is_empty())
            .map(|hint| truncate(hint, MAX_DESCRIPTION_CHARS));
        Some(Skill {
            name,
            description: truncate(&description, MAX_DESCRIPTION_CHARS),
            input_hint,
            body,
            path: file.to_path_buf(),
        })
    }

    fn insert(&mut self, skill: Skill) {
        match self
            .skills
            .iter_mut()
            .find(|existing| existing.name == skill.name)
        {
            // One directory, one name: the first file in path order wins.
            Some(existing) => self.warnings.push(format!(
                "duplicate skill `{}` — {} ignored, {} wins",
                skill.name,
                skill.path.display(),
                existing.path.display()
            )),
            None => self.skills.push(skill),
        }
    }
}

/// `skill`: read one skill's instructions, or list what this session has.
///
/// The catalog is discovered once per session, so the tool serves a fixed
/// snapshot — the same one the host injects and the UI lists.
pub struct SkillTool {
    catalog: Arc<SkillCatalog>,
}

impl SkillTool {
    pub const NAME: &'static str = "skill";

    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self { catalog }
    }
}

impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.into(),
            description: "Read a skill: a markdown instruction document for a specific task. \
                          Call with `name` to load that skill's instructions, or with no \
                          arguments to list the skills available in this session."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Skill to read; omit to list the available ones."
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        let Some(object) = arguments.as_object() else {
            return Err(ToolError::Failed("skill takes an object".into()));
        };
        if object.keys().any(|key| key != "name") {
            return Err(ToolError::Failed("skill takes only `name`".into()));
        }
        match object.get("name") {
            None | Some(Value::Null) => Ok(()),
            Some(Value::String(name)) if !name.trim().is_empty() => Ok(()),
            Some(_) => Err(ToolError::Failed(
                "`name` must be a non-empty string".into(),
            )),
        }
    }

    fn execute<'a>(&'a self, arguments: Value, _: ToolContext) -> ToolFuture<'a> {
        let requested = arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            if requested.is_empty() {
                if self.catalog.is_empty() {
                    return Ok(ToolOutput::text("No skills are available in this session."));
                }
                let mut listing = String::from("Available skills:\n");
                for skill in self.catalog.skills() {
                    listing.push_str(&format!("- {} · {}\n", skill.name, skill.description));
                }
                return Ok(ToolOutput::text(listing));
            }
            let Some(skill) = self.catalog.get(&requested) else {
                let names = self
                    .catalog
                    .skills()
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ToolError::Failed(if names.is_empty() {
                    format!("unknown skill `{requested}` — this session has no skills")
                } else {
                    format!("unknown skill `{requested}` — available: {names}")
                }));
            };
            Ok(ToolOutput::text(format!(
                "Skill \"{}\" ({}) — supporting files it names resolve relative to {}.\n\n{}",
                skill.name,
                skill.path.display(),
                skill.dir().display(),
                skill.body
            )))
        })
    }
}

/// Split optional `---` frontmatter from the body. An unterminated fence is
/// body text: the document keeps the meaning its author wrote.
fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut fields = BTreeMap::new();
    let mut lines = text.split_inclusive('\n');
    let Some(opening) = lines.next() else {
        return (fields, text);
    };
    if opening.trim_end() != "---" {
        return (fields, text);
    }
    let mut offset = opening.len();
    for line in lines {
        offset += line.len();
        if line.trim_end() != "---" {
            continue;
        }
        let (front, body) = text.split_at(offset);
        fill_fields(front, &mut fields);
        return (fields, body);
    }
    (BTreeMap::new(), text)
}

/// One `key: value` per line — enough YAML for frontmatter, and no more than
/// that. Unknown keys stay, so callers read what they know; the block scalar
/// forms (`>` folded, `|` literal, either with a chomping indicator) are read
/// too, because a long description is exactly what they are for.
fn fill_fields(front: &str, fields: &mut BTreeMap<String, String>) {
    let lines: Vec<&str> = front.lines().collect();
    // Row 0 is the opening fence.
    let mut index = 1;
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') || line == "---" {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let key = match key.as_str() {
            "input_hint" | "argument-hint" | "argument_hint" => "input-hint".to_string(),
            _ => key,
        };
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let text = match block_scalar(value) {
            Some(folded) => {
                let block = take_block(&lines, &mut index);
                match folded {
                    Folded::Spaces => fold(&block),
                    Folded::Newlines => block.join("\n"),
                }
            }
            None => strip_inline_comment(value)
                .trim_matches(['"', '\''])
                .trim()
                .to_string(),
        };
        if !text.is_empty() {
            fields.insert(key, text);
        }
    }
}

/// Strip an inline ` # comment` unless the value is a fully quoted scalar:
/// YAML keeps `#` inside quotes literal (`description: "fix #42 quickly"`).
fn strip_inline_comment(value: &str) -> &str {
    if let Some(inner) = quoted_scalar(value) {
        return inner;
    }
    value
        .split_once(" #")
        .map_or(value, |(value, _)| value.trim())
}

/// The contents of a fully quoted scalar (`"…"` or `'…'`), if the value is one.
fn quoted_scalar(value: &str) -> Option<&str> {
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &value[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(&rest[..end])
}

/// Whether a block scalar's lines are folded into one paragraph or kept as is.
enum Folded {
    Spaces,
    Newlines,
}

/// `value` as a block scalar header: `>`, `>-`, `>+`, `|`, `|-`, `|+`.
fn block_scalar(value: &str) -> Option<Folded> {
    let header = value.split(" #").next().unwrap_or(value).trim();
    let mut chars = header.chars();
    let folded = match chars.next()? {
        '>' => Folded::Spaces,
        '|' => Folded::Newlines,
        _ => return None,
    };
    match chars.as_str() {
        "" | "-" | "+" => Some(folded),
        _ => None,
    }
}

/// Consume the indented lines a block scalar owns. Blank lines inside the block
/// belong to it; the first line back at column zero ends it.
fn take_block(lines: &[&str], index: &mut usize) -> Vec<String> {
    let mut block = Vec::new();
    while *index < lines.len() {
        let line = lines[*index];
        if line.trim().is_empty() {
            block.push(String::new());
            *index += 1;
            continue;
        }
        if line.starts_with("---") || !line.starts_with([' ', '\t']) {
            break;
        }
        block.push(line.trim().to_string());
        *index += 1;
    }
    // A trailing blank line separates keys; it is not part of the value.
    while block.last().is_some_and(String::is_empty) {
        block.pop();
    }
    block
}

/// Folded style: lines join with a space, a blank line becomes a paragraph
/// break. (`>` in YAML also folds to `\n` at the paragraph level, which this
/// keeps, and the `-` chomping indicator only trims the trailing newline this
/// join never adds.)
fn fold(block: &[String]) -> String {
    let mut out = String::new();
    for line in block {
        if line.is_empty() {
            out.push('\n');
        } else if out.is_empty() || out.ends_with('\n') {
            out.push_str(line);
        } else {
            out.push(' ');
            out.push_str(line);
        }
    }
    out
}

/// The name a file carries when its frontmatter omits one: `foo.md` → `foo`,
/// `foo/SKILL.md` → `foo`.
fn default_name(entry: &Path) -> String {
    let stem = if entry.is_dir() {
        entry.file_name()
    } else {
        entry.file_stem()
    };
    stem.map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(byte) if byte.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= MAX_NAME_BYTES
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The description used when frontmatter omits one: the first body line, minus
/// any leading markdown decoration.
fn first_line(body: &str) -> String {
    body.lines()
        .map(|line| line.trim().trim_start_matches(['#', '*', '-', '>', ' ']))
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_end_matches(['#', '*', ' '])
        .to_string()
}

/// Char-based truncation, so a limit never splits a codepoint.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Cut `text` to at most `limit` bytes without splitting a codepoint.
fn truncate_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancellationToken, context::RequestContext};
    use std::{fs, sync::Mutex, time::Duration};

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The tool never touches the context; one bare context serves every call.
    fn tool_context() -> ToolContext {
        let cancellation = CancellationToken::new();
        let request = RequestContext::new(
            cancellation.clone(),
            Duration::from_secs(30),
            16,
            Arc::new(Mutex::new(vec![])),
        )
        .unwrap();
        ToolContext {
            call_id: "skill-test".into(),
            cancellation,
            deadline: request.deadline,
            max_output_bytes: 64 * 1024,
            request,
            local_session: Arc::default(),
            parent: None,
        }
    }

    fn catalog(workspace: &Path) -> SkillCatalog {
        SkillCatalog::discover(workspace)
    }

    /// YAML keeps `#` inside a quoted scalar literal; only an unquoted ` #`
    /// opens a comment (`description: "fix #42 quickly"` used to truncate).
    #[test]
    fn frontmatter_quotes_keep_hash_and_unquoted_comments_do_not() {
        let mut fields = BTreeMap::new();
        fill_fields(
            "---\ndescription: \"fix #42 quickly\"\nname: simple # a comment\n---\n",
            &mut fields,
        );
        assert_eq!(
            fields.get("description").map(String::as_str),
            Some("fix #42 quickly")
        );
        assert_eq!(fields.get("name").map(String::as_str), Some("simple"));
    }

    /// The workspace's `.agents/skills` directory is the only root: two layouts,
    /// both named after the file, sorted for a stable listing.
    #[test]
    fn discovers_flat_and_directory_skills() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/release-notes.md"),
            "---\nname: release-notes\ndescription: draft the release notes\ninput-hint: <tag>\n---\n\n# Steps\n\nRead the changelog.\n",
        );
        write(
            &workspace.path().join(".agents/skills/triage/SKILL.md"),
            "---\ndescription: sort the inbox\n---\nFirst read the labels.\n",
        );
        let catalog = catalog(workspace.path());
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
        let names: Vec<&str> = catalog
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["release-notes", "triage"], "sorted");
        let release = catalog.get("release-notes").unwrap();
        assert_eq!(release.input_hint.as_deref(), Some("<tag>"));
        assert_eq!(release.description, "draft the release notes");
        assert_eq!(release.body, "# Steps\n\nRead the changelog.");
        assert_eq!(
            release.path,
            workspace.path().join(".agents/skills/release-notes.md")
        );
        // A directory skill takes its name from the directory, `SKILL.md` aside.
        let triage = catalog.get("triage").unwrap();
        assert_eq!(triage.name, "triage");
        assert_eq!(triage.description, "sort the inbox");
    }

    /// Nothing outside the workspace's `.agents/skills` is read: the `.agents`
    /// parent, a sibling workspace, the aby home and the session store all stay
    /// out of the catalog.
    #[test]
    fn only_the_workspace_agents_skills_directory_is_scanned() {
        let workspace = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/kept.md"),
            "---\ndescription: kept\n---\nbody\n",
        );
        // One level up, inside `.agents` itself, is not the skills directory.
        write(
            &workspace.path().join(".agents/dropped.md"),
            "---\ndescription: dropped\n---\nbody\n",
        );
        // The same layout in another workspace, plus the shapes an earlier
        // design used. None of them may appear.
        write(
            &elsewhere.path().join(".agents/skills/dropped.md"),
            "---\ndescription: dropped\n---\nbody\n",
        );
        write(
            &workspace.path().join(".abylab/skills/dropped.md"),
            "---\ndescription: dropped\n---\nbody\n",
        );
        write(
            &workspace.path().join(".abycore/skills/dropped.md"),
            "---\ndescription: dropped\n---\nbody\n",
        );
        let catalog = catalog(workspace.path());
        let names: Vec<&str> = catalog
            .skills()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["kept"], "{:?}", catalog.warnings());
    }

    /// Two files for one name is a mistake, not a merge: the first path in
    /// sort order wins and the other is reported.
    #[test]
    fn duplicate_names_keep_the_first_path_and_warn() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/dup.md"),
            "---\ndescription: first\n---\nfirst\n",
        );
        write(
            &workspace.path().join(".agents/skills/dup/SKILL.md"),
            "---\ndescription: second\n---\nsecond\n",
        );
        let catalog = catalog(workspace.path());
        assert_eq!(catalog.skills().len(), 1);
        assert_eq!(
            catalog.get("dup").unwrap().body,
            "second",
            "the directory sorts before the flat file"
        );
        assert_eq!(catalog.warnings().len(), 1);
        assert!(catalog.warnings()[0].contains("duplicate skill `dup`"));
    }

    /// An invalid name is a mistake and drops the file; an oversized body is
    /// not — the skill loads, truncated at a char boundary, and says so.
    #[test]
    fn invalid_names_are_skipped_and_oversized_bodies_are_truncated() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/Bad Name.md"),
            "---\ndescription: spaces\n---\nbody\n",
        );
        write(
            &workspace.path().join(".agents/skills/huge.md"),
            &format!(
                "---\ndescription: big\n---\n{}\n",
                "x".repeat(MAX_SKILL_BYTES + 1)
            ),
        );
        // Not a skill: a stray markdown-free file and a directory with no SKILL.md.
        write(
            &workspace.path().join(".agents/skills/notes.txt"),
            "not a skill\n",
        );
        fs::create_dir_all(workspace.path().join(".agents/skills/empty-dir")).unwrap();
        // Not a skill: a dotfile.
        write(
            &workspace.path().join(".agents/skills/.hidden.md"),
            "hidden\n",
        );
        let catalog = catalog(workspace.path());
        assert_eq!(catalog.skills().len(), 1, "{:?}", catalog.warnings());
        assert_eq!(catalog.warnings().len(), 2, "{:?}", catalog.warnings());
        assert!(catalog.warnings()[0].contains("not a valid skill name"));
        assert!(
            catalog.warnings()[1].contains("truncated"),
            "{:?}",
            catalog.warnings()
        );

        let huge = catalog.get("huge").unwrap();
        assert_eq!(huge.description, "big", "frontmatter still parses");
        assert!(huge.body.ends_with("]"), "the marker closes the body");
        assert!(
            huge.body.contains("this session keeps the first"),
            "the marker explains the cut: {}",
            huge.body.len()
        );
        let kept = huge.body.split("\n\n[Truncated").next().unwrap();
        assert_eq!(kept.len(), MAX_SKILL_BYTES, "cut at the cap exactly");
    }

    /// The byte cut never splits a codepoint, even for a body made of them.
    #[test]
    fn oversized_bodies_cut_on_a_char_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/wide.md"),
            &format!(
                "---\ndescription: wide\n---\n{}",
                "技".repeat(MAX_SKILL_BYTES)
            ),
        );
        let catalog = catalog(workspace.path());
        let body = &catalog.get("wide").unwrap().body;
        assert!(body.is_char_boundary(body.len() - 1), "whole codepoints");
        let kept = body.split("\n\n[Truncated").next().unwrap();
        assert!(kept.len() <= MAX_SKILL_BYTES);
        assert!(kept.len() > MAX_SKILL_BYTES - 4, "one char short at most");
    }

    #[test]
    fn description_falls_back_to_the_first_body_line() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/plain.md"),
            "\n## Tidy the changelog\n\nBody first, then the steps.\n",
        );
        let catalog = catalog(workspace.path());
        let skill = catalog.get("plain").unwrap();
        assert_eq!(skill.description, "Tidy the changelog");
        assert_eq!(skill.name, "plain", "the file name is the fallback name");
        assert_eq!(
            skill.body,
            "## Tidy the changelog\n\nBody first, then the steps."
        );
    }

    #[test]
    fn an_unterminated_fence_stays_body_text() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/open.md"),
            "---\nname: open\nnot frontmatter after all\n",
        );
        let catalog = catalog(workspace.path());
        let skill = catalog.get("open").unwrap();
        assert_eq!(skill.name, "open", "the file name names it");
        assert!(skill.body.starts_with("---\nname: open"), "{}", skill.body);
    }

    /// Long descriptions are written as YAML block scalars; reading `>-` as the
    /// literal string would put `>-` in the listing, the index and the tool
    /// listing. Extra keys (`tools: Bash, Write`) are kept but unused.
    #[test]
    fn folded_and_literal_block_scalars_are_read() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/audit/SKILL.md"),
            "---\nname: audit\ndescription: >- \n  逐项检查数据来源与计算，\n  自动定位最新报告。\ntools: Bash, Write\n---\n\n# audit\n",
        );
        write(
            &workspace.path().join(".agents/skills/notes.md"),
            "---\ndescription: |-\n  First paragraph.\n\n  Second paragraph.\n---\nbody\n",
        );
        write(
            &workspace.path().join(".agents/skills/broken.md"),
            "---\ndescription: >- # nothing follows\n---\nFallback line.\n",
        );
        write(
            &workspace.path().join(".agents/skills/paragraphs.md"),
            "---\ndescription: >-\n  one\n\n  two\n---\nbody\n",
        );
        let catalog = catalog(workspace.path());
        let audit = catalog.get("audit").unwrap();
        assert_eq!(
            audit.description, "逐项检查数据来源与计算， 自动定位最新报告。",
            "folded lines join with a space"
        );
        assert_eq!(audit.body, "# audit", "the block stays out of the body");
        assert_eq!(
            catalog.get("notes").unwrap().description,
            "First paragraph.\n\nSecond paragraph.",
            "literal style keeps the blank line as written"
        );
        assert_eq!(
            catalog.get("paragraphs").unwrap().description,
            "one\ntwo",
            "a blank line becomes the break in folded style too"
        );
        assert_eq!(
            catalog.get("broken").unwrap().description,
            "Fallback line.",
            "an empty block falls back to the body"
        );
    }

    /// Literal style keeps its newlines instead of folding them.
    #[test]
    fn a_literal_block_keeps_its_line_breaks() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/steps.md"),
            "---\ndescription: |\n  one\n  two\n---\nbody\n",
        );
        let catalog = catalog(workspace.path());
        assert_eq!(catalog.get("steps").unwrap().description, "one\ntwo");
    }

    #[test]
    fn invocation_parses_only_known_skill_lines() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/deploy.md"),
            "---\ninput-hint: <env>\n---\nDeploy it.\n",
        );
        let catalog = catalog(workspace.path());

        let invocation = catalog.invocation("/deploy prod now").unwrap();
        assert_eq!(invocation.skill.name, "deploy");
        assert_eq!(invocation.line, "/deploy prod now");
        assert_eq!(invocation.args, "prod now");

        assert!(
            catalog.invocation("/deploy").is_some(),
            "no argument needed"
        );
        assert!(catalog.invocation("  /deploy  ").is_some(), "trimmed");
        assert!(catalog.invocation("/unknown").is_none());
        assert!(catalog.invocation("/usr/bin/env").is_none());
        assert!(catalog.invocation("//deploy").is_none());
        assert!(catalog.invocation("/").is_none());
        assert!(catalog.invocation("deploy").is_none());
        assert!(catalog.invocation("").is_none());
    }

    #[test]
    fn expansion_keeps_the_typed_line_then_frames_the_body() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/deploy.md"),
            "---\ndescription: ship it\n---\nDeploy to the named environment.\n",
        );
        let catalog = catalog(workspace.path());
        let text = catalog.expand("/deploy prod").unwrap();
        let first = text.lines().next().unwrap();
        assert_eq!(first, "/deploy prod", "the typed line stays readable");
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("user's argument"), "args are acknowledged");
        assert!(text.ends_with("Deploy to the named environment."));
        // Without arguments the frame drops the argument sentence.
        let text = catalog.expand("/deploy").unwrap();
        assert!(!text.contains("user's argument"));
        assert!(catalog.expand("/nope").is_none());
    }

    #[test]
    fn index_block_lists_names_and_is_stable() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(catalog(workspace.path()).index_block().is_none());
        write(
            &workspace.path().join(".agents/skills/deploy.md"),
            "---\ndescription: ship it\n---\nbody\n",
        );
        let catalog = catalog(workspace.path());
        let block = catalog.index_block().unwrap();
        assert!(block.starts_with("<system-reminder>"));
        assert!(block.ends_with("</system-reminder>"));
        assert!(block.contains("- deploy · ship it"), "{block}");
        assert_eq!(
            block,
            catalog.index_block().unwrap(),
            "stable for the session"
        );
    }

    #[tokio::test]
    async fn the_skill_tool_lists_reads_and_rejects_unknown_names() {
        let workspace = tempfile::tempdir().unwrap();
        write(
            &workspace.path().join(".agents/skills/deploy.md"),
            "---\ndescription: ship it\n---\nDeploy to the named environment.\n",
        );
        let catalog = Arc::new(catalog(workspace.path()));
        let tool = catalog.tool();
        assert_eq!(tool.definition().name, "skill");

        let context = tool_context;
        let listing = tool.execute(json!({}), context()).await.unwrap().content;
        assert!(listing.contains("- deploy · ship it"), "{listing}");

        let body = tool
            .execute(json!({"name": "deploy"}), context())
            .await
            .unwrap()
            .content;
        assert!(body.contains("Deploy to the named environment."), "{body}");
        assert!(
            body.contains("deploy.md"),
            "the path rides the result: {body}"
        );

        let err = tool
            .execute(json!({"name": "nope"}), context())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("available: deploy"), "{err}");

        assert!(tool.validate(&json!({})).is_ok());
        assert!(tool.validate(&json!({"name": "deploy"})).is_ok());
        assert!(tool.validate(&json!({"name": ""})).is_err());
        assert!(tool.validate(&json!({"other": 1})).is_err());
        assert!(tool.validate(&json!("deploy")).is_err());

        // An empty catalog says so instead of erroring.
        let empty = Arc::new(SkillCatalog::default());
        let empty_tool = empty.tool();
        assert!(
            empty_tool
                .execute(json!({}), context())
                .await
                .unwrap()
                .content
                .contains("No skills")
        );
    }
}
