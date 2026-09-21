//! Host context compaction: abylab condenses older history before a turn when
//! the estimate approaches the configured window, and on demand with
//! `/compact` — never rewriting the durable transcript.

mod common;

use abylab_backend::{CompactionConfig, TurnLimits};
use common::{Reply, Scenario, Step, drive, single_call_reply};

/// A big file keeps the newest history worth retaining, so the oldest span
/// (the long user prompt) is what gets condensed.
const BIG_FILE: &str = "big.txt";
const SUMMARY: &str = "## Current Work\n- the user read big.txt";

fn big_file(bytes: usize) -> String {
    "0123456789abcdef".repeat(bytes / 16)
}

fn policy() -> CompactionConfig {
    CompactionConfig {
        // Small enough that the seed conversation crosses the threshold, large
        // enough that an empty session does not.
        context_window: 4_000,
        compact_at: 0.8,
        keep_recent: 0.1,
        max_tokens: 4096,
        prune_tool_bytes: 0,
    }
}

/// Automatic compaction: a request after the threshold is crossed carries a
/// summary instead of the shadowed span, and the turn still completes.
#[test]
fn automatic_compaction_condenses_at_a_request_boundary() {
    let run = drive(
        Scenario::new(
            "auto-compact",
            vec![
                // Turn one: read the big file, then answer.
                Reply::sse(single_call_reply(
                    "call-1",
                    "read",
                    r#"{"file_path":"big.txt"}"#,
                )),
                Reply::sse(common::text_body("msg-done-1", "done")),
                // The condensation call.
                Reply::sse(common::text_body("msg-summary", SUMMARY)),
                // Turn two.
                Reply::sse(common::text_body("msg-done-2", "second turn")),
            ],
        )
        .compaction(policy())
        .seed(|workspace| {
            std::fs::write(workspace.join(BIG_FILE), big_file(8_000)).expect("seed big.txt");
        })
        .step(Step::Prompt(format!(
            "read {BIG_FILE} and tell me what it contains, in full detail: {}",
            "explain every part. ".repeat(300)
        )))
        .prompt("and now answer with one word"),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    let notices = run.compactions();
    assert_eq!(notices.len(), 1, "one condensation: {}", run.explain());
    assert!(
        notices[0].contains("compacted") && notices[0].contains("已把"),
        "the notice is bilingual and names the work: {notices:?}"
    );
    assert_eq!(
        run.count(),
        4,
        "turn one, the condensation call, turn two (no hidden extra requests): {}",
        run.explain()
    );
    let summary_request = run
        .requests
        .iter()
        .find(|body| body.contains("compaction engine"))
        .unwrap();
    let summary_request: serde_json::Value = serde_json::from_str(summary_request).unwrap();
    assert_eq!(summary_request["max_tokens"], policy().max_tokens);
    let last = run
        .requests
        .last()
        .expect("the second turn ships a request");
    assert!(
        last.contains("Current Work"),
        "the summary is in the model's view: {last}"
    );
    assert!(
        !last.contains("tell me what it contains"),
        "the condensed prompt is out of the model's view"
    );
}

/// `/compact` condenses below the threshold when the host asks, and reports how
/// much history it replaced.
#[test]
fn the_compact_command_condenses_on_demand() {
    let run = drive(
        Scenario::new(
            "manual-compact",
            vec![
                Reply::sse(single_call_reply(
                    "call-1",
                    "read",
                    r#"{"file_path":"big.txt"}"#,
                )),
                Reply::sse(common::text_body("msg-done-1", "done")),
                Reply::sse(common::text_body("msg-summary", SUMMARY)),
                Reply::sse(common::text_body("msg-done-2", "second turn")),
            ],
        )
        // No automatic policy: only the explicit command runs.
        .limits(TurnLimits::default())
        .seed(|workspace| {
            std::fs::write(workspace.join(BIG_FILE), big_file(8_000)).expect("seed big.txt");
        })
        .step(Step::Prompt(format!(
            "read {BIG_FILE}, please look carefully: {}",
            "take your time. ".repeat(300)
        )))
        .step(Step::Compact)
        .prompt("summarize what you know"),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    let notices = run.compactions();
    assert_eq!(
        notices.len(),
        1,
        "the command reports one condensation: {}",
        run.explain()
    );
    assert!(
        notices[0].starts_with("op-done:compacted"),
        "the manual notice is the command result: {notices:?}"
    );
    let last = run.requests.last().expect("the third step ships a request");
    assert!(last.contains("Current Work"), "{last}");
}

/// The pruner runs before any summary call: when trimming the older tool
/// outputs is enough, no model request is spent on condensation.
#[test]
fn the_pruner_can_avoid_the_summary_call_entirely() {
    // Many short lines: the read tool caps a single line at 2k chars but a whole
    // result at 50 KiB, so this stays a genuinely oversized tool output.
    let huge = "0123456789abcdef\n".repeat(4_000);
    let read_call =
        |call_id: &str| single_call_reply(call_id, "read", r#"{"file_path":"big.txt"}"#);
    let run = drive(
        Scenario::new(
            "prune-only",
            vec![
                // Two big reads in two turns.
                Reply::sse(read_call("call-1")),
                Reply::sse(common::text_body("msg-done-1", "first done")),
                Reply::sse(read_call("call-2")),
                Reply::sse(common::text_body("msg-done-2", "second done")),
                // Turn three must ship pruned history without a summary call.
                Reply::sse(common::text_body("msg-done-3", "third")),
            ],
        )
        .compaction(CompactionConfig {
            // Room for the base envelope plus one full tool result, not two.
            context_window: 30_000,
            compact_at: 0.8,
            keep_recent: 0.02,
            max_tokens: 4096,
            prune_tool_bytes: 512,
        })
        .seed(move |workspace| {
            std::fs::write(workspace.join(BIG_FILE), &huge).expect("seed big.txt");
        })
        .prompt("read big.txt")
        .prompt("read big.txt again")
        .prompt("what did the first file say?"),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    let notices = run.compactions();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("trimmed old tool outputs")),
        "the pruner alone sufficed: {notices:?} / {} / request bytes: {:?}",
        run.explain(),
        run.requests.iter().map(String::len).collect::<Vec<_>>()
    );
    assert!(
        !notices
            .iter()
            .any(|notice| notice.contains("into a summary")),
        "no summary ran: {notices:?}"
    );
    assert_eq!(
        run.count(),
        5,
        "three turns, no condensation request: {}",
        run.explain()
    );
    let last = run.requests.last().expect("the third turn ships a request");
    assert!(
        last.contains("pruned for context"),
        "the model sees the trimmed output: {last}"
    );
}

/// Mid-turn condensation, harness's `agent/pre-step` behavior: one long turn
/// with three tool round trips is condensed between steps, so the turn finishes
/// instead of dying on the window.
#[test]
fn a_long_turn_is_condensed_mid_turn() {
    let big = "0123456789abcdef\n".repeat(4_000);
    let read = |call_id: &str, path: &str| {
        single_call_reply(call_id, "read", &format!(r#"{{"file_path":"{path}"}}"#))
    };
    let run = drive(
        Scenario::new(
            "mid-turn",
            vec![
                Reply::sse(read("call-1", "big1.txt")),
                Reply::sse(read("call-2", "big2.txt")),
                // The condensation call, between the second step and the answer.
                Reply::sse(common::text_body("msg-summary", SUMMARY)),
                Reply::sse(common::text_body("msg-done", "all read")),
            ],
        )
        .compaction(CompactionConfig {
            context_window: 30_000,
            compact_at: 0.8,
            keep_recent: 0.02,
            max_tokens: 4096,
            prune_tool_bytes: 0,
        })
        .seed(move |workspace| {
            for name in ["big1.txt", "big2.txt"] {
                std::fs::write(workspace.join(name), &big).expect("seed file");
            }
        })
        .prompt("read both files and compare them, carefully"),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(
        run.events
            .iter()
            .filter(|event| event.starts_with("turn-start:"))
            .count(),
        1,
        "everything happened inside one turn: {}",
        run.explain()
    );
    assert_eq!(
        run.compactions().len(),
        1,
        "one mid-turn condensation: {}",
        run.explain()
    );
    assert_eq!(
        run.count(),
        4,
        "two steps, the condensation, the answer: {}",
        run.explain()
    );
    // Request 3 is the condensation call: it replays the span *and* the current
    // instruction, which is what lets the summary carry the task forward.
    assert!(
        run.requests[2].contains("compaction engine")
            && run.requests[2].contains("big1.txt")
            && run.requests[2].contains("read both files"),
        "the summarizer sees the span and the instruction"
    );
    assert!(
        run.requests[3].contains("Current Work"),
        "the step after the condensation carries the summary: {}",
        run.requests[3]
    );
    assert!(
        !run.requests[3].contains("big1.txt"),
        "and the condensed span is out of view"
    );
    assert!(
        run.requests[3].contains("big2.txt"),
        "while the recent tool result stays verbatim: {}",
        run.requests[3]
    );
}

/// Provider calibration changes the trigger: the same conversation compacts
/// with the fixture's implausible counters and stays untouched once every
/// provider count is realistic for the envelope it measured.
#[test]
fn calibration_keeps_a_conversation_below_the_threshold() {
    let huge = "0123456789abcdef\n".repeat(300); // ~5 KiB tool result
    let policy = CompactionConfig {
        context_window: 5_000,
        compact_at: 0.8,
        keep_recent: 0.1,
        max_tokens: 4096,
        prune_tool_bytes: 0,
    };
    let long_prompt = format!(
        "read big.txt and report the first word: {}",
        "go. ".repeat(600)
    );

    // Realistic counts: the first request is ~10.9 KiB and the provider priced
    // it at 1900 tokens (~5.7 bytes/token), so the second step's estimate lands
    // well under the 4000-token threshold.
    let calibrated = drive(
        Scenario::new(
            "calibrated",
            vec![
                Reply::sse(common::single_call_reply_with_usage(
                    "call-1",
                    "read",
                    r#"{"file_path":"big.txt"}"#,
                    1_900,
                )),
                Reply::sse(common::text_body_with_usage("msg-done-1", "read it", 3_500)),
            ],
        )
        .compaction(policy)
        .seed({
            let huge = huge.clone();
            move |workspace: &std::path::Path| {
                std::fs::write(workspace.join("big.txt"), &huge).expect("seed big.txt");
            }
        })
        .prompt(long_prompt.clone()),
    );
    assert!(calibrated.ended("completed"), "{}", calibrated.explain());
    assert_eq!(
        calibrated.count(),
        2,
        "no condensation request: {}",
        calibrated.explain()
    );
    assert!(
        calibrated.compactions().is_empty(),
        "the provider count says the window is comfortable: {}",
        calibrated.explain()
    );

    // The same conversation with the fixture's 7-token counter: not plausible,
    // so the conservative byte bound takes over and the step is condensed.
    let conservative = drive(
        Scenario::new(
            "uncalibrated",
            vec![
                Reply::sse(common::single_call_reply(
                    "call-1",
                    "read",
                    r#"{"file_path":"big.txt"}"#,
                )),
                // The summarization call consumes this one.
                Reply::sse(common::text_body("msg-summary", "condensed")),
                Reply::sse(common::text_body("msg-done-1", "read it")),
            ],
        )
        .compaction(policy)
        .seed(move |workspace: &std::path::Path| {
            std::fs::write(workspace.join("big.txt"), &huge).expect("seed big.txt");
        })
        .prompt(long_prompt),
    );
    assert!(
        conservative.ended("completed"),
        "{}",
        conservative.explain()
    );
    assert_eq!(
        conservative.compactions().len(),
        1,
        "the conservative bound crosses the threshold: {}",
        conservative.explain()
    );
    assert_eq!(conservative.count(), 3);
}

/// A provider-confirmed context overflow is recovered by condensing and
/// retrying the same open turn, instead of ending with "/new".
#[test]
fn a_context_overflow_condenses_and_retries_the_turn() {
    let long_prompt = format!(
        "explain everything about this repository {}",
        "in detail. ".repeat(400)
    );
    let run = drive(
        Scenario::new(
            "overflow",
            vec![
                // The provider refuses the first request for context length.
                Reply::error(400, "context_length_exceeded"),
                // The condensation call.
                Reply::sse(common::text_body("msg-summary", SUMMARY)),
                // The retried turn.
                Reply::sse(common::text_body("msg-done", "recovered")),
            ],
        )
        .compaction(policy())
        .prompt(long_prompt),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    let notices = run.compactions();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("context overflow")),
        "the recovery is reported: {notices:?} / {}",
        run.explain()
    );
    assert_eq!(
        run.count(),
        3,
        "refusal, condensation, retried request: {}",
        run.explain()
    );
    let last = run.requests.last().expect("the retry ships a request");
    assert!(
        last.contains("Current Work"),
        "the summary is in view: {last}"
    );
    assert!(
        !last.contains("explain everything about this repository"),
        "the condensed prompt is out of view"
    );
}

/// A condensation whose summary does not shrink its span is refused by the SDK,
/// and the host reports the failure instead of pretending it compacted.
#[test]
fn a_useless_summary_leaves_the_history_intact() {
    let run = drive(
        Scenario::new(
            "compact-refused",
            vec![
                Reply::sse(single_call_reply(
                    "call-1",
                    "read",
                    r#"{"file_path":"big.txt"}"#,
                )),
                Reply::sse(common::text_body("msg-done-1", "done")),
                // A "summary" longer than the span it would replace.
                Reply::sse(common::text_body("msg-huge", &"SUMMARYFILLER ".repeat(800))),
                Reply::sse(common::text_body("msg-done-2", "carry on")),
            ],
        )
        .limits(TurnLimits::default())
        .seed(|workspace| {
            std::fs::write(workspace.join(BIG_FILE), big_file(8_000)).expect("seed big.txt");
        })
        .prompt("read big.txt")
        .step(Step::Compact)
        .prompt("carry on"),
    );

    assert!(run.ended("completed"), "{}", run.explain());
    let failed = run
        .events
        .iter()
        .any(|event| event.starts_with("op-failed:compaction failed"));
    assert!(
        failed,
        "the non-shrinking summary is reported, not applied: {}",
        run.explain()
    );
    let last = run.requests.last().expect("last request");
    assert!(
        !last.contains("SUMMARYFILLER"),
        "the refused summary never reaches the model"
    );
    assert!(
        last.contains("read big.txt"),
        "the original history still does: {last}"
    );
}

#[test]
fn automatic_pruning_advances_as_more_tool_results_arrive() {
    let mut replies = Vec::new();
    for index in 0..4 {
        replies.push(Reply::sse(single_call_reply(
            &format!("read-{index}"),
            "read",
            r#"{"file_path":"big.txt"}"#,
        )));
        replies.push(Reply::sse(common::text_body(
            &format!("done-{index}"),
            "done",
        )));
    }
    replies.push(Reply::sse(common::text_body("last", "finished")));
    let mut scenario = Scenario::new("prune-advances", replies)
        .compaction(CompactionConfig {
            context_window: 30_000,
            compact_at: 0.8,
            keep_recent: 0.02,
            max_tokens: 4096,
            prune_tool_bytes: 512,
        })
        .seed(|workspace| {
            std::fs::write(workspace.join("big.txt"), "0123456789abcdef\n".repeat(4000)).unwrap()
        });
    for _ in 0..4 {
        scenario = scenario.prompt("read big.txt again");
    }
    let run = drive(scenario.prompt("finish"));
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(
        run.count(),
        9,
        "pruning avoids paying for summaries: {}",
        run.explain()
    );
    assert!(
        run.compactions()
            .iter()
            .filter(|notice| notice.contains("trimmed old tool outputs"))
            .count()
            >= 2,
        "the pruning boundary must advance: {}",
        run.explain()
    );
    assert!(
        !run.requests
            .iter()
            .any(|body| body.contains("compaction engine"))
    );
}

/// `compact_at = 0` is the documented "threshold off" value: it must never
/// trigger compaction (or the prune pass that rides the same boundary), not
/// fire at the first request.
#[test]
fn a_zero_compact_at_disables_the_threshold() {
    let off = CompactionConfig {
        compact_at: 0.0,
        ..policy()
    };
    assert_eq!(off.threshold_tokens(), u64::MAX);
    let on = CompactionConfig {
        context_window: 1_000,
        compact_at: 0.5,
        ..policy()
    };
    assert_eq!(on.threshold_tokens(), 500);
}
