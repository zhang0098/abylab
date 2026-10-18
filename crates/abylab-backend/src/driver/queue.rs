//! Service durable inbox commands while the agent owns a running turn.
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
        queue: QueueContext<'_>,
    ) -> F::Output {
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                Some(cmd) = self.rx.recv() => {
                    let target = match &cmd {
                        Cmd::QueueForSession { session_id, .. }
                        | Cmd::QueuePartsForSession { session_id, .. }
                        | Cmd::UpdateQueue { session_id, .. } => Some(session_id),
                        _ => None,
                    };
                    if target.is_some_and(|target| target != queue.session && self.deferred.iter().any(|pending| {
                        matches!(pending, Cmd::NewSession { session_id } | Cmd::Resume { session_id } if session_id == target)
                    })) {
                        self.deferred.push_back(cmd);
                        continue;
                    }
                    let applied = queue.apply(cmd);
                    tokio::pin!(applied);
                    // Slow queue IO must not stop polling the turn's cancellation
                    // and goal-control channels. Mutations still commit in order.
                    tokio::select! {
                        cmd = &mut applied => if let Some(cmd) = cmd { self.deferred.push_back(cmd); },
                        result = &mut work => {
                            if let Some(cmd) = applied.await { self.deferred.push_back(cmd); }
                            return result;
                        }
                    }
                }
                result = &mut work => return result,
            }
        }
    }
}

pub(super) struct QueueContext<'a> {
    pub session: &'a str,
    pub inbox: &'a HostInbox,
    pub sink: &'a Arc<dyn Fn(Event) + Send + Sync>,
}

impl QueueContext<'_> {
    pub(super) async fn apply(&self, cmd: Cmd) -> Option<Cmd> {
        match self.inbox.apply(cmd).await {
            Ok(cmd) => cmd,
            Err(error) => {
                (self.sink)(Event::Ctl(CtlEvent::TuiOpFailed(error)));
                None
            }
        }
    }
}
