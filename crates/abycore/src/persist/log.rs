use super::{Result, SessionHeader, SessionSnapshot, invalid, io_error};
use crate::{
    Compaction, ContentPart, Goal, Item, MessageRole, PendingCall, Prune, RequestRecord, TodoItem,
};
use serde::{Deserialize, Serialize};
use std::io::BufRead;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Discovery {
    pub preview: String,
    pub title: Option<String>,
    pub has_snapshot: bool,
}

impl Discovery {
    pub fn observe(&mut self, snapshot: &SessionSnapshot) {
        self.has_snapshot = true;
        if !self.preview.is_empty() {
            return;
        }
        for item in &snapshot.items {
            if let Item::Message {
                role: MessageRole::User,
                content,
                ..
            } = item
            {
                self.preview = content
                    .iter()
                    .map(ContentPart::text)
                    .flat_map(str::chars)
                    .take(160)
                    .collect();
                if !self.preview.is_empty() {
                    break;
                }
            }
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Title { title, .. } => self.title = Some(title.clone()),
            Event::Snapshot { snapshot, .. } => self.observe(snapshot),
            // scan observes the resulting snapshot after folding this delta.
            Event::Delta { .. } => self.has_snapshot = true,
        }
    }
}

/// Tail replacement of an append-only collection: the committed prefix up to
/// `from` stays, everything from `from` onward is replaced by `added`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Span<T> {
    pub from: usize,
    pub added: Vec<T>,
}

/// A delta record's payload. Valid only after an anchor: appended items and
/// request records from the first divergent record onward, plus the small
/// fields replaced wholesale.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeltaEvent {
    #[serde(rename = "seq")]
    pub _seq: u64,
    /// Commit wall-clock time in Unix epoch milliseconds; absent on older logs.
    /// Parsed to keep `deny_unknown_fields` strict; read by debugging, not code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(dead_code)]
    pub time: Option<u64>,
    pub items: Span<Item>,
    pub requests: Span<RequestRecord>,
    pub pending: Vec<PendingCall>,
    pub needs_response: bool,
    pub run_sequence: u64,
    pub todos: Option<Vec<TodoItem>>,
    pub compactions: Vec<Compaction>,
    pub prune: Option<Prune>,
    pub goal: Option<Goal>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Event {
    Title {
        title: String,
        /// Commit wall-clock time in Unix epoch milliseconds; absent on older logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[allow(dead_code)]
        time: Option<u64>,
    },
    /// A complete checkpoint anchor. `load` returns the last anchor with all
    /// following deltas folded in.
    Snapshot {
        #[serde(rename = "seq")]
        _seq: u64,
        /// Commit wall-clock time in Unix epoch milliseconds; absent on older logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[allow(dead_code)]
        time: Option<u64>,
        // Boxed: the snapshot dwarfs the header event, and one log line is
        // materialized at a time.
        snapshot: Box<SessionSnapshot>,
    },
    /// An incremental checkpoint.
    Delta(Box<DeltaEvent>),
}

impl Event {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let event: Self = serde_json::from_slice(bytes).map_err(|_| {
            invalid("corrupt or unsupported complete session record; upgrade may be required")
        })?;
        if let Self::Snapshot { snapshot, .. } = &event {
            snapshot.validate()?;
        }
        Ok(event)
    }
}

pub(super) struct Scanned {
    pub header: Option<SessionHeader>,
    /// The newest anchor with every following delta folded in.
    pub snapshot: Option<SessionSnapshot>,
    pub discovery: Discovery,
    pub committed_len: u64,
    /// Delta records seen after the newest anchor.
    pub deltas_since_anchor: u32,
    /// Byte length of the newest anchor record, including its newline; 0 when
    /// no anchor was scanned.
    pub anchor_bytes: usize,
}

/// Retain at most one record buffer and the most recent folded snapshot.
pub(super) fn scan(mut reader: impl BufRead, id: &str) -> Result<Scanned> {
    let mut scanned = Scanned {
        header: None,
        snapshot: None,
        discovery: Discovery::default(),
        committed_len: 0,
        deltas_since_anchor: 0,
        anchor_bytes: 0,
    };
    let mut line = vec![];
    loop {
        line.clear();
        let count = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| io_error("read session record", e))?;
        if count == 0 || line.last() != Some(&b'\n') {
            break;
        }
        if scanned.header.is_none() {
            let header = SessionHeader::parse(&line)?;
            header.validate(id)?;
            scanned.header = Some(header);
        } else {
            let event = Event::parse(&line)?;
            scanned.discovery.apply(&event);
            match event {
                Event::Title { .. } => {}
                Event::Snapshot { snapshot, .. } => {
                    scanned.snapshot = Some(*snapshot);
                    scanned.deltas_since_anchor = 0;
                    scanned.anchor_bytes = count;
                }
                Event::Delta(delta) => {
                    let DeltaEvent {
                        items,
                        requests,
                        pending,
                        needs_response,
                        run_sequence,
                        todos,
                        compactions,
                        prune,
                        goal,
                        ..
                    } = *delta;
                    let snapshot = scanned.snapshot.as_mut().ok_or_else(|| {
                        invalid("session log records a delta before any checkpoint anchor")
                    })?;
                    fold_span(&mut snapshot.items, &items)?;
                    fold_span(&mut snapshot.requests, &requests)?;
                    snapshot.pending = pending;
                    snapshot.needs_response = needs_response;
                    snapshot.run_sequence = run_sequence;
                    snapshot.todos = todos;
                    snapshot.compactions = compactions;
                    snapshot.prune = prune;
                    snapshot.goal = goal;
                    // Each committed record must stand on its own; a later
                    // delta or anchor must never hide earlier corruption.
                    snapshot.validate()?;
                    scanned.discovery.observe(snapshot);
                    scanned.deltas_since_anchor += 1;
                }
            }
        }
        scanned.committed_len += count as u64;
    }
    Ok(scanned)
}

/// Replace everything from `from` onward with the delta's tail. A `from`
/// beyond the folded length is a gap between records: the log is corrupt.
fn fold_span<T: Clone>(items: &mut Vec<T>, span: &Span<T>) -> Result<()> {
    if span.from > items.len() {
        return Err(invalid(
            "session delta replaces past the committed transcript",
        ));
    }
    items.truncate(span.from);
    items.extend(span.added.iter().cloned());
    Ok(())
}
