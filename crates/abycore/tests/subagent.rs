mod common;
use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn agent(server: &Server) -> Agent {
    Agent::new(server.client(), "parent persona", ModelOptions::default()).unwrap()
}

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

async fn settled(agents: &Subagents, id: &str) -> SubagentInfo {
    tokio::time::timeout(Duration::from_secs(3), agents.wait(id))
        .await
        .unwrap()
        .unwrap()
}

async fn requests(server: &Server, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.captured().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn delayed(text: &str) -> Reply {
    let mut reply = Reply::sse(response(text, vec![message("m", text)]));
    reply.header_delay = Duration::from_millis(100);
    reply
}

struct Effect(Arc<AtomicUsize>);
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "test effect".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
        Ok(())
    }
    fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("effect done"))
        })
    }
}

#[tokio::test]
async fn foreground_inherits_tools_but_keeps_history_and_accounting_separate() {
    let server = Server::start(vec![
        Reply::sse(response(
            "parent1",
            vec![call(
                "delegate",
                "subagent",
                r#"{"description":"child","prompt":"child task"}"#,
            )],
        )),
        Reply::sse(response("child1", vec![call("effect1", "effect", "{}")])),
        Reply::sse(response("child2", vec![message("m", "child answer")])),
        Reply::sse(response("parent2", vec![message("m", "parent answer")])),
    ])
    .await;
    let mut parent = agent(&server);
    let count = Arc::new(AtomicUsize::new(0));
    parent.register_tool(Effect(count.clone())).unwrap();
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    let mut events = agents.subscribe();
    let outcome = parent
        .run("parent private task", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(outcome.response.output_text(), "parent answer");
    assert_eq!(outcome.requests.len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let child = agents.list().pop().unwrap();
    assert_eq!(child.status, SubagentStatus::Idle);
    assert_eq!(child.parent_id, parent.id());
    assert_eq!(child.depth, 1);
    assert_eq!(child.result.as_ref().unwrap().output, "child answer");
    let child_snapshot = agents.snapshot(&child.id).unwrap();
    assert_eq!(child_snapshot.requests.len(), 2);
    child_snapshot.validate().unwrap();
    assert!(matches!(
        events.try_recv().unwrap(),
        SubagentEvent::Started(_)
    ));
    let bodies = server.captured();
    assert_eq!(bodies[1].body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        bodies[1].body["messages"][0]["content"][0]["text"],
        "child task"
    );
    assert_eq!(bodies[1].body["system"], bodies[0].body["system"]);
    let snapshot = parent.snapshot();
    assert!(
        !snapshot
            .items
            .iter()
            .any(|item| matches!(item, Item::FunctionCall { name, .. } if name == "effect"))
    );
    let output = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput { output, meta, .. } => Some((output, meta)),
            _ => None,
        })
        .unwrap();
    assert!(output.0.contains("child answer"));
    assert_eq!(output.1.as_ref().unwrap()["subagent"]["agent_id"], child.id);
    agents.shutdown().await;
}

#[tokio::test]
async fn fork_seeds_completed_turns_and_excludes_current_prompt_and_open_calls() {
    let server = Server::start(vec![
        Reply::sse(response("history", vec![message("m", "previous answer")])),
        Reply::sse(response(
            "parent1",
            vec![call(
                "fork",
                "subagent_fork",
                r#"{"description":"fork","prompt":"new child task"}"#,
            )],
        )),
        Reply::sse(response("child", vec![message("m", "fork answer")])),
        Reply::sse(response("parent2", vec![message("m", "done")])),
    ])
    .await;
    let mut parent = agent(&server);
    parent
        .run("previous question", RunOptions::default(), ignore)
        .await
        .unwrap();
    // Restored parents retain exactly the same completed-turn seeding behavior.
    let mut parent = Agent::restore(server.client(), parent.snapshot()).unwrap();
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    parent
        .run("current private turn", RunOptions::default(), ignore)
        .await
        .unwrap();
    let child = &server.captured()[2].body;
    assert_eq!(child["messages"].as_array().unwrap().len(), 3);
    let json = child["messages"].to_string();
    assert!(
        json.contains("previous question")
            && json.contains("previous answer")
            && json.contains("new child task")
    );
    assert!(!json.contains("current private turn") && !json.contains("tool_use"));
    agents.shutdown().await;
}

/// A fork whose inherited prefix is a compaction summary (the parent's
/// completed history condensed, the active prompt still unanswered) must seed
/// a valid child: the summary is a user item, so the snapshot has to say the
/// turn owes a response — restore used to reject the whole fork.
#[tokio::test]
async fn fork_seeds_a_compaction_summary_that_ends_at_the_active_prompt() {
    let server = Server::start(vec![Reply::sse(response(
        "child",
        vec![message("m", "fork answer")],
    ))])
    .await;
    let mut snapshot = SessionSnapshot::new("parent persona", ModelOptions::default());
    snapshot.items = vec![
        Item::user("previous question"),
        Item::Message {
            id: None,
            role: MessageRole::Assistant,
            content: vec![ContentPart::OutputText {
                text: "previous answer".into(),
            }],
        },
        Item::user("current private turn"),
    ];
    snapshot.needs_response = true;
    snapshot.compactions = vec![Compaction {
        start: 0,
        end: 2,
        summary: "condensed history".into(),
    }];
    let parent = Agent::restore(server.client(), snapshot).unwrap();
    let agents = Subagents::new();
    let request = SubagentRequest {
        description: "fork".into(),
        prompt: "new child task".into(),
        mode: SubagentMode::Fork,
    };
    let child = agents
        .start(&parent, request)
        .expect("the condensed prefix is seeded, not rejected");
    let info = settled(&agents, &child.id).await;
    assert_eq!(info.status, SubagentStatus::Idle);
    let body = &server.captured()[0].body;
    let json = body["messages"].to_string();
    assert!(
        json.contains("condensed history") && json.contains("new child task"),
        "{json}"
    );
    assert!(!json.contains("current private turn"), "{json}");
    agents.shutdown().await;
}

#[tokio::test]
async fn background_runs_concurrently_and_enforces_capacity_and_retention() {
    let server = Server::start(vec![delayed("first"), delayed("second"), delayed("third")]).await;
    let agents = Subagents::with_config(SubagentConfig {
        max_running: 2,
        max_agents: 2,
        ..Default::default()
    })
    .unwrap();
    let parent = agent(&server);
    let first = agents
        .start(&parent, SubagentRequest::new("one", "one"))
        .unwrap();
    let second = agents
        .start(&parent, SubagentRequest::new("two", "two"))
        .unwrap();
    assert_eq!(
        agents
            .start(&parent, SubagentRequest::new("three", "three"))
            .unwrap_err()
            .kind,
        ErrorKind::BudgetExceeded
    );
    requests(&server, 2).await;
    assert_eq!(
        agents.get(&first.id).unwrap().status,
        SubagentStatus::Running
    );
    assert_eq!(
        agents.get(&second.id).unwrap().status,
        SubagentStatus::Running
    );
    settled(&agents, &first.id).await;
    settled(&agents, &second.id).await;
    assert!(
        agents
            .start(&parent, SubagentRequest::new("three", "three"))
            .is_err()
    );
    agents.forget(&first.id).unwrap();
    let third = agents
        .start(&parent, SubagentRequest::new("three", "three"))
        .unwrap();
    settled(&agents, &third.id).await;
    agents.shutdown().await;
    assert!(
        agents
            .start(&parent, SubagentRequest::new("four", "four"))
            .is_err()
    );
    assert!(agents.send_message(&third.id, "more").is_err());
}

#[tokio::test]
async fn running_messages_steer_at_a_boundary_and_idle_messages_continue_same_session() {
    let server = Server::start(vec![
        delayed("first answer"),
        delayed("steered answer"),
        delayed("followup answer"),
    ])
    .await;
    let agents = Subagents::new();
    let child = agents
        .start(&agent(&server), SubagentRequest::new("task", "initial"))
        .unwrap();
    requests(&server, 1).await;
    agents.send_message(&child.id, "steering").unwrap();
    let info = settled(&agents, &child.id).await;
    assert_eq!(info.result.unwrap().output, "steered answer");
    let snapshot = agents.snapshot(&child.id).unwrap();
    assert_eq!(snapshot.run_sequence, 1);
    assert!(server.captured()[1].body.to_string().contains("steering"));
    agents.send_message(&child.id, "followup").unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.result.unwrap().output,
        "followup answer"
    );
    assert_eq!(agents.snapshot(&child.id).unwrap().run_sequence, 2);
    assert!(server.captured()[2].body.to_string().contains("initial"));
    agents.shutdown().await;
}

#[tokio::test]
async fn foreground_parent_cancellation_stops_child_and_preserves_unknown_parent_call() {
    let mut slow = delayed("late");
    slow.header_delay = Duration::from_secs(30);
    let server = Server::start(vec![
        Reply::sse(response(
            "parent",
            vec![call(
                "delegate",
                "subagent",
                r#"{"description":"slow","prompt":"slow"}"#,
            )],
        )),
        slow,
    ])
    .await;
    let mut parent = agent(&server);
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let work = parent.run(
        "delegate",
        RunOptions {
            cancellation,
            ..Default::default()
        },
        ignore,
    );
    let cancelling = async {
        requests(&server, 2).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(work, cancelling);
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
    assert_eq!(parent.snapshot().pending[0].state, PendingState::Unknown);
    let child = agents.list().pop().unwrap();
    let result = settled(&agents, &child.id).await.result.unwrap();
    assert_eq!(result.stop_reason, StopReason::Error(ErrorKind::Cancelled));
    agents.shutdown().await;
}

#[tokio::test]
async fn model_background_survives_parent_tool_return_and_can_be_collected() {
    let server = Server::start_with_handler(|req| {
        let text = req.body["messages"].to_string();
        if text.contains("background child")
            && !text.contains("tool_result")
            && req.body["messages"].as_array().unwrap().len() == 1
        {
            delayed("background answer")
        } else if !text.contains("tool_result") {
            Reply::sse(response(
                "parent1",
                vec![call(
                    "bg",
                    "subagent",
                    r#"{"description":"bg","prompt":"background child","run_in_background":true}"#,
                )],
            ))
        } else {
            Reply::sse(response("parent2", vec![message("m", "started")]))
        }
    })
    .await;
    let mut parent = agent(&server);
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    let cancellation = CancellationToken::new();
    parent
        .run(
            "start task",
            RunOptions {
                cancellation: cancellation.clone(),
                ..Default::default()
            },
            ignore,
        )
        .await
        .unwrap();
    cancellation.cancel();
    let child = agents.list().pop().unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.result.unwrap().output,
        "background answer"
    );
    agents.shutdown().await;
}

#[tokio::test]
async fn recursive_delegation_is_rejected_at_runtime_depth_limit() {
    let server = Server::start(vec![
        Reply::sse(response(
            "child1",
            vec![call(
                "nested",
                "subagent",
                r#"{"description":"nested","prompt":"nested"}"#,
            )],
        )),
        Reply::sse(response("child2", vec![message("m", "limit handled")])),
    ])
    .await;
    let agents = Subagents::with_config(SubagentConfig {
        max_depth: 1,
        ..Default::default()
    })
    .unwrap();
    let mut parent = agent(&server);
    agents.register(&mut parent).unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("one", "one"))
        .unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.result.unwrap().output,
        "limit handled"
    );
    assert_eq!(agents.list().len(), 1);
    assert!(
        server.captured()[1]
            .body
            .to_string()
            .contains("subagent depth limit reached")
    );
    agents.shutdown().await;
}

#[derive(Default)]
struct Hooks {
    checkpoints: Mutex<Vec<(CheckpointKind, SessionSnapshot)>>,
    authorizations: AtomicUsize,
    deny: bool,
}
impl AgentHooks for Hooks {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            snapshot.validate()?;
            self.checkpoints.lock().unwrap().push((kind, snapshot));
            Ok(())
        })
    }
    fn authorize<'a>(&'a self, _: PendingCall) -> BoxFuture<'a, Result<ToolDecision>> {
        Box::pin(async {
            self.authorizations.fetch_add(1, Ordering::SeqCst);
            Ok(if self.deny {
                ToolDecision::Deny("parent policy denied".into())
            } else {
                ToolDecision::Allow
            })
        })
    }
}

#[tokio::test]
async fn child_inherits_authorization_without_reusing_parent_checkpoint_writer() {
    let server = Server::start(vec![
        Reply::sse(response("child1", vec![call("effect", "effect", "{}")])),
        Reply::sse(response("child2", vec![message("m", "denied")])),
    ])
    .await;
    let mut parent = agent(&server);
    let count = Arc::new(AtomicUsize::new(0));
    parent.register_tool(Effect(count.clone())).unwrap();
    let parent_hooks = Arc::new(Hooks {
        deny: true,
        ..Default::default()
    });
    let child_hooks = Arc::new(Hooks::default());
    let hooks = child_hooks.clone();
    parent.set_hooks(parent_hooks.clone());
    let agents = Subagents::with_config(SubagentConfig {
        hooks: Some(Arc::new(move |_| Ok(hooks.clone()))),
        ..Default::default()
    })
    .unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("policy", "policy"))
        .unwrap();
    settled(&agents, &child.id).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(parent_hooks.authorizations.load(Ordering::SeqCst), 1);
    assert!(parent_hooks.checkpoints.lock().unwrap().is_empty());
    assert!(!child_hooks.checkpoints.lock().unwrap().is_empty());
    assert!(
        server.captured()[1]
            .body
            .to_string()
            .contains("parent policy denied")
    );
    agents.shutdown().await;
}

#[tokio::test]
async fn child_model_persona_and_tool_allowlist_apply_before_first_request() {
    let server = Server::start(vec![Reply::sse(response(
        "child",
        vec![message("m", "done")],
    ))])
    .await;
    let mut parent = agent(&server);
    parent.register_tool(Effect(Arc::default())).unwrap();
    let agents = Subagents::with_config(SubagentConfig {
        model: Some(ModelOptions {
            model: "child-model".into(),
            ..Default::default()
        }),
        system_prompt: Some("child persona".into()),
        allowed_tools: Some(vec![]),
        ..Default::default()
    })
    .unwrap();
    agents.register(&mut parent).unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("limited", "limited"))
        .unwrap();
    settled(&agents, &child.id).await;
    let body = &server.captured()[0].body;
    assert_eq!(body["model"], "child-model");
    assert!(body["system"].to_string().contains("child persona"));
    assert!(body["tools"].is_null() || body["tools"].as_array().unwrap().is_empty());
    agents.shutdown().await;
}

#[tokio::test]
async fn interrupt_can_resume_without_replaying_uncertain_tools() {
    struct Block(Arc<tokio::sync::Notify>);
    impl Tool for Block {
        fn definition(&self) -> ToolDefinition {
            Effect(Arc::default()).definition()
        }
        fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
            Ok(())
        }
        fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
            Box::pin(async {
                self.0.notify_one();
                std::future::pending().await
            })
        }
    }
    let server = Server::start(vec![
        Reply::sse(response("child1", vec![call("uncertain", "effect", "{}")])),
        Reply::sse(response("child2", vec![message("m", "recovered")])),
    ])
    .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut parent = agent(&server);
    parent.register_tool(Block(entered.clone())).unwrap();
    let agents = Subagents::new();
    let child = agents
        .start(&parent, SubagentRequest::new("blocked", "blocked"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    agents.interrupt(&child.id).unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.status,
        SubagentStatus::NeedsResolution
    );
    let snapshot = agents.snapshot(&child.id).unwrap();
    assert_eq!(snapshot.pending[0].state, PendingState::Unknown);
    assert_eq!(
        agents.send_message(&child.id, "resume").unwrap_err().kind,
        ErrorKind::NeedsResolution
    );
    agents
        .resolve_tool(
            &child.id,
            "uncertain",
            ToolOutput::text("host verified effect"),
        )
        .unwrap();
    agents.send_message(&child.id, "resume").unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.result.unwrap().output,
        "recovered"
    );
    assert_eq!(server.captured().len(), 2);
    assert!(
        server.captured()[1]
            .body
            .to_string()
            .contains("host verified effect")
    );
    agents.shutdown().await;
}

#[tokio::test]
async fn dropping_last_manager_owner_cancels_running_children() {
    let mut reply = delayed("never");
    reply.header_delay = Duration::from_secs(30);
    let server = Server::start(vec![reply]).await;
    let agents = Subagents::new();
    let mut events = agents.subscribe();
    let child = agents
        .start(&agent(&server), SubagentRequest::new("child", "child"))
        .unwrap();
    requests(&server, 1).await;
    drop(agents);
    let info = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let SubagentEvent::Finished(info) = events.recv().await.unwrap() {
                break info;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(info.id, child.id);
    assert_eq!(
        info.result.unwrap().stop_reason,
        StopReason::Error(ErrorKind::Cancelled)
    );
}

#[tokio::test]
async fn incomplete_child_output_is_preserved_but_not_reported_as_success() {
    let mut incomplete = response("child", vec![message("m", "partial answer")]);
    incomplete["stop_reason"] = json!("max_tokens");
    let server = Server::start(vec![
        Reply::sse(response(
            "parent1",
            vec![call(
                "sub",
                "subagent",
                r#"{"description":"child","prompt":"child"}"#,
            )],
        )),
        Reply::sse(incomplete),
        Reply::sse(response("parent2", vec![message("m", "handled")])),
    ])
    .await;
    let mut parent = agent(&server);
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    parent
        .run("work", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert!(parent.snapshot().items.iter().any(|item| matches!(item, Item::FunctionCallOutput { is_error: true, output, .. } if output.contains("partial answer") && output.contains("Incomplete"))));
    agents.shutdown().await;
}

#[tokio::test]
async fn model_controls_cannot_reach_another_roots_children() {
    let target = Arc::new(Mutex::new(String::new()));
    let id = target.clone();
    let server = Server::start_with_handler(move |req| {
        let messages = req.body["messages"].to_string();
        if messages.contains("owned task") {
            return Reply::sse(response("owned", vec![message("m", "owned answer")]));
        }
        if messages.contains("tool_result") {
            return Reply::sse(response("outsider2", vec![message("m", "denied")]));
        }
        let id = id.lock().unwrap().clone();
        Reply::sse(response(
            "outsider1",
            vec![
                call("list", "list_agents", "{}"),
                call("wait", "wait_agent", &json!({"agent_id":id}).to_string()),
                call(
                    "send",
                    "send_message",
                    &json!({"agent_id":id,"message":"injected"}).to_string(),
                ),
                call(
                    "stop",
                    "interrupt_agent",
                    &json!({"agent_id":id}).to_string(),
                ),
            ],
        ))
    })
    .await;
    let agents = Subagents::new();
    let child = agents
        .start(&agent(&server), SubagentRequest::new("owned", "owned task"))
        .unwrap();
    settled(&agents, &child.id).await;
    *target.lock().unwrap() = child.id.clone();
    let mut outsider = agent(&server);
    agents.register(&mut outsider).unwrap();
    outsider
        .run("try controls", RunOptions::default(), ignore)
        .await
        .unwrap();
    let outputs: Vec<_> = outsider
        .snapshot()
        .items
        .into_iter()
        .filter_map(|item| match item {
            Item::FunctionCallOutput {
                call_id,
                output,
                is_error,
                ..
            } => Some((call_id, output, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(outputs[0], ("list".into(), "[]".into(), false));
    assert_eq!(outputs.len(), 4);
    assert!(
        outputs[1..]
            .iter()
            .all(|(_, text, error)| *error && text.contains("lineage"))
    );
    assert_eq!(agents.snapshot(&child.id).unwrap().run_sequence, 1);
    agents.shutdown().await;
}

#[tokio::test]
async fn model_can_collect_and_follow_up_its_own_background_child() {
    let id = Arc::new(Mutex::new(String::new()));
    let target = id.clone();
    let root_turn = Arc::new(AtomicUsize::new(0));
    let count = root_turn.clone();
    let server = Server::start_with_handler(move |req| {
        let text = req.body["messages"].to_string();
        if text.contains("child initial") {
            let answer = if text.contains("child followup") {
                "followup answer"
            } else {
                "first answer"
            };
            return delayed(answer);
        }
        let id = target.lock().unwrap().clone();
        match count.fetch_add(1, Ordering::SeqCst) {
            0 => Reply::sse(response(
                "root1",
                vec![
                    call("list", "list_agents", "{}"),
                    call("wait1", "wait_agent", &json!({"agent_id":id}).to_string()),
                    call(
                        "send",
                        "send_message",
                        &json!({"agent_id":id,"message":"child followup"}).to_string(),
                    ),
                    call("wait2", "wait_agent", &json!({"agent_id":id}).to_string()),
                ],
            )),
            _ => Reply::sse(response("root2", vec![message("m", "collected")])),
        }
    })
    .await;
    let agents = Subagents::new();
    let mut parent = agent(&server);
    agents.register(&mut parent).unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("child", "child initial"))
        .unwrap();
    *id.lock().unwrap() = child.id.clone();
    parent
        .run("manage child", RunOptions::default(), ignore)
        .await
        .unwrap();
    let snapshot = parent.snapshot();
    assert!(snapshot.items.iter().any(|item| matches!(item, Item::FunctionCallOutput { call_id, output, is_error: false, .. } if call_id == "wait2" && output.contains("followup answer"))));
    assert!(snapshot.items.iter().any(|item| matches!(item, Item::Message { role: MessageRole::User, content, .. } if content.iter().any(|part| part.text().contains("finished:")))));
    assert_eq!(agents.snapshot(&child.id).unwrap().run_sequence, 2);
    agents.shutdown().await;
}

#[tokio::test]
async fn child_cannot_edit_using_its_parents_file_observation() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "original").unwrap();
    let server = Server::start(vec![
        Reply::sse(response(
            "root1",
            vec![call("read", "read", r#"{"file_path":"note.txt"}"#)],
        )),
        Reply::sse(response(
            "root2",
            vec![call(
                "delegate",
                "subagent",
                r#"{"description":"edit","prompt":"edit file"}"#,
            )],
        )),
        Reply::sse(response(
            "child1",
            vec![call(
                "edit",
                "edit",
                r#"{"file_path":"note.txt","old_string":"original","new_string":"changed"}"#,
            )],
        )),
        Reply::sse(response("child2", vec![message("m", "need to read first")])),
        Reply::sse(response("root3", vec![message("m", "done")])),
    ])
    .await;
    let mut parent = agent(&server);
    let local = LocalTools::new(workspace.path()).unwrap();
    local.register(&mut parent).unwrap();
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    parent
        .run("read and delegate", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt")).unwrap(),
        "original"
    );
    let snapshot = agents.snapshot(&agents.list()[0].id).unwrap();
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| matches!(item, Item::FunctionCallOutput { is_error: true, .. }))
    );
    agents.shutdown().await;
    local.shutdown().await;
}

#[tokio::test]
async fn panicking_child_settles_waiters_and_retains_recoverable_snapshot() {
    struct Panic;
    impl Tool for Panic {
        fn definition(&self) -> ToolDefinition {
            Effect(Arc::default()).definition()
        }
        fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
            Ok(())
        }
        fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
            Box::pin(async { panic!("fixture panic") })
        }
    }
    let server = Server::start(vec![Reply::sse(response(
        "panic",
        vec![call("effect", "effect", "{}")],
    ))])
    .await;
    let mut parent = agent(&server);
    parent.register_tool(Panic).unwrap();
    let agents = Subagents::new();
    let child = agents
        .start(&parent, SubagentRequest::new("panic", "panic"))
        .unwrap();
    assert_eq!(
        settled(&agents, &child.id).await.status,
        SubagentStatus::Unavailable
    );
    let snapshot = agents.snapshot(&child.id).unwrap();
    assert_eq!(snapshot.pending[0].state, PendingState::Unknown);
    snapshot.validate().unwrap();
    assert!(agents.send_message(&child.id, "again").is_err());
    agents.shutdown().await;
}

#[test]
fn runtime_shutdown_settles_children_even_before_first_poll() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let agents = Subagents::new();
    let client = DeepSeekClient::new(ClientConfig::new("fixture-key")).unwrap();
    let parent = Agent::new(client, "test", ModelOptions::default()).unwrap();
    let child = {
        let _entered = runtime.enter();
        agents
            .start(&parent, SubagentRequest::new("unpolled", "unpolled"))
            .unwrap()
    };
    drop(runtime);
    assert_eq!(
        agents.get(&child.id).unwrap().status,
        SubagentStatus::Unavailable
    );
}

#[tokio::test]
async fn dropping_foreground_run_future_cancels_the_child() {
    let mut slow = delayed("late");
    slow.header_delay = Duration::from_secs(30);
    let server = Server::start(vec![
        Reply::sse(response(
            "parent",
            vec![call(
                "delegate",
                "subagent",
                r#"{"description":"slow","prompt":"slow"}"#,
            )],
        )),
        slow,
    ])
    .await;
    let mut parent = agent(&server);
    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    {
        let work = parent.run("delegate", RunOptions::default(), ignore);
        tokio::pin!(work);
        tokio::select! {
            _ = requests(&server, 2) => {}
            result = &mut work => panic!("parent unexpectedly settled: {result:?}"),
        }
    }
    assert_eq!(parent.snapshot().pending[0].state, PendingState::Unknown);
    let child = agents.list().pop().unwrap();
    assert_eq!(
        settled(&agents, &child.id)
            .await
            .result
            .unwrap()
            .stop_reason,
        StopReason::Error(ErrorKind::Cancelled)
    );
    agents.shutdown().await;
}

#[tokio::test]
async fn steering_never_splits_an_unfinished_tool_batch() {
    struct Gate(
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<AtomicUsize>,
    );
    impl Tool for Gate {
        fn definition(&self) -> ToolDefinition {
            Effect(Arc::default()).definition()
        }
        fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
            Ok(())
        }
        fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
            Box::pin(async {
                if self.2.fetch_add(1, Ordering::SeqCst) == 0 {
                    self.0.notify_one();
                    self.1.notified().await;
                }
                Ok(ToolOutput::text("done"))
            })
        }
    }
    let server = Server::start(vec![
        Reply::sse(response(
            "child1",
            vec![
                call("first", "effect", "{}"),
                call("second", "effect", "{}"),
            ],
        )),
        Reply::sse(response("child2", vec![message("m", "steered")])),
    ])
    .await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let mut parent = agent(&server);
    parent
        .register_tool(Gate(entered.clone(), release.clone(), count.clone()))
        .unwrap();
    let hooks = Arc::new(Hooks::default());
    let child_hooks = hooks.clone();
    let agents = Subagents::with_config(SubagentConfig {
        hooks: Some(Arc::new(move |_| Ok(child_hooks.clone()))),
        ..Default::default()
    })
    .unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("task", "task"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    agents.send_message(&child.id, "steer after batch").unwrap();
    release.notify_one();
    assert_eq!(
        settled(&agents, &child.id).await.result.unwrap().output,
        "steered"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let snapshot = agents.snapshot(&child.id).unwrap();
    snapshot.validate().unwrap();
    let steering = snapshot.items.iter().position(|item| matches!(item, Item::Message { role: MessageRole::User, content, .. } if content[0].text() == "steer after batch")).unwrap();
    let second_result = snapshot
        .items
        .iter()
        .position(
            |item| matches!(item, Item::FunctionCallOutput { call_id, .. } if call_id == "second"),
        )
        .unwrap();
    assert!(steering > second_result);
    {
        let checkpoints = hooks.checkpoints.lock().unwrap();
        assert_eq!(
            checkpoints
                .iter()
                .filter(|(kind, _)| *kind == CheckpointKind::RunStarted)
                .count(),
            1
        );
        assert!(
            checkpoints
                .iter()
                .any(|(kind, _)| *kind == CheckpointKind::MessagesReceived)
        );
    }
    agents.shutdown().await;
}

#[tokio::test]
async fn ancestor_can_interrupt_grandchild_without_interrupting_its_parent() {
    let target = Arc::new(Mutex::new(String::new()));
    let id = target.clone();
    let server = Server::start_with_handler(move |req| {
        let messages = &req.body["messages"];
        let first = messages[0]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        if first == "leaf task"
            || (first == "middle task" && messages.as_array().unwrap().len() > 1)
        {
            let mut slow = delayed("late");
            slow.header_delay = Duration::from_secs(30);
            return slow;
        }
        if first == "middle task" {
            return Reply::sse(response(
                "middle",
                vec![call(
                    "leaf",
                    "subagent",
                    r#"{"description":"leaf","prompt":"leaf task","run_in_background":true}"#,
                )],
            ));
        }
        if !messages.to_string().contains("tool_result") {
            return Reply::sse(response(
                "root1",
                vec![call(
                    "stop",
                    "interrupt_agent",
                    &json!({"agent_id":id.lock().unwrap().clone()}).to_string(),
                )],
            ));
        }
        Reply::sse(response("root2", vec![message("m", "interrupted")]))
    })
    .await;
    let agents = Subagents::new();
    let mut parent = agent(&server);
    agents.register(&mut parent).unwrap();
    let middle = agents
        .start(&parent, SubagentRequest::new("middle", "middle task"))
        .unwrap();
    requests(&server, 3).await;
    let leaf = agents
        .list()
        .into_iter()
        .find(|info| info.depth == 2)
        .unwrap();
    *target.lock().unwrap() = leaf.id.clone();
    parent
        .run("interrupt descendant", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(
        settled(&agents, &leaf.id).await.result.unwrap().stop_reason,
        StopReason::Error(ErrorKind::Cancelled)
    );
    assert_eq!(
        agents.get(&middle.id).unwrap().status,
        SubagentStatus::Running
    );
    assert!(agents.forget(&middle.id).is_err());
    agents.shutdown().await;
    assert!(agents.forget(&middle.id).is_err());
    agents.forget(&leaf.id).unwrap();
    agents.forget(&middle.id).unwrap();
}

#[tokio::test]
async fn child_request_budget_and_shutdown_are_enforced_without_stopping_parent() {
    let server = Server::start(vec![
        Reply::sse(response("child", vec![call("effect", "effect", "{}")])),
        delayed("parent still works"),
    ])
    .await;
    let mut parent = agent(&server);
    parent.register_tool(Effect(Arc::default())).unwrap();
    let agents = Subagents::with_config(SubagentConfig {
        run_options: RunOptions {
            max_requests: 1,
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let child = agents
        .start(&parent, SubagentRequest::new("budget", "budget"))
        .unwrap();
    assert_eq!(
        settled(&agents, &child.id)
            .await
            .result
            .unwrap()
            .stop_reason,
        StopReason::Error(ErrorKind::BudgetExceeded)
    );
    assert_eq!(agents.snapshot(&child.id).unwrap().requests.len(), 1);
    agents.shutdown().await;
    assert_eq!(
        parent
            .run("parent", RunOptions::default(), ignore)
            .await
            .unwrap()
            .response
            .output_text(),
        "parent still works"
    );
}

#[tokio::test]
async fn managers_cannot_mix_delegation_authority_and_unregistered_parents_get_no_notices() {
    let server = Server::start(vec![
        Reply::sse(response("child", vec![message("m", "child done")])),
        Reply::sse(response("parent", vec![message("m", "parent done")])),
    ])
    .await;
    let mut parent = agent(&server);
    let first = Subagents::new();
    let second = Subagents::new();
    first.register(&mut parent).unwrap();
    assert!(
        second
            .start(&parent, SubagentRequest::new("wrong", "wrong"))
            .is_err()
    );
    assert!(second.list().is_empty());
    assert!(server.captured().is_empty());
    // A host may delegate without granting model control tools; no unusable tool hint leaks in.
    let mut plain = agent(&server);
    let child = second
        .start(&plain, SubagentRequest::new("child", "child"))
        .unwrap();
    settled(&second, &child.id).await;
    plain
        .run("plain parent", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(
        server.captured()[1].body["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn child_hooks_forward_view_policy_and_tool_reminders_only_to_the_child() {
    struct ChildPolicy(AtomicUsize);
    impl AgentHooks for ChildPolicy {
        fn checkpoint<'a>(
            &'a self,
            _: CheckpointKind,
            _: SessionSnapshot,
        ) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn view_request<'a>(
            &'a self,
            estimate: &'a ViewEstimate,
        ) -> BoxFuture<'a, Result<Option<ViewRequest>>> {
            let action =
                (self.0.fetch_add(1, Ordering::SeqCst) == 0).then_some(ViewRequest::Condense {
                    start: 0,
                    end: estimate.items,
                    instruction: None,
                    max_tokens: Some(4321),
                });
            Box::pin(async move { Ok(action) })
        }
        fn tool_reminder<'a>(
            &'a self,
            _: &'a PendingCall,
            _: &'a ToolOutput,
        ) -> BoxFuture<'a, Result<Option<String>>> {
            Box::pin(async { Ok(Some("CHILD_ONLY_REMINDER".into())) })
        }
    }
    let server = Server::start(vec![
        Reply::sse(response("summary", vec![message("s", "child checkpoint")])),
        Reply::sse(response("call", vec![call("c", "effect", "{}")])),
        Reply::sse(response("done", vec![message("done", "child finished")])),
    ])
    .await;
    let mut parent = agent(&server);
    parent.register_tool(Effect(Arc::default())).unwrap();
    let agents = Subagents::with_config(SubagentConfig {
        hooks: Some(Arc::new(|_| Ok(Arc::new(ChildPolicy(AtomicUsize::new(0)))))),
        ..Default::default()
    })
    .unwrap();
    agents.register(&mut parent).unwrap();
    let child = agents
        .start(
            &parent,
            SubagentRequest::new("child", "long child task ".repeat(300)),
        )
        .unwrap();
    let result = settled(&agents, &child.id).await.result.unwrap();
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(server.captured().len(), 3);
    assert_eq!(server.captured()[0].body["max_tokens"], json!(4321));
    assert!(
        server.captured()[1]
            .body
            .to_string()
            .contains("child checkpoint")
    );
    assert!(
        server.captured()[2]
            .body
            .to_string()
            .contains("CHILD_ONLY_REMINDER")
    );
    assert!(parent.snapshot().items.is_empty());
    agents.shutdown().await;
}
