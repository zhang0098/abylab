mod common;
use abycore::*;
use common::*;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(unix)]
#[tokio::test]
async fn four_tools_execute_in_order_and_results_survive_restore() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(vec![
        Reply::sse(response("r1",vec![
            call("c1","write",&json!({"file_path":"note.txt","content":"alpha alpha\n"}).to_string()),
            call("c2","read",&json!({"file_path":"note.txt"}).to_string()),
            call("c3","edit",&json!({"file_path":"note.txt","old_string":"alpha","new_string":"beta","replace_all":true}).to_string()),
            call("c4","bash",&json!({"command":"for ((i=0;i<200;i++)); do printf 'beta\\n'; done","description":"Print long output"}).to_string()),
        ])),
        Reply::raw(503,"text/plain","temporary model failure"),
        Reply::sse(response("r2",vec![message("m","done")])),
    ]).await;
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    let tools = LocalTools::new(directory.path()).unwrap();
    tools.register(&mut agent).unwrap();
    let details = Arc::new(Mutex::new(vec![]));
    let observed = details.clone();
    let error = agent
        .run(
            "use all four tools",
            RunOptions {
                max_tool_output_bytes: 512,
                ..Default::default()
            },
            move |event| {
                if let AgentEvent::ToolFinished { output, .. } = event {
                    observed.lock().unwrap().push(output.details);
                }
                async { Ok(()) }
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Server);
    let snapshot = agent.snapshot();
    assert!(snapshot.pending.is_empty());
    let results: Vec<_> = snapshot
        .items
        .iter()
        .filter_map(|item| match item {
            Item::FunctionCallOutput { output, .. } => Some(output.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4);
    assert!(results[1].contains("1: alpha alpha"));
    assert!(results[2].contains("All occurrences"));
    let captured = details.lock().unwrap().clone();
    assert_eq!(captured[2].as_ref().unwrap()["replacements"], 2);
    assert_eq!(
        captured[2].as_ref().unwrap()["diffs"][0]["newText"],
        "beta beta\n"
    );
    let bash: BashResult = serde_json::from_value(captured[3].clone().unwrap()).unwrap();
    let log = bash.stdout.spill_path.unwrap();
    drop(captured);
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "beta\n".repeat(200));
    std::fs::write(directory.path().join("note.txt"), "external change").unwrap();
    let mut restored = Agent::restore(
        server.client(),
        SessionSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
    )
    .unwrap();
    tools.register(&mut restored).unwrap();
    restored
        .continue_run(RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "external change"
    );
    let requests = server.captured();
    assert_eq!(requests[1].body["messages"], requests[2].body["messages"]);
    assert_eq!(std::fs::read_to_string(log).unwrap(), "beta\n".repeat(200));
}

#[tokio::test]
async fn registering_bundle_is_atomic_on_duplicate_tool_name() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(vec![Reply::sse(response("r", vec![message("m", "ok")]))]).await;
    let tools = LocalTools::new(directory.path()).unwrap();
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    agent.register_tool(tools.read()).unwrap();
    assert!(tools.register(&mut agent).is_err());
    agent
        .run("hello", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(
        server.captured()[0].body["tools"].as_array().unwrap().len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_execution_timeout_returns_result_and_agent_continues_after_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(vec![
        Reply::sse(response("r",vec![call("c","bash",&json!({"command":"printf started > started; sleep 60","description":"Test host tool timeout"}).to_string())])),
        Reply::sse(response("done",vec![message("m","timeout handled")])),
    ]).await;
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    let mut config = LocalToolConfig::new(directory.path());
    config.bash_grace = Duration::from_millis(30);
    LocalTools::with_config(config)
        .unwrap()
        .register(&mut agent)
        .unwrap();
    let outcome = agent
        .run(
            "start",
            RunOptions {
                tool_timeout: Duration::from_millis(100),
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(agent.snapshot().pending.is_empty());
    assert!(directory.path().join("started").exists());
    assert!(outcome.new_items.iter().any(
        |item| matches!(item,Item::FunctionCallOutput{output,..} if output.contains("timed out"))
    ));
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn restoring_an_agent_requires_fresh_observation_before_editing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a"), "old").unwrap();
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "read", &json!({"file_path":"a"}).to_string())],
        )),
        Reply::sse(response("r2", vec![message("m1", "read")])),
        Reply::sse(response(
            "r3",
            vec![call(
                "c2",
                "edit",
                &json!({"file_path":"a","old_string":"old","new_string":"new"}).to_string(),
            )],
        )),
        Reply::sse(response("r4", vec![message("m2", "needs reread")])),
    ])
    .await;
    let tools = LocalTools::new(directory.path()).unwrap();
    let mut original = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    tools.register(&mut original).unwrap();
    original
        .run("read", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    let mut restored = Agent::restore(server.client(), original.snapshot()).unwrap();
    tools.register(&mut restored).unwrap();
    let outcome = restored
        .run("edit", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert!(outcome.new_items.iter().any(|item| matches!(item,Item::FunctionCallOutput{output,..} if output.contains("FS_NOT_OBSERVED"))));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("a")).unwrap(),
        "old"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn background_job_is_owned_by_host_after_the_agent_turn() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(vec![
        Reply::sse(response("r1",vec![call("c1","bash",&json!({"command":"sleep 0.1; printf finished","description":"Run background task","run_in_background":true}).to_string())])),
        Reply::sse(response("r2",vec![message("m","started")])),
    ]).await;
    let tools = LocalTools::new(directory.path()).unwrap();
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    tools.register(&mut agent).unwrap();
    agent
        .run("start", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert!(agent.snapshot().pending.is_empty());
    let id = tools.jobs()[0].id.clone();
    drop(agent);
    let job = tools.wait_job(&id).await.unwrap();
    assert_eq!(job.status, BashJobStatus::Completed);
    assert_eq!(job.result.unwrap().stdout.text, "finished");
    tools.shutdown().await;
}
