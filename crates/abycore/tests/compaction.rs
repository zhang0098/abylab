//! Context compaction: the durable transcript keeps every item while the
//! model-visible request view replaces a balanced span with a host summary.

mod common;
use abycore::*;
use common::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Echo {
    calls: Arc<AtomicUsize>,
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
        if arguments["text"].is_string() {
            Ok(())
        } else {
            Err(ToolError::Failed("text is required".into()))
        }
    }
    fn execute<'a>(&'a self, arguments: Value, _context: ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text(arguments["text"].as_str().unwrap()))
        })
    }
}

fn agent(server: &Server) -> Agent {
    Agent::new(server.client(), "system prompt", ModelOptions::default()).unwrap()
}

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

/// One tool round trip leaves `[user, call, output, message]`; the cuts make the
/// tool batch indivisible and a compaction replace it with one summary message.
#[tokio::test]
async fn compaction_replaces_a_balanced_span_in_the_request_view_only() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "done")])),
        Reply::sse(response("r3", vec![message("m3", "second turn")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    let long_prompt = "first prompt ".repeat(200);
    agent
        .run(long_prompt.clone(), RunOptions::default(), ignore)
        .await
        .unwrap();

    assert_eq!(agent.snapshot().items.len(), 4);
    // The tool batch (call + output) is the only indivisible unit: no cut may
    // fall between them.
    assert_eq!(agent.compaction_cuts(), vec![0, 1, 3, 4]);
    assert!(
        agent.compact(1, 2, "summary").is_err(),
        "a span splitting the tool batch is rejected"
    );
    assert!(
        agent.compact(0, 0, "empty").is_err(),
        "an empty span is rejected"
    );
    assert!(
        agent.compact(0, 4, "   ").is_err(),
        "a blank summary is rejected"
    );
    assert!(
        agent
            .compact(
                3,
                4,
                "a summary longer than the message it replaces, which must be rejected"
            )
            .is_err(),
        "a non-shrinking summary is rejected"
    );

    let before = agent.context_estimate().unwrap();
    assert_eq!(before.compactions, 0);
    let record = agent.compact(0, 3, "the user asked for one echo").unwrap();
    assert_eq!((record.start, record.end), (0, 3));
    assert_eq!(agent.compactions().len(), 1);
    assert!(
        agent.compact(2, 4, "overlap").is_err(),
        "spans must be ordered and non-overlapping"
    );

    // The durable transcript is untouched; only the view changed.
    assert_eq!(agent.snapshot().items.len(), 4);
    let view = agent.request_view();
    assert_eq!(view.len(), 2, "three span items became one summary message");
    assert!(
        matches!(&view[0], Item::Message { role: MessageRole::User, content, .. }
            if content.iter().map(ContentPart::text).collect::<String>().contains("the user asked for one echo")
                && content.iter().map(ContentPart::text).collect::<String>().contains(SUMMARY_OPEN_TAG)),
        "the summary lands where the span was: {view:?}"
    );
    assert_eq!(view[1], agent.snapshot().items[3]);

    let after = agent.context_estimate().unwrap();
    assert!(
        after.request_bytes < before.request_bytes,
        "compaction must shrink the request: {before:?} -> {after:?}"
    );
    assert_eq!(after.compactions, 1);

    // The next request carries the summary instead of the shadowed span, and
    // the transcript keeps every original item for replay.
    agent
        .run("second prompt", RunOptions::default(), ignore)
        .await
        .unwrap();
    let captured = server.captured();
    let body = captured[2].body.to_string();
    assert!(body.contains("the user asked for one echo"), "{body}");
    assert!(
        !body.contains("first prompt"),
        "the shadowed user message is out of the model's view"
    );
    assert_eq!(
        agent.snapshot().items.len(),
        6,
        "appending never rewrites history"
    );
    assert_eq!(agent.snapshot().compactions.len(), 1);
}

/// A snapshot round trip preserves the compaction, and clearing it restores the
/// full history without touching the transcript.
#[tokio::test]
async fn compaction_survives_snapshot_round_trip_and_clears() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "done")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    agent
        .run("first prompt ".repeat(200), RunOptions::default(), ignore)
        .await
        .unwrap();
    agent.compact(0, 3, "checkpoint text").unwrap();

    let json = agent.snapshot().to_json().unwrap();
    let restored = SessionSnapshot::from_json(&json).unwrap();
    assert_eq!(restored.compactions.len(), 1);
    let restored = Agent::restore(server.client(), restored).unwrap();
    let view = restored.request_view();
    assert_eq!(view.len(), 2);
    assert!(matches!(&view[0], Item::Message { content, .. }
            if content.iter().map(ContentPart::text).collect::<String>().contains("checkpoint text")));

    let mut agent = agent;
    agent.clear_compactions();
    assert_eq!(agent.request_view(), agent.snapshot().items);

    // A corrupted record is rejected on load rather than silently dropped.
    let mut broken = SessionSnapshot::from_json(&json).unwrap();
    broken.compactions[0].end = 2; // cuts the tool batch in half
    assert!(broken.validate().is_err());
    let mut overlapping = SessionSnapshot::from_json(&json).unwrap();
    overlapping.compactions.push(Compaction {
        start: 2,
        end: 4,
        summary: "later".into(),
    });
    assert!(overlapping.validate().is_err());
}

/// A fresh session has no messages yet, but its envelope can still be priced —
/// hosts need the number *before* the first prompt to decide whether to compact.
#[tokio::test]
async fn a_fresh_session_can_be_measured() {
    let server = Server::start(vec![]).await;
    let agent = agent(&server);
    let estimate = agent.context_estimate().unwrap();
    assert!(estimate.request_bytes > 0);
    assert_eq!(estimate.history_bytes, 2, "empty history serializes as []");
    assert_eq!(estimate.compactions, 0);
    assert!(estimate.estimated_tokens() > 0);
}

/// The summarization call replays the conversation prefix and appends the
/// instruction last; only assistant text becomes the summary.
#[tokio::test]
async fn summarize_span_replays_the_prefix_and_returns_text_only() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "done")])),
        Reply::sse(response(
            "r3",
            vec![
                reasoning("think", "private reasoning"),
                message("m3", "## Current Work\n- summarized"),
            ],
        )),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    agent
        .run("first prompt ".repeat(200), RunOptions::default(), ignore)
        .await
        .unwrap();

    let outcome = agent
        .summarize_span(0, 3, SummarizeOptions::default())
        .await
        .unwrap();
    assert_eq!(outcome.summary, "## Current Work\n- summarized");
    assert!(outcome.usage.is_some());
    assert_eq!(outcome.requests.len(), 1);
    assert_eq!(outcome.requests[0].purpose, RequestPurpose::Compaction);

    let body = server.captured()[2].body.to_string();
    assert!(
        body.contains("system prompt"),
        "the session prompt is replayed"
    );
    assert!(
        body.contains("\\\"echo\\\"") || body.contains("\"echo\""),
        "tool schemas are replayed"
    );
    assert!(
        body.contains("first prompt"),
        "the span is replayed verbatim"
    );
    assert!(
        body.contains("compaction engine"),
        "the instruction is the final user message"
    );
    assert!(
        !body.contains("second prompt"),
        "nothing outside the span is sent"
    );

    // The host applies the summary explicitly; summarize alone never mutates.
    assert!(agent.compactions().is_empty());
    agent.compact(0, 3, outcome.summary).unwrap();
    assert_eq!(agent.compactions().len(), 1);
}

/// A host view policy: condense once, at the `at`-th request boundary.
struct PolicyHooks {
    at: usize,
    seen: AtomicUsize,
}

impl AgentHooks for PolicyHooks {
    fn checkpoint<'a>(
        &'a self,
        _: CheckpointKind,
        _: SessionSnapshot,
    ) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn view_request<'a>(
        &'a self,
        estimate: &'a ViewEstimate,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<ViewRequest>>> {
        let boundary = self.seen.fetch_add(1, Ordering::SeqCst) + 1;
        let action = (boundary == self.at).then(|| ViewRequest::Condense {
            start: 0,
            end: estimate
                .cuts
                .iter()
                .rev()
                .map(|cut| cut.index)
                .find(|index| *index > 0)
                .expect("a completed turn has a cut"),
            instruction: None,
            max_tokens: None,
        });
        Box::pin(async move { Ok(action) })
    }
}

/// The request-boundary seam condenses mid-turn: the host picks the boundary,
/// the SDK produces and applies the summary, and the next request carries it.
#[tokio::test]
async fn a_host_policy_can_condense_mid_turn() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        // The summarization call the SDK makes on the host's behalf.
        Reply::sse(response("r2", vec![message("m2", "condensed")])),
        Reply::sse(response("r3", vec![message("m3", "done")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    agent.set_hooks(Arc::new(PolicyHooks {
        at: 2,
        seen: AtomicUsize::new(0),
    }));
    let changes = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&changes);
    agent
        .run(
            "long prompt ".repeat(300),
            RunOptions::default(),
            move |event| {
                if let AgentEvent::ViewChanged { change } = event {
                    captured.lock().unwrap().push(change);
                }
                async { Ok(()) }
            },
        )
        .await
        .unwrap();

    let bodies = server.captured();
    assert_eq!(
        bodies.len(),
        3,
        "tool call, summarization, condensed request"
    );
    let final_body = bodies[2].body.to_string();
    assert!(
        final_body.contains("condensed"),
        "the second request carries the summary: {final_body}"
    );
    assert!(
        !final_body.contains("long prompt"),
        "and not the condensed span"
    );
    assert_eq!(agent.compactions().len(), 1);
    let changes = changes.lock().unwrap();
    assert!(
        matches!(changes.as_slice(), [ViewChange::Compacted { start: 0, .. }]),
        "the change is reported to the host: {changes:?}"
    );
}

/// Provider calibration: the measured count is carried forward when the
/// envelope is plausible, and ignored when it cannot be trusted.
#[test]
fn calibration_carries_a_measured_count_forward() {
    let measured = |request_bytes: usize,
                    last_request_bytes: Option<usize>,
                    last_input_tokens: Option<u64>| ViewEstimate {
        request_bytes,
        history_bytes: 0,
        items: 0,
        compactions: 0,
        compacted_through: 0,
        pruned: false,
        pruned_through: 0,
        last_input_tokens,
        last_request_bytes,
        cuts: vec![],
    };

    // Four bytes per token: 2 000 appended bytes are about 500 tokens.
    let estimate = measured(12_000, Some(10_000), Some(2_500));
    assert_eq!(estimate.calibrated_tokens(), Some(3_000));
    assert_eq!(estimate.conservative_tokens(), 4_000);
    assert_eq!(estimate.tokens(), 3_000, "the calibrated figure wins");
    assert_eq!(estimate.bytes_per_token(), 4);

    // A shrunken view invalidates the measurement: something was compacted.
    let shrunken = measured(9_000, Some(10_000), Some(2_500));
    assert_eq!(shrunken.calibrated_tokens(), None);
    assert_eq!(shrunken.tokens(), shrunken.conservative_tokens());

    // Implausible counters (1 400 bytes per token) are ignored, not trusted.
    let bogus = measured(12_000, Some(10_000), Some(7));
    assert_eq!(bogus.calibrated_tokens(), None);
    assert_eq!(bogus.tokens(), bogus.conservative_tokens());
    assert_eq!(bogus.bytes_per_token(), 3);

    // Nothing measured yet.
    let fresh = measured(8_000, None, None);
    assert_eq!(fresh.tokens(), fresh.conservative_tokens());

    // Provider measurements take precedence over the byte fallback, including
    // dense tokenization at one byte per token.
    let huge = measured(100_000, Some(1_000), Some(1_000));
    assert!(huge.calibrated_tokens().unwrap() > huge.conservative_tokens());
    assert_eq!(huge.tokens(), 100_000);
}

/// Every dispatch records the serialized size of the body the provider priced,
/// which is what makes the calibration possible at all.
#[tokio::test]
async fn the_ledger_records_the_measured_envelope() {
    let server = Server::start(vec![Reply::sse(response("r1", vec![message("m1", "hi")]))]).await;
    let mut agent = agent(&server);
    agent
        .run("hello", RunOptions::default(), ignore)
        .await
        .unwrap();
    let records = agent.snapshot().requests;
    let record = records
        .iter()
        .find(|record| record.purpose == RequestPurpose::Conversation)
        .expect("a conversation request is recorded");
    assert!(
        record.request_bytes.is_some_and(|bytes| bytes > 0),
        "the envelope is measured: {record:?}"
    );
    assert!(record.usage.is_some(), "the provider count is recorded");
}

/// Pruning trims only tool outputs older than its boundary, keeps the
/// transcript intact, and survives a snapshot round trip.
#[tokio::test]
async fn pruning_trims_old_tool_outputs_in_the_view_only() {
    let calls = Arc::new(AtomicUsize::new(0));
    let big = "x".repeat(4_000);
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", &format!(r#"{{"text":"{big}"}}"#))],
        )),
        Reply::sse(response("r2", vec![message("m2", "first done")])),
        Reply::sse(response(
            "r3",
            vec![call("c2", "echo", &format!(r#"{{"text":"{big}"}}"#))],
        )),
        Reply::sse(response("r4", vec![message("m4", "second done")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    agent
        .run("first", RunOptions::default(), ignore)
        .await
        .unwrap();
    agent
        .run("second", RunOptions::default(), ignore)
        .await
        .unwrap();

    let before = agent.context_estimate().unwrap();
    assert!(agent.prune().is_none());
    assert!(
        agent.prune_outputs(0, MIN_PRUNE_BYTES - 1).is_err(),
        "a trim below the minimum is rejected"
    );
    assert!(
        agent.prune_outputs(99, 512).is_err(),
        "the boundary must lie inside the transcript"
    );
    // Everything before the last completed round trip is fair game.
    // Items: [user, call, output, message, user, call, output, message]; the
    // boundary just after the first round trip trims only its output.
    let through = 3;
    agent.prune_outputs(through, 512).unwrap();

    let view = agent.request_view();
    let outputs: Vec<&Item> = view
        .iter()
        .filter(|item| matches!(item, Item::FunctionCallOutput { .. }))
        .collect();
    assert_eq!(outputs.len(), 2);
    let trimmed = |item: &Item| match item {
        Item::FunctionCallOutput { output, .. } => output.clone(),
        _ => unreachable!(),
    };
    assert!(
        trimmed(outputs[0]).contains("pruned for context") && trimmed(outputs[0]).len() < 700,
        "the older output is trimmed: {}",
        trimmed(outputs[0]).len()
    );
    assert_eq!(
        trimmed(outputs[1]).len(),
        agent
            .snapshot()
            .items
            .iter()
            .filter_map(|item| match item {
                Item::FunctionCallOutput { output, .. } => Some(output.len()),
                _ => None,
            })
            .nth(1)
            .unwrap(),
        "the newest output stays verbatim"
    );

    let after = agent.context_estimate().unwrap();
    assert!(after.request_bytes < before.request_bytes);

    // Durable: the record round-trips, and the transcript never changed.
    let restored = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    assert_eq!(
        restored.prune,
        Some(Prune {
            through,
            max_bytes: 512
        })
    );
    assert_eq!(restored.items.len(), agent.snapshot().items.len());
    let mut restored = Agent::restore(server.client(), restored).unwrap();
    restored.clear_prune();
    assert_eq!(
        restored.request_view(),
        restored.snapshot().items,
        "clearing restores the full view"
    );

    let mut broken = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    broken.prune = Some(Prune {
        through: 1,
        max_bytes: 8,
    });
    assert!(broken.validate().is_err());
}

/// A fork derives from the model-visible view: a child must not pay for history
/// the parent already condensed.
#[tokio::test]
async fn forked_children_inherit_the_compacted_view() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "done")])),
        // The host applies its own summary text; no summarization call here.
        Reply::sse(response(
            "r3",
            vec![call(
                "fork",
                "subagent_fork",
                r#"{"description":"fork","prompt":"new child task"}"#,
            )],
        )),
        Reply::sse(response("r4", vec![message("m4", "child answer")])),
        Reply::sse(response("r5", vec![message("m5", "parent done")])),
    ])
    .await;
    let mut parent = agent(&server);
    parent.register_tool(Echo { calls }).unwrap();
    let long_prompt = "parent history ".repeat(200);
    parent
        .run(long_prompt.clone(), RunOptions::default(), ignore)
        .await
        .unwrap();
    parent.compact(0, 3, "SUMMARY-TEXT").unwrap();

    let agents = Subagents::new();
    agents.register(&mut parent).unwrap();
    parent
        .run("fork the work", RunOptions::default(), ignore)
        .await
        .unwrap();

    let child = server.captured()[3].body.to_string();
    assert!(
        child.contains("SUMMARY-TEXT"),
        "the child starts from the summary: {child}"
    );
    assert!(
        !child.contains("parent history"),
        "and never from the shadowed span"
    );
    assert!(child.contains("new child task"));
    agents.shutdown().await;
}

/// `max_tokens` overrides the session cap for the auxiliary call only.
#[tokio::test]
async fn summarize_span_honors_its_own_output_cap() {
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::start(vec![
        Reply::sse(response(
            "r1",
            vec![call("c1", "echo", r#"{"text":"one"}"#)],
        )),
        Reply::sse(response("r2", vec![message("m2", "done")])),
        Reply::sse(response("r3", vec![message("m3", "summary")])),
    ])
    .await;
    let mut agent = agent(&server);
    agent.register_tool(Echo { calls }).unwrap();
    agent
        .run("first prompt", RunOptions::default(), ignore)
        .await
        .unwrap();
    agent
        .summarize_span(
            0,
            3,
            SummarizeOptions {
                max_tokens: Some(1234),
                timeout: Duration::from_secs(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let body = &server.captured()[2].body;
    assert_eq!(body["max_tokens"], json!(1234));
}

fn seeded_agent(server: &Server) -> Agent {
    let mut snapshot = SessionSnapshot::new("system", ModelOptions::default());
    snapshot.items = vec![
        Item::user("original context ".repeat(1000)),
        Item::Message {
            id: None,
            role: MessageRole::Assistant,
            content: vec![ContentPart::OutputText {
                text: "older answer ".repeat(300),
            }],
        },
    ];
    Agent::restore(server.client(), snapshot).unwrap()
}

#[tokio::test]
async fn truncated_or_tool_call_summaries_never_replace_history() {
    let mut partial = response("partial", vec![message("partial", "only half")]);
    partial["stop_reason"] = json!("max_tokens");
    let server = Server::start(vec![
        Reply::sse(partial),
        Reply::sse(response(
            "tool",
            vec![
                message("text", "text plus an unwanted action"),
                call("call", "echo", r#"{"text":"unwanted"}"#),
            ],
        )),
    ])
    .await;
    let agent = seeded_agent(&server);
    let original = agent.request_view();
    for _ in 0..2 {
        let err = agent
            .summarize_span(0, 2, SummarizeOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert_eq!(agent.request_view(), original);
        assert!(agent.compactions().is_empty());
    }
}

#[tokio::test]
async fn automatic_summary_shares_the_run_request_budget() {
    let server = Server::start(vec![Reply::sse(response(
        "summary",
        vec![message("s", "checkpoint")],
    ))])
    .await;
    let mut agent = seeded_agent(&server);
    agent.set_hooks(Arc::new(PolicyHooks {
        at: 1,
        seen: AtomicUsize::new(0),
    }));
    let err = agent
        .run(
            "next",
            RunOptions {
                max_requests: 1,
                ..Default::default()
            },
            ignore,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::BudgetExceeded);
    assert_eq!(server.captured().len(), 1);
    assert_eq!(agent.snapshot().requests.len(), 1);
    assert_eq!(
        agent.snapshot().requests[0].purpose,
        RequestPurpose::Compaction
    );
    assert_eq!(agent.compactions().len(), 1);
}

#[tokio::test]
async fn automatic_summary_obeys_cancellation_and_the_run_window() {
    for cancel in [false, true] {
        let mut reply = Reply::sse(response("summary", vec![message("s", "checkpoint")]));
        reply.header_delay = Duration::from_secs(5);
        let server = Server::start(vec![reply]).await;
        let mut agent = seeded_agent(&server);
        agent.set_hooks(Arc::new(PolicyHooks {
            at: 1,
            seen: AtomicUsize::new(0),
        }));
        let cancellation = CancellationToken::new();
        if cancel {
            let trigger = cancellation.clone();
            let requests = server.requests.clone();
            tokio::spawn(async move {
                while requests.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
                trigger.cancel();
            });
        }
        // A window far shorter than this fixture's answer: a summary that says
        // nothing for the whole window is a stalled run, whichever bound
        // (transport or window) fires first.
        let options = RunOptions {
            cancellation,
            timeout: if cancel {
                Duration::from_secs(10)
            } else {
                Duration::from_millis(100)
            },
            ..Default::default()
        };
        let outcome =
            tokio::time::timeout(Duration::from_secs(2), agent.run("next", options, ignore))
                .await
                .expect("summary must stop before the delayed response");
        assert_eq!(
            outcome.unwrap_err().kind,
            if cancel {
                ErrorKind::Cancelled
            } else {
                ErrorKind::Timeout
            }
        );
        assert_eq!(server.captured().len(), 1);
        assert!(agent.compactions().is_empty());
    }
}

#[tokio::test]
async fn repeated_compaction_uses_existing_summaries_and_pruned_outputs() {
    let server = Server::start(vec![Reply::sse(response(
        "s",
        vec![message("s", "merged checkpoint")],
    ))])
    .await;
    let mut snapshot = seeded_agent(&server).snapshot();
    snapshot.items.extend([
        Item::user("recent instruction ".repeat(200)),
        Item::FunctionCall {
            id: "id".into(),
            call_id: "call".into(),
            name: "read".into(),
            arguments: "{}".into(),
        },
        Item::FunctionCallOutput {
            call_id: "call".into(),
            output: format!("{}HIDDEN_TAIL", "x".repeat(10_000)),
            is_error: false,
            meta: None,
        },
        Item::user("keep this latest instruction"),
    ]);
    snapshot.needs_response = true;
    let mut agent = Agent::restore(server.client(), snapshot.clone()).unwrap();
    agent.compact(0, 2, "FIRST_CHECKPOINT").unwrap();
    agent.prune_outputs(5, 64).unwrap();
    assert!(
        !agent.compaction_cuts().contains(&1),
        "existing summaries are indivisible"
    );
    assert!(
        agent
            .summarize_span(1, 5, SummarizeOptions::default())
            .await
            .is_err()
    );
    assert!(
        server.captured().is_empty(),
        "invalid span costs no request"
    );
    let estimate = agent.view_estimate().unwrap();
    let view = agent.request_view();
    assert_eq!(
        estimate.cuts[0].suffix_bytes,
        serde_json::to_vec(&view).unwrap().len()
    );
    let cut = estimate.cuts.iter().find(|cut| cut.index == 2).unwrap();
    assert_eq!(
        cut.suffix_bytes,
        serde_json::to_vec(&view[1..]).unwrap().len()
    );
    let summary = agent
        .summarize_span(0, 5, SummarizeOptions::default())
        .await
        .unwrap();
    let body = server.captured()[0].body.to_string();
    assert!(body.contains("FIRST_CHECKPOINT") && body.contains("pruned for context"));
    assert!(!body.contains("original context") && !body.contains("HIDDEN_TAIL"));
    assert!(!body.contains("keep this latest instruction"));
    agent.compact(0, 5, summary.summary).unwrap();
    assert_eq!(agent.compactions().len(), 1);
    assert_eq!(agent.compactions()[0].end, 5);
    assert_eq!(agent.snapshot().items, snapshot.items);
    let restored = Agent::restore(
        server.client(),
        SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(restored.request_view(), agent.request_view());
}

#[tokio::test]
async fn a_replacement_must_shrink_the_current_view_and_invalidates_old_calibration() {
    let server = Server::start(vec![]).await;
    let mut agent = seeded_agent(&server);
    agent.compact(0, 2, "small").unwrap();
    let before = agent.request_view();
    assert!(agent.compact(0, 2, "x".repeat(500)).is_err());
    assert_eq!(agent.request_view(), before);
    let dense = ContextEstimate {
        request_bytes: 3000,
        history_bytes: 2900,
        last_input_tokens: Some(2000),
        last_request_bytes: Some(3000),
        compactions: 0,
    };
    assert_eq!(dense.tokens(), 2000, "never undercut known provider usage");

    let mut snapshot = seeded_agent(&server).snapshot();
    snapshot.requests.push(RequestRecord {
        purpose: RequestPurpose::Conversation,
        attempt: 1,
        response_id: None,
        status: Some("Completed".into()),
        usage: Some(Usage {
            input_tokens: Some(1000),
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            uncached_input_tokens: Some(1000),
            cache_creation_tokens: None,
            total_tokens: None,
            consistent: true,
        }),
        request_bytes: Some(4000),
    });
    let mut agent = Agent::restore(server.client(), snapshot).unwrap();
    assert_eq!(
        agent.context_estimate().unwrap().last_input_tokens,
        Some(1000)
    );
    agent.prune_outputs(0, 64).unwrap();
    assert_eq!(
        agent.context_estimate().unwrap().last_input_tokens,
        None,
        "a changed view needs a new measurement even after growing beyond the old byte count"
    );
}

#[tokio::test]
async fn a_failed_compaction_checkpoint_blocks_the_next_request() {
    struct Barrier;
    impl AgentHooks for Barrier {
        fn checkpoint<'a>(
            &'a self,
            kind: CheckpointKind,
            snapshot: SessionSnapshot,
        ) -> futures_util::future::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                if kind == CheckpointKind::ViewChanged {
                    assert_eq!(snapshot.compactions.len(), 1);
                    Err(Error::new(ErrorKind::Session, "disk unavailable"))
                } else {
                    Ok(())
                }
            })
        }
        fn view_request<'a>(
            &'a self,
            _: &'a ViewEstimate,
        ) -> futures_util::future::BoxFuture<'a, Result<Option<ViewRequest>>> {
            Box::pin(async {
                Ok(Some(ViewRequest::Condense {
                    start: 0,
                    end: 2,
                    instruction: None,
                    max_tokens: None,
                }))
            })
        }
    }
    let server = Server::start(vec![Reply::sse(response(
        "summary",
        vec![message("s", "checkpoint")],
    ))])
    .await;
    let mut agent = seeded_agent(&server);
    agent.set_hooks(Arc::new(Barrier));
    let err = agent
        .run("next", RunOptions::default(), ignore)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Session);
    assert_eq!(
        server.captured().len(),
        1,
        "a failed durable barrier must prevent the conversation dispatch"
    );
    assert_eq!(
        agent.compactions().len(),
        1,
        "the host can retry saving the final snapshot"
    );
}
