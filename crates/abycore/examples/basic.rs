use abycore::{ClientConfig, DeepSeekClient, MessageRequest, RequestOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let response = client
        .complete(
            MessageRequest::new("你好，请用一句话介绍自己。"),
            RequestOptions::default(),
        )
        .await?;
    println!("{}", response.output_text());
    Ok(())
}
