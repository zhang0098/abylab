//! Paid network probes: ignored by default and require host-provided credentials.
use abycore::*;
use futures_util::StreamExt;

fn config() -> ClientConfig {
    let mut config = ClientConfig::new(
        std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly"),
    );
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.base_url = url;
    }
    config
}
fn model() -> String {
    std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".into())
}
fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
#[ignore = "paid Messages probe; requires DEEPSEEK_API_KEY"]
async fn messages_live() {
    let config = config();
    eprintln!(
        "Messages probe: endpoint={}, model={}, unix_time={}",
        config.base_url,
        model(),
        timestamp()
    );
    let client = DeepSeekClient::new(config).unwrap();
    let mut request = MessageRequest::new("Reply with exactly OK.");
    request.options.model = model();
    let result = client
        .complete(request.clone(), RequestOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ResponseStatus::Completed);
    assert!(!result.output_text().is_empty());
    let mut stream = client
        .stream(request, RequestOptions::default())
        .await
        .unwrap();
    let mut completed = false;
    while let Some(event) = stream.next().await {
        if let StreamEvent::Finished { response, .. } = event.unwrap() {
            assert_eq!(response.status, ResponseStatus::Completed);
            completed = true;
        }
    }
    assert!(completed);
    eprintln!("Messages probe: verified (complete and stream)");
}

#[cfg(feature = "web-search")]
#[tokio::test]
#[ignore = "paid native search probe; verified independently of conversation"]
async fn search_live() {
    let mut config = SearchConfig::new(
        std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly"),
    );
    if let Ok(url) = std::env::var("DEEPSEEK_SEARCH_BASE_URL") {
        config.client.base_url = url;
    }
    if let Ok(model) = std::env::var("DEEPSEEK_SEARCH_MODEL") {
        config.model = model;
    }
    eprintln!(
        "Search probe: endpoint={}, model={}, unix_time={}",
        config.client.base_url,
        config.model,
        timestamp()
    );
    let result = DeepSeekWebSearch::new(config)
        .unwrap()
        .search(
            "DeepSeek official API documentation",
            RequestOptions::default(),
        )
        .await;
    match result {
        Ok(result) => {
            assert!(!result.sources.is_empty());
            eprintln!("Search probe: verified, {} sources", result.sources.len());
        }
        Err(error) => panic!("Search probe: unavailable or failed: {error}"),
    }
}
