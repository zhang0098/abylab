use crate::{ClientConfig, Error, ErrorKind, RequestPurpose, Result, context::RequestContext};
use futures_util::StreamExt;
use reqwest::{Url, header::HeaderMap};
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Clone)]
pub(crate) struct Transport {
    client: reqwest::Client,
    config: Arc<ClientConfig>,
    root: Url,
    protocol: Protocol,
}

#[derive(Clone)]
enum Protocol {
    Messages,
    #[cfg(feature = "web-search")]
    MessagesSearch,
    Web,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport").finish_non_exhaustive()
    }
}

impl Transport {
    pub fn new(config: ClientConfig) -> Result<Self> {
        Self::build(config, Protocol::Messages)
    }

    #[cfg(feature = "web-search")]
    pub fn new_web(config: ClientConfig) -> Result<Self> {
        Self::build(config, Protocol::Web)
    }

    #[cfg(feature = "web-search")]
    pub fn new_search(config: ClientConfig) -> Result<Self> {
        Self::build(config, Protocol::MessagesSearch)
    }

    fn build(config: ClientConfig, protocol: Protocol) -> Result<Self> {
        config.validate()?;
        let mut root = Url::parse(&config.base_url)
            .map_err(|_| Error::new(ErrorKind::Configuration, "invalid API base URL"))?;
        if !matches!(root.scheme(), "http" | "https")
            || root.host_str().is_none()
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "base URL must be HTTP(S), without credentials, query or fragment",
            ));
        }
        let prefix = root.path().trim_end_matches('/');
        let path = if !matches!(protocol, Protocol::Messages) || prefix.ends_with("/v1") {
            format!("{prefix}/")
        } else {
            format!("{prefix}/v1/")
        };
        root.set_path(&path);
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout);
        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| {
                Error::new(ErrorKind::Configuration, "invalid proxy configuration")
            })?);
        }
        let client = builder
            .build()
            .map_err(|_| Error::new(ErrorKind::Configuration, "cannot initialize HTTP client"))?;
        Ok(Self {
            client,
            config: Arc::new(config),
            root,
            protocol,
        })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }
    pub fn redact(&self, text: &str) -> String {
        text.replace(&self.config.api_key, "[REDACTED]")
    }

    pub fn endpoint(&self, path: &str) -> Result<Url> {
        self.root
            .join(path)
            .map_err(|_| Error::new(ErrorKind::Configuration, "invalid endpoint path"))
    }

    pub async fn upload_image(
        &self,
        data: Vec<u8>,
        media_type: &str,
        context: &RequestContext,
    ) -> Result<Value> {
        let mut context = context.clone();
        context.resource = Some("file_upload".into());
        context.model_context = None;
        let index = context.reserve(RequestPurpose::Conversation, 1, Some(data.len()))?;
        if let Err(error) = context.before_dispatch().await {
            context.record(index, None, "not_dispatched", None);
            return Err(error);
        }
        let result = async {
            let part = reqwest::multipart::Part::bytes(data)
                .file_name("abylab-image")
                .mime_str(media_type)
                .map_err(|_| Error::protocol("invalid image media type"))?;
            let form = reqwest::multipart::Form::new()
                .text("expires_after[anchor]", "created_at")
                .text(
                    "expires_after[seconds]",
                    self.config.files.expires_after_secs.to_string(),
                )
                .part("file", part);
            let response = context
                .timed(
                    Duration::from_secs(self.config.files.timeout_secs),
                    self.client
                        .post(self.endpoint("files")?)
                        .header("x-api-key", &self.config.api_key)
                        .header("anthropic-version", "2023-06-01")
                        .header("anthropic-beta", "files-api-2025-04-14")
                        .multipart(form)
                        .send(),
                )
                .await?
                .map_err(|_| Error::new(ErrorKind::Transport, "Files API upload failed"))?;
            if !response.status().is_success() {
                return Err(self.http_error(response, &context).await);
            }
            let bytes = self.body(response, &context).await?;
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::protocol("invalid Files API response"))
        }
        .await;
        context.record(
            index,
            None,
            if result.is_ok() {
                "uploaded"
            } else {
                "upload_failed"
            },
            None,
        );
        result
    }

    /// `GET {origin}/models`: the provider's advertised model listing in the
    /// OpenAI shape. Best-effort — one dispatch with the configured timeouts,
    /// no retry loop and no request-budget accounting.
    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>> {
        let url = self.models_url()?;
        let request = self
            .client
            .get(url)
            .bearer_auth(&self.config.api_key)
            .header("accept", "application/json")
            .header("user-agent", concat!("abycore/", env!("CARGO_PKG_VERSION")));
        let response = tokio::time::timeout(self.config.first_byte_timeout, request.send())
            .await
            .map_err(|_| Error::new(ErrorKind::Timeout, "models request timed out"))?
            .map_err(|error| {
                Error::new(
                    if error.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Transport
                    },
                    "models request failed",
                )
            })?;
        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Server,
                format!("models request rejected ({})", response.status().as_u16()),
            ));
        }
        let bytes = tokio::time::timeout(self.config.first_byte_timeout, response.bytes())
            .await
            .map_err(|_| Error::new(ErrorKind::Timeout, "models response timed out"))?
            .map_err(|_| Error::new(ErrorKind::Transport, "models response failed"))?;
        if bytes.len() > self.config.max_response_bytes {
            return Err(Error::protocol("models response exceeds the size cap"));
        }
        let wire: Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::protocol("invalid models JSON"))?;
        let data = wire
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::protocol("models listing has no data array"))?;
        Ok(data
            .iter()
            .filter_map(|entry| {
                Some(crate::ModelInfo {
                    id: entry.get("id").and_then(Value::as_str)?.to_owned(),
                    owned_by: entry
                        .get("owned_by")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect())
    }

    /// The listing root: strip the Messages suffix (`/anthropic`, `/v1`) back
    /// to the provider origin, so the default
    /// `https://api.deepseek.com/anthropic` resolves `/models` at
    /// `https://api.deepseek.com/models`.
    fn models_url(&self) -> Result<Url> {
        let mut base = self.config.base_url.trim().trim_end_matches('/').to_owned();
        for suffix in ["/v1", "/anthropic"] {
            if let Some(stripped) = base.strip_suffix(suffix) {
                base = stripped.to_owned();
            }
        }
        Url::parse(&format!("{base}/models"))
            .map_err(|_| Error::new(ErrorKind::Configuration, "invalid models URL"))
    }

    pub async fn send(
        &self,
        path: &str,
        body: &Value,
        context: &RequestContext,
        purpose: RequestPurpose,
    ) -> Result<(reqwest::Response, usize)> {
        let url = self.endpoint(path)?;
        // Measured once per dispatch: the provider's token count for this exact
        // envelope is what makes a host's byte-based estimate calibratable.
        let request_bytes = serde_json::to_vec(body).ok().map(|bytes| bytes.len());
        for attempt in 0..=self.config.retry.max_retries {
            context.check()?;
            let index = context.reserve(purpose, attempt.saturating_add(1), request_bytes)?;
            if let Err(error) = context.before_dispatch().await {
                context.record(index, None, "not_dispatched", None);
                return Err(error);
            }
            if let Some(gate) = &context.dispatch_gate
                && let Err(error) = context.wait(gate()).await.and_then(|v| v)
            {
                context.record(index, None, "not_dispatched", None);
                return Err(error);
            }
            let mut request = self
                .client
                .post(url.clone())
                .json(body)
                .header("user-agent", concat!("abycore/", env!("CARGO_PKG_VERSION")));
            request = match &self.protocol {
                Protocol::Messages => request
                    .header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header(
                        "accept",
                        if body["stream"] == true {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    ),
                Protocol::Web => request
                    .bearer_auth(&self.config.api_key)
                    .header("accept", "application/json"),
                #[cfg(feature = "web-search")]
                Protocol::MessagesSearch => request
                    .header("x-api-key", &self.config.api_key)
                    .bearer_auth(&self.config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("accept", "application/json"),
            };
            if matches!(self.protocol, Protocol::Messages) {
                let messages = body["messages"].as_array();
                let blocks = || {
                    messages
                        .into_iter()
                        .flatten()
                        .flat_map(|m| m["content"].as_array().into_iter().flatten())
                };
                let mut betas = Vec::new();
                if blocks().any(|b| b["source"]["type"] == "file") {
                    betas.push("files-api-2025-04-14");
                }
                if blocks()
                    .any(|b| matches!(b["type"].as_str(), Some("tool_addition" | "tool_removal")))
                {
                    betas.push("mid-conversation-tool-changes-2026-07-01");
                }
                if !betas.is_empty() {
                    request = request.header("anthropic-beta", betas.join(","));
                }
            }
            let result = context
                .timed(self.config.first_byte_timeout, request.send())
                .await;
            // Any answer — success or provider error — is the run moving.
            if matches!(result, Ok(Ok(_))) {
                context.touch();
            }
            let error = match result {
                Ok(Ok(response)) if response.status().is_success() => {
                    return Ok((response, index));
                }
                Ok(Ok(response)) => self.http_error(response, context).await,
                Ok(Err(error)) => Error::new(
                    if error.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Transport
                    },
                    "HTTP request failed",
                ),
                Err(error) => error,
            };
            context.record(index, None, format!("{:?}", error.kind), None);
            let retryable = matches!(
                error.kind,
                ErrorKind::RateLimit
                    | ErrorKind::Server
                    | ErrorKind::Transport
                    | ErrorKind::Timeout
            );
            if !retryable || attempt == self.config.retry.max_retries {
                return Err(error);
            }
            let cap = self
                .config
                .retry
                .initial_delay
                .saturating_mul(2u32.saturating_pow(attempt.min(31)))
                .min(self.config.retry.max_delay);
            let jitter = cap.mul_f64(0.5 + rand::random::<f64>() * 0.5);
            let delay = error
                .retry_after
                .map_or(jitter, |minimum| minimum.max(jitter));
            // Waiting longer than a whole window cannot help — the run has to
            // move within one — and a run that has already gone quiet belongs to
            // the host's continuation, not to another silent attempt here.
            if delay > self.config.retry.max_delay
                || context.window().is_some_and(|window| delay >= window)
                || context.stalled()
            {
                return Err(error);
            }
            context.wait(tokio::time::sleep(delay)).await?;
        }
        unreachable!("finite inclusive attempt range")
    }

    pub async fn body(
        &self,
        response: reqwest::Response,
        context: &RequestContext,
    ) -> Result<Vec<u8>> {
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = context
            .timed(self.config.stream_idle_timeout, stream.next())
            .await?
        {
            let chunk =
                chunk.map_err(|_| Error::new(ErrorKind::Transport, "HTTP body interrupted"))?;
            // Bytes on the wire are progress, however slowly they arrive.
            context.touch();
            if chunk.len() > self.config.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(Error::protocol("response size limit exceeded"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn http_error(&self, response: reqwest::Response, context: &RequestContext) -> Error {
        let status = response.status().as_u16();
        let headers = response.headers();
        let request_id = ["request-id", "x-request-id", "x-deepseek-request-id"]
            .iter()
            .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
            .map(|s| self.redact(s));
        let retry_after = retry_after(headers);
        let bytes = match self.body(response, context).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind == ErrorKind::Cancelled => {
                return error;
            }
            Err(_) => {
                if let Err(error) = context.check() {
                    return error;
                }
                vec![]
            }
        };
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let code = json
            .pointer("/error/code")
            .or_else(|| json.pointer("/error/type"))
            .and_then(Value::as_str)
            .map(|s| self.redact(s));
        let kind = match (status, code.as_deref()) {
            (400 | 404 | 410, _)
                if matches!(self.protocol, Protocol::Messages)
                    && json
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| {
                            let message = message.to_ascii_lowercase();
                            message.contains("file")
                                && (message.contains("expired")
                                    || message.contains("not found")
                                    || message.contains("not exist"))
                        }) =>
            {
                ErrorKind::FileUnavailable
            }
            (_, Some("context_length_exceeded" | "context_limit_exceeded")) => {
                ErrorKind::ContextLimitExceeded
            }
            (402, _) | (_, Some("insufficient_quota" | "quota_exceeded")) => ErrorKind::Quota,
            (403, _) if matches!(self.protocol, Protocol::Web) => ErrorKind::AccessDenied,
            (401 | 403, _) => ErrorKind::Authentication,
            (404, _) if matches!(self.protocol, Protocol::Web) => ErrorKind::WebUnavailable,
            (408, _) => ErrorKind::Timeout,
            (429, _) => ErrorKind::RateLimit,
            (500..=599, _) => ErrorKind::Server,
            (300..=399, _) => ErrorKind::Protocol,
            _ => ErrorKind::InvalidRequest,
        };
        // Do not echo provider error messages: they can contain input or credentials.
        let mut error = Error::new(kind, format!("API returned HTTP {status}"));
        error.status = Some(status);
        error.code = code;
        error.request_id = request_id;
        error.retry_after = retry_after;
        error
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
        })
}
