mod support;
use abycore::{
    Agent, AgentEvent, ClientConfig, DeepSeekClient, ModelOptions, RunOptions, StreamEvent,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let mut agent = Agent::new(client, "使用工具处理用户的文字。", ModelOptions::default())?;
    agent.register_tool(support::Uppercase)?;
    let outcome = agent
        .run(
            "请调用 uppercase 处理 hello, world",
            RunOptions::default(),
            |event| async move {
                if let AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) = event {
                    print!("{delta}");
                }
                Ok(())
            },
        )
        .await?;
    println!(
        "\n停止原因：{:?}，请求数：{}",
        outcome.stop_reason,
        outcome.requests.len()
    );
    Ok(())
}
