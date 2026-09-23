//! Paid end-to-end probe for a model-requested question and its answer.

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use abylab_backend::driver::spawn;
use abylab_backend::{
    Cmd, CtlEvent, DriverConfig, Event, PermissionReply, TurnLimits, UiEvent, UserQuestionReply,
};

#[test]
#[ignore = "paid probe; requires DEEPSEEK_API_KEY"]
fn model_asks_and_continues_after_a_real_answer() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly");
    let root = std::env::temp_dir().join(format!(
        "abylab-live-question-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let answers = Arc::new(Mutex::new(Vec::<String>::new()));
    let text = Arc::new(Mutex::new(String::new()));
    let question_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = {
        let calls = Arc::clone(&calls);
        let answers = Arc::clone(&answers);
        let text = Arc::clone(&text);
        let question_count = Arc::clone(&question_count);
        move |event| match event {
            Event::UserQuestion { question, reply } => {
                assert_eq!(question.id, "confirm");
                assert_eq!(
                    question
                        .options
                        .iter()
                        .map(|o| o.label.as_str())
                        .collect::<Vec<_>>(),
                    ["Yes", "No"]
                );
                question_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                reply
                    .send(Some(UserQuestionReply {
                        selected: vec!["Yes".into()],
                        custom: None,
                    }))
                    .unwrap();
            }
            Event::Ui(UiEvent::ToolCall { name, .. }) => calls.lock().unwrap().push(name),
            Event::Ui(UiEvent::ToolResult { text, .. }) => answers.lock().unwrap().push(text),
            Event::Ui(UiEvent::TextDelta { text: delta, .. }) => {
                text.lock().unwrap().push_str(&delta)
            }
            Event::Ui(UiEvent::TurnEnd { kind, .. }) => {
                let _ = done_tx.send(kind);
            }
            Event::Ctl(CtlEvent::Error(error)) => {
                let _ = done_tx.send(format!("error: {error}"));
            }
            Event::PermissionAsk { reply, .. } => {
                let _ = reply.send(PermissionReply::Cancelled);
            }
            _ => {}
        }
    };
    let handle = spawn(
        DriverConfig {
            session_id: "live-user-question".into(),
            resume: None,
            sessions_root: Some(root.join("sessions").to_string_lossy().into_owned()),
            home: None,
            workspace: workspace.to_string_lossy().into_owned(),
            model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
            reasoning: "off".into(),
            permission: Some("danger-full-access".into()),
            max_tokens: None,
            api_key: Some(key),
            base_url: std::env::var("DEEPSEEK_BASE_URL").ok(),
            limits: TurnLimits {
                max_requests: 4,
                max_tool_calls: 2,
                continuations: 0,
                run_timeout: None,
                tool_timeout: Duration::from_secs(60),
            },
            compaction: None,
        },
        sink,
    )
    .unwrap();
    handle.send(Cmd::Prompt { text: "Call ask_user_question with exactly one question: id 'confirm', question 'Continue?', options labeled 'Yes' and 'No'. Wait for my answer, then reply with exactly 'Answer: Yes'.".into() });
    let end = done_rx
        .recv_timeout(Duration::from_secs(180))
        .expect("real API turn finished");
    handle.shutdown();
    drop(handle);
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(end, "completed");
    assert_eq!(question_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(*calls.lock().unwrap(), ["ask_user_question"]);
    assert!(
        answers
            .lock()
            .unwrap()
            .iter()
            .any(|text| text.contains("\"selected\":[\"Yes\"]"))
    );
    assert!(
        text.lock().unwrap().contains("Answer: Yes"),
        "model did not continue with the selected answer"
    );
}
