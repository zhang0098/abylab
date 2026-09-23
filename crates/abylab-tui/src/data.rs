//! The data half of `abylab --uninstall`: delete everything this program saved
//! under the aby home, and nothing else.
//!
//! `--uninstall` asks about this half and the program half separately, and this
//! plan is what the first question is about. The contract is the aby home.
//! Five entries there are ours — the interface preferences (`settings.json`),
//! the per-workspace mode cache (`abylab-modes.json`), the API key `/login`
//! stored (`.credentials.yaml`), the shared session store (`sessions/`) and the
//! durable prompt queue (`queued/`) — and the plan lists them by name, with what
//! is inside, before anything goes.
//!
//! Two things the home may hold are *not* the command's to delete, and the
//! plan says so instead of quietly skipping them:
//!
//! - a file the user wrote by hand (`AGENTS.md`, which abycore reads as
//!   workspace instructions) is theirs, not the program's;
//! - the session store is `--session-root`, which defaults to
//!   `$ABYLAB_HOME/sessions` but may point anywhere. A store *inside* the home
//!   is cleaned like the rest; one outside it is out of the command's scope
//!   and survives (as does a `--session-root` naming the home itself — the
//!   command removes the entries it put there, not a directory it was pointed
//!   at).
//!
//! Nothing here decides *when* or *how many times* the user confirms: the CLI
//! asks on the terminal and hands the [`DataPlan`] to [`wipe`]. Sizes are
//! measured with `lstat`, so a symlink counts as the file it is instead of the
//! tree it names — exactly what `remove_dir_all` will unlink.

use std::path::{Path, PathBuf};

use crate::locale::Locale;

/// The hand-written file abycore reads out of the home (workspace
/// instructions). The plan reports it as kept; it is never deleted.
const INSTRUCTIONS_FILE: &str = "AGENTS.md";

/// Upper bound on the files one plan entry measures. A session store is a few
/// small JSONL logs per session, so this is unreachable in practice; it keeps
/// the plan from stalling on a home someone filled by hand. The count then
/// reads as a lower bound (`20000+`).
const MAX_WALK: u64 = 20_000;

/// What one removed entry is, in the interface language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataWhat {
    /// `settings.json`: language, model, effort, permission, appearance.
    Settings,
    /// `abylab-modes.json`: the per-workspace mode cache.
    Modes,
    /// `.credentials.yaml`: the key `/login` stored.
    Credentials,
    /// The session store root (`$ABYLAB_HOME/sessions` by default).
    Sessions,
    /// `queued/`: prompts typed while a turn was running.
    Queue,
}

impl DataWhat {
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            Self::Settings => locale.tr("settings", "设置"),
            Self::Modes => locale.tr("mode cache", "模式缓存"),
            Self::Credentials => locale.tr("api key", "api key"),
            Self::Sessions => locale.tr("session logs", "会话日志"),
            Self::Queue => locale.tr("queued prompts", "排队消息"),
        }
    }
}

/// Why one path survives the wipe. Every reason is printed, so the reader
/// learns what uninstall does *not* cover before it runs, not after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataKeepWhy {
    /// `--session-root` points outside the aby home.
    OutsideHome,
    /// `--session-root` names the home itself.
    RootIsHome,
    /// A file the user wrote by hand; the program only reads it.
    HandWritten,
}

impl DataKeepWhy {
    pub fn note(self, locale: Locale) -> &'static str {
        match self {
            Self::OutsideHome => locale.tr(
                "session store outside the aby home (--session-root)",
                "会话库在 aby 主目录之外（--session-root）",
            ),
            Self::RootIsHome => locale.tr(
                "--session-root names the aby home itself",
                "--session-root 指的就是 aby 主目录",
            ),
            Self::HandWritten => locale.tr(
                "written by hand, not saved by abylab",
                "手写文件，不是本程序保存的",
            ),
        }
    }
}

/// One entry the command owns in the aby home: its name and its label.
struct Owned {
    name: &'static str,
    what: DataWhat,
}

/// The entries this program creates in the home, in the order the plan reads:
/// preferences first, then the state that costs the most to lose.
const OWNED: &[Owned] = &[
    Owned {
        name: "settings.json",
        what: DataWhat::Settings,
    },
    Owned {
        name: "abylab-modes.json",
        what: DataWhat::Modes,
    },
    Owned {
        name: crate::credentials::CREDENTIALS_FILENAME,
        what: DataWhat::Credentials,
    },
    Owned {
        name: "sessions",
        what: DataWhat::Sessions,
    },
    Owned {
        name: "queued",
        what: DataWhat::Queue,
    },
];

/// One path the command removes, with what is inside it right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataItem {
    pub what: DataWhat,
    pub path: PathBuf,
    /// Files under `path` (1 for a plain file); a lower bound when the walk
    /// hit [`MAX_WALK`].
    pub files: u64,
    /// Apparent bytes those files take.
    pub bytes: u64,
    pub truncated: bool,
}

impl DataItem {
    /// The entry's name, as it reads in a one-line report.
    #[cfg(test)]
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// One path the command leaves alone, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataKeep {
    pub path: PathBuf,
    pub why: DataKeepWhy,
}

/// What a wipe would remove, and what it would not: measured once, shown to
/// the user, then handed to [`wipe`] — so what was listed is what goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataPlan {
    pub home: PathBuf,
    /// The entries that exist right now; absent ones are not listed.
    pub items: Vec<DataItem>,
    pub kept: Vec<DataKeep>,
}

impl DataPlan {
    /// Nothing this program saved is there: the command has no work to do.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn files(&self) -> u64 {
        self.items.iter().map(|item| item.files).sum()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|item| item.bytes).sum()
    }
}

/// What a completed [`wipe`] did: entries removed with the size the plan
/// promised, plus every path that refused to go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataOutcome {
    pub removed: usize,
    pub files: u64,
    pub bytes: u64,
    /// `(path, reason)` — a partial wipe is reported per path, never summed up
    /// into a success.
    pub failures: Vec<(PathBuf, String)>,
}

/// Measure the home and the session store the launcher was pointed at.
pub fn scan(home: &Path, sessions_root: &Path) -> DataPlan {
    let mut items = Vec::new();
    for owned in OWNED {
        include(&mut items, home, owned.what, home.join(owned.name));
    }
    let mut kept = Vec::new();
    if sessions_root == home {
        kept.push(DataKeep {
            path: home.to_path_buf(),
            why: DataKeepWhy::RootIsHome,
        });
    } else if sessions_root.starts_with(home) {
        // The default `$ABYLAB_HOME/sessions` is one of the entries above, so
        // this only adds a store the launcher pointed somewhere else inside
        // the home. `include` dedupes the two by path.
        include(
            &mut items,
            home,
            DataWhat::Sessions,
            sessions_root.to_path_buf(),
        );
    } else if sessions_root.exists() {
        kept.push(DataKeep {
            path: sessions_root.to_path_buf(),
            why: DataKeepWhy::OutsideHome,
        });
    }
    let instructions = home.join(INSTRUCTIONS_FILE);
    if instructions.is_file() {
        kept.push(DataKeep {
            path: instructions,
            why: DataKeepWhy::HandWritten,
        });
    }
    DataPlan {
        home: home.to_path_buf(),
        items,
        kept,
    }
}

/// Add one existing path to the plan. The home itself is never a target: the
/// command removes the entries it put in there, not the directory they live in.
fn include(items: &mut Vec<DataItem>, home: &Path, what: DataWhat, path: PathBuf) {
    if path == home || items.iter().any(|item| item.path == path) {
        return;
    }
    if let Some(item) = measure(what, path) {
        items.push(item);
    }
}

fn measure(what: DataWhat, path: PathBuf) -> Option<DataItem> {
    let meta = std::fs::symlink_metadata(&path).ok()?;
    let (files, bytes, truncated) = if meta.is_dir() {
        walk(&path)
    } else {
        (1, meta.len(), false)
    };
    Some(DataItem {
        what,
        path,
        files,
        bytes,
        truncated,
    })
}

/// Count the files under `path` and the bytes they take. Symlinks are counted
/// as the link itself (they are what `remove_dir_all` unlinks, and a cycle
/// cannot trap the walk).
fn walk(path: &Path) -> (u64, u64, bool) {
    let (mut files, mut bytes) = (0, 0);
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue; // unreadable directory: the wipe will report what it cannot do
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
                continue;
            }
            files += 1;
            bytes += meta.len();
            if files >= MAX_WALK {
                return (files, bytes, true);
            }
        }
    }
    (files, bytes, false)
}

/// Remove every entry the plan listed. A path that vanished since the scan is
/// not a failure (there is nothing left to delete); everything else that
/// refuses to go is named in [`DataOutcome::failures`].
pub fn wipe(plan: &DataPlan) -> DataOutcome {
    let mut outcome = DataOutcome::default();
    for item in &plan.items {
        let result = match std::fs::symlink_metadata(&item.path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&item.path),
            Ok(_) => std::fs::remove_file(&item.path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                outcome.removed += 1;
                outcome.files += item.files;
                outcome.bytes += item.bytes;
            }
            Err(error) => outcome
                .failures
                .push((item.path.clone(), error.to_string())),
        }
    }
    outcome
}

/// Apparent size in the units the installer prints: whole KB below a megabyte,
/// one decimal above it.
pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes.div_ceil(KB))
    } else {
        format!("{bytes} B")
    }
}

/// A file count as it reads in a plan row; `10000+` when the walk stopped at
/// [`MAX_WALK`].
pub fn human_files(files: u64, truncated: bool) -> String {
    if truncated {
        format!("{files}+")
    } else {
        files.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch home with the five owned entries, a hand-written AGENTS.md
    /// and a session tree of its own.
    fn scratch(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "aby-data-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("sessions/ws-abc/aby-1")).unwrap();
        std::fs::create_dir_all(home.join("queued")).unwrap();
        std::fs::write(home.join("settings.json"), "{\"language\":\"en\"}").unwrap();
        std::fs::write(home.join("abylab-modes.json"), "{}").unwrap();
        std::fs::write(home.join(".credentials.yaml"), "version: 1\n").unwrap();
        std::fs::write(
            home.join("sessions/ws-abc/aby-1/session.jsonl"),
            "x".repeat(2048),
        )
        .unwrap();
        std::fs::write(home.join("queued/aby-1.json"), "[]").unwrap();
        std::fs::write(home.join("AGENTS.md"), "be terse").unwrap();
        home
    }

    #[test]
    fn the_plan_names_what_the_program_saved_and_nothing_else() {
        let home = scratch("plan");
        let plan = scan(&home, &home.join("sessions"));
        let names: Vec<String> = plan.items.iter().map(DataItem::name).collect();
        assert_eq!(
            names,
            vec![
                "settings.json",
                "abylab-modes.json",
                ".credentials.yaml",
                "sessions",
                "queued"
            ],
            "the plan lists the owned entries once each, in order: {plan:?}"
        );
        // The default store is one of them, not a second row.
        assert_eq!(
            plan.items
                .iter()
                .filter(|item| item.what == DataWhat::Sessions)
                .count(),
            1
        );
        let sessions = plan
            .items
            .iter()
            .find(|item| item.what == DataWhat::Sessions)
            .unwrap();
        assert_eq!(sessions.files, 1);
        assert_eq!(sessions.bytes, 2048);
        assert_eq!(plan.files(), 5);
        // 17 (`{"language":"en"}`) + 2 (`{}`) + 11 (`version: 1\n`) + 2048 (the
        // log) + 2 (`[]`), one file each.
        assert_eq!(plan.bytes(), 17 + 2 + 11 + 2048 + 2);
        assert!(!plan.is_empty());
        assert_eq!(
            plan.kept
                .iter()
                .map(|keep| (
                    keep.path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    keep.why
                ))
                .collect::<Vec<_>>(),
            vec![("AGENTS.md".to_string(), DataKeepWhy::HandWritten)],
            "the hand-written instructions file is reported as kept"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_store_inside_the_home_is_cleaned_and_one_outside_is_reported() {
        let home = scratch("store");
        // A store the launcher pointed at elsewhere inside the home.
        let inside = home.join("store");
        std::fs::create_dir_all(inside.join("ws-abc")).unwrap();
        let plan = scan(&home, &inside);
        assert!(
            plan.items.iter().any(|item| item.path == inside),
            "an inside store is one more entry: {plan:?}"
        );
        // The default `sessions` tree is still listed — both are ours.
        assert!(plan
            .items
            .iter()
            .any(|item| item.path == home.join("sessions")));

        // A store outside the home is out of scope, and says so.
        let outside = std::env::temp_dir().join(format!("aby-data-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let plan = scan(&home, &outside);
        assert!(
            !plan.items.iter().any(|item| item.path == outside),
            "an outside store is never a target: {plan:?}"
        );
        assert_eq!(
            plan.kept
                .iter()
                .find(|keep| keep.path == outside)
                .map(|k| k.why),
            Some(DataKeepWhy::OutsideHome)
        );
        let _ = std::fs::remove_dir_all(&outside);

        // `--session-root ~/.abylab`: the home is never a target either.
        let plan = scan(&home, &home);
        assert!(!plan.items.iter().any(|item| item.path == home));
        assert_eq!(
            plan.kept
                .iter()
                .find(|keep| keep.path == home)
                .map(|k| k.why),
            Some(DataKeepWhy::RootIsHome)
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_empty_home_has_nothing_to_remove() {
        let home = std::env::temp_dir().join(format!("aby-data-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let plan = scan(&home, &home.join("sessions"));
        assert!(plan.is_empty(), "{plan:?}");
        assert!(
            plan.kept.is_empty(),
            "no instructions file, nothing to keep"
        );
        assert_eq!(wipe(&plan), DataOutcome::default());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_wipe_removes_what_the_plan_listed_and_leaves_the_rest() {
        let home = scratch("wipe");
        let outside = std::env::temp_dir().join(format!("aby-data-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let plan = scan(&home, &outside);
        let outcome = wipe(&plan);
        assert_eq!(outcome.removed, plan.items.len());
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert_eq!(outcome.bytes, plan.bytes());
        for name in [
            "settings.json",
            "abylab-modes.json",
            ".credentials.yaml",
            "sessions",
            "queued",
        ] {
            assert!(!home.join(name).exists(), "{name} should be gone");
        }
        assert!(
            home.join("AGENTS.md").exists(),
            "a hand-written file is not ours to delete"
        );
        assert!(outside.exists(), "an outside store survives");
        assert!(home.exists(), "the home itself stays");
        // A second wipe has nothing left to do.
        assert_eq!(wipe(&scan(&home, &outside)), DataOutcome::default());
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn sizes_read_the_way_the_installer_prints_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1025), "2 KB", "a byte over rounds up");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
        assert_eq!(human_files(7, false), "7");
        assert_eq!(human_files(MAX_WALK, true), "20000+");
    }
}
