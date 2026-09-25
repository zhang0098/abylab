//! Full driver → controller → TUI regression for restoring pending work.
use crate::{app, bus, controller, runtime, theme};
#[path = "../../../abylab-backend/tests/common/mod.rs"]
mod common;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abylab-review-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}
#[test]
fn resume_keeps_the_restored_queue_visible() {
    use bus::{AppEvent, Cmd, CtlEvent};
    let root = scratch("resume");
    let server =
        common::MockServer::start(vec![common::Reply::sse(common::text_body("first", "done"))
            .delayed(Duration::from_millis(400))]);
    let cfg = runtime::RuntimeConfig {
        workspace: root.to_string_lossy().into_owned(),
        home: root.join("home").to_string_lossy().into_owned(),
        sessions_root: root.join("sessions").to_string_lossy().into_owned(),
        provider: "deepseek-official".into(),
        model: "deepseek-flash".into(),
        max_tokens: None,
        base_url: Some(server.base_url.clone()),
        api_key: Some("fixture-key".into()),
        key_origin: None,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let ctl = controller::Controller::start_aby(
        cfg.clone(),
        "old".into(),
        tx,
        abylab_backend::TurnLimits::default(),
        None,
    );
    let mut app = app::App::new(theme::Theme::dark(), cfg, "old".into());
    let fold_until = |app: &mut app::App, name: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ev = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            let bound = matches!(&ev, AppEvent::Ctl(CtlEvent::SessionBound{session_id,..}) if session_id==name);
            app.handle(ev, &ctl);
            if bound {
                break;
            }
        }
    };
    fold_until(&mut app, "old");
    ctl.send(Cmd::Prompt {
        session_id: "old".into(),
        text: "first".into(),
    });
    // FIFO ordering guarantees queue is stored, then the switch runs before it can drain.
    ctl.send(Cmd::Queue {
        session_id: "old".into(),
        item_id: 17,
        text: "pending work".into(),
    });
    ctl.send(Cmd::NewSession);
    // NewSession asks the driver to mint a new session id. Read its bound event.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        let bound = matches!(&ev, AppEvent::Ctl(CtlEvent::SessionBound{session_id,..}) if session_id!="old");
        app.handle(ev, &ctl);
        if bound {
            break;
        }
    }
    ctl.send(Cmd::LoadSession {
        session_id: "old".into(),
    });
    fold_until(&mut app, "old");
    // Permission facts mark the end of binding/replay, after the queue snapshot.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        let settled = matches!(&event, AppEvent::Ui(crate::events::UiEvent::PermissionPreset { session, .. }) if session == "old");
        app.handle(event, &ctl);
        if settled {
            break;
        }
    }
    let visible = app.queued;
    ctl.send(Cmd::Shutdown);
    drop(ctl);
    // Dropping the last event sender proves the controller and driver stopped.
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(error) => panic!("driver did not shut down: {error}"),
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(visible, 1, "restored queued work vanished from the TUI");
}
