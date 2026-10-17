//! Direct DeepSeek Search/Fetch JSON APIs, sharing a credential snapshot and rate limits.
use super::*;
use crate::{
    ClientConfig, RequestPurpose, SearchSource, WebRequestRecord, deepseek::transport::Transport,
};
use tokio::{sync::Mutex, time::Instant};

#[derive(Clone, Debug, Default)]
pub struct WebRateLimiter {
    search: Arc<Mutex<Option<Instant>>>,
    fetch: Arc<Mutex<Option<Instant>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResult {
    pub url: String,
    pub title: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct DeepSeekWebApi {
    transport: Transport,
    config: WebConfig,
    limiter: WebRateLimiter,
}

impl DeepSeekWebApi {
    pub fn new(key: &str, config: WebConfig) -> Result<Self> {
        Self::with_limiter(key, config, WebRateLimiter::default())
    }
    pub fn with_limiter(key: &str, config: WebConfig, limiter: WebRateLimiter) -> Result<Self> {
        config.validate()?;
        let mut client = ClientConfig::new(key);
        client.base_url = config.api_base_url.clone();
        client.retry.max_retries = config.max_retries;
        client.first_byte_timeout =
            Duration::from_secs(config.search_timeout_secs.max(config.fetch_timeout_secs));
        Ok(Self {
            transport: Transport::new_web(client)?,
            config,
            limiter,
        })
    }

    pub async fn search(
        &self,
        query: &str,
        question: Option<&str>,
        options: RequestOptions,
    ) -> Result<SearchResult> {
        let context = standalone_context(
            options,
            Duration::from_secs(self.config.search_timeout_secs),
        )?;
        self.search_with_context(query, question, context).await
    }
    pub async fn fetch(&self, url: &str, options: RequestOptions) -> Result<FetchResult> {
        let context =
            standalone_context(options, Duration::from_secs(self.config.fetch_timeout_secs))?;
        self.fetch_with_context(url, context).await
    }

    async fn dispatch<T>(
        &self,
        operation: &str,
        body: Value,
        mut context: RequestContext,
        decode: impl FnOnce(Value) -> Result<T> + Send,
    ) -> Result<T> {
        let endpoint = self.transport.endpoint(operation)?.to_string();
        context.web = Some(WebRequestRecord {
            provider: format!("deepseek-{operation}-api"),
            operation: operation.into(),
            endpoint: endpoint.clone(),
            body: body.clone(),
        });
        let gate = if operation == "search" {
            self.limiter.search.clone()
        } else {
            self.limiter.fetch.clone()
        };
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.config.requests_per_second));
        context.dispatch_gate = Some(Arc::new(move || {
            let gate = gate.clone();
            Box::pin(async move {
                let mut next = gate.lock().await;
                if let Some(deadline) = *next {
                    tokio::time::sleep_until(deadline).await;
                }
                *next = Some(Instant::now() + interval);
                Ok(())
            })
        }));
        let (http, index) = self.transport.send(operation, &body, &context, RequestPurpose::WebSearch).await.map_err(|mut error| {
            error.message = format!("{} (web endpoint: {endpoint}; configure web.api_base_url independently of chat)", error.message);
            error
        })?;
        let result = async {
            let bytes = self.transport.body(http, &context).await?;
            let value = serde_json::from_slice(&bytes)
                .map_err(|_| Error::protocol("invalid web API JSON"))?;
            decode(value)
        }
        .await;
        context.record(
            index,
            None,
            if result.is_ok() {
                "completed"
            } else {
                "invalid_response"
            },
            None,
        );
        result.map_err(|mut error: Error| {
            error.message = format!(
                "{} (web endpoint: {endpoint}; configure web.api_base_url independently of chat)",
                error.message
            );
            error
        })
    }

    pub(super) async fn search_with_context(
        &self,
        query: &str,
        question: Option<&str>,
        context: RequestContext,
    ) -> Result<SearchResult> {
        if query.trim().is_empty() || query.chars().count() > 128 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "search query must contain 1..128 Unicode characters and not be blank",
            ));
        }
        let mut body = json!({"query":query,"count":self.config.count.min(self.config.max_results),"need_content":false});
        if self.config.send_original_question {
            let question = question
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(128)
                .collect::<String>();
            if !question.is_empty() {
                body["ori_question"] = json!(question);
            }
        }
        self.dispatch("search", body, context, |value| {
            let results = value["results"]
                .as_array()
                .ok_or_else(|| Error::protocol("search response has no results array"))?;
            let mut sources = Vec::new();
            for result in results {
                let required = |field| {
                    result[field]
                        .as_str()
                        .ok_or_else(|| Error::protocol("invalid search result fields"))
                };
                let url = required("url")?;
                super::validate_url(url)
                    .map_err(|_| Error::protocol("invalid search source URL"))?;
                let title = required("title")?;
                let snippet = required("snippet")?;
                let date = result
                    .get("date")
                    .ok_or_else(|| Error::protocol("search result has no date"))?;
                if !date.is_null() && !date.is_string() {
                    return Err(Error::protocol("invalid search publication date"));
                }
                sources.push(SearchSource {
                    url: url.into(),
                    title: (!title.is_empty()).then(|| title.into()),
                    snippet: (!snippet.is_empty()).then(|| snippet.into()),
                    published_at: date.as_str().filter(|v| !v.is_empty()).map(str::to_owned),
                });
            }
            let truncated = sources.len() > self.config.max_results;
            sources.truncate(self.config.max_results);
            Ok(SearchResult {
                sources,
                truncated,
                usage: None,
            })
        })
        .await
    }

    pub(super) async fn fetch_with_context(
        &self,
        url: &str,
        context: RequestContext,
    ) -> Result<FetchResult> {
        super::validate_url(url)?;
        self.dispatch("fetch", json!({"url":url}), context, |value| {
            let title = value["title"]
                .as_str()
                .ok_or_else(|| Error::protocol("fetch response has no title"))?;
            let content = value["content"]
                .as_str()
                .ok_or_else(|| Error::protocol("fetch response has no Markdown content"))?;
            if content.trim().is_empty() {
                return Err(Error::new(
                    ErrorKind::WebUnavailable,
                    "fetch returned no usable content",
                ));
            }
            Ok(FetchResult {
                url: url.into(),
                title: title.into(),
                content: content.into(),
            })
        })
        .await
    }
}
