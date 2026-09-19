//! Harness-style advisory loop guard: repeating the same call with identical
//! arguments gets the model a reminder, never a blocked or delayed call.
//!
//! Mirrors `dsh-repeat-tool-reminder` from deepseek-harness, which the base
//! bundle ships because the agent loop itself has no turn budget. abylab's
//! budgets are a distant backstop for the same reason, so the reminder is what
//! actually breaks a stuck loop.

mod common;

use abylab_backend::TurnLimits;
use common::{Reply, drive_prompt, text_reply, tool_call_reply};

/// Three identical calls in a row: the third queues the reminder, which rides
/// the next request and stays in history. The calls themselves all ran.
#[test]
fn a_repeated_identical_call_gets_a_reminder_in_the_next_request() {
    let run = drive_prompt(
        "loop-guard",
        TurnLimits::default(),
        vec![
            Reply::sse(tool_call_reply("call-1")),
            Reply::sse(tool_call_reply("call-2")),
            Reply::sse(tool_call_reply("call-3")),
            Reply::sse(text_reply()),
        ],
    );

    assert!(
        run.ended("completed"),
        "the guard never blocks the turn: {}",
        run.explain()
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 4, "every call ran and was answered");
    assert!(
        run.requests[..3]
            .iter()
            .all(|body| !body.contains("Loop guard")),
        "no reminder is injected before the third identical call completes"
    );
    assert!(
        run.requests[3].contains("Loop guard") && run.requests[3].contains("3 times"),
        "the third repeat queues a reminder for the request after it: {}",
        run.requests[3]
    );
    assert!(
        run.requests[3].contains("missing.txt"),
        "and the tool result it comments on is unchanged"
    );
}

/// A different call resets the streak: two pairs are not a run of four.
#[test]
fn a_different_call_resets_the_repeat_count() {
    let different =
        || common::single_call_reply("call-other", "read", r#"{"file_path":"other.txt"}"#);
    let run = drive_prompt(
        "loop-guard-reset",
        TurnLimits::default(),
        vec![
            Reply::sse(tool_call_reply("call-1")),
            Reply::sse(tool_call_reply("call-2")),
            Reply::sse(different()),
            Reply::sse(tool_call_reply("call-3")),
            Reply::sse(tool_call_reply("call-4")),
            Reply::sse(text_reply()),
        ],
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(
        !run.requests.iter().any(|body| body.contains("Loop guard")),
        "the interrupted streak never reaches a threshold"
    );
}
