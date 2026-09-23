mod assembler;
mod messages;
mod sse;
pub(crate) mod transport;

use crate::{
    Error, ErrorKind, Item, ModelOptions, RequestOptions, RequestPurpose, Response, Result,
    StreamEvent, ToolDefinition, context::RequestContext,
};
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};
use transport::Transport;

pub type MessageStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// A native Messages request built from the SDK's provider-neutral transcript.
#[derive(Clone, Debug)]
pub struct MessageRequest {
    pub system: String,
    pub history: Vec<Item>,
    pub options: ModelOptions,
    pub tools: Vec<ToolDefinition>,
}

impl MessageRequest {
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            system: String::new(),
            history: vec![Item::user(input)],
            options: ModelOptions::default(),
            tools: vec![],
        }
    }
    pub(crate) fn body(&self, stream: bool) -> Result<Value> {
        messages::request(
            &self.system,
            &self.history,
            &self.options,
            &self.tools,
            stream,
        )
    }

    /// The wire body without the non-empty-history dispatch rule, for pricing a
    /// request the host has not sent yet.
    pub(crate) fn measurement_body(&self) -> Result<Value> {
        messages::measurement_body(&self.system, &self.history, &self.options, &self.tools)
    }
}

#[derive(Clone, Debug)]
pub struct DeepSeekClient {
    transport: Transport,
}

impl DeepSeekClient {
    pub fn new(config: crate::ClientConfig) -> Result<Self> {
        Ok(Self {
            transport: Transport::new(config)?,
        })
    }

    /// Fetch the provider's advertised model listing (`GET /models`).
    ///
    /// Best-effort and single-shot: it exists for host catalogs and pickers,
    /// not the agent loop, so it never spends the run's request budget.
    pub async fn models(&self) -> Result<Vec<crate::ModelInfo>> {
        self.transport.models().await
    }

    pub async fn complete(
        &self,
        request: MessageRequest,
        options: RequestOptions,
    ) -> Result<Response> {
        let context = RequestContext::new(
            options.cancellation,
            options.timeout,
            usize::MAX,
            Arc::new(Mutex::new(vec![])),
        )?;
        let body = request.body(false)?;
        let (http, index) = self
            .transport
            .send("messages", &body, &context, RequestPurpose::Conversation)
            .await?;
        let bytes = self.transport.body(http, &context).await?;
        let wire =
            serde_json::from_slice(&bytes).map_err(|_| Error::protocol("invalid response JSON"))?;
        let response = messages::decode(wire, &request.options.model)?;
        context.record(
            index,
            Some(response.id.clone()),
            format!("{:?}", response.status),
            response.usage.clone(),
        );
        Ok(response)
    }

    pub async fn stream(
        &self,
        request: MessageRequest,
        options: RequestOptions,
    ) -> Result<MessageStream> {
        let context = RequestContext::new(
            options.cancellation,
            options.timeout,
            usize::MAX,
            Arc::new(Mutex::new(vec![])),
        )?;
        self.stream_with_context(request, context).await
    }

    pub(crate) async fn stream_with_context(
        &self,
        request: MessageRequest,
        context: RequestContext,
    ) -> Result<MessageStream> {
        self.stream_with_purpose(request, context, RequestPurpose::Conversation)
            .await
    }

    /// Same stream, with the ledger purpose the caller owns: a host-driven
    /// summarization call must not be booked as conversation traffic.
    pub(crate) async fn stream_with_purpose(
        &self,
        request: MessageRequest,
        context: RequestContext,
        purpose: RequestPurpose,
    ) -> Result<MessageStream> {
        let body = request.body(true)?;
        let (http, index) = self
            .transport
            .send("messages", &body, &context, purpose)
            .await?;
        if !http
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|s| {
                s.split(';')
                    .next()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return Err(Error::protocol("stream response is not text/event-stream"));
        }
        let client = self.clone();
        Ok(Box::pin(async_stream::try_stream! {
            let mut chunks = http.bytes_stream();
            let mut parser = sse::Parser::new(client.transport.config().max_event_bytes);
            let mut assembler = assembler::Assembler::new(request.options.model.clone());
            let mut size = 0usize;
            loop {
                let chunk = context.timed(client.transport.config().stream_idle_timeout, chunks.next()).await?
                    .ok_or_else(|| Error::new(ErrorKind::StreamClosed, "SSE ended without a terminal response event"))?
                    .map_err(|_| Error::new(ErrorKind::Transport, "response stream interrupted"))?;
                size = size.checked_add(chunk.len()).ok_or_else(|| Error::protocol("stream size overflow"))?;
                if size > client.transport.config().max_response_bytes { Err(Error::protocol("stream size limit exceeded"))?; }
                // Bytes on the wire are progress: a long response is a working
                // run, not a stalled one.
                context.touch();
                for frame in parser.feed(&chunk)? {
                    context.check()?;
                    if let Some(mut event) = assembler.accept(frame)? {
                        if let StreamEvent::Started { response_id, .. } = &event {
                            context.record(index, Some(response_id.clone()), "in_progress", None);
                        }
                        let terminal = if let StreamEvent::Finished { response, .. } = &mut event {
                            if let Some(message) = &mut response.error_message { *message = client.transport.redact(message); }
                            if let Some(code) = &mut response.error_code { *code = client.transport.redact(code); }
                            context.record(index, Some(response.id.clone()), format!("{:?}", response.status), response.usage.clone());
                            true
                        } else { false };
                        yield event;
                        if terminal { return; }
                    }
                }
            }
        }))
    }
}
