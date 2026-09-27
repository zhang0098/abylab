#![cfg(unix)]

//! `abylab ask "<question>"` end to end, against a scripted provider: the
//! answer on stdout, nothing else on it, and an exit code a script can read.
//!
//! The mock provider is a trimmed copy of the backend fixture
//! (`crates/abylab-backend/tests/common`): what is under test here is the front
//! end — argument parsing, the headless turn and the exit code — which only
//! exists in this crate.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a headless ask may take before the test calls it wedged. Every
/// scripted reply is local, so the only real cost is process startup.
const PATIENCE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Anthropic-Messages SSE fixtures (the shape abycore speaks)
// ---------------------------------------------------------------------------

fn frame(event_type: &str, data: &str) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn message_start(id: &str) -> String {
    frame(
        "message_start",
        &format!(
            r#"{{"type":"message_start","message":{{"type":"message","role":"assistant","id":"{id}","model":"fixture-model","content":[],"stop_reason":null,"usage":{{"input_tokens":7,"output_tokens":1}}}}}}"#
        ),
    )
}

fn block_start(index: usize, content: &str) -> String {
    frame(
        "content_block_start",
        &format!(r#"{{"type":"content_block_start","index":{index},"content_block":{content}}}"#),
    )
}

fn block_delta(index: usize, delta: &str) -> String {
    frame(
        "content_block_delta",
        &format!(r#"{{"type":"content_block_delta","index":{index},"delta":{delta}}}"#),
    )
}

fn block_stop(index: usize) -> String {
    frame(
        "content_block_stop",
        &format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    )
}

fn message_delta(stop_reason: &str) -> String {
    frame(
        "message_delta",
        &format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}},"usage":{{"input_tokens":7,"output_tokens":4}}}}"#
        ),
    )
}

fn message_stop() -> String {
    frame("message_stop", r#"{"type":"message_stop"}"#)
}

/// One complete SSE body: the message's blocks in order — each already a whole
/// start…stop group — then the stop reason that settles it.
fn sse(id: &str, blocks: &[String], stop_reason: &str) -> String {
    let mut body = message_start(id);
    for block in blocks {
        body.push_str(block);
    }
    body.push_str(&message_delta(stop_reason));
    body.push_str(&message_stop());
    body
}

fn json(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}

/// A text block: what the model said, all of it in the start event.
fn text_block(index: usize, text: &str) -> String {
    block_start(
        index,
        &format!(r#"{{"type":"text","text":{}}}"#, json(text)),
    ) + &block_stop(index)
}

/// The same block written the way a live provider writes one: the start event
/// carries an empty string and the text arrives as `text_delta`s — the path the
/// printed answer accumulates.
fn streamed_text(index: usize, chunks: &[&str]) -> String {
    let mut body = block_start(index, &format!(r#"{{"type":"text","text":{}}}"#, json("")));
    for chunk in chunks {
        body.push_str(&block_delta(
            index,
            &format!(r#"{{"type":"text_delta","text":{}}}"#, json(chunk)),
        ));
    }
    body + &block_stop(index)
}

/// A thinking block: what the model says to itself, which is not the answer.
fn thinking_block(index: usize, text: &str) -> String {
    block_start(
        index,
        &format!(r#"{{"type":"thinking","thinking":{}}}"#, json(text)),
    ) + &block_stop(index)
}

/// A `read` call: allowed under every permission preset, so a headless turn
/// runs it without a permission ask.
fn read_call(index: usize, call_id: &str, path: &str) -> String {
    block_start(
        index,
        &format!(
            r#"{{"type":"tool_use","id":"{call_id}","name":"read","input":{{"file_path":"{path}"}}}}"#
        ),
    ) + &block_stop(index)
}

// ---------------------------------------------------------------------------
// Blocking HTTP fixture
// ---------------------------------------------------------------------------

/// One scripted answer: a status, a body and the content type it ships with.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Reply {
    fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body,
        }
    }

    fn error(status: u16) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: r#"{"type":"error","error":{"type":"invalid_request_error","message":"boom"}}"#
                .into(),
        }
    }
}

/// Serves `replies` in order, records every request body, and closes each
/// connection so the SSE stream reaches EOF.
struct Provider {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Provider {
    fn start(replies: Vec<Reply>) -> Self {
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
                captured.lock().expect("request lock").push(body);
                let head = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len(),
                );
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

    fn bodies(&self) -> Vec<String> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one request head plus its `content-length` body.
fn read_request(socket: &mut TcpStream) -> Option<String> {
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
// The front end under test
// ---------------------------------------------------------------------------

struct AskRun {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// One machine-local run: its own `$ABYLAB_HOME` and workspace, so nothing of
/// the developer's own abylab is read or written.
struct Sandbox {
    home: PathBuf,
    workspace: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "aby-ask-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let home = root.join("home");
        let workspace = root.join("work");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&workspace).expect("workspace");
        Self { home, workspace }
    }

    /// Run the real binary, with `args` after `ask …`. `key` is the
    /// `--api-key` this run carries, if any.
    fn ask(&self, question: &[&str], base_url: &str, key: Option<&str>) -> AskRun {
        let mut command = Command::new(env!("CARGO_BIN_EXE_abylab"));
        command.arg("ask").args(question);
        command
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--base-url")
            .arg(base_url)
            .env("ABYLAB_HOME", &self.home)
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(key) = key {
            command.arg("--api-key").arg(key);
        }
        wait(command.spawn().expect("spawn abylab"))
    }
}

/// Wait for the child, reading both pipes on their own threads so a full pipe
/// buffer cannot deadlock the run. A child that outstays `PATIENCE` is killed
/// and reported as wedged, never as a pass.
fn wait(mut child: std::process::Child) -> AskRun {
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PATIENCE;
    let status = loop {
        match child.try_wait().expect("poll abylab") {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("abylab ask never exited");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    AskRun {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

/// Only the answer is the answer: a preamble before a tool call gives way to
/// the message the turn ended on, the model's thinking stays out of stdout, and
/// the tool itself runs — with no interface to authorize it.
#[test]
fn ask_prints_the_final_answer_and_nothing_else() {
    let sandbox = Sandbox::new("answer");
    std::fs::write(sandbox.workspace.join("notes.txt"), "scratch notes\n").expect("fixture file");
    let provider = Provider::start(vec![
        Reply::sse(sse(
            "msg-1",
            &[
                thinking_block(0, "I should read the notes"),
                text_block(1, "let me read the notes first"),
                read_call(2, "call-1", "notes.txt"),
            ],
            "tool_use",
        )),
        Reply::sse(sse(
            "msg-2",
            &[streamed_text(0, &["the answer ", "is ", "42"])],
            "end_turn",
        )),
    ]);

    let run = sandbox.ask(
        &["what", "is", "the answer"],
        &provider.base_url,
        Some("sk-fixture"),
    );

    assert_eq!(run.status.code(), Some(0), "stderr: {}", run.stderr);
    assert_eq!(
        run.stdout, "the answer is 42\n",
        "stdout carries the answer, in the order the deltas arrived"
    );
    assert_eq!(run.stderr, "", "a clean run says nothing on stderr");

    // The unwrapped words were one question, and the second request proves the
    // tool call really ran inside the same turn.
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 2, "one tool call, then the answer");
    assert!(
        bodies[0].contains("what is the answer"),
        "the question reaches the provider: {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("scratch notes"),
        "the tool result is in the turn: {}",
        bodies[1]
    );
}

/// An answer cut off at the output limit is still printed — it is what the
/// model said — but the run is not a success.
#[test]
fn a_truncated_answer_exits_non_zero() {
    let sandbox = Sandbox::new("truncated");
    let provider = Provider::start(vec![Reply::sse(sse(
        "msg-1",
        &[text_block(0, "half an ans")],
        "max_tokens",
    ))]);

    let run = sandbox.ask(&["tell", "me"], &provider.base_url, Some("sk-fixture"));

    assert_eq!(run.status.code(), Some(1), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "half an ans\n");
    assert!(
        run.stderr.contains("output limit"),
        "the cut-off is named: {}",
        run.stderr
    );
}

/// A failed turn leaves stdout without an answer: a half-written message is not
/// one, and a script must not mistake it for the reply.
#[test]
fn a_failed_turn_names_the_reason_on_stderr() {
    let sandbox = Sandbox::new("failed");
    let provider = Provider::start(vec![Reply::error(400)]);

    let run = sandbox.ask(&["anything"], &provider.base_url, Some("sk-fixture"));

    assert_eq!(run.status.code(), Some(1));
    assert_eq!(run.stdout, "", "a failure is not an answer");
    assert!(
        run.stderr.contains("Error"),
        "the reason is reported: {}",
        run.stderr
    );
}

/// Without a key the run is refused before a session is opened: the provider
/// must see no request at all.
#[test]
fn ask_without_a_key_never_reaches_the_provider() {
    let sandbox = Sandbox::new("nokey");
    let provider = Provider::start(vec![Reply::sse(sse(
        "msg-1",
        &[text_block(0, "unreachable")],
        "end_turn",
    ))]);

    let run = sandbox.ask(&["hello"], &provider.base_url, None);

    assert_eq!(run.status.code(), Some(1));
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr.contains("no API key"),
        "the fix is named: {}",
        run.stderr
    );
    assert_eq!(provider.bodies().len(), 0, "nothing was asked");
}

/// The session is as durable as any other: the same `--session-id` twice
/// continues one conversation rather than starting another, and the replayed
/// history still does not leak into stdout.
#[test]
fn the_same_session_id_continues_the_conversation() {
    let sandbox = Sandbox::new("resume");
    let first = Provider::start(vec![Reply::sse(sse(
        "msg-1",
        &[text_block(0, "42")],
        "end_turn",
    ))]);
    let run = sandbox.ask(
        &["--session-id", "aby-ask-test", "what is the answer"],
        &first.base_url,
        Some("sk-fixture"),
    );
    assert_eq!(run.status.code(), Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "42\n");

    let second = Provider::start(vec![Reply::sse(sse(
        "msg-2",
        &[text_block(0, "still 42")],
        "end_turn",
    ))]);
    let run = sandbox.ask(
        &["--session-id", "aby-ask-test", "are you sure"],
        &second.base_url,
        Some("sk-fixture"),
    );
    assert_eq!(run.status.code(), Some(0), "stderr: {}", run.stderr);
    assert_eq!(run.stdout, "still 42\n", "only the new answer is printed");
    let bodies = second.bodies();
    assert!(
        bodies[0].contains("what is the answer") && bodies[0].contains("are you sure"),
        "the resumed turn carries the conversation: {}",
        bodies[0]
    );
}
