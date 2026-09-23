//! Shared fixtures for the driver integration tests.
//!
//! A scripted Anthropic-Messages endpoint plus a small harness that drives one
//! prompt through the real driver and captures both the emitted events and the
//! request bodies the model would have seen.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use abylab_backend::driver::spawn;
use abylab_backend::{Cmd, CompactionConfig, CtlEvent, DriverConfig, Event, TurnLimits, UiEvent};

// ---------------------------------------------------------------------------
// Anthropic-Messages SSE fixtures (same event shape as abycore's own tests).
// ---------------------------------------------------------------------------

fn frame(event_type: &str, data: &str) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// The fixture's default provider count. Deliberately implausible for the
/// envelope size, which keeps byte-based estimates in charge unless a test
/// supplies a realistic figure.
const FIXTURE_INPUT_TOKENS: u64 = 7;

fn message_start(id: &str, input_tokens: u64) -> String {
    frame(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"type":"message","role":"assistant","id":"{id}","model":"fixture-model","content":[],"stop_reason":null,"usage":{{"input_tokens":{input_tokens},"cache_read_input_tokens":3,"cache_creation_input_tokens":0,"output_tokens":1}}}}}}"#
        ),
    )
}

fn block_start(index: usize, block: &str) -> String {
    frame(
        "content_block_start",
        &format!(r#"{{"type":"content_block_start","index":{index},"content_block":{block}}}"#),
    )
}

fn block_stop(index: usize) -> String {
    frame(
        "content_block_stop",
        &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    )
}

fn message_delta(stop_reason: &str, input_tokens: u64) -> String {
    frame(
        "message_delta",
        &format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}},"usage":{{"input_tokens":{input_tokens},"output_tokens":4,"cache_read_input_tokens":3,"cache_creation_input_tokens":0}}}}"#
        ),
    )
}

fn message_stop() -> String {
    frame("message_stop", r#"{"type":"message_stop"}"#)
}

/// A complete SSE body: one message with `blocks`, ending on `stop_reason`.
fn sse(id: &str, blocks: &[String], stop_reason: &str) -> String {
    sse_with_usage(id, blocks, stop_reason, FIXTURE_INPUT_TOKENS)
}

/// The same, with the provider count a test wants the estimate calibrated
/// against.
fn sse_with_usage(id: &str, blocks: &[String], stop_reason: &str, input_tokens: u64) -> String {
    let mut body = message_start(id, input_tokens);
    for (index, block) in blocks.iter().enumerate() {
        body.push_str(&block_start(index, block));
        body.push_str(&block_stop(index));
    }
    body.push_str(&message_delta(stop_reason, input_tokens));
    body.push_str(&message_stop());
    body
}

/// One tool call with arbitrary name and arguments.
pub fn single_call_reply(call_id: &str, name: &str, arguments: &str) -> String {
    single_call_reply_with_usage(call_id, name, arguments, FIXTURE_INPUT_TOKENS)
}

/// The same call, claiming `input_tokens` for the request that produced it —
/// realistic figures let the host calibrate its byte-based estimate.
pub fn single_call_reply_with_usage(
    call_id: &str,
    name: &str,
    arguments: &str,
    input_tokens: u64,
) -> String {
    sse_with_usage(
        &format!("msg-{call_id}"),
        &[format!(
            r#"{{"type":"tool_use","id":"{call_id}","name":"{name}","input":{arguments}}}"#
        )],
        "tool_use",
        input_tokens,
    )
}

/// A `read` call: allowed under the default permission preset, so the driver
/// executes it without a permission ask the test cannot answer. Every reply
/// carries the same arguments, so a repeated reply is a repeated call.
pub fn tool_call_reply(call_id: &str) -> String {
    single_call_reply(call_id, "read", r#"{"file_path":"missing.txt"}"#)
}

/// A tool call whose arguments stream as `input_json_delta`s, the way a real
/// provider writes them — the live path a host titles the card from.
pub fn streamed_call_reply(call_id: &str, name: &str, chunks: &[&str]) -> String {
    let mut body = message_start(&format!("msg-{call_id}"), FIXTURE_INPUT_TOKENS);
    body.push_str(&block_start(
        0,
        &format!(r#"{{"type":"tool_use","id":"{call_id}","name":"{name}","input":{{}}}}"#),
    ));
    for chunk in chunks {
        body.push_str(&frame(
            "content_block_delta",
            &format!(
                r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                serde_json::Value::String((*chunk).to_string())
            ),
        ));
    }
    body.push_str(&block_stop(0));
    body.push_str(&message_delta("tool_use", FIXTURE_INPUT_TOKENS));
    body.push_str(&message_stop());
    body
}

/// Two calls in one batch: under a one-call tool budget the second call stops
/// the segment with a pending batch, not an empty one.
pub fn two_tool_call_reply() -> String {
    let call = |call_id: &str, path: &str| {
        format!(
            r#"{{"type":"tool_use","id":"{call_id}","name":"read","input":{{"file_path":"{path}"}}}}"#
        )
    };
    sse(
        "msg-tools",
        &[
            call("call-1", "missing.txt"),
            call("call-2", "missing2.txt"),
        ],
        "tool_use",
    )
}

/// A text-only reply with arbitrary content (JSON-escaped).
pub fn text_body(id: &str, text: &str) -> String {
    text_body_with_usage(id, text, FIXTURE_INPUT_TOKENS)
}

/// The same, claiming `input_tokens` for the request that produced it.
pub fn text_body_with_usage(id: &str, text: &str, input_tokens: u64) -> String {
    let text = serde_json::Value::String(text.to_string()).to_string();
    sse_with_usage(
        id,
        &[format!(r#"{{"type":"text","text":{text}}}"#)],
        "end_turn",
        input_tokens,
    )
}

pub fn text_reply() -> String {
    text_body("msg-text", "done")
}

pub fn incomplete_reply(id: &str) -> String {
    sse(
        id,
        &[r#"{"type":"text","text":"partial answer"}"#.into()],
        "max_tokens",
    )
}

// ---------------------------------------------------------------------------
// Blocking HTTP fixture
// ---------------------------------------------------------------------------

/// One scripted response. Failures carry the headers that drive abycore's own
/// retry pacing (`retry-after`).
pub struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    dynamic: Option<Dynamic>,
    /// Wait before answering, so a deadline can actually fire.
    delay: Option<Duration>,
}

impl Reply {
    /// Build the reply from the request body it answers.
    pub fn dynamic(build: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Reply {
        Reply {
            status: 0,
            headers: vec![],
            body: String::new(),
            dynamic: Some(Box::new(build)),
            delay: None,
        }
    }

    pub fn sse(body: String) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
            dynamic: None,
            delay: None,
        }
    }

    /// A JSON error reply with a provider error type, e.g. a context overflow.
    pub fn error(status: u16, error_type: &str) -> Self {
        Self::status(status).body(format!(
            r#"{{"error":{{"type":"{error_type}","message":"fixture"}}}}"#
        ))
    }

    /// Replace the response body — a fixture that answers a non-SSE endpoint
    /// (the `/models` listing, say) spells the JSON here.
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into(),
            dynamic: None,
            delay: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Answer only after `delay`; the run deadline can then expire first.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            _ => "Status",
        }
    }
}

/// A scripted reply that may depend on the request that triggered it (used when
/// the answer must quote state the test cannot know up front, e.g. a goal id).
pub type Dynamic = Box<dyn Fn(&str) -> Reply + Send + Sync>;

/// Serves `replies` in order, records every request body, and closes each
/// connection after the body so the SSE stream reaches EOF.
pub struct MockServer {
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let base_url = format!("http://{}", listener.local_addr().expect("fixture addr"));
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut replies = replies.into_iter();
            while !stopping.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                socket
                    .set_nonblocking(false)
                    .expect("fixture socket blocking");
                let Some(reply) = replies.next() else { break };
                let Some(body) = read_request(&mut socket) else {
                    break;
                };
                let reply = match reply.dynamic {
                    Some(build) => build(&body),
                    None => reply,
                };
                captured.lock().expect("request lock").push(body);
                if let Some(delay) = reply.delay {
                    std::thread::sleep(delay);
                }
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                    reply.status,
                    reply.reason(),
                    reply.body.len(),
                );
                for (name, value) in &reply.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str("\r\n");
                let _ = socket.write_all(head.as_bytes());
                let _ = socket.write_all(reply.body.as_bytes());
                let _ = socket.flush();
            }
        });
        Self {
            base_url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn bodies(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one request head plus its `content-length` body. Headers and body may
/// arrive in separate reads, so keep reading until the body is complete.
fn read_request(socket: &mut std::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = socket.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(head_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buffer.len() >= head_end + 4 + length {
            let body = buffer[head_end + 4..head_end + 4 + length].to_vec();
            return Some(String::from_utf8_lossy(&body).into_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Driver harness
// ---------------------------------------------------------------------------

/// Captured event summaries. The sink is only allowed to append.
type Captured = Arc<Mutex<Vec<String>>>;

fn captured_sink(events: Captured) -> impl Fn(Event) + Send + Sync + 'static {
    move |event: Event| {
        let summary = match &event {
            Event::Ui(ui) => match ui {
                UiEvent::TurnStart { turn, .. } => format!("turn-start:{turn}"),
                UiEvent::TurnEnd { kind, .. } => format!("turn-end:{kind}"),
                UiEvent::SessionStatus { running, .. } => format!("status:{running}"),
                UiEvent::PermissionPreset { preset, .. } => format!("permission:{preset}"),
                // The live tool line: streamed arguments, then the completed
                // call, then execution start, then the result.
                UiEvent::ToolCallDelta { call_id, delta, .. } => {
                    format!("tool-delta:{call_id}:{delta}")
                }
                UiEvent::ToolCall {
                    call_id, arguments, ..
                } => format!("tool-call:{call_id}:{arguments}"),
                UiEvent::ToolStarted { call_id, .. } => format!("tool-started:{call_id}"),
                UiEvent::ToolResult { call_id, .. } => format!("tool-result:{call_id}"),
                _ => return,
            },
            Event::Ctl(ctl) => match ctl {
                CtlEvent::TuiOpDone(message) => format!("op-done:{message}"),
                CtlEvent::TuiOpFailed(message) => format!("op-failed:{message}"),
                CtlEvent::Error(message) => format!("error:{message}"),
                _ => return,
            },
            Event::PermissionAsk { title, .. } => format!("permission-ask:{title}"),
        };
        events.lock().expect("event lock").push(summary);
    }
}

fn temp_workspace(tag: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("abylab-{tag}-{}-{unique}", std::process::id()))
}

/// One driven prompt: the transcript-ready events and every request body the
/// provider received, in order.
pub struct Run {
    pub events: Vec<String>,
    pub requests: Vec<String>,
}

impl Run {
    pub fn count(&self) -> usize {
        self.requests.len()
    }

    /// Auto-continuation notices, in order: a budget stop or a transient retry.
    pub fn continuations(&self) -> Vec<&String> {
        self.events
            .iter()
            .filter(|event| {
                event.starts_with("op-done:turn budget reached")
                    || event.starts_with("op-done:connection lost")
                    || event.starts_with("op-done:rate limited")
                    || event.starts_with("op-done:server error")
                    || event.starts_with("op-done:timed out")
            })
            .collect()
    }

    /// Compaction notices: automatic condensation, overflow recovery, the
    /// pruner-only path, or a `/compact` result.
    pub fn compactions(&self) -> Vec<&String> {
        self.events
            .iter()
            .filter(|event| {
                event.starts_with("op-done:compacted")
                    || event.starts_with("op-done:context near the window")
                    || event.starts_with("op-done:context overflow")
                    || event.starts_with("op-done:trimmed old tool outputs")
            })
            .collect()
    }

    pub fn ended(&self, kind: &str) -> bool {
        self.events
            .iter()
            .any(|event| event == &format!("turn-end:{kind}"))
    }

    /// No error surfaced and no permission overlay was needed.
    pub fn is_clean(&self) -> bool {
        !self.events.iter().any(|event| event.starts_with("error:"))
            && !self
                .events
                .iter()
                .any(|event| event.starts_with("permission-ask:"))
    }

    pub fn explain(&self) -> String {
        format!("events: {:?}", self.events)
    }
}

/// Workspace setup run before the driver starts (e.g. seeding a file a tool
/// call will read).
pub type Seed = Box<dyn FnOnce(&std::path::Path)>;

/// One driver script: what to send, how the provider answers, and the host
/// context policy. A prompt is followed to its turn end (so a hang fails
/// loudly); `/compact` waits for its own notice.
pub struct Scenario {
    pub tag: String,
    pub max_tokens: Option<u64>,
    pub resume: Option<String>,
    pub limits: TurnLimits,
    pub compaction: Option<CompactionConfig>,
    pub steps: Vec<Step>,
    pub replies: Vec<Reply>,
    /// Files written into the workspace before the driver starts.
    pub seed: Option<Seed>,
}

#[derive(Clone, Debug)]
pub enum Step {
    Prompt(String),
    /// Send `text`, then Esc once `marker` shows up among the events; the step is
    /// done when the turn ends. The marker keeps the interrupt timed to a known
    /// point inside the turn instead of a guess.
    Interrupted {
        text: String,
        marker: String,
    },
    Compact,
    /// `/goal <arg>`: the driver settles the whole goal run before the next step.
    Goal(String),
}

impl Scenario {
    pub fn new(tag: &str, replies: Vec<Reply>) -> Self {
        Self {
            tag: tag.to_string(),
            max_tokens: None,
            resume: None,
            limits: TurnLimits::default(),
            compaction: None,
            steps: vec![],
            replies,
            seed: None,
        }
    }

    pub fn limits(mut self, limits: TurnLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn resume(mut self, session_id: &str) -> Self {
        self.resume = Some(session_id.into());
        self
    }

    pub fn compaction(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = Some(compaction);
        self
    }

    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    pub fn prompt(mut self, text: impl Into<String>) -> Self {
        self.steps.push(Step::Prompt(text.into()));
        self
    }

    /// `/goal <arg>` as one scripted step.
    pub fn goal(mut self, arg: impl Into<String>) -> Self {
        self.steps.push(Step::Goal(arg.into()));
        self
    }

    /// Send `text` and Esc as soon as `marker` appears in the event stream
    /// (e.g. `tool-started:call-1`).
    pub fn interrupted(mut self, text: impl Into<String>, marker: impl Into<String>) -> Self {
        self.steps.push(Step::Interrupted {
            text: text.into(),
            marker: marker.into(),
        });
        self
    }

    pub fn seed(mut self, seed: impl FnOnce(&std::path::Path) + 'static) -> Self {
        self.seed = Some(Box::new(seed));
        self
    }
}

/// Drive one prompt against `replies`; the turn must end on its own, so a hang
/// fails loudly.
pub fn drive_prompt(tag: &str, limits: TurnLimits, replies: Vec<Reply>) -> Run {
    drive(
        Scenario::new(tag, replies)
            .limits(limits)
            .prompt("read the missing file"),
    )
}

/// Drive a full script and return the transcript events plus every request body
/// the provider received.
pub fn drive(scenario: Scenario) -> Run {
    let Scenario {
        tag,
        max_tokens,
        resume,
        limits,
        compaction,
        steps,
        replies,
        seed,
    } = scenario;
    let server = MockServer::start(replies);
    let workspace = temp_workspace(&tag);
    std::fs::create_dir_all(&workspace).expect("workspace directory");
    if let Some(seed) = seed {
        seed(&workspace);
    }
    let events: Captured = Arc::new(Mutex::new(Vec::new()));
    let handle = spawn(
        DriverConfig {
            session_id: format!("{tag}-test"),
            resume,
            sessions_root: None,
            home: None,
            workspace: workspace.to_string_lossy().into_owned(),
            model: "deepseek-flash".into(),
            reasoning: "off".into(),
            permission: None,
            max_tokens,
            api_key: Some("test-key".into()),
            base_url: Some(server.base_url.clone()),
            limits,
            compaction,
        },
        captured_sink(Arc::clone(&events)),
    )
    .expect("driver spawns");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut turns = 0usize;
    for step in &steps {
        match step {
            Step::Prompt(text) => {
                turns += 1;
                handle.send(Cmd::Prompt { text: text.clone() });
                let expected = turns;
                wait_for(&events, deadline, &format!("turn {expected}"), |events| {
                    events
                        .iter()
                        .filter(|event| event.starts_with("turn-end:"))
                        .count()
                        >= expected
                });
            }
            Step::Interrupted { text, marker } => {
                handle.send(Cmd::Prompt { text: text.clone() });
                wait_for(&events, deadline, "the interrupt marker", |events| {
                    events
                        .iter()
                        .any(|event| event.starts_with(marker.as_str()))
                });
                let ended = events
                    .lock()
                    .expect("event lock")
                    .iter()
                    .filter(|event| event.starts_with("turn-end:"))
                    .count();
                handle.interrupt();
                wait_for(&events, deadline, "the interrupted turn", |events| {
                    events
                        .iter()
                        .filter(|event| event.starts_with("turn-end:"))
                        .count()
                        > ended
                });
            }
            Step::Compact => {
                let start = events.lock().expect("event lock").len();
                handle.send(Cmd::Compact);
                wait_for(&events, deadline, "the compaction result", |events| {
                    events[start..].iter().any(|event| {
                        event.starts_with("op-done:compacted") || event.starts_with("op-failed:")
                    })
                });
            }
            Step::Goal(arg) => {
                handle.send(Cmd::Goal { arg: arg.clone() });
                // The driver settles every round before it reads the next
                // command, so quiescence means the whole run is done — a
                // predicate cannot tell an intermediate round notice from the
                // last one.
                wait_quiet(&events, deadline, &format!("goal {arg:?}"));
            }
        }
    }
    handle.shutdown();
    drop(handle);

    let run = Run {
        events: events.lock().expect("event lock").clone(),
        requests: server.bodies(),
    };
    let _ = std::fs::remove_dir_all(&workspace);
    run
}

/// Wait until the event stream stops growing: the driver processes commands in
/// order, so quiescence means it finished this command and emitted everything.
fn wait_quiet(events: &Captured, deadline: Instant, what: &str) {
    // The driver may still be starting: never treat pre-first-event silence as
    // quiescence.
    while events.lock().expect("event lock").is_empty() {
        assert!(
            Instant::now() < deadline,
            "{what} emitted nothing: {:?}",
            events.lock().expect("event lock")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut last = events.lock().expect("event lock").len();
    let mut quiet = 0;
    loop {
        std::thread::sleep(Duration::from_millis(60));
        let now = events.lock().expect("event lock").len();
        if now == last {
            quiet += 1;
            if quiet >= 3 {
                return;
            }
        } else {
            quiet = 0;
            last = now;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never settled: {:?}",
            events.lock().expect("event lock")
        );
    }
}

fn wait_for(events: &Captured, deadline: Instant, what: &str, ready: impl Fn(&[String]) -> bool) {
    loop {
        if ready(&events.lock().expect("event lock")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never settled: {:?}",
            events.lock().expect("event lock")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
