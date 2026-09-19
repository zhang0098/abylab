mod common;

use common::{Reply, Scenario, incomplete_reply, text_reply};

#[test]
fn repeated_truncation_accepts_the_next_user_instruction_without_resuming_first() {
    let run = common::drive(
        Scenario::new(
            "incomplete-followup",
            vec![
                Reply::sse(incomplete_reply("first")),
                Reply::sse(incomplete_reply("second")),
            ],
        )
        .prompt("original task")
        .prompt("stop that task and answer briefly"),
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 2, "{}", run.explain());
    assert!(run.requests[1].contains("stop that task and answer briefly"));
    assert_eq!(
        run.events
            .iter()
            .filter(|event| *event == "turn-end:incomplete")
            .count(),
        2,
        "{}",
        run.explain(),
    );
    assert!(
        run.events
            .iter()
            .any(|event| event.contains("--max-tokens")),
        "{}",
        run.explain()
    );
}

#[test]
fn incomplete_goal_round_stops_before_starting_another_round() {
    let run = common::drive(
        Scenario::new(
            "incomplete-goal",
            vec![Reply::sse(incomplete_reply("round-1"))],
        )
        .goal("@3 finish the task"),
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 1);
    assert_eq!(
        run.events
            .iter()
            .filter(|event| event.starts_with("op-done:goal round "))
            .count(),
        1,
        "{}",
        run.explain(),
    );
}

#[test]
fn configured_output_limit_reaches_the_provider() {
    let run = common::drive(
        Scenario::new("output-limit", vec![Reply::sse(text_reply())])
            .max_tokens(32768)
            .prompt("answer"),
    );
    assert!(run.is_clean(), "{}", run.explain());
    let request: serde_json::Value = serde_json::from_str(&run.requests[0]).unwrap();
    assert_eq!(request["max_tokens"], 32768);
}

#[test]
fn invalid_output_limits_are_rejected_before_starting_the_driver() {
    for max_tokens in [0, u32::MAX as u64 + 1] {
        let result = abylab_backend::driver::spawn(
            abylab_backend::DriverConfig {
                session_id: "invalid-limit".into(),
                resume: None,
                sessions_root: None,
                home: None,
                workspace: String::new(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: Some(max_tokens),
                api_key: None,
                base_url: None,
                limits: Default::default(),
                compaction: None,
            },
            |_| panic!("invalid configuration must not start the driver"),
        );
        assert!(matches!(result, Err(message) if message.contains("--max-tokens")));
    }
}

#[test]
fn resumed_incomplete_session_accepts_new_input_and_output_limit() {
    let run = common::drive(
        Scenario::new("resume-incomplete", vec![Reply::sse(text_reply())])
            .max_tokens(32768)
            .resume("old-session")
            .seed(|workspace| {
                let mut snapshot =
                    abycore::SessionSnapshot::new("system", abycore::ModelOptions::default());
                snapshot.items.push(abycore::Item::user("unfinished task"));
                snapshot.needs_response = true;
                let store = abycore::SessionStore::new(workspace).unwrap();
                let mut writer = store.create("old-session", &snapshot).unwrap();
                store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
            })
            .prompt("finish with a short answer"),
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 1, "no preliminary request for the old task");
    let request: serde_json::Value = serde_json::from_str(&run.requests[0]).unwrap();
    assert_eq!(request["max_tokens"], 32768);
    assert!(run.requests[0].contains("unfinished task"));
    assert!(run.requests[0].contains("finish with a short answer"));
}
