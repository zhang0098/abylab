mod common;
use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{Arc, Mutex};

fn image_request() -> MessageRequest {
    let mut request = MessageRequest::new("describe");
    request.history = vec![Item::user_parts(vec![ContentPart::InputImage {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    }])];
    request
}
fn client(server: &Server) -> DeepSeekClient {
    let mut config = server.config();
    config.files.enabled = true;
    DeepSeekClient::new(config).unwrap()
}
fn file(id: &str) -> Reply {
    Reply::json(json!({"id":id}))
}
fn answer() -> Reply {
    Reply::json(response("r", vec![message("m", "red")]))
}

#[tokio::test]
async fn multipart_upload_cache_beta_and_original_history() {
    let server = Server::start(vec![
        file("file-1"),
        answer(),
        answer(),
        file("file-2"),
        answer(),
    ])
    .await;
    let client = client(&server);
    let original = image_request();
    for _ in 0..2 {
        client
            .complete(original.clone(), RequestOptions::default())
            .await
            .unwrap();
    }
    assert!(
        matches!(&original.history[0], Item::Message { content, .. } if matches!(&content[0], ContentPart::InputImage { data, .. } if data == "YWJj"))
    );
    // A new credential/client gets a separate cache; file ids never cross it.
    let mut rotated = server.config();
    rotated.api_key = "rotated-secret".into();
    rotated.files.enabled = true;
    DeepSeekClient::new(rotated)
        .unwrap()
        .complete(original, RequestOptions::default())
        .await
        .unwrap();
    let requests = server.captured();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].path, "/gateway/v1/files");
    let headers = requests[0].headers.to_ascii_lowercase();
    assert!(headers.contains("multipart/form-data"));
    assert!(headers.contains("anthropic-beta: files-api-2025-04-14"));
    assert!(!headers.contains("authorization:"));
    let form = String::from_utf8_lossy(&requests[0].raw_body);
    for text in [
        "name=\"file\"",
        "name=\"expires_after[anchor]\"",
        "created_at",
        "name=\"expires_after[seconds]\"",
        "604800",
        "abc",
    ] {
        assert!(form.contains(text), "missing {text}");
    }
    for index in [1, 2] {
        assert_eq!(
            requests[index].body["messages"][0]["content"][0]["source"],
            json!({"type":"file","file_id":"file-1"})
        );
        assert!(requests[index].headers.contains("files-api-2025-04-14"));
    }
    assert_eq!(
        requests[4].body["messages"][0]["content"][0]["source"]["file_id"],
        "file-2"
    );
}

#[tokio::test]
async fn stale_file_falls_back_inline_once_and_evicts_cache() {
    let server = Server::start(vec![
        file("file-old"),
        Reply::raw(
            404,
            "application/json",
            json!({"error":{"message":"file not found"}}).to_string(),
        ),
        answer(),
        file("file-new"),
        answer(),
    ])
    .await;
    let client = client(&server);
    for _ in 0..2 {
        client
            .complete(image_request(), RequestOptions::default())
            .await
            .unwrap();
    }
    let requests = server.captured();
    assert_eq!(
        requests[2].body["messages"][0]["content"][0]["source"]["type"],
        "base64"
    );
    assert!(!requests[2].headers.contains("files-api"));
    assert_eq!(requests[3].path, "/gateway/v1/files");
}

#[tokio::test]
async fn proactive_expiry_and_upload_failure_fallback() {
    let server = Server::start(vec![
        Reply::json(json!({"id":"old", "created_at":1})),
        answer(),
        file("fresh"),
        answer(),
    ])
    .await;
    let client = client(&server);
    for _ in 0..2 {
        client
            .complete(image_request(), RequestOptions::default())
            .await
            .unwrap();
    }
    assert_eq!(server.captured()[2].path, "/gateway/v1/files");
    let server = Server::start(vec![Reply::raw(503, "text/plain", "unavailable"), answer()]).await;
    self::client(&server)
        .complete(image_request(), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(
        server.captured()[1].body["messages"][0]["content"][0]["source"]["type"],
        "base64"
    );
}

#[tokio::test]
async fn auth_limits_and_unsupported_model_do_not_fallback() {
    let server = Server::start(vec![Reply::raw(401, "application/json", "{}")]).await;
    assert_eq!(
        client(&server)
            .complete(image_request(), RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Authentication
    );
    assert_eq!(server.captured().len(), 1);
    let server = Server::start(vec![]).await;
    let mut request = image_request();
    request.options.model = "deepseek-v4-pro".into();
    assert_eq!(
        client(&server)
            .complete(request, RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
    let mut config = server.config();
    config.files.max_request_bytes = 2;
    assert_eq!(
        DeepSeekClient::new(config)
            .unwrap()
            .complete(image_request(), RequestOptions::default())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
    assert!(server.captured().is_empty());
}

#[derive(Default)]
struct Hooks {
    saved: Mutex<Vec<SessionSnapshot>>,
    fail: bool,
}
impl AgentHooks for Hooks {
    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if kind == CheckpointKind::ModelRequest && self.fail {
                return Err(Error::new(ErrorKind::Session, "disk full"));
            }
            self.saved.lock().unwrap().push(snapshot);
            Ok(())
        })
    }
}

#[tokio::test]
async fn upload_uses_budget_and_persistence_barrier_without_token_usage() {
    let server = Server::start(vec![file("file-1")]).await;
    let hooks = Arc::new(Hooks::default());
    let mut agent = Agent::new(client(&server), "test", ModelOptions::default()).unwrap();
    agent.set_hooks(hooks.clone());
    let parts = vec![ContentPart::InputImage {
        media_type: "image/png".into(),
        data: "YWJj".into(),
    }];
    let error = agent
        .run_parts(
            parts.clone(),
            RunOptions {
                max_requests: 1,
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::BudgetExceeded);
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.requests.len(), 1);
    assert_eq!(
        snapshot.requests[0].resource.as_deref(),
        Some("file_upload")
    );
    assert!(snapshot.requests[0].usage.is_none());
    assert!(snapshot.requests[0].model_context.is_none());
    assert!(hooks.saved.lock().unwrap().iter().any(|s| {
        s.requests
            .last()
            .is_some_and(|r| r.resource.is_some() && r.status.is_none())
    }));
    assert_eq!(server.captured().len(), 1);
    let server = Server::start(vec![]).await;
    let mut agent = Agent::new(client(&server), "test", ModelOptions::default()).unwrap();
    agent.set_hooks(Arc::new(Hooks {
        fail: true,
        ..Default::default()
    }));
    assert!(
        agent
            .run_parts(parts, RunOptions::default(), |_| async { Ok(()) })
            .await
            .is_err()
    );
    assert!(server.captured().is_empty());
    assert_eq!(
        agent.snapshot().requests[0].status.as_deref(),
        Some("not_dispatched")
    );
}
