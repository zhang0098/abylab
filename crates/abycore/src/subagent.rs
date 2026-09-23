//! In-process delegation with isolated conversations and host-owned background lifetimes.
mod runtime;
mod tools;

use crate::{
    Agent, AgentEvent, AgentHooks, CancellationToken, DeepSeekClient, Error, ErrorKind, Item,
    ModelOptions, Result, RunOptions, SessionSnapshot, StopReason, ToolOutput,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, broadcast};

/// Optional per-child persistence/authorization hooks, created before publication.
/// Parent authorization also applies; the parent's checkpoint writer is never reused.
pub type SubagentHookFactory =
    Arc<dyn Fn(&SubagentInfo) -> Result<Arc<dyn AgentHooks>> + Send + Sync>;

#[derive(Clone)]
pub struct SubagentConfig {
    /// Top-level agents are depth zero; zero disables delegation. Default: 3.
    pub max_depth: usize,
    /// Maximum simultaneously running children across this manager. Default: 8.
    pub max_running: usize,
    /// Includes completed records; the host can forget them. Default: 64.
    pub max_agents: usize,
    /// Independent per-child turn budgets. Background turns outlive the calling tool.
    /// Cancelling this token stops all current and future turns in this manager.
    pub run_options: RunOptions,
    /// Fixed child model override; omitted means inherit the parent's model options.
    pub model: Option<ModelOptions>,
    /// Fixed child system prompt override; omitted means inherit the parent prompt.
    pub system_prompt: Option<String>,
    /// Restrict inherited tools by name. None inherits all; an empty list grants none.
    pub allowed_tools: Option<Vec<String>>,
    pub hooks: Option<SubagentHookFactory>,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_running: 8,
            max_agents: 64,
            run_options: RunOptions::default(),
            model: None,
            system_prompt: None,
            allowed_tools: None,
            hooks: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMode {
    /// Fresh history; the task must contain all necessary context.
    #[default]
    Spawn,
    /// Seed with the parent's completed turns, excluding its active turn.
    Fork,
}

#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
    pub mode: SubagentMode,
}

impl SubagentRequest {
    pub fn new(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            prompt: prompt.into(),
            mode: SubagentMode::Spawn,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.description.trim().is_empty() || self.description.len() > 1024 {
            return Err(invalid("description must contain 1–1024 bytes"));
        }
        Inbox::validate_message(&self.prompt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    /// The last turn settled. The result's stop reason says whether it succeeded.
    Idle,
    /// A tool call needs explicit host resolution before further work.
    NeedsResolution,
    /// The executor was dropped or panicked; inspect the retained snapshot.
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentResult {
    pub output: String,
    pub stop_reason: StopReason,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub id: String,
    pub parent_id: String,
    pub depth: usize,
    pub description: String,
    pub mode: SubagentMode,
    pub status: SubagentStatus,
    pub result: Option<SubagentResult>,
}

/// Best-effort live observation. A lagging broadcast receiver may lose events;
/// `get` and `snapshot` remain authoritative. Persistence uses the hook factory.
#[derive(Clone, Debug)]
pub enum SubagentEvent {
    Started(SubagentInfo),
    Agent { agent_id: String, event: AgentEvent },
    Finished(SubagentInfo),
}

#[derive(Default)]
pub(crate) struct Inbox(Mutex<VecDeque<Vec<crate::ContentPart>>>);

impl Inbox {
    fn validate_message(message: &str) -> Result<()> {
        if message.trim().is_empty() || message.len() > 64 * 1024 {
            return Err(invalid("message must contain 1–65536 bytes"));
        }
        Ok(())
    }

    pub(crate) fn send(&self, message: String) -> Result<()> {
        Self::validate_message(&message)?;
        self.send_parts(vec![crate::ContentPart::InputText { text: message }])
    }

    pub(crate) fn send_parts(&self, parts: Vec<crate::ContentPart>) -> Result<()> {
        if parts.is_empty()
            || !parts.iter().any(|part| match part {
                crate::ContentPart::InputText { text } => !text.trim().is_empty(),
                crate::ContentPart::InputImage { .. } => true,
                _ => false,
            })
        {
            return Err(invalid("message must contain text or an image"));
        }
        let input = crate::Item::user_parts(parts);
        input.validate()?;
        let crate::Item::Message { content, .. } = input else {
            unreachable!("user_parts makes a message")
        };
        let mut queue = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if queue.len() >= 32 {
            return Err(Error::new(ErrorKind::BudgetExceeded, "agent inbox is full"));
        }
        queue.push_back(content);
        Ok(())
    }

    pub(crate) fn drain(&self) -> Vec<Vec<crate::ContentPart>> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    }
}

/// Host-facing handle for injecting user text into a live agent.
///
/// A message sent here is **not** a new turn and never cancels the running
/// step: [`Agent::run`](crate::Agent::run) drains it at the next complete
/// tool-batch boundary (right before the following request), and when the
/// model has already finished its last step the run continues one more step
/// instead of ending. An idle agent keeps the message and spends it on the
/// first request of its next run, so a steer that misses the window degrades
/// into the next turn rather than failing.
///
/// This is the harness's `agent.steer` / `inbox.nextStep` seam; the driver
/// uses it for the composer's send-now gesture.
#[derive(Clone)]
pub struct SteerHandle(pub(crate) Arc<Inbox>);

impl SteerHandle {
    /// Queue one steering message. Errors only for empty/oversized text or a
    /// full inbox (32 pending), never because the agent is idle.
    pub fn send(&self, text: impl Into<String>) -> Result<()> {
        self.0.send(text.into())
    }

    pub fn send_parts(&self, parts: Vec<crate::ContentPart>) -> Result<()> {
        self.0.send_parts(parts)
    }

    /// Whether a message is waiting for the next step boundary.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) struct Parent {
    pub id: String,
    pub depth: usize,
    pub client: DeepSeekClient,
    pub system_prompt: String,
    pub model: ModelOptions,
    pub history: Vec<Item>,
    pub tools: BTreeMap<String, crate::agent::RegisteredTool>,
    pub hooks: Option<Arc<dyn AgentHooks>>,
    pub inbox: Arc<Inbox>,
    pub subagent_manager: Option<u128>,
}

struct ChildState {
    info: SubagentInfo,
    agent: Option<Agent>,
    cancellation: CancellationToken,
}

struct Entry {
    state: Mutex<ChildState>,
    snapshot: Mutex<SessionSnapshot>,
    inbox: Arc<Inbox>,
    parent_inbox: Arc<Inbox>,
    done: Notify,
    notify_parent: bool,
}

impl Entry {
    fn info(&self) -> SubagentInfo {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .info
            .clone()
    }

    async fn wait(&self) -> SubagentInfo {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let info = self.info();
            if info.status != SubagentStatus::Running {
                return info;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct Store {
    entries: BTreeMap<String, Arc<Entry>>,
    closed: bool,
}

struct Manager {
    id: u128,
    config: SubagentConfig,
    store: Mutex<Store>,
    events: broadcast::Sender<SubagentEvent>,
}

/// A cloneable owner for child agents. Registering tools grants delegation explicitly.
/// Background work survives parent turn completion/cancellation. Call `shutdown` before
/// stopping Tokio; dropping the last owner requests cancellation without waiting.
#[derive(Clone)]
pub struct Subagents(Arc<Manager>);

impl Default for Subagents {
    fn default() -> Self {
        Self::with_config(SubagentConfig::default()).expect("valid default subagent config")
    }
}

impl Subagents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: SubagentConfig) -> Result<Self> {
        config.run_options.validate()?;
        if config.max_running == 0 || config.max_agents == 0 {
            return Err(invalid(
                "subagent running and retention limits must be positive",
            ));
        }
        if let Some(model) = &config.model {
            model.validate()?;
        }
        let (events, _) = broadcast::channel(256);
        Ok(Self(Arc::new(Manager {
            id: rand::random(),
            config,
            store: Mutex::default(),
            events,
        })))
    }

    /// Register spawn/fork and all four control tools atomically. Keep this owner alive.
    pub fn register(&self, agent: &mut Agent) -> Result<()> {
        agent.register_tools(tools::definitions(Arc::downgrade(&self.0)))?;
        agent.subagent_manager = Some(self.0.id);
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SubagentEvent> {
        self.0.events.subscribe()
    }

    /// Start a continuable background child using the parent's current capabilities.
    /// No network request is made until the new Tokio task is scheduled.
    pub fn start(&self, parent: &Agent, request: SubagentRequest) -> Result<SubagentInfo> {
        self.0.start(
            &parent.subagent_parent(),
            request,
            None,
            parent.subagent_manager.is_some(),
        )
    }

    /// Host access spans all children in this manager; model tools enforce lineage.
    pub fn list(&self) -> Vec<SubagentInfo> {
        self.0
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .values()
            .map(|entry| entry.info())
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<SubagentInfo> {
        Ok(self.0.entry(id)?.info())
    }

    /// Wait for the current turn to settle. Dropping this wait does not cancel the child.
    pub async fn wait(&self, id: &str) -> Result<SubagentInfo> {
        Ok(self.0.entry(id)?.wait().await)
    }

    /// Queue steering at the next complete tool-batch boundary, or start an idle child.
    /// An interrupted child with pending tools must first be resolved by the host.
    pub fn send_message(&self, id: &str, message: impl Into<String>) -> Result<()> {
        self.0.send_message(id, message.into())
    }

    /// Request cancellation of this child's current turn; descendants keep running.
    pub fn interrupt(&self, id: &str) -> Result<()> {
        self.0
            .entry(id)?
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cancellation
            .cancel();
        Ok(())
    }

    /// Last checkpoint or final snapshot, including unknown tool intents. No IO is performed.
    pub fn snapshot(&self, id: &str) -> Result<SessionSnapshot> {
        Ok(self
            .0
            .entry(id)?
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone())
    }

    /// Resolve one pending call after verifying its side effects. Never replays a
    /// tool. Returns the output as committed (see [`Agent::resolve_tool`]).
    pub fn resolve_tool(&self, id: &str, call_id: &str, output: ToolOutput) -> Result<ToolOutput> {
        let entry = self.0.entry(id)?;
        let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
        let agent = state
            .agent
            .as_mut()
            .ok_or_else(|| invalid("agent is running or unavailable"))?;
        let committed = agent.resolve_tool(call_id, output)?;
        let snapshot = agent.snapshot();
        state.info.status = if snapshot.pending.is_empty() {
            SubagentStatus::Idle
        } else {
            SubagentStatus::NeedsResolution
        };
        *entry.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snapshot;
        Ok(committed)
    }

    /// Forget a settled leaf. Children must be forgotten first to preserve control ancestry.
    pub fn forget(&self, id: &str) -> Result<()> {
        let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
        let entry = store
            .entries
            .get(id)
            .ok_or_else(|| invalid("unknown subagent"))?;
        if entry.info().status == SubagentStatus::Running {
            return Err(invalid(
                "interrupt and wait for the subagent before forgetting it",
            ));
        }
        if store
            .entries
            .values()
            .any(|entry| entry.info().parent_id == id)
        {
            return Err(invalid("forget descendants before their parent"));
        }
        store.entries.remove(id);
        Ok(())
    }

    /// Reject new starts/follow-ups, cancel all running children, and await settlement.
    /// Independently launched tool jobs (such as background Bash) remain host-owned.
    pub async fn shutdown(&self) {
        let entries: Vec<_> = {
            let mut store = self.0.store.lock().unwrap_or_else(|p| p.into_inner());
            store.closed = true;
            store.entries.values().cloned().collect()
        };
        for entry in &entries {
            entry
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancellation
                .cancel();
        }
        for entry in entries {
            entry.wait().await;
        }
    }
}

impl Manager {
    fn entry(&self, id: &str) -> Result<Arc<Entry>> {
        self.store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("unknown subagent"))
    }

    fn check_capacity(&self, store: &Store) -> Result<()> {
        if store.closed || self.config.run_options.cancellation.is_cancelled() {
            return Err(invalid("subagent manager is shut down or cancelled"));
        }
        if store
            .entries
            .values()
            .filter(|e| e.info().status == SubagentStatus::Running)
            .count()
            >= self.config.max_running
        {
            return Err(Error::new(
                ErrorKind::BudgetExceeded,
                "subagent concurrency limit reached",
            ));
        }
        Ok(())
    }

    fn start(
        &self,
        parent: &Parent,
        request: SubagentRequest,
        foreground: Option<CancellationToken>,
        notify_parent: bool,
    ) -> Result<SubagentInfo> {
        request.validate()?;
        if parent.subagent_manager.is_some_and(|id| id != self.id) {
            return Err(invalid(
                "parent tools belong to a different subagent manager",
            ));
        }
        if parent.depth >= self.config.max_depth {
            return Err(Error::new(
                ErrorKind::BudgetExceeded,
                "subagent depth limit reached",
            ));
        }
        if let Some(allow) = &self.config.allowed_tools
            && allow.iter().any(|name| !parent.tools.contains_key(name))
        {
            return Err(invalid(
                "subagent tool allowlist contains an unavailable tool",
            ));
        }
        let mut snapshot = SessionSnapshot::new(
            self.config
                .system_prompt
                .clone()
                .unwrap_or_else(|| parent.system_prompt.clone()),
            self.config
                .model
                .clone()
                .unwrap_or_else(|| parent.model.clone()),
        );
        if request.mode == SubagentMode::Fork {
            snapshot.items = parent.history.clone();
            // The inherited prefix can end with a compaction summary — a user
            // item standing in for completed history. The snapshot invariant
            // ties `needs_response` to that boundary, so mirror it here; the
            // child's own prompt then answers the summary in the same run
            // instead of `Agent::restore` rejecting the fork outright.
            snapshot.needs_response = matches!(
                snapshot.items.last(),
                Some(
                    Item::Message {
                        role: crate::MessageRole::User,
                        ..
                    } | Item::FunctionCallOutput { .. }
                )
            );
        }
        let mut agent = Agent::restore(parent.client.clone(), snapshot.clone())?;
        agent.depth = parent.depth + 1;
        agent.inherit_tools(parent, self.config.allowed_tools.as_deref());
        let info = SubagentInfo {
            id: agent.id.clone(),
            parent_id: parent.id.clone(),
            depth: agent.depth,
            description: request.description,
            mode: request.mode,
            status: SubagentStatus::Running,
            result: None,
        };
        let hooks = self
            .config
            .hooks
            .as_ref()
            .map(|factory| factory(&info))
            .transpose()?;
        let cancellation = self.config.run_options.cancellation.child_token();
        let entry = Arc::new(Entry {
            state: Mutex::new(ChildState {
                info: info.clone(),
                agent: None,
                cancellation: cancellation.clone(),
            }),
            snapshot: Mutex::new(snapshot),
            inbox: agent.inbox.clone(),
            parent_inbox: parent.inbox.clone(),
            done: Notify::new(),
            notify_parent,
        });
        agent.set_hooks(Arc::new(runtime::ChildHooks {
            entry: Arc::downgrade(&entry),
            parent: parent.hooks.clone(),
            own: hooks,
        }));
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("subagents require a Tokio runtime"))?;
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        self.check_capacity(&store)?;
        if store.entries.len() >= self.config.max_agents {
            return Err(Error::new(
                ErrorKind::BudgetExceeded,
                "subagent retention limit reached; forget completed agents",
            ));
        }
        store.entries.insert(info.id.clone(), entry.clone());
        let _ = self.events.send(SubagentEvent::Started(info.clone()));
        runtime::launch(
            &runtime,
            entry,
            agent,
            Some(request.prompt),
            self.config.run_options.clone(),
            foreground,
            self.events.clone(),
        );
        Ok(info)
    }

    fn send_message(&self, id: &str, message: String) -> Result<()> {
        Inbox::validate_message(&message)?;
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if store.closed || self.config.run_options.cancellation.is_cancelled() {
            return Err(invalid("subagent manager is shut down or cancelled"));
        }
        let entry = store
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("unknown subagent"))?;
        // Keep store -> entry lock ordering consistent with starts, listing and forgetting.
        let running = entry.info().status == SubagentStatus::Running;
        if !running {
            self.check_capacity(&store)?;
        }
        let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.info.status == SubagentStatus::Running {
            return entry.inbox.send(message);
        }
        if state.info.status != SubagentStatus::Idle {
            return Err(Error::new(
                ErrorKind::NeedsResolution,
                "subagent needs host recovery before receiving another message",
            ));
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| invalid("subagents require a Tokio runtime"))?;
        entry.inbox.send(message)?;
        let agent = state.agent.take().expect("idle agent is resident");
        state.cancellation = self.config.run_options.cancellation.child_token();
        state.info.status = SubagentStatus::Running;
        state.info.result = None;
        let _ = self.events.send(SubagentEvent::Started(state.info.clone()));
        drop(state);
        runtime::launch(
            &runtime,
            entry,
            agent,
            None,
            self.config.run_options.clone(),
            None,
            self.events.clone(),
        );
        Ok(())
    }

    fn authorize(&self, caller: &Parent, id: &str, descendants: bool) -> Result<Arc<Entry>> {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let target = store
            .entries
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("unknown subagent"))?;
        let mut parent_id = target.info().parent_id;
        loop {
            if parent_id == caller.id {
                return Ok(target);
            }
            if !descendants {
                break;
            }
            let Some(parent) = store.entries.get(&parent_id) else {
                break;
            };
            parent_id = parent.info().parent_id;
        }
        Err(invalid(
            "target is outside the caller's permitted subagent lineage",
        ))
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        for entry in self
            .store
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .values()
        {
            entry
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancellation
                .cancel();
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::Configuration, message)
}
