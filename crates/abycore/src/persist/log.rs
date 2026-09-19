use super::{Result, SessionHeader, SessionSnapshot, invalid, io_error};
use crate::{ContentPart, Item, MessageRole};
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
            Event::Title { title } => self.title = Some(title.clone()),
            Event::Snapshot { snapshot, .. } => self.observe(snapshot),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Event {
    Title {
        title: String,
    },
    Snapshot {
        #[serde(rename = "seq")]
        _seq: u64,
        // Boxed: the snapshot dwarfs the header event, and one log line is
        // materialized at a time.
        snapshot: Box<SessionSnapshot>,
    },
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
    pub snapshot: Option<SessionSnapshot>,
    pub discovery: Discovery,
    pub committed_len: u64,
}

/// Retain at most one record buffer and the most recent decoded snapshot.
pub(super) fn scan(mut reader: impl BufRead, id: &str) -> Result<Scanned> {
    let mut scanned = Scanned {
        header: None,
        snapshot: None,
        discovery: Discovery::default(),
        committed_len: 0,
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
            if let Event::Snapshot { snapshot, .. } = event {
                scanned.snapshot = Some(*snapshot);
            }
        }
        scanned.committed_len += count as u64;
    }
    Ok(scanned)
}
