use crate::{
    AgentEvent, AgentHooks, CallBudget, CheckpointKind, Compaction, ContentPart, ContextEstimate,
    DeepSeekClient, Error, ErrorKind, Item, MessageRequest, MessageRole, ModelOptions, PendingCall,
    PendingState, Prune, RequestPurpose, Response, ResponseStatus, Result, RunOptions, RunOutcome,
    SessionSnapshot, StopReason, StreamEvent, SummarizeOptions, SummarizeOutcome, Tool,
    ToolContext, ToolDecision, ToolDefinition, ToolError, ToolOutput,
    compaction::ViewEstimate,
    compaction::{SUMMARIZE_INSTRUCTION, balanced_cuts, request_view, validate_compactions},
    context::{Ledger, RequestContext, records},
    goal::{Goal, GoalRequest, GoalStatus, apply_goal_request, goal_json_text},
    session::validate_history,
};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
pub(crate) struct RegisteredTool {
    definition: ToolDefinition,
    implementation: Arc<dyn Tool>,
}

/// The host owns this value and its runtime. The agent loop has no background worker.
pub struct Agent {
    client: DeepSeekClient,
    state: SessionSnapshot,
    tools: BTreeMap<String, RegisteredTool>,
    ledger: Ledger,
    // Earlier provider measurements describe a different view. On restore,
    // conservatively remeasure any already-compacted/pruned session.
    calibration_start: usize,
    local_session: Arc<crate::local_tools::LocalSession>,
    hooks: Option<Arc<dyn AgentHooks>>,
    pub(crate) id: String,
    pub(crate) depth: usize,
    pub(crate) inbox: Arc<crate::subagent::Inbox>,
    pub(crate) subagent_manager: Option<u128>,
}

impl Agent {
    pub fn new(
        client: DeepSeekClient,
        system_prompt: impl Into<String>,
        model: ModelOptions,
    ) -> Result<Self> {
        Self::restore(client, SessionSnapshot::new(system_prompt, model))
    }

    /// Restoring never issues requests or executes a tool. Register tools again in the host.
    pub fn restore(client: DeepSeekClient, mut snapshot: SessionSnapshot) -> Result<Self> {
        snapshot.validate()?;
        let calibration_start = if snapshot.compactions.is_empty() && snapshot.prune.is_none() {
            0
        } else {
            snapshot.requests.len()
        };
        let ledger = Arc::new(Mutex::new(std::mem::take(&mut snapshot.requests)));
        Ok(Self {
            client,
            state: snapshot,
            tools: BTreeMap::new(),
            ledger,
            calibration_start,
            local_session: Arc::default(),
            hooks: None,
            id: format!("agent-{:032x}", rand::random::<u128>()),
            depth: 0,
            inbox: Arc::default(),
            subagent_manager: None,
        })
    }

    /// Handle for injecting user text at the next step boundary of a running
    /// turn (see [`SteerHandle`](crate::SteerHandle)). Never cancels a step.
    pub fn steer_handle(&self) -> crate::SteerHandle {
        crate::SteerHandle(Arc::clone(&self.inbox))
    }

    pub fn register_tool(&mut self, tool: impl Tool + 'static) -> Result<()> {
        self.register_shared_tool(Arc::new(tool))
    }

    pub fn register_shared_tool(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        self.register_tools([tool])
    }

    /// Replace existing tool implementations atomically, retaining runtime
    /// identity, inbox and local session state. Children keep their delegated
    /// tool implementations. Unknown or duplicate names are rejected.
    pub fn replace_tools(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Result<()> {
        let mut pending = BTreeMap::new();
        for implementation in tools {
            let definition = implementation.definition();
            definition.check()?;
            if !self.tools.contains_key(&definition.name) || pending.contains_key(&definition.name)
            {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "replacement tool must be registered exactly once",
                ));
            }
            pending.insert(
                definition.name.clone(),
                RegisteredTool {
                    definition,
                    implementation,
                },
            );
        }
        self.tools.extend(pending);
        self.calibration_start = records(&self.ledger).len();
        Ok(())
    }

    /// Apply credentials/transport configuration to subsequent requests without
    /// rebuilding the agent. Existing children keep their delegated client.
    pub fn set_client(&mut self, client: DeepSeekClient) {
        self.client = client;
    }

    /// Validate the whole batch before registration, leaving the agent unchanged on error.
    pub fn register_tools(&mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Result<()> {
        let mut pending = BTreeMap::new();
        for implementation in tools {
            let definition = implementation.definition();
            definition.check()?;
            if self.tools.contains_key(&definition.name) || pending.contains_key(&definition.name) {
                return Err(Error::new(
                    ErrorKind::Configuration,
                    "tool is already registered",
                ));
            }
            pending.insert(
                definition.name.clone(),
                RegisteredTool {
                    definition,
                    implementation,
                },
            );
        }
        self.tools.extend(pending);
        Ok(())
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = self.state.clone();
        snapshot.requests = records(&self.ledger);
        snapshot
    }

    /// Current turn's task list and progress, including a completed checklist until the next run.
    pub fn plan_view(&self) -> Option<crate::PlanView> {
        self.state.plan_view()
    }

    /// Runtime identity, used to scope subagent control. Restoring creates a new identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn subagent_parent(&self) -> crate::subagent::Parent {
        crate::subagent::Parent {
            id: self.id.clone(),
            depth: self.depth,
            client: self.client.clone(),
            system_prompt: self.state.system_prompt.clone(),
            model: self.state.model.clone(),
            history: self.completed_history(),
            tools: self.tools.clone(),
            hooks: self.hooks.clone(),
            inbox: self.inbox.clone(),
            subagent_manager: self.subagent_manager,
        }
    }

    pub(crate) fn inherit_tools(
        &mut self,
        parent: &crate::subagent::Parent,
        allow: Option<&[String]>,
    ) {
        self.subagent_manager = parent.subagent_manager;
        self.tools = parent
            .tools
            .iter()
            .filter(|(name, _)| allow.is_none_or(|names| names.contains(name)))
            .map(|(name, tool)| (name.clone(), tool.clone()))
            .collect();
    }

    /// The history a forked child inherits: completed turns only, and — like
    /// every other consumer — the model-visible view, so a child does not pay
    /// for history the parent already condensed.
    fn completed_history(&self) -> Vec<Item> {
        if !self.state.needs_response {
            return self.request_view();
        }
        let mut end = 0;
        let mut answered = false;
        for (index, item) in self.state.items.iter().enumerate() {
            match item {
                Item::Message {
                    role: crate::MessageRole::User,
                    ..
                } => {
                    if answered {
                        end = index;
                    }
                    answered = false;
                }
                Item::Message {
                    role: crate::MessageRole::Assistant,
                    ..
                }
                | Item::Reasoning { .. } => answered = true,
                Item::FunctionCall { .. } | Item::FunctionCallOutput { .. } => answered = false,
            }
        }
        let items = &self.state.items[..end];
        let compactions: Vec<Compaction> = self
            .state
            .compactions
            .iter()
            .filter(|compaction| compaction.end <= end)
            .cloned()
            .collect();
        request_view(items, &compactions, self.state.prune.as_ref())
    }

    fn receive_messages(&mut self) -> bool {
        let messages = self.inbox.drain();
        let received = !messages.is_empty();
        self.state
            .items
            .extend(messages.into_iter().map(Item::user));
        if received {
            self.state.needs_response = true;
        }
        received
    }

    /// Install optional persistence/authorization barriers. Not included in snapshots.
    pub fn set_hooks(&mut self, hooks: Arc<dyn AgentHooks>) {
        // Host context can change when installing hooks, including on restore.
        self.calibration_start = records(&self.ledger).len();
        self.hooks = Some(hooks);
    }

    async fn checkpoint(&self, kind: CheckpointKind, context: &RequestContext) -> Result<()> {
        if let Some(hooks) = &self.hooks {
            context
                .wait(hooks.checkpoint(kind, self.snapshot()))
                .await??;
        }
        Ok(())
    }

    /// Supply a verified result (or an explicit host decision) for the first pending call.
    /// This method never executes a tool. Resolve the batch in its original order.
    ///
    /// Returns the output as committed. The agent answers `get_goal` from its
    /// own state and settles goal mutations here, so the result the model sees
    /// is not always the one the tool handed in: hosts showing a tool result
    /// must display the returned value, not their own copy.
    pub fn resolve_tool(&mut self, call_id: &str, output: ToolOutput) -> Result<ToolOutput> {
        if self
            .state
            .pending
            .first()
            .is_none_or(|call| call.call_id != call_id)
        {
            return Err(Error::new(
                ErrorKind::Session,
                "resolve the first pending call in transcript order",
            ));
        }
        let tool_name = self.state.pending[0].name.clone();
        let plan = if tool_name == "todo_write" {
            crate::PlanView::from_tool_output(&output)?
        } else {
            None
        };
        let mut output = output;
        match tool_name.as_str() {
            // The goal lives in the agent, so the read is answered here rather
            // than by the stateless tool.
            "get_goal" if !output.is_error => {
                output.content = goal_json_text(self.state.goal.as_ref());
            }
            // Create/update are committed here: the tool only proposes.
            "create_goal" | "update_goal" if !output.is_error => {
                let request = output
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("goal_request"))
                    .map(GoalRequest::parse)
                    .transpose();
                match request {
                    Ok(Some(request)) => {
                        match apply_goal_request(self.state.goal.as_ref(), &request) {
                            Ok(goal) => {
                                output.content = format!("goal → {}", goal.summary());
                                output.meta = Some(serde_json::json!({ "goal": goal }));
                                self.state.goal = Some(goal);
                            }
                            Err(error) => {
                                output.is_error = true;
                                output.content = error.message;
                                output.meta = None;
                            }
                        }
                    }
                    _ => {
                        output.is_error = true;
                        output.content = "invalid goal request".into();
                        output.meta = None;
                    }
                }
            }
            _ => {}
        }
        self.state.items.push(Item::FunctionCallOutput {
            call_id: call_id.into(),
            output: output.wire(),
            is_error: output.is_error,
            meta: output.meta.clone(),
        });
        self.state.pending.remove(0);
        self.state.needs_response = true;
        if let Some(plan) = plan {
            self.state.todos = Some(plan.todos);
        }
        Ok(output)
    }

    /// The exact request envelope the loop would send for `history`.
    fn build_request(&self, mut history: Vec<Item>) -> MessageRequest {
        if let Some(context) = self
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.request_context())
            && !context.is_empty()
        {
            history.insert(0, Item::user(context));
        }
        MessageRequest {
            system: self.state.system_prompt.clone(),
            history,
            options: self.state.model.clone(),
            tools: self
                .tools
                .values()
                .map(|tool| tool.definition.clone())
                .collect(),
        }
    }

    /// The model-visible transcript of the next request: every compacted span
    /// replaced by its summary. The durable [`SessionSnapshot::items`] are
    /// untouched — `snapshot()` still returns the complete transcript. Host
    /// `AgentHooks::request_context` is added separately to the request envelope.
    pub fn request_view(&self) -> Vec<Item> {
        request_view(
            &self.state.items,
            &self.state.compactions,
            self.state.prune.as_ref(),
        )
    }

    /// Compaction records currently applied to the request view, in order.
    pub fn compactions(&self) -> &[Compaction] {
        &self.state.compactions
    }

    /// Cut positions at which the transcript can be split without separating a
    /// tool call from its result. A span `[start, end)` may be compacted only
    /// when both ends are cuts — hosts pick the retention policy, this decides
    /// what is mechanically safe.
    pub fn compaction_cuts(&self) -> Vec<usize> {
        balanced_cuts(&self.state.items)
            .into_iter()
            .enumerate()
            .filter_map(|(cut, balanced)| {
                (balanced
                    && !self
                        .state
                        .compactions
                        .iter()
                        .any(|span| span.start < cut && cut < span.end))
                .then_some(cut)
            })
            .collect()
    }

    /// Validate and render a span of the current view, using durable indices.
    /// Existing summaries are indivisible and already-pruned outputs stay pruned.
    fn view_span(&self, start: usize, end: usize) -> Result<Vec<Item>> {
        let cuts = self.compaction_cuts();
        if start >= end || !cuts.contains(&start) || !cuts.contains(&end) {
            return Err(Error::new(
                ErrorKind::Configuration,
                "compaction needs a non-empty balanced span and must not split an existing summary",
            ));
        }
        Ok(crate::compaction::indexed_request_view(
            &self.state.items,
            &self.state.compactions,
            self.state.prune.as_ref(),
        )
        .into_iter()
        .filter_map(|(index, item)| (start <= index && index < end).then_some(item))
        .collect())
    }

    /// Replace `items[start..end]` with `summary` **in the model-visible view**.
    ///
    /// The durable transcript keeps every item, so a snapshot round-trip and
    /// [`Agent::clear_compactions`] both restore the original history. Later
    /// compactions may include entire existing summaries, which are replaced
    /// atomically by the new record; partial overlaps are rejected.
    pub fn compact(
        &mut self,
        start: usize,
        end: usize,
        summary: impl Into<String>,
    ) -> Result<Compaction> {
        let record = Compaction {
            start,
            end,
            summary: summary.into(),
        };
        let view = self.view_span(start, end)?;
        let mut next: Vec<_> = self
            .state
            .compactions
            .iter()
            .filter(|span| span.end <= start || span.start >= end)
            .cloned()
            .collect();
        next.push(record.clone());
        next.sort_by_key(|span| span.start);
        validate_compactions(&self.state.items, &next)?;
        // A replacement that does not shrink its source only loses information:
        // harness rejects one for the same reason.
        let shadowed = serde_json::to_vec(&view)
            .map_err(|_| Error::new(ErrorKind::Session, "cannot measure the compacted span"))?
            .len();
        let replacement = serde_json::to_vec(&[crate::compaction::summary_item(&record.summary)])
            .map_err(|_| Error::new(ErrorKind::Session, "cannot measure the summary"))?
            .len();
        if replacement >= shadowed {
            return Err(Error::new(
                ErrorKind::Configuration,
                "compaction summary must be smaller than the span it replaces",
            ));
        }
        self.state.compactions = next;
        self.calibration_start = records(&self.ledger).len();
        Ok(record)
    }

    /// Drop every compaction record. The transcript was never rewritten, so the
    /// next request carries the full history again.
    pub fn clear_compactions(&mut self) {
        self.state.compactions.clear();
        self.calibration_start = records(&self.ledger).len();
    }

    /// Trim tool outputs older than `through` in the model-visible view.
    ///
    /// Harness's `compaction-tool-result-pruner`, as a primitive: cheaper
    /// history may fit without a summary call at all. The transcript keeps the
    /// full output — only the request view is trimmed.
    pub fn prune_outputs(&mut self, through: usize, max_bytes: usize) -> Result<Prune> {
        let prune = Prune { through, max_bytes };
        crate::compaction::validate_prune(&self.state.items, Some(&prune))?;
        self.state.prune = Some(prune);
        self.calibration_start = records(&self.ledger).len();
        Ok(prune)
    }

    /// The active prune record, if any.
    pub fn prune(&self) -> Option<Prune> {
        self.state.prune
    }

    /// The session's completion objective, if one is set.
    pub fn goal(&self) -> Option<&Goal> {
        self.state.goal.as_ref()
    }

    /// Start a goal (the human path behind `/goal <objective>`).
    pub fn set_goal(
        &mut self,
        objective: impl Into<String>,
        max_rounds: Option<u64>,
    ) -> Result<Goal> {
        let goal = crate::goal::new_goal(objective.into(), max_rounds)?;
        if self
            .state
            .goal
            .as_ref()
            .is_some_and(|current| current.status != GoalStatus::Complete)
        {
            return Err(Error::new(
                ErrorKind::Configuration,
                "a goal already exists; clear it or complete it first",
            ));
        }
        self.state.goal = Some(goal.clone());
        Ok(goal)
    }

    /// Move the goal's status from the host (the human owns the session, so no
    /// revision check applies); the model path keeps compare-and-set.
    pub fn update_goal(&mut self, status: GoalStatus, note: Option<String>) -> Result<Goal> {
        let Some(goal) = self.state.goal.as_ref() else {
            return Err(Error::new(ErrorKind::Configuration, "no goal to update"));
        };
        let updated = Goal {
            revision: goal.revision.saturating_add(1),
            status,
            note: note.filter(|note| !note.trim().is_empty()),
            ..goal.clone()
        };
        self.state.goal = Some(updated.clone());
        Ok(updated)
    }

    /// Forget the goal entirely.
    pub fn clear_goal(&mut self) {
        self.state.goal = None;
    }

    /// Spend one round: increments the counter when the goal is active and the
    /// allowance is not exhausted. The host's round driver owns the loop.
    pub fn begin_goal_round(&mut self) -> Result<Option<Goal>> {
        let Some(goal) = self.state.goal.as_ref() else {
            return Ok(None);
        };
        if !goal.may_start_round() {
            return Ok(None);
        }
        let started = Goal {
            rounds_started: goal.rounds_started.saturating_add(1),
            ..goal.clone()
        };
        self.state.goal = Some(started.clone());
        Ok(Some(started))
    }
    /// Stop trimming tool outputs in the view.
    pub fn clear_prune(&mut self) {
        self.state.prune = None;
        self.calibration_start = records(&self.ledger).len();
    }

    /// Byte-level measurement of the next request, plus the provider's latest
    /// conversation input count for calibration.
    pub fn context_estimate(&self) -> Result<ContextEstimate> {
        let estimate = self.view_estimate()?;
        Ok(ContextEstimate {
            request_bytes: estimate.request_bytes,
            history_bytes: estimate.history_bytes,
            last_input_tokens: estimate.last_input_tokens,
            compactions: estimate.compactions,
            last_request_bytes: estimate.last_request_bytes,
        })
    }

    /// The newest measured conversation envelope: provider tokens plus the
    /// serialized size the provider priced.
    fn measured_conversation(&self) -> (Option<u64>, Option<usize>) {
        records(&self.ledger)
            .iter()
            .skip(self.calibration_start)
            .rev()
            .find_map(|record| {
                if record.purpose != RequestPurpose::Conversation {
                    return None;
                }
                let tokens = record.usage.as_ref().and_then(|usage| usage.input_tokens);
                tokens.map(|tokens| (Some(tokens), record.request_bytes))
            })
            .unwrap_or((None, None))
    }

    /// The same measurement plus every balanced cut, priced: what a host policy
    /// needs to choose a retention boundary without a second API round trip.
    pub fn view_estimate(&self) -> Result<ViewEstimate> {
        let view = self.request_view();
        let request_bytes =
            serde_json::to_vec(&self.build_request(view.clone()).measurement_body()?)
                .map_err(|_| Error::new(ErrorKind::Session, "cannot measure the request"))?
                .len();
        let history_bytes = serde_json::to_vec(&view)
            .map_err(|_| Error::new(ErrorKind::Session, "cannot measure the history"))?
            .len();
        let indexed = crate::compaction::indexed_request_view(
            &self.state.items,
            &self.state.compactions,
            self.state.prune.as_ref(),
        );
        let mut suffix = vec![0usize; self.state.items.len() + 1];
        // Array overhead plus commas: price exactly the visible suffix, without
        // resurrecting shadowed records or unpruned output bytes.
        let mut bytes = 1usize;
        for (index, item) in indexed.iter().rev() {
            bytes += serde_json::to_vec(item)
                .map_err(|_| Error::new(ErrorKind::Session, "cannot measure the history"))?
                .len()
                + 1;
            suffix[*index] = bytes;
        }
        suffix[self.state.items.len()] = 2;
        let cuts = self
            .compaction_cuts()
            .into_iter()
            .map(|index| crate::ViewCut {
                index,
                suffix_bytes: suffix[index],
            })
            .collect();
        let (last_input_tokens, last_request_bytes) = self.measured_conversation();
        Ok(ViewEstimate {
            request_bytes,
            history_bytes,
            items: self.state.items.len(),
            compactions: self.state.compactions.len(),
            compacted_through: self
                .state
                .compactions
                .last()
                .map(|compaction| compaction.end)
                .unwrap_or(0),
            pruned: self.state.prune.is_some(),
            pruned_through: self.state.prune.map_or(0, |prune| prune.through),
            last_input_tokens,
            last_request_bytes,
            cuts,
        })
    }

    /// Summarize an explicit balanced span with one auxiliary model call.
    ///
    /// The call replays the session system prompt, the registered tool schemas
    /// and the span's current visible messages (including prior summaries and
    /// pruning), then appends the instruction as the final user message. Only
    /// complete assistant text becomes the summary; incomplete responses and
    /// tool calls are rejected, reasoning is ignored. The caller
    /// decides whether to apply it with [`Agent::compact`].
    pub async fn summarize_span(
        &self,
        start: usize,
        end: usize,
        options: SummarizeOptions,
    ) -> Result<SummarizeOutcome> {
        let context = RequestContext::new(
            options.cancellation,
            options.timeout,
            4,
            self.ledger.clone(),
        )?;
        self.summarize_in_context(
            start,
            end,
            options.instruction,
            options.max_tokens,
            &context,
        )
        .await
    }

    async fn summarize_in_context(
        &self,
        start: usize,
        end: usize,
        instruction: Option<String>,
        max_tokens: Option<u32>,
        context: &RequestContext,
    ) -> Result<SummarizeOutcome> {
        context.check()?;
        let mut history = self.view_span(start, end)?;
        history.push(Item::user(
            instruction.unwrap_or_else(|| SUMMARIZE_INSTRUCTION.to_string()),
        ));
        let mut request = self.build_request(history);
        if let Some(max_tokens) = max_tokens {
            request.options.max_tokens = max_tokens;
        }
        let record_start = records(&self.ledger).len();
        let mut stream = self
            .client
            .stream_with_purpose(request, context.clone(), RequestPurpose::Compaction)
            .await?;
        let mut final_response = None;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Finished { response, .. } = event? {
                final_response = Some(response);
            }
        }
        let response = final_response.ok_or_else(|| {
            Error::new(
                ErrorKind::StreamClosed,
                "summarization stream has no final response",
            )
        })?;
        if response.status != ResponseStatus::Completed
            || response
                .output
                .iter()
                .any(|item| matches!(item, Item::FunctionCall { .. }))
        {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "summarization did not finish with complete text (possibly max_tokens); the previous view is retained",
            ));
        }
        let summary = response
            .output
            .iter()
            .filter_map(|item| match item {
                Item::Message {
                    role: MessageRole::Assistant,
                    content,
                    ..
                } => Some(content.iter().map(ContentPart::text).collect::<String>()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_string();
        if summary.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "summarization returned no text",
            ));
        }
        Ok(SummarizeOutcome {
            summary,
            usage: response.usage.clone(),
            requests: records(&self.ledger)[record_start..].to_vec(),
        })
    }

    pub async fn run<F, Fut>(
        &mut self,
        input: impl Into<String>,
        options: RunOptions,
        mut on_event: F,
    ) -> Result<RunOutcome>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        options.validate()?;
        self.ensure_boundary()?;
        if self.state.needs_response {
            return Err(Error::new(
                ErrorKind::Session,
                "continue the unfinished turn before adding user input",
            ));
        }
        let context = RequestContext::new(
            options.cancellation.clone(),
            options.timeout,
            options.max_requests,
            self.ledger.clone(),
        )?;
        context.check()?;
        let start = self.state.items.len();
        self.state.items.push(Item::user(input));
        self.state.needs_response = true;
        self.state.todos = None;
        self.drive(options, context, start, &mut on_event).await
    }

    pub async fn continue_run<F, Fut>(
        &mut self,
        options: RunOptions,
        on_event: F,
    ) -> Result<RunOutcome>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.resume_run(None, options, on_event).await
    }

    /// Add a user instruction before resuming an unfinished turn. The input is
    /// included in the RunStarted checkpoint and the very next model request.
    /// Completed tool results and the current plan remain intact; pending calls
    /// must first be explicitly resolved, just as for `continue_run`.
    pub async fn continue_run_with_input<F, Fut>(
        &mut self,
        input: impl Into<String>,
        options: RunOptions,
        on_event: F,
    ) -> Result<RunOutcome>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.resume_run(Some(input.into()), options, on_event).await
    }

    async fn resume_run<F, Fut>(
        &mut self,
        input: Option<String>,
        options: RunOptions,
        mut on_event: F,
    ) -> Result<RunOutcome>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        options.validate()?;
        self.ensure_boundary()?;
        if !self.state.needs_response {
            return Err(Error::new(
                ErrorKind::Session,
                "session has no unfinished turn",
            ));
        }
        let context = RequestContext::new(
            options.cancellation.clone(),
            options.timeout,
            options.max_requests,
            self.ledger.clone(),
        )?;
        context.check()?;
        let start = self.state.items.len();
        if let Some(input) = input {
            self.state.items.push(Item::user(input));
        }
        self.drive(options, context, start, &mut on_event).await
    }

    fn ensure_boundary(&self) -> Result<()> {
        self.state.validate()?;
        if !self.state.pending.is_empty() {
            return Err(Error::new(
                ErrorKind::NeedsResolution,
                "pending tool calls require explicit host resolution",
            ));
        }
        Ok(())
    }

    async fn drive<F, Fut>(
        &mut self,
        options: RunOptions,
        context: RequestContext,
        start: usize,
        on_event: &mut F,
    ) -> Result<RunOutcome>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.state.run_sequence = self
            .state
            .run_sequence
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorKind::Session, "run sequence exhausted"))?;
        let run = self.state.run_sequence;
        let record_start = records(&self.ledger).len();
        let result: Result<RunOutcome> = async {
            self.checkpoint(CheckpointKind::RunStarted, &context)
                .await?;
            emit(on_event, AgentEvent::RunStarted { run }, &context).await?;
            emit(
                on_event,
                AgentEvent::PlanChanged {
                    plan: self.plan_view(),
                },
                &context,
            )
            .await?;
            emit(
                on_event,
                AgentEvent::GoalChanged {
                    goal: self.state.goal.clone(),
                },
                &context,
            )
            .await?;
            let response = self.cycle(&options, &context, on_event).await?;
            let stop_reason = match response.status {
                ResponseStatus::Completed => StopReason::Completed,
                ResponseStatus::Incomplete => StopReason::Incomplete,
                _ => StopReason::Failed,
            };
            Ok(RunOutcome {
                stop_reason,
                response,
                new_items: self.state.items[start..].to_vec(),
                requests: records(&self.ledger)[record_start..].to_vec(),
            })
        }
        .await;
        let reason = match &result {
            Ok(outcome) => outcome.stop_reason.clone(),
            Err(error) => StopReason::Error(error.kind),
        };
        // Best-effort terminal notification even when the run token is cancelled.
        // A failed handler or dropped run future cannot guarantee delivery.
        if !matches!(&result, Err(error) if error.kind == ErrorKind::EventHandler) {
            let delivered = tokio::time::timeout(
                Duration::from_secs(1),
                on_event(AgentEvent::RunFinished { run, reason }),
            )
            .await;
            if result.is_ok() && !matches!(delivered, Ok(Ok(()))) {
                return Err(Error::new(
                    ErrorKind::EventHandler,
                    "terminal event handler failed or timed out",
                ));
            }
        }
        result
    }

    async fn cycle<F, Fut>(
        &mut self,
        options: &RunOptions,
        context: &RequestContext,
        on_event: &mut F,
    ) -> Result<Response>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut tool_calls = 0usize;
        // Forks share only turns completed before this invocation, never an open tool batch.
        let parent = self
            .subagent_manager
            .is_some()
            .then(|| Arc::new(self.subagent_parent()));
        loop {
            context.check()?;
            if self.receive_messages() {
                self.checkpoint(CheckpointKind::MessagesReceived, context)
                    .await?;
            }
            self.apply_view_policy(on_event, context).await?;
            let request = self.build_request(self.request_view());
            let bytes = serde_json::to_vec(&request.body(true)?)
                .map_err(|_| Error::protocol("cannot encode request"))?;
            if bytes.len() > options.max_input_bytes {
                return Err(Error::new(
                    ErrorKind::ContextLimitExceeded,
                    "serialized request exceeds the host input budget",
                ));
            }
            let mut stream = self
                .client
                .stream_with_context(request, context.clone())
                .await?;
            let mut final_response = None;
            while let Some(event) = stream.next().await {
                let event = event?;
                if let StreamEvent::Finished { response, .. } = &event {
                    if response.status == ResponseStatus::Completed {
                        let mut history = self.state.items.clone();
                        history.extend(response.output.clone());
                        validate_history(&history)?;
                        let pending = response
                            .output
                            .iter()
                            .filter_map(|item| match item {
                                Item::FunctionCall {
                                    call_id,
                                    name,
                                    arguments,
                                    ..
                                } => Some(PendingCall {
                                    call_id: call_id.clone(),
                                    name: name.clone(),
                                    arguments: arguments.clone(),
                                    state: PendingState::Ready,
                                }),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        self.state.items = history;
                        self.state.needs_response = !pending.is_empty();
                        self.state.pending = pending;
                        self.checkpoint(CheckpointKind::ModelResponse, context)
                            .await?;
                    }
                    final_response = Some((**response).clone());
                }
                emit(on_event, AgentEvent::Model(event), context).await?;
            }
            let response = final_response.ok_or_else(|| {
                Error::new(
                    ErrorKind::StreamClosed,
                    "model stream has no final response",
                )
            })?;
            if response.status != ResponseStatus::Completed {
                return Ok(response);
            }
            if self.state.pending.is_empty() {
                if self.receive_messages() {
                    self.checkpoint(CheckpointKind::MessagesReceived, context)
                        .await?;
                    continue;
                }
                return Ok(response);
            }
            while let Some(call) = self.state.pending.first().cloned() {
                context.check()?;
                if tool_calls >= options.max_tool_calls {
                    return Err(Error::new(
                        ErrorKind::BudgetExceeded,
                        "tool call budget exhausted",
                    ));
                }
                tool_calls += 1;
                let arguments = serde_json::from_str::<Value>(&call.arguments);
                let registered = self.tools.get(&call.name);
                let prepared = match (registered, arguments) {
                    (None, _) => Err("unknown tool".to_owned()),
                    (_, Err(_)) => Err("arguments are not valid JSON".to_owned()),
                    (Some(tool), Ok(arguments)) if arguments.is_object() => tool
                        .implementation
                        .validate(&arguments)
                        .map(|()| (tool.implementation.clone(), arguments))
                        .map_err(|e| e.to_string()),
                    _ => Err("arguments must be a JSON object".to_owned()),
                };
                let output = match prepared {
                    Err(message) => ToolOutput::error(message),
                    Ok((tool, arguments)) => {
                        let decision = match &self.hooks {
                            Some(hooks) => context.wait(hooks.authorize(call.clone())).await??,
                            None => ToolDecision::Allow,
                        };
                        match decision {
                            ToolDecision::Deny(reason) => ToolOutput::error(reason),
                            ToolDecision::Allow => {
                                if self.hooks.is_some() {
                                    self.state.pending[0].state = PendingState::Unknown;
                                    self.checkpoint(CheckpointKind::ToolIntent { call: self.state.pending[0].clone() }, context).await?;
                                }
                                emit(on_event, AgentEvent::ToolStarted { call_id: call.call_id.clone(), name: call.name.clone() }, context).await?;
                                self.state.pending[0].state = PendingState::Unknown;
                                let cancellation = context.cancellation.child_token();
                                let _cancel_on_drop = cancellation.clone().drop_guard();
                                let budget = call_budget(&*tool, &arguments, options);
                                let mut tool_request = context.clone();
                                tool_request.cancellation = cancellation.clone();
                                let deadline = budget.map_or(
                                    tokio::time::Instant::now() + crate::NO_DEADLINE,
                                    |budget| tokio::time::Instant::now() + budget,
                                );
                                let tool_context = ToolContext { call_id: call.call_id.clone(), cancellation: cancellation.clone(), deadline, max_output_bytes: options.max_tool_output_bytes, request: tool_request, local_session: self.local_session.clone(), parent: parent.clone() };
                                match run_tool(tool, arguments, tool_context, context, cancellation, budget).await? {
                                    // The call's budget expired: the model is told, and the
                                    // turn goes on with the rest of the batch.
                                    ToolRun::TimedOut => ToolOutput::error(format!(
                                        "the tool call timed out after {} and was stopped; whatever it already did is unverified — check the workspace before repeating it",
                                        budget.map_or_else(|| "its budget".into(), budget_label)
                                    )),
                                    ToolRun::Settled(Ok(output)) => output,
                                    ToolRun::Settled(Err(ToolError::Failed(message))) => ToolOutput::error(message),
                                    ToolRun::Settled(Err(ToolError::Uncertain(_))) => return Err(Error::new(ErrorKind::NeedsResolution, "tool reported uncertain side effects; resolve its result explicitly")),
                                }
                            }
                        }
                    }
                }.bounded(options.max_tool_output_bytes.saturating_sub("Tool error: ".len()));
                // A committed step is progress: the batch moves even when one
                // call in it timed out or failed.
                context.touch();
                // From here on `output` is the committed result, not the tool's
                // proposal: checkpoints, host events and the next request all
                // carry the same text (see `resolve_tool`).
                let output = self.resolve_tool(&call.call_id, output)?;
                self.checkpoint(
                    CheckpointKind::ToolResult {
                        call_id: call.call_id.clone(),
                        output: output.clone(),
                    },
                    context,
                )
                .await?;
                if matches!(call.name.as_str(), "create_goal" | "update_goal") {
                    emit(
                        on_event,
                        AgentEvent::GoalChanged {
                            goal: self.state.goal.clone(),
                        },
                        context,
                    )
                    .await?;
                }
                if call.name == "todo_write"
                    && crate::PlanView::from_tool_output(&output)?.is_some()
                {
                    emit(
                        on_event,
                        AgentEvent::PlanChanged {
                            plan: self.plan_view(),
                        },
                        context,
                    )
                    .await?;
                }
                // Advisory loop guard (see `AgentHooks::tool_reminder`): the
                // text is queued, not emitted, so it reaches the model with the
                // next request after this result — never before it, and never
                // in place of the result.
                if let Some(hooks) = &self.hooks {
                    let reminder = context.wait(hooks.tool_reminder(&call, &output)).await??;
                    if let Some(text) = reminder {
                        self.inbox.send(text)?;
                    }
                }
                emit(
                    on_event,
                    AgentEvent::ToolFinished {
                        call_id: call.call_id,
                        output,
                    },
                    context,
                )
                .await?;
            }
        }
    }
}

impl Agent {
    /// Ask the host's view policy at a request boundary and apply what it asks
    /// for. A prune is followed by one more question (so a policy can trim and
    /// then condense at the same boundary); a condensation ends the boundary,
    /// because the retention policy already decided what stays verbatim.
    ///
    /// A refused change is reported through [`AgentEvent::ViewChanged`] and the
    /// request goes out with the unchanged view: condensing is an optimization,
    /// never a reason to fail a turn.
    async fn apply_view_policy<F, Fut>(
        &mut self,
        on_event: &mut F,
        context: &RequestContext,
    ) -> Result<()>
    where
        F: FnMut(AgentEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        for _ in 0..2 {
            let Some(hooks) = self.hooks.clone() else {
                return Ok(());
            };
            let estimate = self.view_estimate()?;
            let Some(request) = context.wait(hooks.view_request(&estimate)).await?? else {
                return Ok(());
            };
            let before = estimate.request_bytes;
            let change = match request {
                crate::ViewRequest::Prune { through, max_bytes } => {
                    match self.prune_outputs(through, max_bytes) {
                        Ok(_) => crate::ViewChange::Pruned {
                            through,
                            max_bytes,
                            before_bytes: before,
                            after_bytes: self.view_estimate()?.request_bytes,
                        },
                        Err(error) => crate::ViewChange::Failed {
                            message: error.message,
                        },
                    }
                }
                crate::ViewRequest::Condense {
                    start,
                    end,
                    instruction,
                    max_tokens,
                } => match self
                    .summarize_in_context(start, end, instruction, max_tokens, context)
                    .await
                {
                    Ok(outcome) => match self.compact(start, end, outcome.summary) {
                        Ok(_) => crate::ViewChange::Compacted {
                            start,
                            end,
                            before_bytes: before,
                            after_bytes: self.view_estimate()?.request_bytes,
                        },
                        Err(error) => crate::ViewChange::Failed {
                            message: error.message,
                        },
                    },
                    Err(error)
                        if matches!(
                            error.kind,
                            ErrorKind::Cancelled | ErrorKind::Timeout | ErrorKind::BudgetExceeded
                        ) =>
                    {
                        return Err(error);
                    }
                    Err(error) => crate::ViewChange::Failed {
                        message: error.message,
                    },
                },
            };
            let failed = matches!(change, crate::ViewChange::Failed { .. });
            let pruned = matches!(change, crate::ViewChange::Pruned { .. });
            if !failed {
                self.checkpoint(CheckpointKind::ViewChanged, context)
                    .await?;
            }
            emit(on_event, AgentEvent::ViewChanged { change }, context).await?;
            if failed || !pruned {
                return Ok(());
            }
        }
        Ok(())
    }
}

async fn emit<F, Fut>(handler: &mut F, event: AgentEvent, context: &RequestContext) -> Result<()>
where
    F: FnMut(AgentEvent) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    context
        .wait(handler(event))
        .await?
        .map_err(|_| Error::new(ErrorKind::EventHandler, "event handler failed"))
}

/// One tool call's budget: what the tool declares for these arguments, else the
/// deployment's backstop. `None` means the call has no timer at all.
///
/// The declaration wins because it is the tool's own contract — the harness
/// reads the same value from the tool definition rather than capping every call
/// at one host-wide number, so a `bash` call that asks for ten minutes gets ten
/// minutes and a `read` that asks for nothing still cannot hang the turn.
fn call_budget(tool: &dyn Tool, arguments: &Value, options: &RunOptions) -> Option<Duration> {
    /// Past this a declaration is not a limit but a mistake: clamping keeps one
    /// from turning a single call into an unbounded wait inside the turn.
    const MAX_CALL_BUDGET: Duration = Duration::from_secs(60 * 60);
    match tool.call_budget(arguments) {
        CallBudget::Backstop => Some(options.tool_timeout),
        CallBudget::Own(declared) => Some(declared.min(MAX_CALL_BUDGET)),
        CallBudget::Unbounded => None,
    }
}

/// A budget in the units a human reads: seconds when there is at least one,
/// milliseconds below that (`bash` asks in milliseconds and tests use tiny ones).
fn budget_label(budget: Duration) -> String {
    if budget.as_secs() > 0 {
        format!("{}s", budget.as_secs())
    } else {
        format!("{}ms", budget.as_millis())
    }
}

/// What one tool call produced under its budget.
enum ToolRun {
    Settled(std::result::Result<ToolOutput, ToolError>),
    /// The budget expired, and the call did not settle within its cleanup
    /// grace after being asked to stop.
    TimedOut,
}

/// Drive one tool call under its budget (`None`: no timer), inside the run's
/// cancellation.
///
/// On expiry the call's token is cancelled — a compliant tool stops and reports
/// what it managed to do — and it gets its cleanup grace to settle. A tool that
/// still will not settle is dropped, which is its hard stop, and the caller
/// answers the model with an error result. deepseek-harness's
/// `dsh-tool-call-timeout-policy` does the same thing for the same reason: one
/// slow call is not a failed turn.
async fn run_tool(
    tool: Arc<dyn Tool>,
    arguments: Value,
    tool_context: ToolContext,
    context: &RequestContext,
    cancellation: crate::CancellationToken,
    budget: Option<Duration>,
) -> Result<ToolRun> {
    let grace = tool.cleanup_grace();
    let mut call = std::pin::pin!(tool.execute(arguments, tool_context));
    let Some(budget) = budget else {
        // No timer of its own: the work bounds the call, cancellation ends it.
        return tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                Err(Error::new(ErrorKind::Cancelled, "operation cancelled"))
            }
            result = &mut call => Ok(ToolRun::Settled(result)),
        };
    };
    let mut stop_at = tokio::time::Instant::now() + budget;
    let mut stopping = false;
    loop {
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                return Err(Error::new(ErrorKind::Cancelled, "operation cancelled"));
            }
            result = &mut call => return Ok(ToolRun::Settled(result)),
            _ = tokio::time::sleep_until(stop_at) => {
                if stopping {
                    return Ok(ToolRun::TimedOut);
                }
                stopping = true;
                cancellation.cancel();
                stop_at = tokio::time::Instant::now() + grace;
            }
        }
    }
}
