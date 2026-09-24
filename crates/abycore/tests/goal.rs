//! One durable completion objective per session: host API, model tools, and the
//! round bookkeeping a host driver reads.

mod common;
use abycore::*;
use common::*;

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

fn agent(server: &Server) -> Agent {
    let mut agent = Agent::new(server.client(), "system", ModelOptions::default()).unwrap();
    agent.register_tool(GetGoalTool).unwrap();
    agent.register_tool(CreateGoalTool).unwrap();
    agent.register_tool(UpdateGoalTool).unwrap();
    agent
}

/// The host path: create, pause, resume, complete, clear, and round spending.
#[tokio::test]
async fn host_api_owns_the_lifecycle_and_round_allowance() {
    let server = Server::start(vec![]).await;
    let mut agent = agent(&server);
    assert!(agent.goal().is_none());
    assert!(agent.update_goal(GoalStatus::Complete, None).is_err());
    assert_eq!(agent.begin_goal_round().unwrap(), None);

    let goal = agent.set_goal("ship the release", Some(2)).unwrap();
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(goal.max_rounds, Some(2));
    assert!(
        agent.set_goal("another", None).is_err(),
        "one goal per session while it is not complete"
    );

    let first = agent.begin_goal_round().unwrap().expect("round one");
    assert_eq!(first.rounds_started, 1);
    assert_eq!(first.remaining_rounds(), Some(1));
    let second = agent.begin_goal_round().unwrap().expect("round two");
    assert_eq!(second.rounds_started, 2);
    assert_eq!(
        agent.begin_goal_round().unwrap(),
        None,
        "the allowance is spent"
    );
    assert!(!agent.goal().unwrap().may_start_round());

    let paused = agent.update_goal(GoalStatus::Paused, None).unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert!(!paused.may_start_round(), "a paused goal spends no rounds");
    let resumed = agent
        .update_goal(GoalStatus::Active, Some("  ".into()))
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(resumed.note, None, "blank notes are dropped");

    agent.clear_goal();
    assert!(agent.goal().is_none());
    // Round limits are validated before anything is stored.
    assert!(agent.set_goal("x", Some(0)).is_err());
    assert!(agent.set_goal("x", Some(1_000)).is_ok());
    agent.clear_goal();
    assert!(agent.set_goal("   ", None).is_err());
}

/// The model path: create, read back, and complete with compare-and-set.
#[tokio::test]
async fn model_tools_create_read_and_complete_the_goal() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call(
                "c1",
                "create_goal",
                r#"{"objective":"ship the release","max_rounds":3}"#,
            )],
        )),
        Reply::sse(response("r2", vec![message("m2", "created")])),
    ])
    .await;
    let mut agent = agent(&server);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = events.clone();
    agent
        .run("long task please", RunOptions::default(), move |event| {
            if let AgentEvent::GoalChanged { goal } = event {
                captured.lock().unwrap().push(goal);
            }
            async { Ok(()) }
        })
        .await
        .unwrap();

    let goal = agent.goal().cloned().expect("the tool created the goal");
    assert_eq!(goal.objective, "ship the release");
    assert_eq!(goal.status, GoalStatus::Paused);
    assert!(!goal.may_start_round());
    assert_eq!(goal.max_rounds, Some(3));
    assert_eq!(goal.revision, 1);
    assert!(goal.id.starts_with("goal-"));
    let seen = events.lock().unwrap().clone();
    assert!(
        seen.iter().any(|event| event.is_some()),
        "the host is told about the goal: {seen:?}"
    );

    // The committed tool result is what the model saw, through the same meta.
    let output = agent
        .snapshot()
        .items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput { output, .. } if output.contains("goal") => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("a goal tool result");
    assert!(output.contains("ship the release"), "{output}");

    // Read it back, then complete it quoting the exact revision.
    let server = Server::start(vec![
        Reply::sse(response("g1", vec![call("g1", "get_goal", "{}")])),
        Reply::sse(response(
            "g2",
            vec![call(
                "g2",
                "update_goal",
                &format!(
                    r#"{{"goal_id":"{}","revision":{},"status":"complete"}}"#,
                    goal.id, goal.revision
                ),
            )],
        )),
        Reply::sse(response("g3", vec![message("m3", "done")])),
    ])
    .await;
    let mut agent = Agent::restore(server.client(), agent.snapshot()).unwrap();
    agent.register_tool(GetGoalTool).unwrap();
    agent.register_tool(UpdateGoalTool).unwrap();
    agent
        .run("finish it", RunOptions::default(), ignore)
        .await
        .unwrap();
    let goal = agent.goal().expect("still there");
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(goal.revision, 2, "an accepted update bumps the revision");
}

/// The read is answered by the agent, not by the stateless tool: the result a
/// host displays is the committed one, never the tool's empty placeholder.
#[tokio::test]
async fn the_host_sees_the_committed_goal_read() {
    let server = Server::start(vec![
        Reply::sse(response("r1", vec![call("c1", "get_goal", "{}")])),
        Reply::sse(response("r2", vec![message("m2", "read it")])),
    ])
    .await;
    let mut agent = agent(&server);
    let goal = agent.set_goal("ship the release", Some(3)).unwrap();
    let finished = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = finished.clone();
    agent
        .run("what is the goal?", RunOptions::default(), move |event| {
            if let AgentEvent::ToolFinished { call_id, output } = event {
                captured.lock().unwrap().push((call_id, output));
            }
            async { Ok(()) }
        })
        .await
        .unwrap();

    let seen = finished.lock().unwrap().clone();
    let [(call_id, output)] = seen.as_slice() else {
        panic!("one read finished: {seen:?}")
    };
    assert_eq!(call_id, "c1");
    assert!(!output.is_error);
    // The snapshot the model read, so the card and the transcript agree.
    assert!(
        output.content.contains(r#""objective":"ship the release""#),
        "{}",
        output.content
    );
    assert!(
        output.content.contains(r#""revision":1"#),
        "{}",
        output.content
    );
    let committed = agent
        .snapshot()
        .items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput {
                call_id, output, ..
            } if call_id == "c1" => Some(output.clone()),
            _ => None,
        })
        .expect("the read is committed");
    assert_eq!(committed, output.content);

    // A mutation reports what the agent committed (`goal → …`), not the
    // tool's proposal.
    let server = Server::start(vec![
        Reply::sse(response(
            "r3",
            vec![call(
                "c3",
                "update_goal",
                &format!(
                    r#"{{"goal_id":"{}","revision":{},"status":"complete","note":"shipped"}}"#,
                    goal.id, goal.revision
                ),
            )],
        )),
        Reply::sse(response("r4", vec![message("m4", "done")])),
    ])
    .await;
    let mut agent = Agent::restore(server.client(), agent.snapshot()).unwrap();
    agent.register_tool(UpdateGoalTool).unwrap();
    let finished = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = finished.clone();
    agent
        .run("complete it", RunOptions::default(), move |event| {
            if let AgentEvent::ToolFinished { call_id, output } = event {
                captured.lock().unwrap().push((call_id, output));
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    let seen = finished.lock().unwrap().clone();
    let [(_, output)] = seen.as_slice() else {
        panic!("one mutation finished: {seen:?}")
    };
    assert_eq!(
        output.content,
        "goal → ship the release · complete · round 0/3 · shipped"
    );
}

/// A stale revision is refused and leaves the stored goal untouched.
#[tokio::test]
async fn a_stale_revision_is_refused() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "create_goal", r#"{"objective":"ship it"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "created")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent
        .run("go", RunOptions::default(), ignore)
        .await
        .unwrap();
    let goal = agent.goal().cloned().unwrap();
    agent
        .update_goal(GoalStatus::Paused, None)
        .expect("the host pauses it, bumping the revision");

    let server = Server::start(vec![
        Reply::sse(response(
            "s1",
            vec![call(
                "s1",
                "update_goal",
                &format!(
                    r#"{{"goal_id":"{}","revision":{},"status":"complete"}}"#,
                    goal.id, goal.revision
                ),
            )],
        )),
        Reply::sse(response("s2", vec![message("m2", "done")])),
    ])
    .await;
    let mut agent = Agent::restore(server.client(), agent.snapshot()).unwrap();
    agent.register_tool(UpdateGoalTool).unwrap();
    agent
        .run("complete it", RunOptions::default(), ignore)
        .await
        .unwrap();

    let current = agent.goal().unwrap();
    assert_eq!(current.status, GoalStatus::Paused, "the stale write lost");
    assert!(
        agent.snapshot().items.iter().any(|item| matches!(
            item,
            Item::FunctionCallOutput { output, is_error: true, .. } if output.contains("stale")
        )),
        "the model is told why"
    );
}

/// The goal survives a snapshot round trip, and a corrupt one is rejected.
#[tokio::test]
async fn a_goal_round_trips_and_is_validated() {
    let server = Server::start(vec![]).await;
    let mut agent = agent(&server);
    let goal = agent.set_goal("keep going", Some(5)).unwrap();
    agent.begin_goal_round().unwrap();

    let json = agent.snapshot().to_json().unwrap();
    let restored = SessionSnapshot::from_json(&json).unwrap();
    assert_eq!(
        restored.goal.as_ref().map(|goal| goal.id.clone()),
        Some(goal.id)
    );
    assert_eq!(restored.goal.as_ref().unwrap().rounds_started, 1);
    let restored = Agent::restore(server.client(), restored).unwrap();
    assert_eq!(restored.goal().unwrap().max_rounds, Some(5));

    let mut broken = SessionSnapshot::from_json(&json).unwrap();
    broken.goal.as_mut().unwrap().max_rounds = Some(0);
    assert!(broken.validate().is_err());
}

#[tokio::test]
async fn resuming_with_a_larger_total_preserves_identity_and_spent_rounds() {
    let server = Server::start(vec![]).await;
    let mut agent = agent(&server);
    agent.set_goal("finish the migration", Some(1)).unwrap();
    agent.begin_goal_round().unwrap();
    let stopped = agent
        .update_goal(
            GoalStatus::Blocked,
            Some("round allowance exhausted".into()),
        )
        .unwrap();
    for allowance in [None, Some(0), Some(1)] {
        assert!(agent.resume_goal(allowance).is_err());
        assert_eq!(agent.goal(), Some(&stopped), "failed resume is atomic");
    }
    let resumed = agent.resume_goal(Some(3)).unwrap();
    assert_eq!(
        resumed,
        Goal {
            status: GoalStatus::Active,
            max_rounds: Some(3),
            revision: stopped.revision + 1,
            note: None,
            ..stopped
        }
    );
    agent.begin_goal_round().unwrap();
    agent.update_goal(GoalStatus::Paused, None).unwrap();
    let paused = agent.goal().unwrap().clone();
    assert!(agent.set_goal_rounds(Some(1)).is_err());
    assert_eq!(agent.goal(), Some(&paused));
    let extended = agent.set_goal_rounds(Some(5)).unwrap();
    assert_eq!(
        extended,
        Goal {
            max_rounds: Some(5),
            revision: paused.revision + 1,
            ..paused
        }
    );
    assert_eq!(
        extended.status,
        GoalStatus::Paused,
        "changing allowance never resumes"
    );
    agent.update_goal(GoalStatus::Complete, None).unwrap();
    let complete = agent.goal().cloned();
    assert!(agent.resume_goal(Some(10)).is_err());
    assert_eq!(agent.goal().cloned(), complete);
}

#[tokio::test]
async fn unlimited_goals_round_trip_and_only_explicit_limits_stop_rounds() {
    let server = Server::start(vec![]).await;
    let mut agent = agent(&server);
    let goal = agent.set_goal("long task", None).unwrap();
    assert_eq!(goal.max_rounds, None);
    for _ in 0..150 {
        assert!(agent.begin_goal_round().unwrap().is_some());
    }
    assert_eq!(agent.goal().unwrap().remaining_rounds(), None);
    let snapshot = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    assert_eq!(snapshot.goal, agent.goal().cloned());
    let limited = agent.set_goal_rounds(Some(150)).unwrap();
    assert!(!limited.may_start_round());
    assert!(agent.begin_goal_round().unwrap().is_none());
    // An older snapshot's numeric max_rounds retains its original allowance.
    let legacy = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    assert_eq!(legacy.goal.unwrap().max_rounds, Some(150));
    let unlimited = agent.set_goal_rounds(None).unwrap();
    assert_eq!(unlimited.id, goal.id);
    assert_eq!(unlimited.rounds_started, 150);
    assert!(agent.begin_goal_round().unwrap().is_some());
    assert!(agent.set_goal_rounds(Some(1000)).is_ok());
}

#[tokio::test]
async fn model_cannot_activate_a_paused_goal() {
    let server = Server::start(vec![]).await;
    let mut agent = agent(&server);
    agent.set_goal("wait for authorization", Some(3)).unwrap();
    let paused = agent.update_goal(GoalStatus::Paused, None).unwrap();
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call(
                "c1",
                "update_goal",
                &serde_json::json!({
                    "goal_id": paused.id, "revision": paused.revision, "status": "active"
                })
                .to_string(),
            )],
        )),
        Reply::sse(response(
            "r2",
            vec![message("m2", "resume requires the host")],
        )),
    ])
    .await;
    let mut restored = Agent::restore(server.client(), agent.snapshot()).unwrap();
    restored.register_tool(UpdateGoalTool).unwrap();
    restored
        .run("attempt resume", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(restored.goal(), Some(&paused));
    assert!(restored.snapshot().items.iter().any(|item| matches!(item, Item::FunctionCallOutput { output, .. } if output.contains("only the host"))));
}
