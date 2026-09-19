use abycore::{
    Agent, AgentEvent, ClientConfig, DeepSeekClient, ModelOptions, RunOptions, StreamEvent,
    Subagents,
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?);
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.base_url = url;
    }
    let mut model = ModelOptions::default();
    if let Ok(name) = std::env::var("DEEPSEEK_MODEL") {
        model.model = name;
    }
    let mut agent = Agent::new(
        DeepSeekClient::new(config)?,
        "你可以用 subagent 委派独立任务。并行任务使用 run_in_background，使用 wait_agent 收集结果后汇总。",
        model,
    )?;
    // Register any LocalTools/custom tools before granting delegation; children inherit them.
    let subagents = Subagents::new();
    subagents.register(&mut agent)?;
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "用两个后台子代理分别分析归并排序的时间复杂度和空间复杂度，等待结果后给出简短总结。".into()
    } else {
        prompt
    };
    let result = agent
        .run(
            prompt,
            RunOptions {
                tool_timeout: Duration::from_secs(300),
                ..Default::default()
            },
            |event| async move {
                if let AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) = event {
                    print!("{delta}");
                }
                Ok(())
            },
        )
        .await;
    // Cleanup is also required on the error path. Background work otherwise survives the turn.
    subagents.shutdown().await;
    for child in subagents.list() {
        eprintln!(
            "{} ({}) {:?}",
            child.id,
            child.description,
            child.result.as_ref().map(|result| &result.stop_reason)
        );
    }
    let outcome = result?;
    println!("\n{:?}", outcome.stop_reason);
    Ok(())
}
