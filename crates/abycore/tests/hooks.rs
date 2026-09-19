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

struct Hooks {
    states: Mutex<Vec<(CheckpointKind, SessionSnapshot)>>,
    fail_intent: bool,
    fail_start: bool,
    fail_result: bool,
    deny: bool,
    wait: Duration,
    entered: tokio::sync::Notify,
    /// Advisory text handed back after every tool result.
    reminder: Option<String>,
}
impl Default for Hooks {
    fn default() -> Self {
        Self {
            states: Mutex::new(vec![]),
            fail_intent: false,
            fail_start: false,
            fail_result: false,
            deny: false,
            wait: Duration::ZERO,
            entered: tokio::sync::Notify::new(),
            reminder: None,
        }
    }
}
impl AgentHooks for Hooks {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            snapshot.validate()?;
            if (self.fail_intent && matches!(kind, CheckpointKind::ToolIntent { .. }))
                || (self.fail_start && kind == CheckpointKind::RunStarted)
                || (self.fail_result && matches!(kind, CheckpointKind::ToolResult { .. }))
            {
                return Err(Error::new(ErrorKind::EventHandler, "disk failure"));
            }
            self.states.lock().unwrap().push((kind, snapshot));
            Ok(())
        })
    }
    fn authorize<'a>(&'a self, _: PendingCall) -> BoxFuture<'a, Result<ToolDecision>> {
        Box::pin(async move {
            self.entered.notify_one();
            tokio::time::sleep(self.wait).await;
            Ok(if self.deny {
                ToolDecision::Deny("denied".into())
            } else {
                ToolDecision::Allow
            })
        })
    }
    fn tool_reminder<'a>(
        &'a self,
        _: &'a PendingCall,
        _: &'a ToolOutput,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move { Ok(self.reminder.clone()) })
    }
}

/// The advisory hook is a queue, not an interrupt: the next request carries the
/// text right after the tool result it comments on, and nothing else changes.
#[tokio::test]
async fn tool_reminder_reaches_the_next_request_after_the_result() {
    let server = Server::start(vec![
        Reply::sse(response("r1", vec![call("c1", "effect", "{}")])),
        Reply::sse(response("r2", vec![call("c2", "effect", "{}")])),
        Reply::sse(response("r3", vec![message("m3", "done")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(Hooks {
        reminder: Some("REMINDER: the same call already ran — change approach.".into()),
        ..Default::default()
    });
    let mut agent = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    agent
        .register_tool(Effect(calls.clone(), hooks.clone()))
        .unwrap();
    agent.set_hooks(hooks);
    agent
        .run("go", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();

    let captured = server.captured();
    assert_eq!(captured.len(), 3);
    let body = |index: usize| captured[index].body.to_string();
    assert!(
        !body(0).contains("REMINDER"),
        "nothing is injected before the first result"
    );
    assert!(
        body(1).contains("REMINDER"),
        "the reminder rides the request after the result it comments on"
    );
    assert!(
        body(2).contains("REMINDER"),
        "and stays in history like any other message"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        agent.snapshot().items.iter().any(
            |item| matches!(item, Item::Message { role: MessageRole::User, content, .. }
                if content.iter().any(|part| part.text().contains("REMINDER")))
        ),
        "the queue becomes an ordinary user item"
    );
}

#[tokio::test]
async fn result_checkpoint_failure_leaves_durable_intent_requiring_resolution() {
    let hooks = Arc::new(Hooks {
        fail_result: true,
        ..Default::default()
    });
    let (result, calls, current, requests) = run(hooks.clone()).await;
    assert!(result.is_err());
    assert_eq!(calls, 1);
    assert_eq!(requests, 1);
    assert!(current.pending.is_empty());
    let durable = hooks.states.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(durable.pending[0].state, PendingState::Unknown);
    let server = Server::start(vec![]).await;
    let mut restored = Agent::restore(server.client(), durable).unwrap();
    assert_eq!(
        restored
            .continue_run(RunOptions::default(), |_| async { Ok(()) })
            .await
            .unwrap_err()
            .kind,
        ErrorKind::NeedsResolution
    );
    assert!(server.captured().is_empty());
}
#[tokio::test]
async fn continuation_input_passes_the_durable_checkpoint_before_dispatch() {
    for fail_start in [false, true] {
        let server =
            Server::start(vec![Reply::sse(response("r", vec![message("m", "done")]))]).await;
        let hooks = Arc::new(Hooks {
            fail_start,
            ..Default::default()
        });
        let mut snapshot = SessionSnapshot::new("system", ModelOptions::default());
        snapshot.items.push(Item::user("unfinished request"));
        snapshot.needs_response = true;
        let mut agent = Agent::restore(server.client(), snapshot).unwrap();
        agent.set_hooks(hooks.clone());
        let result = agent
            .continue_run_with_input("new instruction", RunOptions::default(), |_| async {
                Ok(())
            })
            .await;
        if fail_start {
            assert_eq!(result.unwrap_err().kind, ErrorKind::EventHandler);
            assert!(server.captured().is_empty());
            assert_eq!(
                agent.snapshot().items.last(),
                Some(&Item::user("new instruction"))
            );
        } else {
            assert_eq!(result.unwrap().stop_reason, StopReason::Completed);
            let states = hooks.states.lock().unwrap();
            assert_eq!(states[0].0, CheckpointKind::RunStarted);
            assert_eq!(
                states[0].1.items.last(),
                Some(&Item::user("new instruction"))
            );
            assert_eq!(server.captured().len(), 1);
        }
    }
}

struct Effect(Arc<AtomicUsize>, Arc<Hooks>);
impl Tool for Effect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "effect".into(),
            description: "test".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
        Ok(())
    }
    fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let saved = self.1.states.lock().unwrap();
            let (_, snapshot) = saved.last().unwrap();
            assert_eq!(snapshot.pending[0].state, PendingState::Unknown);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("done"))
        })
    }
}
async fn run(hooks: Arc<Hooks>) -> (Result<RunOutcome>, usize, SessionSnapshot, usize) {
    let server = Server::start(vec![
        Reply::sse(response("r1", vec![call("c1", "effect", "{}")])),
        Reply::sse(response("r2", vec![message("m2", "done")])),
    ])
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    agent
        .register_tool(Effect(calls.clone(), hooks.clone()))
        .unwrap();
    agent.set_hooks(hooks);
    let result = agent
        .run(
            "go",
            RunOptions {
                tool_timeout: Duration::from_millis(50),
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await;
    (
        result,
        calls.load(Ordering::SeqCst),
        agent.snapshot(),
        server.captured().len(),
    )
}

#[tokio::test]
async fn intent_is_saved_before_side_effect_and_authorization_does_not_use_tool_timeout() {
    let hooks = Arc::new(Hooks {
        wait: Duration::from_millis(100),
        ..Default::default()
    });
    let (result, calls, snapshot, requests) = run(hooks.clone()).await;
    result.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(requests, 2);
    assert!(snapshot.pending.is_empty());
    let states = hooks.states.lock().unwrap();
    assert!(matches!(
        states.first().unwrap().0,
        CheckpointKind::RunStarted
    ));
    assert!(
        states
            .iter()
            .any(|(kind, _)| matches!(kind, CheckpointKind::ToolResult { .. }))
    );
}

#[tokio::test]
async fn failed_persistence_prevents_requests_and_tools() {
    let (result, calls, _, requests) = run(Arc::new(Hooks {
        fail_start: true,
        ..Default::default()
    }))
    .await;
    assert!(result.is_err());
    assert_eq!((calls, requests), (0, 0));
    let (result, calls, snapshot, requests) = run(Arc::new(Hooks {
        fail_intent: true,
        ..Default::default()
    }))
    .await;
    assert!(result.is_err());
    assert_eq!((calls, requests), (0, 1));
    assert_eq!(snapshot.pending[0].state, PendingState::Unknown);
}

#[tokio::test]
async fn denied_tool_never_runs_and_has_a_resolved_error() {
    let (result, calls, snapshot, _) = run(Arc::new(Hooks {
        deny: true,
        ..Default::default()
    }))
    .await;
    result.unwrap();
    assert_eq!(calls, 0);
    assert!(snapshot.pending.is_empty());
    assert!(
        snapshot
            .items
            .iter()
            .any(|i| matches!(i,Item::FunctionCallOutput{output,..} if output.contains("denied")))
    );
}

#[tokio::test]
async fn cancelling_authorization_leaves_ready_not_unknown() {
    let hooks = Arc::new(Hooks {
        wait: Duration::from_secs(60),
        ..Default::default()
    });
    let signal = hooks.clone();
    let server = Server::start(vec![Reply::sse(response(
        "r",
        vec![call("c", "effect", "{}")],
    ))])
    .await;
    let mut agent = Agent::new(server.client(), "test", ModelOptions::default()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    agent
        .register_tool(Effect(calls.clone(), hooks.clone()))
        .unwrap();
    agent.set_hooks(hooks);
    let token = CancellationToken::new();
    let cancel = token.clone();
    let cancel_task = tokio::spawn(async move {
        signal.entered.notified().await;
        cancel.cancel();
    });
    let error = agent
        .run(
            "go",
            RunOptions {
                cancellation: token,
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err();
    cancel_task.await.unwrap();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(agent.snapshot().pending[0].state, PendingState::Ready);
}
