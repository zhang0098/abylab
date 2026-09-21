//! Host-initiated steering: user text injected into a turn that is already
//! running, at the next complete tool-batch boundary.
//!
//! The harness equivalent is `agent.steer` / the `next-step` inbox. The three
//! cases here are the ones abylab's composer depends on: a steer during a step,
//! a steer that arrives just as the model was about to end the turn, and a
//! steer that misses the window entirely and is spent on the next run.

mod common;
use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

/// A tool the fixtures can call; it only counts its executions.
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

/// Stands in for the composer's send-now gesture: it fires exactly one steer
/// at a chosen checkpoint, so the test never depends on wall-clock timing.
struct SteerAt {
    handle: SteerHandle,
    at: At,
    text: String,
    fired: Mutex<bool>,
    /// Serialized user texts per request, in request order (the host's view of
    /// what each step actually carried).
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

/// The checkpoint a steer fires at. Matched by kind, never by payload: the
/// SDK's own call id/state is not the fixture's business.
#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    ModelResponse,
    ToolIntent,
    ToolResult,
}

impl At {
    fn is(self, kind: &CheckpointKind) -> bool {
        matches!(
            (self, kind),
            (At::ModelResponse, CheckpointKind::ModelResponse)
                | (At::ToolIntent, CheckpointKind::ToolIntent { .. })
                | (At::ToolResult, CheckpointKind::ToolResult { .. })
        )
    }
}

impl SteerAt {
    fn new(handle: SteerHandle, at: At, text: &str) -> (Self, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                handle,
                at,
                text: text.into(),
                fired: Mutex::new(false),
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl AgentHooks for SteerAt {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(user_texts(&snapshot.items));
            if !self.at.is(&kind) {
                return Ok(());
            }
            let mut fired = self.fired.lock().unwrap();
            if !*fired {
                *fired = true;
                self.handle.send(self.text.clone())?;
            }
            Ok(())
        })
    }
}

/// Every user text in a transcript, in order.
fn user_texts(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Message {
                role: MessageRole::User,
                content,
                ..
            } => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::InputText { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

/// Every `text` string carried by a request body, at any nesting depth (tool
/// results nest their own content array).
fn texts_in(body: &Value) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(text)) = map.get("text") {
                    out.push(text.clone());
                }
                for nested in map.values() {
                    walk(nested, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&body["messages"], &mut out);
    out
}

#[tokio::test]
async fn a_steer_lands_at_the_next_step_boundary_of_the_running_turn() {
    let server = Server::start(vec![
        Reply::sse(response("a", vec![call("c1", "effect", "{}")])),
        Reply::sse(response("b", vec![message("m", "steered answer")])),
    ])
    .await;
    let mut agent = Agent::new(server.client(), "persona", ModelOptions::default()).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    agent.register_tool(Effect(Arc::clone(&count))).unwrap();
    let handle = agent.steer_handle();
    // ToolIntent fires while the step is in flight: the tool has not run yet,
    // so the message can only be delivered at the boundary after the batch.
    let (hooks, seen) = SteerAt::new(handle.clone(), At::ToolIntent, "change course");
    agent.set_hooks(Arc::new(hooks));

    let outcome = agent
        .run("start", RunOptions::default(), ignore)
        .await
        .unwrap();

    // One turn, one run: steering never re-enters the driver's run loop.
    assert_eq!(agent.snapshot().run_sequence, 1);
    assert_eq!(outcome.requests.len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.response.output_text(), "steered answer");
    // The steer text entered the transcript after the tool result, and the
    // second request carried both.
    let snapshot = agent.snapshot();
    let texts = user_texts(&snapshot.items);
    assert_eq!(
        texts,
        vec!["start".to_string(), "change course".to_string()]
    );
    let last = snapshot
        .items
        .iter()
        .rposition(|item| matches!(item, Item::FunctionCall { .. }))
        .unwrap();
    let steer_at = snapshot
        .items
        .iter()
        .position(|item| matches!(item, Item::Message { role: MessageRole::User, content, .. }
            if content.iter().any(|part| matches!(part, ContentPart::InputText { text } if text == "change course"))))
        .unwrap();
    assert!(steer_at > last, "the steer must land after the tool batch");
    let captured = server.captured();
    assert_eq!(captured.len(), 2);
    let second = texts_in(&captured[1].body);
    assert!(second.contains(&"start".to_string()));
    assert!(second.contains(&"effect done".to_string()));
    assert!(second.contains(&"change course".to_string()));
    // The hook saw the boundary: the second request is the one that carried it.
    let boundaries = seen.lock().unwrap().clone();
    assert!(
        boundaries.iter().any(|texts| texts.len() == 2),
        "the boundary checkpoint must observe the injected message: {boundaries:?}"
    );
}

#[tokio::test]
async fn a_steer_after_the_last_step_continues_the_run_instead_of_ending_it() {
    let server = Server::start(vec![
        Reply::sse(response("a", vec![message("m", "first answer")])),
        Reply::sse(response("b", vec![message("m", "second answer")])),
    ])
    .await;
    let mut agent = Agent::new(server.client(), "persona", ModelOptions::default()).unwrap();
    let handle = agent.steer_handle();
    // ModelResponse with no pending calls is the exact window the loop checks
    // right after the final response: a message there turns "stop" into
    // "continue one more step" instead of a new turn.
    let (hooks, _) = SteerAt::new(handle, At::ModelResponse, "keep going");
    agent.set_hooks(Arc::new(hooks));

    let outcome = agent
        .run("start", RunOptions::default(), ignore)
        .await
        .unwrap();

    assert_eq!(agent.snapshot().run_sequence, 1);
    assert_eq!(outcome.requests.len(), 2);
    assert_eq!(outcome.response.output_text(), "second answer");
    assert_eq!(server.captured().len(), 2);
    assert_eq!(
        user_texts(&agent.snapshot().items),
        vec!["start".to_string(), "keep going".to_string()]
    );
}

#[tokio::test]
async fn an_idle_agent_spends_a_steer_on_the_next_run() {
    let server = Server::start(vec![Reply::sse(response(
        "a",
        vec![message("m", "answer")],
    ))])
    .await;
    let mut agent = Agent::new(server.client(), "persona", ModelOptions::default()).unwrap();
    let handle = agent.steer_handle();

    // No turn is running: the message waits instead of failing, which is what
    // lets the TUI treat a missed window as "the next turn" rather than an error.
    assert!(handle.send("  ").is_err(), "empty text is rejected");
    handle.send("while you were away").unwrap();
    assert!(!handle.is_empty());

    let outcome = agent
        .run("start", RunOptions::default(), ignore)
        .await
        .unwrap();

    assert_eq!(outcome.requests.len(), 1);
    assert!(handle.is_empty());
    assert_eq!(
        user_texts(&agent.snapshot().items),
        vec!["start".to_string(), "while you were away".to_string()]
    );
    let captured = texts_in(&server.captured()[0].body);
    assert!(captured.contains(&"start".to_string()));
    assert!(captured.contains(&"while you were away".to_string()));
}

/// A steer is ordinary transcript text: it survives a restore, so a resumed
/// session shows what the user actually injected.
#[tokio::test]
async fn a_settled_steer_is_part_of_the_durable_transcript() {
    let server = Server::start(vec![
        Reply::sse(response("a", vec![call("c1", "effect", "{}")])),
        Reply::sse(response("b", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = Agent::new(server.client(), "persona", ModelOptions::default()).unwrap();
    agent
        .register_tool(Effect(Arc::new(AtomicUsize::new(0))))
        .unwrap();
    let handle = agent.steer_handle();
    let (hooks, _) = SteerAt::new(handle, At::ToolResult, "and then stop");
    agent.set_hooks(Arc::new(hooks));
    agent
        .run("start", RunOptions::default(), ignore)
        .await
        .unwrap();

    let restored = Agent::restore(server.client(), agent.snapshot()).unwrap();
    restored.snapshot().validate().unwrap();
    assert_eq!(
        user_texts(&restored.snapshot().items),
        vec!["start".to_string(), "and then stop".to_string()]
    );
}
