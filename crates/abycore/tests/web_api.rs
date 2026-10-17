#![cfg(feature = "web-search")]
mod common;
use abycore::*;
use common::*;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn results(url: &str) -> Value {
    json!({"results":[{"url":url,"title":"Title","snippet":"Excerpt","date":null,"summary":"Long summary","content":"","authority":3,"rerank_score":2.1}]})
}
fn config(server: &Server) -> WebConfig {
    WebConfig {
        search_provider: SearchProvider::Api,
        api_base_url: format!("{}/gateway/v1/tools/", server.url),
        max_retries: 0,
        ..Default::default()
    }
}

fn native_result(urls: &[&str]) -> Value {
    json!({
        "id":"native-search", "type":"message", "role":"assistant", "stop_reason":"end_turn",
        "content":[
            {"type":"server_tool_use","id":"server-search","name":"web_search","input":{"query":"q"}},
            {"type":"web_search_tool_result","tool_use_id":"server-search","content":urls.iter().map(|url| json!({"type":"web_search_result","url":url,"title":"Native title","page_age":"today"})).collect::<Vec<_>>()},
            {"type":"text","text":"Generated answer is not a search result","citations":urls.iter().map(|url| json!({"url":url,"cited_text":"Cited excerpt"})).collect::<Vec<_>>()}
        ],
        "usage":{"input_tokens":10,"cache_read_input_tokens":2,"output_tokens":3}
    })
}

fn native_config(server: &Server) -> WebConfig {
    WebConfig {
        messages_search: MessagesSearchConfig {
            base_url: format!("{}/native/v1/", server.url),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn messages_is_the_default_and_old_settings_pick_it_without_migration() {
    for value in [
        json!({}),
        json!({"api_base_url":"https://example.org/tools"}),
    ] {
        let config: WebConfig = serde_json::from_value(value).unwrap();
        assert_eq!(config.search_provider, SearchProvider::Messages);
        assert_eq!(config.messages_search, MessagesSearchConfig::default());
        config.validate().unwrap();
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["search_provider"], "messages");
        assert_eq!(serde_json::from_value::<WebConfig>(saved).unwrap(), config);
    }
    let config: WebConfig = serde_json::from_value(json!({"search_provider":"api"})).unwrap();
    assert_eq!(config.search_provider, SearchProvider::Api);
    for value in [
        json!({"search_provider":"fallback"}),
        json!({"messages_search":{"unknown":true}}),
    ] {
        assert!(serde_json::from_value::<WebConfig>(value).is_err());
    }
    for value in [
        json!({"messages_search":{"base_url":"https://user:secret@example.org"}}),
        json!({"messages_search":{"base_url":"file:///tmp/search"}}),
        json!({"messages_search":{"model":" "}}),
        json!({"messages_search":{"max_tokens":0}}),
        json!({"messages_search":{"max_uses":0}}),
        json!({"messages_search":{"max_retries":11}}),
    ] {
        assert!(
            serde_json::from_value::<WebConfig>(value)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[tokio::test]
async fn native_search_uses_messages_headers_server_tools_and_structured_sources() {
    let native = Server::start(vec![Reply::json(native_result(&[
        "https://example.org/a",
        "https://example.org/a",
        "https://example.org/b",
    ]))])
    .await;
    let mut cfg = native_config(&native);
    cfg.max_results = 1;
    let api = DeepSeekMessagesSearch::new("fixture-secret", cfg).unwrap();
    let result = api.search("q", RequestOptions::default()).await.unwrap();
    assert_eq!(result.sources.len(), 1);
    assert_eq!(result.sources[0].url, "https://example.org/a");
    assert_eq!(result.sources[0].snippet.as_deref(), Some("Cited excerpt"));
    assert_eq!(result.sources[0].published_at.as_deref(), Some("today"));
    assert!(result.truncated);
    assert_eq!(result.usage.as_ref().unwrap().input_tokens, Some(12));
    let request = &native.captured()[0];
    assert_eq!(request.path, "/native/v1/messages");
    assert!(request.headers.contains("x-api-key: fixture-secret"));
    assert!(
        request
            .headers
            .contains("authorization: Bearer fixture-secret")
    );
    assert!(request.headers.contains("anthropic-version: 2023-06-01"));
    assert!(request.headers.contains("accept: application/json"));
    assert_eq!(request.body["model"], "deepseek-v4-flash");
    assert_eq!(request.body["max_tokens"], 4096);
    assert_eq!(
        request.body["tools"],
        json!([{"type":"web_search_20250305","name":"web_search","max_uses":5}])
    );
    assert_eq!(request.body["thinking"], json!({"type":"disabled"}));
    assert_eq!(
        request.body["messages"][0]["content"][0]["text"],
        "Perform a web search for the query: q"
    );
    assert!(request.body.get("ori_question").is_none());
}

#[tokio::test]
async fn default_tool_uses_native_search_but_fetch_still_uses_direct_api() {
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let saved = snapshots.clone();
    let native = Server::start_with_handler(move |request| {
        assert!(
            saved
                .lock()
                .unwrap()
                .iter()
                .any(|s: &SessionSnapshot| s.requests.iter().any(|r| {
                    r.web.as_ref().is_some_and(|w| {
                        w.provider == "deepseek-messages-search" && w.body == request.body
                    })
                }))
        );
        Reply::json(native_result(&["https://example.org/a"]))
    })
    .await;
    let direct = Server::start(vec![Reply::json(
        json!({"title":"Page","content":"# Page body"}),
    )])
    .await;
    let chat = Server::start(vec![
        Reply::sse(response(
            "s",
            vec![call("s", "web_search", r#"{"queries":["q","q"]}"#)],
        )),
        Reply::sse(response(
            "f",
            vec![call("f", "web_fetch", r#"{"url":"https://example.org/a"}"#)],
        )),
        Reply::sse(response("done", vec![message("m", "done")])),
    ])
    .await;
    let mut cfg = native_config(&native);
    cfg.api_base_url = format!("{}/tools", direct.url);
    let mut agent = Agent::new(chat.client(), "system", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Save {
        snapshots,
        reject_web: false,
    }));
    agent
        .register_tools(
            cfg.tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let result = agent
        .run("user question", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(result.requests.len(), 5);
    assert_eq!(native.captured().len(), 1);
    assert_eq!(direct.captured()[0].path, "/tools/fetch");
    assert_eq!(direct.captured().len(), 1);
    assert_eq!(result.requests[1].purpose, RequestPurpose::WebSearch);
    assert_eq!(
        result.requests[1].usage.as_ref().unwrap().output_tokens,
        Some(3)
    );
    assert!(result.requests[3].usage.is_none());
    assert!(
        chat.captured()[1]
            .body
            .to_string()
            .contains("Cited excerpt")
    );
    let snapshot = agent.snapshot();
    let saved = snapshot.to_json().unwrap();
    assert!(!saved.contains("fixture-secret"));
    assert_eq!(SessionSnapshot::from_json(&saved).unwrap(), snapshot);
}

#[tokio::test]
async fn selected_search_backend_never_falls_back_and_native_has_no_default_retry() {
    let native =
        Server::start_with_handler(|_| Reply::raw(503, "text/plain", "fixture-secret")).await;
    let direct =
        Server::start_with_handler(|_| Reply::raw(503, "text/plain", "fixture-secret")).await;
    let mut cfg = native_config(&native);
    cfg.api_base_url = format!("{}/tools", direct.url);
    cfg.max_retries = 0;
    let (result, snapshot) = run_tool(
        cfg.clone(),
        "web_search",
        json!({"queries":["q"]}),
        RunOptions::default(),
    )
    .await;
    result.unwrap();
    assert_eq!(native.captured().len(), 1);
    assert!(direct.captured().is_empty());
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| matches!(item, Item::FunctionCallOutput { is_error: true, .. }))
    );
    assert!(!snapshot.to_json().unwrap().contains("fixture-secret"));
    cfg.search_provider = SearchProvider::Api;
    let (result, _) = run_tool(
        cfg,
        "web_search",
        json!({"queries":["q"]}),
        RunOptions::default(),
    )
    .await;
    result.unwrap();
    assert_eq!(native.captured().len(), 1);
    assert_eq!(direct.captured().len(), 1);
}

#[tokio::test]
async fn native_search_rejects_prose_and_bad_blocks_but_preserves_usage() {
    for content in [
        json!([{"type":"text","text":"I found https://example.org"}]),
        json!([{"type":"web_search_tool_result","content":{"type":"web_search_tool_result_error","error_code":"fixture-secret"}}]),
        json!([{"type":"web_search_tool_result","content":[{"type":"web_search_tool_result_error"}]}]),
        json!([{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":"file:///etc/passwd"}]}]),
    ] {
        let mut response = native_result(&[]);
        response["content"] = content;
        let native = Server::start(vec![Reply::json(response)]).await;
        let (result, snapshot) = run_tool(
            native_config(&native),
            "web_search",
            json!({"queries":["q"]}),
            RunOptions::default(),
        )
        .await;
        result.unwrap();
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| matches!(item, Item::FunctionCallOutput { is_error: true, .. }))
        );
        let record = &snapshot.requests[1];
        assert_eq!(record.status.as_deref(), Some("invalid_response"));
        assert_eq!(record.usage.as_ref().unwrap().input_tokens, Some(12));
        assert!(!snapshot.to_json().unwrap().contains("fixture-secret"));
    }
    let native = Server::start(vec![Reply::json(native_result(&[]))]).await;
    assert!(
        DeepSeekMessagesSearch::new("fixture-secret", native_config(&native))
            .unwrap()
            .search("q", RequestOptions::default())
            .await
            .unwrap()
            .sources
            .is_empty()
    );
}

#[tokio::test]
async fn native_search_rejects_redirects_and_honors_cancellation() {
    let target = Server::start(vec![]).await;
    let mut redirect = Reply::raw(302, "text/plain", "");
    redirect
        .headers
        .push(("location".into(), target.url.clone()));
    let native = Server::start(vec![redirect]).await;
    let client = DeepSeekMessagesSearch::new("fixture-secret", native_config(&native)).unwrap();
    assert!(client.search("q", RequestOptions::default()).await.is_err());
    assert!(target.captured().is_empty());
    let native = Server::start_with_handler(|_| {
        let mut reply = Reply::json(native_result(&[]));
        reply.chunks[0].0 = Duration::from_secs(10);
        reply
    })
    .await;
    let client = DeepSeekMessagesSearch::new("fixture-secret", native_config(&native)).unwrap();
    let cancellation = CancellationToken::new();
    let search = client.search(
        "q",
        RequestOptions {
            cancellation: cancellation.clone(),
            timeout: None,
        },
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancellation.cancel();
    };
    let (result, _) = tokio::join!(search, cancel);
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
}

#[tokio::test]
async fn native_search_dispatch_requires_persistence_and_budget_and_accepts_long_queries() {
    let native = Server::start(vec![]).await;
    let cfg = native_config(&native);
    let tools = cfg
        .tools("fixture-secret", WebRateLimiter::default())
        .unwrap();
    assert!(
        tools[0]
            .validate(&json!({"queries":["q".repeat(129)]}))
            .is_ok()
    );
    assert!(tools[0].validate(&json!({"queries":[" "]})).is_err());
    let chat = Server::start(vec![
        Reply::sse(response(
            "s",
            vec![call("s", "web_search", r#"{"queries":["q"]}"#)],
        )),
        Reply::sse(response("done", vec![message("m", "failed")])),
    ])
    .await;
    let mut agent = Agent::new(chat.client(), "", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Save {
        snapshots: Arc::default(),
        reject_web: true,
    }));
    agent.register_tools(tools).unwrap();
    let result = agent
        .run("q", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert_eq!(result.requests[1].status.as_deref(), Some("not_dispatched"));
    assert!(native.captured().is_empty());
    let (_, snapshot) = run_tool(
        cfg,
        "web_search",
        json!({"queries":["q"]}),
        RunOptions {
            max_requests: 1,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(snapshot.requests.len(), 1);
    assert!(native.captured().is_empty());
}

#[tokio::test]
async fn direct_json_routes_and_headers_match_the_official_api() {
    let server = Server::start(vec![
        Reply::json(results("https://example.org/a")),
        Reply::json(json!({"title":"Page","content":"# Markdown"})),
    ])
    .await;
    let api = DeepSeekWebApi::new("fixture-secret", config(&server)).unwrap();
    let question = format!("  用户\n{}   ", "🙂".repeat(130));
    let search = api
        .search(
            "DeepSeek site:github.com",
            Some(&question),
            RequestOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(search.sources.len(), 1);
    assert_eq!(search.usage, None);
    let page = api
        .fetch("https://example.org/a", RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(page.url, "https://example.org/a");
    assert_eq!(page.content, "# Markdown");
    let requests = server.captured();
    assert_eq!(requests[0].path, "/gateway/v1/tools/search");
    assert_eq!(requests[1].path, "/gateway/v1/tools/fetch");
    assert_eq!(requests[0].body["count"], 8);
    assert_eq!(requests[0].body["need_content"], false);
    assert_eq!(
        requests[0].body["ori_question"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        128
    );
    assert_eq!(requests[1].body, json!({"url":"https://example.org/a"}));
    for request in requests {
        assert!(
            request
                .headers
                .contains("authorization: Bearer fixture-secret")
        );
        assert!(!request.headers.contains("x-api-key"));
        assert!(!request.headers.contains("anthropic-version"));
        assert!(request.body.get("model").is_none());
        assert!(request.body.get("tools").is_none());
    }
}

#[tokio::test]
async fn query_and_url_validation_happen_before_dispatch() {
    let server = Server::start(vec![Reply::json(json!({"results":[]}))]).await;
    let mut cfg = config(&server);
    cfg.send_original_question = false;
    let api = DeepSeekWebApi::new("fixture-secret", cfg.clone()).unwrap();
    for query in [" ".into(), "字".repeat(129)] {
        assert_eq!(
            api.search(&query, None, RequestOptions::default())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidRequest
        );
    }
    for url in [
        "file:///etc/passwd",
        "https://user:password@example.org",
        "http:///",
        "",
    ] {
        assert_eq!(
            api.fetch(url, RequestOptions::default())
                .await
                .unwrap_err()
                .kind,
            ErrorKind::InvalidRequest
        );
    }
    let result = api
        .search(
            &"🙂".repeat(128),
            Some("private original question"),
            RequestOptions::default(),
        )
        .await
        .unwrap();
    assert!(result.sources.is_empty());
    assert!(server.captured()[0].body.get("ori_question").is_none());
    let tools = cfg
        .tools("fixture-secret", WebRateLimiter::default())
        .unwrap();
    assert!(
        tools[0]
            .validate(&json!({"queries":["x".repeat(129)]}))
            .is_err()
    );
    assert!(
        tools[1]
            .validate(&json!({"url":"https://example.org","extra":true}))
            .is_err()
    );
    assert_eq!(server.captured().len(), 1);
}

#[tokio::test]
async fn provider_errors_have_operation_specific_classification_and_finite_retries() {
    for (status, kind, attempts) in [
        (400, ErrorKind::InvalidRequest, 1),
        (401, ErrorKind::Authentication, 1),
        (402, ErrorKind::Quota, 1),
        (403, ErrorKind::AccessDenied, 1),
        (404, ErrorKind::WebUnavailable, 1),
        (408, ErrorKind::Timeout, 2),
        (413, ErrorKind::InvalidRequest, 1),
        (429, ErrorKind::RateLimit, 2),
        (504, ErrorKind::Server, 2),
    ] {
        let server = Server::start_with_handler(move |_| {
            let mut r = Reply::raw(status, "text/plain", "fixture-secret must never leak");
            r.headers.push(("retry-after".into(), "0".into()));
            r
        })
        .await;
        let mut cfg = config(&server);
        cfg.max_retries = 1;
        let error = DeepSeekWebApi::new("fixture-secret", cfg)
            .unwrap()
            .fetch("https://example.org", RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind, "{status}");
        assert_eq!(server.captured().len(), attempts, "{status}");
        assert!(!error.to_string().contains("fixture-secret"));
    }
}

#[tokio::test]
async fn redirect_and_malformed_responses_are_never_successes() {
    let target = Server::start(vec![]).await;
    let mut redirect = Reply::raw(302, "text/plain", "");
    redirect
        .headers
        .push(("location".into(), format!("{}/steal", target.url)));
    let server = Server::start(vec![
        redirect,
        Reply::json(json!({"results":[{"url":"https://example.org","title":"T","snippet":"S"}]})),
        Reply::json(json!({"title":"empty","content":""})),
    ])
    .await;
    let api = DeepSeekWebApi::new("fixture-secret", config(&server)).unwrap();
    assert_eq!(
        api.search("q", None, RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Protocol
    );
    assert!(target.captured().is_empty());
    assert_eq!(
        api.search("q", None, RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Protocol
    );
    assert_eq!(
        api.fetch("https://example.org", RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::WebUnavailable
    );
}

struct Save {
    snapshots: Arc<Mutex<Vec<SessionSnapshot>>>,
    reject_web: bool,
}
impl AgentHooks for Save {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.reject_web && kind == CheckpointKind::WebRequest {
                return Err(Error::new(ErrorKind::Session, "write failed"));
            }
            self.snapshots.lock().unwrap().push(snapshot);
            Ok(())
        })
    }
}

#[tokio::test]
async fn agent_search_fetch_replay_records_every_dispatch_without_token_usage() {
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let saved = snapshots.clone();
    let web = Server::start_with_handler(move |r| {
        assert!(
            saved
                .lock()
                .unwrap()
                .iter()
                .any(|s: &SessionSnapshot| s.requests.iter().any(|r| r.web.is_some()))
        );
        if r.path.ends_with("/search") {
            Reply::json(results("https://example.org/a"))
        } else {
            Reply::json(json!({"title":"Page","content":"# Body"}))
        }
    })
    .await;
    let chat = Server::start(vec![
        Reply::sse(response(
            "s",
            vec![call("s", "web_search", r#"{"queries":["first","second"]}"#)],
        )),
        Reply::sse(response(
            "f",
            vec![call("f", "web_fetch", r#"{"url":"https://example.org/a"}"#)],
        )),
        Reply::sse(response(
            "done",
            vec![message("m", "[Page](https://example.org/a)")],
        )),
    ])
    .await;
    let mut agent = Agent::new(chat.client(), "system", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Save {
        snapshots,
        reject_web: false,
    }));
    agent
        .register_tools(
            config(&web)
                .tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let outcome = agent
        .run("  user question  ", RunOptions::default(), |_| async {
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.requests.len(), 6);
    let auxiliary: Vec<_> = outcome
        .requests
        .iter()
        .filter(|r| r.web.is_some())
        .collect();
    assert_eq!(auxiliary.len(), 3);
    assert!(auxiliary.iter().all(|r| r.usage.is_none()));
    assert_eq!(auxiliary[2].web.as_ref().unwrap().operation, "fetch");
    for r in &web.captured()[..2] {
        assert_eq!(r.body["ori_question"], "user question");
    }
    let snapshot = agent.snapshot();
    let encoded = snapshot.to_json().unwrap();
    assert!(!encoded.contains("fixture-secret"));
    assert_eq!(SessionSnapshot::from_json(&encoded).unwrap(), snapshot);
    // Additive request metadata remains readable when old logs omit it.
    let mut old: Value = serde_json::from_str(&encoded).unwrap();
    for record in old["requests"].as_array_mut().unwrap() {
        record.as_object_mut().unwrap().remove("web");
    }
    assert!(SessionSnapshot::from_json(&old.to_string()).is_ok());
}

#[tokio::test]
async fn persistence_failure_prevents_auxiliary_network_dispatch() {
    let web = Server::start(vec![]).await;
    let chat = Server::start(vec![
        Reply::sse(response(
            "s",
            vec![call("s", "web_search", r#"{"queries":["q"]}"#)],
        )),
        Reply::sse(response("done", vec![message("m", "failed")])),
    ])
    .await;
    let mut agent = Agent::new(chat.client(), "", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Save {
        snapshots: Arc::default(),
        reject_web: true,
    }));
    agent
        .register_tools(
            config(&web)
                .tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let outcome = agent
        .run("q", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    assert!(web.captured().is_empty());
    assert!(
        outcome
            .new_items
            .iter()
            .any(|i| matches!(i, Item::FunctionCallOutput { is_error: true, .. }))
    );
    assert_eq!(
        outcome.requests[1].status.as_deref(),
        Some("not_dispatched")
    );
}

#[tokio::test]
async fn cancellation_interrupts_waiting_for_rate_limit_and_response_body() {
    let server = Server::start_with_handler(|_| Reply::json(json!({"results":[]}))).await;
    let mut cfg = config(&server);
    cfg.requests_per_second = 1;
    let api = DeepSeekWebApi::new("fixture-secret", cfg).unwrap();
    api.search("q", None, RequestOptions::default())
        .await
        .unwrap();
    let token = CancellationToken::new();
    let task = api.search(
        "next",
        None,
        RequestOptions {
            cancellation: token.clone(),
            timeout: None,
        },
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        token.cancel();
    };
    let (result, _) = tokio::join!(task, cancel);
    assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
    assert_eq!(server.captured().len(), 1);
    let server = Server::start_with_handler(|_| {
        let mut reply = Reply::json(json!({"title":"page","content":"body"}));
        reply.chunks[0].0 = Duration::from_secs(10);
        reply
    })
    .await;
    let api = DeepSeekWebApi::new("fixture-secret", config(&server)).unwrap();
    let result = api
        .fetch(
            "https://example.org",
            RequestOptions {
                timeout: Some(Duration::from_millis(30)),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(result.unwrap_err().kind, ErrorKind::Timeout);
}

async fn run_tool(
    config: WebConfig,
    name: &str,
    args: Value,
    options: RunOptions,
) -> (Result<RunOutcome>, SessionSnapshot) {
    let chat = Server::start(vec![
        Reply::sse(response(
            "tool",
            vec![call("call", name, &args.to_string())],
        )),
        Reply::sse(response("final", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = Agent::new(chat.client(), "test", ModelOptions::default()).unwrap();
    agent
        .register_tools(
            config
                .tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let result = agent
        .run("original question", options, |_| async { Ok(()) })
        .await;
    (result, agent.snapshot())
}

#[tokio::test]
async fn batch_deduplicates_round_robins_and_keeps_replay_metadata() {
    let server = Server::start_with_handler(|r| {
        let q = r.body["query"].as_str().unwrap();
        let urls = if q == "a" { vec!["a1", "shared", "a3"] } else { vec!["b1", "shared", "b3"] };
        let mut reply = Reply::json(json!({"results": urls.iter().map(|url| json!({"url":format!("https://example.org/{url}"),"title":url,"snippet":"text","date":"2026-10-17"})).collect::<Vec<_>>()}));
        if q == "a" { reply.header_delay = Duration::from_millis(400); }
        reply
    }).await;
    let mut cfg = config(&server);
    cfg.max_results = 4;
    let (result, snapshot) = run_tool(
        cfg,
        "web_search",
        json!({"queries":["a","b","a"]}),
        RunOptions::default(),
    )
    .await;
    assert_eq!(result.unwrap().stop_reason, StopReason::Completed);
    let meta = snapshot
        .items
        .iter()
        .find_map(|item| match item {
            Item::FunctionCallOutput {
                meta,
                is_error: false,
                ..
            } => meta.as_ref(),
            _ => None,
        })
        .unwrap();
    let urls: Vec<_> = meta["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        [
            "https://example.org/a1",
            "https://example.org/b1",
            "https://example.org/shared",
            "https://example.org/a3"
        ]
    );
    assert_eq!(meta["truncated"], true);
    assert_eq!(server.captured().len(), 2);
}

#[tokio::test]
async fn batch_failure_discards_partial_success_and_stops_waiting_siblings() {
    let server = Server::start_with_handler(|r| match r.body["query"].as_str().unwrap() {
        "success" => Reply::json(results("https://example.org/partial")),
        "failure" => Reply::raw(400, "application/json", "{}"),
        _ => {
            let mut reply = Reply::json(results("https://example.org/slow"));
            reply.header_delay = Duration::from_secs(20);
            reply
        }
    })
    .await;
    let (result, _) = tokio::time::timeout(
        Duration::from_secs(3),
        run_tool(
            config(&server),
            "web_search",
            json!({"queries":["success","failure","slow"]}),
            RunOptions::default(),
        ),
    )
    .await
    .unwrap();
    let outcome = result.unwrap();
    assert!(outcome.new_items.iter().any(|item| matches!(item, Item::FunctionCallOutput { is_error:true, output, .. } if !output.contains("partial"))));
    assert_eq!(
        server.captured().len(),
        2,
        "cancelled rate-gated sibling was never dispatched"
    );
}

#[tokio::test]
async fn fetch_is_unicode_bounded_and_request_budget_blocks_web_dispatch() {
    let server = Server::start(vec![Reply::json(
        json!({"title":"Page","content":"🙂正文".repeat(2000)}),
    )])
    .await;
    let mut cfg = config(&server);
    cfg.max_output_bytes = 512;
    let (result, _) = run_tool(
        cfg,
        "web_fetch",
        json!({"url":"https://example.org"}),
        RunOptions::default(),
    )
    .await;
    assert!(result.unwrap().new_items.iter().any(|item| matches!(item, Item::FunctionCallOutput { is_error:false, output, meta:Some(meta), .. } if output.len() <= 512 && meta["truncated"] == true && output.contains("https://example.org"))));
    let server = Server::start(vec![]).await;
    let (result, snapshot) = run_tool(
        config(&server),
        "web_search",
        json!({"queries":["q"]}),
        RunOptions {
            max_requests: 1,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(result.unwrap_err().kind, ErrorKind::BudgetExceeded);
    assert_eq!(snapshot.requests.len(), 1);
    assert!(server.captured().is_empty());
}

#[tokio::test]
async fn separate_clients_share_search_rate_gate_but_fetch_is_independent() {
    let server = Server::start_with_handler(|r| {
        if r.path.ends_with("/search") {
            Reply::json(json!({"results":[]}))
        } else {
            Reply::json(json!({"title":"P","content":"C"}))
        }
    })
    .await;
    let mut cfg = config(&server);
    cfg.requests_per_second = 1;
    let gate = WebRateLimiter::default();
    let first = DeepSeekWebApi::with_limiter("first-key", cfg.clone(), gate.clone()).unwrap();
    let second = DeepSeekWebApi::with_limiter("second-key", cfg, gate).unwrap();
    first
        .search("first", None, RequestOptions::default())
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let (search, fetch) = tokio::join!(
        second.search("second", None, RequestOptions::default()),
        tokio::time::timeout(
            Duration::from_millis(700),
            second.fetch("https://example.org", RequestOptions::default())
        )
    );
    search.unwrap();
    fetch.unwrap().unwrap();
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert!(server.captured()[2].headers.contains("Bearer second-key"));
}

struct Reminder;
impl AgentHooks for Reminder {
    fn checkpoint<'a>(
        &'a self,
        _: CheckpointKind,
        _: SessionSnapshot,
    ) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn request_context(&self) -> Option<&str> {
        Some("workspace instructions, not a question")
    }
    fn tool_reminder<'a>(
        &'a self,
        _: &'a PendingCall,
        _: &'a ToolOutput,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(Some("host reminder, not a question".into())) })
    }
}

#[tokio::test]
async fn children_inherit_direct_tools_but_keep_questions_and_ledgers_isolated() {
    let times = Arc::new(Mutex::new(Vec::new()));
    let observed = times.clone();
    let web = Server::start_with_handler(move |_| {
        observed.lock().unwrap().push(tokio::time::Instant::now());
        Reply::json(json!({"results":[]}))
    })
    .await;
    let chat = Server::start_with_handler(|request| {
        if request.body["messages"].to_string().contains("tool_result") {
            Reply::sse(response("done", vec![message("m", "done")]))
        } else {
            Reply::sse(response(
                "search",
                vec![call("call", "web_search", r#"{"queries":["q"]}"#)],
            ))
        }
    })
    .await;
    let mut parent = Agent::new(chat.client(), "parent", ModelOptions::default()).unwrap();
    parent
        .register_tools(
            config(&web)
                .tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let children = Subagents::with_config(SubagentConfig {
        run_options: RunOptions {
            max_requests: 3,
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let a = children
        .start(&parent, SubagentRequest::new("a", "child A"))
        .unwrap();
    let b = children
        .start(&parent, SubagentRequest::new("b", "child B"))
        .unwrap();
    children.wait(&a.id).await.unwrap();
    children.wait(&b.id).await.unwrap();
    assert!(parent.snapshot().requests.is_empty());
    let mut questions: Vec<_> = web
        .captured()
        .into_iter()
        .map(|r| r.body["ori_question"].as_str().unwrap().to_owned())
        .collect();
    questions.sort();
    assert_eq!(questions, ["child A", "child B"]);
    {
        let times = times.lock().unwrap();
        assert_eq!(times.len(), 2);
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(190));
    }
    children.shutdown().await;
}

#[tokio::test]
async fn original_question_tracks_user_steers_but_not_host_reminders() {
    let web = Server::start_with_handler(|_| Reply::json(json!({"results":[]}))).await;
    let chat = Server::start(vec![
        Reply::sse(response(
            "one",
            vec![call("one", "web_search", r#"{"queries":["q"]}"#)],
        )),
        Reply::sse(response(
            "two",
            vec![call("two", "web_search", r#"{"queries":["q"]}"#)],
        )),
        Reply::sse(response(
            "three",
            vec![call("three", "web_search", r#"{"queries":["q"]}"#)],
        )),
        Reply::sse(response("end", vec![message("m", "done")])),
    ])
    .await;
    let mut agent = Agent::new(chat.client(), "system", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Reminder));
    agent
        .register_tools(
            config(&web)
                .tools("fixture-secret", WebRateLimiter::default())
                .unwrap(),
        )
        .unwrap();
    let steer = agent.steer_handle();
    agent
        .run("first question", RunOptions::default(), move |event| {
            if matches!(event, AgentEvent::ToolFinished { ref call_id, .. } if call_id == "one") {
                steer.send("new user question").unwrap();
            }
            async { Ok(()) }
        })
        .await
        .unwrap();
    let questions: Vec<_> = web
        .captured()
        .into_iter()
        .map(|r| r.body["ori_question"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        questions,
        ["first question", "new user question", "new user question"]
    );
}
