//! Host context stays outside compactable/durable history, but is included in
//! every request and its byte measurement, including summaries and children.
mod common;

use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use std::sync::Arc;

struct Context(&'static str);
impl AgentHooks for Context {
    fn request_context(&self) -> Option<&str> {
        Some(self.0)
    }
    fn checkpoint<'a>(
        &'a self,
        _: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { snapshot.validate() })
    }
}

async fn ignore(_: AgentEvent) -> Result<()> {
    Ok(())
}

#[tokio::test]
async fn context_is_measured_survives_compaction_and_is_reloaded_on_restore() {
    let server = Server::start(vec![
        Reply::sse(response("first", vec![message("a", "done")])),
        Reply::sse(response("summary", vec![message("s", "brief summary")])),
        Reply::sse(response("second", vec![message("b", "done")])),
        Reply::sse(response("third", vec![message("c", "done")])),
    ])
    .await;
    let mut agent = Agent::new(server.client(), "system", ModelOptions::default()).unwrap();
    let without = agent.context_estimate().unwrap();
    agent.set_hooks(Arc::new(Context("HOST_RULE_OLD")));
    let with = agent.context_estimate().unwrap();
    assert!(with.request_bytes >= without.request_bytes + "HOST_RULE_OLD".len());
    assert_eq!(with.history_bytes, without.history_bytes);
    agent
        .run("long user task ".repeat(200), RunOptions::default(), ignore)
        .await
        .unwrap();
    let end = agent.snapshot().items.len();
    let summary = agent
        .summarize_span(0, end, SummarizeOptions::default())
        .await
        .unwrap();
    agent.compact(0, end, summary.summary).unwrap();
    agent
        .run("follow up", RunOptions::default(), ignore)
        .await
        .unwrap();
    let snapshot = agent.snapshot();
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("HOST_RULE_OLD")
    );
    let mut resumed = Agent::restore(server.client(), snapshot).unwrap();
    resumed.set_hooks(Arc::new(Context("HOST_RULE_NEW")));
    assert!(
        resumed
            .context_estimate()
            .unwrap()
            .last_input_tokens
            .is_none()
    );
    resumed
        .run("resumed task", RunOptions::default(), ignore)
        .await
        .unwrap();
    let requests = server.captured();
    assert_eq!(requests.len(), 4);
    for request in &requests[..3] {
        assert_eq!(
            request.body["messages"]
                .to_string()
                .matches("HOST_RULE_OLD")
                .count(),
            1
        );
        assert!(!request.body["system"].to_string().contains("HOST_RULE_OLD"));
        assert_eq!(request.body["messages"][0]["role"], "user");
    }
    assert!(
        requests[2].body["messages"]
            .to_string()
            .contains("brief summary")
    );
    let last = requests[3].body.to_string();
    assert!(last.contains("HOST_RULE_NEW"));
    assert!(!last.contains("HOST_RULE_OLD"));
}

#[tokio::test]
async fn spawned_and_forked_children_inherit_context_once_and_can_override_it() {
    let server = Server::start(
        (0..3)
            .map(|_| Reply::sse(response("child", vec![message("m", "done")])))
            .collect(),
    )
    .await;
    let mut parent = Agent::new(server.client(), "system", ModelOptions::default()).unwrap();
    parent.set_hooks(Arc::new(Context("PARENT_RULE")));
    let inherited = Subagents::new();
    for mode in [SubagentMode::Spawn, SubagentMode::Fork] {
        let mut request = SubagentRequest::new("child", "task");
        request.mode = mode;
        let child = inherited.start(&parent, request).unwrap();
        inherited.wait(&child.id).await.unwrap();
    }
    let overridden = Subagents::with_config(SubagentConfig {
        hooks: Some(Arc::new(|_| Ok(Arc::new(Context("CHILD_RULE"))))),
        ..Default::default()
    })
    .unwrap();
    let child = overridden
        .start(&parent, SubagentRequest::new("child", "task"))
        .unwrap();
    overridden.wait(&child.id).await.unwrap();
    let requests = server.captured();
    assert_eq!(requests.len(), 3);
    for request in &requests[..2] {
        assert_eq!(request.body.to_string().matches("PARENT_RULE").count(), 1);
    }
    assert!(requests[2].body.to_string().contains("CHILD_RULE"));
    assert!(!requests[2].body.to_string().contains("PARENT_RULE"));
    inherited.shutdown().await;
    overridden.shutdown().await;
}
