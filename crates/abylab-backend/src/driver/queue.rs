//! Service durable queue mutations while the agent owns a running turn.
//!
//! Ordinary commands stay ordered for the next idle boundary. Queue commands
//! carry their session identity and can be saved without borrowing the Agent.
use super::*;

pub(super) struct Commands {
    rx: mpsc::UnboundedReceiver<Cmd>,
    deferred: VecDeque<Cmd>,
}

impl Commands {
    pub(super) fn new(rx: mpsc::UnboundedReceiver<Cmd>) -> Self {
        Self {
            rx,
            deferred: VecDeque::new(),
        }
    }

    pub(super) fn try_recv(&mut self) -> Result<Cmd, mpsc::error::TryRecvError> {
        self.deferred
            .pop_front()
            .map_or_else(|| self.rx.try_recv(), Ok)
    }

    pub(super) async fn recv(&mut self) -> Option<Cmd> {
        match self.deferred.pop_front() {
            Some(cmd) => Some(cmd),
            None => self.rx.recv().await,
        }
    }

    pub(super) async fn during<F: std::future::Future>(
        &mut self,
        work: F,
        mut queue: QueueContext<'_>,
    ) -> F::Output {
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                Some(cmd) = self.rx.recv() => {
                    // A caller may already have requested a switch and then
                    // queued work for its target. Preserve that ordering;
                    // accepting it into the current session would be wrong.
                    let target = match &cmd {
                        Cmd::QueueForSession { session_id, .. }
                        | Cmd::QueuePartsForSession { session_id, .. }
                        | Cmd::UpdateQueue { session_id, .. } => Some(session_id),
                        _ => None,
                    };
                    if target.is_some_and(|target| {
                        target != queue.session && self.deferred.iter().any(|pending| {
                            matches!(pending, Cmd::NewSession { session_id } | Cmd::Resume { session_id } if session_id == target)
                        })
                    }) {
                        self.deferred.push_back(cmd);
                        continue;
                    }
                    if let Some(cmd) = queue.apply(cmd) {
                        self.deferred.push_back(cmd);
                    }
                }
                result = &mut work => return result,
            }
        }
    }
}

pub(super) struct QueueContext<'a> {
    pub cfg: &'a DriverConfig,
    pub session: &'a str,
    pub queue: &'a mut VecDeque<QueuedItem>,
    pub taken: &'a OutstandingSteers,
    pub admitted: &'a OutstandingSteers,
    pub sink: &'a Arc<dyn Fn(Event) + Send + Sync>,
}

impl QueueContext<'_> {
    /// Return non-queue commands untouched for the ordinary driver loop.
    pub(super) fn apply(&mut self, cmd: Cmd) -> Option<Cmd> {
        let cfg = self.cfg;
        let active_session = self.session;
        let queue = &mut *self.queue;
        let taken = self.taken;
        let admitted = self.admitted;
        let ctl = |event| (self.sink)(Event::Ctl(event));
        // A steer can take an already-persisted row while work is polled.
        // Remove it before the next mutation publishes another queue snapshot.
        settle_taken(queue, taken, cfg, active_session, &ctl);
        match cmd {
            Cmd::QueueForSession {
                session_id,
                item_id,
                text,
            } => {
                if session_id != active_session || text.trim().is_empty() {
                    // The session moved on, or there is nothing to send: the
                    // client's optimistic row goes away instead of lying.
                    ctl(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                if taken.contains(item_id) || admitted.contains(item_id) {
                    // The running turn already took this id (the client queued
                    // and steered it in the same breath): it is being delivered,
                    // so say that instead of queueing a duplicate.
                    ctl(CtlEvent::QueueClaimed { item_id });
                    return None;
                }
                queue.push_back(QueuedItem {
                    item_id,
                    text,
                    parts: None,
                    placement: QueuePlacement::Queued,
                    held: false,
                });
                if !save_queue_or_report(cfg, active_session, queue, &ctl) {
                    queue.pop_back();
                    ctl(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                publish_queue(queue, active_session, &ctl);
            }
            Cmd::QueuePartsForSession {
                session_id,
                item_id,
                text,
                parts,
            } => {
                if session_id != active_session || parts.is_empty() {
                    ctl(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                if taken.contains(item_id) || admitted.contains(item_id) {
                    ctl(CtlEvent::QueueClaimed { item_id });
                    return None;
                }
                queue.push_back(QueuedItem {
                    item_id,
                    text,
                    parts: Some(parts),
                    placement: QueuePlacement::Queued,
                    held: false,
                });
                if !save_queue_or_report(cfg, active_session, queue, &ctl) {
                    queue.pop_back();
                    ctl(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                publish_queue(queue, active_session, &ctl);
            }
            Cmd::UpdateQueue {
                session_id,
                item_id,
                action,
            } => {
                if session_id != active_session {
                    ctl(CtlEvent::QueueRemoved { item_id });
                    return None;
                }
                if taken.contains(item_id) || admitted.contains(item_id) {
                    ctl(CtlEvent::QueueClaimed { item_id });
                    return None;
                }
                let before_edit = queue.clone();
                match action {
                    QueueAction::Remove => {
                        if let Some(index) = queue.iter().position(|item| item.item_id == item_id) {
                            queue.remove(index);
                        }
                        ctl(CtlEvent::QueueRemoved { item_id });
                    }
                    QueueAction::Edit(text) => {
                        match queue.iter().position(|item| item.item_id == item_id) {
                            Some(index) if text.trim().is_empty() => {
                                queue.remove(index);
                                ctl(CtlEvent::QueueRemoved { item_id });
                            }
                            Some(index) => {
                                queue[index].text = text.clone();
                                if let Some(parts) = &mut queue[index].parts {
                                    parts.retain(|part| {
                                        matches!(part, abycore::ContentPart::InputImage { .. })
                                    });
                                    parts.insert(0, abycore::ContentPart::InputText { text });
                                }
                            }
                            // Already delivered or removed: the client's row is
                            // stale, and saying so is cheaper than resurrecting it.
                            None => ctl(CtlEvent::QueueRemoved { item_id }),
                        }
                    }
                    QueueAction::EditParts { text, parts } => {
                        if let Some(index) = queue.iter().position(|item| item.item_id == item_id) {
                            if parts.is_empty() {
                                queue.remove(index);
                                ctl(CtlEvent::QueueRemoved { item_id });
                            } else {
                                queue[index].text = text;
                                queue[index].parts = Some(parts);
                            }
                        } else {
                            ctl(CtlEvent::QueueRemoved { item_id });
                        }
                    }
                }
                if !save_queue_or_report(cfg, active_session, queue, &ctl) {
                    *queue = before_edit;
                }
                publish_queue(queue, active_session, &ctl);
            }
            cmd => return Some(cmd),
        }
        None
    }
}
