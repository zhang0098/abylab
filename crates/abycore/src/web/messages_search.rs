//! Auxiliary Messages requests using DeepSeek's native server-side search tool.
use super::*;
use crate::{
    ClientConfig, RequestPurpose, Usage, WebRequestRecord, deepseek::transport::Transport,
};
use std::collections::HashMap;

/// Independent Messages search settings. The base includes its version prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagesSearchConfig {
    /// Only `/messages` is appended; independent of chat and direct Search/Fetch.
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub max_uses: u32,
    /// Additional HTTP attempts; auxiliary model requests do not retry by default.
    pub max_retries: u32,
}

impl Default for MessagesSearchConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/anthropic/v1".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: 4096,
            max_uses: 5,
            max_retries: 0,
        }
    }
}

impl MessagesSearchConfig {
    pub fn validate(&self) -> Result<()> {
        let valid_url = reqwest::Url::parse(&self.base_url).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
        if !valid_url
            || self.model.trim().is_empty()
            || self.max_tokens == 0
            || self.max_uses == 0
            || self.max_retries > 10
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "invalid Messages search settings: HTTP(S) base without credentials, query or fragment, nonempty model, positive limits, retries 0..10",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DeepSeekMessagesSearch {
    transport: Transport,
    config: WebConfig,
}

impl DeepSeekMessagesSearch {
    pub fn new(key: &str, config: WebConfig) -> Result<Self> {
        config.validate()?;
        let mut client = ClientConfig::new(key);
        client.base_url = config.messages_search.base_url.clone();
        client.retry.max_retries = config.messages_search.max_retries;
        client.first_byte_timeout = Duration::from_secs(config.search_timeout_secs);
        Ok(Self {
            transport: Transport::new_search(client)?,
            config,
        })
    }

    pub async fn search(&self, query: &str, options: RequestOptions) -> Result<SearchResult> {
        let context = standalone_context(
            options,
            Duration::from_secs(self.config.search_timeout_secs),
        )?;
        self.search_with_context(query, context).await
    }

    pub(super) async fn search_with_context(
        &self,
        query: &str,
        mut context: RequestContext,
    ) -> Result<SearchResult> {
        if query.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "search query must not be blank",
            ));
        }
        let endpoint = self.transport.endpoint("messages")?.to_string();
        let settings = &self.config.messages_search;
        let body = json!({
            "model": settings.model,
            "max_tokens": settings.max_tokens,
            "messages": [{"role":"user","content":[{"type":"text","text":format!("Perform a web search for the query: {query}")}]}],
            "tools": [{"type":"web_search_20250305","name":"web_search","max_uses":settings.max_uses}],
            "thinking": {"type":"disabled"}
        });
        context.web = Some(WebRequestRecord {
            provider: "deepseek-messages-search".into(),
            operation: "search".into(),
            endpoint: endpoint.clone(),
            body: body.clone(),
        });
        let result = async {
            let (http, index) = self
                .transport
                .send("messages", &body, &context, RequestPurpose::WebSearch)
                .await?;
            let mut usage = None;
            let mut response_id = None;
            let result = async {
                let bytes = self.transport.body(http, &context).await?;
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::protocol("invalid Messages search JSON"))?;
                usage = value
                    .get("usage")
                    .filter(|v| v.is_object())
                    .map(Usage::from_wire);
                response_id = value["id"].as_str().map(str::to_owned);
                parse_search(&value, self.config.max_results, usage.clone())
            }
            .await;
            // Auxiliary model usage remains known even when the search result is invalid.
            context.record(
                index,
                response_id,
                if result.is_ok() {
                    "completed"
                } else {
                    "invalid_response"
                },
                usage,
            );
            result
        }
        .await;
        result.map_err(|mut error: Error| {
            error.message = self.transport.redact(&format!(
                "{} (Messages search endpoint: {endpoint}; configure web.messages_search.base_url independently of chat and web.api_base_url)",
                error.message,
            ));
            error
        })
    }
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn parse_search(value: &Value, max_results: usize, usage: Option<Usage>) -> Result<SearchResult> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(Error::new(
            ErrorKind::SearchUnavailable,
            "native search returned an error",
        ));
    }
    let blocks = value["content"]
        .as_array()
        .ok_or_else(|| Error::protocol("Messages search response has no content array"))?;
    let mut snippets = HashMap::new();
    for block in blocks.iter().filter(|block| block["type"] == "text") {
        for citation in block["citations"].as_array().into_iter().flatten() {
            if let (Some(url), Some(text)) = (
                nonempty(&citation["url"]),
                nonempty(&citation["cited_text"]),
            ) {
                snippets.entry(url).or_insert(text);
            }
        }
    }
    let mut saw_results = false;
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    let mut truncated = false;
    for block in blocks
        .iter()
        .filter(|block| block["type"] == "web_search_tool_result")
    {
        saw_results = true;
        let results = block["content"].as_array().ok_or_else(|| {
            Error::new(
                ErrorKind::SearchUnavailable,
                "native search returned an error or invalid result block",
            )
        })?;
        for result in results {
            if result["type"] == "web_search_tool_result_error" {
                return Err(Error::new(
                    ErrorKind::SearchUnavailable,
                    "native search tool failed",
                ));
            }
            if result["type"] != "web_search_result" {
                continue;
            }
            let Some(url) = nonempty(&result["url"]) else {
                continue;
            };
            validate_url(url)
                .map_err(|_| Error::protocol("native search source has an invalid URL"))?;
            if !seen.insert(url) {
                continue;
            }
            if sources.len() >= max_results {
                truncated = true;
                continue;
            }
            sources.push(SearchSource {
                url: url.into(),
                title: nonempty(&result["title"]).map(str::to_owned),
                snippet: snippets.get(url).map(|text| (*text).to_owned()),
                published_at: nonempty(&result["page_age"]).map(str::to_owned),
            });
        }
    }
    if !saw_results {
        return Err(Error::new(
            ErrorKind::SearchUnavailable,
            "no native web_search_tool_result blocks; Messages search support is unverified for this endpoint and model",
        ));
    }
    Ok(SearchResult {
        sources,
        truncated,
        usage,
    })
}
