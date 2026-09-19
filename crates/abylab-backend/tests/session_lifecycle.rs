//! Cross-layer regressions: acknowledged host mutations are durable, and live
//! settings changes retain the parent's writer, identity and child controls.
mod common;

use abycore::{GoalStatus, SessionSnapshot, SessionStore};
use abylab_backend::{Cmd, CtlEvent, DriverConfig, Event, TurnLimits, UiEvent};
use common::{MockServer, Reply, single_call_reply, text_body};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

struct Live {
    handle: Option<abylab_backend::driver::DriverHandle>,
    rx: mpsc::Receiver<Event>,
    workspace: PathBuf,
}
impl Live {
    fn start(server: &MockServer) -> Self {
        Self::start_with_setup(server, |_| {})
    }
    fn start_with_setup(server: &MockServer, setup: impl FnOnce(&std::path::Path)) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "aby-session-regression-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        setup(&workspace);
        let (tx, rx) = mpsc::channel();
        let handle = abylab_backend::driver::spawn(
            DriverConfig {
                session_id: "session".into(),
                resume: None,
                sessions_root: None,
                home: Some(workspace.join("home").to_string_lossy().into_owned()),
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: Some("fixture-key".into()),
                base_url: Some(server.base_url.clone()),
                limits: TurnLimits {
                    continuations: 1,
                    ..Default::default()
                },
                compaction: None,
            },
            move |event| {
                let _ = tx.send(event);
            },
        )
        .unwrap();
        let live = Self {
            handle: Some(handle),
            rx,
            workspace,
        };
        live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { .. })));
        live
    }
    fn send(&self, cmd: Cmd) {
        self.handle.as_ref().unwrap().send(cmd);
    }
    fn wait(&self, predicate: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let event = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("driver event deadline");
            if let Event::PermissionAsk { reply, .. } = event {
                let _ = reply.send(abylab_backend::PermissionReply::Selected("allow".into()));
                continue;
            }
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
    fn done(&self, prefix: &str) {
        self.wait(|e| matches!(e, Event::Ctl(CtlEvent::TuiOpDone(s)) if s.starts_with(prefix)));
    }
    fn prompt(&self, text: &str) {
        self.send(Cmd::Prompt { text: text.into() });
        self.wait(|e| matches!(e, Event::Ui(UiEvent::TurnEnd { session, kind }) if session == "session" && kind == "completed"));
    }
    fn snapshot(&self) -> SessionSnapshot {
        SessionStore::new(&self.workspace)
            .unwrap()
            .load("session")
            .unwrap()
            .1
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
            drop(handle);
        }
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

#[test]
fn child_with_its_own_host_policy_inherits_workspace_instructions() {
    let server = MockServer::start(vec![
        Reply::sse(single_call_reply(
            "spawn",
            "subagent",
            r#"{"description":"child","prompt":"child task"}"#,
        )),
        Reply::sse(text_body("second", "done")),
        Reply::sse(text_body("third", "done")),
    ]);
    let live = Live::start_with_setup(&server, |workspace| {
        std::fs::write(workspace.join("AGENTS.md"), "CHILD_MUST_SEE_THIS_RULE").unwrap();
    });
    live.send(Cmd::Prompt {
        text: "delegate the task".into(),
    });
    let parent_done = std::cell::Cell::new(false);
    let child_done = std::cell::Cell::new(false);
    live.wait(|event| {
        if matches!(event, Event::Ui(UiEvent::TurnEnd { session, kind }) if session == "session" && kind == "completed") {
            parent_done.set(true);
        }
        if matches!(event, Event::Ui(UiEvent::SubagentFinished { .. })) {
            child_done.set(true);
        }
        parent_done.get() && child_done.get()
    });
    let requests = server.bodies();
    assert_eq!(requests.len(), 3, "parent, child and parent continuation");
    for request in &requests {
        assert_eq!(request.matches("CHILD_MUST_SEE_THIS_RULE").count(), 1);
    }
}

#[test]
fn workspace_instructions_survive_compaction_and_reload_on_resume_without_polluting_history() {
    let server = MockServer::start(vec![
        Reply::sse(text_body("first", &"answer ".repeat(400))),
        Reply::sse(text_body("summary", "short task summary")),
        Reply::sse(text_body("compacted", "done")),
        Reply::sse(text_body("changed", "done")),
        Reply::sse(text_body("removed", "done")),
    ]);
    let live = Live::start_with_setup(&server, |workspace| {
        std::fs::create_dir_all(workspace.join("home")).unwrap();
        std::fs::write(workspace.join("home/AGENTS.md"), "GLOBAL_RULE_MARKER").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "PROJECT_RULE_OLD").unwrap();
    });
    let task = "USER_TASK ".repeat(200);
    live.prompt(&task);
    let store = SessionStore::new(&live.workspace).unwrap();
    let snapshot = store.load("session").unwrap().1;
    assert!(store.title_of("session").unwrap().starts_with("USER_TASK"));
    assert!(
        !serde_json::to_string(&snapshot.items)
            .unwrap()
            .contains("PROJECT_RULE_OLD")
    );
    assert!(!snapshot.system_prompt.contains("PROJECT_RULE_OLD"));
    live.send(Cmd::Compact);
    live.done("compacted");
    live.prompt("after compaction");

    let resume = || {
        live.send(Cmd::NewSession {
            session_id: "other".into(),
        });
        live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "other"));
        live.send(Cmd::Resume {
            session_id: "session".into(),
        });
        live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "session"));
    };
    std::fs::write(live.workspace.join("AGENTS.md"), "PROJECT_RULE_NEW").unwrap();
    resume();
    live.prompt("after file update");
    std::fs::remove_file(live.workspace.join("AGENTS.md")).unwrap();
    std::fs::remove_file(live.workspace.join("home/AGENTS.md")).unwrap();
    resume();
    live.prompt("after removal");
    let requests = server.bodies();
    assert_eq!(requests.len(), 5);
    for request in &requests[..3] {
        let body: serde_json::Value = serde_json::from_str(request).unwrap();
        assert_eq!(
            body["messages"]
                .to_string()
                .matches("PROJECT_RULE_OLD")
                .count(),
            1
        );
        assert_eq!(
            body["messages"]
                .to_string()
                .matches("GLOBAL_RULE_MARKER")
                .count(),
            1
        );
        assert!(!body["system"].to_string().contains("PROJECT_RULE_OLD"));
    }
    assert!(requests[3].contains("PROJECT_RULE_NEW"));
    assert!(requests[3].contains("GLOBAL_RULE_MARKER"));
    assert!(!requests[3].contains("PROJECT_RULE_OLD"));
    assert!(!requests[4].contains("PROJECT_RULE_"));
    assert!(!requests[4].contains("GLOBAL_RULE_MARKER"));
    assert!(requests[4].contains("Earlier file-sourced workspace instruction baselines"));
}

#[test]
fn settings_changes_preserve_the_writer_and_child_control_identity() {
    let server = MockServer::start(vec![
        Reply::sse(single_call_reply(
            "spawn",
            "subagent",
            r#"{"description":"child","prompt":"child task"}"#,
        )),
        Reply::sse(text_body("child", "CHILD_RESULT")),
        Reply::sse(text_body("parent", "delegated")),
        Reply::dynamic(|raw| {
            // The user passes the id received from the actual child-start event.
            let body: serde_json::Value = serde_json::from_str(raw).unwrap();
            let messages = body["messages"].as_array().unwrap();
            let text = messages.last().unwrap()["content"][0]["text"]
                .as_str()
                .unwrap();
            let id = text.strip_prefix("collect ").unwrap();
            Reply::sse(single_call_reply(
                "collect",
                "wait_agent",
                &serde_json::json!({"agent_id":id}).to_string(),
            ))
        }),
        Reply::sse(text_body("collected", "collected child")),
        Reply::sse(text_body("resumed", "resumed after switching away")),
    ]);
    let live = Live::start(&server);
    live.send(Cmd::Prompt {
        text: "delegate".into(),
    });
    let event = live.wait(|e| matches!(e, Event::Ui(UiEvent::SubagentStarted { .. })));
    let Event::Ui(UiEvent::SubagentStarted { child, parent, .. }) = event else {
        unreachable!()
    };
    assert_eq!(
        parent, "session",
        "route the event to the durable UI session"
    );
    live.wait(|e| matches!(e, Event::Ui(UiEvent::TurnEnd { session, kind }) if session == "session" && kind == "completed"));
    live.send(Cmd::SetPermission {
        preset: "read-only".into(),
    });
    live.done("permission → read-only");
    live.send(Cmd::SetApiKey {
        key: Some("rotated-fixture-key".into()),
    });
    live.done("api key set");
    live.prompt(&format!("collect {child}"));
    let stored = live.snapshot();
    assert!(stored.items.iter().any(|item| matches!(item, abycore::Item::FunctionCallOutput { call_id, output, is_error: false, .. } if call_id == "collect" && output.contains("CHILD_RESULT"))), "the live parent must still be authorized to collect its child");
    live.send(Cmd::NewSession {
        session_id: "other".into(),
    });
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "other"));
    live.send(Cmd::Resume {
        session_id: "session".into(),
    });
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::SessionBound { session_id, .. }) if session_id == "session"));
    live.prompt("continue restored session");
    assert_eq!(
        server.bodies().len(),
        6,
        "children retained by the manager must not hold an abandoned writer"
    );
}

#[test]
fn compact_and_goal_commands_are_durable_before_the_success_event() {
    let server = MockServer::start(vec![
        Reply::sse(text_body("first", &"answer ".repeat(800))),
        Reply::sse(text_body("summary1", &"checkpoint detail ".repeat(30))),
        Reply::sse(text_body("summary2", "merged checkpoint")),
        Reply::sse(text_body("goal", "one round")),
    ]);
    let live = Live::start(&server);
    live.prompt(&"original history ".repeat(1000));
    live.send(Cmd::Compact);
    live.done("compacted");
    assert_eq!(live.snapshot().compactions.len(), 1);
    live.send(Cmd::Compact);
    live.done("compacted");
    let saved = live.snapshot();
    assert_eq!(
        saved.compactions.len(),
        1,
        "replace the previous checkpoint"
    );
    assert_eq!(saved.compactions[0].summary, "merged checkpoint");
    let requests = server.bodies();
    assert!(requests[2].contains("checkpoint detail"));
    assert!(!requests[2].contains("original history"));
    live.send(Cmd::Goal {
        arg: "@1 finish requested work".into(),
    });
    live.wait(|e| matches!(e, Event::Ctl(CtlEvent::TuiOpFailed(s)) if s.starts_with("goal rounds exhausted")));
    live.done("goal settled");
    assert_eq!(live.snapshot().goal.unwrap().status, GoalStatus::Blocked);
    for (command, status) in [
        ("pause", GoalStatus::Paused),
        ("complete", GoalStatus::Complete),
    ] {
        live.send(Cmd::Goal {
            arg: command.into(),
        });
        live.done("goal →");
        assert_eq!(live.snapshot().goal.unwrap().status, status);
    }
    live.send(Cmd::Goal {
        arg: "clear".into(),
    });
    live.done("goal cleared");
    assert!(live.snapshot().goal.is_none());
}

#[test]
fn manual_and_overflow_summaries_are_interruptible_and_keep_history() {
    for overflow in [false, true] {
        let mut replies = vec![Reply::sse(text_body("first", &"answer ".repeat(1200)))];
        if overflow {
            replies.push(Reply::error(400, "context_length_exceeded"));
        }
        replies.push(Reply::sse(text_body("summary", "summary")).delayed(Duration::from_secs(2)));
        let server = MockServer::start(replies);
        let live = Live::start(&server);
        live.prompt(&"original history ".repeat(1000));
        if overflow {
            live.send(Cmd::Prompt {
                text: "next prompt".into(),
            });
        } else {
            live.send(Cmd::Compact);
        }
        live.wait(|e| matches!(e, Event::Ui(UiEvent::SessionStatus { session, running: true }) if session == "session"));
        let expected = if overflow { 3 } else { 2 };
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.bodies().len() < expected {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let now = Instant::now();
        live.handle.as_ref().unwrap().interrupt();
        if overflow {
            live.wait(|e| matches!(e, Event::Ui(UiEvent::TurnEnd { session, kind }) if session == "session" && kind == "interrupted"));
        } else {
            live.wait(
                |e| matches!(e, Event::Ctl(CtlEvent::TuiOpFailed(s)) if s.contains("cancelled")),
            );
        }
        assert!(now.elapsed() < Duration::from_secs(1));
        let snapshot = live.snapshot();
        assert!(snapshot.compactions.is_empty());
        assert_eq!(
            snapshot.requests.len(),
            expected,
            "persist the interrupted request ledger too"
        );
    }
}
