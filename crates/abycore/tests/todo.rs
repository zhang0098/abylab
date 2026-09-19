mod common;

use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn agent(server: &Server, parallel: bool) -> Agent {
    let mut agent = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    agent.register_tool(TodoWriteTool::new(parallel)).unwrap();
    agent
}

fn write(id: &str, todos: Value) -> Value {
    call(id, "todo_write", &json!({"todos":todos}).to_string())
}

fn item(content: &str, status: &str) -> Value {
    json!({"content":content,"status":status})
}

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

#[tokio::test]
async fn model_updates_whole_list_and_emits_committed_progress_views() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![write(
                "c1",
                json!([
                    item("  inspect  ", "in_progress"),
                    item("implement", "pending"),
                    item("test", "pending"),
                ]),
            )],
        )),
        Reply::sse(response(
            "r2",
            vec![write(
                "c2",
                json!([
                    item("inspect", "completed"),
                    item("implement", "in_progress"),
                    item("test", "in_progress"),
                ]),
            )],
        )),
        Reply::sse(response(
            "r3",
            vec![write("c3", json!([item("replacement", "completed")]))],
        )),
        Reply::sse(response("r4", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = agent(&server, true);
    let mut views = vec![];
    let mut outputs = vec![];
    agent
        .run("work", RunOptions::default(), |event| {
            match event {
                AgentEvent::PlanChanged { plan } => views.push(plan),
                AgentEvent::ToolFinished { output, .. } => outputs.push(output),
                _ => {}
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(views.len(), 4);
    assert_eq!(views[0], None);
    let first = views[1].as_ref().unwrap();
    assert_eq!(first.todos[0].content, "inspect");
    assert_eq!(
        first.counts,
        TodoCounts {
            pending: 2,
            in_progress: 1,
            completed: 0
        }
    );
    let parallel = views[2].as_ref().unwrap();
    assert_eq!(parallel.active_content.as_deref(), Some("implement"));
    assert_eq!(parallel.active_extra, 1);
    assert_eq!(parallel.total, 3);
    assert_eq!(parallel.counts.completed, 1);
    assert_eq!(
        outputs[1].content,
        "Updated todo list: 0 pending, 2 in progress, 1 completed."
    );
    assert_eq!(
        outputs[1].details.as_ref().unwrap()["counts"]["inProgress"],
        2
    );
    assert_eq!(PlanView::from_tool_output(&outputs[1]).unwrap(), views[2]);
    let final_view = agent.plan_view().unwrap();
    assert_eq!(final_view.todos.len(), 1);
    assert_eq!(final_view.todos[0].content, "replacement");
    assert_eq!(final_view.active_content, None);
    assert_eq!(final_view.active_extra, 0);
    assert_eq!(views.last().unwrap().as_ref(), Some(&final_view));
    // The projection adds no synthetic user messages or model-facing metadata.
    assert_eq!(
        agent
            .snapshot()
            .items
            .iter()
            .filter(|item| matches!(
                item,
                Item::Message {
                    role: MessageRole::User,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        !server.captured()[3]
            .body
            .to_string()
            .contains("activeExtra")
    );
    assert!(!server.captured()[3].body.to_string().contains("\"counts\""));
}

#[tokio::test]
async fn invalid_updates_preserve_the_current_plan_and_report_tool_errors() {
    let invalid = [
        json!({"todos":[item("", "pending")]}),
        json!({"todos":[item(" \n\t", "pending")]}),
        json!({"todos":[item("same", "pending"), item(" same ", "completed")]}),
        json!({"todos":[item("bad", "doing")]}),
        json!({"todos":[{"content":"bad","status":"pending","children":[]}]}),
        json!({"todos":[{"content":"bad","status":"pending","id":"id"}]}),
        json!({"todos":[{"content":"bad"}]}),
        json!({"todos":[{"content":null,"status":"pending"}]}),
        json!({"todos":"bad"}),
        json!({"todos":[],"append":true}),
        json!({"todos":[item("one", "in_progress"), item("two", "in_progress")]}),
    ];
    let mut calls = vec![write("valid", json!([item("keep", "in_progress")]))];
    calls.extend(
        invalid
            .iter()
            .enumerate()
            .map(|(i, args)| call(&format!("bad{i}"), "todo_write", &args.to_string())),
    );
    let server = Server::start(vec![
        Reply::sse(response("r1", calls)),
        Reply::sse(response("r2", vec![message("m", "handled")])),
    ])
    .await;
    let mut agent = agent(&server, false);
    let mut events = vec![];
    agent
        .run("work", RunOptions::default(), |event| {
            events.push(event);
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(agent.plan_view().unwrap().todos[0].content, "keep");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::PlanChanged { plan: Some(_) }))
            .count(),
        1
    );
    let errors = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { output, .. } if output.is_error => Some(output),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), invalid.len());
    assert!(
        errors
            .iter()
            .all(|output| PlanView::from_tool_output(output).unwrap().is_none())
    );
}

#[tokio::test]
async fn empty_list_clears_and_all_pending_is_accepted_like_harness() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![
                write("c1", json!([item("later", "pending")])),
                write("c2", json!([])),
            ],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = agent(&server, false);
    let mut plans = vec![];
    agent
        .run("work", RunOptions::default(), |event| {
            if let AgentEvent::PlanChanged { plan } = event {
                plans.push(plan);
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(plans[1].as_ref().unwrap().counts.pending, 1);
    let cleared = agent.plan_view().unwrap();
    assert!(cleared.todos.is_empty());
    assert_eq!(cleared.total, 0);
    assert_eq!(cleared.counts, TodoCounts::default());
    assert_eq!(agent.snapshot().todos, Some(vec![]));
}

#[tokio::test]
async fn json_and_jsonl_restore_plan_but_a_fresh_turn_clears_it() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![write("c1", json!([item("done", "completed")]))],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
        Reply::sse(response("r3", vec![message("m", "new answer")])),
    ])
    .await;
    let mut original = agent(&server, true);
    original
        .run("first turn", RunOptions::default(), ignore)
        .await
        .unwrap();
    let saved = original.snapshot();
    let restored = SessionSnapshot::from_json(&saved.to_json().unwrap()).unwrap();
    assert_eq!(saved, restored);
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::new(directory.path()).unwrap();
    let mut writer = store.create("todo-session", &saved).unwrap();
    store
        .append_checkpoint(&mut writer, saved.run_sequence, &saved)
        .unwrap();
    drop(writer);
    let (_, loaded) = store.load("todo-session").unwrap();
    assert_eq!(loaded.plan_view(), original.plan_view());
    // Read-only hosts can display the view even before tools are registered again.
    let mut restored = Agent::restore(server.client(), loaded).unwrap();
    assert_eq!(restored.plan_view(), original.plan_view());
    let mut views = vec![];
    restored
        .run("second turn", RunOptions::default(), |event| {
            if let AgentEvent::PlanChanged { plan } = event {
                views.push(plan);
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(views, vec![None]);
    assert_eq!(restored.snapshot().todos, None);
    assert!(restored.plan_view().is_none());
    // Historical successful update metadata is retained, not rewritten by the new turn.
    assert!(restored.snapshot().items.iter().any(|item| matches!(item, Item::FunctionCallOutput { meta: Some(meta), .. } if meta.get("todo_write").is_some())));
}

#[tokio::test]
async fn continue_run_preserves_plan_without_replaying_a_tool_or_rechecking_old_parallel_policy() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![write(
                "c1",
                json!([item("a", "in_progress"), item("b", "in_progress")]),
            )],
        )),
        Reply::raw(503, "text/plain", "fixture failure"),
        Reply::sse(response("r2", vec![message("m", "resumed")])),
    ])
    .await;
    let mut original = agent(&server, true);
    assert_eq!(
        original
            .run("work", RunOptions::default(), ignore)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Server
    );
    let saved = original.snapshot();
    let mut restored = Agent::restore(
        server.client(),
        SessionSnapshot::from_json(&saved.to_json().unwrap()).unwrap(),
    )
    .unwrap();
    restored.register_tool(TodoWriteTool::new(false)).unwrap();
    let mut events = vec![];
    restored
        .continue_run(RunOptions::default(), |event| {
            events.push(event);
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(restored.plan_view().unwrap().counts.in_progress, 2);
    assert!(events.iter().any(|event| matches!(event, AgentEvent::PlanChanged { plan: Some(plan) } if plan.counts.in_progress == 2)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolStarted { .. }))
    );
    assert_eq!(server.captured().len(), 3);
}

#[test]
fn legacy_snapshots_load_and_malformed_current_lists_are_rejected() {
    let mut value =
        serde_json::to_value(SessionSnapshot::new("", ModelOptions::default())).unwrap();
    assert!(value.get("todos").is_none());
    assert!(
        SessionSnapshot::from_json(&value.to_string())
            .unwrap()
            .plan_view()
            .is_none()
    );
    for todos in [
        json!([item("   ", "pending")]),
        json!([item(" padded ", "pending")]),
        json!([item("duplicate", "pending"), item("duplicate", "completed")]),
        json!([item("bad", "doing")]),
        json!([{"content":"bad","status":"pending","children":[]}]),
        json!("bad"),
    ] {
        value["todos"] = todos;
        assert!(SessionSnapshot::from_json(&value.to_string()).is_err());
    }
    value["todos"] = json!([item("合法步骤", "completed")]);
    assert_eq!(
        SessionSnapshot::from_json(&value.to_string())
            .unwrap()
            .plan_view()
            .unwrap()
            .counts
            .completed,
        1
    );
}

#[derive(Default)]
struct Hooks {
    deny: bool,
    fail_intent: bool,
    fail_result: bool,
    saved: Mutex<Vec<SessionSnapshot>>,
}
impl AgentHooks for Hooks {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if (self.fail_intent && matches!(kind, CheckpointKind::ToolIntent { .. }))
                || (self.fail_result && matches!(kind, CheckpointKind::ToolResult { .. }))
            {
                return Err(Error::new(ErrorKind::Session, "fixture storage failure"));
            }
            snapshot.validate()?;
            self.saved.lock().unwrap().push(snapshot);
            Ok(())
        })
    }
    fn authorize<'a>(&'a self, _: PendingCall) -> BoxFuture<'a, Result<ToolDecision>> {
        Box::pin(async move {
            Ok(if self.deny {
                ToolDecision::Deny("denied".into())
            } else {
                ToolDecision::Allow
            })
        })
    }
}

#[tokio::test]
async fn denied_or_uncommitted_updates_are_never_published_as_progress() {
    for scenario in ["denied", "intent", "result"] {
        let server = Server::start(vec![
            Reply::sse(response(
                "r1",
                vec![write("c1", json!([item("step", "in_progress")]))],
            )),
            Reply::sse(response("r2", vec![message("m", "handled")])),
        ])
        .await;
        let hooks = Arc::new(Hooks {
            deny: scenario == "denied",
            fail_intent: scenario == "intent",
            fail_result: scenario == "result",
            ..Default::default()
        });
        let mut agent = agent(&server, true);
        agent.set_hooks(hooks.clone());
        let mut views = vec![];
        let result = agent
            .run("work", RunOptions::default(), |event| {
                if let AgentEvent::PlanChanged { plan: Some(plan) } = event {
                    views.push(plan);
                }
                async { Ok(()) }
            })
            .await;
        assert!(views.is_empty());
        assert_eq!(result.is_ok(), scenario == "denied");
        assert!(
            hooks
                .saved
                .lock()
                .unwrap()
                .iter()
                .all(|snapshot| snapshot.todos.is_none())
        );
        if scenario == "result" {
            // As with any checkpoint failure, host may save final in-memory state explicitly.
            assert!(agent.plan_view().is_some());
            let durable = hooks.saved.lock().unwrap().last().unwrap().clone();
            assert!(durable.todos.is_none());
            assert_eq!(durable.pending[0].state, PendingState::Unknown);
        } else {
            assert!(agent.plan_view().is_none());
        }
    }
}

#[tokio::test]
async fn complete_structured_result_budget_rejects_oversize_without_truncating_the_plan() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![
                write("c1", json!([item("keep", "in_progress")])),
                write("c2", json!([item(&"界".repeat(500), "completed")])),
            ],
        )),
        Reply::sse(response("r2", vec![message("m", "handled")])),
    ])
    .await;
    let mut agent = agent(&server, true);
    let mut results = vec![];
    agent
        .run(
            "work",
            RunOptions {
                max_tool_output_bytes: 512,
                ..Default::default()
            },
            |event| {
                if let AgentEvent::ToolFinished { output, .. } = event {
                    results.push(output);
                }
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
    assert!(!results[0].is_error);
    assert!(serde_json::to_vec(&results[0]).unwrap().len() <= 512);
    assert!(results[1].is_error);
    assert!(results[1].content.contains("output budget"));
    assert!(results[1].meta.is_none());
    assert_eq!(agent.plan_view().unwrap().todos[0].content, "keep");
}

#[tokio::test]
async fn shared_tool_and_delegated_agents_own_separate_lists() {
    let tool: Arc<dyn Tool> = Arc::new(TodoWriteTool::new(true));
    let server = Server::start(vec![
        Reply::sse(response(
            "root1",
            vec![write(
                "root-plan",
                json!([item("parent work", "in_progress")]),
            )],
        )),
        Reply::sse(response("root2", vec![message("m", "root done")])),
        Reply::sse(response(
            "child1",
            vec![write(
                "child-plan",
                json!([item("child work", "completed")]),
            )],
        )),
        Reply::sse(response("child2", vec![message("m", "child done")])),
    ])
    .await;
    let mut parent = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    parent.register_shared_tool(tool.clone()).unwrap();
    parent
        .run("parent", RunOptions::default(), ignore)
        .await
        .unwrap();
    let original = parent.plan_view().unwrap();
    let agents = Subagents::new();
    let mut child_events = agents.subscribe();
    let child = agents
        .start(
            &parent,
            SubagentRequest {
                description: "fork".into(),
                prompt: "child".into(),
                mode: SubagentMode::Fork,
            },
        )
        .unwrap();
    agents.wait(&child.id).await.unwrap();
    let child_view = agents.snapshot(&child.id).unwrap().plan_view().unwrap();
    assert_eq!(child_view.todos[0].content, "child work");
    assert_eq!(parent.plan_view(), Some(original));
    let mut child_views = vec![];
    while let Ok(event) = child_events.try_recv() {
        if let SubagentEvent::Agent {
            agent_id,
            event: AgentEvent::PlanChanged { plan },
        } = event
        {
            assert_eq!(agent_id, child.id);
            child_views.push(plan);
        }
    }
    // Even a fork that carries old todo outputs in its history starts with no current list.
    assert_eq!(child_views.first(), Some(&None));
    assert_eq!(child_views.last().unwrap().as_ref(), Some(&child_view));
    let mut other = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    other.register_shared_tool(tool).unwrap();
    assert!(other.plan_view().is_none());
    agents.shutdown().await;
}

#[tokio::test]
async fn manually_resolved_update_is_atomic_and_republished_on_resume() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![write("c1", json!([item("step", "in_progress")]))],
        )),
        Reply::sse(response("r2", vec![message("m", "resumed")])),
    ])
    .await;
    let mut agent = agent(&server, false);
    agent.set_hooks(Arc::new(Hooks {
        fail_intent: true,
        ..Default::default()
    }));
    assert!(
        agent
            .run("work", RunOptions::default(), ignore)
            .await
            .is_err()
    );
    let before = agent.snapshot();
    assert_eq!(before.pending.len(), 1);
    let mut output = ToolOutput::text("host verified result");
    output.meta = Some(json!({"todo_write":{"todos":[item(" padded ", "completed")]}}));
    assert!(agent.resolve_tool("c1", output.clone()).is_err());
    assert_eq!(agent.snapshot(), before);
    output.meta = Some(json!({"todo_write":{"todos":[item("step", "completed")]}}));
    assert!(agent.resolve_tool("wrong-id", output.clone()).is_err());
    assert_eq!(agent.snapshot(), before);
    agent.resolve_tool("c1", output).unwrap();
    assert_eq!(agent.plan_view().unwrap().counts.completed, 1);
    agent.snapshot().validate().unwrap();
    let mut views = vec![];
    agent
        .continue_run(RunOptions::default(), |event| {
            if let AgentEvent::PlanChanged { plan } = event {
                views.push(plan);
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(views, vec![agent.plan_view()]);
}

#[tokio::test]
async fn incomplete_model_response_never_applies_proposed_tasks() {
    let mut wire = response(
        "r1",
        vec![write("c1", json!([item("unfinished", "in_progress")]))],
    );
    wire["stop_reason"] = json!("max_tokens");
    let server = Server::start(vec![Reply::sse(wire)]).await;
    let mut agent = agent(&server, false);
    let mut views = vec![];
    let result = agent
        .run("work", RunOptions::default(), |event| {
            if let AgentEvent::PlanChanged { plan } = event {
                views.push(plan);
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Incomplete);
    assert_eq!(views, vec![None]);
    assert!(agent.plan_view().is_none());
}

#[test]
fn view_uses_successful_canonical_metadata_and_ignores_truncated_text() {
    let mut output = ToolOutput::text("[tool output truncated]");
    output.truncated = true;
    output.meta = Some(
        json!({"todo_write":{"todos":[item("first", "in_progress"),item("second", "in_progress")]}}),
    );
    let view = PlanView::from_tool_output(&output).unwrap().unwrap();
    assert_eq!(view.active_content.as_deref(), Some("first"));
    assert_eq!(view.active_extra, 1);
    output.is_error = true;
    assert!(PlanView::from_tool_output(&output).unwrap().is_none());
    output.is_error = false;
    output.meta = Some(json!({"todo_write":{"todos":[item(" padded ", "pending")]}}));
    assert!(PlanView::from_tool_output(&output).is_err());
    output.meta = None;
    assert!(PlanView::from_tool_output(&output).unwrap().is_none());
}
