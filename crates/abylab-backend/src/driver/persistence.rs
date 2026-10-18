//! Exclusive session writer and its ordered commit/drain protocol.
use super::{blocking, first_user_text};
use abycore::ErrorKind;

/// The exclusive writer is reserved before a session is bound to the UI.
pub(super) struct PersistState {
    owner: blocking::BlockingOwner<PersistWriter>,
}

struct PersistWriter {
    store: abycore::SessionStore,
    writer: Option<abycore::SessionWriter>,
}

impl PersistState {
    pub(super) fn with_writer(
        store: &abycore::SessionStore,
        writer: abycore::SessionWriter,
    ) -> abycore::Result<Self> {
        let owner = blocking::BlockingOwner::new(
            "aby-session",
            PersistWriter {
                store: store.clone(),
                writer: Some(writer),
            },
        )
        .map_err(|error| {
            abycore::Error::new(ErrorKind::Session, format!("start session writer: {error}"))
        })?;
        Ok(Self { owner })
    }

    pub(super) async fn save(&self, snapshot: abycore::SessionSnapshot) -> abycore::Result<()> {
        self.owner
            .run(move |state| state.save(&snapshot))
            .await
            .map_err(storage_error)?
    }

    pub(super) async fn close(&self) -> abycore::Result<()> {
        self.owner
            .run(|state| {
                state.writer.take();
            })
            .await
            .map_err(storage_error)
    }
}

pub(super) fn storage_error(message: String) -> abycore::Error {
    abycore::Error::new(ErrorKind::Session, message)
}

impl PersistWriter {
    fn save(&mut self, snapshot: &abycore::SessionSnapshot) -> abycore::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| storage_error("session writer is closed".into()))?;
        // Preserve stored titles on resume; defer a new title until a prompt exists.
        if writer.title().is_none()
            && let Some(title) = first_user_text(snapshot)
                .map(|text| text.chars().take(48).collect::<String>())
                .filter(|text| !text.is_empty())
        {
            self.store.set_title(writer, &title)?;
        }
        self.store
            .append_checkpoint(writer, snapshot.run_sequence, snapshot)
    }
}

pub(super) async fn delete_session_files(
    store: Option<abycore::SessionStore>,
    home: Option<String>,
    workspace: String,
    id: String,
) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let mut errors = Vec::new();
        if let Some(store) = store
            && let Err(error) = store.delete(&id)
        {
            errors.push(format!("session delete failed: {error}"));
        }
        if let Some(home) = home
            && let Err(error) = crate::queue_store::remove(&home, &workspace, &id)
        {
            errors.push(format!("queue delete failed: {error}"));
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    })
    .await
    .unwrap_or_else(|error| Some(format!("session delete worker failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[tokio::test]
    #[ignore = "local storage benchmark; reports clone, validation and commit latency"]
    async fn checkpoint_costs() {
        for (turns, image_bytes) in [(10, 0), (100, 0), (1000, 0), (10, 4 * 1024 * 1024)] {
            let root = tempfile::tempdir().unwrap();
            let store = abycore::SessionStore::new(root.path()).unwrap();
            let mut snapshot = abycore::SessionSnapshot::new("benchmark", Default::default());
            for n in 0..turns {
                snapshot.items.push(abycore::Item::user("x".repeat(4096)));
                snapshot.items.push(abycore::Item::Message {
                    id: Some(format!("m{n}")),
                    source: None,
                    role: abycore::MessageRole::Assistant,
                    content: vec![abycore::ContentPart::OutputText {
                        text: "y".repeat(4096),
                    }],
                });
            }
            if image_bytes > 0 {
                snapshot.items.insert(
                    0,
                    abycore::Item::user_parts(vec![abycore::ContentPart::InputImage {
                        media_type: "image/png".into(),
                        data: "A".repeat(image_bytes),
                    }]),
                );
            }
            let start = Instant::now();
            let cloned = snapshot.clone();
            let clone_us = start.elapsed().as_micros();
            let start = Instant::now();
            cloned.validate().unwrap();
            let validate_us = start.elapsed().as_micros();
            let writer = store.create_new("bench", &snapshot).unwrap();
            let owner = PersistState::with_writer(&store, writer).unwrap();
            let start = Instant::now();
            owner.save(cloned).await.unwrap();
            let anchor_us = start.elapsed().as_micros();
            let start = Instant::now();
            owner.save(snapshot).await.unwrap();
            let unchanged_us = start.elapsed().as_micros();
            owner.close().await.unwrap();
            let peak = std::fs::read_to_string("/proc/self/status")
                .unwrap_or_default()
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .unwrap_or("VmHWM: unavailable")
                .to_string();
            println!(
                "turns={turns}, image_base64_bytes={image_bytes}, clone_us={clone_us}, validate_us={validate_us}, anchor_commit_us={anchor_us}, unchanged_commit_us={unchanged_us}, {peak}"
            );
        }
    }
    #[tokio::test]
    async fn cancellation_during_commit_never_dispatches_and_close_drains_the_receipt() {
        use abycore::*;
        use std::sync::{Arc, Mutex};
        struct SlowHook {
            persist: Arc<PersistState>,
            gate: Mutex<
                Option<(
                    tokio::sync::oneshot::Sender<()>,
                    std::sync::mpsc::Receiver<()>,
                )>,
            >,
        }
        impl AgentHooks for SlowHook {
            fn checkpoint<'a>(
                &'a self,
                _: CheckpointKind,
                snapshot: SessionSnapshot,
            ) -> futures_util::future::BoxFuture<'a, abycore::Result<()>> {
                let gate = self.gate.lock().unwrap().take();
                Box::pin(async move {
                    self.persist
                        .owner
                        .run(move |state| {
                            if let Some((entered, release)) = gate {
                                let _ = entered.send(());
                                release.recv().unwrap();
                            }
                            state.save(&snapshot)
                        })
                        .await
                        .map_err(storage_error)?
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path()).unwrap();
        let snapshot = SessionSnapshot::new("test", Default::default());
        let writer = store.create_new("s", &snapshot).unwrap();
        let persist = Arc::new(PersistState::with_writer(&store, writer).unwrap());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut config = ClientConfig::new("fixture-key");
        config.base_url = "http://127.0.0.1:9".into();
        let mut agent = Agent::restore(DeepSeekClient::new(config).unwrap(), snapshot).unwrap();
        agent.set_hooks(Arc::new(SlowHook {
            persist: persist.clone(),
            gate: Mutex::new(Some((entered_tx, release_rx))),
        }));
        let cancellation = CancellationToken::new();
        let options = RunOptions {
            cancellation: cancellation.clone(),
            ..Default::default()
        };
        let input = UserInput::text("must survive cancellation");
        let id = input.id.clone();
        let run = tokio::spawn(async move {
            let result = agent.run_input(input, options, |_| async { Ok(()) }).await;
            (agent, result)
        });
        entered_rx.await.unwrap();
        cancellation.cancel();
        let (agent, result) = tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
        assert!(
            agent.snapshot().requests.is_empty(),
            "no dispatch crossed the pending commit"
        );
        release_tx.send(()).unwrap();
        persist.close().await.unwrap();
        let saved = store.load("s").unwrap().1;
        assert!(saved.contains_input(&id));
        assert!(saved.requests.is_empty());
        assert!(
            store.open_for_resume("s").is_ok(),
            "the close receipt releases the exclusive lock"
        );
    }
}
