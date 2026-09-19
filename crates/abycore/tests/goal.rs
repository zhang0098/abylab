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
    assert_eq!(goal.max_rounds, 2);
    assert!(
        agent.set_goal("another", None).is_err(),
        "one goal per session while it is not complete"
    );

    let first = agent.begin_goal_round().unwrap().expect("round one");
    assert_eq!(first.rounds_started, 1);
    assert_eq!(first.remaining_rounds(), 1);
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
    assert!(agent.set_goal("x", Some(1_000)).is_err());
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
    assert_eq!(goal.max_rounds, 3);
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
    assert_eq!(restored.goal().unwrap().max_rounds, 5);

    let mut broken = SessionSnapshot::from_json(&json).unwrap();
    broken.goal.as_mut().unwrap().max_rounds = 0;
    assert!(broken.validate().is_err());
}
