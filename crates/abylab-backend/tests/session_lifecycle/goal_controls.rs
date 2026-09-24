use super::*;

/// Hold the provider response until the test has observed the control result.
/// Releasing on unwind keeps fixture failures from hanging the server thread.
struct HeldReply {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

impl HeldReply {
    fn new() -> (Reply, Self) {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let reply = Reply::dynamic(move |_| {
            let _ = entered_tx.send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(20));
            Reply::sse(text_body("held", "late answer"))
        });
        (reply, Self { entered, release })
    }
}

impl Drop for HeldReply {
    fn drop(&mut self) {
        let _ = self.release.send(());
    }
}

fn goal(live: &Live, arg: &str) {
    live.send(Cmd::Goal { arg: arg.into() });
}

fn exhausted(live: &Live) {
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::TuiOpFailed(message)) if message.starts_with("goal rounds exhausted")));
    live.done("goal settled");
}

fn soon(live: &Live, predicate: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let event = live
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("goal control must finish while the provider response is held");
        if predicate(&event) {
            return event;
        }
        assert!(
            !matches!(
                event,
                Event::Ctl(CtlEvent::Error(_) | CtlEvent::TuiOpFailed(_))
            ),
            "{event:?}"
        );
    }
}

#[test]
fn status_and_pause_or_esc_work_before_the_provider_finishes() {
    for pause_command in [true, false] {
        let (reply, held) = HeldReply::new();
        let server = MockServer::start(vec![
            reply,
            Reply::sse(text_body("chat", "separate answer")),
        ]);
        // This binding drops the gate before the server if an assertion fails.
        let held = held;
        let live = Live::start(&server);
        goal(&live, "@10 finish the goal");
        held.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        goal(&live, "status");
        let status = soon(
            &live,
            |event| matches!(event, Event::Ctl(CtlEvent::TuiOpDone(message)) if message.starts_with("goal ·")),
        );
        assert!(
            matches!(status, Event::Ctl(CtlEvent::TuiOpDone(message)) if message.contains("active") && message.contains("1/10"))
        );
        let before = live.snapshot().goal.unwrap();
        if pause_command {
            goal(&live, "pause");
        } else {
            live.handle.as_ref().unwrap().interrupt();
        }
        soon(
            &live,
            |event| matches!(event, Event::Ctl(CtlEvent::TuiOpDone(message)) if message.starts_with("goal →") && message.contains("paused")),
        );
        let paused = live.snapshot().goal.unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
        assert_eq!(paused.id, before.id);
        assert_eq!(paused.rounds_started, 1);
        assert!(paused.revision > before.revision);
        soon(&live, |event| {
            matches!(
                event,
                Event::Ui(UiEvent::SessionStatus { running: false, .. })
            )
        });
        drop(held);

        live.prompt("answer a separate question");
        goal(&live, "status");
        live.done("goal ·");
        assert_eq!(live.snapshot().goal, Some(paused));
        assert_eq!(
            server.bodies().len(),
            2,
            "ordinary chat must not start more goal rounds"
        );
    }
}

#[test]
fn exhausted_goal_accepts_a_larger_total_without_losing_its_identity() {
    let server = MockServer::start(
        (1..=5)
            .map(|round| Reply::sse(text_body(&format!("r{round}"), "progress")))
            .collect(),
    );
    let live = Live::start(&server);
    goal(&live, "@1 finish the goal");
    exhausted(&live);
    let first = live.snapshot().goal.unwrap();
    goal(&live, "resume");
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::TuiOpFailed(message)) if message.contains("rounds exhausted")));
    assert_eq!(
        live.snapshot().goal,
        Some(first.clone()),
        "failed resume preserves state"
    );
    goal(&live, "@3 resume");
    exhausted(&live);
    let third = live.snapshot().goal.unwrap();
    assert_eq!(third.id, first.id);
    assert_eq!((third.rounds_started, third.max_rounds), (3, Some(3)));
    assert_eq!(server.bodies().len(), 3);

    goal(&live, "rounds 5");
    live.done("goal →");
    let extended = live.snapshot().goal.unwrap();
    assert_eq!(
        extended.status,
        GoalStatus::Blocked,
        "editing the limit does not resume"
    );
    assert_eq!((extended.rounds_started, extended.max_rounds), (3, Some(5)));
    for invalid in ["rounds 2", "@0 resume", "@4 pause"] {
        goal(&live, invalid);
        live.wait(|event| matches!(event, Event::Ctl(CtlEvent::TuiOpFailed(_))));
        assert_eq!(live.snapshot().goal, Some(extended.clone()));
    }
    goal(&live, "resume");
    exhausted(&live);
    let fifth = live.snapshot().goal.unwrap();
    assert_eq!(fifth.id, first.id);
    assert_eq!((fifth.rounds_started, fifth.max_rounds), (5, Some(5)));
    assert_eq!(server.bodies().len(), 5);
    goal(&live, "rounds off");
    live.done("goal →");
    let unlimited = live.snapshot().goal.unwrap();
    assert_eq!(unlimited.max_rounds, None);
    assert_eq!(unlimited.rounds_started, 5);
    assert_eq!(unlimited.id, first.id);
    assert_eq!(unlimited.status, GoalStatus::Blocked);
    goal(&live, "complete");
    live.done("goal →");
    let complete = live.snapshot().goal;
    goal(&live, "pause");
    live.done("goal →");
    assert_eq!(
        live.snapshot().goal,
        complete,
        "a late pause cannot reopen a completed goal"
    );
}

#[test]
fn restored_active_goal_is_saved_paused_before_binding() {
    let server = MockServer::start(vec![Reply::sse(text_body("resume", "progress"))]);
    let live = Live::start_with_setup(&server, |workspace| {
        let mut snapshot = SessionSnapshot::new("test", abycore::ModelOptions::default());
        snapshot.goal = Some(abycore::Goal {
            id: "original-goal".into(),
            revision: 4,
            objective: "saved objective".into(),
            status: GoalStatus::Active,
            rounds_started: 1,
            max_rounds: Some(2),
            note: None,
        });
        let store = SessionStore::new(workspace).unwrap();
        let mut writer = store.create_new("saved", &snapshot).unwrap();
        store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    });
    live.send(Cmd::Resume {
        session_id: "saved".into(),
    });
    live.wait(|event| matches!(event, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "saved"));
    let store = SessionStore::new(&live.workspace).unwrap();
    let paused = store.load("saved").unwrap().1.goal.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.revision, 5);
    assert_eq!(paused.rounds_started, 1);
    goal(&live, "status");
    live.done("goal ·");
    assert!(
        server.bodies().is_empty(),
        "loading does not execute the goal"
    );
    goal(&live, "resume");
    exhausted(&live);
    let finished = store.load("saved").unwrap().1.goal.unwrap();
    assert_eq!(finished.id, paused.id);
    assert_eq!(finished.rounds_started, 2);
    assert_eq!(server.bodies().len(), 1);
}

#[test]
fn model_created_goal_waits_paused_until_explicit_resume() {
    let server = MockServer::start(vec![
        Reply::sse(single_call_reply(
            "create",
            "create_goal",
            r#"{"objective":"new objective","max_rounds":1}"#,
        )),
        Reply::sse(text_body("created", "goal saved")),
        Reply::sse(text_body("round", "progress")),
    ]);
    let live = Live::start(&server);
    live.prompt("create a goal for later");
    let paused = live.snapshot().goal.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.rounds_started, 0);
    goal(&live, "status");
    live.done("goal ·");
    assert_eq!(server.bodies().len(), 2);
    goal(&live, "resume");
    exhausted(&live);
    assert_eq!(live.snapshot().goal.unwrap().id, paused.id);
    assert_eq!(server.bodies().len(), 3);
}
