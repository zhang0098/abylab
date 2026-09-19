use abycore::{
    Agent, AgentEvent, ClientConfig, DeepSeekClient, DeepSeekWebSearch, ModelOptions, RunOptions,
    SearchConfig, StreamEvent,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("DEEPSEEK_API_KEY")?;
    let mut chat = ClientConfig::new(key.clone());
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        chat.base_url = url;
    }
    let client = DeepSeekClient::new(chat)?;
    let mut agent = Agent::new(
        client,
        "使用 web_search 查询，并在回答中列出来源链接。",
        ModelOptions::default(),
    )?;
    // Explicitly reuse the credential. The search endpoint is configured independently.
    let mut search = SearchConfig::new(key);
    if let Ok(url) = std::env::var("DEEPSEEK_SEARCH_BASE_URL") {
        search.client.base_url = url;
    }
    if let Ok(model) = std::env::var("DEEPSEEK_SEARCH_MODEL") {
        search.model = model;
    }
    agent.register_tool(DeepSeekWebSearch::new(search)?)?;
    agent
        .run(
            "查询 DeepSeek 官方最新 API 公告。",
            RunOptions::default(),
            |event| async move {
                if let AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) = event {
                    print!("{delta}");
                }
                Ok(())
            },
        )
        .await?;
    println!();
    Ok(())
}
