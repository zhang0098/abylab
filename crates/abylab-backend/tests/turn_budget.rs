//! Regression: a turn that exhausts its per-run HTTP or tool budget must
//! continue instead of stopping dead, and a transient request failure must
//! retry the unfinished step inside the same open turn.
//!
//! The driver runs against a scripted Anthropic-Messages endpoint (see
//! `common`). With a one-request budget, the first response asks for a tool
//! call; the tool runs, the follow-up model request cannot be reserved, and the
//! segment ends with `BudgetExceeded`. Auto-continuation grants a fresh
//! segment, the second request is answered, and the turn completes — the exact
//! shape of the `web_search`-heavy turns that used to die on abycore's
//! 16-request default.

mod common;

use abylab_backend::TurnLimits;
use common::{
    Reply, Scenario, drive, drive_prompt, text_reply, tool_call_reply, two_tool_call_reply,
};
use std::time::Duration;

/// Request budget: the tool call forces a second reservation that trips the
/// one-request segment budget; the continuation ships the follow-up request.
#[test]
fn a_request_budget_stop_continues_the_turn_instead_of_ending_it() {
    let run = drive_prompt(
        "budget",
        TurnLimits {
            max_requests: 1,
            max_tool_calls: 4,
            continuations: 2,
            run_timeout: std::time::Duration::from_secs(600),
            tool_timeout: std::time::Duration::from_secs(60),
        },
        vec![
            Reply::sse(tool_call_reply("call-1")),
            Reply::sse(text_reply()),
        ],
    );

    let notices = run.continuations();
    assert_eq!(notices.len(), 1, "one continuation: {}", run.explain());
    assert!(
        notices[0].contains("(1/2)") && notices[0].contains("自动续跑"),
        "the notice counts the continuation: {notices:?}"
    );
    assert!(
        run.ended("completed"),
        "the continued turn completes: {}",
        run.explain()
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(
        run.count(),
        2,
        "segment one stops at one request; the continuation ships the second: {}",
        run.explain()
    );
}

/// Tool budget: the stop happens with a pending call the model never saw run,
/// so the driver settles it as an error result before continuing.
#[test]
fn a_tool_budget_stop_settles_the_pending_batch_and_continues() {
    let run = drive_prompt(
        "tool-budget",
        TurnLimits {
            max_requests: 4,
            max_tool_calls: 1,
            continuations: 2,
            run_timeout: std::time::Duration::from_secs(600),
            tool_timeout: std::time::Duration::from_secs(60),
        },
        vec![Reply::sse(two_tool_call_reply()), Reply::sse(text_reply())],
    );

    assert_eq!(
        run.continuations().len(),
        1,
        "one continuation: {}",
        run.explain()
    );
    assert!(
        run.ended("completed"),
        "the continued turn completes: {}",
        run.explain()
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(
        run.count(),
        2,
        "the continuation resumes the same turn: {}",
        run.explain()
    );
    assert!(run.requests[1].contains("did not execute"));
}

/// Transient failure, the harness's `dsh-llm-retry` case: every provider
/// attempt inside the segment is rate limited, and the turn continues with the
/// same open step instead of asking the user to retype the prompt.
#[test]
fn a_transient_failure_retries_the_unfinished_step_in_the_same_turn() {
    let limited = || Reply::status(429).header("retry-after", "0");
    let run = drive_prompt(
        "transient",
        TurnLimits {
            max_requests: 1000,
            max_tool_calls: 1000,
            continuations: 2,
            run_timeout: std::time::Duration::from_secs(600),
            tool_timeout: std::time::Duration::from_secs(60),
        },
        vec![limited(), limited(), limited(), Reply::sse(text_reply())],
    );

    assert_eq!(
        run.continuations().len(),
        1,
        "one continuation: {}",
        run.explain()
    );
    assert!(
        run.ended("completed"),
        "the retried turn completes: {}",
        run.explain()
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(
        run.count(),
        4,
        "three provider attempts inside the segment, then the continued step"
    );
}

/// The window is a gap between two moments of work, not a cap on the turn: a
/// segment that keeps running tool after tool outlives it without a single
/// continuation. This is the shape that used to die at a fixed wall-clock mark.
#[cfg(unix)]
#[test]
fn a_segment_that_keeps_working_outlives_the_window() {
    let slow = |id: &str| {
        Reply::sse(common::single_call_reply(
            id,
            "bash",
            &format!(r#"{{"command":"sleep 0.5","description":"Keep the run moving ({id})"}}"#),
        ))
    };
    let run = drive_prompt(
        "window-progress",
        TurnLimits {
            run_timeout: Duration::from_secs(1),
            continuations: 1,
            ..TurnLimits::default()
        },
        vec![
            slow("call-1"),
            slow("call-2"),
            slow("call-3"),
            Reply::sse(text_reply()),
        ],
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    assert!(
        run.continuations().is_empty(),
        "nothing stalled: {}",
        run.explain()
    );
    assert_eq!(run.count(), 4, "{}", run.explain());
    assert_eq!(
        run.events
            .iter()
            .filter(|e| e.starts_with("tool-result:"))
            .count(),
        3,
        "every step ran: {}",
        run.explain()
    );
}

/// An expired segment must get the same continuation headroom as a budget
/// stop, without adding another user prompt or ending the visible turn.
#[test]
fn a_run_deadline_continues_the_unfinished_turn() {
    let run = drive_prompt(
        "deadline-continue",
        TurnLimits {
            run_timeout: Duration::from_secs(1),
            continuations: 1,
            ..TurnLimits::default()
        },
        vec![
            Reply::sse(text_reply()).delayed(Duration::from_millis(1200)),
            Reply::sse(text_reply()),
        ],
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.continuations().len(), 1, "{}", run.explain());
    assert!(run.continuations()[0].contains("(1/1)"));
    assert_eq!(run.count(), 2, "{}", run.explain());
    assert_eq!(run.requests[0], run.requests[1], "retry the same input");
    assert_eq!(
        run.events
            .iter()
            .filter(|e| e.starts_with("turn-end:"))
            .count(),
        1,
        "the segment timeout must not end the visible turn"
    );
}

/// Completed tools remain in history when the follow-up model request times
/// out. A continuation must not execute those calls a second time.
#[test]
fn a_run_deadline_preserves_completed_tool_results() {
    let run = drive_prompt(
        "deadline-after-tool",
        TurnLimits {
            run_timeout: Duration::from_secs(1),
            continuations: 1,
            ..TurnLimits::default()
        },
        vec![
            Reply::sse(tool_call_reply("call-1")),
            Reply::sse(text_reply()).delayed(Duration::from_millis(1200)),
            Reply::sse(text_reply()),
        ],
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 3, "{}", run.explain());
    assert_eq!(run.requests[1], run.requests[2]);
    assert_eq!(
        run.events
            .iter()
            .filter(|e| *e == "tool-started:call-1")
            .count(),
        1,
        "a completed call executes only once"
    );
}

/// A tool call runs under its own budget, so the turn's window never cuts it
/// short — but Esc does. A call stopped mid-execution may have changed the
/// workspace: the next model request must say its result is unverified, never
/// that it did not execute, and the driver must not replay the side effect.
#[cfg(unix)]
#[test]
fn a_tool_interrupted_mid_run_requires_verification_before_repeating() {
    let run = drive(
        Scenario::new(
            "interrupted-tool",
            vec![
                Reply::sse(common::single_call_reply(
                    "slow-call",
                    "bash",
                    r#"{"command":"printf once >> interrupt-marker; sleep 30","description":"Record a side effect, then wait"}"#,
                )),
                Reply::sse(common::single_call_reply(
                    "verify-call",
                    "read",
                    r#"{"file_path":"interrupt-marker"}"#,
                )),
                Reply::sse(text_reply()),
            ],
        )
        .limits(TurnLimits {
            run_timeout: Duration::from_secs(600),
            continuations: 1,
            ..TurnLimits::default()
        })
        .interrupted("run a slow tool", "tool-started:slow-call")
        .prompt("carry on"),
    );

    assert!(run.ended("interrupted"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    let resumed: serde_json::Value = serde_json::from_str(&run.requests[1]).unwrap();
    let result = &resumed["messages"].as_array().unwrap().last().unwrap()["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "slow-call");
    assert_eq!(result["is_error"], true);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    assert!(text.contains("unverified"), "{text}");
    assert!(text.contains("check before repeating"), "{text}");
    assert!(!text.contains("did not execute"), "{text}");
    let verified: serde_json::Value = serde_json::from_str(&run.requests[2]).unwrap();
    let result = &verified["messages"].as_array().unwrap().last().unwrap()["content"][0];
    assert_eq!(result["tool_use_id"], "verify-call");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("once"), "{text}");
    assert!(!text.contains("onceonce"), "{text}");
    assert_eq!(
        run.events
            .iter()
            .filter(|e| *e == "tool-started:slow-call")
            .count(),
        1,
        "the interrupted call is never replayed"
    );
}

/// A persistently hung provider still stops once the configured continuation
/// headroom is spent; timeouts must not start an unlimited retry loop.
#[test]
fn repeated_run_deadlines_exhaust_the_continuation_limit() {
    let run = drive_prompt(
        "deadline-exhausted",
        TurnLimits {
            run_timeout: Duration::from_secs(1),
            continuations: 1,
            ..TurnLimits::default()
        },
        (0..2)
            .map(|_| Reply::sse(text_reply()).delayed(Duration::from_millis(1200)))
            .collect(),
    );

    assert!(run.ended("error"), "{}", run.explain());
    assert_eq!(run.continuations().len(), 1, "{}", run.explain());
    assert_eq!(run.count(), 2, "{}", run.explain());
    let error = run.events.iter().find(|e| e.starts_with("error:")).unwrap();
    assert!(error.contains("ABY_TURN_TIMEOUT=1"), "{error}");
    assert!(error.contains("ABY_TOOL_TIMEOUT=60"), "{error}");
    assert!(error.contains("ABY_AUTO_CONTINUE=1"), "{error}");
    assert!(error.contains("继续输入"), "{error}");
}

/// Turning off automatic continuation keeps a deadline terminal and leaves
/// the session resumable by the user.
#[test]
fn the_run_deadline_ends_a_hung_segment() {
    let run = drive_prompt(
        "deadline",
        TurnLimits {
            run_timeout: std::time::Duration::from_secs(1),
            continuations: 0,
            ..TurnLimits::default()
        },
        vec![Reply::sse(text_reply()).delayed(std::time::Duration::from_secs(3))],
    );

    assert!(
        run.ended("error"),
        "the hung segment ends: {}",
        run.explain()
    );
    let error = run
        .events
        .iter()
        .find(|event| event.starts_with("error:"))
        .expect("a timeout is reported");
    assert!(
        error.contains("timed out") && error.contains("继续输入"),
        "the timeout text says the turn is ours to continue: {error}"
    );
    assert!(run.continuations().is_empty(), "{}", run.explain());
    assert_eq!(run.count(), 1);
}

/// Exhausted headroom: the budget error must still end the turn cleanly (no
/// hang, no panic) and name the flags that raise the limit.
#[test]
fn an_exhausted_budget_ends_the_turn_with_an_actionable_error() {
    let run = drive_prompt(
        "budget-stop",
        TurnLimits {
            max_requests: 1,
            max_tool_calls: 4,
            continuations: 0,
            run_timeout: std::time::Duration::from_secs(600),
            tool_timeout: std::time::Duration::from_secs(60),
        },
        vec![Reply::sse(tool_call_reply("call-1"))],
    );

    assert!(
        run.ended("error"),
        "the turn ends as an error: {}",
        run.explain()
    );
    let error = run
        .events
        .iter()
        .find(|event| event.starts_with("error:"))
        .expect("the budget error is reported");
    assert!(error.contains("--max-requests"), "{error}");
    assert!(error.contains("--max-tool-calls"), "{error}");
    assert!(
        run.continuations().is_empty(),
        "continuations=0 disables auto-continuation: {}",
        run.explain()
    );
    assert_eq!(run.count(), 1, "the stop ships no extra request");
}
