//! Host-owned input delivery. Queue records retain their stable identity until
//! a committed transcript contains the same id. All queue IO is serialized on
//! a blocking worker, including migrations and crash-recovery reconciliation.
use super::{blocking::BlockingOwner, *};
use std::collections::HashSet;

#[derive(Clone)]
pub(super) struct HostInbox {
    owner: BlockingOwner<State>,
}

#[derive(Clone)]
struct Row {
    item_id: u64,
    record: crate::queue_store::QueuedRecord,
    held: bool,
    in_flight: bool,
    steering: bool,
    visible: bool,
}

struct State {
    cfg: DriverConfig,
    session: String,
    rows: VecDeque<Row>,
    claimed: VecDeque<u64>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    blocked: Option<String>,
}

impl State {
    fn emit(&self, event: CtlEvent) {
        (self.sink)(Event::Ctl(event));
    }

    fn publish(&self) {
        self.emit(CtlEvent::Queue {
            session_id: self.session.clone(),
            items: self
                .rows
                .iter()
                .filter(|row| row.visible)
                .map(|row| QueueRow {
                    item_id: row.item_id,
                    text: row.record.text.clone(),
                    parts: row.record.parts.clone(),
                    placement: if row.steering {
                        QueuePlacement::Steering
                    } else {
                        QueuePlacement::Queued
                    },
                })
                .collect(),
        });
    }

    fn save(&self) -> Result<(), String> {
        if let Some(error) = &self.blocked {
            return Err(error.clone());
        }
        let Some(home) = self.cfg.home.as_deref().filter(|home| !home.is_empty()) else {
            return Ok(());
        };
        let rows: Vec<_> = self.rows.iter().map(|row| row.record.clone()).collect();
        crate::queue_store::save(home, &self.cfg.workspace, &self.session, &rows)
            .map_err(|error| format!("queue save failed: {error}"))
    }

    fn apply(&mut self, cmd: Cmd) -> Option<Cmd> {
        let (session, item_id, action) = match cmd {
            Cmd::QueueForSession {
                session_id,
                item_id,
                text,
            } => (session_id, item_id, Mutation::Add(text, None)),
            Cmd::QueuePartsForSession {
                session_id,
                item_id,
                text,
                parts,
            } => (session_id, item_id, Mutation::Add(text, Some(parts))),
            Cmd::UpdateQueue {
                session_id,
                item_id,
                action,
            } => (session_id, item_id, Mutation::Edit(action)),
            other => return Some(other),
        };
        if session != self.session {
            self.emit(CtlEvent::QueueRemoved { item_id });
            return None;
        }
        if self.claimed.contains(&item_id) {
            self.emit(CtlEvent::QueueClaimed { item_id });
            return None;
        }
        let index = self.rows.iter().position(|row| row.item_id == item_id);
        if index.is_some_and(|index| self.rows[index].in_flight) {
            // An input handed to a turn cannot be edited; it remains visible
            // until the durable admission receipt arrives.
            self.publish();
            return None;
        }
        let before = self.rows.clone();
        match action {
            Mutation::Add(text, parts) => {
                if parts.as_ref().map_or(text.trim().is_empty(), Vec::is_empty) {
                    self.emit(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                // Retried client commands are idempotent. Edits have their own
                // command and may not silently replace an accepted input.
                if index.is_some() {
                    self.publish();
                    return None;
                }
                self.rows.push_back(Row {
                    item_id,
                    record: crate::queue_store::QueuedRecord {
                        parts,
                        ..crate::queue_store::QueuedRecord::text(text)
                    },
                    held: false,
                    in_flight: false,
                    steering: false,
                    visible: true,
                });
            }
            Mutation::Edit(action) => {
                let Some(index) = index else {
                    self.emit(CtlEvent::QueueRemoved { item_id });
                    return None;
                };
                match action {
                    QueueAction::Remove => {
                        self.rows.remove(index);
                    }
                    QueueAction::Edit(text) if text.trim().is_empty() => {
                        self.rows.remove(index);
                    }
                    QueueAction::Edit(text) => {
                        self.rows[index].record.text = text.clone();
                        if let Some(parts) = &mut self.rows[index].record.parts {
                            parts.retain(|part| {
                                matches!(part, abycore::ContentPart::InputImage { .. })
                            });
                            parts.insert(0, abycore::ContentPart::InputText { text });
                        }
                    }
                    QueueAction::EditParts { parts, .. } if parts.is_empty() => {
                        self.rows.remove(index);
                    }
                    QueueAction::EditParts { text, parts } => {
                        self.rows[index].record.text = text;
                        self.rows[index].record.parts = Some(parts);
                    }
                }
            }
        }
        if let Err(error) = self.save() {
            self.rows = before;
            self.emit(CtlEvent::TuiOpFailed(error));
            if index.is_none() {
                self.emit(CtlEvent::QueueRemoved { item_id });
            }
        } else if !self.rows.iter().any(|row| row.item_id == item_id) {
            self.emit(CtlEvent::QueueRemoved { item_id });
        }
        self.publish();
        None
    }
}

enum Mutation {
    Add(String, Option<Vec<abycore::ContentPart>>),
    Edit(QueueAction),
}

impl HostInbox {
    pub(super) fn new(
        cfg: &DriverConfig,
        sink: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self, String> {
        let mut cfg = cfg.clone();
        cfg.api_key = None;
        let owner = BlockingOwner::new(
            "aby-inbox",
            State {
                session: cfg.session_id.clone(),
                cfg,
                sink,
                rows: VecDeque::new(),
                claimed: VecDeque::new(),
                blocked: Some("session writer unavailable".into()),
            },
        )
        .map_err(|error| format!("start inbox worker: {error}"))?;
        Ok(Self { owner })
    }

    pub(super) async fn bind(
        &self,
        session: &str,
        snapshot: Option<abycore::SessionSnapshot>,
    ) -> Result<(), String> {
        let session = session.to_string();
        self.owner
            .run(move |state| {
                state.session = session;
                state.rows.clear();
                state.claimed.clear();
                state.blocked = snapshot
                    .is_none()
                    .then(|| "session writer unavailable; input was not accepted".into());
                // Migration and reconciliation write to disk. Only the exclusive
                // session owner may perform either operation.
                if snapshot.is_none() {
                    state.publish();
                    return Ok(());
                }
                let records = match crate::queue_store::load_checked(&state.cfg, &state.session) {
                    Ok(records) => records,
                    Err(error) => {
                        state.blocked = Some(error.clone());
                        state.publish();
                        return Err(error);
                    }
                };
                let mut reconciled = false;
                for record in records {
                    if snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.contains_input(&record.input_id))
                    {
                        reconciled = true;
                        continue;
                    }
                    state.rows.push_back(Row {
                        item_id: next_queue_item_id(),
                        record,
                        held: true,
                        in_flight: false,
                        steering: false,
                        visible: true,
                    });
                }
                if reconciled && let Err(error) = state.save() {
                    state.emit(CtlEvent::TuiOpFailed(error));
                }
                state.publish();
                Ok(())
            })
            .await?
    }

    pub(super) async fn apply(&self, cmd: Cmd) -> Result<Option<Cmd>, String> {
        self.owner.run(move |state| state.apply(cmd)).await
    }

    pub(super) async fn next(&self) -> Result<Option<Cmd>, String> {
        self.owner
            .run(|state| {
                if state.blocked.is_some() {
                    return None;
                }
                let row = state
                    .rows
                    .iter_mut()
                    .find(|row| !row.held && !row.in_flight)?;
                row.in_flight = true;
                Some(Cmd::DeliverQueued {
                    session_id: state.session.clone(),
                    item_id: row.item_id,
                    text: row.record.text.clone(),
                    parts: row.record.parts.clone(),
                })
            })
            .await
    }

    /// Persist before entering the SDK's memory-only inbox. Reusing a queued
    /// row preserves its identity and its last acknowledged edited content.
    pub(super) async fn prepare(
        &self,
        session: &str,
        item_id: Option<u64>,
        text: String,
        parts: Option<Vec<abycore::ContentPart>>,
        steering: bool,
    ) -> Result<Option<abycore::UserInput>, String> {
        let session = session.to_string();
        self.owner
            .run(move |state| {
                if state.session != session {
                    return Err("the active session changed".into());
                }
                if let Some(error) = &state.blocked {
                    return Err(error.clone());
                }
                if item_id.is_some_and(|id| state.claimed.contains(&id)) {
                    return Ok(None);
                }
                let before = state.rows.clone();
                let existing =
                    item_id.and_then(|id| state.rows.iter().position(|row| row.item_id == id));
                let index = match existing {
                    Some(index) => index,
                    None => {
                        if parts.as_ref().map_or(text.trim().is_empty(), Vec::is_empty) {
                            return Err("message must contain text or an image".into());
                        }
                        state.rows.push_back(Row {
                            item_id: item_id.unwrap_or_else(next_queue_item_id),
                            record: crate::queue_store::QueuedRecord {
                                parts,
                                ..crate::queue_store::QueuedRecord::text(text)
                            },
                            held: false,
                            in_flight: false,
                            steering: false,
                            visible: item_id.is_some(),
                        });
                        state.rows.len() - 1
                    }
                };
                let row = &mut state.rows[index];
                row.in_flight = true;
                row.steering = steering;
                let input = abycore::UserInput {
                    id: row.record.input_id.clone(),
                    parts: row.record.parts.clone().unwrap_or_else(|| {
                        vec![abycore::ContentPart::InputText {
                            text: row.record.text.clone(),
                        }]
                    }),
                };
                if let Err(error) = state.save() {
                    state.rows = before;
                    return Err(error);
                }
                if steering {
                    state.publish();
                }
                Ok(Some(input))
            })
            .await?
    }

    pub(super) fn receipts(snapshot: &abycore::SessionSnapshot) -> Vec<String> {
        snapshot
            .items
            .iter()
            .filter_map(|item| match item {
                abycore::Item::Message {
                    role: abycore::MessageRole::User,
                    id: Some(id),
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Only called after the session checkpoint has committed. A stale disk
    /// copy is recoverable because each id is now a durable admission receipt.
    pub(super) async fn committed(
        &self,
        session: &str,
        receipts: Vec<String>,
    ) -> Result<(), String> {
        let session = session.to_string();
        self.owner
            .run(move |state| {
                if state.session != session {
                    return;
                }
                let ids: HashSet<_> = receipts.iter().map(String::as_str).collect();
                let admitted: Vec<_> = state
                    .rows
                    .iter()
                    .filter(|row| ids.contains(row.record.input_id.as_str()))
                    .cloned()
                    .collect();
                if admitted.is_empty() {
                    return;
                }
                state
                    .rows
                    .retain(|row| !ids.contains(row.record.input_id.as_str()));
                if let Err(error) = state.save() {
                    state.emit(CtlEvent::TuiOpFailed(error));
                }
                let mut steers = vec![];
                for row in admitted {
                    if state.claimed.len() >= 4096 {
                        state.claimed.pop_front();
                    }
                    state.claimed.push_back(row.item_id);
                    if row.steering {
                        steers.push(row.item_id);
                    }
                    if row.visible {
                        state.emit(CtlEvent::QueueClaimed {
                            item_id: row.item_id,
                        });
                    }
                }
                if !steers.is_empty() {
                    state.emit(CtlEvent::SteerAdmitted {
                        message_ids: steers,
                    });
                }
                state.publish();
            })
            .await
    }

    pub(super) async fn defer(&self, item_id: u64) -> Result<(), String> {
        self.owner
            .run(move |state| {
                if let Some(row) = state.rows.iter_mut().find(|row| row.item_id == item_id) {
                    row.in_flight = false;
                    row.held = true;
                    row.steering = false;
                    row.visible = true;
                    state.publish();
                }
            })
            .await
    }

    pub(super) async fn finish_turn(
        &self,
        steer: abycore::SteerHandle,
        admitted: Vec<String>,
    ) -> Result<(), String> {
        self.owner
            .run(move |state| {
                let mut deferred = Vec::new();
                for row in &mut state.rows {
                    row.held = row.in_flight;
                    // A cancelled or late steer must not remain in the SDK's
                    // memory inbox after its durable row becomes editable again.
                    steer.withdraw_input(&row.record.input_id);
                    let in_memory = admitted.contains(&row.record.input_id);
                    if row.steering && !in_memory {
                        deferred.push(row.item_id);
                    }
                    row.in_flight = in_memory;
                    row.steering = false;
                    row.visible = true;
                }
                for message_id in deferred {
                    state.emit(CtlEvent::SteerSettled {
                        message_id,
                        deferred: true,
                    });
                }
                state.publish();
            })
            .await
    }

    pub(super) async fn barrier(&self) -> Result<(), String> {
        self.owner.run(|_| ()).await
    }
}
