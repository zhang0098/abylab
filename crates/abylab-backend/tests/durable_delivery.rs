mod common;

use abylab_backend::{Cmd, CtlEvent, DriverConfig, Event, UiEvent};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

const MARKER: &str = "review-queued-message-must-survive-crash";

fn config(workspace: &Path, home: &Path, base: &str) -> DriverConfig {
    DriverConfig {
        messages: Default::default(),
        web: Default::default(),
        session_id: "review-session".into(),
        resume: None,
        sessions_root: None,
        home: Some(home.display().to_string()),
        workspace: workspace.display().to_string(),
        model: "fixture-model".into(),
        reasoning: "off".into(),
        permission: Some("danger-full-access".into()),
        max_tokens: None,
        api_key: Some("local-fixture-key".into()),
        base_url: Some(base.into()),
        limits: Default::default(),
        compaction: None,
    }
}

fn contains_marker(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let p = entry.path();
        if p.is_dir() {
            contains_marker(&p)
        } else {
            std::fs::read_to_string(p).is_ok_and(|text| text.contains(MARKER))
        }
    })
}

#[test]
#[ignore]
fn queue_crash_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("ABY_REVIEW_SCRATCH").unwrap());
    let workspace = root.join("workspace");
    let home = root.join("home");
    let mut cfg = config(&workspace, &home, "http://127.0.0.1:9");
    cfg.resume =
        (std::env::var("ABY_FRESH").as_deref() != Ok("true")).then(|| cfg.session_id.clone());
    let stage = std::env::var("ABY_CRASH_STAGE").unwrap();
    let persisted = Arc::new(AtomicBool::new(false));
    let observed = persisted.clone();
    let handle = abylab_backend::driver::spawn(cfg, move |event| match event {
        Event::Ctl(CtlEvent::Queue { items, .. }) if !items.is_empty() => {
            assert!(contains_marker(&home.join("queued")));
            observed.store(true, Ordering::SeqCst);
            if stage == "queued" {
                std::process::exit(73);
            }
        }
        Event::Ctl(CtlEvent::PromptQueued { .. }) if stage == "admitting" => {
            std::process::exit(73);
        }
        Event::Ctl(CtlEvent::QueueClaimed { .. }) => {
            assert!(observed.load(Ordering::SeqCst));
            std::process::exit(73);
        }
        Event::Ctl(CtlEvent::Error(error)) => panic!("{error}"),
        _ => {}
    })
    .unwrap();
    handle.send(Cmd::QueueForSession {
        session_id: "review-session".into(),
        item_id: 41,
        text: MARKER.into(),
    });
    std::thread::sleep(Duration::from_secs(10));
    panic!("claim was not observed");
}

#[test]
fn durable_input_survives_process_exit_at_each_handoff_boundary() {
    for (stage, fresh) in [
        ("queued", false),
        ("admitting", false),
        ("committed", false),
        ("queued", true),
        ("admitting", true),
        ("committed", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = abycore::SessionStore::new(&workspace).unwrap();
        let snapshot = abycore::SessionSnapshot::new("fixture", Default::default());
        if !fresh {
            let mut writer = store.create_new("review-session", &snapshot).unwrap();
            store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "queue_crash_worker", "--ignored", "--nocapture"])
            .env("ABY_REVIEW_SCRATCH", temp.path())
            .env("ABY_CRASH_STAGE", stage)
            .env("ABY_FRESH", fresh.to_string())
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(73),
            "{stage}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let snapshot = store.load("review-session").unwrap().1;
        let committed = snapshot
            .items
            .iter()
            .filter(|item| serde_json::to_string(item).unwrap().contains(MARKER))
            .count();
        assert!(contains_marker(temp.path()), "input was lost at {stage}");
        if stage == "committed" {
            assert_eq!(committed, 1);
        } else {
            assert_eq!(committed, 0);
            assert!(contains_marker(&temp.path().join("home/queued")));
        }
    }
}

fn wait_for(rx: &mpsc::Receiver<Event>, predicate: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut events = vec![];
    loop {
        let event = rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(
            !matches!(&event, Event::Ctl(CtlEvent::Error(_))),
            "{event:?}"
        );
        let done = predicate(&event);
        events.push(event);
        if done {
            return events;
        }
    }
}

#[test]
fn completed_children_do_not_exhaust_capacity_across_new_sessions() {
    let mut replies = vec![];
    for i in 0..65 {
        replies.push(common::Reply::sse(common::single_call_reply(
            &format!("delegate-{i}"),
            "subagent",
            r#"{"description":"review child","prompt":"return done"}"#,
        )));
        replies.push(common::Reply::sse(common::text_body(
            &format!("child-answer-{i}"),
            "done",
        )));
        replies.push(common::Reply::sse(common::text_body(
            &format!("parent-answer-{i}"),
            "done",
        )));
    }
    let server = common::MockServer::start(replies);
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = abylab_backend::driver::spawn(
        config(&workspace, &temp.path().join("home"), &server.base_url),
        move |event| {
            let _ = tx.send(event);
        },
    )
    .unwrap();
    wait_for(&rx, |e| {
        matches!(e, Event::Ctl(CtlEvent::SessionBound { .. }))
    });
    for i in 0..65 {
        if i == 64 {
            handle.send(Cmd::NewSession {
                session_id: "review-fresh-session".into(),
            });
            wait_for(&rx, |e| {
                matches!(e, Event::Ctl(CtlEvent::SessionBound { .. }))
            });
        }
        handle.send(Cmd::Prompt {
            text: format!("delegate task {i}"),
        });
        let events = wait_for(&rx, |e| matches!(e, Event::Ui(UiEvent::TurnEnd { .. })));
        let error = events.iter().find_map(|e| match e {
            Event::Ui(UiEvent::ToolResult {
                is_error: true,
                text,
                ..
            }) => Some(text),
            _ => None,
        });
        assert!(error.is_none(), "child {i}: {error:?}");
    }
    handle.shutdown();
}
