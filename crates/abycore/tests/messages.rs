mod common;
use abycore::*;
use common::*;
use futures_util::TryStreamExt;
use serde_json::{Value, json};
use std::time::Duration;

fn block_start(index: usize, block: Value) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":block})
}
fn delta(index: usize, delta: Value) -> Value {
    json!({"type":"content_block_delta","index":index,"delta":delta})
}
fn block_stop(index: usize) -> Value {
    json!({"type":"content_block_stop","index":index})
}
fn end(reason: &str) -> Vec<Value> {
    vec![
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":6}}),
        json!({"type":"message_stop"}),
    ]
}
async fn collect(server: &Server) -> Result<Vec<StreamEvent>> {
    server
        .client()
        .stream(MessageRequest::new("hello"), RequestOptions::default())
        .await?
        .try_collect()
        .await
}

#[tokio::test]
async fn native_stream_preserves_interleaving_signatures_arguments_and_cumulative_usage() {
    let mut events = vec![
        message_start("msg"),
        json!({"type":"ping"}),
        block_start(4, json!({"type":"thinking","thinking":"先"})),
        block_start(8, message("unused", "前")),
        delta(4, json!({"type":"thinking_delta","thinking":"思考"})),
        delta(4, json!({"type":"signature_delta","signature":"signed-"})),
        delta(4, json!({"type":"signature_delta","signature":"thinking"})),
        delta(8, json!({"type":"text_delta","text":"你好"})),
        block_stop(4),
        block_stop(8),
        block_start(12, call("call-1", "echo", "{}")),
        delta(
            12,
            json!({"type":"input_json_delta","partial_json":"{\"text\":"}),
        ),
        block_start(15, call("call-2", "echo", r#"{"text":"two"}"#)),
        delta(
            12,
            json!({"type":"input_json_delta","partial_json":"\"一\"}"}),
        ),
        block_stop(15),
        block_stop(12),
        json!({"type":"message_delta","delta":{},"usage":{"input_tokens":9,"output_tokens":3}}),
    ];
    events.extend(end("tool_use"));
    let bytes = events.iter().map(frame).collect::<String>().into_bytes();
    let mut reply = Reply::raw(200, "text/event-stream", "");
    // Exercise real network UTF-8 and JSON fragmentation, not only parsed fixtures.
    reply.chunks = bytes
        .chunks(7)
        .map(|b| (Duration::ZERO, b.to_vec()))
        .collect();
    let server = Server::start(vec![reply]).await;
    let output = collect(&server).await.unwrap();
    let text: String = output
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "前你好");
    let reasoning: String = output
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ReasoningDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "先思考");
    // A host can render a tool call while it streams: every argument delta names
    // the call it belongs to.
    let streamed: Vec<(&str, &str, &str)> = output
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolArgumentsDelta {
                call_id,
                name,
                delta,
                ..
            } => Some((call_id.as_str(), name.as_str(), delta.as_str())),
            _ => None,
        })
        .collect();
    assert!(
        streamed
            .iter()
            .all(|(call_id, name, _)| *call_id == "call-1" && *name == "echo"),
        "deltas carry their call identity: {streamed:?}"
    );
    assert_eq!(
        streamed.iter().map(|(_, _, d)| *d).collect::<String>(),
        r#"{"text":"一"}"#
    );
    assert_eq!(
        output
            .iter()
            .filter(|e| matches!(e, StreamEvent::ItemDone { .. }))
            .count(),
        4
    );
    let StreamEvent::Finished { response, .. } = output.last().unwrap() else {
        panic!("missing final message")
    };
    assert_eq!(response.output_text(), text);
    let Item::Reasoning {
        signature: Some(signature),
        ..
    } = &response.output[0]
    else {
        panic!("lost signature")
    };
    assert_eq!(signature.value, "signed-thinking");
    assert_eq!(signature.model, "deepseek-flash");
    assert!(
        matches!(&response.output[2],Item::FunctionCall{call_id,arguments,..} if call_id=="call-1" && arguments==r#"{"text":"一"}"#)
    );
    assert!(matches!(&response.output[3],Item::FunctionCall{call_id,..} if call_id=="call-2"));
    let usage = response.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.output_tokens, Some(6));
    assert_eq!(usage.total_tokens, Some(18));
    assert_eq!(usage.reasoning_tokens, None);
}

/// Providers send `null` for counters that do not apply (`cache_read_input_tokens`
/// on an uncached request). The non-streaming decoder reads those as absent;
/// the streaming path must not abort the whole response over one.
#[tokio::test]
async fn null_usage_counters_read_as_absent_and_do_not_fail_the_stream() {
    let events = vec![
        json!({"type":"message_start","message":{"type":"message","role":"assistant","id":"msg","model":"fixture-model","content":[],
            "usage":{"input_tokens":7,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"output_tokens":null}}}),
        block_start(0, message("m", "hi")),
        block_stop(0),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":null,"output_tokens":6}}),
        json!({"type":"message_stop"}),
    ];
    let server = Server::start(vec![Reply::events(events)]).await;
    let output = collect(&server).await.unwrap();
    let StreamEvent::Finished { response, .. } = output.last().unwrap() else {
        panic!("missing final message")
    };
    assert_eq!(response.output_text(), "hi");
    let usage = response
        .usage
        .as_ref()
        .expect("usage survives null counters");
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(usage.output_tokens, Some(6));
}

/// A completed response with no content blocks is a transient provider defect
/// (harness's EMPTY_RESPONSE), not a gateway protocol violation: the kind is
/// what lets the host retry it inside the open turn.
#[tokio::test]
async fn an_empty_completed_response_is_classified_as_retryable() {
    let events = vec![
        message_start("msg"),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}}),
        json!({"type":"message_stop"}),
    ];
    let server = Server::start(vec![Reply::events(events)]).await;
    let error = collect(&server).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::EmptyResponse);
}

#[tokio::test]
async fn malformed_streams_never_report_success_or_retry() {
    let start = message_start("m");
    let text = block_start(0, message("unused", ""));
    let chunk = delta(0, json!({"type":"text_delta","text":"hi"}));
    let stop = block_stop(0);
    let settle = end("end_turn")[0].clone();
    let mut cases = vec![
        vec![text.clone()],
        vec![start.clone(), start.clone()],
        vec![start.clone(), text.clone(), text.clone()],
        vec![start.clone(), chunk.clone()],
        vec![start.clone(), text.clone(), stop.clone(), chunk.clone()],
        vec![start.clone(), text.clone(), stop.clone(), stop.clone()],
        vec![
            start.clone(),
            text.clone(),
            delta(0, json!({"type":"thinking_delta","thinking":"x"})),
        ],
        vec![start.clone(), text.clone(), settle.clone()],
        vec![start.clone(), json!({"type":"message_stop"})],
        vec![
            start.clone(),
            text.clone(),
            stop.clone(),
            settle.clone(),
            block_start(1, message("unused", "late")),
        ],
        vec![
            start.clone(),
            text.clone(),
            stop.clone(),
            settle.clone(),
            end("max_tokens")[0].clone(),
        ],
        vec![
            start.clone(),
            json!({"type":"content_block_start","index":-1,"content_block":message("unused","")}),
        ],
        vec![
            start.clone(),
            block_start(0, json!({"type":"redacted_thinking","data":"opaque"})),
        ],
        vec![
            start.clone(),
            block_start(
                0,
                json!({"type":"tool_use","id":"c","name":"echo","input":[]}),
            ),
        ],
        vec![
            start.clone(),
            block_start(0, call("c", "echo", "{}")),
            block_start(1, call("c", "echo", "{}")),
        ],
        vec![
            start.clone(),
            json!({"type":"response.created","response":{"id":"legacy"}}),
        ],
        vec![
            start.clone(),
            json!({"type":"message_delta","delta":{},"usage":{"output_tokens":-1}}),
        ],
        vec![
            start.clone(),
            text.clone(),
            stop.clone(),
            end("unknown")[0].clone(),
        ],
        vec![
            start.clone(),
            text.clone(),
            stop.clone(),
            end("tool_use")[0].clone(),
        ],
    ];
    for arguments in ["{", "[]", "null"] {
        cases.push(vec![
            start.clone(),
            block_start(0, call("c", "echo", "{}")),
            delta(
                0,
                json!({"type":"input_json_delta","partial_json":arguments}),
            ),
            block_stop(0),
            end("tool_use")[0].clone(),
        ]);
    }
    for (index, mut events) in cases.into_iter().enumerate() {
        events.push(json!({"type":"message_stop"}));
        let server = Server::start(vec![Reply::events(events)]).await;
        let mut config = server.config();
        config.retry.max_retries = 2;
        let result = DeepSeekClient::new(config)
            .unwrap()
            .stream(MessageRequest::new("hello"), RequestOptions::default())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await;
        assert_eq!(
            result.unwrap_err().kind,
            ErrorKind::Protocol,
            "case {index}"
        );
        assert_eq!(server.captured().len(), 1);
    }
}

#[tokio::test]
async fn truncated_tool_json_is_observable_but_never_executed_or_committed() {
    let mut events = vec![
        message_start("truncated"),
        block_start(0, call("c", "write", "{}")),
        delta(
            0,
            json!({"type":"input_json_delta","partial_json":"{\"file_path\":"}),
        ),
        block_stop(0),
    ];
    events.extend(end("max_tokens"));
    let server = Server::start(vec![Reply::events(events)]).await;
    let dir = tempfile::tempdir().unwrap();
    let tools = LocalTools::new(dir.path()).unwrap();
    let mut agent = Agent::new(server.client(), "", ModelOptions::default()).unwrap();
    tools.register(&mut agent).unwrap();
    let outcome = agent
        .run("write", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Incomplete);
    assert_eq!(
        outcome.response.incomplete_reason.as_deref(),
        Some("max_tokens")
    );
    assert!(
        matches!(&outcome.response.output[0],Item::FunctionCall{arguments,..} if arguments=="{\"file_path\":")
    );
    assert!(agent.snapshot().pending.is_empty());
    assert_eq!(agent.snapshot().items, vec![Item::user("write")]);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn request_serialization_matches_harness_and_scopes_signature_replay_to_model() {
    let wire = response(
        "first",
        vec![
            reasoning("r", "keep reasoning"),
            call("call-1", "echo", r#"{"text":"ok"}"#),
        ],
    );
    let server = Server::start(vec![
        Reply::json(wire),
        Reply::json(response("second", vec![message("m", "ok")])),
        Reply::json(response("third", vec![message("m", "ok")])),
    ])
    .await;
    let first = server
        .client()
        .complete(MessageRequest::new("start"), RequestOptions::default())
        .await
        .unwrap();
    let mut request = MessageRequest::new("start");
    request.system = "system instructions".into();
    request.history.extend(first.output);
    request.history.push(Item::FunctionCallOutput {
        call_id: "call-1".into(),
        output: "known failure".into(),
        is_error: true,
        meta: Some(json!({"host_only":"must not reach provider"})),
    });
    request.history.push(Item::user("retry"));
    request.tools = vec![ToolDefinition {
        name: "echo".into(),
        description: "echo text".into(),
        parameters: json!({"type":"object"}),
    }];
    request.options.tool_choice = ToolChoice::None;
    request.options.reasoning = ReasoningEffort::Off;
    server
        .client()
        .complete(request.clone(), RequestOptions::default())
        .await
        .unwrap();
    request.options.model = "other-model".into();
    server
        .client()
        .complete(request, RequestOptions::default())
        .await
        .unwrap();
    let captured = server.captured();
    let body = &captured[1].body;
    assert_eq!(body["system"], "system instructions");
    assert!(!body.to_string().contains("host_only"));
    assert_eq!(body["thinking"], json!({"type":"disabled"}));
    assert!(body.get("output_config").is_none());
    assert_eq!(
        body["tools"],
        json!([{"name":"echo","description":"echo text","input_schema":{"type":"object"}}])
    );
    assert_eq!(body["tool_choice"], json!({"type":"none"}));
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({"type":"thinking","thinking":"keep reasoning","signature":"sig-r"})
    );
    assert_eq!(
        body["messages"][1]["content"][1],
        call("call-1", "echo", r#"{"text":"ok"}"#)
    );
    assert_eq!(
        body["messages"][2]["content"],
        json!([
        {"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"known failure"}],"is_error":true},
        {"type":"text","text":"retry"}])
    );
    assert!(
        captured[2].body["messages"][1]["content"][0]
            .get("signature")
            .is_none()
    );
    assert_eq!(
        captured[2].body["messages"][1]["content"][0]["thinking"],
        "keep reasoning"
    );
}

#[tokio::test]
async fn endpoint_prefixes_and_thinking_efforts_are_explicit() {
    assert_eq!(
        ClientConfig::new("x").base_url,
        "https://api.deepseek.com/anthropic"
    );
    for (suffix, expected) in [
        ("/anthropic", "/anthropic/v1/messages"),
        ("/anthropic/", "/anthropic/v1/messages"),
        ("/proxy/v1/", "/proxy/v1/messages"),
    ] {
        let server =
            Server::start(vec![Reply::json(response("r", vec![message("m", "ok")]))]).await;
        let mut config = server.config();
        config.base_url = format!("{}{suffix}", server.url);
        DeepSeekClient::new(config)
            .unwrap()
            .complete(MessageRequest::new("hi"), RequestOptions::default())
            .await
            .unwrap();
        assert_eq!(server.captured()[0].path, expected);
    }
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ] {
        let server =
            Server::start(vec![Reply::json(response("r", vec![message("m", "ok")]))]).await;
        let mut request = MessageRequest::new("hi");
        request.options.reasoning = effort;
        server
            .client()
            .complete(request, RequestOptions::default())
            .await
            .unwrap();
        assert_eq!(
            server.captured()[0].body["output_config"]["effort"],
            json!(effort)
        );
        assert!(server.captured()[0].body.get("tools").is_none());
        assert!(server.captured()[0].body.get("tool_choice").is_none());
    }
}

#[tokio::test]
async fn nonstream_decode_rejects_wrong_protocol_blocks_and_stop_reasons() {
    let valid = response("r", vec![message("m", "ok")]);
    let mut invalid = vec![json!({"id":"r","status":"completed","output":[]})];
    for patch in [
        json!({"role":"user"}),
        json!({"stop_reason":null}),
        json!({"stop_reason":"tool_use"}),
        json!({"content":[{"type":"redacted_thinking","data":"x"}]}),
        json!({"usage":[]}),
        json!({"id":""}),
    ] {
        let mut response = valid.clone();
        response
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        invalid.push(response);
    }
    for wire in invalid {
        let server = Server::start(vec![Reply::json(wire)]).await;
        assert_eq!(
            server
                .client()
                .complete(MessageRequest::new("hi"), RequestOptions::default())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Protocol
        );
    }
    // An empty completed response is a transient provider defect, not a
    // protocol violation: its own kind lets the host retry it inside the turn.
    let mut empty = valid.clone();
    empty["content"] = json!([]);
    let server = Server::start(vec![Reply::json(empty)]).await;
    assert_eq!(
        server
            .client()
            .complete(MessageRequest::new("hi"), RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::EmptyResponse
    );
    for (reason, status) in [
        ("end_turn", ResponseStatus::Completed),
        ("stop_sequence", ResponseStatus::Completed),
        ("max_tokens", ResponseStatus::Incomplete),
    ] {
        let mut wire = valid.clone();
        wire["stop_reason"] = json!(reason);
        let server = Server::start(vec![Reply::json(wire)]).await;
        assert_eq!(
            server
                .client()
                .complete(MessageRequest::new("hi"), RequestOptions::default())
                .await
                .unwrap()
                .status,
            status
        );
    }
}

#[tokio::test]
async fn invalid_history_is_rejected_before_dispatch() {
    let server = Server::start(vec![]).await;
    for arguments in ["{", "[]", "null"] {
        let mut request = MessageRequest::new("hi");
        request.history.push(Item::FunctionCall {
            id: "local-id".into(),
            call_id: "call".into(),
            name: "echo".into(),
            arguments: arguments.into(),
        });
        request.history.push(Item::FunctionCallOutput {
            call_id: "call".into(),
            output: "x".into(),
            is_error: false,
            meta: None,
        });
        assert_eq!(
            server
                .client()
                .complete(request, RequestOptions::default())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Session
        );
    }
    assert!(server.captured().is_empty());
}

#[tokio::test]
async fn in_band_errors_are_classified_without_echoing_provider_text_or_retrying() {
    for (kind, expected) in [
        ("authentication_error", ErrorKind::Authentication),
        ("rate_limit_error", ErrorKind::RateLimit),
        ("overloaded_error", ErrorKind::Server),
        ("invalid_request_error", ErrorKind::InvalidRequest),
    ] {
        let error = json!({"type":"error","error":{"type":kind,"message":"fixture-secret prompt contents"}});
        let server = Server::start(vec![
            Reply::events(vec![message_start("m"), error.clone()]),
            Reply::json(error),
        ])
        .await;
        let error = collect(&server).await.unwrap_err();
        assert_eq!(error.kind, expected);
        assert!(!format!("{error:?}").contains("fixture-secret"));
        assert_eq!(server.captured().len(), 1);
        assert_eq!(
            server
                .client()
                .complete(MessageRequest::new("hi"), RequestOptions::default())
                .await
                .unwrap_err()
                .kind,
            expected
        );
    }
}

#[test]
fn legacy_snapshots_and_logs_are_refused_without_modification() {
    let snapshot = SessionSnapshot::new("system", ModelOptions::default());
    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.protocol, "deepseek-messages");
    let mut legacy = serde_json::to_value(&snapshot).unwrap();
    legacy["version"] = json!(1);
    legacy["protocol"] = json!("deepseek-responses");
    let legacy_model = legacy["model"].as_object_mut().unwrap();
    let limit = legacy_model.remove("max_tokens").unwrap();
    legacy_model.insert("max_output_tokens".into(), limit);
    legacy_model.insert("reasoning".into(), json!("none"));
    assert!(
        SessionSnapshot::from_json(&legacy.to_string())
            .unwrap_err()
            .to_string()
            .contains("legacy Responses")
    );
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path()).unwrap();
    let mut writer = store.create("legacy", &snapshot).unwrap();
    store.append_checkpoint(&mut writer, 0, &snapshot).unwrap();
    let path = writer.path().to_owned();
    drop(writer);
    let old = std::fs::read_to_string(&path)
        .unwrap()
        .replace("deepseek-messages", "deepseek-responses");
    std::fs::write(&path, &old).unwrap();
    assert!(store.load("legacy").is_err());
    assert!(store.create("legacy", &snapshot).is_err());
    assert!(store.list().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), old);
}

#[tokio::test]
async fn empty_tool_delta_keeps_initial_input_and_empty_results_stay_empty() {
    let mut events = vec![
        message_start("empty"),
        block_start(0, call("c", "echo", "{}")),
        delta(0, json!({"type":"input_json_delta","partial_json":""})),
        block_stop(0),
    ];
    events.extend(end("tool_use"));
    let server = Server::start(vec![
        Reply::events(events),
        Reply::json(response("final", vec![message("m", "ok")])),
    ])
    .await;
    let events = collect(&server).await.unwrap();
    let StreamEvent::Finished { response, .. } = events.last().unwrap() else {
        panic!("missing final message")
    };
    assert!(matches!(&response.output[0],Item::FunctionCall{arguments,..} if arguments=="{}"));
    let mut request = MessageRequest::new("hi");
    request.history.extend(response.output.clone());
    request.history.push(Item::FunctionCallOutput {
        call_id: "c".into(),
        output: String::new(),
        is_error: false,
        meta: None,
    });
    server
        .client()
        .complete(request, RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(
        server.captured()[1].body["messages"][2]["content"][0]["content"],
        json!([])
    );
}
