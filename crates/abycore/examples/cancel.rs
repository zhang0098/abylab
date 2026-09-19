use abycore::{Agent, ClientConfig, DeepSeekClient, ModelOptions, RunOptions};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let mut agent = Agent::new(client, "", ModelOptions::default())?;
    let options = RunOptions::default();
    let cancellation = options.cancellation.clone();
    let result = {
        let run = agent.run("写一篇长文。", options, |_| async { Ok(()) });
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => result,
            _ = tokio::time::sleep(Duration::from_secs(2)) => { cancellation.cancel(); run.await }
        }
    };
    println!(
        "结果：{:?}；待解决调用：{}",
        result.map(|r| r.stop_reason),
        agent.snapshot().pending.len()
    );
    Ok(())
}
