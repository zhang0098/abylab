//! `abylab --uninstall`: take back what install.sh put on this machine.
//!
//! Two halves, two questions: the data half is [`crate::reset`]'s (the entries
//! this program saved under the aby home), and the program half is the binary
//! (or binaries) this machine has plus the PATH block install.sh appended to a
//! shell startup file. The caller deletes only what it listed first, only what
//! this program owns, and only after the user says so — "keep the data, remove
//! the program" is the point of asking the two halves separately.
//!
//! The program half follows the rules uninstall.sh documents:
//!
//! - every `abylab` on `$PATH` is a target, plus the running executable
//!   (`std::env::current_exe`) and the installer's default `~/.local/bin`;
//! - a symlink goes together with the file it points at (bounded: a link loop
//!   must not hang);
//! - a file another installer owns (Homebrew, Nix) is reported, never removed;
//! - a copy in cargo's bin directory goes through `cargo uninstall`, so the
//!   bookkeeping in `~/.cargo/.crates.toml` goes with it;
//! - the PATH block is recognized by the marker install.sh writes, plus one
//!   line under it that still has to look like a PATH line — a hand edit below
//!   the marker survives.

use std::path::{Path, PathBuf};

/// The name install.sh installs the binary as. Only a file that is still
/// called this is ours to delete; a renamed or reshaped path is not.
pub const BIN_NAME: &str = "abylab";

/// The cargo package `cargo install` installs, for copies in cargo's bin.
pub const CARGO_PKG: &str = "abylab-tui";

/// The comment install.sh writes above the PATH line it appends. The marker is
/// the contract on the read side, exactly as it is on the write side.
pub const PATH_MARKER: &str = "# added by the abylab installer";

/// How many symlink hops to follow before giving up. A loop must not hang the
/// command, and no real install is this deep.
const MAX_LINK_HOPS: usize = 20;

/// Startup files that can hold install.sh's PATH block, one per shell the
/// installer may have run under. The marker is the authority, not `$SHELL`:
/// someone who changed shells since installing still gets the right file
/// cleaned, and a file without the marker is never touched.
pub fn rc_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".zshrc"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".profile"),
        home.join(".config/fish/config.fish"),
    ]
}

/// Where [`scan`] looks. Every input is passed in: the launcher reads the
/// environment once and tests pin whatever they need, so a scan can never
/// wander into a real home or PATH on its own.
#[derive(Clone, Debug, Default)]
pub struct ScanInput {
    /// The running executable, as `std::env::current_exe` reports it.
    pub current_exe: Option<PathBuf>,
    /// Directories that may hold a file called `abylab`: the `$PATH` entries,
    /// the installer's default `~/.local/bin`, and `$ABYLAB_BIN_DIR`.
    pub dirs: Vec<PathBuf>,
    /// cargo's bin directory. A binary in there is removed through
    /// `cargo uninstall`, with the bookkeeping that goes with it.
    pub cargo_bin: Option<PathBuf>,
    /// Shell startup files that may hold the installer's PATH block.
    pub rc_files: Vec<PathBuf>,
}

/// One binary the command removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramBin {
    pub path: PathBuf,
    /// In cargo's bin directory: `cargo uninstall` has to take it.
    pub cargo: bool,
    /// Apparent bytes of the file; a symlink counts as the link itself.
    pub bytes: u64,
}

/// One startup file whose PATH block goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEdit {
    pub path: PathBuf,
    /// Marker blocks in the file right now.
    pub blocks: usize,
}

/// Why a binary survives the command. The reason is printed, so the reader
/// learns what uninstall does *not* cover before it runs, not after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramKeepWhy {
    /// Another package manager owns the path (Homebrew cellar, Nix store).
    ForeignInstaller,
}

impl ProgramKeepWhy {
    pub fn note(self) -> &'static str {
        match self {
            Self::ForeignInstaller => "owned by another installer (Homebrew / Nix)",
        }
    }
}

/// One path the command leaves alone, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramKeep {
    pub path: PathBuf,
    pub why: ProgramKeepWhy,
}

/// What an uninstall would remove from the program half, and what it would
/// not: measured once, shown to the user, then handed to [`wipe`] — so what
/// was listed is what goes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramPlan {
    pub bins: Vec<ProgramBin>,
    pub edits: Vec<PathEdit>,
    pub kept: Vec<ProgramKeep>,
}

impl ProgramPlan {
    /// No binary and no PATH block this command owns: the program half has no
    /// work to do.
    pub fn is_empty(&self) -> bool {
        self.bins.is_empty() && self.edits.is_empty()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> u64 {
        self.bins.iter().map(|bin| bin.bytes).sum()
    }
}

/// What a completed [`wipe`] did: binaries removed with the size the plan
/// promised, startup files edited, plus every path that refused to go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProgramOutcome {
    pub removed: usize,
    pub bytes: u64,
    /// Startup files the PATH block was stripped from.
    pub edited: usize,
    /// `cargo uninstall` took the cargo-bin copies, bookkeeping included.
    pub cargo_uninstalled: bool,
    /// `cargo uninstall` was tried and did not work; the file was removed
    /// directly instead, so cargo's list may still name the package.
    pub cargo_failed: bool,
    /// `(path, reason)` — a partial wipe is reported per path, never summed up
    /// into a success.
    pub failures: Vec<(PathBuf, String)>,
}

/// Find every binary this machine has and every startup file carrying the
/// installer's PATH block.
pub fn scan(input: &ScanInput) -> ProgramPlan {
    let mut plan = ProgramPlan::default();
    let mut queue: Vec<PathBuf> = Vec::new();
    if let Some(current) = &input.current_exe {
        queue.push(current.clone());
    }
    for dir in &input.dirs {
        if !dir.as_os_str().is_empty() {
            queue.push(dir.join(BIN_NAME));
        }
    }
    // A symlink is planned together with the file it lands on: the shim on
    // PATH is only half of it. Bounded per chain, and deduped by literal path.
    let mut seen: Vec<PathBuf> = Vec::new();
    while let Some(path) = queue.pop() {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        if path.file_name().and_then(|name| name.to_str()) != Some(BIN_NAME) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if foreign_installer(&path) {
            plan.kept.push(ProgramKeep {
                path,
                why: ProgramKeepWhy::ForeignInstaller,
            });
            continue;
        }
        let cargo = input
            .cargo_bin
            .as_ref()
            .is_some_and(|dir| path.parent() == Some(dir.as_path()));
        plan.bins.push(ProgramBin {
            path: path.clone(),
            cargo,
            bytes: meta.len(),
        });
        if let Some(target) = link_target(&path) {
            queue.push(target);
        }
    }
    plan.bins.sort_by(|a, b| a.path.cmp(&b.path));
    plan.kept.sort_by(|a, b| a.path.cmp(&b.path));

    for file in &input.rc_files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let blocks = marker_blocks(&text).len();
        if blocks > 0 {
            plan.edits.push(PathEdit {
                path: file.clone(),
                blocks,
            });
        }
    }
    plan
}

/// Remove every binary the plan listed and strip every PATH block. A path that
/// vanished since the scan is not a failure (there is nothing left to delete);
/// everything else that refuses to go is named in [`ProgramOutcome::failures`].
pub fn wipe(plan: &ProgramPlan) -> ProgramOutcome {
    let mut outcome = ProgramOutcome::default();
    if plan.bins.iter().any(|bin| bin.cargo) {
        outcome.cargo_uninstalled = run_cargo_uninstall();
        outcome.cargo_failed = !outcome.cargo_uninstalled;
    }
    for bin in &plan.bins {
        match std::fs::remove_file(&bin.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                outcome.failures.push((bin.path.clone(), error.to_string()));
                continue;
            }
        }
        outcome.removed += 1;
        outcome.bytes += bin.bytes;
    }
    for edit in &plan.edits {
        match strip_file(&edit.path) {
            Ok(()) => outcome.edited += 1,
            Err(error) => outcome
                .failures
                .push((edit.path.clone(), error.to_string())),
        }
    }
    outcome
}

/// `cargo uninstall abylab-tui` in cargo's own bin directory, so the entry in
/// `~/.cargo/.crates.toml` goes with the file. False when cargo is absent or
/// the package is not one cargo installed; the caller falls back to unlinking
/// the file directly (the script does the same, with a warning).
fn run_cargo_uninstall() -> bool {
    std::process::Command::new("cargo")
        .arg("uninstall")
        .arg(CARGO_PKG)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Strip the marker blocks from one startup file, re-reading it at wipe time:
/// content that changed since the scan is re-checked, and a file that no
/// longer holds the marker is simply done.
fn strip_file(path: &Path) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let spans = marker_blocks(&text);
    if spans.is_empty() {
        return Ok(());
    }
    std::fs::write(path, strip_blocks(&text, &spans))
}

/// The line spans one PATH block occupies: the marker, plus the line beneath
/// it when that line really is a PATH line. A hand-written line under the
/// marker is left where it is.
fn marker_blocks(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if line_text(lines[i]) == PATH_MARKER {
            let end = if i + 1 < lines.len() && is_path_line(lines[i + 1]) {
                i + 2
            } else {
                i + 1
            };
            spans.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    spans
}

/// A line without its terminator, whichever terminator the file uses.
fn line_text(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

/// Does this look like the line install.sh writes under the marker? Fish
/// spells it `fish_add_path`; every other shell writes `export PATH=…`.
fn is_path_line(line: &str) -> bool {
    line.contains("PATH") || line.contains("fish_add_path")
}

/// Drop the given line spans, keeping every other byte — terminators included,
/// so a file's line endings survive an edit.
fn strip_blocks(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        if spans.iter().any(|(start, end)| i >= *start && i < *end) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Follow a symlink to the file it lands on, so a shim goes together with its
/// payload. `None` when the path is not a symlink; bounded so a loop cannot
/// hang, exactly like the shell uninstaller's `link_target`.
fn link_target(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    let mut hops = 0;
    while let Ok(target) = std::fs::read_link(&current) {
        if hops >= MAX_LINK_HOPS {
            break;
        }
        current = if target.is_absolute() {
            target
        } else {
            match current.parent() {
                Some(dir) => dir.join(target),
                None => target,
            }
        };
        hops += 1;
    }
    (hops > 0).then_some(current)
}

/// A copy another package manager owns: deleting the file would leave that
/// package manager's database lying (brew upgrade, nix-collect-garbage).
fn foreign_installer(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return false;
    };
    path.starts_with("/nix/store/")
        || path.starts_with("/opt/homebrew/")
        || path.starts_with("/homebrew/")
        || path.contains("/Cellar/")
        || path.contains("/.nix-profile/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aby-uninstall-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_bin(path: &Path, body: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// The plan finds the binary in a PATH directory, follows a symlink to its
    /// payload, and refuses a file that is no longer called abylab.
    #[cfg(unix)]
    #[test]
    fn scan_finds_path_hits_payloads_and_the_running_exe() {
        let root = scratch("scan");
        let payload = root.join("libexec/abylab");
        write_bin(&payload, b"binary payload");
        let shim = root.join("path-bin/abylab");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&payload, &shim).unwrap();
        let renamed = root.join("renamed/abylab-dev");
        write_bin(&renamed, b"not ours");

        let plan = scan(&ScanInput {
            current_exe: Some(renamed),
            dirs: vec![root.join("path-bin"), root.join("empty")],
            cargo_bin: None,
            rc_files: vec![],
        });
        assert_eq!(
            plan.bins
                .iter()
                .map(|bin| bin.path.clone())
                .collect::<Vec<_>>(),
            vec![payload.clone(), shim.clone()],
            "the shim and its payload are both planned, sorted by path: {plan:?}"
        );
        assert!(plan.edits.is_empty() && plan.kept.is_empty());
        // The symlink counts as the link itself, the payload as its bytes.
        assert_eq!(
            plan.bytes(),
            std::fs::symlink_metadata(&shim).unwrap().len() + 14
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// cargo's bin is marked for `cargo uninstall`; another installer's copy is
    /// reported as kept and never queued.
    #[test]
    fn scan_separates_cargo_copies_and_foreign_installers() {
        let root = scratch("cargo");
        let cargo = root.join(".cargo/bin/abylab");
        write_bin(&cargo, b"cargo copy");
        let plan = scan(&ScanInput {
            current_exe: Some(cargo.clone()),
            dirs: vec![],
            cargo_bin: Some(root.join(".cargo/bin")),
            rc_files: vec![],
        });
        assert_eq!(plan.bins.len(), 1);
        assert!(plan.bins[0].cargo, "{plan:?}");

        // Foreign paths are recognized by shape, and reported not planned.
        for path in [
            "/opt/homebrew/bin/abylab",
            "/usr/local/Cellar/abylab/0.1.12/bin/abylab",
            "/home/linuxbrew/.linuxbrew/Cellar/abylab/0.1.12/bin/abylab",
            "/nix/store/abc-abylab/bin/abylab",
            "/Users/alice/.nix-profile/bin/abylab",
        ] {
            assert!(
                foreign_installer(Path::new(path)),
                "{path} is another installer's"
            );
        }
        assert!(!foreign_installer(Path::new(
            "/home/alice/.local/bin/abylab"
        )));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The marker is the contract: a file without it is not an edit at all,
    /// and under the marker only a real PATH line is dropped.
    #[test]
    fn scan_reads_the_marker_block_not_just_the_marker() {
        let root = scratch("marker");
        let rc = root.join(".bashrc");
        std::fs::write(
            &rc,
            "export FOO=1\n\
             # added by the abylab installer\n\
             export PATH=\"/home/alice/.local/bin:$PATH\"\n\
             alias ll='ls -l'\n\
             \n\
             # added by the abylab installer\n\
             # a comment the user wrote\n\
             keep me\n",
        )
        .unwrap();
        let untouched = root.join(".zshrc");
        std::fs::write(&untouched, "export BAR=1\n").unwrap();

        let plan = scan(&ScanInput {
            current_exe: None,
            dirs: vec![],
            cargo_bin: None,
            rc_files: vec![rc.clone(), untouched.clone()],
        });
        assert_eq!(
            plan.edits,
            vec![PathEdit {
                path: rc.clone(),
                blocks: 2
            }],
            "only the marked file is an edit: {plan:?}"
        );

        let outcome = wipe(&plan);
        assert_eq!(outcome.edited, 1);
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(&rc).unwrap(),
            "export FOO=1\n\
             alias ll='ls -l'\n\
             \n\
             # a comment the user wrote\n\
             keep me\n",
            "the PATH line under the marker goes; the hand comment under it stays"
        );
        assert_eq!(
            std::fs::read_to_string(&untouched).unwrap(),
            "export BAR=1\n",
            "a file without the marker is byte-for-byte untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A wipe removes what the plan listed, leaves everything else (including
    /// the directory the binary lived in), and a second wipe is a no-op.
    #[cfg(unix)]
    #[test]
    fn a_wipe_removes_what_the_plan_listed_and_leaves_the_rest() {
        let root = scratch("wipe");
        let bin_dir = root.join("local/bin");
        write_bin(&bin_dir.join("abylab"), b"binary payload");
        let decoy = bin_dir.join("abylab.rb");
        write_bin(&decoy, b"not ours");
        let rc = root.join(".zshrc");
        std::fs::write(
            &rc,
            "# added by the abylab installer\n\
             fish_add_path \"/home/alice/.local/bin\"\n\
             set -gx EDITOR vim\n",
        )
        .unwrap();

        let plan = scan(&ScanInput {
            current_exe: None,
            dirs: vec![bin_dir.clone()],
            cargo_bin: None,
            rc_files: vec![rc.clone()],
        });
        let outcome = wipe(&plan);
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.edited, 1);
        assert!(!outcome.cargo_uninstalled && !outcome.cargo_failed);
        assert_eq!(outcome.bytes, plan.bytes());
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert!(!bin_dir.join("abylab").exists());
        assert!(decoy.exists(), "another file in the directory stays");
        assert!(bin_dir.exists(), "the directory itself stays");
        assert_eq!(
            std::fs::read_to_string(&rc).unwrap(),
            "set -gx EDITOR vim\n"
        );
        assert!(wipe(&scan(&ScanInput {
            current_exe: None,
            dirs: vec![bin_dir],
            cargo_bin: None,
            rc_files: vec![rc],
        }))
        .failures
        .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
