//! Turn execution, interruption, steering and exactly-once terminal results.
use super::*;

/// What one turn needs besides the agent: the session label, the event sink,
/// the interrupt channel, the steer channel and the budgets. Grouped so the
/// segment helpers do not grow one parameter per turn feature.
pub(super) struct TurnCtx<'a> {
    pub(super) session: &'a str,
    pub(super) sink: &'a Arc<dyn Fn(Event) + Send + Sync>,
    pub(super) interrupt_rx: &'a mut mpsc::UnboundedReceiver<()>,
    pub(super) goal_rx: &'a mut mpsc::UnboundedReceiver<GoalControl>,
    pub(super) pause_requested: bool,
    pub(super) steer_rx: &'a mut mpsc::UnboundedReceiver<SteerRequest>,
    /// Inbox of the live agent: text pushed here is appended at its next step
    /// boundary, which is what makes Send Now not cancel anything.
    pub(super) steer: abycore::SteerHandle,
    /// The launch skill snapshot: a steered `/<name>` line expands here too,
    /// because the steer path skips the prompt loop's own expansion.
    pub(super) skills: &'a Arc<abycore::SkillCatalog>,
    /// Steers taken but not yet spent, so the next inbox drain can report them.
    pub(super) inbox: &'a HostInbox,
    pub(super) limits: TurnLimits,
    /// Host compaction policy for overflow recovery inside a turn.
    pub(super) compaction: Option<CompactionConfig>,
}

/// One agent segment racing the interrupt channel. New input is included in
/// the next request even when the session still has an unfinished turn.
///
/// Cancellation races the run future; the losing branch still drains the
/// outcome so history stays consistent. A steer races it too but never wins:
/// it rides into the agent's inbox and the run keeps its output, which is the
/// difference between "change course mid-turn" and "cancel and retype".
pub(super) async fn run_segment(
    agent: &mut Agent,
    input: Option<PromptInput>,
    ctx: &mut TurnCtx<'_>,
) -> abycore::Result<RunOutcome> {
    let cancellation = CancellationToken::new();
    let live_goal = Arc::new(std::sync::Mutex::new(agent.goal().cloned()));
    let event_goal = Arc::clone(&live_goal);
    let has_image = agent.has_image_input()
        || matches!(&input,
            Some(PromptInput::Identified(input)) if input.parts.iter().any(|part| matches!(part, abycore::ContentPart::InputImage { .. }))
        );
    let options = RunOptions {
        cancellation: cancellation.clone(),
        max_requests: ctx.limits.max_requests,
        max_tool_calls: ctx.limits.max_tool_calls,
        timeout: ctx.limits.run_timeout,
        tool_timeout: ctx.limits.tool_timeout,
        max_input_bytes: if has_image {
            48 * 1024 * 1024
        } else {
            RunOptions::default().max_input_bytes
        },
        ..RunOptions::default()
    };
    let (sess, sink_for_events) = (ctx.session.to_string(), Arc::clone(ctx.sink));
    let on_event = move |event: AgentEvent| {
        let sink = Arc::clone(&sink_for_events);
        let sess = sess.clone();
        let event_goal = Arc::clone(&event_goal);
        async move {
            if let AgentEvent::GoalChanged { goal } = &event {
                *event_goal.lock().unwrap_or_else(|p| p.into_inner()) = goal.clone();
            }
            // A view change is a control fact, not transcript content: report it
            // as a notice before the (empty) translation.
            if let AgentEvent::ViewChanged { change } = &event {
                match change {
                    abycore::ViewChange::Failed { message } => {
                        sink(Event::Ctl(CtlEvent::TuiOpFailed(format!(
                            "compaction failed: {message} / 压缩失败：{message}"
                        ))))
                    }
                    change => sink(Event::Ctl(CtlEvent::TuiOpDone(view_change_notice(change)))),
                }
            }
            for ui in translate(&sess, event) {
                sink(Event::Ui(ui));
            }
            Ok(())
        }
    };

    let mut run_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = abycore::Result<RunOutcome>> + '_>,
    > = match input {
        Some(PromptInput::Text(text)) if agent.snapshot().needs_response => {
            Box::pin(agent.continue_run_with_input(text, options, on_event))
        }
        Some(PromptInput::Text(text)) => Box::pin(agent.run(text, options, on_event)),
        Some(PromptInput::Identified(input)) if agent.snapshot().contains_input(&input.id) => {
            Box::pin(agent.continue_run(options, on_event))
        }
        Some(PromptInput::Identified(input)) => Box::pin(agent.run_input(input, options, on_event)),
        None => Box::pin(agent.continue_run(options, on_event)),
    };
    // Controls win before another request can start. After cancellation, keep
    // polling the run for its cleanup and keep answering status requests.
    let mut interrupted = false;
    let mut delivery: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> =
        None;
    let mut completed = None;
    loop {
        if delivery.is_none()
            && let Some(result) = completed.take()
        {
            return if interrupted {
                Err(abycore::Error::new(
                    ErrorKind::Cancelled,
                    "operation cancelled",
                ))
            } else {
                result
            };
        }
        tokio::select! {
            biased;
            Some(control) = ctx.goal_rx.recv() => {
                let snapshot = live_goal.lock().unwrap_or_else(|p| p.into_inner());
                if goal::during_turn(control, snapshot.as_ref(), &|event| (ctx.sink)(Event::Ctl(event))) {
                    ctx.pause_requested = true;
                    interrupted = true;
                    cancellation.cancel();
                }
            }
            Some(()) = ctx.interrupt_rx.recv() => {
                interrupted = true;
                cancellation.cancel();
            }
            result = &mut run_fut, if completed.is_none() => completed = Some(result),
            () = async { delivery.as_mut().expect("guarded delivery").await }, if delivery.is_some() => { delivery = None; }
            Some(request) = ctx.steer_rx.recv(), if delivery.is_none() && completed.is_none() => {
                // Own the handoff future so disk latency cannot monopolize the
                // select loop. Its receipt is drained even if the run ends first.
                let session = ctx.session.to_owned();
                let inbox = ctx.inbox.clone();
                let skills = Arc::clone(ctx.skills);
                let steer = ctx.steer.clone();
                let sink = Arc::clone(ctx.sink);
                delivery = Some(Box::pin(async move {
                    deliver_steer(request, &session, &inbox, &skills, &steer, &sink).await;
                }));
            }
        }
    }
}

/// Take one Send Now request into the live agent.
///
/// Success means "the agent holds it": abycore appends it at the next complete
/// tool-batch boundary, or — when the model was already about to end the turn —
/// continues one more step instead of stopping. Nothing is cancelled and no
/// event from the in-flight step is discarded.
#[cfg(test)]
pub(super) async fn settle_steer(request: SteerRequest, ctx: &mut TurnCtx<'_>) {
    deliver_steer(
        request,
        ctx.session,
        ctx.inbox,
        ctx.skills,
        &ctx.steer,
        ctx.sink,
    )
    .await;
}

pub(super) async fn deliver_steer(
    request: SteerRequest,
    session: &str,
    inbox: &HostInbox,
    skills: &abycore::SkillCatalog,
    steer: &abycore::SteerHandle,
    sink: &Arc<dyn Fn(Event) + Send + Sync>,
) {
    let id = request.message_id;
    if request.session_id != session {
        sink(Event::Ctl(CtlEvent::SteerSettled {
            message_id: id,
            deferred: true,
        }));
        return;
    }
    let result = if request.session_id != session {
        Err("the active session changed".to_string())
    } else {
        match inbox
            .prepare(session, Some(id), request.text, request.parts, true)
            .await
        {
            Ok(Some(mut input)) => {
                expand_input(&mut input, skills);
                steer.send_input(input).map_err(|error| error.to_string())
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        }
    };
    let deferred = result.is_err();
    if let Err(error) = result {
        (sink)(Event::Ctl(CtlEvent::TuiOpFailed(format!(
            "send now failed: {error}"
        ))));
        let _ = inbox.defer(id).await;
    }
    (sink)(Event::Ctl(CtlEvent::SteerSettled {
        message_id: id,
        deferred,
    }));
}

pub(super) fn expand_input(input: &mut abycore::UserInput, skills: &abycore::SkillCatalog) {
    if let [abycore::ContentPart::InputText { text }] = input.parts.as_mut_slice()
        && let Some(expanded) = skills.expand(text)
    {
        *text = expanded;
    }
}

pub(super) async fn run_segments(
    agent: &mut SessionAgent,
    input: Option<PromptInput>,
    ctx: &mut TurnCtx<'_>,
    emit: &impl Fn(Event),
    emit_ctl: &impl Fn(CtlEvent),
) -> abycore::Result<RunOutcome> {
    if input.is_some() && agent.snapshot().needs_response {
        settle_pending(
            agent,
            "previous tool call was interrupted; its result is unverified — check before repeating",
            ctx,
        )?;
    }
    // Stale interrupts (sent between turns, e.g. by the steer path) must not
    // cancel this turn — but an interrupt that arrives while a later segment
    // runs must still land, so drain only once here.
    if !agent
        .goal()
        .is_some_and(|goal| goal.status == abycore::GoalStatus::Active)
    {
        while ctx.interrupt_rx.try_recv().is_ok() {}
    }

    let mut next = input;
    let mut continuations = 0usize;
    'segments: loop {
        let outcome = run_segment(agent, next.take(), ctx).await;
        // A context overflow is recoverable: condense hard, then resume the
        // same open turn (harness retries the request after the surface
        // replacement generation advances).
        if matches!(&outcome, Err(err) if err.kind == ErrorKind::ContextLimitExceeded)
            && continuations < ctx.limits.continuations
        {
            match compact_history(
                agent,
                ctx.compaction,
                CompactTrigger::Overflow,
                ctx.interrupt_rx,
                ctx.goal_rx,
                ctx.sink,
            )
            .await
            {
                Ok(Some(report)) => {
                    continuations += 1;
                    emit(Event::Ctl(CtlEvent::TuiOpDone(
                        report.notice(CompactTrigger::Overflow),
                    )));
                    continue;
                }
                Ok(None) => {}
                Err(err) if err.kind == ErrorKind::Cancelled => break Err(err),
                Err(err) => emit(Event::Ctl(CtlEvent::TuiOpFailed(format!(
                    "compaction failed: {err} / 压缩失败：{err}"
                )))),
            }
        }
        // The turn ends on a failure that resuming cannot help, on a stop with
        // no headroom left, or when the session is no longer continuable.
        let failure = match &outcome {
            Err(err) if resumable_failure(err) => err,
            _ => break outcome,
        };
        if continuations >= ctx.limits.continuations || !agent.snapshot().needs_response {
            break outcome;
        }
        // Pending calls may be unstarted or interrupted mid-execution. Settle
        // each according to its state so a timeout never invites a blind replay.
        if !agent.snapshot().pending.is_empty() {
            settle_pending(
                agent,
                "the turn stopped before this tool ran — it did not execute; re-issue it if it is still needed",
                ctx,
            )?;
        }
        // Transient failures carry their own pacing; a cheap retry
        // storm would only burn the remaining headroom. The wait stays
        // interruptible so esc still ends the turn.
        if let Some(delay) = continuation_delay(failure) {
            let wait = tokio::time::sleep(delay);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    biased;
                    Some(control) = ctx.goal_rx.recv() => {
                        if goal::during_turn(control, agent.goal(), &emit_ctl) {
                            ctx.pause_requested = true;
                            break 'segments Err(abycore::Error::new(ErrorKind::Cancelled, "operation cancelled"));
                        }
                    }
                    Some(()) = ctx.interrupt_rx.recv() => {
                        break 'segments Err(abycore::Error::new(ErrorKind::Cancelled, "operation cancelled"));
                    }
                    _ = &mut wait => break,
                }
            }
        }
        continuations += 1;
        emit(Event::Ctl(CtlEvent::TuiOpDone(continuation_notice(
            failure,
            continuations,
            ctx.limits.continuations,
        ))));
    }
}

/// Drive one turn: `Some(text)` adds a prompt to a fresh or unfinished turn;
/// `None` retries an unfinished turn without new input.
///
/// Like deepseek-harness's agent loop, the turn ends when the model stops or
/// the user interrupts — not because requests ran out. A segment that stops on
/// the per-run watchdog budget, a timeout or a transient request failure leaves
/// the session at a continuable boundary (abycore's `continue_run`), so the driver
/// settles any tool calls the stop left unresolved and resumes the same open
/// turn, up to [`TurnLimits::continuations`] times, before surfacing the error.
/// Without that, one cheap `web_search` batch or a dropped connection silently
/// ends the turn while the model still had work queued.
pub(super) async fn turn(
    agent: &mut SessionAgent,
    input: Option<PromptInput>,
    ctx: &mut TurnCtx<'_>,
    settle_status: bool,
) -> abycore::Result<RunOutcome> {
    let session = ctx.session.to_string();
    let sink = Arc::clone(ctx.sink);
    let emit = |event: Event| sink(event);
    let emit_ctl = |event: CtlEvent| sink(Event::Ctl(event));
    agent.last_turn = agent
        .last_turn
        .max(agent.snapshot().run_sequence)
        .saturating_add(1);
    let turn = agent.last_turn;
    emit(Event::Ui(UiEvent::TurnStart {
        session: session.clone(),
        turn,
    }));
    emit(Event::Ui(UiEvent::SessionStatus {
        session: session.clone(),
        running: true,
    }));

    let initial_requests = agent.snapshot().requests.len();
    let outcome = run_segments(agent, input, ctx, &emit, &emit_ctl).await;
    let mut outcome = outcome;
    let stopped_goal = if agent
        .goal()
        .is_some_and(|goal| goal.status == abycore::GoalStatus::Active)
    {
        let stopped = match &outcome {
            Ok(result) if result.stop_reason == StopReason::Completed => None,
            Ok(_) => Some((
                abycore::GoalStatus::Paused,
                "turn stopped; use /goal resume",
            )),
            Err(error) if error.kind == ErrorKind::Cancelled => {
                Some((abycore::GoalStatus::Paused, "interrupted; use /goal resume"))
            }
            Err(_) => Some((
                abycore::GoalStatus::Blocked,
                "turn failed; resolve the error and use /goal resume",
            )),
        };
        match stopped {
            Some((status, note)) => {
                if let Err(error) = agent.update_goal(status, Some(note.into())) {
                    outcome = Err(error);
                }
                true
            }
            None => false,
        }
    } else {
        false
    };
    // Save final state and the ledger on every exit, including cancellation
    // while waiting between segments or an incomplete model response.
    let saved = agent.save().await;
    if saved.is_ok()
        && (stopped_goal || ctx.pause_requested)
        && let Some(goal) = agent.goal()
    {
        emit_ctl(CtlEvent::TuiOpDone(format!(
            "goal → {} / 目标 → {}",
            goal.summary(),
            goal.summary()
        )));
    }
    ctx.pause_requested = false;
    let outcome = saved.and(outcome);
    if let Err(error) = ctx
        .inbox
        .finish_turn(agent.steer_handle(), HostInbox::receipts(&agent.snapshot()))
        .await
    {
        emit_ctl(CtlEvent::TuiOpFailed(error));
    }
    let kind = match &outcome {
        Ok(outcome) => stop_kind(&outcome.stop_reason),
        Err(err) if err.kind == ErrorKind::Cancelled => TurnStatus::Interrupted,
        Err(_) => TurnStatus::Error,
    };
    if matches!(&outcome, Ok(outcome) if outcome.stop_reason == StopReason::Incomplete) {
        let max_tokens = agent.snapshot().model.max_tokens;
        emit_ctl(CtlEvent::TuiOpDone(format!(
            "response reached its output limit (max_tokens={max_tokens}); send a shorter instruction or resume with a larger --max-tokens / 回答达到输出上限；可继续输入缩短回答的指令，或提高 --max-tokens 后恢复会话"
        )));
    }
    let mut usage = TurnUsage::default();
    for record in agent.snapshot().requests.iter().skip(initial_requests) {
        if record.usage.as_ref().is_none_or(|value| !value.consistent) {
            usage.unknown_requests += 1;
        }
        if let Some(value) = &record.usage {
            usage.input = usage.input.saturating_add(value.input_tokens.unwrap_or(0));
            usage.output = usage
                .output
                .saturating_add(value.output_tokens.unwrap_or(0));
            usage.cached = usage
                .cached
                .saturating_add(value.cached_tokens.unwrap_or(0));
            usage.reasoning = usage
                .reasoning
                .saturating_add(value.reasoning_tokens.unwrap_or(0));
        }
    }
    let error = match &outcome {
        Err(error) if error.kind == ErrorKind::Cancelled => None,
        Err(error) => Some(turn_error_text(error, ctx.limits)),
        Ok(result) if kind == TurnStatus::Failed => Some(
            result
                .response
                .error_message
                .clone()
                .or_else(|| result.response.error_code.clone())
                .unwrap_or_else(|| "model response failed".into()),
        ),
        _ => None,
    };
    emit(Event::Ui(UiEvent::Usage {
        session: session.clone(),
        input: usage.input,
        output: usage.output,
        cached: usage.cached,
        reasoning: usage.reasoning,
    }));
    emit(Event::Ui(UiEvent::TurnEnd {
        session: session.clone(),
        turn,
        kind,
        error,
        usage,
    }));
    // The goal-round sequence owns exactly one idle status: a `running:false`
    // after every round told the UI the session was idle while the driver was
    // still inside the loop (and it dispatched queued prompts early).
    if settle_status {
        emit(Event::Ui(UiEvent::SessionStatus {
            session,
            running: false,
        }));
    }
    outcome
}

/// Whether a failed segment is worth resuming inside the same open turn.
///
/// Mirrors deepseek-harness's retryable step failures (`dsh-llm-retry`:
/// `TRANSPORT`, `RATE_LIMIT`, `SERVER`, `EMPTY_RESPONSE`). A timeout — the run's
/// own no-progress window, or a request that reached its deadline — also leaves
/// the turn resumable: a segment that went quiet can need a fresh one, and the
/// work already committed is unaffected. Timeout continuations share the same
/// finite headroom as all other retries. `BudgetExceeded` is abylab's own
/// watchdog.
pub(super) fn resumable_failure(err: &abycore::Error) -> bool {
    matches!(
        err.kind,
        ErrorKind::BudgetExceeded
            | ErrorKind::Timeout
            | ErrorKind::Transport
            | ErrorKind::StreamClosed
            | ErrorKind::Server
            | ErrorKind::RateLimit
            | ErrorKind::EmptyResponse
    )
}

/// Pacing before a transient continuation: the provider's `Retry-After` when it
/// sent one, else one second, capped like abycore's own retry backoff. Budget
/// stops retry immediately. Timeouts wait one second and remain interruptible.
pub(super) fn continuation_delay(err: &abycore::Error) -> Option<Duration> {
    match err.kind {
        ErrorKind::Timeout => Some(Duration::from_secs(1)),
        ErrorKind::RateLimit | ErrorKind::Server => Some(
            err.retry_after
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(15)),
        ),
        _ => None,
    }
}

/// One transcript notice per automatic continuation.
pub(super) fn continuation_notice(err: &abycore::Error, round: usize, rounds: usize) -> String {
    if err.kind == ErrorKind::BudgetExceeded {
        return format!(
            "turn budget reached — continuing the unfinished turn ({round}/{rounds}) / 达到回合预算，自动续跑（{round}/{rounds}）"
        );
    }
    if err.kind == ErrorKind::Timeout {
        return format!(
            "timed out — continuing the unfinished turn ({round}/{rounds}) / 超时，自动续跑（{round}/{rounds}）"
        );
    }
    if err.kind == ErrorKind::EmptyResponse {
        return format!(
            "empty response — retrying the unfinished step ({round}/{rounds}) / 空响应，正在同一回合内重试（{round}/{rounds}）"
        );
    }
    let (en, zh) = match err.kind {
        ErrorKind::Transport | ErrorKind::StreamClosed => ("connection lost", "连接中断"),
        ErrorKind::RateLimit => ("rate limited", "限流"),
        ErrorKind::Server => ("server error", "服务端错误"),
        _ => ("request failed", "请求失败"),
    };
    format!(
        "{en} — retrying the unfinished step ({round}/{rounds}) / {zh}，正在同一回合内重试（{round}/{rounds}）"
    )
}

/// A limit for the error text: `usize::MAX` reads as "unlimited", not a number
/// no human wants to parse.
pub(super) fn limit_label(value: usize) -> String {
    if value == TurnLimits::UNLIMITED {
        "unlimited".into()
    } else {
        value.to_string()
    }
}

pub(super) fn stop_kind(reason: &StopReason) -> TurnStatus {
    match reason {
        StopReason::Completed => TurnStatus::Completed,
        StopReason::Incomplete => TurnStatus::Incomplete,
        StopReason::Failed => TurnStatus::Failed,
        StopReason::Error(kind) if kind == &ErrorKind::Cancelled => TurnStatus::Interrupted,
        StopReason::Error(_) => TurnStatus::Error,
    }
}
