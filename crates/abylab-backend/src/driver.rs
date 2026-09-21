//! The abycore agent driver: owns the Tokio runtime and the `Agent`, turns
//! [`Cmd`]s into `agent.run()` turns, and streams [`Event`]s back to the TUI.
//!
//! Architecture mirrors the Martty ACP client (`acp.rs`): the driver thread
//! runs one `block_on` loop; the UI thread never blocks. Interrupts ride a
//! dedicated channel so they land even while a turn is in flight. A prompt
//! arriving mid-turn queues behind the active turn (durable-inbox semantics),
//! while read-only queries (`/resume`'s listing, `/model`'s catalog) ride a
//! third channel served off the loop — a running turn never holds a picker.

use std::sync::Arc;
use std::time::Duration;

use abycore::{
    Agent, AgentEvent, AgentHooks, CancellationToken, ClientConfig, DeepSeekClient,
    DeepSeekWebSearch, ErrorKind, LocalToolConfig, LocalTools, ModelOptions, PermissionMode,
    ReasoningEffort, RunOptions, RunOutcome, SearchConfig, StopReason, TodoWriteTool, ToolDecision,
    ToolOutput,
};
use tokio::sync::{mpsc, oneshot};

use crate::contract::{
    AskOption, Cmd, CompactionConfig, CtlEvent, DriverConfig, Event, PermissionReply, PlanItem,
    PlanStatus, TurnLimits, UiEvent,
};

const SERVER_LABEL: &str = "abycore · deepseek-responses";

/// Handle to one running driver. `send` never blocks; turns serialize inside
/// the driver, so a prompt sent mid-turn queues after it. Read-only queries
/// (`ListSessions`, `FetchCatalog`) take a side channel instead: the driver
/// loop is busy for the whole turn, and neither `/resume`'s picker nor the
/// `/model` listing should wait for it.
pub struct DriverHandle {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    query_tx: mpsc::UnboundedSender<Cmd>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DriverHandle {
    pub fn send(&self, cmd: Cmd) {
        // The turn loop answers everything in order; a query that touches no
        // agent state is served by the side task so it lands immediately.
        let tx = match &cmd {
            Cmd::ListSessions { .. } | Cmd::FetchCatalog | Cmd::FetchSkills => &self.query_tx,
            _ => &self.cmd_tx,
        };
        let _ = tx.send(cmd);
    }

    /// Cancel the active turn (no-op when idle).
    pub fn interrupt(&self) {
        let _ = self.interrupt_tx.send(());
    }

    /// Stop the loop, cancel any turn; the join happens when the handle
    /// (and its clones) drop.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        self.interrupt();
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        self.interrupt();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Spawn the driver thread. `sink` receives every event; it must not block.
pub fn spawn(
    cfg: DriverConfig,
    sink: impl Fn(Event) + Send + Sync + 'static,
) -> Result<DriverHandle, String> {
    if cfg
        .max_tokens
        .is_some_and(|tokens| tokens == 0 || tokens > u32::MAX as u64)
    {
        return Err("--max-tokens must be between 1 and 4294967295".into());
    }
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (query_tx, query_rx) = mpsc::unbounded_channel::<Cmd>();
    let (interrupt_tx, interrupt_rx) = mpsc::unbounded_channel::<()>();
    let join = std::thread::Builder::new()
        .name("aby-driver".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    sink(Event::Ctl(CtlEvent::Error(format!(
                        "tokio runtime failed: {err}"
                    ))));
                    return;
                }
            };
            runtime.block_on(drive(cfg, cmd_rx, query_rx, interrupt_rx, Arc::new(sink)));
        })
        .map_err(|err| format!("spawn aby driver: {err}"))?;
    Ok(DriverHandle {
        cmd_tx,
        query_tx,
        interrupt_tx,
        join: Some(join),
    })
}

/// What a freshly built agent needs from the host: where its events go, and how
/// its model-visible view may be reshaped at request boundaries.
#[derive(Clone)]
struct HostPolicy {
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    compaction: Option<CompactionConfig>,
    max_tokens: Option<u32>,
    agent_sessions: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    instructions: crate::instructions::InstructionSource,
    /// Discovered once at launch; the menu, the `/<name>` injection and the
    /// `skill` tool all serve this one snapshot.
    skills: Arc<abycore::SkillCatalog>,
}

/// Host hooks: sandboxed modes can route tool calls through the TUI's
/// permission overlay, and every safe point persists the session under
/// `workspace/.abycore`.
struct UiHooks {
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    permission_mode: PermissionMode,
    /// `None` disables persistence (e.g. a store failure).
    persist: Option<std::sync::Weak<PersistState>>,
    /// Advisory loop guard behind [`abycore::AgentHooks::tool_reminder`].
    guard: RepeatGuard,
    /// Host context policy answered at every request boundary
    /// ([`abycore::AgentHooks::view_request`]); `None` disables automatic
    /// compaction, leaving only `/compact` and overflow recovery.
    compaction: Option<CompactionConfig>,
    /// The stable user-context prefix: the workspace instruction baseline plus
    /// the skill index, or whichever of the two exists.
    context: Option<String>,
}

/// deepseek-harness's `dsh-repeat-tool-reminder`, as host policy: count
/// consecutive identical tool calls and, at [`RepeatGuard::THRESHOLDS`],
/// hand the model a short reminder instead of blocking the call. Harness ships
/// it in the base bundle precisely because its loop has no turn budget — the
/// same reason abylab wants it now that the SDK budgets are a distant backstop.
#[derive(Default)]
struct RepeatGuard {
    /// Tool name and canonical arguments of the previous completed call.
    last: std::sync::Mutex<Option<(String, serde_json::Value)>>,
    count: std::sync::atomic::AtomicUsize,
}

impl RepeatGuard {
    /// Repeat counts that trigger a reminder (the harness default).
    const THRESHOLDS: [usize; 3] = [3, 5, 8];
    /// Calls whose repeats are legitimate work: an idempotent checklist rewrite.
    const EXCLUDED: [&'static str; 1] = ["todo_write"];
    /// Arguments preview cap (the harness `argumentsPreviewChars` default).
    const PREVIEW_CHARS: usize = 500;

    /// Forget the streak. Runs at every run boundary, so a fresh prompt — or a
    /// continuation segment — is never counted as a repeat of what came before.
    fn reset(&self) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.count.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Count one completed call; `Some(text)` once a threshold is reached.
    fn observe(&self, call: &abycore::PendingCall) -> Option<String> {
        if Self::EXCLUDED.contains(&call.name.as_str()) {
            return None;
        }
        // Property order must not defeat the comparison (harness normalizes the
        // same way): `serde_json::Value` objects compare by content.
        let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or(serde_json::Value::Null);
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let same = last
            .as_ref()
            .is_some_and(|(name, previous)| name == &call.name && previous == &arguments);
        let count = if same {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        } else {
            *last = Some((call.name.clone(), arguments.clone()));
            self.count.store(1, std::sync::atomic::Ordering::SeqCst);
            1
        };
        drop(last);
        Self::THRESHOLDS
            .contains(&count)
            .then(|| Self::text(&call.name, &arguments, count))
    }

    /// `count == 3` stays short, like harness's first nudge; 5 and 8 name the
    /// call and preview its arguments so the model can see what it keeps doing.
    fn text(name: &str, arguments: &serde_json::Value, count: usize) -> String {
        if count <= 3 {
            return format!(
                "Loop guard: `{name}` has now been called {count} times in a row with identical \
                 arguments. Read the result you already have; if it did not change anything, \
                 change approach or finish instead of calling it again."
            );
        }
        let rendered = arguments.to_string();
        let preview = if rendered.chars().count() > Self::PREVIEW_CHARS {
            format!(
                "{}…",
                rendered
                    .chars()
                    .take(Self::PREVIEW_CHARS)
                    .collect::<String>()
            )
        } else {
            rendered
        };
        format!(
            "Loop guard: identical call #{count} — `{name}` with arguments {preview}. That call \
             has returned the same result {count} times, so repeating it cannot make progress. \
             Analyze the last result, then change the arguments or the approach, gather different \
             evidence, or finish and report what you have."
        )
    }
}

/// The exclusive writer is reserved before a session is bound to the UI.
struct PersistState {
    store: abycore::SessionStore,
    writer: std::sync::Mutex<abycore::SessionWriter>,
}

impl PersistState {
    fn with_writer(store: &abycore::SessionStore, writer: abycore::SessionWriter) -> Self {
        Self {
            store: store.clone(),
            writer: std::sync::Mutex::new(writer),
        }
    }
}

/// Owns the session writer independently from hooks retained by children.
/// Settings changes mutate the live Agent; switching sessions drops this owner.
struct SessionAgent {
    inner: Agent,
    persist: Option<Arc<PersistState>>,
}

impl std::ops::Deref for SessionAgent {
    type Target = Agent;
    fn deref(&self) -> &Agent {
        &self.inner
    }
}

impl std::ops::DerefMut for SessionAgent {
    fn deref_mut(&mut self) -> &mut Agent {
        &mut self.inner
    }
}

impl SessionAgent {
    fn save(&self) -> abycore::Result<()> {
        match &self.persist {
            Some(persist) => persist.save(&self.snapshot()),
            None => Ok(()),
        }
    }

    fn set_policy(&mut self, mode: PermissionMode, host: &HostPolicy) {
        let mut instructions = host.instructions.load();
        // A restored summary may mention an instruction file that was deleted.
        // Explicitly clear that older baseline even when discovery finds none.
        if instructions.text.is_none() && !self.inner.snapshot().items.is_empty() {
            instructions.text = Some(crate::instructions::EMPTY_BASELINE.into());
        }
        for warning in instructions.warnings {
            (host.sink)(Event::Ctl(CtlEvent::Error(format!(
                "workspace instructions: {warning}"
            ))));
        }
        // The skill index rides the same stable prefix: both are fixed for the
        // session, so provider calibration stays valid.
        let context = match (instructions.text, host.skills.index_block()) {
            (Some(baseline), Some(index)) => Some(format!("{baseline}\n{index}")),
            (baseline, index) => baseline.or(index),
        };
        self.inner.set_hooks(Arc::new(UiHooks {
            sink: Arc::clone(&host.sink),
            permission_mode: mode,
            persist: self.persist.as_ref().map(Arc::downgrade),
            guard: RepeatGuard::default(),
            compaction: host.compaction,
            context,
        }));
    }
}

impl PersistState {
    fn save(&self, snapshot: &abycore::SessionSnapshot) -> abycore::Result<()> {
        let mut writer = self.writer.lock().map_err(|_| {
            abycore::Error::new(abycore::ErrorKind::Session, "persist lock poisoned")
        })?;
        // Preserve stored titles on resume; defer a new title until a prompt exists.
        if writer.title().is_none()
            && let Some(title) = first_user_text(snapshot)
                .map(|text| text.chars().take(48).collect::<String>())
                .filter(|text| !text.is_empty())
        {
            self.store.set_title(&mut writer, &title)?;
        }
        self.store
            .append_checkpoint(&mut writer, snapshot.run_sequence, snapshot)
    }
}

impl AgentHooks for UiHooks {
    fn request_context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    fn checkpoint<'a>(
        &'a self,
        kind: abycore::CheckpointKind,
        snapshot: abycore::SessionSnapshot,
    ) -> futures_util::future::BoxFuture<'a, abycore::Result<()>> {
        if kind == abycore::CheckpointKind::RunStarted {
            self.guard.reset();
        }
        let result = self
            .persist
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .map_or(Ok(()), |persist| persist.save(&snapshot));
        Box::pin(async move { result })
    }

    /// The host's context policy, answered at every request boundary — this is
    /// what makes automatic compaction work *inside* a turn, harness's
    /// `agent/pre-step` seam, not just between turns.
    fn view_request<'a>(
        &'a self,
        estimate: &'a abycore::ViewEstimate,
    ) -> futures_util::future::BoxFuture<'a, abycore::Result<Option<abycore::ViewRequest>>> {
        let Some(policy) = self.compaction else {
            return Box::pin(async { Ok(None) });
        };
        // Provider-calibrated when a measured envelope is available, bounded by
        // the conservative byte estimate otherwise.
        if estimate.tokens() < policy.threshold_tokens() {
            return Box::pin(async { Ok(None) });
        }
        let keep_bytes = policy
            .retained_tokens()
            .saturating_mul(estimate.bytes_per_token() as u64) as usize;
        // Consolidate existing summaries with older visible history.
        let start = 0;
        let Some(end) = estimate
            .cuts
            .iter()
            .rev()
            .find(|cut| cut.index > estimate.compacted_through && cut.suffix_bytes >= keep_bytes)
            .map(|cut| cut.index)
        else {
            return Box::pin(async { Ok(None) });
        };
        // Trim first: it costs no model call, and the SDK asks again at this
        // same boundary, so a still-over-budget request is condensed next.
        let request = if policy.prune_tool_bytes > 0 && estimate.pruned_through < end {
            abycore::ViewRequest::Prune {
                through: end,
                max_bytes: policy.prune_tool_bytes,
            }
        } else {
            abycore::ViewRequest::Condense {
                start,
                end,
                instruction: None,
                max_tokens: Some(policy.max_tokens),
            }
        };
        Box::pin(async move { Ok(Some(request)) })
    }

    /// The advisory half of the loop guard: text queued after the tool result,
    /// delivered with the next request. It never denies or delays a call.
    fn tool_reminder<'a>(
        &'a self,
        call: &'a abycore::PendingCall,
        _output: &'a ToolOutput,
    ) -> futures_util::future::BoxFuture<'a, abycore::Result<Option<String>>> {
        let reminder = self.guard.observe(call);
        Box::pin(async move { Ok(reminder) })
    }

    fn authorize<'a>(
        &'a self,
        call: abycore::PendingCall,
    ) -> futures_util::future::BoxFuture<'a, abycore::Result<ToolDecision>> {
        // Full access is the explicit trusted-directory preset. File tools
        // already enforce the selected root policy, so safe reads and
        // workspace-scoped mutations do not need a second prompt. Bash and
        // unknown tools still go through approval in sandboxed modes.
        // `glob`/`grep` are workspace-rooted reads, exactly like `read`.
        // `todo_write` only updates the in-memory checklist, so it never asks.
        if self.permission_mode == PermissionMode::FullAccess
            || matches!(call.name.as_str(), "read" | "glob" | "grep")
            || call.name == "todo_write"
            // Goal tools only read and update session metadata, exactly like
            // the checklist: they never touch the workspace or the network.
            || matches!(call.name.as_str(), "get_goal" | "create_goal" | "update_goal")
            // `skill` reads a markdown file discovery already loaded.
            || call.name == abycore::SkillTool::NAME
            || (self.permission_mode == PermissionMode::WorkspaceWrite
                && matches!(call.name.as_str(), "write" | "edit"))
        {
            return Box::pin(async { Ok(ToolDecision::Allow) });
        }
        if self.permission_mode == PermissionMode::ReadOnly
            && matches!(call.name.as_str(), "write" | "edit")
        {
            return Box::pin(async {
                Ok(ToolDecision::Deny(
                    "read-only permission mode denies file mutations".into(),
                ))
            });
        }
        let sink = Arc::clone(&self.sink);
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            sink(Event::PermissionAsk {
                title: format!("allow {}?", call.name),
                options: vec![
                    AskOption {
                        option_id: "allow".into(),
                        kind: "allow_once".into(),
                        name: "Allow once".into(),
                    },
                    AskOption {
                        option_id: "reject".into(),
                        kind: "reject_once".into(),
                        name: "Reject".into(),
                    },
                ],
                reply: reply_tx,
            });
            let decision = reply_rx.await.unwrap_or(PermissionReply::Cancelled);
            match decision {
                PermissionReply::Selected(id) if id == "allow" => Ok(ToolDecision::Allow),
                _ => Ok(ToolDecision::Deny("denied by user".into())),
            }
        })
    }
}

async fn drive(
    cfg: DriverConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    query_rx: mpsc::UnboundedReceiver<Cmd>,
    mut interrupt_rx: mpsc::UnboundedReceiver<()>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
) {
    let ctl = |event: CtlEvent| sink(Event::Ctl(event));
    // Turn budgets are fixed at launch (CLI/env): a mid-session change would
    // silently alter how the next segment behaves.
    let limits = cfg.limits;
    let compaction = cfg.compaction;
    // Skills come from the workspace's `.agents/skills`, discovered once per
    // run. Discovery is a directory read, but the snapshot must stay stable:
    // the menu, the injection and the tool all answer from it, and a mid-session
    // rescan would let them disagree.
    let skills = Arc::new(abycore::SkillCatalog::discover(std::path::Path::new(
        &cfg.workspace,
    )));
    for warning in skills.warnings() {
        ctl(CtlEvent::Error(format!("skills: {warning}")));
    }
    let host = HostPolicy {
        sink: Arc::clone(&sink),
        compaction,
        max_tokens: cfg
            .max_tokens
            .map(|tokens| u32::try_from(tokens).expect("validated max_tokens")),
        agent_sessions: Arc::default(),
        instructions: crate::instructions::InstructionSource {
            workspace: cfg.workspace.clone().into(),
            home: cfg.home.as_ref().map(Into::into),
        },
        skills: Arc::clone(&skills),
    };
    // Harness arms goal continuation explicitly: creating a goal from the model
    // does not start spending rounds, `/goal <objective>` or `/goal resume` does.
    let mut goal_armed = false;

    // The TUI resolves the key (--api-key override, else the /login store);
    // the environment is not consulted.
    let mut api_key = cfg.api_key.clone().unwrap_or_default();
    // The query task answers `/model`'s catalog fetch too, and `/login` can
    // rotate the key mid-session: the loop publishes every change here.
    let live_key = Arc::new(std::sync::Mutex::new(api_key.clone()));
    let mut model = cfg.model.clone();
    let mut effort = parse_effort(&cfg.reasoning);

    if api_key.trim().is_empty() {
        ctl(CtlEvent::Error(
            "no API key — /login <apikey> stores one · get a key at https://platform.deepseek.com/"
                .into(),
        ));
    } else {
        ctl(CtlEvent::Ready {
            server: SERVER_LABEL.into(),
        });
    }

    // Trusted-directory default: abylab runs with full access unless the
    // host launches (or the user switches to) a stricter preset.
    let mut permission_mode = cfg
        .permission
        .as_deref()
        .and_then(parse_permission_mode)
        .unwrap_or(PermissionMode::FullAccess);
    let mut local = match build_local(&cfg.workspace, permission_mode) {
        Ok(local) => Some(local),
        Err(err) => {
            ctl(CtlEvent::Error(err));
            None
        }
    };
    let store = match &cfg.sessions_root {
        Some(root) => abycore::SessionStore::at(root, &cfg.workspace).ok(),
        None => abycore::SessionStore::new(&cfg.workspace).ok(),
    };
    // Read-only queries answer off the turn loop (`DriverHandle::send` routes
    // them here): the loop is busy for a whole turn, and both `/resume`'s
    // picker and the `/model` catalog have to answer while the agent is still
    // working. The task owns a store clone, the live key and the same sink, so
    // it needs nothing from the loop.
    {
        let store = store.clone();
        let base_url = cfg.base_url.clone();
        let live_key = Arc::clone(&live_key);
        let sink = Arc::clone(&sink);
        let skills = Arc::clone(&skills);
        tokio::spawn(async move {
            let mut queries = query_rx;
            while let Some(cmd) = queries.recv().await {
                match cmd {
                    Cmd::ListSessions { prefix } => sink(Event::Ctl(CtlEvent::SessionList {
                        sessions: session_rows(store.as_ref()),
                        prefix,
                    })),
                    Cmd::FetchCatalog => spawn_catalog_fetch(
                        current_key(&live_key),
                        base_url.clone(),
                        Arc::clone(&sink),
                    ),
                    Cmd::FetchSkills => sink(Event::Ctl(CtlEvent::Skills {
                        skills: skill_rows(&skills),
                    })),
                    _ => {}
                }
            }
        });
    }
    // Subagent delegation: one owner for the whole driver, shared by every
    // fresh/restored agent; its broadcast feeds the TUI's subagent views.
    let subagents: Option<Arc<abycore::Subagents>> = local.as_ref().map(|_| {
        let child_sink = Arc::clone(&sink);
        Arc::new(
            abycore::Subagents::with_config(abycore::SubagentConfig {
                run_options: RunOptions {
                    max_requests: limits.max_requests,
                    max_tool_calls: limits.max_tool_calls,
                    timeout: limits.run_timeout,
                    tool_timeout: limits.tool_timeout,
                    ..Default::default()
                },
                hooks: Some(Arc::new(move |_| {
                    Ok(Arc::new(UiHooks {
                        sink: Arc::clone(&child_sink),
                        // Parent authorization remains the delegation boundary.
                        permission_mode: PermissionMode::FullAccess,
                        persist: None,
                        guard: RepeatGuard::default(),
                        compaction,
                        // The SDK inherits the parent's request context, so a
                        // child sees the same skill index; the skill tool
                        // itself arrives through tool inheritance.
                        context: None,
                    }))
                })),
                ..Default::default()
            })
            .expect("valid subagent configuration"),
        )
    });
    if let Some(subagents) = &subagents {
        spawn_subagent_forwarder(
            Arc::clone(subagents),
            Arc::clone(&sink),
            Arc::clone(&host.agent_sessions),
        );
    }
    // Retain the intended restore while credentials are unavailable or a
    // load fails. Login must never turn a failed restore into a fresh session.
    let mut resume_target = cfg.resume.clone();
    let mut active_session = resume_target
        .clone()
        .unwrap_or_else(|| cfg.session_id.clone());
    let mut agent = None;
    if !api_key.trim().is_empty()
        && let Some(local) = local.as_ref()
    {
        let ctx = AgentContext {
            local,
            api_key: &api_key,
            base_url: cfg.base_url.as_deref(),
            store: store.as_ref(),
            session_id: &active_session,
            subagents: subagents.as_ref(),
            host: &host,
        };
        let result = if resume_target.is_some() {
            resume_agent(&ctx)
        } else {
            fresh_agent(&ctx, &model, effort)
        };
        match result {
            Ok(next) => agent = Some(next),
            Err(error) => ctl(CtlEvent::Error(format!("session open failed: {error}"))),
        }
    }
    if let Some(current) = agent.as_ref() {
        bind_session(
            current,
            &active_session,
            resume_target
                .as_ref()
                .map(|_| format!("resumed · {} turns", current.snapshot().run_sequence)),
            resume_target.is_some(),
            &sink,
        );
    }
    emit_permission_facts(&sink, &active_session, permission_mode);

    while let Some(cmd) = cmd_rx.recv().await {
        let cmd = match cmd {
            Cmd::PromptForSession { session_id, text } => {
                if session_id != active_session {
                    ctl(CtlEvent::Error(
                        "prompt rejected: the active session changed".into(),
                    ));
                    continue;
                }
                Cmd::Prompt { text }
            }
            cmd => cmd,
        };
        let restoring = matches!(cmd, Cmd::Resume { .. });
        match cmd {
            Cmd::PromptForSession { .. } => unreachable!("normalized above"),
            // The skill catalog is a pure read of the launch snapshot and the handle
            // routes it to the query task so it answers mid-turn; this arm
            // covers a direct send on the loop channel.
            Cmd::FetchSkills => ctl(CtlEvent::Skills {
                skills: skill_rows(&skills),
            }),
            Cmd::Goal { arg } => {
                let Some(agent) = agent.as_mut() else {
                    ctl(CtlEvent::TuiOpFailed(
                        "agent unavailable — /login <apikey> first, or restart abylab".into(),
                    ));
                    continue;
                };
                let mut ctx = TurnCtx {
                    session: &active_session,
                    sink: &sink,
                    interrupt_rx: &mut interrupt_rx,
                    limits,
                    compaction,
                };
                let arg = arg.trim();
                let (arg, max_rounds) = parse_goal_argument(arg);
                match arg {
                    "" | "status" => match agent.goal() {
                        Some(goal) => ctl(CtlEvent::TuiOpDone(format!(
                            "goal · {} / 目标 · {}",
                            goal.summary(),
                            goal.summary()
                        ))),
                        None => ctl(CtlEvent::TuiOpFailed(
                            "no goal is set — /goal <objective> / 当前没有目标：/goal <目标>"
                                .into(),
                        )),
                    },
                    "rounds" => ctl(CtlEvent::TuiOpFailed(
                        "usage: /goal @<rounds> <objective> / 用法：/goal @<轮数> <目标>".into(),
                    )),
                    "pause" | "resume" | "complete" | "clear" => {
                        let outcome = match arg {
                            "clear" => {
                                agent.clear_goal();
                                goal_armed = false;
                                Ok("goal cleared / 目标已清除".to_string())
                            }
                            _ => {
                                let status = match arg {
                                    "pause" => abycore::GoalStatus::Paused,
                                    "resume" => abycore::GoalStatus::Active,
                                    _ => abycore::GoalStatus::Complete,
                                };
                                agent.update_goal(status, None).map(|goal| {
                                    format!("goal → {} / 目标 → {}", goal.summary(), goal.summary())
                                })
                            }
                        };
                        match outcome {
                            Ok(message) => {
                                if let Err(error) = agent.save() {
                                    goal_armed = false;
                                    ctl(CtlEvent::TuiOpFailed(format!(
                                        "goal save failed: {error}"
                                    )));
                                    continue;
                                }
                                match arg {
                                    // Resuming an idle goal continues it, exactly
                                    // like a fresh goal: arming is the human opt-in.
                                    "resume" => {
                                        goal_armed = true;
                                        ctl(CtlEvent::TuiOpDone(message));
                                        drive_goal_rounds(agent, &mut ctx, &ctl).await;
                                        goal_armed = goal_armed
                                            && agent
                                                .goal()
                                                .is_some_and(abycore::Goal::may_start_round);
                                        continue;
                                    }
                                    "pause" | "complete" | "clear" => goal_armed = false,
                                    _ => {}
                                }
                                ctl(CtlEvent::TuiOpDone(message));
                            }
                            Err(error) => ctl(CtlEvent::TuiOpFailed(format!(
                                "goal update failed: {error} / 目标更新失败：{error}"
                            ))),
                        }
                    }
                    objective => match agent.set_goal(objective, max_rounds) {
                        Ok(goal) => {
                            if let Err(error) = agent.save() {
                                ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
                                continue;
                            }
                            goal_armed = true;
                            ctl(CtlEvent::TuiOpDone(format!(
                                "goal · {} / 目标 · {} — starting round 1",
                                goal.summary(),
                                goal.summary()
                            )));
                            drive_goal_rounds(agent, &mut ctx, &ctl).await;
                            goal_armed = goal_armed
                                && agent.goal().is_some_and(abycore::Goal::may_start_round);
                        }
                        Err(error) => ctl(CtlEvent::TuiOpFailed(format!(
                            "goal not set: {error} / 目标创建失败：{error}"
                        ))),
                    },
                }
            }
            Cmd::Compact => {
                let Some(agent) = agent.as_mut() else {
                    ctl(CtlEvent::TuiOpFailed(
                        "agent unavailable — /login <apikey> first, or restart abylab".into(),
                    ));
                    continue;
                };
                while interrupt_rx.try_recv().is_ok() {}
                sink(Event::Ui(UiEvent::SessionStatus {
                    session: active_session.clone(),
                    running: true,
                }));
                let result =
                    compact_history(agent, compaction, CompactTrigger::Manual, &mut interrupt_rx)
                        .await;
                sink(Event::Ui(UiEvent::SessionStatus {
                    session: active_session.clone(),
                    running: false,
                }));
                match result {
                    Ok(Some(report)) => {
                        ctl(CtlEvent::TuiOpDone(report.notice(CompactTrigger::Manual)))
                    }
                    Ok(None) => ctl(CtlEvent::TuiOpFailed(
                        "nothing safe to compact yet — the history is still short / 暂无可压缩的历史：会话还很短"
                            .into(),
                    )),
                    Err(err) => ctl(CtlEvent::TuiOpFailed(format!(
                        "compaction failed: {err} / 压缩失败：{err}"
                    ))),
                }
            }
            Cmd::FetchCatalog => {
                // The handle routes this to the query task so a running turn
                // can't hold the `/model` listing back; this arm serves a
                // caller that pokes the command channel directly.
                spawn_catalog_fetch(
                    current_key(&live_key),
                    cfg.base_url.clone(),
                    Arc::clone(&sink),
                );
            }
            Cmd::Shutdown => break,
            Cmd::SetModel {
                model: next_model,
                effort: next_effort,
            } => {
                if agent
                    .as_ref()
                    .is_some_and(|agent| !agent.snapshot().items.is_empty())
                    || (agent.is_none() && resume_target.is_some())
                {
                    ctl(CtlEvent::TuiOpFailed(
                        "model is fixed for a live session — /new starts a fresh one".into(),
                    ));
                    continue;
                }
                let next_model = next_model.unwrap_or_else(|| model.clone());
                let next_effort = next_effort.as_deref().map(parse_effort).unwrap_or(effort);
                if let (Some(current), Some(local)) = (agent.as_ref(), local.as_ref()) {
                    let mut snapshot = current.snapshot();
                    snapshot.model.model = next_model.clone();
                    snapshot.model.reasoning = next_effort;
                    let ctx = AgentContext {
                        local,
                        api_key: &api_key,
                        base_url: cfg.base_url.as_deref(),
                        store: store.as_ref(),
                        session_id: &active_session,
                        subagents: subagents.as_ref(),
                        host: &host,
                    };
                    match build_agent(&ctx, snapshot, current.persist.clone()).and_then(|next| {
                        next.save()?;
                        Ok(next)
                    }) {
                        Ok(next) => agent = Some(next),
                        Err(error) => {
                            ctl(CtlEvent::TuiOpFailed(format!(
                                "model switch failed: {error}"
                            )));
                            continue;
                        }
                    }
                }
                model = next_model;
                effort = next_effort;
                if let Some(current) = &agent {
                    ctl(CtlEvent::TuiOpDone(format!(
                        "model → {model} · effort {}",
                        effort_label(effort)
                    )));
                    bind_session(current, &active_session, None, false, &sink);
                }
            }
            Cmd::NewSession { session_id: id } | Cmd::Resume { session_id: id } => {
                // A same-session restore keeps the existing owner and its lock.
                if restoring
                    && id == active_session
                    && let Some(current) = agent.as_ref()
                {
                    bind_session(current, &id, None, false, &sink);
                    continue;
                }
                let result = (|| -> abycore::Result<SessionAgent> {
                    let local = local.as_ref().ok_or_else(|| {
                        abycore::Error::new(ErrorKind::Configuration, "local tools unavailable")
                    })?;
                    let ctx = AgentContext {
                        local,
                        api_key: &api_key,
                        base_url: cfg.base_url.as_deref(),
                        store: store.as_ref(),
                        session_id: &id,
                        subagents: subagents.as_ref(),
                        host: &host,
                    };
                    if restoring {
                        resume_agent(&ctx)
                    } else {
                        fresh_agent(&ctx, &model, effort)
                    }
                })();
                match result {
                    Ok(next) => {
                        let notice = if restoring {
                            format!("resumed · {} turns", next.snapshot().run_sequence)
                        } else {
                            "new session · abycore".into()
                        };
                        agent = Some(next);
                        active_session = id;
                        resume_target = restoring.then(|| active_session.clone());
                        goal_armed = false;
                        bind_session(
                            agent.as_ref().unwrap(),
                            &active_session,
                            Some(notice),
                            restoring,
                            &sink,
                        );
                        emit_permission_facts(&sink, &active_session, permission_mode);
                    }
                    Err(error) => ctl(CtlEvent::SessionSwitchFailed(format!(
                        "session switch failed: {error}"
                    ))),
                }
            }
            Cmd::ListSessions { prefix } => {
                // The handle routes this to the query task so a running turn
                // can't hold the picker; this arm serves a caller that pokes
                // the command channel directly.
                ctl(CtlEvent::SessionList {
                    sessions: session_rows(store.as_ref()),
                    prefix,
                });
            }
            Cmd::SetPermission { preset } => {
                let Some(next_mode) = parse_permission_mode(&preset) else {
                    ctl(CtlEvent::TuiOpFailed(format!(
                        "unknown permission preset: {preset}"
                    )));
                    continue;
                };
                if next_mode == permission_mode {
                    // Facts only, no op-done echo: a frontend confirms a mode
                    // with its own chip, and the echo would land in the
                    // transcript as a line of its own.
                    emit_permission_facts(&sink, &active_session, permission_mode);
                    continue;
                }

                let next_local = match build_local(&cfg.workspace, next_mode) {
                    Ok(local) => local,
                    Err(err) => {
                        ctl(CtlEvent::TuiOpFailed(format!(
                            "permission switch failed: {err}"
                        )));
                        continue;
                    }
                };
                if let Some(current) = agent.as_mut() {
                    if let Err(error) = next_local.replace(&mut current.inner) {
                        ctl(CtlEvent::TuiOpFailed(format!(
                            "permission switch failed: {error}"
                        )));
                        continue;
                    }
                    current.set_policy(next_mode, &host);
                }
                // The parent keeps its identity and writer. Children retain
                // their launch-time tools and authorization policy.
                let old_local = local.replace(next_local);
                if let Some(old_local) = old_local {
                    old_local.shutdown().await;
                }
                permission_mode = next_mode;
                // Silent success, same reason as the equal-preset branch above;
                // a refused switch still lands as `TuiOpFailed`.
                emit_permission_facts(&sink, &active_session, permission_mode);
            }
            Cmd::SetApiKey { key } => {
                api_key = key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .unwrap_or_default()
                    .to_string();
                // The query task fetches catalogs with whatever key is live.
                *live_key.lock().expect("key lock") = api_key.clone();
                if api_key.is_empty() {
                    // `logout`: abycore refuses a keyless client config, so a
                    // running agent keeps its old key until /new or restart;
                    // the durable store is already cleared by the TUI.
                    ctl(CtlEvent::TuiOpDone(
                        "api key cleared — the running session keeps the old key until /new or restart"
                            .into(),
                    ));
                    continue;
                }
                // Rotate credentials in place; existing child clients remain
                // pinned to the delegation they were launched with.
                if let Some(current) = agent.as_mut() {
                    let rotate = (|| -> abycore::Result<()> {
                        let mut config = ClientConfig::new(&api_key);
                        if let Some(url) = &cfg.base_url {
                            config.base_url = url.clone();
                        }
                        let client = DeepSeekClient::new(config)?;
                        let search = DeepSeekWebSearch::new(SearchConfig::new(&api_key))?;
                        current.replace_tools([Arc::new(search) as Arc<dyn abycore::Tool>])?;
                        current.set_client(client);
                        Ok(())
                    })();
                    if let Err(error) = rotate {
                        ctl(CtlEvent::TuiOpFailed(format!(
                            "api key update failed: {error}"
                        )));
                        continue;
                    }
                } else if let Some(local) = local.as_ref() {
                    let ctx = AgentContext {
                        local,
                        api_key: &api_key,
                        base_url: cfg.base_url.as_deref(),
                        store: store.as_ref(),
                        session_id: &active_session,
                        subagents: subagents.as_ref(),
                        host: &host,
                    };
                    let result = if resume_target.is_some() {
                        resume_agent(&ctx)
                    } else {
                        fresh_agent(&ctx, &model, effort)
                    };
                    match result {
                        Ok(next) => {
                            agent = Some(next);
                            bind_session(
                                agent.as_ref().unwrap(),
                                &active_session,
                                Some("api key set — session ready".into()),
                                resume_target.is_some(),
                                &sink,
                            );
                            emit_permission_facts(&sink, &active_session, permission_mode);
                        }
                        Err(error) => {
                            ctl(CtlEvent::TuiOpFailed(format!(
                                "session open failed: {error}"
                            )));
                            continue;
                        }
                    }
                }
                if agent.is_some() {
                    ctl(CtlEvent::TuiOpDone(
                        "api key set — applies to the next request".into(),
                    ));
                } else {
                    ctl(CtlEvent::TuiOpFailed(
                        "api key set, but the agent could not be built".into(),
                    ));
                }
            }
            Cmd::Prompt { text } => {
                let Some(agent) = agent.as_mut() else {
                    ctl(CtlEvent::Error(
                        "agent unavailable — /login <apikey> first, or restart abylab".into(),
                    ));
                    continue;
                };
                // `/name [args]` naming a skill becomes that skill's body: the
                // host injects it here, at the one seam every prompt crosses
                // (typed, queued, steered). Any other line ships unchanged.
                let text = skills.expand(&text).unwrap_or(text);
                let message_id = format!("aby-{}", agent.snapshot().run_sequence + 1);
                ctl(CtlEvent::PromptQueued { message_id });
                let mut ctx = TurnCtx {
                    session: &active_session,
                    sink: &sink,
                    interrupt_rx: &mut interrupt_rx,
                    limits,
                    compaction,
                };
                // Context pressure is handled by the agent itself at every
                // request boundary (`AgentHooks::view_request`), so it applies
                // inside a turn too; nothing to do before shipping the prompt.
                // An armed goal can continue immediately after this prompt.
                // Keep the UI busy until those rounds settle, so its queued
                // prompts cannot be dispatched between the two turns.
                let goal_continues = goal_armed;
                match turn(agent, Some(text), &mut ctx, !goal_continues).await {
                    Ok(outcome)
                        if goal_continues && outcome.stop_reason == StopReason::Completed =>
                    {
                        drive_goal_rounds(agent, &mut ctx, &ctl).await;
                        goal_armed =
                            goal_armed && agent.goal().is_some_and(abycore::Goal::may_start_round);
                    }
                    Ok(_) => {
                        if goal_continues {
                            emit_idle_status(&ctx);
                        }
                    }
                    Err(err) => {
                        if goal_continues {
                            emit_idle_status(&ctx);
                        }
                        report_turn_err(&ctl, &err, limits);
                    }
                }
            }
        }
    }

    if let Some(subagents) = &subagents {
        subagents.shutdown().await;
    }
    if let Some(local) = &local {
        local.shutdown().await;
    }
}

fn report_turn_err(ctl: &impl Fn(CtlEvent), err: &abycore::Error, limits: TurnLimits) {
    if err.kind == ErrorKind::Cancelled {
        ctl(CtlEvent::Interrupted);
    } else {
        ctl(CtlEvent::Error(turn_error_text(err, limits)));
    }
}

/// ErrorKind → the user-facing line: one actionable hint per family,
/// bilingual where the hint matters (mirrors locale.rs's zh-first chrome).
fn turn_error_text(err: &abycore::Error, limits: TurnLimits) -> String {
    let detail = err.message.trim();
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" · {detail}")
    };
    let retry = err
        .retry_after
        .map(|d| {
            format!(
                " · retry in {}s",
                d.as_secs() + u64::from(d.subsec_millis() > 0)
            )
        })
        .unwrap_or_default();
    let text = match err.kind {
        ErrorKind::Authentication => {
            "API key rejected — /login <apikey> re-stores it / 请求被拒，/login <apikey> 重新保存 key"
        }
        ErrorKind::Quota => "quota exhausted — check your plan / 额度已用尽，检查套餐",
        // Continuation headroom is spent, but the turn is still resumable:
        // point at the message that continues it, not just a bare failure.
        ErrorKind::RateLimit => {
            return format!(
                "rate limited{retry} — the turn is unfinished; send a message to continue it / 限流，稍后自动重试{retry}：回合未完成，继续输入可续跑"
            );
        }
        ErrorKind::ContextLimitExceeded => {
            return "context limit reached — /compact condenses the history, /new starts a fresh session / 上下文已达上限：/compact 压缩历史，/new 开新会话".into();
        }
        ErrorKind::Timeout => {
            return format!(
                "timed out (ABY_TURN_TIMEOUT={}, ABY_TOOL_TIMEOUT={} seconds; ABY_AUTO_CONTINUE={}) \
                 — the turn is unfinished; send a message to continue it or adjust these environment variables \
                 / 超时：回合未完成，继续输入可续跑；可调整上述环境变量中的时限和自动续跑次数{detail}",
                limits.run_timeout.as_secs(),
                limits.tool_timeout.as_secs(),
                limits.continuations,
            );
        }
        // The watchdog tripped and auto-continuation already spent its
        // headroom. The cap is abylab's own backstop, not an API limit, so
        // name the knobs that raise or remove it.
        ErrorKind::BudgetExceeded => {
            let requests = limit_label(limits.max_requests);
            let tools = limit_label(limits.max_tool_calls);
            return format!(
                "turn budget reached ({requests} requests / {tools} tool calls per run; \
                 --max-requests/--max-tool-calls, 0 = no cap) — the turn is unfinished: \
                 send a message to continue it{detail} \
                 / 达到回合预算上限（每轮 {requests} 次请求 / {tools} 次工具调用；\
                 可用 --max-requests/--max-tool-calls 调整，0 表示不限），回合未完成：继续输入可续跑"
            );
        }
        ErrorKind::Session => {
            "session state error — the turn is unfinished; retry / 会话状态错误：回合未完成，请重试"
        }
        ErrorKind::NeedsResolution => {
            return "a tool result needs verification — the turn is paused / 工具结果待核实，回合挂起".into();
        }
        ErrorKind::InvalidRequest => "request rejected by the API / 请求被 API 拒绝",
        ErrorKind::Server => {
            "server error — the turn is unfinished; send a message to continue it / 服务端错误：回合未完成，继续输入可续跑"
        }
        ErrorKind::Transport | ErrorKind::StreamClosed => {
            "connection lost — the turn is unfinished; send a message to continue it / 连接中断：回合未完成，继续输入可续跑"
        }
        ErrorKind::EmptyResponse => {
            "the provider returned an empty response — the turn is unfinished; send a message to continue it / 返回了空响应：回合未完成，继续输入可续跑"
        }
        ErrorKind::Protocol => "protocol error — likely a gateway incompatibility / 协议不兼容",
        _ => "turn failed",
    };
    format!("{text}{detail}")
}

fn build_local(workspace: &str, permission_mode: PermissionMode) -> Result<LocalTools, String> {
    let root = std::path::PathBuf::from(workspace);
    std::fs::create_dir_all(&root).map_err(|err| format!("workspace {}: {err}", root.display()))?;
    let mut config = LocalToolConfig::new(&root);
    config.permission_mode = permission_mode;
    LocalTools::with_config(config).map_err(|err| format!("local tools: {err:#}"))
}

fn parse_permission_mode(preset: &str) -> Option<PermissionMode> {
    match preset {
        "read-only" => Some(PermissionMode::ReadOnly),
        "workspace-write" => Some(PermissionMode::WorkspaceWrite),
        "danger-full-access" | "full-access" => Some(PermissionMode::FullAccess),
        _ => None,
    }
}

fn permission_preset(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "read-only",
        PermissionMode::WorkspaceWrite => "workspace-write",
        PermissionMode::FullAccess => "danger-full-access",
    }
}

/// Split `/goal`'s argument into the objective and an optional round
/// allowance: `/goal @3 objective` sets the allowance, a bare `@3` names no
/// objective (the caller routes the literal `"rounds"` to its usage arm), and
/// anything else is the objective as typed.
fn parse_goal_argument(arg: &str) -> (&str, Option<u64>) {
    let Some(rest) = arg.strip_prefix('@') else {
        return (arg, None);
    };
    match rest.split_once(char::is_whitespace) {
        Some((rounds, objective)) => match rounds.parse::<u64>() {
            Ok(rounds) => (objective.trim(), Some(rounds)),
            Err(_) => (arg, None),
        },
        None if rest.trim().parse::<u64>().is_ok() => ("rounds", None),
        None => (arg, None),
    }
}

fn emit_permission_facts(
    sink: &Arc<dyn Fn(Event) + Send + Sync>,
    session: &str,
    mode: PermissionMode,
) {
    let preset = permission_preset(mode).to_string();
    sink(Event::Ui(UiEvent::PermissionPreset {
        session: session.to_string(),
        preset: preset.clone(),
    }));
    sink(Event::Ui(UiEvent::SandboxMode {
        session: session.to_string(),
        mode: preset,
    }));
    sink(Event::Ui(UiEvent::ApprovalPolicy {
        session: session.to_string(),
        policy: if mode == PermissionMode::FullAccess {
            "never"
        } else {
            "ask"
        }
        .into(),
    }));
}

/// Forward the subagent broadcast into the TUI event stream. Child
/// transcripts key on the child's agent id; `Started`/`Finished` drive the
/// rail and lifecycle, `Agent { event }` reuses the transcript translation.
fn spawn_subagent_forwarder(
    subagents: Arc<abycore::Subagents>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    agent_sessions: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
) {
    let mut rx = subagents.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(abycore::SubagentEvent::Started(info)) => {
                    let label = (!info.description.trim().is_empty())
                        .then(|| info.description.trim().to_string());
                    sink(Event::Ui(UiEvent::SubagentStarted {
                        parent: agent_sessions
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .get(&info.parent_id)
                            .cloned()
                            .unwrap_or(info.parent_id.clone()),
                        child: info.id.clone(),
                        label,
                    }));
                }
                Ok(abycore::SubagentEvent::Agent { agent_id, event }) => {
                    for ui in translate(&agent_id, event) {
                        sink(Event::Ui(ui));
                    }
                }
                Ok(abycore::SubagentEvent::Finished(info)) => {
                    sink(Event::Ui(UiEvent::SubagentFinished {
                        child: info.id.clone(),
                    }));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Everything a session agent needs besides its model choice: the local tool
/// bundle, credentials, persistence target and host policy. Bundled so
/// [`fresh_agent`] and [`resume_agent`] stay in lockstep.
struct AgentContext<'a> {
    local: &'a LocalTools,
    api_key: &'a str,
    base_url: Option<&'a str>,
    store: Option<&'a abycore::SessionStore>,
    session_id: &'a str,
    subagents: Option<&'a Arc<abycore::Subagents>>,
    host: &'a HostPolicy,
}

/// A fresh session reserves its id before it is published to the UI.
fn fresh_agent(
    ctx: &AgentContext<'_>,
    model: &str,
    effort: ReasoningEffort,
) -> abycore::Result<SessionAgent> {
    let snapshot = abycore::SessionSnapshot::new(
        "You are abylab, a coding agent in the user's terminal. Keep answers tight; use the provided tools to read, write, edit and run things in the workspace. Plan multi-step work with todo_write and keep the list current.",
        ModelOptions {
            model: model.to_string(),
            reasoning: effort,
            max_tokens: ctx
                .host
                .max_tokens
                .unwrap_or_else(|| ModelOptions::default().max_tokens),
            ..ModelOptions::default()
        },
    );
    let persist = ctx
        .store
        .map(|store| {
            store
                .create_new(ctx.session_id, &snapshot)
                .map(|writer| Arc::new(PersistState::with_writer(store, writer)))
        })
        .transpose()?;
    build_agent(ctx, snapshot, persist)
}

/// The snapshot and writer come from one locked read; no pre-lock state is used.
fn resume_agent(ctx: &AgentContext<'_>) -> abycore::Result<SessionAgent> {
    let store = ctx
        .store
        .ok_or_else(|| abycore::Error::new(ErrorKind::Session, "session store unavailable"))?;
    let (writer, snapshot) = store.open_for_resume(ctx.session_id)?;
    let persist = Arc::new(PersistState::with_writer(store, writer));
    build_agent(ctx, snapshot, Some(persist))
}

fn build_agent(
    ctx: &AgentContext<'_>,
    mut snapshot: abycore::SessionSnapshot,
    persist: Option<Arc<PersistState>>,
) -> abycore::Result<SessionAgent> {
    let mut config = ClientConfig::new(ctx.api_key);
    if let Some(url) = ctx.base_url {
        config.base_url = url.to_string();
    }
    let client = DeepSeekClient::new(config)?;
    if let Some(max_tokens) = ctx.host.max_tokens {
        snapshot.model.max_tokens = max_tokens;
    }
    let mut inner = Agent::restore(client, snapshot)?;
    ctx.local.register(&mut inner)?;
    inner.register_tool(TodoWriteTool::new(false))?;
    inner.register_tool(abycore::GetGoalTool)?;
    inner.register_tool(abycore::CreateGoalTool)?;
    inner.register_tool(abycore::UpdateGoalTool)?;
    if let Some(tool) = skill_tool(ctx.host) {
        inner.register_tool(tool)?;
    }
    if let Some(subagents) = ctx.subagents {
        subagents.register(&mut inner)?;
    }
    inner.register_tool(DeepSeekWebSearch::new(SearchConfig::new(ctx.api_key))?)?;
    let mut agent = SessionAgent { inner, persist };
    agent.set_policy(ctx.local.permission_mode(), ctx.host);
    ctx.host
        .agent_sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(agent.id().to_string(), ctx.session_id.to_string());
    Ok(agent)
}

fn bind_session(
    agent: &SessionAgent,
    session: &str,
    notice: Option<String>,
    replay: bool,
    sink: &Arc<dyn Fn(Event) + Send + Sync>,
) {
    let snapshot = agent.snapshot();
    sink(Event::Ctl(CtlEvent::SessionBound {
        session_id: session.into(),
        notice,
        model: Some(snapshot.model.model.clone()),
        effort: Some(effort_label(snapshot.model.reasoning).into()),
    }));
    if replay {
        for event in replay_events(session, &snapshot) {
            sink(Event::Ui(event));
        }
    }
}

/// Rebuild transcript-side UiEvents from a restored snapshot so the TUI can
/// render the conversation history. Reasoning text is skipped (the renderer
/// tracks live streaming state for it).
fn replay_events(session: &str, snapshot: &abycore::SessionSnapshot) -> Vec<UiEvent> {
    let mut out = Vec::new();
    for item in &snapshot.items {
        match item {
            abycore::Item::Message {
                role: abycore::MessageRole::User,
                content,
                ..
            } => {
                let text = content
                    .iter()
                    .map(|part| part.text().to_string())
                    .collect::<Vec<_>>()
                    .join("");
                out.push(UiEvent::UserMessage {
                    session: session.to_string(),
                    text,
                });
            }
            abycore::Item::Message {
                role: abycore::MessageRole::Assistant,
                content,
                ..
            } => {
                let text = content
                    .iter()
                    .map(|part| part.text().to_string())
                    .collect::<Vec<_>>()
                    .join("");
                out.push(UiEvent::AssistantFinal {
                    session: session.to_string(),
                    text,
                    model: Some(snapshot.model.model.clone()),
                });
            }
            abycore::Item::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => out.push(UiEvent::ToolCall {
                session: session.to_string(),
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            abycore::Item::FunctionCallOutput {
                call_id,
                output,
                is_error,
                ..
            } => {
                out.push(UiEvent::ToolResult {
                    session: session.to_string(),
                    call_id: call_id.clone(),
                    is_error: *is_error,
                    text: output.clone(),
                    error: None,
                });
            }
            abycore::Item::Reasoning { .. } => {}
        }
    }
    // The snapshot carries the current turn's checklist; repainting it after
    // the replayed history restores the transcript's plan cell and the
    // composer's todo line in place.
    if let Some(plan) = snapshot.plan_view() {
        out.push(plan_event(session, Some(&plan)));
    }
    out
}

fn parse_effort(raw: &str) -> ReasoningEffort {
    match raw.to_ascii_lowercase().as_str() {
        "off" | "none" => ReasoningEffort::Off,
        "low" => ReasoningEffort::Low,
        "max" => ReasoningEffort::Max,
        _ => ReasoningEffort::High,
    }
}

fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Off => "off",
        ReasoningEffort::Low => "low",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "max",
    }
}

/// Supply explicit error outputs for any unresolved tool calls, in transcript
/// order, so the interrupted turn can be continued or settled. `reason` is the
/// model-visible explanation for calls that never started. A call whose
/// execution was interrupted may already have side effects, so its output
/// always asks the model to verify the result before repeating it.
///
/// `resolve_tool` only removes the first pending call on success, so ignoring
/// its `Result` would spin on the same call forever; a refusal ends the turn.
fn settle_pending(agent: &mut Agent, reason: &str, ctx: &TurnCtx<'_>) -> abycore::Result<()> {
    while let Some(call) = agent.snapshot().pending.first().cloned() {
        let reason = match call.state {
            abycore::PendingState::Unknown => {
                "previous tool call was interrupted; its result is unverified — check before repeating"
            }
            abycore::PendingState::Ready => reason,
        };
        agent.resolve_tool(
            &call.call_id,
            ToolOutput {
                content: reason.into(),
                is_error: true,
                truncated: false,
                details: None,
                meta: None,
            },
        )?;
        (ctx.sink)(Event::Ui(UiEvent::ToolResult {
            session: ctx.session.into(),
            call_id: call.call_id,
            is_error: true,
            text: reason.into(),
            error: None,
        }));
    }
    Ok(())
}

/// Spend rounds on an armed, active goal.
///
/// Harness's `goal-round-driver`, as host policy: each round is one ordinary
/// turn seeded with the objective, the goal's own allowance bounds the loop,
/// and exhaustion records a blocker instead of looping forever. The model ends
/// the loop by completing or blocking the goal through `update_goal`; an error
/// or an interrupt stops it too.
async fn drive_goal_rounds(
    agent: &mut SessionAgent,
    ctx: &mut TurnCtx<'_>,
    ctl: &impl Fn(CtlEvent),
) {
    run_goal_rounds(agent, ctx, ctl).await;
    if let Err(error) = agent.save() {
        ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
        emit_idle_status(ctx);
        return;
    }
    // One settled state after the loop, whatever stopped it: the transcript's
    // last word on the goal is always its current status.
    if let Some(goal) = agent.goal() {
        ctl(CtlEvent::TuiOpDone(format!(
            "goal settled · {} / 目标状态 · {}",
            goal.summary(),
            goal.summary()
        )));
    }
    // The rounds suppressed their per-turn idle status; emit it once after
    // the sequence and final save, even if no round was needed.
    emit_idle_status(ctx);
}

fn emit_idle_status(ctx: &TurnCtx<'_>) {
    (ctx.sink)(Event::Ui(UiEvent::SessionStatus {
        session: ctx.session.into(),
        running: false,
    }));
}

async fn run_goal_rounds(agent: &mut SessionAgent, ctx: &mut TurnCtx<'_>, ctl: &impl Fn(CtlEvent)) {
    loop {
        let Some(goal) = agent.goal() else {
            return;
        };
        if goal.status != abycore::GoalStatus::Active {
            return;
        }
        if goal.remaining_rounds() == 0 {
            let _ = agent.update_goal(
                abycore::GoalStatus::Blocked,
                Some("round allowance exhausted".into()),
            );
            if let Err(error) = agent.save() {
                ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
                return;
            }
            ctl(CtlEvent::TuiOpFailed(
                "goal rounds exhausted — marked blocked / 目标轮次用尽，已标记 blocked".into(),
            ));
            return;
        }
        let Some(goal) = agent.begin_goal_round().ok().flatten() else {
            return;
        };
        if let Err(error) = agent.save() {
            ctl(CtlEvent::TuiOpFailed(format!("goal save failed: {error}")));
            return;
        }
        ctl(CtlEvent::TuiOpDone(format!(
            "goal round {}/{} — {} / 目标第 {}/{} 轮",
            goal.rounds_started,
            goal.max_rounds,
            goal.objective,
            goal.rounds_started,
            goal.max_rounds
        )));
        let prompt = format!(
            "Continue working toward the session goal (round {}/{}): {}\n             When the objective is achieved, call update_goal with status \"complete\".              If progress is impossible, call update_goal with status \"blocked\" and explain in note.              Otherwise keep working; do not restate the goal, just make progress.",
            goal.rounds_started, goal.max_rounds, goal.objective
        );
        match turn(agent, Some(prompt), ctx, false).await {
            Ok(outcome) if outcome.stop_reason == StopReason::Completed => {}
            Ok(_) => return,
            Err(err) => {
                report_turn_err(ctl, &err, ctx.limits);
                return;
            }
        }
    }
}

/// Why a *forced* compaction is running. Pressure-driven condensation happens
/// inside the agent at request boundaries and never reaches this path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompactTrigger {
    /// The user asked (`/compact`).
    Manual,
    /// A request was refused for context length: reduce hard, then resume.
    Overflow,
}

/// What one compaction freed, for the transcript notice.
struct CompactionReport {
    /// Transcript items replaced by the summary (0 when only outputs were trimmed).
    items: usize,
    /// Whether a summary call actually ran.
    summarized: bool,
    before_bytes: usize,
    after_bytes: usize,
}

impl CompactionReport {
    fn notice(&self, trigger: CompactTrigger) -> String {
        let freed = self.before_bytes.saturating_sub(self.after_bytes);
        match (trigger, self.summarized) {
            (CompactTrigger::Manual, true) => format!(
                "compacted {} history items — {} bytes freed / 已压缩 {} 条历史，释放 {} 字节",
                self.items, freed, self.items, freed
            ),
            (CompactTrigger::Overflow, _) => format!(
                "context overflow — condensed {} history items and retrying ({} bytes freed) / 上下文超限：已压缩 {} 条历史并重试（释放 {} 字节）",
                self.items, freed, self.items, freed
            ),
            (CompactTrigger::Manual, false) => format!(
                "trimmed old tool outputs — {} bytes freed; the history was small enough to keep / 已裁剪旧工具输出，释放 {} 字节；历史较短无需摘要",
                freed, freed
            ),
        }
    }
}

/// Condense history when the request estimate reaches the policy threshold, on
/// demand (`/compact`), or after a context-overflow refusal.
///
/// Region policy mirrors `dsh-compaction-basic`: trim old tool outputs first,
/// replace the *oldest* span with a summary, keep the newest `keep_recent`
/// fraction of the window verbatim, and only cut where no tool call is
/// separated from its result. Overflow bypasses the retention policy and keeps
/// just the current turn. The raw transcript is never rewritten —
/// `Agent::compact`/`Agent::prune_outputs` record view-only replacements.
async fn compact_history(
    agent: &mut SessionAgent,
    policy: Option<CompactionConfig>,
    trigger: CompactTrigger,
    interrupt_rx: &mut mpsc::UnboundedReceiver<()>,
) -> abycore::Result<Option<CompactionReport>> {
    let cancellation = CancellationToken::new();
    let outcome = {
        let work = compact_view(agent, policy, trigger, cancellation.clone());
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = interrupt_rx.recv() => {
                cancellation.cancel();
                work.await
            }
            outcome = &mut work => outcome,
        }
    };
    // Save both successful view changes and the ledger/prune left on failure.
    agent.save()?;
    outcome
}

async fn compact_view(
    agent: &mut Agent,
    policy: Option<CompactionConfig>,
    trigger: CompactTrigger,
    cancellation: CancellationToken,
) -> abycore::Result<Option<CompactionReport>> {
    let before = agent.view_estimate()?;
    let items = agent.snapshot().items;
    if items.is_empty() {
        return Ok(None);
    }
    let cuts: Vec<_> = before.cuts.iter().map(|cut| cut.index).collect();
    let mut suffix = vec![0usize; items.len() + 1];
    for cut in &before.cuts {
        suffix[cut.index] = cut.suffix_bytes;
    }
    // The retention budget converts to bytes at the measured ratio when one is
    // available, so the trigger and the retention speak the same unit.
    let bytes_per_token = before.bytes_per_token() as u64;
    let end = match (policy, trigger) {
        // One maximal head reduction: keep only the current turn, whatever the
        // retention policy says.
        (_, CompactTrigger::Overflow) => {
            last_turn_start(&items, &cuts).or_else(|| retained_cut(&cuts, &suffix, 0))
        }
        (Some(policy), CompactTrigger::Manual) => retained_cut(
            &cuts,
            &suffix,
            policy.retained_tokens().saturating_mul(bytes_per_token) as usize,
        ),
        // `/compact` without a configured window: keep the newest quarter.
        (None, CompactTrigger::Manual) => retained_cut(&cuts, &suffix, before.history_bytes / 4),
    };
    let Some(end) = end.filter(|end| *end > 0 && *end <= items.len()) else {
        return Ok(None);
    };

    // Trim old tool outputs before paying for a summary; harness's pruner runs
    // in exactly this position. Forced paths still summarize: the caller asked
    // for a reduction, or the provider already refused the request.
    if let Some(policy) = policy
        && policy.prune_tool_bytes > 0
        && !agent.prune().is_some_and(|prune| prune.through >= end)
    {
        agent.prune_outputs(end, policy.prune_tool_bytes)?;
    }

    let options = abycore::SummarizeOptions {
        max_tokens: Some(
            policy
                .map(|policy| policy.max_tokens)
                .unwrap_or_else(|| CompactionConfig::default().max_tokens),
        ),
        // Forced compaction is an explicit recovery operation with its own
        // deadline and the driver's active interrupt signal.
        cancellation,
        timeout: Duration::from_secs(300),
        ..Default::default()
    };
    let summary = agent.summarize_span(0, end, options).await?.summary;
    agent.compact(0, end, summary)?;
    let after = agent.context_estimate()?;
    Ok(Some(CompactionReport {
        items: end,
        summarized: true,
        before_bytes: before.request_bytes,
        after_bytes: after.request_bytes,
    }))
}

/// The transcript line for a policy-driven view change (harness's pruner and
/// automatic condensation).
fn view_change_notice(change: &abycore::ViewChange) -> String {
    let freed = |before: usize, after: usize| before.saturating_sub(after);
    match change {
        abycore::ViewChange::Pruned {
            before_bytes,
            after_bytes,
            ..
        } => format!(
            "context near the window — trimmed old tool outputs ({} bytes freed, no summary needed) / 上下文接近上限：已裁剪旧工具输出（释放 {} 字节，无需摘要）",
            freed(*before_bytes, *after_bytes),
            freed(*before_bytes, *after_bytes)
        ),
        abycore::ViewChange::Compacted {
            start,
            end,
            before_bytes,
            after_bytes,
        } => format!(
            "context near the window — compacted {} history items into a summary ({} bytes freed) / 上下文接近上限：已把 {} 条历史压缩为摘要（释放 {} 字节）",
            end - start,
            freed(*before_bytes, *after_bytes),
            end - start,
            freed(*before_bytes, *after_bytes)
        ),
        abycore::ViewChange::Failed { message } => {
            format!("compaction failed: {message} / 压缩失败：{message}")
        }
    }
}

/// The newest cut that leaves at least `keep_bytes` of history verbatim.
fn retained_cut(cuts: &[usize], suffix: &[usize], keep_bytes: usize) -> Option<usize> {
    cuts.iter()
        .copied()
        .filter(|cut| *cut > 0 && suffix[*cut] >= keep_bytes)
        .max()
}

/// The cut at the start of the newest user turn: everything older is condensed,
/// so an over-window request keeps its current instruction and tool results.
fn last_turn_start(items: &[abycore::Item], cuts: &[usize]) -> Option<usize> {
    let last_user = items.iter().rposition(|item| {
        matches!(
            item,
            abycore::Item::Message {
                role: abycore::MessageRole::User,
                ..
            }
        )
    })?;
    (last_user > 0 && cuts.contains(&last_user)).then_some(last_user)
}

/// What one turn needs besides the agent: the session label, the event sink,
/// the interrupt channel and the budgets. Grouped so the segment helpers do
/// not grow one parameter per turn feature.
struct TurnCtx<'a> {
    session: &'a str,
    sink: &'a Arc<dyn Fn(Event) + Send + Sync>,
    interrupt_rx: &'a mut mpsc::UnboundedReceiver<()>,
    limits: TurnLimits,
    /// Host compaction policy for overflow recovery inside a turn.
    compaction: Option<CompactionConfig>,
}

/// One agent segment racing the interrupt channel. New input is included in
/// the next request even when the session still has an unfinished turn.
///
/// Cancellation races the run future; the losing branch still drains the
/// outcome so history stays consistent.
async fn run_segment(
    agent: &mut Agent,
    text: Option<String>,
    ctx: &mut TurnCtx<'_>,
) -> abycore::Result<RunOutcome> {
    let cancellation = CancellationToken::new();
    let options = RunOptions {
        cancellation: cancellation.clone(),
        max_requests: ctx.limits.max_requests,
        max_tool_calls: ctx.limits.max_tool_calls,
        timeout: ctx.limits.run_timeout,
        tool_timeout: ctx.limits.tool_timeout,
        ..RunOptions::default()
    };
    let (sess, sink_for_events) = (ctx.session.to_string(), Arc::clone(ctx.sink));
    let on_event = move |event: AgentEvent| {
        let sink = Arc::clone(&sink_for_events);
        let sess = sess.clone();
        async move {
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
    > = match text {
        Some(text) if agent.snapshot().needs_response => {
            Box::pin(agent.continue_run_with_input(text, options, on_event))
        }
        Some(text) => Box::pin(agent.run(text, options, on_event)),
        None => Box::pin(agent.continue_run(options, on_event)),
    };
    tokio::select! {
        result = &mut run_fut => result,
        _ = ctx.interrupt_rx.recv() => {
            cancellation.cancel();
            (&mut run_fut).await
        }
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
async fn turn(
    agent: &mut SessionAgent,
    text: Option<String>,
    ctx: &mut TurnCtx<'_>,
    settle_status: bool,
) -> abycore::Result<RunOutcome> {
    if text.is_some() && agent.snapshot().needs_response {
        settle_pending(
            agent,
            "previous tool call was interrupted; its result is unverified — check before repeating",
            ctx,
        )?;
    }
    let session = ctx.session.to_string();
    let sink = Arc::clone(ctx.sink);
    let emit = |event: Event| sink(event);
    let emit_ctl = |event: CtlEvent| sink(Event::Ctl(event));
    let turn = agent.snapshot().run_sequence + 1;
    emit(Event::Ui(UiEvent::TurnStart {
        session: session.clone(),
        turn,
    }));
    emit(Event::Ui(UiEvent::SessionStatus {
        session: session.clone(),
        running: true,
    }));

    // Stale interrupts (sent between turns, e.g. by the steer path) must not
    // cancel this turn — but an interrupt that arrives while a later segment
    // runs must still land, so drain only once here.
    while ctx.interrupt_rx.try_recv().is_ok() {}

    let mut next = text;
    let mut continuations = 0usize;
    let outcome = loop {
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
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ctx.interrupt_rx.recv() => {
                    break Err(abycore::Error::new(ErrorKind::Cancelled, "operation cancelled"));
                }
            }
        }
        continuations += 1;
        emit(Event::Ctl(CtlEvent::TuiOpDone(continuation_notice(
            failure,
            continuations,
            ctx.limits.continuations,
        ))));
    };

    // Save final state and the ledger on every exit, including cancellation
    // while waiting between segments or an incomplete model response.
    let outcome = agent.save().and(outcome);
    let kind = match &outcome {
        Ok(outcome) => stop_kind(&outcome.stop_reason),
        Err(err) if err.kind == ErrorKind::Cancelled => "interrupted",
        Err(_) => "error",
    };
    if matches!(&outcome, Ok(outcome) if outcome.stop_reason == StopReason::Incomplete) {
        let max_tokens = agent.snapshot().model.max_tokens;
        emit_ctl(CtlEvent::TuiOpDone(format!(
            "response reached its output limit (max_tokens={max_tokens}); send a shorter instruction or resume with a larger --max-tokens / 回答达到输出上限；可继续输入缩短回答的指令，或提高 --max-tokens 后恢复会话"
        )));
    }
    emit(Event::Ui(UiEvent::TurnEnd {
        session: session.clone(),
        kind: kind.into(),
    }));
    if let Ok(outcome) = &outcome {
        let usage = outcome.response.usage.as_ref();
        emit(Event::Ui(UiEvent::Usage {
            session: session.clone(),
            input: usage.and_then(|u| u.input_tokens).unwrap_or(0),
            output: usage.and_then(|u| u.output_tokens).unwrap_or(0),
            cached: usage.and_then(|u| u.cached_tokens).unwrap_or(0),
            reasoning: usage.and_then(|u| u.reasoning_tokens).unwrap_or(0),
        }));
    }
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
/// `TRANSPORT`, `RATE_LIMIT`, `SERVER`, `EMPTY_RESPONSE`). A timeout, including
/// the whole-segment deadline, also leaves the turn resumable: a long task can
/// need a fresh segment even when every individual request/tool made progress.
/// Timeout continuations share the same finite headroom as all other retries.
/// `BudgetExceeded` is abylab's own watchdog.
fn resumable_failure(err: &abycore::Error) -> bool {
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
fn continuation_delay(err: &abycore::Error) -> Option<Duration> {
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
fn continuation_notice(err: &abycore::Error, round: usize, rounds: usize) -> String {
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
fn limit_label(value: usize) -> String {
    if value == TurnLimits::UNLIMITED {
        "unlimited".into()
    } else {
        value.to_string()
    }
}

fn stop_kind(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Completed => "completed",
        StopReason::Incomplete => "incomplete",
        StopReason::Failed => "failed",
        StopReason::Error(kind) if kind == &ErrorKind::Cancelled => "interrupted",
        StopReason::Error(_) => "error",
    }
}

/// AgentEvent → transcript-ready UiEvents.
fn translate(session: &str, event: AgentEvent) -> Vec<UiEvent> {
    let session = session.to_string();
    match event {
        AgentEvent::RunStarted { .. } => Vec::new(),
        AgentEvent::Model(abycore::StreamEvent::TextDelta { delta, .. }) => {
            vec![UiEvent::TextDelta {
                session,
                text: delta,
            }]
        }
        AgentEvent::Model(abycore::StreamEvent::ReasoningDelta { delta, .. }) => {
            vec![UiEvent::ReasoningDelta {
                session,
                text: delta,
            }]
        }
        AgentEvent::Model(abycore::StreamEvent::Finished { response, .. }) => {
            vec![UiEvent::AssistantFinal {
                session,
                text: String::new(),
                model: Some(response.model),
            }]
        }
        // The call's card is fed while the model is still writing the arguments,
        // so a live card shows the command instead of a bare tool name.
        AgentEvent::Model(abycore::StreamEvent::ToolArgumentsDelta {
            call_id,
            name,
            delta,
            ..
        }) => {
            if delta.is_empty() {
                Vec::new()
            } else {
                vec![UiEvent::ToolCallDelta {
                    session,
                    call_id,
                    name,
                    delta,
                }]
            }
        }
        // Authoritative arguments once the call's content block closes (also the
        // only signal for a tool call that streams no argument deltas).
        AgentEvent::Model(abycore::StreamEvent::ItemDone {
            item:
                abycore::Item::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                },
            ..
        }) => vec![UiEvent::ToolCall {
            session,
            call_id,
            name,
            arguments,
        }],
        AgentEvent::ToolStarted { call_id, name } => vec![UiEvent::ToolStarted {
            session,
            call_id,
            name,
        }],
        AgentEvent::ToolFinished {
            call_id,
            output: ToolOutput {
                content, is_error, ..
            },
        } => vec![UiEvent::ToolResult {
            session,
            call_id,
            is_error,
            text: content,
            error: None,
        }],
        AgentEvent::PlanChanged { plan } => {
            vec![plan_event(&session, plan.as_ref())]
        }
        AgentEvent::ViewChanged { .. } | AgentEvent::GoalChanged { .. } => Vec::new(),
        AgentEvent::Model(_) | AgentEvent::RunFinished { .. } => Vec::new(),
    }
}

/// One plan snapshot for every plan surface: the transcript's digest cell, the
/// composer's live todo line, and the clickable progress dialog. An empty list
/// is an explicit clear — abycore documents that no surface should show a task
/// panel then — so it folds into the same blank event as `None`.
fn plan_event(session: &str, plan: Option<&abycore::PlanView>) -> UiEvent {
    let plan = plan.filter(|plan| plan.total > 0);
    UiEvent::Plan {
        session: session.to_string(),
        summary: plan.map(plan_summary).unwrap_or_default(),
        todos: plan.map_or_else(Vec::new, |plan| {
            plan.todos
                .iter()
                .map(|todo| PlanItem {
                    content: todo.content.clone(),
                    status: match todo.status {
                        abycore::TodoStatus::Pending => PlanStatus::Pending,
                        abycore::TodoStatus::InProgress => PlanStatus::InProgress,
                        abycore::TodoStatus::Completed => PlanStatus::Completed,
                    },
                })
                .collect()
        }),
        active: plan.and_then(|plan| plan.active_content.clone()),
        active_extra: plan.map_or(0, |plan| plan.active_extra),
        completed: plan.map_or(0, |plan| plan.counts.completed),
        total: plan.map_or(0, |plan| plan.total),
    }
}

/// One-line plan digest for the transcript's plan cell. Only called with a
/// non-empty list, so an all-zero digest never reaches a surface.
fn plan_summary(plan: &abycore::PlanView) -> String {
    let mut parts = vec![format!("{} of {} done", plan.counts.completed, plan.total)];
    if let Some(active) = &plan.active_content {
        let mut now = format!("now: {active}");
        if plan.active_extra > 0 {
            now.push_str(&format!(" (+{})", plan.active_extra));
        }
        parts.push(now);
    }
    parts.push(format!("{} pending", plan.counts.pending));
    parts.join(" · ")
}

/// The key the driver is currently using; the query task reads it so a
/// mid-session `/login` reaches the catalog fetch too.
fn current_key(live: &std::sync::Mutex<String>) -> String {
    live.lock().expect("key lock").clone()
}

/// Best-effort provider listing for the `/model` picker. The fetch rides its
/// own task so a slow catalog never delays anything else; failures leave the
/// picker on its stock presets. Both the turn loop and the query task call it,
/// so a catalog asked for mid-turn answers without waiting for that turn.
fn spawn_catalog_fetch(
    api_key: String,
    base_url: Option<String>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
) {
    if api_key.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut config = ClientConfig::new(&api_key);
        if let Some(url) = base_url {
            config.base_url = url;
        }
        let Ok(client) = DeepSeekClient::new(config) else {
            return;
        };
        let Ok(models) = client.models().await else {
            return;
        };
        let models = models
            .into_iter()
            .map(|model| crate::contract::CatalogModel {
                provider: "deepseek-official".into(),
                name: model.id.clone(),
                id: model.id,
                vision: false,
            })
            .collect();
        sink(Event::Ctl(CtlEvent::Catalog { models }));
    });
}

/// The model-facing skill reader, or `None` when the session has no skills —
/// an empty catalog would only spend tool-definition tokens.
fn skill_tool(host: &HostPolicy) -> Option<abycore::SkillTool> {
    (!host.skills.is_empty()).then(|| host.skills.tool())
}

/// The `/skill` listing: the launch snapshot, in name order (`SkillCatalog` sorts).
fn skill_rows(catalog: &abycore::SkillCatalog) -> Vec<crate::contract::SkillRow> {
    catalog
        .skills()
        .iter()
        .map(|skill| crate::contract::SkillRow {
            name: skill.name.clone(),
            description: skill.description.clone(),
            input_hint: skill.input_hint.clone(),
            path: skill.path.display().to_string(),
        })
        .collect()
}
/// `/resume` rows from the durable store, newest first (`SessionStore::list`
/// orders them). An absent store — persistence disabled, or a store that could
/// not be opened — lists nothing instead of failing the command.
fn session_rows(store: Option<&abycore::SessionStore>) -> Vec<crate::contract::SessionRow> {
    store
        .map(|store| {
            store
                .list()
                .unwrap_or_default()
                .into_iter()
                .map(|s| crate::contract::SessionRow {
                    id: s.id,
                    title: s.title.or(if s.preview.is_empty() {
                        None
                    } else {
                        Some(s.preview)
                    }),
                    updated_at: Some(epoch_stamp(s.modified)),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `YYYY-MM-DD HH:MM` (UTC) for a picker meta line; civil-from-days per
/// Howard Hinnant's algorithm.
fn epoch_stamp(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// The first user prompt in a snapshot (the session title / preview).
/// The first non-empty line of the first user message, for the session title.
/// A line rather than the whole text: a skill invocation and any multi-line
/// prompt both open with the part worth naming, and the title stays one line.
fn first_user_text(snapshot: &abycore::SessionSnapshot) -> Option<String> {
    snapshot.items.iter().find_map(|item| match item {
        abycore::Item::Message {
            role: abycore::MessageRole::User,
            content,
            ..
        } => {
            let text = content
                .iter()
                .map(|part| part.text().to_string())
                .collect::<Vec<_>>()
                .join("");
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string)
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live tool call reaches the TUI in three pieces, all keyed by the call
    /// id: streamed deltas, the completed arguments, then execution start.
    #[test]
    fn live_tool_call_arguments_stream_before_execution() {
        let delta = AgentEvent::Model(abycore::StreamEvent::ToolArgumentsDelta {
            output_index: 0,
            item_id: "msg:0".into(),
            call_id: "call-1".into(),
            name: "bash".into(),
            delta: r#"{"command":"ls"#.into(),
            sequence: 3,
        });
        assert_eq!(
            translate("s", delta),
            vec![UiEvent::ToolCallDelta {
                session: "s".into(),
                call_id: "call-1".into(),
                name: "bash".into(),
                delta: r#"{"command":"ls"#.into(),
            }]
        );
        // Whitespace-only keep-alive deltas never reach the UI.
        let empty = AgentEvent::Model(abycore::StreamEvent::ToolArgumentsDelta {
            output_index: 0,
            item_id: "msg:0".into(),
            call_id: "call-1".into(),
            name: "bash".into(),
            delta: String::new(),
            sequence: 4,
        });
        assert!(translate("s", empty).is_empty());

        let done = AgentEvent::Model(abycore::StreamEvent::ItemDone {
            output_index: 0,
            item: abycore::Item::FunctionCall {
                id: "msg:0".into(),
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
            sequence: 5,
        });
        assert_eq!(
            translate("s", done),
            vec![UiEvent::ToolCall {
                session: "s".into(),
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }]
        );

        let started = AgentEvent::ToolStarted {
            call_id: "call-1".into(),
            name: "bash".into(),
        };
        assert_eq!(
            translate("s", started),
            vec![UiEvent::ToolStarted {
                session: "s".into(),
                call_id: "call-1".into(),
                name: "bash".into(),
            }]
        );
    }

    /// One committed `todo_write` reaches the UI as every plan surface at once:
    /// the transcript digest, the counts behind the composer's todo line, and
    /// the full checklist the progress dialog lists. A cleared list (`PlanView`
    /// with no items) hides them all instead of painting an all-zero panel.
    #[test]
    fn plan_events_carry_the_digest_and_the_live_todo_counts() {
        let plan = plan_view(&[
            ("inspect", "completed"),
            ("patch", "in_progress"),
            ("test", "pending"),
        ]);
        assert_eq!(
            translate("s", AgentEvent::PlanChanged { plan: Some(plan) }),
            vec![UiEvent::Plan {
                session: "s".into(),
                summary: "1 of 3 done · now: patch · 1 pending".into(),
                todos: vec![
                    PlanItem {
                        content: "inspect".into(),
                        status: PlanStatus::Completed,
                    },
                    PlanItem {
                        content: "patch".into(),
                        status: PlanStatus::InProgress,
                    },
                    PlanItem {
                        content: "test".into(),
                        status: PlanStatus::Pending,
                    },
                ],
                active: Some("patch".into()),
                active_extra: 0,
                completed: 1,
                total: 3,
            }]
        );

        // Concurrent work reports the first task plus the overflow count.
        let parallel = plan_view(&[("a", "in_progress"), ("b", "in_progress")]);
        let Some(UiEvent::Plan {
            active,
            active_extra,
            total,
            ..
        }) = translate(
            "s",
            AgentEvent::PlanChanged {
                plan: Some(parallel),
            },
        )
        .pop()
        else {
            panic!("a plan event");
        };
        assert_eq!(active.as_deref(), Some("a"));
        assert_eq!(active_extra, 1);
        assert_eq!(total, 2);

        // No list has been written in this turn.
        assert_eq!(
            translate("s", AgentEvent::PlanChanged { plan: None }),
            vec![blank_plan()]
        );
        // An explicit clear is an empty list, not `None`.
        let cleared = plan_view(&[]);
        assert_eq!(cleared.total, 0);
        assert_eq!(
            translate(
                "s",
                AgentEvent::PlanChanged {
                    plan: Some(cleared)
                }
            ),
            vec![blank_plan()]
        );
    }

    /// A `PlanView` for tests, built through abycore's public metadata path
    /// (its constructor is crate-private).
    fn plan_view(items: &[(&str, &str)]) -> abycore::PlanView {
        let todos: Vec<serde_json::Value> = items
            .iter()
            .map(|(content, status)| serde_json::json!({"content": content, "status": status}))
            .collect();
        let output = ToolOutput::text("updated")
            .with_meta(serde_json::json!({"todo_write": {"todos": todos}}));
        abycore::PlanView::from_tool_output(&output)
            .expect("todo metadata parses")
            .expect("a committed todo_write result yields a plan view")
    }

    fn blank_plan() -> UiEvent {
        UiEvent::Plan {
            session: "s".into(),
            summary: String::new(),
            todos: Vec::new(),
            active: None,
            active_extra: 0,
            completed: 0,
            total: 0,
        }
    }

    fn hooks(mode: PermissionMode) -> UiHooks {
        UiHooks {
            sink: Arc::new(|_| {}),
            permission_mode: mode,
            persist: None,
            guard: RepeatGuard::default(),
            compaction: None,
            context: None,
        }
    }

    fn pending(name: &str) -> abycore::PendingCall {
        abycore::PendingCall {
            call_id: "call-1".into(),
            name: name.into(),
            arguments: "{}".into(),
            state: abycore::PendingState::Ready,
        }
    }

    fn call(name: &str, arguments: &str) -> abycore::PendingCall {
        abycore::PendingCall {
            arguments: arguments.into(),
            ..pending(name)
        }
    }

    /// A unique scratch directory for tests that need real files on disk.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("abylab-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// The harness thresholds are the contract: a reminder on the 3rd, 5th and
    /// 8th consecutive identical call — and nowhere else.
    #[test]
    fn loop_guard_reminds_at_the_harness_thresholds_only() {
        let guard = RepeatGuard::default();
        let repeated = call("read", r#"{"file_path":"a.txt"}"#);
        assert!(guard.observe(&repeated).is_none(), "first call");
        assert!(guard.observe(&repeated).is_none(), "second call");
        let third = guard.observe(&repeated).expect("third call reminds");
        assert!(third.contains("3 times"), "{third}");
        assert!(guard.observe(&repeated).is_none(), "fourth call");
        assert!(guard.observe(&repeated).is_some(), "fifth call");
        assert!(guard.observe(&repeated).is_none(), "sixth call");
        assert!(guard.observe(&repeated).is_none(), "seventh call");
        let eighth = guard.observe(&repeated).expect("eighth call");
        assert!(eighth.contains("#8"), "{eighth}");
        assert!(
            eighth.contains("a.txt"),
            "the detailed reminder previews arguments"
        );
    }

    /// Property order must not defeat the comparison, and a different call
    /// starts a new streak.
    #[test]
    fn loop_guard_normalizes_arguments_and_resets_on_a_change() {
        let guard = RepeatGuard::default();
        let first = call("read", r#"{"file_path":"a.txt","offset":1}"#);
        let reordered = call("read", r#"{"offset":1,"file_path":"a.txt"}"#);
        assert!(guard.observe(&first).is_none());
        assert!(guard.observe(&reordered).is_none());
        assert!(
            guard.observe(&first).is_some(),
            "reordered keys are the same call"
        );

        let guard = RepeatGuard::default();
        let other = call("read", r#"{"file_path":"b.txt"}"#);
        let same = call("read", r#"{"file_path":"a.txt"}"#);
        assert!(guard.observe(&same).is_none());
        assert!(guard.observe(&same).is_none());
        assert!(guard.observe(&other).is_none(), "a different call resets");
        assert!(guard.observe(&other).is_none(), "count restarts at one");
        assert!(guard.observe(&other).is_some(), "and reaches three again");
    }

    /// A new run (a fresh prompt or a continuation segment) starts the count
    /// over, and an idempotent checklist rewrite is never counted.
    #[test]
    fn loop_guard_resets_per_run_and_skips_todo_write() {
        let guard = RepeatGuard::default();
        let repeated = call("read", "{}");
        for _ in 0..3 {
            guard.observe(&repeated);
        }
        guard.reset();
        assert!(
            guard.observe(&repeated).is_none(),
            "a run boundary is not a repeat"
        );

        let todo = pending("todo_write");
        for _ in 0..9 {
            assert!(guard.observe(&todo).is_none(), "todo_write is excluded");
        }
    }

    /// The message a user sees when auto-continuation ran out: it must name
    /// the live limits and both ways out, not the old "raise limits" hint
    /// that pointed at no flag.
    #[test]
    fn budget_error_names_the_limits_and_the_flags() {
        let limits = TurnLimits {
            max_requests: 128,
            max_tool_calls: 256,
            continuations: 3,
            ..TurnLimits::default()
        };
        let text = turn_error_text(
            &abycore::Error::new(ErrorKind::BudgetExceeded, "HTTP request budget exhausted"),
            limits,
        );
        assert!(text.contains("--max-requests"), "{text}");
        assert!(text.contains("--max-tool-calls"), "{text}");
        assert!(text.contains("128"), "{text}");
        assert!(text.contains("256"), "{text}");
        assert!(text.contains("HTTP request budget exhausted"), "{text}");
        assert!(text.contains("继续输入"), "{text}");
    }

    #[test]
    fn stock_presets_map_to_abycore_modes() {
        assert_eq!(
            parse_permission_mode("read-only"),
            Some(PermissionMode::ReadOnly)
        );
        assert_eq!(
            parse_permission_mode("workspace-write"),
            Some(PermissionMode::WorkspaceWrite)
        );
        assert_eq!(
            parse_permission_mode("danger-full-access"),
            Some(PermissionMode::FullAccess)
        );
        assert_eq!(parse_permission_mode("unknown"), None);
        assert_eq!(
            permission_preset(PermissionMode::FullAccess),
            "danger-full-access"
        );
    }

    #[test]
    fn goal_arguments_split_the_round_allowance_from_the_objective() {
        assert_eq!(
            parse_goal_argument("ship the release"),
            ("ship the release", None)
        );
        assert_eq!(
            parse_goal_argument("@3 ship the release"),
            ("ship the release", Some(3))
        );
        assert_eq!(parse_goal_argument("@0 ship"), ("ship", Some(0)));
        // A bare allowance names no objective: the usage arm handles "rounds".
        assert_eq!(parse_goal_argument("@3"), ("rounds", None));
        // A non-numeric round token is an ordinary objective.
        assert_eq!(parse_goal_argument("@here fix it"), ("@here fix it", None));
        assert_eq!(parse_goal_argument("rounds"), ("rounds", None));
    }

    #[tokio::test]
    async fn modes_apply_the_expected_approval_policy() {
        let denied = hooks(PermissionMode::ReadOnly)
            .authorize(pending("write"))
            .await
            .expect("authorization succeeds");
        assert!(matches!(denied, ToolDecision::Deny(reason) if reason.contains("read-only")));

        let allowed = hooks(PermissionMode::FullAccess)
            .authorize(pending("bash"))
            .await
            .expect("authorization succeeds");
        assert_eq!(allowed, ToolDecision::Allow);

        let workspace_write = hooks(PermissionMode::WorkspaceWrite)
            .authorize(pending("edit"))
            .await
            .expect("authorization succeeds");
        assert_eq!(workspace_write, ToolDecision::Allow);

        // Read-only file tools never ask, in either sandboxed preset: glob and
        // grep are workspace-rooted reads like `read`.
        for mode in [PermissionMode::ReadOnly, PermissionMode::WorkspaceWrite] {
            for name in ["read", "glob", "grep", "todo_write"] {
                let decision = hooks(mode)
                    .authorize(pending(name))
                    .await
                    .expect("authorization succeeds");
                assert_eq!(decision, ToolDecision::Allow, "{name} in {mode:?}");
            }
        }
    }

    #[test]
    fn permission_facts_match_the_selected_preset() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });

        emit_permission_facts(&sink, "session-1", PermissionMode::FullAccess);

        let events = events.lock().expect("event lock");
        assert!(matches!(
            &events[0],
            Event::Ui(UiEvent::PermissionPreset { session, preset })
                if session == "session-1" && preset == "danger-full-access"
        ));
        assert!(matches!(
            &events[1],
            Event::Ui(UiEvent::SandboxMode { session, mode })
                if session == "session-1" && mode == "danger-full-access"
        ));
        assert!(matches!(
            &events[2],
            Event::Ui(UiEvent::ApprovalPolicy { session, policy })
                if session == "session-1" && policy == "never"
        ));
    }

    #[tokio::test]
    async fn set_permission_command_switches_a_live_driver() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("abylab-permission-{}-{unique}", std::process::id()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_query_tx, query_rx) = mpsc::unbounded_channel();
        let (_interrupt_tx, interrupt_rx) = mpsc::unbounded_channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });

        cmd_tx
            .send(Cmd::SetPermission {
                preset: "danger-full-access".into(),
            })
            .expect("queue permission command");
        cmd_tx.send(Cmd::Shutdown).expect("queue shutdown");
        drive(
            DriverConfig {
                session_id: "permission-test".into(),
                resume: None,
                sessions_root: None,
                home: None,
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: Some("test-key".into()),
                base_url: None,
                limits: TurnLimits::default(),
                compaction: None,
            },
            cmd_rx,
            query_rx,
            interrupt_rx,
            sink,
        )
        .await;

        let events = events.lock().expect("event lock");
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Ui(UiEvent::PermissionPreset { preset, .. })
                if preset == "danger-full-access"
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Event::Ctl(CtlEvent::TuiOpDone(message))
                    if message.starts_with("permission →")
            )),
            "a successful switch reports facts only, never an op-done echo"
        );
        drop(events);
        std::fs::remove_dir_all(&workspace).expect("remove test workspace");
    }

    #[tokio::test]
    async fn fresh_startup_binds_the_session_before_the_first_turn() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("abylab-bind-{}-{unique}", std::process::id()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_query_tx, query_rx) = mpsc::unbounded_channel();
        let (_interrupt_tx, interrupt_rx) = mpsc::unbounded_channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });

        cmd_tx.send(Cmd::Shutdown).expect("queue shutdown");
        drive(
            DriverConfig {
                session_id: "bind-test".into(),
                resume: None,
                sessions_root: None,
                home: None,
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: Some("test-key".into()),
                base_url: None,
                limits: TurnLimits::default(),
                compaction: None,
            },
            cmd_rx,
            query_rx,
            interrupt_rx,
            sink,
        )
        .await;

        let events = events.lock().expect("event lock");
        let bound = events.iter().position(|event| {
            matches!(
                event,
                Event::Ctl(CtlEvent::SessionBound { session_id, notice, .. })
                    if session_id == "bind-test" && notice.is_none()
            )
        });
        let facts = events
            .iter()
            .position(|event| matches!(event, Event::Ui(UiEvent::PermissionPreset { .. })));
        assert!(bound.is_some(), "fresh start binds the session: {events:?}");
        assert!(
            facts.is_some() && bound.unwrap() < facts.unwrap(),
            "session binds before the permission facts arrive: {events:?}"
        );
        drop(events);
        std::fs::remove_dir_all(&workspace).expect("remove test workspace");
    }

    /// A keyless boot leaves the agent unbuilt (abycore refuses an empty
    /// client config); the `/login` command must bring the session alive
    /// without a restart.
    #[tokio::test]
    async fn login_after_a_keyless_boot_binds_the_session() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("abylab-login-{}-{unique}", std::process::id()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_query_tx, query_rx) = mpsc::unbounded_channel();
        let (_interrupt_tx, interrupt_rx) = mpsc::unbounded_channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });

        cmd_tx
            .send(Cmd::SetApiKey {
                key: Some("sk-test-key-000000".into()),
            })
            .expect("queue login command");
        cmd_tx.send(Cmd::Shutdown).expect("queue shutdown");
        drive(
            DriverConfig {
                session_id: "login-test".into(),
                resume: None,
                sessions_root: None,
                home: None,
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: None,
                base_url: None,
                limits: TurnLimits::default(),
                compaction: None,
            },
            cmd_rx,
            query_rx,
            interrupt_rx,
            sink,
        )
        .await;

        let events = events.lock().expect("event lock");
        // The keyless boot reports its detection first…
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Ctl(CtlEvent::Error(err)) if err.contains("no API key"))),
            "the keyless boot is reported: {events:?}"
        );
        // …and the login command binds a fresh session without a restart.
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Ctl(CtlEvent::SessionBound { session_id, .. })
                    if session_id == "login-test")
            ),
            "login binds the session: {events:?}"
        );
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Ctl(CtlEvent::TuiOpDone(message))
                    if message.contains("api key set"))
            ),
            "the login reports success: {events:?}"
        );
        drop(events);
        std::fs::remove_dir_all(&workspace).expect("remove test workspace");
    }

    /// `/resume` discovery: a persisted workspace snapshot is listed with a
    /// A skill is a read of text discovery already loaded: no prompt, in any
    /// permission preset.
    #[tokio::test]
    async fn the_skill_tool_never_asks_for_approval() {
        let allowed = hooks(PermissionMode::ReadOnly)
            .authorize(pending(abycore::SkillTool::NAME))
            .await
            .expect("authorization succeeds");
        assert_eq!(allowed, ToolDecision::Allow);
    }

    /// Rows carry what the menu and `/skill` render, and the path tells the
    /// user which file won.
    #[test]
    fn skill_rows_expose_the_menu_metadata() {
        let workspace = scratch_dir("skills-workspace");
        let file = workspace.join(".agents/skills/deploy/SKILL.md");
        std::fs::create_dir_all(file.parent().expect("skill directory")).expect("create");
        std::fs::write(
            &file,
            "---\ndescription: ship it\ninput-hint: <env>\n---\nDeploy to <env>.\n",
        )
        .expect("write skill");

        let catalog = abycore::SkillCatalog::discover(&workspace);
        assert!(catalog.warnings().is_empty(), "{:?}", catalog.warnings());
        let rows = skill_rows(&catalog);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "deploy");
        assert_eq!(rows[0].description, "ship it");
        assert_eq!(rows[0].input_hint.as_deref(), Some("<env>"));
        assert_eq!(rows[0].path, file.to_string_lossy());

        // The tool registers only when there is something to read.
        let host = HostPolicy {
            sink: Arc::new(|_| {}),
            compaction: None,
            max_tokens: None,
            agent_sessions: Arc::default(),
            instructions: crate::instructions::InstructionSource {
                workspace: workspace.clone(),
                home: None,
            },
            skills: Arc::new(catalog),
        };
        assert!(skill_tool(&host).is_some(), "a session with skills has it");
        let without = scratch_dir("skills-none");
        let empty = HostPolicy {
            skills: Arc::new(abycore::SkillCatalog::discover(&without)),
            instructions: crate::instructions::InstructionSource {
                workspace: without.clone(),
                home: None,
            },
            ..host
        };
        assert!(
            skill_tool(&empty).is_none(),
            "no skills, no tool definition to pay for"
        );
        std::fs::remove_dir_all(&without).expect("remove scratch directory");
        std::fs::remove_dir_all(&workspace).expect("remove scratch workspace");
    }

    /// The title names the first line: a skill invocation opens with the typed
    /// command, and a multi-line prompt with its own first sentence.
    #[test]
    fn session_titles_take_the_first_line_only() {
        let mut snapshot = abycore::SessionSnapshot::new("sys", ModelOptions::default());
        assert_eq!(first_user_text(&snapshot), None, "no user message yet");
        snapshot.items.push(abycore::Item::user(
            "\n\n/deploy prod\n\n<system-reminder>\nbody",
        ));
        assert_eq!(first_user_text(&snapshot).as_deref(), Some("/deploy prod"));
    }

    /// `drive` discovers from its launch config, so `/skill` answers with what
    /// the menu and the injection would use.
    #[tokio::test]
    async fn skills_reach_the_ui_from_the_workspace() {
        let home = scratch_dir("drive-skills-home");
        let workspace = scratch_dir("drive-skills-workspace");
        std::fs::create_dir_all(workspace.join(".agents/skills/deploy")).expect("skill directory");
        std::fs::write(
            workspace.join(".agents/skills/deploy/SKILL.md"),
            "---\ndescription: ship it\n---\nDeploy it.\n",
        )
        .expect("directory skill");
        std::fs::write(
            workspace.join(".agents/skills/review.md"),
            "Review the diff.\n",
        )
        .expect("flat skill");

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_query_tx, query_rx) = mpsc::unbounded_channel();
        let (_interrupt_tx, interrupt_rx) = mpsc::unbounded_channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });
        cmd_tx.send(Cmd::FetchSkills).expect("queue fetch");
        cmd_tx.send(Cmd::Shutdown).expect("queue shutdown");
        drive(
            DriverConfig {
                session_id: "skills-list-test".into(),
                resume: None,
                sessions_root: Some(workspace.join("store").to_string_lossy().into_owned()),
                home: Some(home.to_string_lossy().into_owned()),
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: Some("test-key".into()),
                base_url: None,
                limits: TurnLimits::default(),
                compaction: None,
            },
            cmd_rx,
            query_rx,
            interrupt_rx,
            sink,
        )
        .await;

        let events = events.lock().expect("event lock");
        let rows = events
            .iter()
            .find_map(|event| match event {
                Event::Ctl(CtlEvent::Skills { skills }) => Some(skills.clone()),
                _ => None,
            })
            .expect("Skills arrives");
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["deploy", "review"], "both layouts, name order");
        assert_eq!(rows[0].description, "ship it");
        assert_eq!(rows[1].description, "Review the diff.", "from the body");
        assert!(rows[1].path.ends_with(".agents/skills/review.md"));
        drop(events);
        std::fs::remove_dir_all(&home).expect("remove scratch home");
        std::fs::remove_dir_all(&workspace).expect("remove scratch workspace");
    }

    /// human `updated_at` stamp and the echoed `/resume <prefix>` argument.
    #[tokio::test]
    async fn list_sessions_reports_workspace_snapshots() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let workspace =
            std::env::temp_dir().join(format!("abylab-list-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("workspace directory");
        let store = abycore::SessionStore::at(workspace.join("store"), &workspace).expect("store");
        let snapshot = abycore::SessionSnapshot::new("sys", ModelOptions::default());
        let mut writer = store.create("resume-list-test", &snapshot).expect("writer");
        store
            .append_checkpoint(&mut writer, 0, &snapshot)
            .expect("checkpoint");
        drop(writer);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (_query_tx, query_rx) = mpsc::unbounded_channel();
        let (_interrupt_tx, interrupt_rx) = mpsc::unbounded_channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            captured.lock().expect("event lock").push(event);
        });
        // The handle routes `ListSessions` to the query task; a command
        // written straight to the channel is served by the loop arm instead.
        cmd_tx
            .send(Cmd::ListSessions {
                prefix: Some("resume-list".into()),
            })
            .expect("queue list");
        cmd_tx.send(Cmd::Shutdown).expect("queue shutdown");
        drive(
            DriverConfig {
                session_id: "list-test".into(),
                resume: None,
                sessions_root: Some(workspace.join("store").to_string_lossy().into_owned()),
                home: None,
                workspace: workspace.to_string_lossy().into_owned(),
                model: "deepseek-flash".into(),
                reasoning: "off".into(),
                permission: None,
                max_tokens: None,
                api_key: Some("test-key".into()),
                base_url: None,
                limits: TurnLimits::default(),
                compaction: None,
            },
            cmd_rx,
            query_rx,
            interrupt_rx,
            sink,
        )
        .await;

        let events = events.lock().expect("event lock");
        let rows = events.iter().find_map(|event| match event {
            Event::Ctl(CtlEvent::SessionList { sessions, prefix }) => {
                assert_eq!(prefix.as_deref(), Some("resume-list"));
                Some(sessions.clone())
            }
            _ => None,
        });
        let rows = rows.expect("SessionList arrives");
        assert_eq!(rows.len(), 1, "the persisted session is listed: {rows:?}");
        assert_eq!(rows[0].id, "resume-list-test");
        assert_eq!(
            rows[0].updated_at.as_deref().map(str::len),
            Some("YYYY-MM-DD HH:MM".len()),
            "picker-ready timestamp, not raw epoch seconds"
        );
        drop(events);
        std::fs::remove_dir_all(&workspace).expect("remove test workspace");
    }
}
