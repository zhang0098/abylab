mod common;

use abycore::{Item, ModelOptions, SessionSnapshot, SessionStore};
use abylab_backend::{Cmd, CtlEvent, DriverConfig, Event, TurnLimits, UiEvent};
use common::{MockServer, Reply, text_body};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aby-recovery-{}-{now}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> SessionStore {
        SessionStore::new(&self.0).unwrap()
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Live {
    handle: Option<abylab_backend::driver::DriverHandle>,
    rx: mpsc::Receiver<Event>,
}
impl Live {
    fn start(server: &MockServer, workspace: &Workspace, resume: bool, keyless: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = abylab_backend::driver::spawn(
            DriverConfig {
                session_id: "s".into(),
                resume: resume.then(|| "s".into()),
                sessions_root: None,
                home: Some(workspace.0.join("home").to_string_lossy().into_owned()),
                workspace: workspace.0.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: (!keyless).then(|| "test-key".into()),
                base_url: Some(server.base_url.clone()),
                limits: TurnLimits {
                    continuations: 0,
                    ..Default::default()
                },
                compaction: None,
            },
            move |event| {
                let _ = tx.send(event);
            },
        )
        .unwrap();
        Self {
            handle: Some(handle),
            rx,
        }
    }
    fn send(&self, cmd: Cmd) {
        self.handle.as_ref().unwrap().send(cmd);
    }
    fn wait(&self, pred: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if pred(&event) {
                return event;
            }
            assert!(
                !matches!(
                    event,
                    Event::Ctl(
                        CtlEvent::Error(_)
                            | CtlEvent::TuiOpFailed(_)
                            | CtlEvent::SessionSwitchFailed(_)
                    )
                ),
                "{event:?}"
            );
        }
    }
    fn bound(&self, id: &str) {
        self.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == id));
    }
    fn prompt(&self, id: &str, text: &str) {
        self.send(Cmd::PromptForSession {
            session_id: id.into(),
            text: text.into(),
        });
        self.wait(|e| matches!(e, Event::Ui(UiEvent::TurnEnd { session, kind }) if session == id && kind == "completed"));
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
            drop(handle);
        }
    }
}

fn seed() -> SessionSnapshot {
    let mut snapshot = SessionSnapshot::new("system", ModelOptions::default());
    snapshot.items.push(Item::user("ORIGINAL_HISTORY_MARKER"));
    snapshot.needs_response = true;
    snapshot
}
fn contains(snapshot: &SessionSnapshot, marker: &str) -> bool {
    serde_json::to_string(&snapshot.items)
        .unwrap()
        .contains(marker)
}

#[test]
fn keyless_resume_then_login_reads_the_latest_history_and_keeps_the_title() {
    let workspace = Workspace::new();
    let store = workspace.store();
    let mut snapshot = seed();
    let mut writer = store.create_new("s", &snapshot).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    store.set_title(&mut writer, "my custom title").unwrap();
    let server = MockServer::start(vec![Reply::sse(text_body("answer", "done"))]);
    let live = Live::start(&server, &workspace, true, true);
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::Error(s)) if s.contains("no API key")));
    snapshot
        .items
        .push(Item::user("COMMITTED_WHILE_WAITING_FOR_LOGIN"));
    store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
    drop(writer);
    live.send(Cmd::SetApiKey {
        key: Some("test-key".into()),
    });
    live.bound("s");
    assert!(
        store.create("s", &snapshot).is_err(),
        "restore already owns the lock"
    );
    live.prompt("s", "new prompt");
    let restored = store.load("s").unwrap().1;
    assert!(contains(&restored, "ORIGINAL_HISTORY_MARKER"));
    assert!(contains(&restored, "COMMITTED_WHILE_WAITING_FOR_LOGIN"));
    assert_eq!(store.title_of("s").as_deref(), Some("my custom title"));
    assert!(server.bodies()[0].contains("COMMITTED_WHILE_WAITING_FOR_LOGIN"));
}

#[test]
fn locked_startup_never_binds_or_overwrites_and_retry_reads_the_latest_snapshot() {
    let workspace = Workspace::new();
    let store = workspace.store();
    let mut snapshot = seed();
    let mut writer = store.create_new("s", &snapshot).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let server = MockServer::start(vec![Reply::sse(text_body("answer", "done"))]);
    let live = Live::start(&server, &workspace, true, false);
    live.wait(|e| {
        assert!(
            !matches!(e, Event::Ctl(CtlEvent::SessionBound { .. })),
            "must not bind without a lock"
        );
        matches!(e, Event::Ctl(CtlEvent::Error(s)) if s.contains("lock held"))
    });
    assert!(server.bodies().is_empty());
    snapshot
        .items
        .push(Item::user("CONCURRENT_COMMITTED_MARKER"));
    store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
    drop(writer);
    live.send(Cmd::Resume {
        session_id: "s".into(),
    });
    live.bound("s");
    assert!(store.open_for_resume("s").is_err());
    // Re-selecting the same session must retain its lock and identity.
    live.send(Cmd::Resume {
        session_id: "s".into(),
    });
    live.bound("s");
    live.prompt("s", "new prompt");
    assert!(contains(
        &store.load("s").unwrap().1,
        "CONCURRENT_COMMITTED_MARKER"
    ));
    assert!(server.bodies()[0].contains("CONCURRENT_COMMITTED_MARKER"));
}

#[test]
fn failed_switches_preserve_the_current_session_and_stale_prompts_are_rejected() {
    let workspace = Workspace::new();
    let store = workspace.store();
    let snapshot = seed();
    let mut writer = store.create_new("target", &snapshot).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let server = MockServer::start(vec![
        Reply::sse(text_body("first", "first answer")),
        Reply::sse(text_body("second", "second answer")),
        Reply::sse(text_body("third", "third answer")),
    ]);
    let live = Live::start(&server, &workspace, false, false);
    live.bound("s");
    live.prompt("s", "CURRENT_SESSION_MARKER");
    for target in ["target", "missing"] {
        live.send(Cmd::Resume {
            session_id: target.into(),
        });
        live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionSwitchFailed(_))));
        assert!(
            store.open_for_resume("s").is_err(),
            "old writer must stay owned"
        );
    }
    drop(writer);
    live.send(Cmd::NewSession {
        session_id: "target".into(),
    });
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionSwitchFailed(s)) if s.contains("already exists")));
    live.prompt("s", "continue original");
    assert!(server.bodies()[1].contains("CURRENT_SESSION_MARKER"));
    assert_eq!(store.load("target").unwrap().1, snapshot);

    live.send(Cmd::NewSession {
        session_id: "new".into(),
    });
    live.bound("new");
    assert!(
        store.open_for_resume("s").is_ok(),
        "switch releases the old lock"
    );
    live.send(Cmd::PromptForSession {
        session_id: "s".into(),
        text: "STALE_PROMPT".into(),
    });
    live.wait(
        |e| matches!(e, Event::Ctl(CtlEvent::Error(s)) if s.contains("active session changed")),
    );
    assert_eq!(server.bodies().len(), 2);
    live.prompt("new", "new task");
    assert!(!server.bodies()[2].contains("STALE_PROMPT"));
    assert!(!server.bodies()[2].contains("CURRENT_SESSION_MARKER"));
}

#[test]
fn failed_keyless_restore_never_falls_back_to_creating_the_same_id() {
    let workspace = Workspace::new();
    let server = MockServer::start(vec![]);
    let live = Live::start(&server, &workspace, true, true);
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::Error(s)) if s.contains("no API key")));
    live.send(Cmd::SetApiKey {
        key: Some("test-key".into()),
    });
    live.wait(|e| {
        assert!(!matches!(e, Event::Ctl(CtlEvent::SessionBound { .. })));
        matches!(e, Event::Ctl(CtlEvent::TuiOpFailed(s)) if s.contains("session open failed"))
    });
    assert!(!workspace.store().log_path("s").unwrap().exists());
    assert!(server.bodies().is_empty());
}
