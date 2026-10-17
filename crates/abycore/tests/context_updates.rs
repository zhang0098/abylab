mod common;
use abycore::*;
use common::*;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;

struct Echo(&'static str);
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.0.into(),
            description: "echo".into(),
            parameters: json!({"type":"object"}),
        }
    }
    fn validate(&self, _: &Value) -> std::result::Result<(), ToolError> {
        Ok(())
    }
    fn execute<'a>(&'a self, _: Value, _: ToolContext) -> ToolFuture<'a> {
        Box::pin(async { Ok(ToolOutput::text("ok")) })
    }
}
struct Context(&'static str);
impl AgentHooks for Context {
    fn request_context(&self) -> Option<&str> {
        Some(self.0)
    }
    fn checkpoint<'a>(
        &'a self,
        _: CheckpointKind,
        _: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
fn answer(id: &str) -> Reply {
    Reply::sse(response(id, vec![message("m", "ok")]))
}

#[tokio::test]
async fn addition_and_system_update_survive_restore_at_the_correct_boundary() {
    let server = Server::start(vec![answer("r1"), answer("r2"), answer("r3")]).await;
    let mut agent = Agent::new(server.client(), "old system", ModelOptions::default()).unwrap();
    agent.register_tool(Echo("one")).unwrap();
    agent.set_hooks(Arc::new(Context("old context")));
    agent
        .run("first", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    let mut snapshot = SessionSnapshot::from_json(&agent.snapshot().to_json().unwrap()).unwrap();
    snapshot.system_prompt = "new system".into();
    let mut agent = Agent::restore(server.client(), snapshot).unwrap();
    agent.register_tool(Echo("one")).unwrap();
    agent.register_tool(Echo("two")).unwrap();
    agent.set_hooks(Arc::new(Context("new context")));
    agent
        .run("second", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    agent
        .run("third", RunOptions::default(), |_| async { Ok(()) })
        .await
        .unwrap();
    let requests = server.captured();
    for index in [1, 2] {
        let body = &requests[index].body;
        assert_eq!(body["system"], "old system");
        assert_eq!(body["messages"][0]["content"][0]["text"], "old context");
        assert_eq!(body["messages"][2]["content"][0]["text"], "second");
        assert_eq!(body["messages"][3]["role"], "system");
        assert_eq!(body["messages"][3]["content"][0]["text"], "new system");
        assert_eq!(
            body["messages"][2]["content"][1]["text"],
            "Updated host context:\nnew context"
        );
        assert_eq!(
            body["messages"][3]["content"][1],
            json!({"type":"tool_addition","tool":{"type":"tool_reference","name":"two"}})
        );
        assert_eq!(body["tools"][1]["defer_loading"], true);
        assert!(body["tools"][0].get("defer_loading").is_none());
        assert!(
            requests[index]
                .headers
                .contains("mid-conversation-tool-changes-2026-07-01")
        );
    }
    assert_eq!(
        agent
            .snapshot()
            .requests
            .iter()
            .filter(|r| r.model_context.is_some())
            .count(),
        2,
        "unchanged configuration is not duplicated"
    );
}

#[tokio::test]
async fn unsupported_models_removal_and_compaction_use_full_current_prefix() {
    for mode in ["unknown", "removal", "compaction", "initially-empty"] {
        let server = Server::start(vec![answer("r1"), answer("r2")]).await;
        let mut agent = Agent::new(server.client(), "old", ModelOptions::default()).unwrap();
        if mode != "initially-empty" {
            agent.register_tool(Echo("one")).unwrap();
        }
        agent
            .run("first ".repeat(300), RunOptions::default(), |_| async {
                Ok(())
            })
            .await
            .unwrap();
        let mut snapshot = agent.snapshot();
        snapshot.system_prompt = "new".into();
        if mode == "unknown" {
            snapshot.model.model = "custom-route".into();
        }
        let mut agent = Agent::restore(server.client(), snapshot).unwrap();
        if mode != "removal" {
            agent.register_tool(Echo("one")).unwrap();
        }
        agent.register_tool(Echo("two")).unwrap();
        if mode == "compaction" {
            agent.compact(0, 2, "first turn complete").unwrap();
        }
        agent
            .run("second", RunOptions::default(), |_| async { Ok(()) })
            .await
            .unwrap();
        let request = &server.captured()[1];
        assert_eq!(request.body["system"], "new");
        assert!(!request.body.to_string().contains("tool_addition"));
        assert!(!request.headers.contains("mid-conversation"));
    }
}

#[test]
fn defaults_catalog_and_legacy_record_compatibility() {
    assert_eq!(
        ClientConfig::new("secret").stream_idle_timeout.as_secs(),
        300
    );
    assert_eq!(
        model_capabilities("deepseek-flash").unwrap().context_window,
        1_000_000
    );
    assert!(!model_capabilities("deepseek-v4-pro").unwrap().images);
    assert!(model_capabilities("custom-route").is_none());
    let record: RequestRecord = serde_json::from_value(
        json!({"purpose":"Conversation","attempt":1,"response_id":null,"status":null,"usage":null}),
    )
    .unwrap();
    assert!(record.model_context.is_none() && record.web.is_none() && record.resource.is_none());
}
