//! Explicit paid probe of tool execution and local snapshot replay.
#[path = "../examples/support/mod.rs"]
mod support;
use abycore::*;

#[tokio::test]
#[ignore = "paid agent and snapshot probe; requires DEEPSEEK_API_KEY"]
async fn tools_and_snapshot_live() {
    let mut config = ClientConfig::new(
        std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly"),
    );
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.base_url = url;
    }
    let model = ModelOptions {
        model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        ..Default::default()
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    eprintln!(
        "Agent probe: endpoint={}, model={}, unix_time={timestamp}",
        config.base_url, model.model
    );
    let client = DeepSeekClient::new(config).unwrap();
    let mut agent = Agent::new(
        client.clone(),
        "You must call uppercase when asked to convert text. Return its result.",
        model,
    )
    .unwrap();
    agent.register_tool(support::Uppercase).unwrap();
    let outcome = agent
        .run(
            "Call uppercase with text abycore_smoke. Return its result.",
            RunOptions {
                max_requests: 4,
                max_tool_calls: 2,
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let snapshot = agent.snapshot();
    assert!(snapshot.items.iter().any(
        |item| matches!(item, Item::FunctionCallOutput { output, .. } if output == "ABYCORE_SMOKE")
    ));
    let mut restored = Agent::restore(
        client,
        SessionSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
    )
    .unwrap();
    restored.register_tool(support::Uppercase).unwrap();
    let outcome = restored
        .run(
            "Repeat the previous uppercase result. No new tool call is necessary.",
            RunOptions {
                max_requests: 4,
                max_tool_calls: 2,
                ..Default::default()
            },
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(outcome.response.output_text().contains("ABYCORE_SMOKE"));
    eprintln!(
        "Agent probe: verified (function call, execution, output replay, JSON restore, next turn)"
    );
}
