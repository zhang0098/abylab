//! Credential-scoped image references. Durable history always retains the original image.
use super::transport::Transport;
use crate::{Error, ErrorKind, Result, context::RequestContext};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Debug)]
struct CachedFile {
    id: String,
    refresh_at: u64,
}

#[derive(Debug, Default)]
pub(super) struct FileStore(Mutex<BTreeMap<String, CachedFile>>);

impl FileStore {
    pub async fn clear(&self) {
        self.0.lock().await.clear();
    }

    pub async fn prepare(
        &self,
        transport: &Transport,
        body: &Value,
        context: &RequestContext,
    ) -> Result<Value> {
        let mut images = Vec::new();
        for (message, value) in body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            for (block, value) in value["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if value["type"] == "image" && value["source"]["type"] == "base64" {
                    images.push((
                        message,
                        block,
                        value["source"]["media_type"].as_str().unwrap_or(""),
                        value["source"]["data"].as_str().unwrap_or(""),
                    ));
                }
            }
        }
        if images.is_empty() {
            return Ok(body.clone());
        }
        let config = &transport.config().files;
        if images.len() > config.max_images {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "too many request images",
            ));
        }
        if crate::model_capabilities(body["model"].as_str().unwrap_or(""))
            .is_some_and(|c| !c.images)
        {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "selected model does not support image input",
            ));
        }
        let mut decoded = Vec::new();
        let mut total = 0usize;
        let mut inline_bytes = 0usize;
        for (message, block, mime, data) in images {
            inline_bytes = inline_bytes.saturating_add(data.len());
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| Error::new(ErrorKind::InvalidRequest, "invalid base64 image"))?;
            total = total.saturating_add(bytes.len());
            if total > config.max_request_bytes {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "request images exceed Files API byte limit",
                ));
            }
            decoded.push((message, block, mime, bytes));
        }
        let inline = || {
            if inline_bytes > config.max_inline_bytes {
                Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "inline image fallback exceeds byte limit",
                ))
            } else {
                Ok(body.clone())
            }
        };
        if !config.enabled {
            return inline();
        }
        let operation = async {
            // One in-flight upload per client prevents duplicate uploads from sibling agents.
            let mut cache = context.wait(self.0.lock()).await?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            cache.retain(|_, file| file.refresh_at > now);
            let mut prepared = body.clone();
            for (message, block, mime, bytes) in decoded {
                context.check()?;
                let hash = format!("{mime}:{:x}", Sha256::digest(&bytes));
                if !cache.contains_key(&hash) {
                    let value = transport.upload_image(bytes, mime, context).await?;
                    let id = value["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| Error::protocol("Files API response has no file id"))?;
                    // Never trust a future server timestamp to extend the local reuse window.
                    let created = value["created_at"].as_u64().unwrap_or(now).min(now);
                    cache.insert(
                        hash.clone(),
                        CachedFile {
                            id: id.into(),
                            refresh_at: created
                                .saturating_add(config.expires_after_secs)
                                .saturating_sub(config.refresh_margin_secs),
                        },
                    );
                }
                prepared["messages"][message]["content"][block]["source"] =
                    json!({"type":"file","file_id":cache[&hash].id});
            }
            Ok(prepared)
        };
        match context
            .timed(Duration::from_secs(config.timeout_secs), operation)
            .await
            .and_then(|v| v)
        {
            Ok(body) => Ok(body),
            Err(error)
                if matches!(
                    error.kind,
                    ErrorKind::Cancelled
                        | ErrorKind::BudgetExceeded
                        | ErrorKind::Session
                        | ErrorKind::EventHandler
                        | ErrorKind::Authentication
                        | ErrorKind::Quota
                ) =>
            {
                Err(error)
            }
            Err(_) => {
                context.check()?;
                inline()
            }
        }
    }

    pub fn inline(transport: &Transport, body: &Value) -> Result<Value> {
        let bytes: usize = body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter_map(|b| b["source"]["data"].as_str())
            .map(str::len)
            .sum();
        if bytes > transport.config().files.max_inline_bytes {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "inline image fallback exceeds byte limit",
            ));
        }
        Ok(body.clone())
    }
}
