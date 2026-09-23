mod common;
use abycore::*;
use common::*;
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn complete_and_stream_preserve_wire_contract() {
    let wire = response(
        "r1",
        vec![reasoning("think1", "考虑中文"), message("msg1", "你好")],
    );
    let server = Server::start(vec![Reply::json(wire.clone()), Reply::sse(wire)]).await;
    let client = server.client();
    let mut request = MessageRequest::new("你好");
    request.options.reasoning = ReasoningEffort::Max;
    let complete = client
        .complete(request.clone(), RequestOptions::default())
        .await
        .unwrap();
    let mut stream = client
        .stream(request, RequestOptions::default())
        .await
        .unwrap();
    let mut streamed = None;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Finished { response, .. } = event.unwrap() {
            streamed = Some(*response);
        }
    }
    assert_eq!(Some(complete.clone()), streamed);
    assert_eq!(complete.output_text(), "你好");
    let usage = complete.usage.unwrap();
    assert_eq!(usage.total_tokens, Some(14));
    assert_eq!(usage.cached_tokens, Some(3));
    for request in server.captured() {
        assert_eq!(request.path, "/gateway/v1/messages");
        assert!(request.headers.contains("x-api-key: fixture-secret"));
        assert!(request.headers.contains("anthropic-version: 2023-06-01"));
        assert!(
            !request
                .headers
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        assert_eq!(request.body["thinking"]["type"], "enabled");
        assert_eq!(request.body["output_config"]["effort"], "max");
        assert_eq!(request.body["messages"][0]["content"][0]["text"], "你好");
        assert_eq!(request.body["max_tokens"], 256_000);
        for unsupported in [
            "input",
            "instructions",
            "reasoning",
            "max_output_tokens",
            "reasoning_effort",
            "store",
            "previous_response_id",
            "max_tool_calls",
        ] {
            assert!(request.body.get(unsupported).is_none());
        }
    }
}

#[tokio::test]
async fn retries_classifies_and_redacts_http_failures() {
    let mut rate = Reply::raw(429, "text/plain", "not json");
    rate.headers.push(("retry-after".into(), "0".into()));
    let server = Server::start(vec![
        rate,
        Reply::json(response("ok", vec![message("m", "ok")])),
    ])
    .await;
    let mut config = server.config();
    config.retry.max_retries = 1;
    DeepSeekClient::new(config)
        .unwrap()
        .complete(MessageRequest::new("x"), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(server.captured().len(), 2);
    for (status, code, kind) in [
        (401, "invalid_api_key", ErrorKind::Authentication),
        (402, "balance", ErrorKind::Quota),
        (
            400,
            "context_length_exceeded",
            ErrorKind::ContextLimitExceeded,
        ),
        (422, "invalid_request", ErrorKind::InvalidRequest),
    ] {
        let mut reply = Reply::raw(
            status,
            "application/json",
            json!({"error":{"code":code,"message":"fixture-secret"}}).to_string(),
        );
        reply
            .headers
            .push(("x-request-id".into(), "id-fixture-secret".into()));
        let server = Server::start(vec![reply]).await;
        let mut config = server.config();
        config.retry.max_retries = 2;
        let client = DeepSeekClient::new(config).unwrap();
        let error = client
            .complete(MessageRequest::new("x"), RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.status, Some(status));
        assert_eq!(error.request_id.as_deref(), Some("id-[REDACTED]"));
        assert!(!format!("{error:?} {client:?}").contains("fixture-secret"));
        assert_eq!(server.captured().len(), 1);
    }
}

#[tokio::test]
async fn retry_after_never_retries_before_server_minimum() {
    for delay in [
        "30".to_owned(),
        httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(60)),
    ] {
        let mut reply = Reply::raw(429, "text/plain", "wait");
        reply.headers.push(("retry-after".into(), delay));
        let server = Server::start(vec![reply]).await;
        let mut config = server.config();
        config.retry.max_retries = 2;
        let error = DeepSeekClient::new(config)
            .unwrap()
            .complete(MessageRequest::new("x"), RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::RateLimit);
        assert!(error.retry_after.unwrap() > Duration::from_secs(10));
        assert_eq!(server.captured().len(), 1);
    }
}

#[tokio::test]
async fn redirects_never_receive_credentials() {
    let destination = Server::start(vec![]).await;
    for status in [301, 302, 303, 307, 308] {
        let mut reply = Reply::raw(status, "text/plain", "redirect");
        reply
            .headers
            .push(("location".into(), destination.url.clone()));
        let server = Server::start(vec![reply]).await;
        let error = server
            .client()
            .complete(MessageRequest::new("x"), RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Protocol);
        assert_eq!(error.status, Some(status));
    }
    assert!(destination.captured().is_empty());
}

#[tokio::test]
async fn stream_disconnect_done_marker_and_wrong_types_fail_closed() {
    let created = message_start("r");
    for tail in [
        "",
        "data: [DONE]\n\n",
        "event: wrong\ndata: {\"type\":\"ping\"}\n\n",
    ] {
        let server = Server::start(vec![Reply::raw(
            200,
            "text/event-stream",
            format!("{}{tail}", frame(&created)),
        )])
        .await;
        let mut config = server.config();
        config.retry.max_retries = 2;
        let mut stream = DeepSeekClient::new(config)
            .unwrap()
            .stream(MessageRequest::new("x"), RequestOptions::default())
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamEvent::Started { .. }
        ));
        let kind = stream.next().await.unwrap().unwrap_err().kind;
        assert!(matches!(
            kind,
            ErrorKind::Protocol | ErrorKind::StreamClosed
        ));
        assert_eq!(server.captured().len(), 1);
    }
}

#[tokio::test]
async fn idle_timeout_counts_network_wait_not_consumer_delay() {
    let created = message_start("r");
    let remaining = message_events(response("r", vec![message("m", "ok")]))
        .iter()
        .skip(1)
        .map(frame)
        .collect::<String>();
    let mut reply = Reply::raw(200, "text/event-stream", "");
    reply.chunks = vec![
        (Duration::ZERO, frame(&created).into_bytes()),
        (Duration::from_millis(15), b": heartbeat\n\n".to_vec()),
        (Duration::from_millis(15), remaining.into_bytes()),
    ];
    let server = Server::start(vec![reply]).await;
    let mut config = server.config();
    config.stream_idle_timeout = Duration::from_millis(100);
    let mut stream = DeepSeekClient::new(config)
        .unwrap()
        .stream(MessageRequest::new("x"), RequestOptions::default())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(180)).await;
    let mut finished = false;
    while let Some(event) = stream.next().await {
        finished |= matches!(event.unwrap(), StreamEvent::Finished { .. });
    }
    assert!(finished);
}

#[tokio::test]
async fn cancellation_first_byte_idle_timeouts_and_the_run_window() {
    let mut reply = Reply::json(response("r", vec![message("m", "ok")]));
    reply.header_delay = Duration::from_secs(1);
    let server = Server::start(vec![reply]).await;
    let mut config = server.config();
    config.first_byte_timeout = Duration::from_millis(30);
    let error = DeepSeekClient::new(config)
        .unwrap()
        .complete(MessageRequest::new("x"), RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Timeout);
    let token = CancellationToken::new();
    token.cancel();
    let error = server
        .client()
        .complete(
            MessageRequest::new("x"),
            RequestOptions {
                cancellation: token,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Cancelled);
    assert_eq!(server.captured().len(), 1);
    // A window is a gap between two moments of progress, not a cap on how long
    // a stream may take: whichever of the two bounds is reached first, a stream
    // that says nothing for a whole window has stalled.
    for window in [false, true] {
        let mut reply = Reply::raw(200, "text/event-stream", "");
        reply.chunks = vec![
            (Duration::ZERO, frame(&message_start("r")).into_bytes()),
            (Duration::from_secs(1), b": wait\n\n".to_vec()),
        ];
        let server = Server::start(vec![reply]).await;
        let mut config = server.config();
        config.stream_idle_timeout = if window {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(30)
        };
        let mut options = RequestOptions::default();
        if window {
            options.timeout = Duration::from_millis(30);
        }
        let mut stream = DeepSeekClient::new(config)
            .unwrap()
            .stream(MessageRequest::new("x"), options)
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().kind,
            ErrorKind::Timeout
        );
    }
    // The same window never cuts a response that keeps producing events: every
    // event re-arms it, so a slow answer is a working run.
    let events = message_events(response("r", vec![message("m", "ok")]));
    let mut reply = Reply::raw(200, "text/event-stream", "");
    reply.chunks = events
        .iter()
        .map(|event| (Duration::from_millis(20), frame(event).into_bytes()))
        .collect();
    let server = Server::start(vec![reply]).await;
    let mut stream = server
        .client()
        .stream(
            MessageRequest::new("x"),
            RequestOptions {
                timeout: Duration::from_millis(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut seen = 0usize;
    while let Some(event) = stream.next().await {
        event.expect("every frame of a progressing stream arrives");
        seen += 1;
    }
    assert!(seen > 1, "the stream ran to its end past the window");
}

#[tokio::test]
async fn slow_authentication_error_body_keeps_status_and_is_not_retried() {
    let mut reply = Reply::raw(401, "application/json", "");
    reply.chunks = vec![(Duration::from_millis(200), b"{}".to_vec())];
    let server = Server::start(vec![reply]).await;
    let mut config = server.config();
    config.stream_idle_timeout = Duration::from_millis(30);
    config.retry.max_retries = 2;
    let error = DeepSeekClient::new(config)
        .unwrap()
        .complete(MessageRequest::new("x"), RequestOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Authentication);
    assert_eq!(error.status, Some(401));
    assert_eq!(server.captured().len(), 1);
}

#[tokio::test]
async fn models_lists_the_provider_catalog_from_the_origin() {
    let server = Server::start(vec![Reply::json(json!({
        "object": "list",
        "data": [
            {"id": "deepseek-flash", "object": "model", "owned_by": "deepseek"},
            {"id": "deepseek-v4-pro", "object": "model", "owned_by": "deepseek"},
            {"id": 7}
        ]
    }))])
    .await;
    let models = server.client().models().await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["deepseek-flash", "deepseek-v4-pro"]
    );
    assert_eq!(models[0].owned_by.as_deref(), Some("deepseek"));
    let request = &server.captured()[0];
    // The Messages suffix (fixture base is `/gateway/v1`) is stripped and the
    // listing rides the OpenAI shape with bearer auth.
    assert_eq!(request.path, "/gateway/models");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-secret")
    );
}
