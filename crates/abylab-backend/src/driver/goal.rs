//! Goal command parsing and host-owned continuation. The Agent stays single-writer.

use super::{CtlEvent, SessionAgent, TurnCtx};
use abycore::{Goal, GoalStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GoalControl {
    Status,
    Pause,
}

impl GoalControl {
    pub(super) fn parse(arg: &str) -> Option<Self> {
        match arg.trim() {
            "" | "status" => Some(Self::Status),
            "pause" => Some(Self::Pause),
            _ => None,
        }
    }

    pub(super) fn command(self) -> super::Cmd {
        super::Cmd::Goal {
            arg: match self {
                Self::Status => "status",
                Self::Pause => "pause",
            }
            .into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalCommand<'a> {
    Control(GoalControl),
    Resume(Option<u64>),
    Rounds(Option<u64>),
    Complete,
    Clear,
    Create {
        objective: &'a str,
        rounds: Option<u64>,
    },
}

impl<'a> GoalCommand<'a> {
    pub(super) fn parse(arg: &'a str) -> Result<Self, &'static str> {
        let mut arg = arg.trim();
        let mut rounds = None;
        if let Some(prefixed) = arg.strip_prefix('@') {
            let (number, rest) = prefixed
                .split_once(char::is_whitespace)
                .unwrap_or((prefixed, ""));
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                rounds = Some(parse_rounds(number)?);
                arg = rest.trim();
                if arg.is_empty() {
                    return Err("usage: /goal @<total rounds> <objective|resume>");
                }
            }
        }
        if arg == "resume" {
            return Ok(Self::Resume(rounds));
        }
        let command = match arg {
            "" | "status" => Self::Control(GoalControl::Status),
            "pause" => Self::Control(GoalControl::Pause),
            "complete" => Self::Complete,
            "clear" => Self::Clear,
            "rounds" => return Err("usage: /goal rounds <total rounds|off>"),
            _ => {
                if let Some(("rounds", value)) = arg.split_once(char::is_whitespace) {
                    Self::Rounds(if value.trim() == "off" {
                        None
                    } else {
                        Some(parse_rounds(value.trim())?)
                    })
                } else {
                    return Ok(Self::Create {
                        objective: arg,
                        rounds,
                    });
                }
            }
        };
        if rounds.is_some() {
            return Err("@<total rounds> is only valid with an objective or resume");
        }
        Ok(command)
    }
}

fn parse_rounds(value: &str) -> Result<u64, &'static str> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("goal total rounds must be a positive integer / 目标总轮数必须为正整数")
}

pub(super) fn report(goal: Option<&Goal>, ctl: &impl Fn(CtlEvent)) {
    match goal {
        Some(goal) => ctl(CtlEvent::TuiOpDone(format!(
            "goal · {} / 目标 · {}",
            goal.summary(),
            goal.summary()
        ))),
        None => ctl(CtlEvent::TuiOpFailed(
            "no goal is set — /goal <objective> / 当前没有目标：/goal <目标>".into(),
        )),
    }
}

/// Returns whether the caller should start autonomous rounds after the saved mutation.
pub(super) fn apply(
    agent: &mut SessionAgent,
    command: GoalCommand<'_>,
    ctl: &impl Fn(CtlEvent),
) -> abycore::Result<bool> {
    if command == GoalCommand::Control(GoalControl::Status) {
        report(agent.goal(), ctl);
        return Ok(false);
    }
    let starts = matches!(command, GoalCommand::Create { .. } | GoalCommand::Resume(_));
    let message = match command {
        GoalCommand::Control(GoalControl::Status) => unreachable!(),
        GoalCommand::Control(GoalControl::Pause) => {
            // A pause arriving at completion must not reopen the completed goal.
            if !agent
                .goal()
                .is_some_and(|goal| goal.status == GoalStatus::Complete)
            {
                agent.update_goal(GoalStatus::Paused, Some("resume with /goal resume".into()))?;
            }
            None
        }
        GoalCommand::Resume(rounds) => {
            agent.resume_goal(rounds)?;
            None
        }
        GoalCommand::Rounds(rounds) => {
            agent.set_goal_rounds(rounds)?;
            None
        }
        GoalCommand::Complete => {
            agent.update_goal(GoalStatus::Complete, None)?;
            None
        }
        GoalCommand::Clear => {
            agent.clear_goal();
            Some("goal cleared / 目标已清除".to_string())
        }
        GoalCommand::Create { objective, rounds } => {
            agent.set_goal(objective, rounds)?;
            None
        }
    };
    if let Err(error) = agent.save() {
        if starts {
            agent.update_goal(
                GoalStatus::Paused,
                Some("goal save failed; not started".into()),
            )?;
        }
        return Err(error);
    }
    ctl(CtlEvent::TuiOpDone(message.unwrap_or_else(|| {
        let summary = agent
            .goal()
            .expect("goal mutation retained the goal")
            .summary();
        format!("goal → {summary} / 目标 → {summary}")
    })));
    Ok(starts)
}

/// Queries read the latest committed goal event while the run holds the Agent.
/// A pause cancels only an active goal; its acknowledgement follows the final save.
pub(super) fn during_turn(
    control: GoalControl,
    goal: Option<&Goal>,
    ctl: &impl Fn(CtlEvent),
) -> bool {
    if control == GoalControl::Pause && goal.is_some_and(|goal| goal.status == GoalStatus::Active) {
        true
    } else {
        report(goal, ctl);
        false
    }
}

pub(super) async fn drive_rounds(
    agent: &mut SessionAgent,
    ctx: &mut TurnCtx<'_>,
    ctl: &impl Fn(CtlEvent),
) {
    run_rounds(agent, ctx, ctl).await;
    // No return path may leave a stopped host loop advertising an active goal.
    if agent
        .goal()
        .is_some_and(|goal| goal.status == GoalStatus::Active)
    {
        let _ = agent.update_goal(
            GoalStatus::Paused,
            Some("goal run stopped; use /goal resume".into()),
        );
    }
    if let Err(error) = agent.save() {
        ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
    } else if let Some(goal) = agent.goal() {
        ctl(CtlEvent::TuiOpDone(format!(
            "goal settled · {} / 目标状态 · {}",
            goal.summary(),
            goal.summary()
        )));
    }
    super::emit_idle_status(ctx);
}

async fn run_rounds(agent: &mut SessionAgent, ctx: &mut TurnCtx<'_>, ctl: &impl Fn(CtlEvent)) {
    loop {
        while let Ok(control) = ctx.goal_rx.try_recv() {
            if let Err(error) = apply(agent, GoalCommand::Control(control), ctl) {
                ctl(CtlEvent::TuiOpFailed(format!(
                    "goal update failed: {error}"
                )));
                return;
            }
        }
        let Some(goal) = agent.goal() else { return };
        if goal.status != GoalStatus::Active {
            return;
        }
        if ctx.interrupt_rx.try_recv().is_ok() {
            let _ = agent.update_goal(
                GoalStatus::Paused,
                Some("interrupted; use /goal resume".into()),
            );
            return;
        }
        if goal.remaining_rounds() == Some(0) {
            let _ = agent.update_goal(
                GoalStatus::Blocked,
                Some("round allowance exhausted".into()),
            );
            if let Err(error) = agent.save() {
                ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
                return;
            }
            ctl(CtlEvent::TuiOpFailed("goal rounds exhausted — use /goal @<larger total> resume / 目标轮次用尽，请提高总配额后恢复".into()));
            return;
        }
        let Some(goal) = agent.begin_goal_round().ok().flatten() else {
            return;
        };
        if let Err(error) = agent.save() {
            ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
            return;
        }
        let total = goal
            .max_rounds
            .map(|limit| limit.to_string())
            .unwrap_or_else(|| "unlimited".into());
        ctl(CtlEvent::TuiOpDone(format!(
            "goal round {}/{} — {} / 目标第 {}/{} 轮",
            goal.rounds_started, total, goal.objective, goal.rounds_started, total
        )));
        let prompt = format!(
            "Continue working toward the session goal (round {}/{}): {}\nWhen the objective is achieved, call update_goal with status \"complete\". If progress is impossible, call update_goal with status \"blocked\" and explain in note. Otherwise keep working; do not restate the goal, just make progress.",
            goal.rounds_started, total, goal.objective
        );
        match super::turn(agent, Some(super::PromptInput::Text(prompt)), ctx, false).await {
            Ok(outcome) if outcome.stop_reason == abycore::StopReason::Completed => {}
            Ok(_) => return,
            Err(error) => {
                super::report_turn_err(ctl, &error, ctx.limits);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_arguments_are_never_silently_discarded() {
        assert_eq!(
            GoalCommand::parse("@20 resume"),
            Ok(GoalCommand::Resume(Some(20)))
        );
        assert_eq!(
            GoalCommand::parse("rounds 20"),
            Ok(GoalCommand::Rounds(Some(20)))
        );
        assert_eq!(
            GoalCommand::parse("rounds off"),
            Ok(GoalCommand::Rounds(None))
        );
        assert_eq!(
            GoalCommand::parse("@1000 resume"),
            Ok(GoalCommand::Resume(Some(1000)))
        );
        assert_eq!(
            GoalCommand::parse("@3 ship"),
            Ok(GoalCommand::Create {
                objective: "ship",
                rounds: Some(3)
            })
        );
        assert_eq!(
            GoalCommand::parse("@here fix it"),
            Ok(GoalCommand::Create {
                objective: "@here fix it",
                rounds: None
            })
        );
        for arg in [
            "@20 pause",
            "@2 status",
            "@3 clear",
            "@3",
            "@0 ship",
            "rounds 0",
            "rounds nope",
            "rounds 3 extra",
            "@999999999999999999999 resume",
        ] {
            assert!(GoalCommand::parse(arg).is_err(), "{arg}");
        }
    }
}
