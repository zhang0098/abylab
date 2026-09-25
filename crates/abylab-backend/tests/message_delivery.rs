//! Delivery must remain session-scoped and durable while a turn waits for input.
mod common;

use abylab_backend::{
    Cmd, CtlEvent, DriverConfig, DriverHandle, Event, PromptPart, QueueAction, SteerRequest,
    TurnLimits, UiEvent, UserQuestionReply,
};
use common::{MockServer, Reply, single_call_reply, text_body};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Live {
    handle: DriverHandle,
    rx: mpsc::Receiver<Event>,
    cfg: DriverConfig,
}

impl Live {
    fn start(cfg: DriverConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = abylab_backend::driver::spawn(cfg.clone(), move |event| {
            let _ = tx.send(event);
        })
        .unwrap();
        let live = Self { handle, rx, cfg };
        live.wait(|event| matches!(event, Event::Ctl(CtlEvent::SessionBound { .. })));
        live
    }

    fn wait(&self, predicate: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let event = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("driver event deadline");
            if predicate(&event) {
                return event;
            }
            assert!(
                !matches!(
                    &event,
                    Event::Ctl(CtlEvent::Error(_) | CtlEvent::TuiOpFailed(_))
                ),
                "unexpected failure: {event:?}"
            );
        }
    }

    fn queue(&self, count: usize) {
        self.wait(|event| matches!(event, Event::Ctl(CtlEvent::Queue { items, .. }) if items.len() == count));
    }

    fn stored_queue(&self) -> serde_json::Value {
        let hash = format!("{:x}", Sha256::digest(self.cfg.workspace.as_bytes()));
        let path = Path::new(self.cfg.home.as_ref().unwrap())
            .join("queued")
            .join(hash)
            .join("active.json");
        serde_json::from_slice(&std::fs::read(path).expect("queue persisted before its event"))
            .unwrap()
    }

    fn ask(&self) -> Event {
        self.handle.send(Cmd::Prompt { text: "Call ask_user_question with one free-text question: id 'q', question 'Continue?'. Wait for my answer, then reply with exactly 'finished'. Do not use any other tools.".into() });
        self.wait(|event| matches!(event, Event::UserQuestion { .. }))
    }
}

fn config(root: &Path, base_url: Option<String>, key: String) -> DriverConfig {
    DriverConfig {
        session_id: "active".into(),
        resume: None,
        sessions_root: None,
        home: Some(root.join("home").to_string_lossy().into_owned()),
        workspace: root.to_string_lossy().into_owned(),
        model: "deepseek-flash".into(),
        reasoning: "off".into(),
        permission: None,
        max_tokens: Some(2048),
        api_key: Some(key),
        base_url,
        limits: TurnLimits {
            max_requests: 4,
            max_tool_calls: 2,
            continuations: 0,
            ..Default::default()
        },
        compaction: None,
    }
}

fn server() -> MockServer {
    MockServer::start(vec![
        Reply::sse(single_call_reply(
            "question",
            "ask_user_question",
            r#"{"questions":[{"id":"q","question":"Continue?"}]}"#,
        )),
        Reply::sse(text_body("done", "finished")),
    ])
}

#[test]
fn queued_text_images_edits_and_removals_persist_before_a_turn_finishes() {
    let root = tempfile::tempdir().unwrap();
    let server = server();
    let live = Live::start(config(
        root.path(),
        Some(server.base_url.clone()),
        "fixture-key".into(),
    ));
    let question = live.ask();
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 1,
        text: "important follow-up".into(),
    });
    live.queue(1);
    assert_eq!(
        live.stored_queue()["items"][0]["text"],
        "important follow-up"
    );
    live.handle.send(Cmd::UpdateQueue {
        session_id: "active".into(),
        item_id: 1,
        action: QueueAction::Edit("edited follow-up".into()),
    });
    live.queue(1);
    assert_eq!(live.stored_queue()["items"][0]["text"], "edited follow-up");
    let image = PromptPart::InputImage {
        media_type: "image/png".into(),
        data: "AA==".into(),
    };
    live.handle.send(Cmd::QueuePartsForSession {
        session_id: "active".into(),
        item_id: 2,
        text: String::new(),
        parts: vec![image.clone()],
    });
    live.queue(2);
    assert_eq!(
        live.stored_queue()["items"][1]["parts"][0],
        serde_json::to_value(image).unwrap()
    );
    live.handle.send(Cmd::UpdateQueue {
        session_id: "active".into(),
        item_id: 2,
        action: QueueAction::EditParts {
            text: "caption".into(),
            parts: vec![PromptPart::InputText {
                text: "caption".into(),
            }],
        },
    });
    live.queue(2);
    assert_eq!(live.stored_queue()["items"][1]["text"], "caption");
    live.handle.send(Cmd::UpdateQueue {
        session_id: "active".into(),
        item_id: 2,
        action: QueueAction::Remove,
    });
    live.queue(1);
    assert_eq!(live.stored_queue()["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        server.bodies().len(),
        1,
        "no queued work runs while waiting for the human"
    );
    let mut cfg = live.cfg.clone();
    live.handle.shutdown();
    drop(question);
    drop(live);

    cfg.resume = Some("active".into());
    let restored = Live::start(cfg);
    restored.queue(1);
    assert_eq!(
        restored.stored_queue()["items"][0]["text"],
        "edited follow-up"
    );
    assert_eq!(server.bodies().len(), 1, "restored work stays held");
}

#[test]
fn wrong_session_steers_are_rejected_without_consuming_ids_or_input() {
    let root = tempfile::tempdir().unwrap();
    let server = server();
    let live = Live::start(config(
        root.path(),
        Some(server.base_url.clone()),
        "fixture-key".into(),
    ));
    let question = live.ask();
    for parts in [
        None,
        Some(vec![PromptPart::InputText {
            text: "WRONG_PARTS".into(),
        }]),
    ] {
        live.handle.steer(SteerRequest {
            session_id: "other".into(),
            message_id: 73,
            text: "WRONG_SESSION".into(),
            parts,
        });
        let event = live.wait(|event| {
            matches!(
                event,
                Event::Ctl(CtlEvent::SteerSettled { message_id: 73, .. })
            )
        });
        assert!(matches!(
            event,
            Event::Ctl(CtlEvent::SteerSettled { deferred: true, .. })
        ));
    }
    live.handle.steer(SteerRequest {
        session_id: "active".into(),
        message_id: 73,
        text: "VALID_STEER".into(),
        parts: None,
    });
    let event = live.wait(|event| {
        matches!(
            event,
            Event::Ctl(CtlEvent::SteerSettled { message_id: 73, .. })
        )
    });
    assert!(matches!(
        event,
        Event::Ctl(CtlEvent::SteerSettled {
            deferred: false,
            ..
        })
    ));
    answer(question);
    live.wait(|event| matches!(event, Event::Ui(UiEvent::TurnEnd { .. })));
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].contains("VALID_STEER"));
    assert!(!bodies[1].contains("WRONG_SESSION") && !bodies[1].contains("WRONG_PARTS"));
}

fn answer(event: Event) {
    let Event::UserQuestion { reply, .. } = event else {
        panic!("expected a question")
    };
    reply
        .send(Some(UserQuestionReply {
            selected: vec![],
            custom: Some("yes".into()),
        }))
        .unwrap();
}

#[test]
fn a_steered_row_cannot_be_republished_edited_or_queued_again() {
    let root = tempfile::tempdir().unwrap();
    let server = server();
    let live = Live::start(config(
        root.path(),
        Some(server.base_url.clone()),
        "fixture-key".into(),
    ));
    let question = live.ask();
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 1,
        text: "STEER_THIS".into(),
    });
    live.queue(1);
    live.handle.steer(SteerRequest {
        session_id: "active".into(),
        message_id: 1,
        text: "STEER_THIS".into(),
        parts: None,
    });
    live.wait(|event| {
        matches!(
            event,
            Event::Ctl(CtlEvent::SteerSettled {
                message_id: 1,
                deferred: false
            })
        )
    });
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::QueueClaimed { item_id: 1 })));
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 2,
        text: "still waiting".into(),
    });
    let updated = live.wait(|event| matches!(event, Event::Ctl(CtlEvent::Queue { items, .. }) if items.iter().any(|item| item.item_id == 2)));
    assert!(matches!(updated, Event::Ctl(CtlEvent::Queue { items, .. }) if items.len() == 1));
    assert_eq!(live.stored_queue()["items"].as_array().unwrap().len(), 1);
    assert_eq!(live.stored_queue()["items"][0]["text"], "still waiting");
    live.handle.send(Cmd::UpdateQueue {
        session_id: "active".into(),
        item_id: 1,
        action: QueueAction::Edit("TOO_LATE".into()),
    });
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::QueueClaimed { item_id: 1 })));
    live.handle.send(Cmd::UpdateQueue {
        session_id: "active".into(),
        item_id: 2,
        action: QueueAction::Remove,
    });
    live.queue(0);
    answer(question);
    live.wait(|event| {
        matches!(
            event,
            Event::Ui(UiEvent::SessionStatus { running: false, .. })
        )
    });
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 1,
        text: "STEER_THIS".into(),
    });
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::QueueClaimed { item_id: 1 })));
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].contains("STEER_THIS"));
    assert!(!bodies[1].contains("TOO_LATE") && !bodies[1].contains("still waiting"));
}

#[test]
fn a_pending_session_switch_keeps_its_followup_queue_commands_in_order() {
    let root = tempfile::tempdir().unwrap();
    let server = MockServer::start(vec![
        Reply::sse(single_call_reply(
            "question",
            "ask_user_question",
            r#"{"questions":[{"id":"q","question":"Continue?"}]}"#,
        )),
        Reply::sse(text_body("done", "finished")),
        Reply::sse(text_body("next", "next session")),
    ]);
    let live = Live::start(config(
        root.path(),
        Some(server.base_url.clone()),
        "fixture-key".into(),
    ));
    let question = live.ask();
    live.handle.send(Cmd::NewSession {
        session_id: "next".into(),
    });
    live.handle.send(Cmd::QueueForSession {
        session_id: "next".into(),
        item_id: 7,
        text: "NEXT_SESSION".into(),
    });
    live.handle.send(Cmd::UpdateQueue {
        session_id: "next".into(),
        item_id: 7,
        action: QueueAction::Edit("NEXT_EDITED".into()),
    });
    // A current-session mutation is a barrier: it proves all preceding commands
    // were read while the question still holds the turn open.
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 8,
        text: "stay here".into(),
    });
    live.queue(1);
    assert_eq!(live.stored_queue()["items"][0]["text"], "stay here");
    answer(question);
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "next"));
    live.wait(
        |event| matches!(event, Event::Ui(UiEvent::TurnEnd { session, .. }) if session == "next"),
    );
    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[2].contains("NEXT_EDITED"));
    assert!(!bodies[2].contains("NEXT_SESSION") && !bodies[2].contains("stay here"));
    assert!(!bodies[1].contains("NEXT_EDITED"));
    assert_eq!(live.stored_queue()["items"][0]["text"], "stay here");
}

#[test]
#[ignore = "paid delivery regression; requires DEEPSEEK_API_KEY"]
fn live_queue_and_session_isolation_while_waiting_for_a_human() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(
        root.path(),
        std::env::var("DEEPSEEK_BASE_URL").ok(),
        std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly"),
    );
    cfg.model = std::env::var("DEEPSEEK_MODEL").unwrap_or(cfg.model);
    let live = Live::start(cfg);
    let question = live.ask();
    live.handle.send(Cmd::QueueForSession {
        session_id: "active".into(),
        item_id: 1,
        text: "Reply with exactly QUEUED_OK. Do not call any tools.".into(),
    });
    live.queue(1);
    assert_eq!(live.stored_queue()["items"].as_array().unwrap().len(), 1);
    live.handle.steer(SteerRequest {
        session_id: "other".into(),
        message_id: 99,
        text: "WRONG_SESSION_MARKER".into(),
        parts: None,
    });
    let rejected = live.wait(|event| {
        matches!(
            event,
            Event::Ctl(CtlEvent::SteerSettled { message_id: 99, .. })
        )
    });
    assert!(matches!(
        rejected,
        Event::Ctl(CtlEvent::SteerSettled { deferred: true, .. })
    ));
    answer(question);
    for _ in 0..2 {
        let ended = live.wait(|event| matches!(event, Event::Ui(UiEvent::TurnEnd { .. })));
        assert!(matches!(ended, Event::Ui(UiEvent::TurnEnd { kind, .. }) if kind == "completed"));
    }
    let snapshot = abycore::SessionStore::new(root.path())
        .unwrap()
        .load("active")
        .unwrap()
        .1;
    let text = serde_json::to_string(&snapshot.items).unwrap();
    assert!(text.contains("QUEUED_OK"));
    assert!(!text.contains("WRONG_SESSION_MARKER"));
    assert_eq!(snapshot.run_sequence, 2);
}
