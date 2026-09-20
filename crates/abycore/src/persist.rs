//! Session JSONL checkpoints. A store is either workspace-scoped
//! (`SessionStore::new` → `<workspace>/.abycore/sessions`) or shared across
//! workspaces with a slug partition (`SessionStore::at` →
//! `<root>/<workspace-slug>`).
//!
//! Only newline-terminated records are committed. Readers ignore an unfinished
//! final record; writers validate complete records and durably remove the tail
//! under an exclusive lock before appending. Corrupt or unsupported complete
//! records are errors, never a reason to restore an older checkpoint.
//!
//! The log appears on the first append. Each append is synced, with failed
//! writes rolled back before the writer can be reused. Discovery reads the
//! header and a disposable summary index bound to the log's metadata. Missing
//! or stale indexes are rebuilt by one streaming scan.

mod index;
mod log;

use crate::{Error, ErrorKind, Result, SessionSnapshot};
use log::{Discovery, scan};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PERSIST_VERSION: u32 = 1;

/// Immutable identity and configuration recorded when the log materializes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub created_at: String,
    pub cwd: String,
    pub protocol: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl SessionHeader {
    fn from_snapshot(id: &str, cwd: &str, snapshot: &SessionSnapshot) -> Self {
        Self {
            version: PERSIST_VERSION,
            id: id.into(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string(),
            cwd: cwd.into(),
            protocol: snapshot.protocol.clone(),
            model: snapshot.model.model.clone(),
            system_prompt: Some(snapshot.system_prompt.clone()),
        }
    }

    /// Encode the immutable first JSONL record.
    pub fn to_line(&self) -> String {
        let mut value = serde_json::to_value(self).expect("serializable session header");
        value["type"] = serde_json::json!("session");
        value.to_string()
    }

    fn parse(line: &[u8]) -> Result<Self> {
        let mut value: serde_json::Value =
            serde_json::from_slice(line).map_err(|_| invalid("invalid session header JSON"))?;
        if value.get("type").and_then(serde_json::Value::as_str) != Some("session") {
            return Err(invalid("session log is missing its header"));
        }
        value
            .as_object_mut()
            .expect("typed header object")
            .remove("type");
        serde_json::from_value(value).map_err(|_| invalid("invalid or unsupported session header"))
    }

    fn validate(&self, id: &str) -> Result<()> {
        if self.id != id {
            return Err(invalid("session log identity mismatch"));
        }
        if self.version > PERSIST_VERSION {
            return Err(invalid(
                "session log was written by a newer abycore — upgrade required",
            ));
        }
        if self.version != PERSIST_VERSION || self.protocol != "deepseek-messages" {
            return Err(invalid(
                "unsupported session version or protocol; expected deepseek-messages (legacy Responses logs are not migrated automatically)",
            ));
        }
        Ok(())
    }
}

/// Discovery metadata. `modified` is the log file's last modification time.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub dir: PathBuf,
    pub file: PathBuf,
    pub modified: SystemTime,
    pub created_at: String,
    /// First nonempty user prompt, limited to 160 characters.
    pub preview: String,
    /// Most recent stored title.
    pub title: Option<String>,
}

/// A session log store. Two layouts:
///
/// - [`SessionStore::new`] — per-workspace store rooted at
///   `<workspace>/.abycore/sessions/<id>` (the embedding default).
/// - [`SessionStore::at`] — a caller-specified shared root (e.g.
///   `~/.abycli/sessions`) partitioned per workspace:
///   `<root>/<workspace-slug>/<id>/session.jsonl`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    workspace: PathBuf,
    root: PathBuf,
    /// Workspace slug under a shared root (`None` for per-workspace roots).
    slug: Option<String>,
}

/// `/Users/x/proj` → `--Users-x-proj--`: the workspace partition under a
/// shared store root.
pub(crate) fn workspace_slug(workspace: &Path) -> String {
    format!("-{}--", workspace.to_string_lossy().replace('/', "-"))
}

impl SessionStore {
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = canonical_workspace(workspace)?;
        let root = workspace.join(".abycore/sessions");
        Ok(Self {
            workspace,
            root,
            slug: None,
        })
    }

    /// Store shared across workspaces, partitioned by the workspace slug.
    /// `root` names the parent (e.g. `~/.abycli/sessions`); `workspace`
    /// remains the session's cwd and selects the partition.
    pub fn at(root: impl AsRef<Path>, workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = canonical_workspace(workspace)?;
        let root = root.as_ref().to_path_buf();
        let slug = workspace_slug(&workspace);
        Ok(Self {
            workspace,
            root,
            slug: Some(slug),
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub fn data_dir(&self) -> PathBuf {
        self.workspace.join(".abycore")
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_of(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() {
            return Err(invalid("session id is empty"));
        }
        let mut dir = self.root.clone();
        if let Some(slug) = &self.slug {
            dir = dir.join(Self::escape_segment(slug));
        }
        dir = dir.join(Self::escape_segment(id));
        Ok(dir)
    }

    fn log_file(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir_of(id)?.join("session.jsonl"))
    }

    pub fn log_path(&self, id: &str) -> Result<PathBuf> {
        self.log_file(id)
    }

    /// Append a validated snapshot. Several checkpoints may share a run sequence;
    /// the last committed record is the state returned by `load`.
    pub fn append_checkpoint(
        &self,
        writer: &mut SessionWriter,
        seq: u64,
        snapshot: &SessionSnapshot,
    ) -> Result<()> {
        snapshot.validate()?;
        let line =
            serde_json::json!({"type":"snapshot", "seq":seq, "snapshot":snapshot}).to_string();
        let mut discovery = writer.discovery.clone();
        discovery.observe(snapshot);
        writer.append(&line, discovery)
    }

    /// Read headers and summary indexes, ordered by log modification time.
    /// Legacy, missing or stale indexes require one streaming scan and a
    /// best-effort index rebuild. Index writes are optional on read-only storage.
    /// Discovery does not validate cached transcript bodies; `load` always does.
    pub fn list(&self) -> Result<Vec<SessionSummary>> {
        let scan = match &self.slug {
            Some(slug) => self.root.join(Self::escape_segment(slug)),
            None => self.root.clone(),
        };
        let entries = match fs::read_dir(&scan) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(io_error("list sessions", error)),
        };
        let mut summaries = vec![];
        for entry in entries.flatten() {
            if let Ok(summary) = self.summary(&entry.path()) {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
        Ok(summaries)
    }

    fn summary(&self, dir: &Path) -> Result<SessionSummary> {
        let file = dir.join("session.jsonl");
        let mut reader =
            BufReader::new(fs::File::open(&file).map_err(|e| io_error("read session", e))?);
        let metadata = reader
            .get_ref()
            .metadata()
            .map_err(|e| io_error("inspect session", e))?;
        let mut line = vec![];
        reader
            .read_until(b'\n', &mut line)
            .map_err(|e| io_error("read header", e))?;
        if line.last() != Some(&b'\n') {
            return Err(invalid("session has no committed header"));
        }
        let header = SessionHeader::parse(&line)?;
        header.validate(&header.id)?;
        if self.dir_of(&header.id)? != dir {
            return Err(invalid("session directory identity mismatch"));
        }
        let discovery = match index::read(dir, &metadata) {
            Some(discovery) => discovery,
            None => {
                reader
                    .seek(SeekFrom::Start(0))
                    .map_err(|e| io_error("seek session", e))?;
                let scanned = scan(reader, &header.id)?;
                // An overlapping append makes this index stale, which the next read detects.
                let _ = index::write(dir, &metadata, &scanned.discovery);
                scanned.discovery
            }
        };
        if !discovery.has_snapshot {
            return Err(invalid("session has no checkpoint to resume"));
        }
        Ok(SessionSummary {
            id: header.id,
            dir: dir.into(),
            file,
            modified: metadata
                .modified()
                .map_err(|e| io_error("read modification time", e))?,
            created_at: header.created_at,
            preview: discovery.preview,
            title: discovery.title,
        })
    }

    /// Acquire the session's exclusive writer lock. Existing logs are validated
    /// and torn tails repaired before returning; new logs materialize on append.
    pub fn create(&self, id: &str, snapshot: &SessionSnapshot) -> Result<SessionWriter> {
        snapshot.validate()?;
        let dir = self.dir_of(id)?;
        fs::create_dir_all(&dir).map_err(|e| io_error("create session directory", e))?;
        // Persist each newly reachable directory entry before a log can be
        // committed (the walk stops at the store root — it always exists).
        let mut child = dir.as_path();
        while child != self.root {
            let parent = child
                .parent()
                .ok_or_else(|| invalid("session directory outside the store root"))?;
            sync_dir(parent).map_err(|e| io_error("sync session directories", e))?;
            child = parent;
        }
        let header = SessionHeader::from_snapshot(id, &self.workspace.to_string_lossy(), snapshot);
        SessionWriter::open(dir, header)
    }

    /// Load the last newline-terminated checkpoint, validating every complete
    /// record. An unfinished tail is ignored without modifying the file.
    pub fn load(&self, id: &str) -> Result<(SessionHeader, SessionSnapshot)> {
        let file = fs::File::open(self.log_file(id)?).map_err(|e| io_error("read session", e))?;
        let scanned = scan(BufReader::new(file), id)?;
        let header = scanned
            .header
            .ok_or_else(|| invalid("session log has no committed header"))?;
        let snapshot = scanned
            .snapshot
            .ok_or_else(|| invalid("session log has no checkpoint to resume"))?;
        Ok((header, snapshot))
    }

    /// Return the most recent committed title, or None for absent/invalid logs.
    pub fn title_of(&self, id: &str) -> Option<String> {
        let file = fs::File::open(self.log_file(id).ok()?).ok()?;
        let metadata = file.metadata().ok()?;
        if let Some(discovery) = index::read(&self.dir_of(id).ok()?, &metadata) {
            return discovery.title;
        }
        scan(BufReader::new(file), id).ok()?.discovery.title
    }

    pub fn set_title(&self, writer: &mut SessionWriter, title: &str) -> Result<()> {
        let mut discovery = writer.discovery.clone();
        discovery.title = Some(title.into());
        writer.append(
            &serde_json::json!({"type":"title", "title":title}).to_string(),
            discovery,
        )
    }

    /// Encode UTF-8 bytes; reserve the complete `.` and `..` segments too.
    fn escape_segment(id: &str) -> String {
        if id == "." {
            return "~2E".into();
        }
        if id == ".." {
            return "~2E~2E".into();
        }
        let mut out = String::new();
        for byte in id.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                out.push(byte as char);
            } else {
                out.push_str(&format!("~{byte:02X}"));
            }
        }
        out
    }
}

/// An exclusive append handle. A successful append is durable even if its
/// disposable discovery index could not be refreshed.
#[derive(Debug)]
pub struct SessionWriter {
    id: String,
    dir: PathBuf,
    file: PathBuf,
    header: SessionHeader,
    len: u64,
    discovery: Discovery,
    log: Option<fs::File>,
    lock: fs::File,
    poisoned: bool,
}

impl SessionWriter {
    fn open(dir: PathBuf, header: SessionHeader) -> Result<Self> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("session.lock"))
            .map_err(|e| io_error("open session lock", e))?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| {
                invalid(format!(
                    "session {} is already open by another writer (lock held)",
                    header.id
                ))
            },
        )?;
        let mut writer = Self {
            id: header.id.clone(),
            file: dir.join("session.jsonl"),
            dir,
            header,
            len: 0,
            discovery: Discovery::default(),
            log: None,
            lock,
            poisoned: false,
        };
        match fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&writer.file)
        {
            Ok(file) => {
                writer.log = Some(file);
                writer.truncate_to_last_line()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("open session log", error)),
        }
        Ok(writer)
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Append one valid title or snapshot record, adding its newline.
    /// Invalid JSON, unknown records and invalid snapshots are rejected before I/O.
    pub fn append_line(&mut self, line: &str) -> Result<()> {
        if line.contains('\n') {
            return Err(invalid("session log lines must not contain newlines"));
        }
        let event = log::Event::parse(line.as_bytes())?;
        let mut discovery = self.discovery.clone();
        discovery.apply(&event);
        self.append(line, discovery)
    }

    fn append(&mut self, line: &str, discovery: Discovery) -> Result<()> {
        if self.poisoned {
            return Err(invalid(
                "session writer requires reopening after an I/O failure",
            ));
        }
        if self.log.is_none() {
            match fs::OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(&self.file)
            {
                Ok(file) => self.log = Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.log = Some(
                        fs::OpenOptions::new()
                            .read(true)
                            .append(true)
                            .open(&self.file)
                            .map_err(|e| io_error("open session log", e))?,
                    );
                    self.truncate_to_last_line()?;
                }
                Err(error) => return Err(io_error("create session log", error)),
            }
        }
        let mut data = Vec::new();
        if self.len == 0 {
            data.extend_from_slice(self.header.to_line().as_bytes());
            data.push(b'\n');
        }
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        let file = self.log.as_mut().expect("opened log");
        if let Err(error) = file
            .write_all(&data)
            .and_then(|_| file.sync_all())
            .and_then(|_| sync_dir(&self.dir))
        {
            if file
                .set_len(self.len)
                .and_then(|_| file.sync_all())
                .is_err()
            {
                self.poisoned = true;
            }
            return Err(io_error("append session checkpoint", error));
        }
        self.len += data.len() as u64;
        self.discovery = discovery;
        if let Ok(metadata) = file.metadata() {
            let _ = index::write(&self.dir, &metadata, &self.discovery);
        }
        Ok(())
    }

    /// Validate committed records and durably remove only an unterminated tail.
    /// Called automatically on reopening; safe to call again while holding this writer.
    pub fn truncate_to_last_line(&mut self) -> Result<u64> {
        let Some(file) = self.log.as_mut() else {
            return Ok(0);
        };
        self.poisoned = true;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io_error("seek session log", e))?;
        let scanned = scan(BufReader::new(&mut *file), &self.id)?;
        let actual = file
            .metadata()
            .map_err(|e| io_error("inspect session log", e))?
            .len();
        if actual != scanned.committed_len {
            file.set_len(scanned.committed_len)
                .and_then(|_| file.sync_all())
                .map_err(|e| io_error("repair session tail", e))?;
        }
        self.len = scanned.committed_len;
        if let Some(header) = scanned.header {
            self.header = header;
        }
        self.discovery = scanned.discovery;
        self.poisoned = false;
        Ok(self.len)
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        // A concurrent fork can retain this open file description until exec.
        // Closing only our descriptor would leave its flock held in that child.
        let _ = rustix::fs::flock(&self.lock, rustix::fs::FlockOperation::Unlock);
    }
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

fn canonical_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf> {
    let workspace = fs::canonicalize(workspace).map_err(|_| {
        Error::new(
            ErrorKind::Configuration,
            "workspace must be an accessible directory for persistence",
        )
    })?;
    if !workspace.is_dir() {
        return Err(Error::new(
            ErrorKind::Configuration,
            "workspace must be a directory",
        ));
    }
    Ok(workspace)
}
fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Session, message)
}
fn io_error(action: &str, error: std::io::Error) -> Error {
    invalid(format!("cannot {action}: {error}"))
}

#[cfg(test)]
mod tests;
