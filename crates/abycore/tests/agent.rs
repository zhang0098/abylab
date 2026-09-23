mod common;
use abycore::*;
use common::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Echo {
    calls: Arc<AtomicUsize>,
    delay: Duration,
    uncertain: bool,
}
impl Echo {
    fn new(calls: Arc<AtomicUsize>) -> Self {
        Self {
            calls,
            delay: Duration::ZERO,
            uncertain: false,
        }
    }
}
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echo a string".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }
    }
    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        if arguments.as_object().is_none_or(|o| o.len() != 1) || !arguments["text"].is_string() {
            return Err(ToolError::Failed("text is required".into()));
        }
        Ok(())
    }
    fn execute<'a>(&'a self, arguments: Value, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if self.uncertain {
                return Err(ToolError::Uncertain("possibly written".into()));
            }
            if arguments["text"] == "business-error" {
                return Err(ToolError::Failed("known failure".into()));
            }
            Ok(ToolOutput::text(arguments["text"].as_str().unwrap()))
        })
    }
}
/// The same tool with a budget of its own, the way `bash` reads `timeoutMs`:
/// its declaration, not the deployment's backstop, is what the call runs under.
/// `unbounded` swaps that for the other declaration a tool can make — none at
/// all, for work that is bounded by what it does (a delegation waiting on a
/// child turn).
struct DeclaredEcho {
    delay: Duration,
    budget: Duration,
    unbounded: bool,
}
impl Tool for DeclaredEcho {
    fn call_budget(&self, _: &Value) -> CallBudget {
        if self.unbounded {
            CallBudget::Unbounded
        } else {
            CallBudget::Own(self.budget)
        }
    }
    fn cleanup_grace(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn definition(&self) -> ToolDefinition {
        Echo::new(Arc::new(AtomicUsize::new(0))).definition()
    }
    fn validate(&self, arguments: &Value) -> std::result::Result<(), ToolError> {
        Echo::new(Arc::new(AtomicUsize::new(0))).validate(arguments)
    }
    fn execute<'a>(&'a self, arguments: Value, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(ToolOutput::text(arguments["text"].as_str().unwrap()))
        })
    }
}
fn agent(server: &Server) -> Agent {
    Agent::new(server.client(), "system", ModelOptions::default()).unwrap()
}
async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

#[tokio::test]
async fn multiple_tools_multiple_rounds_and_snapshot_replay() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![
                reasoning("think", "保留思考"),
                call("c1", "echo", r#"{"text":"one"}"#),
                call("c2", "echo", r#"{"text":"two"}"#),
            ],
        )),
        Reply::sse(response(
            "r2",
            vec![call("c3", "echo", r#"{"text":"three"}"#)],
        )),
        Reply::sse(response("r3", vec![message("m3", "done")])),
        Reply::sse(response("r4", vec![message("m4", "restored")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent.register_tool(Echo::new(calls.clone())).unwrap();
    let mut events = vec![];
    let outcome = agent
        .run("start", RunOptions::default(), |event| {
            events.push(event);
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(outcome.requests.len(), 3);
    assert!(
        outcome
            .requests
            .iter()
            .all(|r| r.usage.as_ref().unwrap().total_tokens == Some(14))
    );
    assert!(matches!(
        events.first(),
        Some(AgentEvent::RunStarted { run: 1 })
    ));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::RunFinished {
            reason: StopReason::Completed,
            ..
        })
    ));
    let snapshot = agent.snapshot();
    let json = snapshot.to_json().unwrap();
    assert!(!json.contains("fixture-secret"));
    assert_eq!(snapshot, SessionSnapshot::from_json(&json).unwrap());
    let mut restored =
        Agent::restore(server.client(), SessionSnapshot::from_json(&json).unwrap()).unwrap();
    assert_eq!(server.captured().len(), 3);
    restored
        .run("next", RunOptions::default(), ignore)
        .await
        .unwrap();
    let requests = server.captured();
    assert_eq!(
        requests[1].body["messages"][1]["content"][0]["thinking"],
        "保留思考"
    );
    assert_eq!(
        requests[1].body["messages"][1]["content"][0]["signature"],
        "sig-think"
    );
    assert_eq!(requests[1].body["messages"][1]["content"][1]["id"], "c1");
    assert_eq!(
        requests[1].body["messages"][1]["content"][1]["input"],
        json!({"text":"one"})
    );
    assert_eq!(
        requests[1].body["messages"][2]["content"][0]["content"][0]["text"],
        "one"
    );
    assert_eq!(
        requests[1].body["messages"][2]["content"][1]["content"][0]["text"],
        "two"
    );
    assert_eq!(
        requests[3].body["messages"][1],
        requests[1].body["messages"][1]
    );
    assert_eq!(
        requests[3].body["messages"][2],
        requests[1].body["messages"][2]
    );
    assert_eq!(
        requests[3].body["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"][0]["text"],
        "next"
    );
}

#[tokio::test]
async fn invalid_unknown_and_business_failure_are_recoverable_tool_results() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![
                call("c1", "echo", r#"{"text":123}"#),
                call("c2", "echo", "{}"),
                call("c3", "missing", "{}"),
                call("c4", "echo", r#"{"text":"business-error"}"#),
            ],
        )),
        Reply::sse(response("r2", vec![message("m", "repaired")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent.register_tool(Echo::new(calls.clone())).unwrap();
    assert!(agent.register_tool(Echo::new(calls.clone())).is_err());
    agent
        .run("start", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let requests = server.captured();
    let outputs: Vec<_> = requests[1].body["messages"][2]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["type"] == "tool_result")
        .collect();
    assert_eq!(outputs.len(), 4);
    assert!(outputs.iter().all(|v| {
        v["is_error"] == true
            && v["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Tool error:")
    }));
}

#[tokio::test]
async fn failed_incomplete_and_duplicate_calls_never_execute() {
    for status in ["incomplete", "failed", "completed"] {
        let mut wire = response("r", vec![call("c", "echo", r#"{"text":"x"}"#)]);
        if status == "incomplete" {
            wire["stop_reason"] = json!("max_tokens");
        }
        if status == "completed" {
            wire["content"]
                .as_array_mut()
                .unwrap()
                .push(call("c", "echo", "{}"));
        }
        let reply = if status == "failed" {
            Reply::events(vec![
                message_start("r"),
                json!({"type":"error","error":{"type":"api_error","message":"fixture-secret"}}),
            ])
        } else {
            Reply::sse(wire)
        };
        let server = Server::start(vec![reply]).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = agent(&server);
        agent.register_tool(Echo::new(calls.clone())).unwrap();
        let result = agent.run("start", RunOptions::default(), ignore).await;
        match status {
            "incomplete" => assert_eq!(result.unwrap().stop_reason, StopReason::Incomplete),
            "failed" => assert_eq!(result.unwrap_err().kind, ErrorKind::Server),
            _ => assert_eq!(result.unwrap_err().kind, ErrorKind::Protocol),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(agent.snapshot().items.len(), 1);
    }
}

#[tokio::test]
async fn completed_tool_survives_http_failure_and_resume_without_reexecution() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c", "echo", r#"{"text":"once"}"#)],
        )),
        Reply::raw(503, "text/plain", "unavailable"),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent.register_tool(Echo::new(calls.clone())).unwrap();
    assert_eq!(
        agent
            .run("start", RunOptions::default(), ignore)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Server
    );
    let snapshot = agent.snapshot();
    assert!(snapshot.pending.is_empty());
    assert!(snapshot.needs_response);
    assert_eq!(snapshot.requests.len(), 2);
    assert!(snapshot.requests[1].usage.is_none());
    let mut restored = Agent::restore(server.client(), snapshot).unwrap();
    restored.register_tool(Echo::new(calls.clone())).unwrap();
    assert!(
        restored
            .run("new", RunOptions::default(), ignore)
            .await
            .is_err()
    );
    restored
        .continue_run(RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        server.captured()[1].body["messages"],
        server.captured()[2].body["messages"]
    );
}

#[tokio::test]
async fn new_input_resumes_truncation_without_replaying_completed_or_incomplete_tools() {
    let mut truncated = response(
        "r2",
        vec![call("partial", "echo", r#"{"text":"must-not-run"}"#)],
    );
    truncated["stop_reason"] = json!("max_tokens");
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("done", "echo", r#"{"text":"once"}"#)],
        )),
        Reply::sse(truncated),
        Reply::sse(response("r3", vec![message("m3", "short answer")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent.register_tool(Echo::new(calls.clone())).unwrap();
    let outcome = agent
        .run("original task", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Incomplete);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let saved = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    let mut restored = Agent::restore(server.client(), saved).unwrap();
    restored.register_tool(Echo::new(calls.clone())).unwrap();
    let outcome = restored
        .continue_run_with_input("stop and answer briefly", RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.new_items[0], Item::user("stop and answer briefly"));
    let captured = server.captured();
    assert_eq!(captured.len(), 3);
    let messages = captured[2].body["messages"].to_string();
    assert!(messages.contains("original task"));
    assert!(messages.contains("once"));
    assert!(messages.contains("stop and answer briefly"));
    assert!(!messages.contains("must-not-run"));
    restored.snapshot().validate().unwrap();
}

#[tokio::test]
async fn continuation_input_does_not_change_pending_idle_or_cancelled_sessions() {
    let server = Server::start(vec![]).await;
    for kind in [
        ErrorKind::NeedsResolution,
        ErrorKind::Session,
        ErrorKind::Cancelled,
    ] {
        let mut snapshot = SessionSnapshot::new("system", ModelOptions::default());
        let options = RunOptions::default();
        if kind != ErrorKind::Session {
            snapshot.items.push(Item::user("original task"));
            snapshot.needs_response = true;
        }
        if kind == ErrorKind::NeedsResolution {
            snapshot.items.push(Item::FunctionCall {
                id: "item".into(),
                call_id: "pending".into(),
                name: "echo".into(),
                arguments: "{}".into(),
            });
            snapshot.pending.push(PendingCall {
                call_id: "pending".into(),
                name: "echo".into(),
                arguments: "{}".into(),
                state: PendingState::Unknown,
            });
        }
        if kind == ErrorKind::Cancelled {
            options.cancellation.cancel();
        }
        let mut agent = Agent::restore(server.client(), snapshot.clone()).unwrap();
        let error = agent
            .continue_run_with_input("new input", options, ignore)
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(agent.snapshot(), snapshot);
    }
    assert!(server.captured().is_empty());
}

#[tokio::test]
async fn a_dropped_tool_call_requires_explicit_resolution() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c", "echo", r#"{"text":"once"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent
        .register_tool(Echo {
            calls: calls.clone(),
            delay: Duration::from_secs(5),
            uncertain: false,
        })
        .unwrap();
    let options = RunOptions {
        tool_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    // Dropping the run drops the call while it is executing: nothing can say
    // whether its side effects happened, so only the host may settle it.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            agent.run("start", options, ignore)
        )
        .await
        .is_err()
    );
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.pending[0].state, PendingState::Unknown);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut restored = Agent::restore(
        server.client(),
        SessionSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        restored
            .continue_run(RunOptions::default(), ignore)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::NeedsResolution
    );
    assert_eq!(server.captured().len(), 1);
    restored
        .resolve_tool("c", ToolOutput::text("host verified result"))
        .unwrap();
    restored
        .continue_run(RunOptions::default(), ignore)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_tool_that_declares_its_own_budget_is_not_clipped_by_the_backstop() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c", "echo", r#"{"text":"slow but allowed"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent
        .register_tool(DeclaredEcho {
            delay: Duration::from_millis(300),
            budget: Duration::from_secs(5),
            unbounded: false,
        })
        .unwrap();
    // The backstop is ten times shorter than the call: a tool that knows how long
    // its work takes owns that number, exactly as the harness reads each tool's
    // own limit instead of capping every call at one host-wide value.
    let options = RunOptions {
        tool_timeout: Duration::from_millis(30),
        ..Default::default()
    };
    let outcome = agent.run("start", options, ignore).await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let output = outcome
        .new_items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("the call is answered");
    assert_eq!(output, "slow but allowed");
}

/// A call with no budget of its own is not silently given the backstop: the work
/// is what bounds it, so a wait that outlives the backstop still answers.
#[tokio::test]
async fn a_tool_with_no_budget_is_not_clipped_by_the_backstop() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c", "echo", r#"{"text":"waiting is the work"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent
        .register_tool(DeclaredEcho {
            delay: Duration::from_millis(300),
            budget: Duration::from_secs(5),
            unbounded: true,
        })
        .unwrap();
    let options = RunOptions {
        tool_timeout: Duration::from_millis(30),
        ..Default::default()
    };
    let outcome = agent.run("start", options, ignore).await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let output = outcome
        .new_items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("the call is answered");
    assert_eq!(output, "waiting is the work");
}

#[tokio::test]
async fn a_call_that_outlives_its_budget_becomes_a_tool_result_and_the_turn_goes_on() {
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c", "echo", r#"{"text":"once"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m", "done")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(&server);
    agent
        .register_tool(Echo {
            calls: calls.clone(),
            delay: Duration::from_secs(5),
            uncertain: false,
        })
        .unwrap();
    let options = RunOptions {
        tool_timeout: Duration::from_millis(40),
        ..Default::default()
    };
    // The call is stopped at its budget and the model is told, the way
    // deepseek-harness's tool-call timeout answers with an error result: one
    // slow call does not fail the turn.
    let outcome = agent.run("start", options, ignore).await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let timed_out = outcome
        .new_items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput {
                output, is_error, ..
            } => Some((output, is_error)),
            _ => None,
        })
        .expect("the timed-out call is answered");
    assert!(timed_out.1, "the result is an error the model can read");
    assert!(timed_out.0.contains("timed out"), "{}", timed_out.0);
    assert!(timed_out.0.contains("unverified"), "{}", timed_out.0);
    assert_eq!(
        server.captured().len(),
        2,
        "the turn continued to the model"
    );
    let snapshot = agent.snapshot();
    assert!(snapshot.pending.is_empty());
    assert!(!snapshot.needs_response);
}

#[tokio::test]
async fn callback_failure_and_cancellation_keep_committed_outputs() {
    for cancel in [false, true] {
        let server = Server::start(vec![
            Reply::sse(response(
                "r1",
                vec![call("c", "echo", r#"{"text":"once"}"#)],
            )),
            Reply::sse(response("r2", vec![message("m", "done")])),
        ])
        .await;
        let mut agent = agent(&server);
        let calls = Arc::new(AtomicUsize::new(0));
        agent.register_tool(Echo::new(calls.clone())).unwrap();
        let options = RunOptions::default();
        let token = options.cancellation.clone();
        let error = agent
            .run("start", options, |event| {
                let token = token.clone();
                async move {
                    if matches!(event, AgentEvent::ToolFinished { .. }) {
                        if cancel {
                            token.cancel();
                        } else {
                            return Err(Error::new(ErrorKind::EventHandler, "host failed"));
                        }
                    }
                    Ok(())
                }
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            if cancel {
                ErrorKind::Cancelled
            } else {
                ErrorKind::EventHandler
            }
        );
        assert!(agent.snapshot().pending.is_empty());
        agent
            .continue_run(RunOptions::default(), ignore)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn request_tool_input_and_output_budgets() {
    let server = Server::start(vec![Reply::sse(response(
        "r1",
        vec![
            call("c1", "echo", &json!({"text":"中".repeat(80)}).to_string()),
            call("c2", "echo", r#"{"text":"two"}"#),
        ],
    ))])
    .await;
    let mut agent = agent(&server);
    let calls = Arc::new(AtomicUsize::new(0));
    agent.register_tool(Echo::new(calls.clone())).unwrap();
    let options = RunOptions {
        max_tool_calls: 1,
        max_tool_output_bytes: 64,
        ..Default::default()
    };
    assert_eq!(
        agent.run("start", options, ignore).await.unwrap_err().kind,
        ErrorKind::BudgetExceeded
    );
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.pending[0].state, PendingState::Ready);
    let Item::FunctionCallOutput { output, .. } = snapshot.items.last().unwrap() else {
        panic!("missing output")
    };
    assert!(output.len() <= 64);
    assert!(output.ends_with("[tool output truncated]"));
    let server = Server::start(vec![Reply::raw(503, "text/plain", "retry")]).await;
    let mut config = server.config();
    config.retry.max_retries = 2;
    let mut agent = Agent::new(
        DeepSeekClient::new(config).unwrap(),
        "",
        ModelOptions::default(),
    )
    .unwrap();
    assert_eq!(
        agent
            .run(
                "start",
                RunOptions {
                    max_requests: 1,
                    ..Default::default()
                },
                ignore
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::BudgetExceeded
    );
    assert_eq!(server.captured().len(), 1);
    let server = Server::start(vec![]).await;
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    assert_eq!(
        agent
            .run(
                "start",
                RunOptions {
                    max_input_bytes: 1,
                    ..Default::default()
                },
                ignore
            )
            .await
            .unwrap_err()
            .kind,
        ErrorKind::ContextLimitExceeded
    );
    assert!(server.captured().is_empty());
}

#[test]
fn snapshot_rejects_versions_protocols_and_broken_pairings() {
    let original = SessionSnapshot::new("system", ModelOptions::default());
    let mut snapshot = original.clone();
    snapshot.version = 3;
    assert!(snapshot.to_json().is_err());
    snapshot = original.clone();
    snapshot.protocol = "messages".into();
    assert!(snapshot.to_json().is_err());
    snapshot = original.clone();
    snapshot.items.push(Item::FunctionCallOutput {
        call_id: "orphan".into(),
        output: "x".into(),
        is_error: false,
        meta: None,
    });
    assert!(snapshot.to_json().is_err());
    snapshot = original;
    snapshot.items.push(Item::FunctionCall {
        id: "local-c".into(),
        call_id: "c".into(),
        name: "echo".into(),
        arguments: "{}".into(),
    });
    assert!(snapshot.to_json().is_err());
}

#[tokio::test]
async fn cancellation_during_callback_keeps_response_identity_and_unknown_usage() {
    let server = Server::start(vec![Reply::sse(response(
        "interrupted",
        vec![message("m", "answer")],
    ))])
    .await;
    let mut agent = agent(&server);
    let options = RunOptions::default();
    let cancellation = options.cancellation.clone();
    let error = agent
        .run("start", options, |event| {
            let cancellation = cancellation.clone();
            async move {
                if matches!(event, AgentEvent::Model(StreamEvent::Started { .. })) {
                    cancellation.cancel();
                    std::future::pending::<()>().await;
                }
                Ok(())
            }
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.requests.len(), 1);
    assert_eq!(
        snapshot.requests[0].response_id.as_deref(),
        Some("interrupted")
    );
    assert!(snapshot.requests[0].usage.is_none());
    assert!(snapshot.needs_response);
    assert_eq!(snapshot.items.len(), 1);
    snapshot.validate().unwrap();
}
