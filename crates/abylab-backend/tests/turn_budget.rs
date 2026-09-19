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
use common::{Reply, drive_prompt, text_reply, tool_call_reply, two_tool_call_reply};

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

/// The segment deadline is the limit that binds once the budgets are
/// backstops: a provider that never answers ends the turn with a timeout the
/// user can act on, instead of hanging.
#[test]
fn the_run_deadline_ends_a_hung_segment() {
    let run = drive_prompt(
        "deadline",
        TurnLimits {
            run_timeout: std::time::Duration::from_secs(1),
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
