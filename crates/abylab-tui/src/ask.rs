//! `abylab ask "<question>"`: one question, one answer, then exit.
//!
//! The same driver the TUI runs, without a screen: one prompt goes in, the
//! answer comes out on stdout, the process exits. Reasoning, tool cards and
//! progress never reach stdout — only the answer does — and problems go to
//! stderr with a non-zero exit code, so `abylab ask … > answer.md` is safe to
//! script.
//!
//! Nobody is at the keyboard, so the two things a UI answers by hand are
//! settled here instead: a permission ask follows the launch preset's own
//! sandbox (`allow` — the preset still decides what a tool may touch), and a
//! model question is declined, leaving the agent to pick its own reading and
//! say so rather than inventing a choice the user never made.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use abylab_backend::{Cmd, CtlEvent, Event, PermissionReply, UiEvent};

use crate::runtime::RuntimeConfig;

/// How long a wait with nothing to do lasts before the driver is checked
/// again. The turn's outcome never depends on it.
const POLL: Duration = Duration::from_millis(100);

/// How long the turn's own reason is still expected after it ends: the driver
/// reports a failed turn from the same loop, right after `TurnEnd`.
const TRAILING: Duration = Duration::from_millis(200);

/// The option id the driver's permission ask names for "yes".
const ALLOW: &str = "allow";

/// Ask once, headless, and print the answer. `Ok` is a turn that finished; a
/// failure — including a turn the driver reported as failed — is an `Err`, so
/// the shell sees a non-zero exit code.
pub fn run(
    cfg: &RuntimeConfig,
    session_id: &str,
    question: &str,
    limits: abylab_backend::TurnLimits,
    compaction: Option<abylab_backend::CompactionConfig>,
) -> anyhow::Result<()> {
    // Refused here, before a session is opened: the driver says the same thing,
    // but its line arrives as a failed turn, and the fix is a login rather than
    // a retry — the two ways to supply a key belong in the first line the
    // caller sees.
    if !cfg.has_credentials() {
        anyhow::bail!(
            "no API key — run abylab once and /login <apikey>, or pass --api-key <key> \
             for this run (get a key at https://platform.deepseek.com/)"
        );
    }
    let (tx, rx) = mpsc::channel::<Event>();
    let handle = abylab_backend::driver::spawn(
        crate::controller::driver_config(cfg, session_id, limits, compaction),
        move |event| {
            // The driver's thread must never block on the sink; a closed
            // receiver simply drops the event.
            let _ = tx.send(event);
        },
    )
    .map_err(anyhow::Error::msg)?;
    handle.send(Cmd::PromptForSession {
        session_id: session_id.to_string(),
        text: question.to_string(),
    });

    let mut turn = Turn::new();
    let ending = turn.wait(&rx);
    // Stop the turn (there is none left to stop, but the driver's own loop is
    // still holding the session writer) before the answer goes out.
    handle.shutdown();
    finish(&turn, ending)
}

/// One headless turn: the answer it streamed and, when the driver named one,
/// the reason it stopped.
struct Turn {
    /// The answer, as streamed: the last assistant message's text.
    answer: String,
    /// A step's stream finished (or nothing has streamed yet), so the next
    /// delta starts a new message and replaces what is buffered.
    at_message_start: bool,
    /// The driver's own words for a failure, when it had any.
    failure: Option<String>,
}

impl Turn {
    fn new() -> Self {
        Self {
            answer: String::new(),
            at_message_start: true,
            failure: None,
        }
    }

    /// Wait for the turn to end, serving what nobody is here to serve.
    fn wait(&mut self, rx: &Receiver<Event>) -> Ending {
        loop {
            match rx.recv_timeout(POLL) {
                Ok(event) => {
                    if let Some(ending) = self.observe(event) {
                        if ending == Ending::Failed && self.failure.is_none() {
                            self.drain_reason(rx);
                        }
                        break ending;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                // The driver's thread is gone: no answer is coming, and the
                // reason is already in whatever it reported before leaving.
                Err(RecvTimeoutError::Disconnected) => {
                    self.failure
                        .get_or_insert_with(|| "the driver stopped".into());
                    break Ending::Failed;
                }
            }
        }
    }

    /// A failed turn's reason arrives right after it ends: the driver reports
    /// `TurnEnd` from inside the turn, the failure from its loop, in that
    /// order. Read on for a moment so the message can name it.
    fn drain_reason(&mut self, rx: &Receiver<Event>) {
        let deadline = Instant::now() + TRAILING;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match rx.recv_timeout(left) {
                Ok(Event::Ctl(CtlEvent::Error(text))) => {
                    self.failure = Some(text);
                    return;
                }
                // The cancellation itself is the reason.
                Ok(Event::Ctl(CtlEvent::Interrupted)) => return,
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    }

    /// One driver event; `Some(ending)` once the turn is over.
    fn observe(&mut self, event: Event) -> Option<Ending> {
        match event {
            // A UI keeps every message it ever printed. One `ask` keeps the
            // answer, so text that follows a finished step replaces the message
            // before it: a preamble before a tool call gives way to the answer.
            Event::Ui(UiEvent::TextDelta { text, .. }) => {
                if self.at_message_start {
                    self.answer.clear();
                    self.at_message_start = false;
                }
                self.answer.push_str(&text);
                None
            }
            // Every step's stream ends here — a tool-only step says nothing,
            // and must not throw the answer away.
            Event::Ui(UiEvent::AssistantFinal { .. }) => {
                self.at_message_start = true;
                None
            }
            Event::Ui(UiEvent::TurnEnd { kind, .. }) => Some(Ending::of(&kind)),
            Event::Ctl(CtlEvent::Interrupted) => Some(Ending::Interrupted),
            // A failure ends the run: there is nobody to retry for, and a
            // non-zero exit code is what the caller is entitled to.
            Event::Ctl(CtlEvent::Error(text)) => {
                self.failure = Some(text);
                Some(Ending::Failed)
            }
            Event::Ctl(CtlEvent::Warning(note)) => {
                eprintln!("abylab ask: {note}");
                None
            }
            // Allow once, by the id the driver's own ask uses. An ask with no
            // way to say yes is cancelled, and the tool call comes back to the
            // agent as denied.
            Event::PermissionAsk { options, reply, .. } => {
                let allow = options
                    .iter()
                    .find(|option| option.kind == "allow_once" || option.option_id == ALLOW);
                let _ = reply.send(match allow {
                    Some(option) => PermissionReply::Selected(option.option_id.clone()),
                    None => PermissionReply::Cancelled,
                });
                None
            }
            // Declining a question is not failing one: the tool reports the
            // cancellation and the agent carries on without the answer.
            Event::UserQuestion { reply, .. } => {
                let _ = reply.send(None);
                None
            }
            _ => None,
        }
    }
}

/// How a turn ended, as the exit code sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The turn ran to the agent's own stop, answer included.
    Answered,
    /// The answer stopped at the output limit: it goes out, the exit code says
    /// it is not the whole one.
    Truncated,
    /// The turn was cancelled.
    Interrupted,
    /// The turn did not finish; `Turn::failure` names why when the driver did.
    Failed,
}

impl Ending {
    fn of(kind: &str) -> Ending {
        match kind {
            "completed" => Ending::Answered,
            "incomplete" => Ending::Truncated,
            "interrupted" => Ending::Interrupted,
            // "failed", "error", and anything a later driver adds: not a
            // finished answer until it says so.
            _ => Ending::Failed,
        }
    }
}

/// Print what the turn produced and turn its ending into an exit code.
fn finish(turn: &Turn, ending: Ending) -> anyhow::Result<()> {
    let answer = turn.answer.trim_end();
    if matches!(ending, Ending::Answered | Ending::Truncated) && !answer.is_empty() {
        println!("{answer}");
    }
    match ending {
        Ending::Answered => Ok(()),
        Ending::Truncated => anyhow::bail!(
            "the answer reached its output limit (max_tokens) and was cut off mid-thought"
        ),
        Ending::Interrupted => anyhow::bail!("the turn was interrupted"),
        Ending::Failed => match &turn.failure {
            Some(reason) => anyhow::bail!("{reason}"),
            None => anyhow::bail!("the turn failed"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abylab_backend::{AskOption, UserQuestion, UserQuestionOption};

    fn delta(session: &str, text: &str) -> Event {
        Event::Ui(UiEvent::TextDelta {
            session: session.into(),
            text: text.into(),
        })
    }

    /// A step - one model request - finished streaming.
    fn step_done(session: &str) -> Event {
        Event::Ui(UiEvent::AssistantFinal {
            session: session.into(),
            text: String::new(),
            model: None,
        })
    }

    fn turn_end(session: &str, kind: &str) -> Event {
        Event::Ui(UiEvent::TurnEnd {
            session: session.into(),
            kind: kind.into(),
        })
    }

    /// Only the last message is the answer: a preamble before a tool call is
    /// not what the question asked for, and a tool-only step says nothing, so
    /// it must not throw away what the message before it said.
    #[test]
    fn the_answer_is_the_last_message_the_turn_spoke() {
        let mut turn = Turn::new();
        for event in [
            delta("s", "let me "),
            delta("s", "look"),
            step_done("s"),
            // A tool ran; the model said nothing about it.
            step_done("s"),
            delta("s", "the answer "),
            delta("s", "is 42"),
            step_done("s"),
        ] {
            assert_eq!(turn.observe(event), None, "the turn is still running");
        }
        assert_eq!(turn.answer, "the answer is 42");

        // A tool-only final step keeps the last spoken message.
        assert_eq!(turn.observe(step_done("s")), None);
        assert_eq!(turn.answer, "the answer is 42");
        assert_eq!(
            turn.observe(turn_end("s", "completed")),
            Some(Ending::Answered)
        );
    }

    /// Every ending a driver can send maps to something the shell can read, and
    /// only a completed turn is a success.
    #[test]
    fn only_a_finished_turn_exits_zero() {
        for (kind, expected) in [
            ("completed", Ending::Answered),
            ("incomplete", Ending::Truncated),
            ("interrupted", Ending::Interrupted),
            ("failed", Ending::Failed),
            ("error", Ending::Failed),
        ] {
            assert_eq!(Ending::of(kind), expected, "kind {kind}");
        }
        let turn = Turn::new();
        assert!(finish(&turn, Ending::Answered).is_ok());
        for ending in [Ending::Truncated, Ending::Interrupted, Ending::Failed] {
            assert!(
                finish(&turn, ending).is_err(),
                "{ending:?} must fail the run"
            );
        }
    }

    /// A complete turn prints its answer and nothing else; a failure names the
    /// driver's reason rather than a generic line.
    #[test]
    fn a_finished_answer_is_the_whole_of_what_goes_out() {
        let mut turn = Turn::new();
        turn.observe(delta("s", "42\n"));
        turn.observe(step_done("s"));
        let ending = turn.observe(turn_end("s", "completed")).expect("over");
        assert!(finish(&turn, ending).is_ok());

        let mut turn = Turn::new();
        assert_eq!(
            turn.observe(Event::Ctl(CtlEvent::Error("no API key".into()))),
            Some(Ending::Failed),
        );
        let err = finish(&turn, Ending::Failed).expect_err("a failure is not an answer");
        assert!(err.to_string().contains("no API key"), "{err:#}");
    }

    /// Nobody is at the keyboard: a permission ask is granted by id, and a
    /// question the agent asks itself is declined instead of answered.
    #[test]
    fn the_two_asks_are_answered_without_a_keyboard() {
        let (allow_tx, mut allow_rx) = tokio::sync::oneshot::channel();
        let mut turn = Turn::new();
        assert_eq!(
            turn.observe(Event::PermissionAsk {
                title: "allow bash?".into(),
                options: vec![
                    AskOption {
                        option_id: "reject".into(),
                        kind: "reject_once".into(),
                        name: "Reject".into(),
                    },
                    AskOption {
                        option_id: "allow".into(),
                        kind: "allow_once".into(),
                        name: "Allow once".into(),
                    },
                ],
                reply: allow_tx,
            }),
            None,
            "the turn keeps running"
        );
        assert_eq!(
            allow_rx.try_recv().expect("the driver got an answer"),
            PermissionReply::Selected("allow".into()),
        );

        // An ask with no way to say yes is cancelled: the preset's sandbox
        // still bounds what the tool may do, and inventing consent is worse.
        let (deny_tx, mut deny_rx) = tokio::sync::oneshot::channel();
        turn.observe(Event::PermissionAsk {
            title: "allow something?".into(),
            options: vec![AskOption {
                option_id: "reject".into(),
                kind: "reject_once".into(),
                name: "Reject".into(),
            }],
            reply: deny_tx,
        });
        assert_eq!(
            deny_rx.try_recv().expect("the driver got an answer"),
            PermissionReply::Cancelled,
        );

        let (ask_tx, mut ask_rx) = tokio::sync::oneshot::channel();
        turn.observe(Event::UserQuestion {
            question: UserQuestion {
                id: "q1".into(),
                question: "which one?".into(),
                header: None,
                options: vec![UserQuestionOption {
                    label: "one".into(),
                    description: None,
                }],
                multi_select: false,
            },
            reply: ask_tx,
        });
        assert_eq!(
            ask_rx.try_recv().expect("the tool got an answer"),
            None,
            "a declined question is how the agent learns to decide itself",
        );
    }

    /// An advisory is worth a line, but it is not the end of the run.
    #[test]
    fn a_warning_does_not_end_the_run() {
        let mut turn = Turn::new();
        assert_eq!(
            turn.observe(Event::Ctl(CtlEvent::Warning("skills: broken".into()))),
            None
        );
        assert_eq!(
            turn.observe(turn_end("s", "completed")),
            Some(Ending::Answered)
        );
        // Neither does the transcript furniture an answer is not made of.
        assert_eq!(
            turn.observe(Event::Ui(UiEvent::UserMessage {
                session: "s".into(),
                text: "hi".into(),
            })),
            None
        );
        assert_eq!(
            turn.observe(Event::Ui(UiEvent::Usage {
                session: "s".into(),
                input: 1,
                output: 1,
                cached: 0,
                reasoning: 0,
            })),
            None
        );
    }
}
