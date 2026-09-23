use super::*;
use crate::{CheckpointKind, PendingCall, RunOutcome, StreamEvent, ToolDecision};
use futures_util::{FutureExt, future::BoxFuture};
use std::{panic::AssertUnwindSafe, sync::Weak};

pub(super) struct ChildHooks {
    pub entry: Weak<Entry>,
    pub parent: Option<Arc<dyn AgentHooks>>,
    pub own: Option<Arc<dyn AgentHooks>>,
}

impl AgentHooks for ChildHooks {
    fn request_context(&self) -> Option<&str> {
        self.own
            .as_ref()
            .and_then(|hooks| hooks.request_context())
            .or_else(|| {
                self.parent
                    .as_ref()
                    .and_then(|hooks| hooks.request_context())
            })
    }

    fn view_request<'a>(
        &'a self,
        estimate: &'a crate::ViewEstimate,
    ) -> BoxFuture<'a, Result<Option<crate::ViewRequest>>> {
        Box::pin(async move {
            match &self.own {
                Some(hooks) => hooks.view_request(estimate).await,
                None => Ok(None),
            }
        })
    }

    fn tool_reminder<'a>(
        &'a self,
        call: &'a PendingCall,
        output: &'a ToolOutput,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            match &self.own {
                Some(hooks) => hooks.tool_reminder(call, output).await,
                None => Ok(None),
            }
        })
    }

    fn checkpoint<'a>(
        &'a self,
        kind: CheckpointKind,
        snapshot: SessionSnapshot,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(entry) = self.entry.upgrade() {
                *entry.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snapshot.clone();
            }
            if let Some(hooks) = &self.own {
                hooks.checkpoint(kind, snapshot).await?;
            }
            Ok(())
        })
    }

    fn authorize<'a>(&'a self, call: PendingCall) -> BoxFuture<'a, Result<ToolDecision>> {
        Box::pin(async move {
            if let Some(parent) = &self.parent {
                let decision = parent.authorize(call.clone()).await?;
                if matches!(decision, ToolDecision::Deny(_)) {
                    return Ok(decision);
                }
            }
            match &self.own {
                Some(hooks) => hooks.authorize(call).await,
                None => Ok(ToolDecision::Allow),
            }
        })
    }
}

pub(super) fn launch(
    runtime: &tokio::runtime::Handle,
    entry: Arc<Entry>,
    mut agent: Agent,
    mut input: Option<String>,
    mut options: RunOptions,
    foreground: Option<CancellationToken>,
    events: broadcast::Sender<SubagentEvent>,
) {
    options.cancellation = entry
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .cancellation
        .clone();
    let guard = CompletionGuard {
        entry: entry.clone(),
        events: events.clone(),
        armed: true,
    };
    runtime.spawn(async move {
        let mut guard = guard;
        loop {
            let mut partial = String::new();
            let mut observe = |event: AgentEvent| {
                match &event {
                    AgentEvent::Model(StreamEvent::Started { .. }) => partial.clear(),
                    AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) => {
                        append_bounded(&mut partial, delta, options.max_tool_output_bytes);
                    }
                    AgentEvent::Model(StreamEvent::Finished { response, .. }) => {
                        partial = ToolOutput::text(response.output_text())
                            .bounded(options.max_tool_output_bytes)
                            .content;
                    }
                    _ => {}
                }
                let _ = events.send(SubagentEvent::Agent {
                    agent_id: agent_id(&entry),
                    event,
                });
                async { Ok(()) }
            };
            let execution = async {
                if agent.snapshot().needs_response {
                    if let Some(message) = input.take() {
                        agent.inbox.send(message)?;
                    }
                    agent.continue_run(options.clone(), &mut observe).await
                } else {
                    let message = input
                        .take()
                        .map(|text| vec![crate::ContentPart::InputText { text }])
                        .or_else(|| {
                            entry
                                .inbox
                                .0
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .pop_front()
                        })
                        .ok_or_else(|| invalid("subagent has no input"))?;
                    agent
                        .run_parts(message, options.clone(), &mut observe)
                        .await
                }
            };
            let result = AssertUnwindSafe(async {
                tokio::pin!(execution);
                if let Some(parent) = &foreground {
                    tokio::select! {
                        biased;
                        _ = parent.cancelled() => {
                            options.cancellation.cancel();
                            execution.await
                        }
                        result = &mut execution => result,
                    }
                } else {
                    execution.await
                }
            })
            .catch_unwind()
            .await;
            let panicked = result.is_err();
            let result = result.unwrap_or_else(|_| {
                Err(Error::new(ErrorKind::Session, "subagent executor panicked"))
            });
            let snapshot = agent.snapshot();
            *entry.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snapshot.clone();
            let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
            // A sender can arrive after the final model response and before settlement.
            // Claim its input while the child still has its concurrency slot.
            if matches!(&result, Ok(outcome) if outcome.stop_reason == StopReason::Completed)
                && !options.cancellation.is_cancelled()
                && !entry.inbox.is_empty()
            {
                drop(state);
                continue;
            }
            state.info.result = Some(to_result(result, partial, options.max_tool_output_bytes));
            state.info.status = if panicked {
                SubagentStatus::Unavailable
            } else if !snapshot.pending.is_empty() {
                SubagentStatus::NeedsResolution
            } else {
                SubagentStatus::Idle
            };
            if !panicked {
                state.agent = Some(agent);
            }
            let info = state.info.clone();
            guard.armed = false;
            finish(&entry, &events, info);
            drop(state);
            break;
        }
    });
}

fn agent_id(entry: &Entry) -> String {
    entry
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .info
        .id
        .clone()
}

fn append_bounded(output: &mut String, delta: &str, limit: usize) {
    let mut end = delta.len().min(limit.saturating_sub(output.len()));
    while !delta.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&delta[..end]);
}

fn to_result(result: Result<RunOutcome>, partial: String, limit: usize) -> SubagentResult {
    match result {
        Ok(outcome) => SubagentResult {
            output: ToolOutput::text(outcome.response.output_text())
                .bounded(limit)
                .content,
            stop_reason: outcome.stop_reason,
            error: None,
        },
        Err(error) => SubagentResult {
            output: partial,
            stop_reason: StopReason::Error(error.kind),
            error: Some(error.to_string()),
        },
    }
}

fn finish(entry: &Entry, events: &broadcast::Sender<SubagentEvent>, info: SubagentInfo) {
    if entry.notify_parent {
        // Notifications carry only an id/status. Output stays behind explicit collection.
        // A full inbox does not change a completed run; host events and wait remain available.
        let _ = entry.parent_inbox.send(format!(
            "[Subagent {} finished: {:?}. Use wait_agent to read its result.]",
            info.id, info.status,
        ));
    }
    let _ = events.send(SubagentEvent::Finished(info));
    entry.done.notify_waiters();
}

struct CompletionGuard {
    entry: Arc<Entry>,
    events: broadcast::Sender<SubagentEvent>,
    armed: bool,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.entry.state.lock().unwrap_or_else(|p| p.into_inner());
        state.cancellation.cancel();
        state.info.status = SubagentStatus::Unavailable;
        state.info.result = Some(SubagentResult {
            output: String::new(),
            stop_reason: StopReason::Error(ErrorKind::Cancelled),
            error: Some("subagent executor stopped before reporting its result".into()),
        });
        let info = state.info.clone();
        finish(&self.entry, &self.events, info);
    }
}
