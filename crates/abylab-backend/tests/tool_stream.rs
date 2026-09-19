//! The live tool line: a call's arguments reach the TUI while the model is
//! still writing them, so a card shows its command instead of a bare tool name.
//!
//! The driver runs against a scripted Anthropic-Messages endpoint (see
//! `common`) that streams the tool call as `input_json_delta`s. The emitted
//! events must arrive in stream order — deltas, completed arguments, execution
//! start, result — all under one call id.

mod common;

use abylab_backend::TurnLimits;
use common::{Reply, drive_prompt, streamed_call_reply, text_reply};

#[test]
fn streamed_tool_arguments_precede_execution_under_one_call_id() {
    let run = drive_prompt(
        "tool-stream",
        TurnLimits::default(),
        vec![
            Reply::sse(streamed_call_reply(
                "call-1",
                "read",
                &[r#"{"file_path":"#, r#""missing.txt"}"#],
            )),
            Reply::sse(text_reply()),
        ],
    );

    let line: Vec<&String> = run
        .events
        .iter()
        .filter(|event| event.starts_with("tool-"))
        .collect();
    assert_eq!(
        line,
        vec![
            r#"tool-delta:call-1:{"file_path":"#,
            r#"tool-delta:call-1:"missing.txt"}"#,
            r#"tool-call:call-1:{"file_path":"missing.txt"}"#,
            "tool-started:call-1",
            "tool-result:call-1",
        ],
        "stream order reaches the transcript: {}",
        run.explain()
    );
    assert!(run.ended("completed"), "{}", run.explain());
}
