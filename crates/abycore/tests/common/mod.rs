#![allow(dead_code)]
use abycore::{ClientConfig, DeepSeekClient};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    pub headers: String,
    pub body: Value,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<(Duration, Vec<u8>)>,
    pub header_delay: Duration,
}
impl Reply {
    pub fn json(body: Value) -> Self {
        Self::raw(200, "application/json", body.to_string())
    }
    pub fn raw(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            chunks: vec![(Duration::ZERO, body.into().into_bytes())],
            header_delay: Duration::ZERO,
        }
    }
    pub fn sse(response: Value) -> Self {
        Self::events(message_events(response))
    }
    pub fn events(events: Vec<Value>) -> Self {
        Self::raw(
            200,
            "text/event-stream",
            events.into_iter().map(|v| frame(&v)).collect::<String>(),
        )
    }
}
pub fn frame(event: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event["type"].as_str().unwrap(),
        event
    )
}
pub fn response(id: &str, output: Vec<Value>) -> Value {
    let reason = if output.iter().any(|b| b["type"] == "tool_use") {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({"type":"message","role":"assistant","id":id,"model":"fixture-model","stop_reason":reason,"content":output,
        "usage":{"input_tokens":7,"output_tokens":4,"cache_read_input_tokens":3,"cache_creation_input_tokens":0}})
}
pub fn message(_id: &str, text: &str) -> Value {
    json!({"type":"text","text":text})
}
pub fn reasoning(id: &str, text: &str) -> Value {
    json!({"type":"thinking","thinking":text,"signature":format!("sig-{id}")})
}
pub fn call(id: &str, name: &str, arguments: &str) -> Value {
    let input: Value = serde_json::from_str(arguments).expect("fixture tool input JSON");
    json!({"type":"tool_use","id":id,"name":name,"input":input})
}
pub fn message_start(id: &str) -> Value {
    json!({"type":"message_start","message":{"type":"message","role":"assistant","id":id,"model":"fixture-model","content":[],"stop_reason":null,
        "usage":{"input_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":0,"output_tokens":1}}})
}
pub fn message_events(response: Value) -> Vec<Value> {
    let mut events = vec![message_start(response["id"].as_str().unwrap())];
    for (index, block) in response["content"].as_array().unwrap().iter().enumerate() {
        events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":response["stop_reason"]},"usage":response["usage"]}));
    events.push(json!({"type":"message_stop"}));
    events
}

pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let mut replies: VecDeque<_> = replies.into();
        Self::start_with_handler(move |_| {
            replies
                .pop_front()
                .unwrap_or_else(|| Reply::raw(500, "text/plain", "fixture exhausted"))
        })
        .await
    }

    pub async fn start_with_handler(
        mut reply: impl FnMut(&Request) -> Reply + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            let mut responders = tokio::task::JoinSet::new();
            loop {
                while responders.try_join_next().is_some() {}
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut bytes = vec![];
                let end = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    assert!(bytes.len() < 1024 * 1024);
                };
                let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let body = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end..end + length]).unwrap()
                };
                let path = headers.split_whitespace().nth(1).unwrap().into();
                let request = Request {
                    path,
                    headers,
                    body,
                };
                let reply = reply(&request);
                captured.lock().unwrap().push(request);
                responders.spawn(async move {
                    tokio::time::sleep(reply.header_delay).await;
                    let size: usize = reply.chunks.iter().map(|(_, b)| b.len()).sum();
                    let mut headers = format!(
                        "HTTP/1.1 {} Fixture\r\nContent-Length: {size}\r\nConnection: close\r\n",
                        reply.status
                    );
                    for (key, value) in reply.headers {
                        headers.push_str(&format!("{key}: {value}\r\n"));
                    }
                    headers.push_str("\r\n");
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    for (delay, chunk) in reply.chunks {
                        tokio::time::sleep(delay).await;
                        if socket.write_all(&chunk).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    pub fn config(&self) -> ClientConfig {
        let mut config = ClientConfig::new("fixture-secret");
        config.base_url = format!("{}/gateway/v1", self.url);
        config.retry.max_retries = 0;
        config.retry.initial_delay = Duration::from_millis(1);
        config.retry.max_delay = Duration::from_millis(10);
        config
    }
    pub fn client(&self) -> DeepSeekClient {
        DeepSeekClient::new(self.config()).unwrap()
    }
    pub fn captured(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
