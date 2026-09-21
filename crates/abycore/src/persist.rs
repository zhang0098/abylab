//! Session JSONL checkpoints. A store is either workspace-scoped
//! (`SessionStore::new` → `<workspace>/.abycore/sessions`) or shared across
//! workspaces with a slug partition (`SessionStore::at` →
//! `<root>/<workspace-slug>`).
//!
//! Only newline-terminated records are committed. Readers ignore an unfinished
//! final record; corrupt or unsupported complete records are errors, never a
//! reason to restore an older checkpoint.
//!
//! The log holds the header, the newest title and the newest checkpoint;
//! snapshots are replaced through a synced temp file plus an atomic rename, so
//! a crash leaves either the previous log or the new one — never a torn
//! snapshot. Reopening adopts an existing log, repairs an unfinished tail and
//! rewrites legacy multi-snapshot logs to this compact form on the next append.
//! Discovery reads the header and a disposable summary index bound to the
//! log's metadata. Missing or stale indexes are rebuilt by one streaming scan.

mod index;
mod log;

use crate::{Error, ErrorKind, Result, SessionSnapshot};
use log::{Discovery, scan};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PERSIST_VERSION: u32 = 1;
/// Anchor once every this many delta records so resume reads stay bounded.
const DELTAS_PER_ANCHOR: u32 = 64;

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

    fn validate_workspace(&self, workspace: &Path) -> Result<()> {
        if Path::new(&self.cwd) != workspace {
            return Err(invalid("session belongs to a different workspace"));
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

/// A bounded, stable partition of the canonical path, including its raw bytes.
pub(crate) fn workspace_slug(workspace: &Path) -> String {
    format!(
        "ws-{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    )
}

fn legacy_workspace_slug(workspace: &Path) -> String {
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
        // Continue verified legacy sessions in place, using their original
        // lock file. Moving them would split locks with an older live writer.
        if !dir
            .try_exists()
            .map_err(|e| io_error("inspect session directory", e))?
            && let Some(legacy) = self.legacy_partition()
        {
            let legacy = legacy.join(Self::escape_segment(id));
            match fs::File::open(legacy.join("session.jsonl")) {
                Ok(file) => {
                    let header = read_header(BufReader::new(file))?;
                    header.validate(id)?;
                    if header.validate_workspace(&self.workspace).is_ok() {
                        return Ok(legacy);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("read legacy session", error)),
            }
        }
        Ok(dir)
    }

    fn legacy_partition(&self) -> Option<PathBuf> {
        self.slug.as_ref().map(|_| {
            self.root.join(Self::escape_segment(&legacy_workspace_slug(
                &self.workspace,
            )))
        })
    }

    fn log_file(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir_of(id)?.join("session.jsonl"))
    }

    pub fn log_path(&self, id: &str) -> Result<PathBuf> {
        self.log_file(id)
    }

    /// Record the newest checkpoint. The writer decides between an incremental
    /// delta record (appended, the common case) and a full anchor rewrite that
    /// also discards accumulated deltas. Several checkpoints may share a run
    /// sequence; the folded last record is the state returned by `load`.
    pub fn append_checkpoint(
        &self,
        writer: &mut SessionWriter,
        seq: u64,
        snapshot: &SessionSnapshot,
    ) -> Result<()> {
        snapshot.validate()?;
        let mut discovery = writer.discovery.clone();
        discovery.observe(snapshot);
        writer.commit_snapshot(seq, snapshot, discovery)
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
        let mut summaries = vec![];
        for partition in std::iter::once(scan).chain(self.legacy_partition()) {
            let entries = match fs::read_dir(&partition) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("list sessions", error)),
            };
            for entry in entries.flatten() {
                if let Ok(summary) = self.summary(&entry.path()) {
                    summaries.push(summary);
                }
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
        header.validate_workspace(&self.workspace)?;
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
    /// For a new identity prefer [`Self::create_new`]; to continue existing
    /// history use [`Self::open_for_resume`] instead of loading before locking.
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

    /// Reserve a new session id without replacing an existing committed log.
    pub fn create_new(&self, id: &str, snapshot: &SessionSnapshot) -> Result<SessionWriter> {
        let writer = self.create(id, snapshot)?;
        if writer.len != 0 {
            return Err(invalid(
                "session already exists; resume it or choose a new id",
            ));
        }
        Ok(writer)
    }

    /// Acquire the exclusive writer before reading the authoritative snapshot.
    /// Keep the returned writer alive for the entire resumed session.
    pub fn open_for_resume(&self, id: &str) -> Result<(SessionWriter, SessionSnapshot)> {
        let header = SessionHeader::from_snapshot(
            id,
            &self.workspace.to_string_lossy(),
            &SessionSnapshot::new("", crate::ModelOptions::default()),
        );
        let writer = SessionWriter::open(self.dir_of(id)?, header)?;
        let snapshot = writer
            .state
            .clone()
            .ok_or_else(|| invalid("session log has no checkpoint to resume"))?;
        Ok((writer, snapshot))
    }

    /// Load the last newline-terminated checkpoint, validating every complete
    /// record. An unfinished tail is ignored without modifying the file.
    /// This is a read-only view, not a reservation for later writes. Continuing
    /// the session requires [`Self::open_for_resume`].
    pub fn load(&self, id: &str) -> Result<(SessionHeader, SessionSnapshot)> {
        let file = fs::File::open(self.log_file(id)?).map_err(|e| io_error("read session", e))?;
        let scanned = scan(BufReader::new(file), id)?;
        let header = scanned
            .header
            .ok_or_else(|| invalid("session log has no committed header"))?;
        header.validate_workspace(&self.workspace)?;
        let snapshot = scanned
            .snapshot
            .ok_or_else(|| invalid("session log has no checkpoint to resume"))?;
        Ok((header, snapshot))
    }

    /// Return the most recent committed title, or None for absent/invalid logs.
    pub fn title_of(&self, id: &str) -> Option<String> {
        let dir = self.dir_of(id).ok()?;
        let file = fs::File::open(dir.join("session.jsonl")).ok()?;
        let metadata = file.metadata().ok()?;
        let mut reader = BufReader::new(file);
        let header = read_header(&mut reader).ok()?;
        header.validate(id).ok()?;
        header.validate_workspace(&self.workspace).ok()?;
        if let Some(discovery) = index::read(&dir, &metadata) {
            return discovery.title;
        }
        reader.seek(SeekFrom::Start(0)).ok()?;
        scan(reader, id).ok()?.discovery.title
    }

    pub fn set_title(&self, writer: &mut SessionWriter, title: &str) -> Result<()> {
        let mut discovery = writer.discovery.clone();
        discovery.title = Some(title.into());
        writer.commit_title(&discovery)
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

/// An exclusive log handle. The log holds one header, one title, one anchor
/// and appended deltas. Anchor commits rewrite it through a synced temp file
/// and an atomic rename; delta commits append one record. A failed rewrite
/// never damages the previous state.
#[derive(Debug)]
pub struct SessionWriter {
    id: String,
    dir: PathBuf,
    file: PathBuf,
    header: SessionHeader,
    len: u64,
    discovery: Discovery,
    /// The most recent committed state: the delta baseline and the source for
    /// anchor rewrites.
    state: Option<SessionSnapshot>,
    deltas_since_anchor: u32,
    /// Byte length of the newest anchor record, including its newline; 0 when
    /// the log has no anchor yet. Deltas larger than half of this are
    /// demoted to anchors.
    anchor_bytes: usize,
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
            state: None,
            deltas_since_anchor: 0,
            anchor_bytes: 0,
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

    /// The last committed title, read under this writer's exclusive lock.
    pub fn title(&self) -> Option<&str> {
        self.discovery.title.as_deref()
    }

    /// Commit one valid title or snapshot record. Invalid JSON, unknown
    /// records, invalid snapshots and delta records are rejected before I/O:
    /// deltas are writer-internal; external callers express full state.
    pub fn append_line(&mut self, line: &str) -> Result<()> {
        if line.contains('\n') {
            return Err(invalid("session log lines must not contain newlines"));
        }
        match log::Event::parse(line.as_bytes())? {
            log::Event::Title { title, .. } => {
                let mut discovery = self.discovery.clone();
                discovery.title = Some(title);
                self.commit_title(&discovery)
            }
            log::Event::Snapshot { _seq, snapshot, .. } => {
                let mut discovery = self.discovery.clone();
                discovery.observe(&snapshot);
                self.commit_snapshot(_seq, &snapshot, discovery)
            }
            log::Event::Delta { .. } => Err(invalid(
                "session log delta records are writer-internal; append a full snapshot instead",
            )),
        }
    }

    /// Decide between an appended delta record and a full anchor rewrite.
    fn commit_snapshot(
        &mut self,
        seq: u64,
        snapshot: &SessionSnapshot,
        discovery: Discovery,
    ) -> Result<()> {
        if self.poisoned {
            return Err(invalid(
                "session writer requires reopening after a validation failure",
            ));
        }
        let must_anchor = match &self.state {
            None => true,
            Some(state) => {
                snapshot.model != state.model
                    || snapshot.system_prompt != state.system_prompt
                    || snapshot.items.len() < state.items.len()
                    || snapshot.requests.len() < state.requests.len()
                    || self.deltas_since_anchor >= DELTAS_PER_ANCHOR
            }
        };
        if must_anchor {
            return self.commit_anchor(seq, snapshot, discovery);
        }
        let base = self
            .state
            .as_ref()
            .expect("the anchor decision above guarantees a baseline");
        // Tail replacement: everything from the first divergent record onward
        // is re-sent, so in-place updates of committed records (usage fields
        // filling in after a dispatch) survive the fold.
        let items_from = diverge_point(&base.items, &snapshot.items);
        let requests_from = diverge_point(&base.requests, &snapshot.requests);
        let delta = serde_json::json!({
            "type":"delta", "seq":seq, "time":now_ms(),
            "items":{"from":items_from, "added": &snapshot.items[items_from..]},
            "requests":{"from":requests_from, "added": &snapshot.requests[requests_from..]},
            "pending":snapshot.pending,
            "needs_response":snapshot.needs_response,
            "run_sequence":snapshot.run_sequence,
            "todos":snapshot.todos,
            "compactions":snapshot.compactions,
            "prune":snapshot.prune,
            "goal":snapshot.goal,
        })
        .to_string();
        if self.anchor_bytes > 0 && delta.len() > self.anchor_bytes / 2 {
            // The new items dwarf the baseline: an anchor is the cheaper
            // representation and it also resets the replay window.
            return self.commit_anchor(seq, snapshot, discovery);
        }
        self.append_delta(&delta, snapshot.clone(), discovery)
    }

    /// Append one delta record and advance the in-memory baseline.
    fn append_delta(
        &mut self,
        line: &str,
        state: SessionSnapshot,
        discovery: Discovery,
    ) -> Result<()> {
        if self.log.is_none()
            && let Err(error) = self.adopt_existing_log()
        {
            return Err(error);
        }
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        let file = self.log.as_mut().expect("delta append requires the log");
        let previous = self.len;
        if let Err(error) = file.write_all(&data).and_then(|_| file.sync_all()) {
            if file
                .set_len(previous)
                .and_then(|_| file.sync_all())
                .is_err()
            {
                self.poisoned = true;
            }
            return Err(io_error("append session delta", error));
        }
        // An existing file entry cannot change; only the file's own fsync
        // matters, so the directory sync that anchors pay is skipped here.
        self.len += data.len() as u64;
        self.deltas_since_anchor += 1;
        self.state = Some(state);
        self.discovery = discovery;
        if let Ok(metadata) = file.metadata() {
            let _ = index::write(&self.dir, &metadata, &self.discovery);
        }
        Ok(())
    }

    /// Materialize a title update. With committed state the rewrite re-anchors
    /// it: accumulated deltas fold into the new anchor and the title stays.
    fn commit_title(&mut self, discovery: &Discovery) -> Result<()> {
        if self.poisoned {
            return Err(invalid(
                "session writer requires reopening after a validation failure",
            ));
        }
        let anchor = self.state.as_ref().map(|state| {
            serde_json::json!({"type":"snapshot", "seq":state.run_sequence, "time":now_ms(), "snapshot":state})
                .to_string()
        });
        self.rewrite(discovery.clone(), anchor.map(|anchor| (anchor, None)))
    }

    /// Rewrite the log to a full anchor from the given snapshot.
    fn commit_anchor(
        &mut self,
        seq: u64,
        snapshot: &SessionSnapshot,
        discovery: Discovery,
    ) -> Result<()> {
        let anchor =
            serde_json::json!({"type":"snapshot", "seq":seq, "time":now_ms(), "snapshot":snapshot})
                .to_string();
        self.rewrite(discovery, Some((anchor, Some(snapshot.clone()))))
    }

    /// Rewrite the log to `header, title?, anchor?` via a synced temp file and
    /// an atomic rename. `anchor: Some` re-baselines the writer: the folded
    /// deltas become the new anchor, the delta baseline resets to that state
    /// (`Some(state)`) or is dropped to a stateless writer (`None`, the
    /// title-only materialization). A failure before the rename leaves the
    /// previous log untouched; a failure after the rename (directory sync)
    /// leaves the new content committed.
    fn rewrite(
        &mut self,
        discovery: Discovery,
        anchor: Option<(String, Option<SessionSnapshot>)>,
    ) -> Result<()> {
        if self.log.is_none()
            && let Err(error) = self.adopt_existing_log()
        {
            return Err(error);
        }
        let mut body = Vec::with_capacity(256);
        body.extend_from_slice(self.header.to_line().as_bytes());
        body.push(b'\n');
        if let Some(title) = &discovery.title {
            body.extend_from_slice(
                serde_json::json!({"type":"title", "title":title, "time":now_ms()})
                    .to_string()
                    .as_bytes(),
            );
            body.push(b'\n');
        }
        if let Some((anchor_line, _)) = &anchor {
            body.extend_from_slice(anchor_line.as_bytes());
            body.push(b'\n');
        }

        let temp = tempfile::NamedTempFile::new_in(&self.dir)
            .map_err(|e| io_error("create session log temp file", e))?;
        {
            let mut file = temp.as_file();
            file.write_all(&body)
                .and_then(|_| file.sync_all())
                .map_err(|e| io_error("write session log", e))?;
        }
        temp.persist(&self.file)
            .map_err(|error| io_error("replace session log", error.error))?;
        let synced = sync_dir(&self.dir);
        if let Some((anchor_line, state)) = anchor {
            self.deltas_since_anchor = 0;
            self.anchor_bytes = anchor_line.len() + 1;
            self.state = state;
        }
        self.discovery = discovery;
        self.len = body.len() as u64;
        self.refresh_log();
        if let Ok(metadata) = fs::metadata(&self.file) {
            let _ = index::write(&self.dir, &metadata, &self.discovery);
        }
        synced.map_err(|e| io_error("sync session directory", e))
    }

    /// Open an externally created log and adopt its validated state. A corrupt
    /// complete record poisons the writer: retrying the commit must keep
    /// failing until the file is repaired. A missing log is not an error:
    /// the first commit materializes it.
    fn adopt_existing_log(&mut self) -> Result<()> {
        match fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.file)
        {
            Ok(file) => {
                self.log = Some(file);
                self.truncate_to_last_line()?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("open session log", error)),
        }
    }

    /// Reopen the in-memory handle onto the current log file so later delta
    /// appends and tail repairs address the replaced inode, not the retired
    /// one. A failed reopen drops the stale handle; the next append adopts
    /// the current file instead of writing into the retired inode.
    fn refresh_log(&mut self) {
        match fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.file)
        {
            Ok(file) => self.log = Some(file),
            Err(_) => self.log = None,
        }
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
        if let Some(header) = &scanned.header {
            header.validate_workspace(Path::new(&self.header.cwd))?;
        }
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
        if let Some(snapshot) = &scanned.snapshot {
            // The folded state becomes the delta baseline; a corrupt fold must
            // poison the writer like any other complete corruption.
            snapshot.validate()?;
        }
        self.state = scanned.snapshot;
        self.deltas_since_anchor = scanned.deltas_since_anchor;
        self.anchor_bytes = scanned.anchor_bytes;
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

fn read_header(mut reader: impl BufRead) -> Result<SessionHeader> {
    let mut line = vec![];
    reader
        .read_until(b'\n', &mut line)
        .map_err(|e| io_error("read header", e))?;
    if line.last() != Some(&b'\n') {
        return Err(invalid("session has no committed header"));
    }
    SessionHeader::parse(&line)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Index of the first record where `next` leaves the committed `base` — the
/// replace-from point for a delta. Equal prefixes (the common append case)
/// yield `base.len()`.
fn diverge_point<T: PartialEq>(base: &[T], next: &[T]) -> usize {
    let mut index = 0;
    let shared = base.len().min(next.len());
    while index < shared && base[index] == next[index] {
        index += 1;
    }
    index
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
