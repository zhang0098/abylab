//! Goal rounds: one durable objective per session, and the host round driver
//! that keeps an armed goal moving — harness's `goal-round-driver`.

mod common;

use abylab_backend::TurnLimits;
use common::{Reply, Scenario, single_call_reply, text_body};

/// A round allowance that is spent without the model finishing the goal records
/// a blocker instead of looping, and the settle notice carries the final state.
#[test]
fn spent_rounds_block_the_goal_instead_of_looping() {
    let run = common::drive(
        Scenario::new(
            "goal-exhausted",
            vec![
                // Two rounds, each answered without touching the goal.
                Reply::sse(text_body("msg-r1", "still working")),
                Reply::sse(text_body("msg-r2", "still working")),
            ],
        )
        .limits(TurnLimits::default())
        .goal("@2 make the widget faster"),
    );

    let rounds: Vec<&String> = run
        .events
        .iter()
        .filter(|event| event.starts_with("op-done:goal round"))
        .collect();
    assert_eq!(
        rounds.len(),
        2,
        "exactly the allowance ran: {}",
        run.explain()
    );
    assert!(
        run.events
            .iter()
            .any(|event| event.starts_with("op-failed:goal rounds exhausted")),
        "exhaustion is reported: {}",
        run.explain()
    );
    let settled = run
        .events
        .iter()
        .find(|event| event.starts_with("op-done:goal settled"))
        .expect("a settle notice");
    assert!(
        settled.contains("blocked") && settled.contains("2/2"),
        "the settle notice carries the blocker: {settled}"
    );
    assert_eq!(run.count(), 2, "one request per round");
}

/// The model ends the loop itself: it reads the goal and completes it, and the
/// driver stops without spending the rest of the allowance.
#[test]
fn the_model_can_complete_the_goal() {
    let run = common::drive(
        Scenario::new(
            "goal-complete",
            vec![
                // Round one: read the goal, then complete it with the exact
                // id and revision the read returned (extracted dynamically).
                Reply::dynamic(|_| Reply::sse(single_call_reply("call-1", "get_goal", "{}"))),
                Reply::dynamic(|request| {
                    // The goal id is `goal-` + 16 hex digits, and the replayed
                    // transcript carries both it and the revision.
                    let goal_id = request
                        .split("goal-")
                        .nth(1)
                        .map(|rest| format!("goal-{}", rest.chars().take(16).collect::<String>()))
                        .expect("the goal id is in the replayed history");
                    let revision = request
                        .rsplit("\\\"revision\\\":")
                        .next()
                        .and_then(|rest| {
                            rest.trim_start_matches(|c: char| !c.is_ascii_digit())
                                .split(|c: char| !c.is_ascii_digit())
                                .next()
                        })
                        .filter(|digits| !digits.is_empty())
                        .expect("the goal revision is in the replayed history")
                        .to_string();
                    Reply::sse(single_call_reply(
                        "call-2",
                        "update_goal",
                        &format!(
                            r#"{{"goal_id":"{goal_id}","revision":{revision},"status":"complete","note":"shipped"}}"#
                        ),
                    ))
                }),
                Reply::sse(text_body("msg-r1", "the goal is complete")),
            ],
        )
        .limits(TurnLimits::default())
        .goal("@4 finish the migration"),
    );

    assert!(
        run.events.iter().any(|event| event == "turn-end:completed"),
        "{}",
        run.explain()
    );
    let rounds: Vec<&String> = run
        .events
        .iter()
        .filter(|event| event.starts_with("op-done:goal round"))
        .collect();
    assert_eq!(rounds.len(), 1, "one round was enough: {}", run.explain());
    assert!(
        run.events
            .iter()
            .any(|event| event.starts_with("op-done:goal settled") && event.contains("complete")),
        "the settle notice reports completion: {}",
        run.explain()
    );
    assert!(
        !run.events
            .iter()
            .any(|event| event.starts_with("op-failed:")),
        "nothing failed: {}",
        run.explain()
    );
}

/// The human controls the goal without spending a model turn: creating one from
/// the model's side is not armed, `/goal` is.
#[test]
fn the_host_controls_the_goal_lifecycle() {
    let run = common::drive(
        Scenario::new("goal-control", vec![])
            .limits(TurnLimits::default())
            .goal("status"),
    );
    assert!(
        run.events
            .iter()
            .any(|event| event.starts_with("op-failed:no goal is set")),
        "status with no goal says so: {}",
        run.explain()
    );
    assert_eq!(run.count(), 0, "no model request: {}", run.explain());

    let run = common::drive(
        Scenario::new(
            "goal-paused",
            // One round, then pause through the command surface.
            vec![Reply::sse(text_body("msg-r1", "working"))],
        )
        .limits(TurnLimits::default())
        .goal("@1 tidy the docs")
        .goal("pause"),
    );
    let settled = run
        .events
        .iter()
        .filter(|event| event.starts_with("op-done:goal settled"))
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 1, "{}", run.explain());
    assert!(settled[0].contains("blocked"), "{}", settled[0]);
    assert!(
        run.events
            .iter()
            .any(|event| event.starts_with("op-done:goal → ") && event.contains("paused")),
        "pause is confirmed: {}",
        run.explain()
    );
    assert_eq!(run.count(), 1, "pause costs no model request");
}

/// The round driver owns exactly one idle status: a `running:false` after
/// every round told the UI the session was idle while the driver was still
/// inside the goal loop (and it dispatched queued prompts early).
#[test]
fn goal_rounds_emit_one_idle_status_after_the_last_round() {
    let run = common::drive(
        Scenario::new(
            "goal-status",
            vec![
                Reply::sse(text_body("msg-r1", "still working")),
                Reply::sse(text_body("msg-r2", "still working")),
            ],
        )
        .goal("@2 make the widget faster"),
    );

    let statuses: Vec<&String> = run
        .events
        .iter()
        .filter(|event| event.starts_with("status:"))
        .collect();
    assert_eq!(
        statuses.last().map(|status| status.as_str()),
        Some("status:false"),
        "the sequence ends idle: {}",
        run.explain()
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| status.as_str() == "status:false")
            .count(),
        1,
        "one idle status for the whole sequence: {}",
        run.explain()
    );
    let last_end = run
        .events
        .iter()
        .rposition(|event| event.starts_with("turn-end:"))
        .expect("rounds ended");
    let last_status = run
        .events
        .iter()
        .rposition(|event| event.starts_with("status:"))
        .expect("a status");
    assert!(
        last_status > last_end,
        "idle comes after the last round: {}",
        run.explain()
    );
}
