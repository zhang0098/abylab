//! Startup workspace instructions. Discovery follows harness's baseline rules;
//! rendering is a bounded user-context prefix outside the compactable history.

use std::{
    collections::HashSet,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const CANDIDATES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    "AGENTS.local.md",
    "CLAUDE.local.md",
];
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 64 * 1024;
const INTRO: &str = "<system-reminder>\nCurrent workspace instruction baseline follows. It replaces earlier file-sourced workspace instructions, including versions mentioned in conversation summaries. More specific instructions take precedence over broader ones. They do not override system, developer, or direct user instructions.\n";
pub(crate) const EMPTY_BASELINE: &str = "<system-reminder>\nNo workspace instruction files were loaded for this session. Earlier file-sourced workspace instruction baselines, including versions mentioned in conversation summaries, are no longer active. Direct user instructions still apply.\n</system-reminder>";
const CLOSE: &str = "\n</system-reminder>";
const OMITTED: &str =
    "\nSome broader instruction files were omitted to fit the context byte limit.\n";
const TRUNCATED: &str = "\n[Instruction content truncated to fit the context byte limit.]";

#[derive(Clone)]
pub(crate) struct InstructionSource {
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
}

#[derive(Default)]
pub(crate) struct LoadedInstructions {
    pub text: Option<String>,
    pub warnings: Vec<String>,
}

struct InstructionFile {
    path: PathBuf,
    scope: String,
    content: String,
}

impl InstructionSource {
    /// Called for each fresh/restored session. Missing files are normal;
    /// unreadable and oversized files produce a visible, nonfatal diagnostic.
    pub fn load(&self) -> LoadedInstructions {
        let mut loaded = LoadedInstructions::default();
        let mut paths = HashSet::new();
        let mut files = Vec::new();
        if let Some(home) = &self.home {
            read_candidate(
                home.join("AGENTS.md"),
                "all work",
                &mut paths,
                &mut files,
                &mut loaded,
            );
        }
        match fs::canonicalize(&self.workspace) {
            Ok(cwd) => {
                let root = project_root(&cwd).unwrap_or_else(|err| {
                    loaded
                        .warnings
                        .push(format!("project root discovery: {err}"));
                    cwd.clone()
                });
                let mut chain: Vec<_> = cwd
                    .ancestors()
                    .take_while(|dir| dir.starts_with(&root))
                    .collect();
                chain.reverse();
                for dir in chain {
                    let start = files.len();
                    for name in CANDIDATES {
                        read_candidate(
                            dir.join(name),
                            &format!("work under {}", dir.display()),
                            &mut paths,
                            &mut files,
                            &mut loaded,
                        );
                    }
                    // Aliases and local overlays with identical trimmed content
                    // in the same directory are represented by their first file.
                    let mut contents = HashSet::new();
                    let mut index = 0;
                    files.retain(|file| {
                        let keep = index < start || contents.insert(file.content.trim().to_owned());
                        index += 1;
                        keep
                    });
                }
            }
            Err(err) => loaded
                .warnings
                .push(format!("{}: {err}", self.workspace.display())),
        }
        loaded.text = render(files, &mut loaded.warnings);
        loaded
    }
}

fn missing(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn project_root(cwd: &Path) -> io::Result<PathBuf> {
    for dir in cwd.ancestors() {
        match fs::metadata(dir.join(".git")) {
            // Worktrees use a .git file instead of a directory.
            Ok(_) => return Ok(dir.to_owned()),
            Err(err) if missing(&err) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(cwd.to_owned())
}

fn read_candidate(
    path: PathBuf,
    scope: &str,
    paths: &mut HashSet<PathBuf>,
    files: &mut Vec<InstructionFile>,
    loaded: &mut LoadedInstructions,
) {
    let read = || -> io::Result<Option<String>> {
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if missing(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        if !meta.is_file() {
            return Ok(None);
        }
        if meta.len() > MAX_SOURCE_BYTES as u64 {
            return Err(io::Error::other("instruction file exceeds 1 MiB; skipped"));
        }
        // Bound the actual read too, in case the file grew after metadata().
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take((MAX_SOURCE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(io::Error::other("instruction file exceeds 1 MiB; skipped"));
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    };
    match read() {
        Ok(Some(content)) => {
            // Only deduplicate identical paths across global/project discovery;
            // the same contents in different directory scopes remain meaningful.
            let absolute = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
            if paths.insert(absolute) {
                files.push(InstructionFile {
                    path,
                    scope: scope.into(),
                    content,
                });
            }
        }
        Ok(None) => {}
        Err(err) => loaded.warnings.push(format!("{}: {err}", path.display())),
    }
}

fn escape(text: &str) -> String {
    text.replace("</system-reminder>", "<\\/system-reminder>")
}

fn render(files: Vec<InstructionFile>, warnings: &mut Vec<String>) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let sections: Vec<_> = files
        .iter()
        .map(|file| {
            escape(&format!(
                "\nInstructions from: {}\nScope: {}\n\n{}\n",
                file.path.display(),
                file.scope,
                file.content
            ))
        })
        .collect();
    let mut size = INTRO.len() + CLOSE.len() + sections.iter().map(String::len).sum::<usize>();
    let mut start = 0;
    while size + if start > 0 { OMITTED.len() } else { 0 } > MAX_CONTEXT_BYTES
        && start + 1 < sections.len()
    {
        warnings.push(format!(
            "{}: omitted to fit the 64 KiB instruction context",
            files[start].path.display()
        ));
        size -= sections[start].len();
        start += 1;
    }
    let mut text = INTRO.to_owned();
    if start > 0 {
        text.push_str(OMITTED);
    }
    for section in &sections[start..] {
        let available = MAX_CONTEXT_BYTES - text.len() - CLOSE.len();
        if section.len() <= available {
            text.push_str(section);
        } else {
            let mut end = available - TRUNCATED.len();
            while !section.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&section[..end]);
            text.push_str(TRUNCATED);
            warnings.push(format!(
                "{}: truncated to fit the 64 KiB instruction context",
                files[start].path.display()
            ));
        }
    }
    text.push_str(CLOSE);
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);
    impl Tree {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("abylab-instructions-{}-{id}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        fn load(&self, cwd: &str) -> LoadedInstructions {
            fs::create_dir_all(self.0.join(cwd)).unwrap();
            InstructionSource {
                workspace: self.0.join(cwd),
                home: Some(self.0.join("home")),
            }
            .load()
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn baseline_orders_global_root_and_cwd_candidates_without_loading_other_scopes() {
        let tree = Tree::new();
        tree.write("AGENTS.md", "OUTSIDE_PROJECT");
        tree.write("home/AGENTS.md", "GLOBAL");
        tree.write("home/CLAUDE.md", "WRONG_GLOBAL_ALIAS");
        tree.write("repo/.git", "gitdir: somewhere");
        tree.write("repo/AGENTS.md", "ROOT_BASE");
        tree.write("repo/CLAUDE.md", "ROOT_ALIAS");
        tree.write("repo/AGENTS.local.md", "ROOT_LOCAL");
        tree.write("repo/CLAUDE.local.md", "ROOT_LOCAL_ALIAS");
        tree.write("repo/src/AGENTS.md", "CWD_BASE");
        tree.write("repo/src/child/AGENTS.md", "NESTED_NOT_TOUCHED");
        tree.write("repo/elsewhere/AGENTS.md", "SIBLING");
        tree.write("repo/src/agents.md", "LOWERCASE");
        let loaded = tree.load("repo/src");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let text = loaded.text.unwrap();
        let positions: Vec<_> = [
            "GLOBAL",
            "ROOT_BASE",
            "ROOT_ALIAS",
            "ROOT_LOCAL",
            "ROOT_LOCAL_ALIAS",
            "CWD_BASE",
        ]
        .iter()
        .map(|s| text.find(s).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        for excluded in [
            "OUTSIDE_PROJECT",
            "WRONG_GLOBAL_ALIAS",
            "NESTED_NOT_TOUCHED",
            "SIBLING",
            "LOWERCASE",
        ] {
            assert!(!text.contains(excluded), "{excluded}");
        }
    }

    #[test]
    fn missing_git_marker_limits_project_discovery_to_cwd() {
        let tree = Tree::new();
        tree.write("AGENTS.md", "ANCESTOR");
        tree.write("plain/AGENTS.md", "CURRENT");
        let text = tree.load("plain").text.unwrap();
        assert!(text.contains("CURRENT"));
        assert!(!text.contains("ANCESTOR"));
    }

    #[test]
    fn git_directory_and_trimmed_alias_dedup_keep_distinct_scopes() {
        let tree = Tree::new();
        fs::create_dir_all(tree.0.join("repo/.git")).unwrap();
        tree.write("repo/AGENTS.md", "SAME_RULE");
        tree.write("repo/CLAUDE.md", "\n SAME_RULE \n");
        tree.write("repo/AGENTS.local.md", "SAME_RULE");
        tree.write("repo/src/AGENTS.md", "SAME_RULE");
        let text = tree.load("repo/src").text.unwrap();
        assert_eq!(text.matches("SAME_RULE").count(), 2);
        assert!(!text.contains("CLAUDE.md"));
        assert!(!text.contains("AGENTS.local.md"));
    }

    #[test]
    fn missing_files_are_silent_and_reload_observes_changes_and_removal() {
        let tree = Tree::new();
        let empty = tree.load("repo");
        assert!(empty.text.is_none());
        assert!(empty.warnings.is_empty());
        tree.write("repo/AGENTS.md", "FIRST");
        assert!(tree.load("repo").text.unwrap().contains("FIRST"));
        tree.write("repo/AGENTS.md", "SECOND");
        let text = tree.load("repo").text.unwrap();
        assert!(text.contains("SECOND"));
        assert!(!text.contains("FIRST"));
        fs::remove_file(tree.0.join("repo/AGENTS.md")).unwrap();
        assert!(tree.load("repo").text.is_none());
    }

    #[test]
    fn render_budget_preserves_specific_scope_and_utf8_frame() {
        let tree = Tree::new();
        tree.write("repo/.git", "gitdir: worktree");
        tree.write("home/AGENTS.md", &"GLOBAL_TO_DROP".repeat(6000));
        tree.write("repo/AGENTS.md", "ROOT_TO_DROP");
        tree.write(
            "repo/src/AGENTS.md",
            &format!("SPECIFIC </system-reminder> {}", "你好世界".repeat(7000)),
        );
        let loaded = tree.load("repo/src");
        let text = loaded.text.unwrap();
        assert!(text.len() <= MAX_CONTEXT_BYTES);
        assert!(text.contains("SPECIFIC <\\/system-reminder>"));
        assert_eq!(text.matches("</system-reminder>").count(), 1);
        assert!(text.ends_with(CLOSE));
        assert!(text.contains(TRUNCATED));
        assert!(!text.contains("GLOBAL_TO_DROP"));
        assert!(!text.contains("ROOT_TO_DROP"));
        assert_eq!(loaded.warnings.len(), 3);
    }

    #[test]
    fn oversized_sources_are_skipped_with_a_diagnostic() {
        let tree = Tree::new();
        tree.write("repo/AGENTS.md", &"x".repeat(MAX_SOURCE_BYTES + 1));
        tree.write("repo/AGENTS.local.md", "SMALL_LOCAL");
        let loaded = tree.load("repo");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("AGENTS.md"));
        assert!(loaded.warnings[0].contains("1 MiB"));
        assert!(loaded.text.unwrap().contains("SMALL_LOCAL"));
    }

    #[cfg(unix)]
    #[test]
    fn file_symlinks_load_and_nonfiles_are_ignored() {
        let tree = Tree::new();
        tree.write("repo/RULES.md", "LINKED_RULE");
        std::os::unix::fs::symlink("RULES.md", tree.0.join("repo/AGENTS.md")).unwrap();
        fs::create_dir_all(tree.0.join("repo/CLAUDE.md")).unwrap();
        let loaded = tree.load("repo");
        assert!(loaded.warnings.is_empty());
        assert!(loaded.text.unwrap().contains("LINKED_RULE"));
    }
}
