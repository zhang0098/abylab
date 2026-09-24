//! Transcript model: the scrollback cells and their rendering to styled lines.
//!
//! Mirrors what the deepseek-harness Web UI surfaces for a session: user
//! prompts, streaming reasoning, streaming assistant text, tool calls with
//! results, injected context, subagent lifecycle, usage accounting, and turn
//! outcomes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::events::UiEvent;
use crate::locale::Locale;
use crate::theme::Theme;

/// Collapsed tool preview height; click toggles full expansion. Mouse wheel
/// always belongs to the outer transcript.
pub const TOOL_VIEWPORT: usize = 4;
const COLLAPSED_REASONING_PREVIEW: usize = 2;
/// Thumbnail width in cells (PNG images reserve a box of this many columns).
const THUMB_COLS: usize = 24;

/// How the transcript paints tool calls. `ctrl+o` cycles the three, so a long
/// agent run can open from full bodies down to one line per call and back
/// without leaving the keyboard. The default is `Summary`: a turn's tool work
/// reads as a short block of lines, and the cards are one chord away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolOutput {
    /// One card per call: a fixed tail preview plus a click-to-expand footer.
    Preview,
    /// One card per call showing the whole body.
    Full,
    /// One line per call — `Ran <command>` — grouped under a count header.
    /// Failures keep their first line; a click expands that call's full output.
    #[default]
    Summary,
}

impl ToolOutput {
    /// The next state in the `ctrl+o` cycle.
    pub fn cycle(self) -> Self {
        match self {
            Self::Preview => Self::Full,
            Self::Full => Self::Summary,
            Self::Summary => Self::Preview,
        }
    }

    /// Whether every thought and tool body should be painted in full. `Summary`
    /// deliberately does not expand thoughts: one line per call is the point.
    pub fn expand_all(self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// How a client-painted user bubble stands with the agent's inbox.
///
/// The composer paints its own echo the moment the key is pressed, before any
/// round trip, so the bubble has to say what it is waiting for: a FIFO item
/// (queued), a message the running turn has not picked up yet (steering), or
/// an ordinary part of the conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// The agent has it: an ordinary user row (default).
    #[default]
    Delivered,
    /// Client FIFO item; ships when the running turn ends.
    Queued,
    /// Send Now: handed to the running turn's inbox, waiting for the next step
    /// boundary to append it (deepseek-harness's pending-steering rows).
    Steering,
}

impl Delivery {
    /// The small trailing marker the transcript paints on the first row.
    fn marker(self) -> Option<&'static str> {
        match self {
            Delivery::Delivered => None,
            Delivery::Queued => Some("queued"),
            Delivery::Steering => Some("steering"),
        }
    }

    /// Marker color: amber for a FIFO item, the brand tone for a steer the
    /// running turn has not picked up yet.
    fn tint(self, theme: &crate::theme::Theme) -> ratatui::style::Color {
        match self {
            Delivery::Queued => theme.warn_soft(),
            _ => theme.brand_soft,
        }
    }
}

#[derive(Debug)]
pub enum CellKind {
    User {
        text: String,
        delivery: Delivery,
    },
    Image {
        name: String,
        caption: String,
        /// Display path for the no-thumbnail fallback.
        path: String,
        /// Encoded raster bytes (PNG for the kitty thumbnail path).
        data: Arc<[u8]>,
        id: u32,
        delivery: Delivery,
    },
    Reasoning {
        text: String,
        done: bool,
        started: Instant,
        seconds: Option<f32>,
        agent: Option<String>,
    },
    Assistant {
        text: String,
        done: bool,
        model: Option<String>,
        agent: Option<String>,
    },
    Tool {
        name: String,
        title: String,
        result: String,
        /// None while running.
        ok: Option<bool>,
        error: Option<String>,
        agent: Option<String>,
    },
    Injected {
        source: String,
        preview: String,
    },
    /// Latest standard ACP plan snapshot. Replaced in place as statuses move.
    Plan {
        summary: String,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    /// A notice rendered through the markdown pipeline (headings, tables,
    /// inline code) instead of plain wrapped text — used by `/keys`.
    MarkdownNotice {
        text: String,
    },
    /// The startup splash: an ASCII wordmark, the project URL and the launch
    /// facts, painted once at the top of a fresh run. `art` rows keep their
    /// own spacing, so they are painted verbatim (never re-wrapped); every
    /// fact is a `(label, value)` pair, so the label keeps the caption tone
    /// and the value stays the readable one.
    Banner {
        art: Vec<String>,
        url: String,
        facts: Vec<(String, String)>,
    },
}

pub struct Cell {
    pub kind: CellKind,
    pub expanded: bool,
}

impl Cell {
    fn new(kind: CellKind) -> Self {
        Cell {
            kind,
            expanded: false,
        }
    }
}

/// One block of a client-owned prompt, as the timeline echoes it
/// ([`Transcript::prompt_echo_cells`]).
pub enum EchoBlock<'a> {
    Text(&'a str),
    Image {
        name: &'a str,
        path: &'a str,
        data: &'a Arc<[u8]>,
    },
}

/// What an edit did to the timeline's cell indices: the run `[start, end)` now
/// holds `len` cells. Every index at or after `end` moves by the difference,
/// and every index inside the run is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellShift {
    start: usize,
    end: usize,
    len: usize,
}

impl CellShift {
    /// An edit that changed nothing.
    fn none() -> Self {
        CellShift {
            start: 0,
            end: 0,
            len: 0,
        }
    }

    /// Where an index the caller held lives now; `None` when the edit removed
    /// it.
    pub fn map(self, index: usize) -> Option<usize> {
        if index < self.start {
            Some(index)
        } else if index < self.end {
            None
        } else {
            Some(index + self.len - (self.end - self.start))
        }
    }
}

/// One image the UI should render as a kitty-graphics thumbnail, positioned
/// by its reserved line range (`line` is relative to the transcript layout;
/// the chat pane adds its banner offset).
pub struct ImageShot {
    pub id: u32,
    pub line: usize,
    pub rows: usize,
    pub cols: usize,
    pub data: Arc<[u8]>,
}

/// One user prompt in a rendered transcript: the transcript cell index and
/// the half-open line span `[line, end)` its bubble occupies (the leading
/// blank separator row is not counted). The composer cap's `↥` button walks
/// these from the newest one back and flashes the jumped span.
#[derive(Clone, Copy, Debug)]
pub struct UserPromptLine {
    pub cell: usize,
    pub line: usize,
    pub end: usize,
}

/// A rendered transcript: styled lines plus, for each line, the index of the
/// transcript cell that owns it (only tool cells report ownership, so mouse
/// clicks can toggle a specific tool preview).
pub struct TranscriptLayout {
    pub lines: Vec<Line<'static>>,
    pub owners: Vec<Option<usize>>,
    pub images: Vec<ImageShot>,
    /// Every laid-out user prompt with its first and last text line.
    pub users: Vec<UserPromptLine>,
}

#[derive(Default, Clone, Copy)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub reasoning: u64,
}

/// Native transcript timing/step facts used by session state and tests. The
/// LLM usage/timing details surface through `/status` (plus the
/// Client-side `acpSessionStats` service for plugins) — never in a persistent
/// row of its own: the composer frame ends at the input box.
#[derive(Default, Clone, Copy)]
pub struct SessionStats {
    pub turns: u64,
    pub steps: u64,
    /// Total turn wall time (turn/start → turn/end).
    pub turn_millis: u64,
    /// Time spent inside tool calls (tool/call → tool/result).
    pub tool_millis: u64,
    /// Sum of per-turn time-to-first-token, plus how many turns were sampled.
    pub ttft_total_millis: u64,
    pub ttft_count: u64,
}

pub struct Transcript {
    pub cells: Vec<Cell>,
    root_session: String,
    open_assistant: HashMap<String, usize>,
    open_reasoning: HashMap<String, usize>,
    tools: HashMap<String, usize>,
    /// Arguments streamed for a tool call, accumulated per call id until its
    /// result lands. Only used to keep a live card's title current.
    tool_args: HashMap<String, String>,
    agents: HashMap<String, String>,
    agent_seq: usize,
    image_seq: u32,
    plan_cell: Option<usize>,
    pub usage: UsageTotals,
    pub stats: SessionStats,
    turn_started: Option<Instant>,
    tool_started: HashMap<String, Instant>,
    ttft_pending: bool,
    pub last_finish: Option<String>,
    /// Provenance-reported model of the last assembled assistant message —
    /// the ground truth of what actually answered.
    pub last_model: Option<String>,
    /// How tool calls are painted; `ctrl+o` cycles it.
    pub tool_output: ToolOutput,
    /// Interface language for this timeline's own chrome: the notice wording,
    /// the tool-card footer, the reasoning heading. Payload text stays
    /// authored by its owner; `App` keeps this in sync with `/lang`.
    locale: Locale,
}

impl Transcript {
    pub fn new(root_session: String) -> Self {
        Transcript {
            cells: Vec::new(),
            root_session,
            open_assistant: HashMap::new(),
            open_reasoning: HashMap::new(),
            tools: HashMap::new(),
            tool_args: HashMap::new(),
            agents: HashMap::new(),
            agent_seq: 0,
            image_seq: 0,
            plan_cell: None,
            usage: UsageTotals::default(),
            stats: SessionStats::default(),
            turn_started: None,
            tool_started: HashMap::new(),
            ttft_pending: false,
            last_finish: None,
            last_model: None,
            tool_output: ToolOutput::default(),
            locale: Locale::default(),
        }
    }

    /// Follow the interface language (`/lang`). The root timeline and each
    /// subagent's are told separately — they are separate `Transcript`s.
    pub fn set_locale(&mut self, locale: Locale) {
        self.locale = locale;
    }

    pub fn set_root_session(&mut self, session: String) {
        self.root_session = session;
        self.open_assistant.clear();
        self.open_reasoning.clear();
        self.tools.clear();
        self.tool_args.clear();
        self.agents.clear();
        self.plan_cell = None;
        self.usage = UsageTotals::default();
        self.stats = SessionStats::default();
        self.turn_started = None;
        self.tool_started.clear();
        self.ttft_pending = false;
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.open_assistant.clear();
        self.open_reasoning.clear();
        self.tools.clear();
        self.tool_args.clear();
        self.plan_cell = None;
        self.last_finish = None;
    }

    /// Drop cells from the timeline and report what moved, so callers holding
    /// cell indices of their own can remap them.
    ///
    /// The transcript's own open-card indices are remapped here, so a stream or
    /// a plan cell below the cut keeps pointing at its cell.
    pub fn remove_cells(&mut self, cells: &[usize]) -> CellShift {
        self.replace_cells(cells, Vec::new()).0
    }

    /// Replace a run of client-owned cells — a queued prompt's echo — in place,
    /// and report the new indices next to the shift.
    ///
    /// A repaint keeps the echo where it was in the timeline instead of moving
    /// it to the tail: the queue's FIFO order is what the transcript shows, and
    /// the `⌥↑` list numbers its rows the same way. `run` is the contiguous,
    /// ascending chain the caller painted (`cells.len()` is how many cells the
    /// run has, `cells[0]` where it starts); a run past the end of the timeline
    /// does nothing.
    pub fn replace_cells(&mut self, run: &[usize], cells: Vec<Cell>) -> (CellShift, Vec<usize>) {
        let Some(&start) = run.first() else {
            return (CellShift::none(), Vec::new());
        };
        if start > self.cells.len() {
            return (CellShift::none(), Vec::new());
        }
        let end = (start + run.len()).min(self.cells.len());
        let shift = CellShift {
            start,
            end,
            len: cells.len(),
        };
        let painted = (start..start + cells.len()).collect();
        self.cells.splice(start..end, cells);
        self.remap_indices(|index| shift.map(index));
        (shift, painted)
    }

    /// The echo cells for one client-owned prompt: a `User` bubble per text
    /// block, a thumbnail per image, all still marked queued. Image ids come
    /// from the transcript's own sequence, so a repaint can never collide with
    /// a thumbnail already on screen.
    pub fn prompt_echo_cells(&mut self, blocks: &[EchoBlock<'_>], delivery: Delivery) -> Vec<Cell> {
        blocks
            .iter()
            .map(|block| match block {
                EchoBlock::Text(text) => Cell::new(CellKind::User {
                    text: (*text).to_string(),
                    delivery,
                }),
                EchoBlock::Image { name, path, data } => {
                    self.image_seq += 1;
                    Cell::new(CellKind::Image {
                        name: (*name).to_string(),
                        caption: String::new(),
                        path: (*path).to_string(),
                        data: Arc::clone(data),
                        id: self.image_seq,
                        delivery,
                    })
                }
            })
            .collect()
    }

    /// Move the transcript's own bookkeeping — the open assistant/reasoning
    /// cells, the tool cards, the plan digest — onto a new numbering. An index
    /// the edit removed drops its entry.
    fn remap_indices(&mut self, map: impl Fn(usize) -> Option<usize>) {
        let remap = |entries: HashMap<String, usize>| -> HashMap<String, usize> {
            entries
                .into_iter()
                .filter_map(|(key, index)| map(index).map(|at| (key, at)))
                .collect()
        };
        self.open_assistant = remap(std::mem::take(&mut self.open_assistant));
        self.open_reasoning = remap(std::mem::take(&mut self.open_reasoning));
        self.tools = remap(std::mem::take(&mut self.tools));
        self.plan_cell = self.plan_cell.and_then(&map);
    }

    fn agent_label(&self, session: &str) -> Option<String> {
        if session == self.root_session || session.is_empty() {
            None
        } else {
            self.agents
                .get(session)
                .cloned()
                .or_else(|| Some("agent".into()))
        }
    }

    pub fn push_user(&mut self, text: String, delivery: Delivery) {
        self.cells
            .push(Cell::new(CellKind::User { text, delivery }));
    }

    /// Record a user-sent image (bytes kept for the kitty thumbnail path).
    pub fn push_image(
        &mut self,
        name: String,
        caption: String,
        path: String,
        data: Arc<[u8]>,
        delivery: Delivery,
    ) {
        self.image_seq += 1;
        let id = self.image_seq;
        self.cells.push(Cell::new(CellKind::Image {
            name,
            caption,
            path,
            data,
            id,
            delivery,
        }));
    }

    pub fn push_notice(&mut self, level: NoticeLevel, text: String) {
        self.cells.push(Cell::new(CellKind::Notice { level, text }));
    }

    /// Push a notice rendered through the markdown pipeline (tables,
    /// headings, inline code). No `· ` prefix and no manual wrapping: the
    /// markdown renderer owns line layout and truncation.
    pub fn push_markdown(&mut self, text: String) {
        self.cells
            .push(Cell::new(CellKind::MarkdownNotice { text }));
    }

    /// Push the startup splash: preformatted `art` rows, the URL that follows
    /// the mark, and the launch facts under it (`App::push_banner`).
    pub fn push_banner(&mut self, art: Vec<String>, url: String, facts: Vec<(String, String)>) {
        self.cells
            .push(Cell::new(CellKind::Banner { art, url, facts }));
    }

    /// Mark one client-owned queued prompt group as delivered.
    pub fn mark_prompt_delivered(&mut self, cells: &[usize]) {
        self.set_delivery(cells, Delivery::Delivered);
    }

    /// Mark a Send Now bubble as a client-owned FIFO item after the agent
    /// rejects concurrent `session/prompt` delivery.
    pub fn mark_prompt_queued(&mut self, cells: &[usize]) {
        self.set_delivery(cells, Delivery::Queued);
    }

    /// Mark a queued bubble as handed to the running turn: it is pending
    /// steering until the agent appends it at a step boundary.
    pub fn mark_prompt_steering(&mut self, cells: &[usize]) {
        self.set_delivery(cells, Delivery::Steering);
    }

    fn set_delivery(&mut self, cells: &[usize], delivery: Delivery) {
        for &index in cells {
            let Some(cell) = self.cells.get_mut(index) else {
                continue;
            };
            match &mut cell.kind {
                CellKind::User { delivery: at, .. } | CellKind::Image { delivery: at, .. } => {
                    *at = delivery
                }
                _ => {}
            }
        }
    }

    /// Backchat `session.tool_cancelled` on user stop: in-flight tools and
    /// streams stop spinning before `session/prompt` unwinds.
    pub fn cancel_open_work(&mut self) {
        for cell in &mut self.cells {
            match &mut cell.kind {
                CellKind::Tool {
                    ok, result, error, ..
                } if ok.is_none() => {
                    *ok = Some(false);
                    *error = Some("cancelled".into());
                    if result.is_empty() {
                        *result = "cancelled".into();
                    }
                }
                CellKind::Reasoning {
                    done,
                    started,
                    seconds,
                    ..
                } if !*done => {
                    *done = true;
                    *seconds = Some(started.elapsed().as_secs_f32());
                }
                CellKind::Assistant { done, .. } if !*done => {
                    *done = true;
                }
                _ => {}
            }
        }
        self.tools.clear();
        self.tool_args.clear();
        self.tool_started.clear();
        self.open_assistant.clear();
        self.open_reasoning.clear();
        self.last_finish = Some("cancelled".into());
    }

    /// Create the tool card for a call, or refresh the name/title of the card a
    /// streamed call already created. The call id owns the card, so a delta, the
    /// completed arguments, and execution start all land on one cell.
    fn upsert_tool_card(&mut self, session: &str, call_id: &str, name: String, title: String) {
        if let Some(&idx) = self.tools.get(call_id) {
            if let Some(cell) = self.cells.get_mut(idx) {
                if let CellKind::Tool {
                    name: current,
                    title: current_title,
                    ..
                } = &mut cell.kind
                {
                    *current = name;
                    *current_title = title;
                }
            }
            return;
        }
        let agent = self.agent_label(session);
        self.cells.push(Cell::new(CellKind::Tool {
            name,
            title,
            result: String::new(),
            ok: None,
            error: None,
            agent,
        }));
        self.tools.insert(call_id.to_string(), self.cells.len() - 1);
    }

    fn close_open(&mut self, session: &str) {
        if let Some(idx) = self.open_assistant.remove(session) {
            if let Some(cell) = self.cells.get_mut(idx) {
                if let CellKind::Assistant { done, text, .. } = &mut cell.kind {
                    *done = true;
                    if text.trim().is_empty() {
                        // Leave the empty husk; render skips empty finished cells.
                    }
                }
            }
        }
        if let Some(idx) = self.open_reasoning.remove(session) {
            if let Some(cell) = self.cells.get_mut(idx) {
                if let CellKind::Reasoning {
                    done,
                    started,
                    seconds,
                    ..
                } = &mut cell.kind
                {
                    *done = true;
                    *seconds = Some(started.elapsed().as_secs_f32());
                }
            }
        }
    }

    fn close_all_open(&mut self) {
        let sessions: Vec<String> = self
            .open_assistant
            .keys()
            .chain(self.open_reasoning.keys())
            .cloned()
            .collect();
        for s in sessions {
            self.close_open(&s);
        }
    }

    /// Record the per-turn first-token latency when the first visible or
    /// reasoning delta for the root session arrives.
    fn note_first_token(&mut self, session: &str) {
        if session != self.root_session || !self.ttft_pending {
            return;
        }
        if let Some(t0) = self.turn_started {
            self.stats.ttft_total_millis += t0.elapsed().as_millis() as u64;
            self.stats.ttft_count += 1;
        }
        self.ttft_pending = false;
    }

    pub fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::SessionStatus { session, running } => {
                if !running && session == self.root_session {
                    self.close_all_open();
                }
            }
            UiEvent::TurnStart { session, .. } => {
                if session == self.root_session {
                    self.stats.turns += 1;
                    self.turn_started = Some(Instant::now());
                    self.ttft_pending = true;
                }
            }
            UiEvent::TurnEnd { session, kind } => {
                self.close_open(&session);
                if session == self.root_session {
                    self.last_finish = Some(kind.clone());
                    if let Some(t0) = self.turn_started.take() {
                        self.stats.turn_millis += t0.elapsed().as_millis() as u64;
                    }
                    if kind != "completed" && kind != "interrupted" {
                        let level = if kind == "error" {
                            NoticeLevel::Error
                        } else {
                            NoticeLevel::Warn
                        };
                        self.push_notice(
                            level,
                            format!("{} · {kind}", self.locale.tr("turn ended", "回合结束")),
                        );
                    }
                }
            }
            UiEvent::TextDelta { session, text } => {
                self.note_first_token(&session);
                // Reasoning for this step is over once visible text streams.
                if let Some(idx) = self.open_reasoning.remove(&session) {
                    if let Some(cell) = self.cells.get_mut(idx) {
                        if let CellKind::Reasoning {
                            done,
                            started,
                            seconds,
                            ..
                        } = &mut cell.kind
                        {
                            *done = true;
                            *seconds = Some(started.elapsed().as_secs_f32());
                        }
                    }
                }
                let idx = match self.open_assistant.get(&session) {
                    Some(&idx) => idx,
                    None => {
                        let agent = self.agent_label(&session);
                        self.cells.push(Cell::new(CellKind::Assistant {
                            text: String::new(),
                            done: false,
                            model: None,
                            agent,
                        }));
                        let idx = self.cells.len() - 1;
                        self.open_assistant.insert(session.clone(), idx);
                        idx
                    }
                };
                if let Some(cell) = self.cells.get_mut(idx) {
                    if let CellKind::Assistant { text: buf, .. } = &mut cell.kind {
                        buf.push_str(&text);
                    }
                }
            }
            UiEvent::ReasoningDelta { session, text } => {
                self.note_first_token(&session);
                let idx = match self.open_reasoning.get(&session) {
                    Some(&idx) => idx,
                    None => {
                        let agent = self.agent_label(&session);
                        self.cells.push(Cell::new(CellKind::Reasoning {
                            text: String::new(),
                            done: false,
                            started: Instant::now(),
                            seconds: None,
                            agent,
                        }));
                        let idx = self.cells.len() - 1;
                        self.open_reasoning.insert(session.clone(), idx);
                        idx
                    }
                };
                if let Some(cell) = self.cells.get_mut(idx) {
                    if let CellKind::Reasoning { text: buf, .. } = &mut cell.kind {
                        buf.push_str(&text);
                    }
                }
            }
            UiEvent::AssistantFinal {
                session,
                text,
                model,
            } => {
                if session == self.root_session {
                    self.stats.steps += 1;
                }
                if model.is_some() && session == self.root_session {
                    self.last_model = model.clone();
                }
                let idx = self.open_assistant.remove(&session);
                match idx {
                    Some(idx) => {
                        if let Some(cell) = self.cells.get_mut(idx) {
                            if let CellKind::Assistant {
                                text: buf,
                                done,
                                model: m,
                                ..
                            } = &mut cell.kind
                            {
                                if !text.is_empty() {
                                    *buf = text;
                                }
                                *done = true;
                                *m = model;
                            }
                        }
                    }
                    None => {
                        if !text.is_empty() {
                            let agent = self.agent_label(&session);
                            self.cells.push(Cell::new(CellKind::Assistant {
                                text,
                                done: true,
                                model,
                                agent,
                            }));
                        }
                    }
                }
            }
            UiEvent::ToolCall {
                session,
                call_id,
                name,
                arguments,
            } => {
                // Streamed calls arrive before execution; replayed calls arrive
                // already complete. Either way the card is created here and
                // completed by ToolStarted/ToolResult, never duplicated.
                self.tool_args.insert(call_id.clone(), arguments.clone());
                let title = tool_title(&name, &arguments);
                self.upsert_tool_card(&session, &call_id, name, title);
            }
            UiEvent::ToolCallDelta {
                session,
                call_id,
                name,
                delta,
            } => {
                let title = {
                    let partial = self.tool_args.entry(call_id.clone()).or_default();
                    partial.push_str(&delta);
                    stream_title(&name, partial)
                };
                self.upsert_tool_card(&session, &call_id, name, title);
            }
            UiEvent::ToolStarted {
                session,
                call_id,
                name,
            } => {
                if session == self.root_session {
                    self.tool_started.insert(call_id.clone(), Instant::now());
                }
                self.close_open(&session);
                let title = self
                    .tool_args
                    .get(&call_id)
                    .map_or_else(String::new, |arguments| tool_title(&name, arguments));
                self.upsert_tool_card(&session, &call_id, name, title);
            }
            UiEvent::ToolResult {
                session,
                call_id,
                is_error,
                text,
                error,
            } => {
                if session == self.root_session {
                    if let Some(t0) = self.tool_started.remove(&call_id) {
                        self.stats.tool_millis += t0.elapsed().as_millis() as u64;
                    }
                }
                self.tool_args.remove(&call_id);
                let idx = self.tools.remove(&call_id);
                match idx {
                    Some(idx) => {
                        if let Some(cell) = self.cells.get_mut(idx) {
                            if let CellKind::Tool {
                                result,
                                ok,
                                error: e,
                                ..
                            } = &mut cell.kind
                            {
                                *result = text;
                                *ok = Some(!is_error);
                                *e = error;
                            }
                        }
                    }
                    None => {
                        let agent = self.agent_label(&session);
                        self.cells.push(Cell::new(CellKind::Tool {
                            name: "tool".into(),
                            title: call_id,
                            result: text,
                            ok: Some(!is_error),
                            error,
                            agent,
                        }));
                    }
                }
            }
            UiEvent::Usage {
                input,
                output,
                cached,
                reasoning,
                ..
            } => {
                self.usage.input += input;
                self.usage.output += output;
                self.usage.cached += cached;
                self.usage.reasoning += reasoning;
            }
            UiEvent::UserInjected {
                source, preview, ..
            } => {
                self.cells
                    .push(Cell::new(CellKind::Injected { source, preview }));
            }
            UiEvent::UserMessage { text, .. } => {
                self.push_user(text, Delivery::Delivered);
            }
            UiEvent::SessionTitle { title, .. } => {
                self.push_notice(
                    NoticeLevel::Info,
                    format!("{} · {title}", self.locale.tr("session", "会话")),
                );
            }
            UiEvent::Plan { summary, .. } => {
                if let Some(idx) = self.plan_cell {
                    if let Some(Cell {
                        kind: CellKind::Plan { summary: current },
                        ..
                    }) = self.cells.get_mut(idx)
                    {
                        *current = summary.clone();
                    } else {
                        self.plan_cell = None;
                    }
                }
                if self.plan_cell.is_none() {
                    self.cells.push(Cell::new(CellKind::Plan { summary }));
                    self.plan_cell = Some(self.cells.len() - 1);
                }
            }
            UiEvent::SubagentStarted { child, .. } => {
                self.agent_seq += 1;
                let label = format!(
                    "{} {}",
                    self.locale.tr("subagent", "子代理"),
                    self.agent_seq
                );
                self.agents.insert(child, label.clone());
                self.push_notice(
                    NoticeLevel::Info,
                    format!("⛭ {label} {}", self.locale.tr("started", "已启动")),
                );
            }
            UiEvent::SubagentFinished { child } => {
                let label = self
                    .agents
                    .get(&child)
                    .cloned()
                    .unwrap_or_else(|| self.locale.tr("subagent", "子代理").to_string());
                self.push_notice(
                    NoticeLevel::Info,
                    format!("⛭ {label} {}", self.locale.tr("finished", "已结束")),
                );
            }
            UiEvent::PlanMode { active, .. } => {
                self.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "⌁ {} {}",
                        self.locale.tr("plan mode", "计划模式"),
                        self.locale.tr(
                            if active { "on" } else { "off" },
                            if active { "开" } else { "关" }
                        )
                    ),
                );
            }
            // Permission facts (`file policy` · `approval policy` ·
            // `permission`) never print: the app folds them into the composer's
            // meta-row chips and stops them there, so a switch is confirmed by
            // the chip instead of echoing itself into the timeline. A granted
            // or denied approval below still speaks for itself.
            UiEvent::SandboxMode { .. }
            | UiEvent::ApprovalPolicy { .. }
            | UiEvent::PermissionPreset { .. } => {}
            UiEvent::ApprovalAsked { tool, reason, .. } => {
                let why = reason.map(|r| format!(" · {r}")).unwrap_or_default();
                self.push_notice(
                    NoticeLevel::Warn,
                    format!(
                        "⚖ {} · {tool}{why}",
                        self.locale.tr("approval requested", "请求审批")
                    ),
                );
            }
            UiEvent::ApprovalDecided { outcome, .. } => {
                let level = if outcome.contains("reject") || outcome.contains("denied") {
                    NoticeLevel::Warn
                } else {
                    NoticeLevel::Info
                };
                self.push_notice(
                    level,
                    format!("⚖ {} · {outcome}", self.locale.tr("approval", "审批")),
                );
            }
        }
    }

    /// Whether this cell is a message the running turn has not taken yet.
    fn pending_steering(&self, index: usize) -> bool {
        matches!(
            self.cells.get(index).map(|cell| &cell.kind),
            Some(CellKind::User {
                delivery: Delivery::Steering,
                ..
            }) | Some(CellKind::Image {
                delivery: Delivery::Steering,
                ..
            })
        )
    }

    /// Whether a cell paints no row at all, so a summary-mode run of tool calls
    /// on either side of it is still one block. Mirrors the `continue`s in
    /// [`Transcript::layout`]; keep the two in step.
    fn paints_nothing(&self, index: usize) -> bool {
        match self.cells.get(index).map(|cell| &cell.kind) {
            Some(CellKind::Assistant { text, .. }) => text.trim().is_empty(),
            Some(CellKind::Plan { summary }) => summary.is_empty(),
            Some(CellKind::MarkdownNotice { text }) => text.trim().is_empty(),
            _ => false,
        }
    }

    /// Is any assistant/reasoning cell currently streaming?
    pub fn streaming(&self) -> bool {
        !self.open_assistant.is_empty() || !self.open_reasoning.is_empty()
    }

    /// Render every cell to wrapped, styled lines for `width` columns.
    #[allow(dead_code)] // kept for tests; the UI uses `layout` for ownership
    pub fn lines(&self, theme: &Theme, width: u16, spinner: char) -> Vec<Line<'static>> {
        self.layout(theme, width, spinner, false).lines
    }

    /// Render every cell to wrapped, styled lines, plus per-line ownership so
    /// the UI can route mouse clicks to a specific tool preview (and only tool
    /// lines claim ownership). `thumbs` reserves blank
    /// lines for kitty-graphics image thumbnails and reports their placements.
    pub fn layout(
        &self,
        theme: &Theme,
        width: u16,
        spinner: char,
        thumbs: bool,
    ) -> TranscriptLayout {
        let width = width.max(8) as usize;
        let mut out: Vec<Line> = Vec::new();
        let mut owners: Vec<Option<usize>> = Vec::new();
        let mut images: Vec<ImageShot> = Vec::new();
        let mut users: Vec<UserPromptLine> = Vec::new();
        // Pending steering renders as a tail section, the way deepseek-harness
        // shows `data-pending-steering` rows: a message the running turn has not
        // picked up yet belongs *after* the live output, not in the middle of
        // the step it is about to interrupt. The cell indices (and everything
        // keyed by them) stay put; only the paint order changes, so the bubble
        // drops back into its chronological slot the moment it is admitted.
        let pending: Vec<usize> = (0..self.cells.len())
            .filter(|ci| self.pending_steering(*ci))
            .collect();
        let order: Vec<usize> = (0..self.cells.len())
            .filter(|ci| !self.pending_steering(*ci))
            .chain(pending.iter().copied())
            .collect();
        // Summary mode paints one line per call, so consecutive calls share a
        // single header counting them. Cells that paint nothing (an empty
        // assistant husk between two steps) do not break a run: the two steps
        // read as one block, which is what a long agent run looks like. The run
        // is drawn as a tree — every call hangs off the head's dot, and the last
        // one closes the branch — so `run_tails` names the closers.
        let mut run_heads: HashMap<usize, ToolRun> = HashMap::new();
        let mut run_tails: HashSet<usize> = HashSet::new();
        if self.tool_output == ToolOutput::Summary {
            let visible: Vec<usize> = order
                .iter()
                .copied()
                .filter(|ci| !self.paints_nothing(*ci))
                .collect();
            let mut at = 0;
            while at < visible.len() {
                if !matches!(self.cells[visible[at]].kind, CellKind::Tool { .. }) {
                    at += 1;
                    continue;
                }
                let head = visible[at];
                let mut run = ToolRun::default();
                while at < visible.len() {
                    let CellKind::Tool { name, ok, .. } = &self.cells[visible[at]].kind else {
                        break;
                    };
                    run.calls += 1;
                    if name == "bash" {
                        run.commands += 1;
                    }
                    if *ok == Some(false) {
                        run.failed += 1;
                    }
                    at += 1;
                }
                run_heads.insert(head, run);
                run_tails.insert(visible[at - 1]);
            }
        }
        let mut in_tail = false;
        for ci in order {
            let cell = &self.cells[ci];
            if !in_tail && self.pending_steering(ci) {
                in_tail = true;
                emit(&mut out, &mut owners, Line::default(), None);
                emit(
                    &mut out,
                    &mut owners,
                    Line::from(Span::styled(
                        format!(
                            "⏳ {}",
                            self.locale
                                .tr(
                                    "{n} steering · lands at the next agent step",
                                    "{n} 条待插话 · Agent 下一步生效",
                                )
                                .replace("{n}", &pending.len().to_string())
                        ),
                        Style::default().fg(Delivery::Steering.tint(theme)),
                    )),
                    None,
                );
            }
            let expanded = cell.expanded || self.tool_output.expand_all();
            match &cell.kind {
                CellKind::User { text, delivery } => {
                    emit(&mut out, &mut owners, Line::default(), None);
                    // Web UI fidelity: the user bubble uses --dsw-specific-bubble.
                    let line = out.len();
                    // "❯ " prefix (2 cells) plus the bubble's own " {l} "
                    // padding (2 cells) leaves `width - 4` for the text; the
                    // old subtraction of 2 painted every full line two cells
                    // past the pane and clipped its tail.
                    for (i, l) in wrap(text, width.saturating_sub(4)).into_iter().enumerate() {
                        let mut spans = vec![
                            Span::styled(
                                if i == 0 { "❯ " } else { "  " }.to_string(),
                                Style::default()
                                    .fg(theme.brand)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!(" {l} "),
                                Style::default()
                                    .fg(theme.bubble_fg)
                                    .bg(theme.bubble_bg)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ];
                        if i == 0 {
                            if let Some(marker) = delivery.marker() {
                                spans.push(Span::styled(
                                    format!("  {marker}"),
                                    Style::default().fg(delivery.tint(theme)),
                                ));
                            }
                        }
                        emit(&mut out, &mut owners, Line::from(spans), None);
                    }
                    users.push(UserPromptLine {
                        cell: ci,
                        line,
                        end: out.len(),
                    });
                }
                CellKind::Image {
                    name,
                    caption,
                    path,
                    data,
                    id,
                    delivery,
                    ..
                } => {
                    emit(&mut out, &mut owners, Line::default(), None);
                    let label = if caption.is_empty() {
                        format!("🖼 {name}")
                    } else {
                        format!("🖼 {name} · {caption}")
                    };
                    let mut spans = vec![
                        Span::styled(
                            "❯ ".to_string(),
                            Style::default()
                                .fg(theme.brand)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(" {label} "),
                            Style::default()
                                .fg(theme.bubble_fg)
                                .bg(theme.bubble_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(marker) = delivery.marker() {
                        spans.push(Span::styled(
                            format!("  {marker}"),
                            Style::default().fg(delivery.tint(theme)),
                        ));
                    }
                    emit(&mut out, &mut owners, Line::from(spans), None);

                    // Thumbnail (PNG only — clipboard screenshots are PNG;
                    // other formats fall back to the path line below).
                    let is_png = data.starts_with(b"\x89PNG\r\n\x1a\n");
                    if thumbs && is_png {
                        if let Some((w, h)) = crate::pet::image_dims(data) {
                            let cols = (width.saturating_sub(2)).min(THUMB_COLS);
                            let rows = thumb_rows(cols, w, h);
                            let line = out.len();
                            for _ in 0..rows {
                                emit(&mut out, &mut owners, Line::default(), None);
                            }
                            images.push(ImageShot {
                                id: *id,
                                line,
                                rows,
                                cols,
                                data: data.clone(),
                            });
                        } else {
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(Span::styled(
                                    format!("  {path}"),
                                    Style::default().fg(theme.caption),
                                )),
                                None,
                            );
                        }
                    } else {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(Span::styled(
                                format!("  {path}"),
                                Style::default().fg(theme.caption),
                            )),
                            None,
                        );
                    }
                }
                CellKind::Reasoning {
                    text,
                    done,
                    started,
                    seconds,
                    agent,
                } => {
                    emit(&mut out, &mut owners, Line::default(), None);
                    let head_style = Style::default().fg(theme.caption);
                    let body_style = Style::default()
                        .fg(theme.fg_tertiary)
                        .add_modifier(Modifier::ITALIC);
                    // A reasoning stream can open with an empty/whitespace
                    // delta. The heading is enough for that frame; an empty
                    // body must not make its height jump from one row to two.
                    let body = text.trim();
                    let lines = if body.is_empty() {
                        Vec::new()
                    } else {
                        wrap(body, width.saturating_sub(2))
                    };
                    let n = lines.len();
                    if *done {
                        let dur = seconds.map(|s| format!(" · {s:.1}s")).unwrap_or_default();
                        let agent = agent_prefix(agent);
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(Span::styled(
                                reasoning_heading(self.locale, &agent, &dur, n),
                                head_style,
                            )),
                            None,
                        );
                        if expanded {
                            for l in lines {
                                emit(
                                    &mut out,
                                    &mut owners,
                                    Line::from(vec![Span::raw("  "), Span::styled(l, body_style)]),
                                    None,
                                );
                            }
                        }
                    } else {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(Span::styled(
                                format!(
                                    "✻ {}thinking… {}s",
                                    agent_prefix(agent),
                                    started.elapsed().as_secs()
                                ),
                                Style::default().fg(theme.brand_soft),
                            )),
                            None,
                        );
                        let tail: Vec<_> = if expanded {
                            lines
                        } else {
                            lines
                                .into_iter()
                                .rev()
                                .take(COLLAPSED_REASONING_PREVIEW)
                                .rev()
                                .collect()
                        };
                        for l in tail {
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(vec![Span::raw("  "), Span::styled(l, body_style)]),
                                None,
                            );
                        }
                    }
                }
                CellKind::Assistant {
                    text, done, agent, ..
                } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    emit(&mut out, &mut owners, Line::default(), None);
                    if let Some(a) = agent {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(Span::styled(
                                format!("⛭ {a}"),
                                Style::default().fg(theme.caption),
                            )),
                            None,
                        );
                    }
                    let mut lines = crate::markdown::render(text, theme, width);
                    if !done {
                        // streaming cursor
                        if let Some(last) = lines.last_mut() {
                            last.spans.push(Span::styled(
                                "▍".to_string(),
                                Style::default().fg(theme.brand),
                            ));
                        }
                    }
                    for l in lines {
                        emit(&mut out, &mut owners, l, None);
                    }
                }
                CellKind::Tool {
                    name,
                    title,
                    result,
                    ok,
                    error,
                    agent,
                } => {
                    if self.tool_output == ToolOutput::Summary {
                        // The run head carries the counts for every call under
                        // it; the calls themselves are one owned line each, so
                        // a click still expands the one you clicked.
                        if let Some(run) = run_heads.get(&ci) {
                            emit(&mut out, &mut owners, Line::default(), None);
                            // A big dot marks the block head, so the run reads as
                            // one unit against the lines of calls under it. The
                            // head sits at `fg_tertiary` — a step below the calls
                            // themselves, a step above the hint greys.
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(vec![
                                    Span::styled(
                                        "● ".to_string(),
                                        Style::default().fg(theme.fg_tertiary),
                                    ),
                                    Span::styled(
                                        tool_run_headline(self.locale, *run),
                                        Style::default().fg(theme.fg_tertiary),
                                    ),
                                ]),
                                None,
                            );
                        }
                        let (line, rows) = tool_summary_rows(
                            theme,
                            width,
                            spinner,
                            &SummaryCall {
                                name,
                                title,
                                result,
                                ok: *ok,
                                error: error.as_deref(),
                                agent: agent.as_deref(),
                            },
                            expanded,
                            run_tails.contains(&ci),
                        );
                        emit(&mut out, &mut owners, line, Some(ci));
                        let bar = match ok {
                            Some(false) => theme.err,
                            _ => theme.border,
                        };
                        for row in rows {
                            // The body hangs off the branch's own column, so the
                            // text under a call lines up with its label.
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(vec![
                                    Span::styled("│  ".to_string(), Style::default().fg(bar)),
                                    Span::styled(row, Style::default().fg(theme.fg_tertiary)),
                                ]),
                                Some(ci),
                            );
                        }
                        continue;
                    }
                    emit(&mut out, &mut owners, Line::default(), None);
                    let body = result.trim_end();
                    let all: Vec<String> = if body.is_empty() {
                        Vec::new()
                    } else {
                        body.lines()
                            .flat_map(|raw| wrap(raw, width.saturating_sub(2)))
                            .collect()
                    };
                    let total = all.len();
                    let has_more = total > TOOL_VIEWPORT;
                    let (glyph, gstyle) = match ok {
                        None => (spinner, Style::default().fg(theme.brand)),
                        Some(true) => ('⏺', Style::default().fg(theme.ok_soft())),
                        Some(false) => ('⏺', Style::default().fg(theme.err)),
                    };
                    let mut spans = vec![
                        Span::styled(format!("{glyph} "), gstyle),
                        Span::styled(
                            name.clone(),
                            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(a) = agent {
                        spans.push(Span::styled(
                            format!(" · {a}"),
                            Style::default().fg(theme.caption),
                        ));
                    }
                    if !title.is_empty() {
                        let prefix_w: usize = spans.iter().map(|s| s.content.width()).sum();
                        let chevron_w = if has_more { 2 } else { 0 };
                        let budget = width.saturating_sub(prefix_w + 2 + chevron_w);
                        spans.push(Span::styled(
                            format!("  {}", clamp_str(title, budget)),
                            Style::default().fg(theme.fg_tertiary),
                        ));
                    }
                    if has_more {
                        spans.push(Span::styled(
                            if expanded { " ▾" } else { " ▸" }.to_string(),
                            Style::default().fg(theme.caption),
                        ));
                    }
                    emit(&mut out, &mut owners, Line::from(spans), Some(ci));

                    if let Some(err) = error {
                        for l in wrap(err, width.saturating_sub(2)) {
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(vec![
                                    Span::styled("│ ".to_string(), Style::default().fg(theme.err)),
                                    Span::styled(l, Style::default().fg(theme.err)),
                                ]),
                                Some(ci),
                            );
                        }
                    }

                    if !body.is_empty() {
                        let bar_color = match ok {
                            Some(false) => theme.err,
                            _ => theme.border,
                        };
                        if expanded || total <= TOOL_VIEWPORT {
                            for l in all {
                                emit(
                                    &mut out,
                                    &mut owners,
                                    Line::from(vec![
                                        Span::styled(
                                            "│ ".to_string(),
                                            Style::default().fg(bar_color),
                                        ),
                                        Span::styled(l, Style::default().fg(theme.fg_tertiary)),
                                    ]),
                                    Some(ci),
                                );
                            }
                        } else {
                            let offset = total - TOOL_VIEWPORT;
                            for l in &all[offset..offset + TOOL_VIEWPORT] {
                                emit(
                                    &mut out,
                                    &mut owners,
                                    Line::from(vec![
                                        Span::styled(
                                            "│ ".to_string(),
                                            Style::default().fg(bar_color),
                                        ),
                                        Span::styled(
                                            l.clone(),
                                            Style::default().fg(theme.fg_tertiary),
                                        ),
                                    ]),
                                    Some(ci),
                                );
                            }
                            emit(
                                &mut out,
                                &mut owners,
                                Line::from(vec![
                                    Span::styled("│ ".to_string(), Style::default().fg(bar_color)),
                                    Span::styled(
                                        if self.locale == Locale::Zh {
                                            format!(
                                                "最后 {}/{} 行 · 点击展开",
                                                TOOL_VIEWPORT, total
                                            )
                                        } else {
                                            format!(
                                                "last {}/{} lines · click to expand",
                                                TOOL_VIEWPORT, total
                                            )
                                        },
                                        Style::default().fg(theme.caption),
                                    ),
                                ]),
                                Some(ci),
                            );
                        }
                    }
                }
                CellKind::Injected { source, preview } => {
                    emit(
                        &mut out,
                        &mut owners,
                        Line::from(vec![
                            Span::styled(
                                "◦ context".to_string(),
                                Style::default().fg(theme.caption),
                            ),
                            Span::styled(
                                format!(" · {source} · "),
                                Style::default().fg(theme.caption),
                            ),
                            Span::styled(
                                clamp_str(preview, width.saturating_sub(source.len() + 14)),
                                Style::default().fg(theme.fg_tertiary),
                            ),
                        ]),
                        None,
                    );
                }
                CellKind::Plan { summary } => {
                    if summary.is_empty() {
                        continue;
                    }
                    let plan = self.locale.tr("plan", "计划");
                    for l in wrap(&format!("{plan} · {summary}"), width.saturating_sub(2)) {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(vec![
                                Span::styled("· ".to_string(), Style::default().fg(theme.caption)),
                                Span::styled(l, Style::default().fg(theme.caption)),
                            ]),
                            None,
                        );
                    }
                }
                CellKind::Notice { level, text } => {
                    let color = match level {
                        NoticeLevel::Info => theme.caption,
                        NoticeLevel::Warn => theme.warn_soft(),
                        NoticeLevel::Error => theme.err,
                    };
                    for l in wrap(text, width.saturating_sub(2)) {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(vec![
                                Span::styled("· ".to_string(), Style::default().fg(color)),
                                Span::styled(l, Style::default().fg(color)),
                            ]),
                            None,
                        );
                    }
                }
                CellKind::MarkdownNotice { text } => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    emit(&mut out, &mut owners, Line::default(), None);
                    for l in crate::markdown::render(text, theme, width) {
                        emit(&mut out, &mut owners, l, None);
                    }
                }
                CellKind::Banner { art, url, facts } => {
                    emit(&mut out, &mut owners, Line::default(), None);
                    // The mark, the URL and the launch facts share one
                    // centered column so the splash reads as a block; rows
                    // wider than the pane (a tiny terminal) fall back to the
                    // left margin.
                    let fact_line =
                        |(label, value): &(String, String)| format!("{label} · {value}");
                    let widest = art
                        .iter()
                        .map(|row| row.width())
                        .chain(std::iter::once(url.width()))
                        .chain(facts.iter().map(|fact| fact_line(fact).width()))
                        .max()
                        .unwrap_or(0);
                    let pad = " ".repeat(width.saturating_sub(widest) / 2);
                    let pad_w = pad.width();
                    for row in art {
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(vec![
                                Span::raw(pad.clone()),
                                Span::styled(row.clone(), Style::default().fg(theme.brand_soft)),
                            ]),
                            None,
                        );
                    }
                    emit(&mut out, &mut owners, Line::default(), None);
                    emit(
                        &mut out,
                        &mut owners,
                        Line::from(vec![
                            Span::raw(pad.clone()),
                            Span::styled(
                                url.clone(),
                                Style::default()
                                    .fg(theme.brand_soft)
                                    .add_modifier(Modifier::UNDERLINED),
                            ),
                        ]),
                        None,
                    );
                    if facts.is_empty() {
                        continue;
                    }
                    emit(&mut out, &mut owners, Line::default(), None);
                    for (label, value) in facts {
                        // The label is the fixed part; the value (a path, a
                        // model id) takes whatever the pane has left and gets
                        // the ellipsis.
                        let prefix = format!("{label} · ");
                        let budget = width.saturating_sub(pad_w + prefix.width());
                        emit(
                            &mut out,
                            &mut owners,
                            Line::from(vec![
                                Span::raw(pad.clone()),
                                Span::styled(prefix, Style::default().fg(theme.caption)),
                                Span::styled(
                                    clamp_str(value, budget),
                                    Style::default().fg(theme.fg_secondary),
                                ),
                            ]),
                            None,
                        );
                    }
                }
            }
        }
        TranscriptLayout {
            lines: out,
            owners,
            images,
            users,
        }
    }
}

fn emit(
    out: &mut Vec<Line<'static>>,
    owners: &mut Vec<Option<usize>>,
    line: Line<'static>,
    owner: Option<usize>,
) {
    out.push(line);
    owners.push(owner);
}

/// Thumbnail height in rows for `cols` columns, preserving the image's pixel
/// aspect under the same 2:1 cell aspect the composer pet assumes.
fn thumb_rows(cols: usize, w: u32, h: u32) -> usize {
    if w == 0 || h == 0 || cols == 0 {
        return 6;
    }
    let rows = (cols as f64 * h as f64 / (2.0 * w as f64)).round() as usize;
    rows.clamp(2, 12)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The reasoning divider, in the interface language: `✻ thought · 12 lines`
/// in English, `✻ 思考 · 12 行` in Chinese (no plural to agree with).
fn reasoning_heading(locale: Locale, agent: &str, dur: &str, lines: usize) -> String {
    match locale {
        Locale::En => format!("✻ {agent}thought{dur} · {lines} line{}", plural(lines)),
        Locale::Zh => format!("✻ {agent}思考{dur} · {lines} 行"),
    }
}

fn agent_prefix(agent: &Option<String>) -> String {
    match agent {
        Some(a) => format!("{a} "),
        None => String::new(),
    }
}

/// What one summary-mode run of consecutive tool calls contains: the count the
/// block header reports, and the two subsets worth breaking out.
#[derive(Clone, Copy, Default)]
struct ToolRun {
    calls: usize,
    /// Calls to `bash` — the shell work, which is what a run usually is.
    commands: usize,
    failed: usize,
}

/// The header over a summary-mode run: `13 tool calls · 13 commands · 1 failed`.
/// The `commands` and `failed` segments are dropped when they are zero, so a run
/// of reads does not claim to have run no commands.
fn tool_run_headline(locale: Locale, run: ToolRun) -> String {
    let mut parts = vec![match (locale, run.calls) {
        (Locale::En, 1) => "1 tool call".to_string(),
        (Locale::En, n) => format!("{n} tool calls"),
        (Locale::Zh, n) => format!("{n} 次工具调用"),
    }];
    if run.commands > 0 {
        parts.push(match (locale, run.commands) {
            (Locale::En, 1) => "1 command".to_string(),
            (Locale::En, n) => format!("{n} commands"),
            (Locale::Zh, n) => format!("{n} 条命令"),
        });
    }
    if run.failed > 0 {
        parts.push(match (locale, run.failed) {
            (Locale::En, 1) => "1 failed".to_string(),
            (Locale::En, n) => format!("{n} failed"),
            (Locale::Zh, n) => format!("{n} 条失败"),
        });
    }
    parts.join(" · ")
}

/// The label a summary-mode call leads with: a verb for the tools we can name
/// (`Ran ls -la`), the tool's own name for the rest. The verb stays English in
/// every locale: it opens a line whose payload is a command or a path, and the
/// un-named tools beside it already show their English tool names (`bash`,
/// `read`). The chrome around the block — the count header, the click hints —
/// still follows the interface language.
fn tool_summary_label(name: &str, title: &str) -> String {
    let verb = match name {
        "bash" => "Ran",
        "read" => "Read",
        "write" => "Wrote",
        "edit" | "str_replace_editor" => "Edited",
        "web_search" => "Searched",
        _ => "",
    };
    match (verb.is_empty(), title.is_empty()) {
        (true, true) => name.to_string(),
        (true, false) => format!("{name} {title}"),
        (false, true) => verb.to_string(),
        (false, false) => format!("{verb} {title}"),
    }
}

/// The facts one summary-mode line is built from: a tool cell's label parts and
/// its outcome.
struct SummaryCall<'a> {
    name: &'a str,
    title: &'a str,
    result: &'a str,
    ok: Option<bool>,
    error: Option<&'a str>,
    agent: Option<&'a str>,
}

/// One call in summary mode: the glyph, the label (or a failure's own first
/// line), the `▸`/`▾` hint — plus the body rows to paint underneath when the
/// call is expanded. A failure leads with its message rather than a verb: the
/// text already names what went wrong, and only its first line shows until the
/// call is clicked.
fn tool_summary_rows(
    theme: &Theme,
    width: usize,
    spinner: char,
    call: &SummaryCall<'_>,
    expanded: bool,
    closes_run: bool,
) -> (Line<'static>, Vec<String>) {
    let SummaryCall {
        name,
        title,
        result,
        ok,
        error,
        agent,
    } = *call;
    let failed = ok == Some(false);
    // The run is a tree under the head's dot: every call hangs off `├─`, and
    // the last one closes the branch with `└─`. A finished call wears no other
    // bullet: the line is the command itself, which is as compact as a tool call
    // gets. A call still running keeps its spinner — that is the only thing
    // saying it has not landed yet.
    let branch = if closes_run { "└─ " } else { "├─ " };
    let lead = match ok {
        None => Some((spinner, Style::default().fg(theme.brand))),
        Some(_) => None,
    };
    let lead_cells = branch.width() + if lead.is_some() { 2 } else { 0 };
    // The call's own text column, which the expanded body shares (the caller
    // hangs the body off a `│` in the same column as the branch).
    let inner = width.saturating_sub(branch.width());
    let (label, rows) = if failed {
        let message = error
            .map(str::trim_end)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| result.trim_end());
        let rows = wrap_rows(message, inner);
        let head = rows
            .first()
            .cloned()
            // English like the verbs: this heads the same kind of line.
            .unwrap_or_else(|| "failed".to_string());
        (head, rows)
    } else {
        (
            tool_summary_label(name, title),
            wrap_rows(result.trim_end(), inner),
        )
    };
    // The hint only appears when there is something behind the click — a body
    // for a success, a second line for a failure.
    let hidden = if failed {
        rows.len() > 1
    } else {
        !rows.is_empty()
    };
    let tail = match (hidden, expanded) {
        (false, _) => "",
        (true, true) => " ▾",
        (true, false) => " ▸",
    };
    let note = agent.map_or_else(String::new, |a| format!(" · {a}"));
    let mut spans = Vec::new();
    spans.push(Span::styled(
        branch,
        Style::default().fg(if failed { theme.err } else { theme.border }),
    ));
    if let Some((glyph, style)) = lead {
        spans.push(Span::styled(format!("{glyph} "), style));
    }
    // The call reads at `fg_secondary`: a shade down from prose so a wall of
    // calls recedes, but well clear of the footer/hint greys.
    spans.push(Span::styled(
        clamp_str(
            &label,
            width.saturating_sub(lead_cells + note.width() + tail.width()),
        ),
        Style::default().fg(if failed {
            theme.err
        } else {
            theme.fg_secondary
        }),
    ));
    if !note.is_empty() {
        spans.push(Span::styled(note, Style::default().fg(theme.caption)));
    }
    if !tail.is_empty() {
        spans.push(Span::styled(
            tail.to_string(),
            Style::default().fg(theme.caption),
        ));
    }
    (Line::from(spans), if expanded { rows } else { Vec::new() })
}

/// Wrap `text` to `inner` columns, one entry per painted row.
fn wrap_rows(text: &str, inner: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().flat_map(|raw| wrap(raw, inner)).collect()
}

/// Human title for a tool call, parsed from its raw JSON argument string.
pub fn tool_title(name: &str, arguments: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(arguments).ok();
    if let Some(v) = parsed {
        match name {
            "bash" => {
                if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                    return one_line(cmd);
                }
            }
            "str_replace_editor" => {
                let cmd = v.get("command").and_then(|c| c.as_str()).unwrap_or("");
                let path = v.get("path").and_then(|c| c.as_str()).unwrap_or("");
                if !cmd.is_empty() || !path.is_empty() {
                    return one_line(&format!("{cmd} {path}"));
                }
            }
            // The file tools title themselves by the path they touched: the raw
            // argument object is noise a card — and even more a one-line summary
            // — should not carry.
            "read" | "write" | "edit" => {
                if let Some(path) = v.get("file_path").and_then(|c| c.as_str()) {
                    return one_line(path);
                }
            }
            "web_search" => {
                if let Some(queries) = v.get("queries").and_then(|q| q.as_array()) {
                    let joined: Vec<&str> = queries
                        .iter()
                        .filter_map(|q| q.as_str())
                        .filter(|q| !q.is_empty())
                        .collect();
                    if !joined.is_empty() {
                        return one_line(&joined.join(", "));
                    }
                }
            }
            _ => {}
        }
        // A call with no arguments takes no title: `{}` is noise the tool name
        // already answers for (`get_goal`), and the result below carries the
        // content.
        if v.as_object().is_some_and(|object| object.is_empty()) {
            return String::new();
        }
        // generic: compact json
        return one_line(&v.to_string());
    }
    one_line(arguments)
}

fn one_line(s: &str) -> String {
    clamp_str(&s.replace('\n', " ⏎ "), 120)
}

/// Title for a tool call whose JSON arguments are still streaming.
///
/// Only tools whose finished title is a single string field can be titled
/// mid-stream (`bash` shows its command as the model writes it); every other
/// tool keeps its card untitled until the complete arguments arrive, exactly
/// like a call that streams no deltas at all.
fn stream_title(name: &str, partial: &str) -> String {
    let key = match name {
        "bash" => "command",
        _ => return String::new(),
    };
    partial_json_string(partial, key).map_or_else(String::new, |text| one_line(&text))
}

/// Best-effort decode of a top-level string value from a partial JSON object.
/// An unterminated value returns the text decoded so far.
fn partial_json_string(json: &str, key: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let mut index = 0;
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b'"' => {
                let (text, next) = json_string(bytes, index + 1);
                let colon = skip_ws(bytes, next);
                if depth == 1 && text == key && bytes.get(colon) == Some(&b':') {
                    let value = skip_ws(bytes, colon + 1);
                    return (bytes.get(value) == Some(&b'"'))
                        .then(|| json_string(bytes, value + 1).0);
                }
                index = next.max(index + 1);
            }
            _ => index += 1,
        }
    }
    None
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    let hex = bytes.get(at..at + 4)?;
    u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
}

/// Decode a JSON string body starting after its opening quote. Returns the text
/// decoded so far and the index after the closing quote (or the end of input).
fn json_string(bytes: &[u8], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return (out, index + 1),
            b'\\' => {
                let Some(&escape) = bytes.get(index + 1) else {
                    break;
                };
                let simple = match escape {
                    b'"' => Some('"'),
                    b'\\' => Some('\\'),
                    b'/' => Some('/'),
                    b'b' => Some('\u{8}'),
                    b'f' => Some('\u{c}'),
                    b'n' => Some('\n'),
                    b'r' => Some('\r'),
                    b't' => Some('\t'),
                    _ => None,
                };
                if let Some(ch) = simple {
                    out.push(ch);
                    index += 2;
                    continue;
                }
                if escape != b'u' {
                    out.push(escape as char);
                    index += 2;
                    continue;
                }
                let Some(code) = hex4(bytes, index + 2) else {
                    break;
                };
                index += 6;
                let code = if (0xD800..0xDC00).contains(&code) {
                    // A high surrogate only decodes with its low half.
                    if bytes.get(index) != Some(&b'\\') || bytes.get(index + 1) != Some(&b'u') {
                        break;
                    }
                    let Some(low) = hex4(bytes, index + 2) else {
                        break;
                    };
                    if !(0xDC00..0xE000).contains(&low) {
                        out.push('\u{FFFD}');
                        continue;
                    }
                    index += 6;
                    0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
                } else {
                    code
                };
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            byte if byte.is_ascii() => {
                out.push(byte as char);
                index += 1;
            }
            _ => {
                // Copy one UTF-8 scalar; a truncated tail ends the scan.
                let tail = &bytes[index..bytes.len().min(index + 4)];
                let Ok(text) = std::str::from_utf8(tail) else {
                    break;
                };
                let Some(ch) = text.chars().next() else {
                    break;
                };
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    (out, bytes.len())
}

pub(crate) fn clamp_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let s = expand_tabs(s);
    if s.width() <= max {
        return s;
    }
    let mut out = String::new();
    let mut w = 0usize;
    let budget = max.saturating_sub(1);
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Expand tabs to spaces at 8-column stops so wrap/layout width matches
/// what a terminal actually paints (a raw `\t` is otherwise width 0/1).
fn expand_tabs(s: &str) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let pad = 8 - (col % 8);
            out.push_str(&" ".repeat(pad));
            col += pad;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

/// Greedy display-width wrap with word-boundary preference; never panics on
/// CJK/emoji. Tabs expand to spaces before wrapping so a tab-indented source
/// line cannot paint past `width` (issue #5).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let raw = expand_tabs(raw);
        let mut line = String::new();
        let mut w = 0usize;
        for ch in raw.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > width && !line.is_empty() {
                // Prefer breaking at the last space when it is not too early.
                match line.rfind(' ') {
                    Some(bidx) if line[..bidx].width() >= width / 2 => {
                        let tail = line[bidx + 1..].to_string();
                        line.truncate(bidx);
                        out.push(std::mem::replace(&mut line, tail));
                        w = line.width();
                    }
                    _ => {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                }
            }
            if w + cw > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            line.push(ch);
            w += cw;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(session: &str) -> Transcript {
        Transcript::new(session.to_string())
    }

    /// Dropping cells shifts every index above the cut, so the transcript's own
    /// open-card and plan indices have to move with them — a tool card below a
    /// withdrawn echo must stay its own, and its click must still find it.
    #[test]
    fn removing_cells_remaps_the_transcripts_own_indices() {
        let mut tr = t("s");
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "1 of 2 done".into(),
            todos: Vec::new(),
            active: None,
            active_extra: 0,
            completed: 1,
            total: 2,
        });
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        assert_eq!(tr.plan_cell, Some(0));
        assert_eq!(tr.tools.get("c1"), Some(&1));
        tr.push_user("withdrawn".into(), Delivery::Queued);
        tr.push_user("still queued".into(), Delivery::Queued);

        // Cell 2 (the withdrawn echo) leaves the timeline.
        let shift = tr.remove_cells(&[2]);

        assert_eq!(shift.map(0), Some(0));
        assert_eq!(shift.map(1), Some(1));
        assert_eq!(shift.map(2), None, "the withdrawn echo is gone");
        assert_eq!(shift.map(3), Some(2));
        assert_eq!(tr.cells.len(), 3);
        assert_eq!(tr.plan_cell, Some(0), "the plan cell keeps its index");
        assert_eq!(tr.tools.get("c1"), Some(&1), "so does the tool card");
        assert!(matches!(tr.cells[0].kind, CellKind::Plan { .. }));
        assert!(matches!(tr.cells[1].kind, CellKind::Tool { .. }));
        assert!(
            matches!(&tr.cells[2].kind, CellKind::User { text, delivery: Delivery::Queued } if text == "still queued"),
            "the surviving echo moved into the gap"
        );
    }

    /// An edited echo is repainted in place: the empty prompt run becoming an
    /// image and a text keeps them where the queue's order put them, and the
    /// tool card under the run follows the shift.
    #[test]
    fn replacing_a_run_repaints_it_in_place_and_shifts_the_tail() {
        let mut tr = t("s");
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        let run: Vec<usize> = (tr.cells.len()..tr.cells.len() + 1).collect();
        let echo = tr.prompt_echo_cells(&[EchoBlock::Text("before the edit")], Delivery::Queued);
        tr.cells.extend(echo);
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c2".into(),
            name: "bash".into(),
            arguments: r#"{"command":"pwd"}"#.into(),
        });
        assert_eq!(tr.tools.get("c1"), Some(&0));
        assert_eq!(tr.tools.get("c2"), Some(&2));

        // One text bubble becomes an image + text: one cell more than before.
        let image_bytes: Arc<[u8]> = Arc::from(vec![0u8; 4]);
        let echo = tr.prompt_echo_cells(
            &[
                EchoBlock::Image {
                    name: "shot.png",
                    path: "/tmp/shot.png",
                    data: &image_bytes,
                },
                EchoBlock::Text("after the edit"),
            ],
            Delivery::Queued,
        );
        let (shift, painted) = tr.replace_cells(&run, echo);

        assert_eq!(painted, [1, 2]);
        assert_eq!(shift.map(0), Some(0), "the first card never moved");
        assert_eq!(shift.map(1), None, "the old bubble is gone");
        assert_eq!(shift.map(2), Some(3), "the second card follows the growth");
        assert_eq!(tr.tools.get("c2"), Some(&3));
        assert!(matches!(
            &tr.cells[1].kind,
            CellKind::Image { name, delivery: Delivery::Queued, .. } if name == "shot.png"
        ));
        assert!(
            matches!(&tr.cells[2].kind, CellKind::User { text, delivery: Delivery::Queued } if text == "after the edit"),
            "the repainted run sits where the old one did"
        );
    }

    /// Timeline chrome follows the interface language: the notices, the plan
    /// divider, the reasoning heading and the tool-card footer all speak the
    /// locale `App` sets, while payload text (a session title) stays as it
    /// arrived.
    #[test]
    fn timeline_chrome_follows_the_interface_language() {
        let mut tr = t("s");
        tr.set_locale(Locale::Zh);
        tr.apply(UiEvent::SessionTitle {
            session: "s".into(),
            title: "fix the tests".into(),
        });
        tr.apply(UiEvent::SubagentStarted {
            parent: "s".into(),
            child: "c1".into(),
            label: None,
        });
        tr.apply(UiEvent::PlanMode {
            session: "s".into(),
            active: true,
        });
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "locate fn main".into(),
            todos: Vec::new(),
            active: None,
            active_extra: 0,
            completed: 1,
            total: 1,
        });
        tr.apply(UiEvent::TurnEnd {
            session: "s".into(),
            kind: "error".into(),
        });
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{\"command\":\"ls\"}".into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "l1\nl2\nl3\nl4\nl5\nl6".into(),
            error: None,
        });
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "".into(),
        });

        let notices: Vec<String> = tr
            .cells
            .iter()
            .filter_map(|c| match &c.kind {
                CellKind::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices.contains(&"会话 · fix the tests".to_string()),
            "zh session notice, payload title untouched: {notices:?}"
        );
        assert!(
            notices.contains(&"⛭ 子代理 1 已启动".to_string()),
            "{notices:?}"
        );
        assert!(
            notices.contains(&"⌁ 计划模式 开".to_string()),
            "{notices:?}"
        );
        assert!(
            notices.contains(&"回合结束 · error".to_string()),
            "{notices:?}"
        );

        // The card footer is what this test is about: the default is summary.
        tr.tool_output = ToolOutput::Preview;
        let theme = Theme::dark();
        let rendered: String = tr
            .lines(&theme, 40, ' ')
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(rendered.contains("计划 · locate fn main"), "{rendered}");
        assert!(rendered.contains("最后 4/6 行 · 点击展开"), "{rendered}");

        // The same timeline in English keeps the words it had before.
        tr.set_locale(Locale::En);
        let rendered: String = tr
            .lines(&theme, 40, ' ')
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(rendered.contains("plan · locate fn main"), "{rendered}");
        assert!(
            rendered.contains("last 4/6 lines · click to expand"),
            "{rendered}"
        );
    }

    /// The `↥` jump walks this index: every user prompt with its bubble rows
    /// (the leading blank separator row is excluded), in transcript order.
    #[test]
    fn layout_indexes_every_user_prompt_span() {
        let mut tr = t("s");
        tr.push_user("first prompt".into(), Delivery::Delivered);
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "an answer".into(),
        });
        tr.push_user("second\nprompt".into(), Delivery::Delivered);

        let layout = tr.layout(&Theme::dark(), 40, '⠋', false);
        assert_eq!(layout.users.len(), 2);
        let [first, second] = [layout.users[0], layout.users[1]];
        let row = |line: usize| -> String {
            layout.lines[line]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert!(
            row(first.line).contains("first prompt"),
            "{}",
            row(first.line)
        );
        assert!(
            !row(first.line - 1).contains("first prompt"),
            "the separator row is not part of the span"
        );
        assert!(row(second.line).contains("second"));
        assert!(row(second.end - 1).contains("prompt"), "wrapped tail row");
        assert!(
            second.line >= first.end,
            "spans never overlap: {first:?} {second:?}"
        );
    }

    #[test]
    fn cancel_open_work_stops_running_tools() {
        let mut tr = t("s");
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        tr.cancel_open_work();
        match &tr.cells[0].kind {
            CellKind::Tool { ok, error, .. } => {
                assert_eq!(*ok, Some(false));
                assert_eq!(error.as_deref(), Some("cancelled"));
            }
            other => panic!("expected tool, got {other:?}"),
        }
        assert_eq!(tr.last_finish.as_deref(), Some("cancelled"));
    }

    #[test]
    fn streaming_text_appends_and_finalizes() {
        let mut tr = t("s");
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "Hello ".into(),
        });
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "world".into(),
        });
        assert!(tr.streaming());
        tr.apply(UiEvent::AssistantFinal {
            session: "s".into(),
            text: "Hello world!".into(),
            model: Some("m".into()),
        });
        assert!(!tr.streaming());
        match &tr.cells[0].kind {
            CellKind::Assistant { text, done, .. } => {
                assert_eq!(text, "Hello world!");
                assert!(done);
            }
            other => panic!("unexpected cell {other:?}"),
        }
    }

    /// A call that takes no arguments titles nothing: `{}` would sit after the
    /// tool name saying nothing, and the answer belongs in the card's body.
    #[test]
    fn no_argument_tool_calls_title_nothing() {
        assert_eq!(tool_title("get_goal", "{}"), "");
        assert_eq!(tool_title("get_goal", " {} "), "");
        assert_eq!(tool_title("get_goal", ""), "");
        // Anything an argument-free call does carry still shows.
        assert_eq!(tool_title("get_goal", r#"{"note":"x"}"#), r#"{"note":"x"}"#);
    }

    #[test]
    fn tool_call_pairs_with_result() {
        let mut tr = t("s");
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "a\nb".into(),
            error: None,
        });
        match &tr.cells[0].kind {
            CellKind::Tool {
                name, title, ok, ..
            } => {
                assert_eq!(name, "bash");
                assert_eq!(title, "ls");
                assert_eq!(*ok, Some(true));
            }
            other => panic!("unexpected cell {other:?}"),
        }
    }

    /// The live path: the model streams the arguments, the card titles itself
    /// from the partial JSON, execution starts on the same cell, and the result
    /// completes it — one card, no duplicate from `ToolStarted`.
    #[test]
    fn streamed_tool_arguments_title_one_card_until_the_result() {
        let mut tr = t("s");
        for delta in [
            r#"{"command":"sed -n 1,80p /home/kk"#,
            r#"/project/abylab/crates/abycore/src/persist/log.rs","description":"Read log"}"#,
        ] {
            tr.apply(UiEvent::ToolCallDelta {
                session: "s".into(),
                call_id: "c1".into(),
                name: "bash".into(),
                delta: delta.into(),
            });
        }
        assert_eq!(tr.cells.len(), 1, "deltas keep one card");
        let CellKind::Tool { title, ok, .. } = &tr.cells[0].kind else {
            panic!("expected tool")
        };
        assert_eq!(
            title,
            "sed -n 1,80p /home/kk/project/abylab/crates/abycore/src/persist/log.rs"
        );
        assert_eq!(*ok, None, "a streamed call is not running yet");

        tr.apply(UiEvent::ToolStarted {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
        });
        assert_eq!(tr.cells.len(), 1, "execution start must not add a card");
        assert!(tr.tool_started.contains_key("c1"), "the timer starts here");

        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "line\n".into(),
            error: None,
        });
        let CellKind::Tool {
            title, result, ok, ..
        } = &tr.cells[0].kind
        else {
            panic!("expected tool")
        };
        assert_eq!(
            title,
            "sed -n 1,80p /home/kk/project/abylab/crates/abycore/src/persist/log.rs"
        );
        assert_eq!(result, "line\n");
        assert_eq!(*ok, Some(true));
        assert!(tr.tool_args.is_empty(), "stream state is released");
    }

    /// Non-bash tools (and calls that stream no deltas at all) stay untitled
    /// until their complete arguments arrive, then execution reuses the card.
    #[test]
    fn complete_arguments_title_a_card_created_by_the_stream() {
        let mut tr = t("s");
        tr.apply(UiEvent::ToolCallDelta {
            session: "s".into(),
            call_id: "c1".into(),
            name: "read".into(),
            delta: r#"{"file_path":"/tmp/a"#.into(),
        });
        let CellKind::Tool { title, .. } = &tr.cells[0].kind else {
            panic!("expected tool")
        };
        assert_eq!(title, "", "no partial title for read");
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"file_path":"/tmp/a.txt"}"#.into(),
        });
        assert_eq!(tr.cells.len(), 1);
        let CellKind::Tool { title, .. } = &tr.cells[0].kind else {
            panic!("expected tool")
        };
        assert_eq!(
            title, "/tmp/a.txt",
            "a file tool titles itself by the path it touched"
        );

        // A call whose stream produced no delta (e.g. a resumed pending call)
        // still gets its card from ToolStarted, as before.
        tr.apply(UiEvent::ToolStarted {
            session: "s".into(),
            call_id: "c2".into(),
            name: "bash".into(),
        });
        assert_eq!(tr.cells.len(), 2);
        let CellKind::Tool { title, .. } = &tr.cells[1].kind else {
            panic!("expected tool")
        };
        assert_eq!(title, "");
    }

    #[test]
    fn partial_json_string_decodes_escapes_and_ignores_nested_keys() {
        assert_eq!(
            partial_json_string(r#"{"command":"a\"b\\c\nd"#, "command").as_deref(),
            Some("a\"b\\c\nd")
        );
        assert_eq!(
            partial_json_string(r#"{"x":{"command":"nested"},"command":"top"}"#, "command")
                .as_deref(),
            Some("top")
        );
        assert_eq!(
            partial_json_string(r#"{"command":123}"#, "command"),
            None,
            "a non-string value has no partial title"
        );
        assert_eq!(partial_json_string(r#"{"other":"x"}"#, "command"), None);
        assert_eq!(
            partial_json_string(r#"{"command":"end"}"#, "command").as_deref(),
            Some("end")
        );
        assert_eq!(
            partial_json_string(r#"{"command":"😀 中"#, "command").as_deref(),
            Some("😀 中")
        );
        assert_eq!(
            partial_json_string(r#"{"command":"\u4e2d\u6587"}"#, "command").as_deref(),
            Some("中文")
        );
    }

    #[test]
    fn reasoning_closes_when_text_starts() {
        let mut tr = t("s");
        tr.apply(UiEvent::ReasoningDelta {
            session: "s".into(),
            text: "plan".into(),
        });
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "answer".into(),
        });
        match &tr.cells[0].kind {
            CellKind::Reasoning { done, .. } => assert!(done),
            other => panic!("unexpected cell {other:?}"),
        }
    }

    #[test]
    fn usage_accumulates() {
        let mut tr = t("s");
        for _ in 0..2 {
            tr.apply(UiEvent::Usage {
                session: "s".into(),
                input: 10,
                output: 5,
                cached: 3,
                reasoning: 1,
            });
        }
        assert_eq!(tr.usage.input, 20);
        assert_eq!(tr.usage.output, 10);
        assert_eq!(tr.usage.cached, 6);
    }

    fn line_width(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    #[test]
    fn wrap_handles_cjk() {
        let lines = wrap("深度求索深度求索", 8);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn wrap_expands_tabs_and_never_exceeds_width() {
        use unicode_width::UnicodeWidthStr;
        let cases = [
            format!("\t{}", "x".repeat(120)),
            format!("{}\t{}", "x".repeat(10), "y".repeat(110)),
            "\t".repeat(20) + &"code();".repeat(15),
            "a".repeat(200),
            "深度求索".repeat(40),
        ];
        for text in cases {
            for width in [40usize, 80, 120] {
                for line in wrap(&text, width) {
                    let w = UnicodeWidthStr::width(line.as_str());
                    assert!(
                        w <= width,
                        "wrapped line {line:?} is {w} cols, budget {width}"
                    );
                    assert!(
                        !line.contains('\t'),
                        "tabs must expand to spaces so display width matches: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn write_tool_tab_indented_body_fits_terminal_width() {
        // GitHub issue #5: writing a file whose lines contain tabs rendered
        // at 128 cols in a 120-col terminal (tab counted as 0, displayed as 8,
        // plus the "│ " gutter).
        let mut tr = t("s");
        let body = format!("\t{}", "x".repeat(120));
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "str_replace_editor".into(),
            arguments: r#"{"command":"create","path":"src/lib.rs"}"#.into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: body,
            error: Some("x".repeat(140)),
        });
        let theme = Theme::dark();
        let width = 120u16;
        let layout = tr.layout(&theme, width, ' ', false);
        for (i, line) in layout.lines.iter().enumerate() {
            let w = line_width(line);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                w <= width as usize,
                "line {i} width {w} > {width}: {text:?}"
            );
            assert!(!text.contains('\t'), "tab leaked into layout line {i}");
        }
    }

    #[test]
    fn render_smoke() {
        let mut tr = t("s");
        tr.push_user("hi".into(), Delivery::Delivered);
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "yo".into(),
        });
        let theme = Theme::dark();
        let lines = tr.lines(&theme, 40, '⠋');
        assert!(lines.len() >= 3);
    }

    /// A wrapped user bubble keeps its `❯ ` prefix and `" {line} "` padding
    /// inside the pane: wrapping used to reserve room for the prefix only, so
    /// every full line lost its tail to the clip.
    #[test]
    fn a_wrapped_user_bubble_never_exceeds_the_viewport_width() {
        let mut tr = t("s");
        tr.push_user("x".repeat(200), Delivery::Delivered);
        let theme = Theme::dark();
        let width = 40u16;
        let layout = tr.layout(&theme, width, ' ', false);
        for (i, line) in layout.lines.iter().enumerate() {
            let w = line_width(line);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                w <= width as usize,
                "line {i} width {w} > {width}: {text:?}"
            );
        }
    }

    #[test]
    fn acp_load_replay_paints_user_title_and_plan() {
        // Pinned English: this test is about the replay painting, and the
        // chrome it reads (the session notice) follows the locale.
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.apply(UiEvent::UserMessage {
            session: "s".into(),
            text: "hello from load".into(),
        });
        tr.apply(UiEvent::SessionTitle {
            session: "s".into(),
            title: "fix the tests".into(),
        });
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "locate fn main [completed]".into(),
            todos: Vec::new(),
            active: None,
            active_extra: 0,
            completed: 1,
            total: 1,
        });
        match &tr.cells[0].kind {
            CellKind::User { text, delivery } => {
                assert_eq!(text, "hello from load");
                assert_eq!(*delivery, Delivery::Delivered);
            }
            other => panic!("expected user cell, got {other:?}"),
        }
        let notices: Vec<&str> = tr
            .cells
            .iter()
            .filter_map(|c| match &c.kind {
                CellKind::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(notices.contains(&"session · fix the tests"));
        let plans: Vec<&str> = tr
            .cells
            .iter()
            .filter_map(|c| match &c.kind {
                CellKind::Plan { summary } => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(plans, ["locate fn main [completed]"]);
    }

    #[test]
    fn plan_snapshots_replace_the_existing_plan_in_place() {
        let mut tr = t("s");
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "inspect [in_progress]".into(),
            todos: Vec::new(),
            active: Some("inspect".into()),
            active_extra: 0,
            completed: 0,
            total: 1,
        });
        let cells_after_first = tr.cells.len();

        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "inspect [completed] · test [in_progress]".into(),
            todos: Vec::new(),
            active: Some("test".into()),
            active_extra: 0,
            completed: 1,
            total: 2,
        });

        assert_eq!(
            tr.cells.len(),
            cells_after_first,
            "a full plan snapshot updates one client-side view instead of appending chat history"
        );
        let rendered = tr
            .lines(&Theme::dark(), 80, '⠋')
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(!rendered.contains("inspect [in_progress]"));
        assert!(rendered.contains("inspect [completed] · test [in_progress]"));
    }

    #[test]
    fn empty_plan_snapshot_hides_the_existing_plan() {
        let mut tr = t("s");
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: "inspect [in_progress]".into(),
            todos: Vec::new(),
            active: Some("inspect".into()),
            active_extra: 0,
            completed: 0,
            total: 1,
        });
        tr.apply(UiEvent::Plan {
            session: "s".into(),
            summary: String::new(),
            todos: Vec::new(),
            active: None,
            active_extra: 0,
            completed: 0,
            total: 0,
        });

        let rendered = tr
            .lines(&Theme::dark(), 80, '⠋')
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(!rendered.contains("plan ·"));
        assert!(!rendered.contains("inspect"));
    }

    #[test]
    fn tool_preview_shows_a_fixed_tail_and_expand_all_opens_it() {
        let text = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8";
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        // The cards are one chord away from the default summary mode.
        tr.tool_output = ToolOutput::Preview;
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: text.into(),
            error: None,
        });
        let theme = Theme::dark();
        let plain = |lines: &[Line]| -> String {
            lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect()
        };

        // Default: a fixed 4-line tail preview (l5..l8) + expand hint.
        let lines = tr.lines(&theme, 40, ' ');
        let p = plain(&lines);
        assert!(
            p.contains("last 4/8 lines"),
            "footer describes the fixed tail preview: {p}"
        );
        assert!(
            !p.contains("wheel"),
            "tool preview has no inner scroll: {p}"
        );
        assert!(!p.contains("l4"), "l4 above the tail window is hidden: {p}");
        assert!(p.contains("l8"), "tail line visible: {p}");

        // full (ctrl+o) opens the whole body and drops the footer.
        tr.tool_output = ToolOutput::Full;
        let lines = tr.lines(&theme, 40, ' ');
        let p = plain(&lines);
        assert!(p.contains("l1"), "full mode shows the top: {p}");
        assert!(
            !p.contains("click to expand"),
            "no footer when expanded: {p}"
        );
    }

    /// Flatten a rendered transcript to one string for `contains` assertions.
    fn flat(lines: &[Line]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    }

    /// One finished tool call, as the UI sees it: a card created by `ToolCall`
    /// and closed by `ToolResult`. `error` marks the call failed and is what the
    /// summary line leads with.
    fn call(
        tr: &mut Transcript,
        id: &str,
        name: &str,
        arguments: &str,
        text: &str,
        error: Option<&str>,
    ) {
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: id.into(),
            is_error: error.is_some(),
            text: text.into(),
            error: error.map(str::to_string),
        });
    }

    /// Summary mode draws the run as a tree: every call hangs off the head's
    /// dot on `├─`, the last one closes the branch with `└─`, and an expanded
    /// body hangs off a `│` in the branch's own column — the text lines up
    /// under the call's label.
    #[test]
    fn summary_draws_the_run_as_a_tree() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(&mut tr, "c1", "bash", r#"{"command":"ls"}"#, "a", None);
        call(&mut tr, "c2", "bash", r#"{"command":"pwd"}"#, "b", None);
        let theme = Theme::dark();
        let p = flat(&tr.lines(&theme, 60, ' '));
        assert!(p.contains("● 2 tool calls · 2 commands"), "{p}");
        assert!(
            p.contains("├─ Ran ls"),
            "the first call opens the branch: {p}"
        );
        assert!(p.contains("└─ Ran pwd"), "the last call closes it: {p}");
        assert_eq!(p.matches('├').count(), 1, "{p}");
        assert_eq!(p.matches('└').count(), 1, "{p}");

        // A single-call run closes on its own.
        let mut one = t("s");
        one.set_locale(Locale::En);
        one.tool_output = ToolOutput::Summary;
        call(&mut one, "c1", "bash", r#"{"command":"ls"}"#, "a\nb", None);
        let p = flat(&one.lines(&theme, 60, ' '));
        assert!(p.contains("└─ Ran ls"), "{p}");
        assert!(!p.contains('├'), "a lone call is the whole branch: {p}");

        // The body hangs off the branch's column (three cells), so it lines up
        // with the label the branch introduced.
        one.cells[0].expanded = true;
        let opened = one.lines(&theme, 60, ' ');
        let p = flat(&opened);
        assert!(p.contains("│  a"), "the body gutter: {p}");
        assert_eq!(
            opened
                .iter()
                .filter(|l| l.spans.first().is_some_and(|s| s.content == "│  "))
                .count(),
            2,
            "one gutter per body row: {p}"
        );
    }

    /// The verbs in a summary line stay English in every locale: the line they
    /// open carries a command or a path, and the un-named tools beside them
    /// already show their English tool names. The chrome around the block still
    /// follows the interface language.
    #[test]
    fn summary_verbs_stay_english_in_every_locale() {
        let mut tr = t("s");
        tr.set_locale(Locale::Zh);
        call(&mut tr, "c1", "bash", r#"{"command":"ls -la"}"#, "a", None);
        call(
            &mut tr,
            "c2",
            "read",
            r#"{"file_path":"src/main.rs"}"#,
            "b",
            None,
        );
        call(
            &mut tr,
            "c3",
            "web_search",
            r#"{"queries":["rust"]}"#,
            "c",
            None,
        );
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(p.contains("Ran ls -la"), "{p}");
        assert!(p.contains("Read src/main.rs"), "{p}");
        assert!(p.contains("Searched rust"), "{p}");
        for zh_word in ["运行", "读取", "搜索"] {
            assert!(!p.contains(zh_word), "{zh_word} must not be a verb: {p}");
        }
        assert!(
            p.contains("3 次工具调用"),
            "the count header stays Chinese: {p}"
        );
    }

    /// The default is the one-line mode: a fresh transcript folds tool calls
    /// into the summary block, and `ctrl+o` opens up from there.
    #[test]
    fn the_default_tool_output_is_the_one_line_summary() {
        assert_eq!(ToolOutput::default(), ToolOutput::Summary);
        assert_eq!(
            ToolOutput::default().cycle(),
            ToolOutput::Preview,
            "the chord opens up from the default, it does not start over"
        );

        let mut tr = t("s");
        tr.set_locale(Locale::En);
        call(&mut tr, "c1", "bash", r#"{"command":"ls"}"#, "a", None);
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(p.contains("1 tool call · 1 command"), "{p}");
        assert!(p.contains("└─ Ran ls"), "{p}");
    }

    #[test]
    fn summary_paints_one_line_per_call_under_one_header() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(
            &mut tr,
            "c1",
            "bash",
            r#"{"command":"ls -la"}"#,
            "one\ntwo",
            None,
        );
        call(&mut tr, "c2", "bash", r#"{"command":"pwd"}"#, "x", None);
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(p.contains("2 tool calls · 2 commands"), "{p}");
        assert!(
            p.contains("● 2 tool calls · 2 commands"),
            "the block head is marked with a dot: {p}"
        );
        assert!(p.contains("Ran ls -la"), "{p}");
        assert!(p.contains("Ran pwd"), "{p}");
        assert!(!p.contains('│'), "bodies stay behind the click: {p}");
    }

    #[test]
    fn summary_header_breaks_out_commands_and_failures() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(
            &mut tr,
            "c1",
            "read",
            r#"{"file_path":"a.rs"}"#,
            "text",
            None,
        );
        call(&mut tr, "c2", "bash", r#"{"command":"ls"}"#, "ok", None);
        call(
            &mut tr,
            "c3",
            "bash",
            r#"{"command":"boom"}"#,
            "first\nsecond",
            Some("first\nsecond"),
        );
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(p.contains("3 tool calls · 2 commands · 1 failed"), "{p}");
        assert!(p.contains("Read a.rs"), "the path titles the call: {p}");
        assert!(p.contains("Ran ls"), "{p}");
        assert!(
            p.contains("first"),
            "the failure leads with its message: {p}"
        );
        assert!(!p.contains("second"), "the rest waits for a click: {p}");
        assert!(
            !p.contains("Ran boom"),
            "a failure does not claim a verb: {p}"
        );
    }

    #[test]
    fn summary_failure_opens_on_click() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(
            &mut tr,
            "c1",
            "bash",
            r#"{"command":"boom"}"#,
            "first\nsecond",
            Some("first\nsecond"),
        );
        let theme = Theme::dark();
        let collapsed = flat(&tr.lines(&theme, 60, ' '));
        assert!(
            collapsed.contains("▸"),
            "a hidden line is hinted: {collapsed}"
        );

        // The click lands on the cell the summary line owns.
        let layout = tr.layout(&theme, 60, ' ', false);
        let ci = layout
            .owners
            .iter()
            .flatten()
            .next()
            .copied()
            .expect("the call line is owned");
        tr.cells[ci].expanded = true;

        let opened = flat(&tr.lines(&theme, 60, ' '));
        assert!(
            opened.contains("second"),
            "the click reveals the rest: {opened}"
        );
        assert!(
            opened.contains("│ "),
            "the body hangs off the call: {opened}"
        );
        assert!(opened.contains("▾"), "the hint flips once open: {opened}");
    }

    #[test]
    fn summary_run_survives_an_empty_assistant_husk_between_steps() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(&mut tr, "c1", "bash", r#"{"command":"ls"}"#, "a", None);
        // A step that called a tool without saying anything leaves a husk the
        // renderer skips; the two steps still read as one block.
        tr.cells.push(Cell::new(CellKind::Assistant {
            text: String::new(),
            done: true,
            model: None,
            agent: None,
        }));
        call(&mut tr, "c2", "bash", r#"{"command":"pwd"}"#, "b", None);
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(p.contains("2 tool calls · 2 commands"), "{p}");
        assert_eq!(p.matches("Ran ").count(), 2, "{p}");
    }

    #[test]
    fn summary_run_stops_at_a_painted_cell() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Summary;
        call(&mut tr, "c1", "bash", r#"{"command":"ls"}"#, "a", None);
        tr.cells.push(Cell::new(CellKind::Assistant {
            text: "here is what I found".into(),
            done: true,
            model: None,
            agent: None,
        }));
        call(&mut tr, "c2", "bash", r#"{"command":"pwd"}"#, "b", None);
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert_eq!(
            p.matches("1 tool call · 1 command").count(),
            2,
            "each run heads its own block: {p}"
        );
    }

    #[test]
    fn full_and_preview_modes_leave_the_summary_alone() {
        let mut tr = t("s");
        tr.set_locale(Locale::En);
        tr.tool_output = ToolOutput::Preview;
        call(&mut tr, "c1", "bash", r#"{"command":"ls"}"#, "a\nb", None);
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(!p.contains("Ran "), "preview keeps the card: {p}");
        assert!(!p.contains("tool call"), "preview has no run header: {p}");
        tr.tool_output = ToolOutput::Full;
        let p = flat(&tr.lines(&Theme::dark(), 60, ' '));
        assert!(!p.contains("Ran "), "full keeps the card: {p}");
    }

    #[test]
    fn tool_output_cycles_preview_full_summary() {
        assert_eq!(ToolOutput::Preview.cycle(), ToolOutput::Full);
        assert_eq!(ToolOutput::Full.cycle(), ToolOutput::Summary);
        assert_eq!(ToolOutput::Summary.cycle(), ToolOutput::Preview);
        assert!(ToolOutput::Full.expand_all());
        assert!(!ToolOutput::Preview.expand_all());
        assert!(
            !ToolOutput::Summary.expand_all(),
            "summary is about one line per call, not expanded thoughts"
        );
    }

    #[test]
    fn image_cell_reserves_thumbnail_or_falls_back_to_path() {
        let mut tr = t("s");
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&100u32.to_be_bytes());
        png[20..24].copy_from_slice(&100u32.to_be_bytes());
        tr.push_image(
            "pic.png".into(),
            "look".into(),
            "/tmp/pic.png".into(),
            std::sync::Arc::from(png.clone()),
            Delivery::Delivered,
        );
        let theme = Theme::dark();

        // Thumbnails on: reserve blank lines and report the placement.
        let layout = tr.layout(&theme, 40, ' ', true);
        assert_eq!(layout.images.len(), 1, "PNG image reports one shot");
        let shot = &layout.images[0];
        assert_eq!(shot.cols, 24);
        assert!(
            (2..=12).contains(&shot.rows),
            "aspect-true rows: {}",
            shot.rows
        );
        let reserved: String = layout.lines[shot.line]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            reserved.trim().is_empty(),
            "reserved line is blank: {reserved:?}"
        );

        // Thumbnails off: no reservation, path shown as the fallback.
        let layout = tr.layout(&theme, 40, ' ', false);
        assert!(layout.images.is_empty());
        let text: String = layout
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("/tmp/pic.png"), "fallback path shown: {text}");
    }

    #[test]
    fn banner_splash_centers_the_mark_and_the_url() {
        let theme = Theme::dark();
        let mut tr = t("s");
        tr.push_banner(
            vec![
                "▄▀█ █▄▄ █▄█ █   ▄▀█ █▄▄".into(),
                "█▀█ █▄█  █  █▄▄ █▀█ █▄█".into(),
            ],
            "https://abylab.ai".into(),
            Vec::new(),
        );
        let row = |layout: &TranscriptLayout, i: usize| -> String {
            layout.lines[i]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };

        // 60 cols, mark 23 wide → 18 leading spaces keep both rows aligned.
        let layout = tr.layout(&theme, 60, ' ', false);
        let art_top = row(&layout, 1);
        let art_bottom = row(&layout, 2);
        assert_eq!(
            art_top,
            format!("{}{}", " ".repeat(18), "▄▀█ █▄▄ █▄█ █   ▄▀█ █▄▄")
        );
        assert_eq!(art_bottom.len() - art_bottom.trim_start().len(), 18);
        // The URL shares the mark's left column, one blank row below.
        assert!(
            row(&layout, 3).trim().is_empty(),
            "blank row under the mark"
        );
        assert_eq!(
            row(&layout, 4),
            format!("{}https://abylab.ai", " ".repeat(18))
        );
        // Art rows keep the brand tone; the URL is underlined.
        let art_style = layout.lines[1].spans.last().expect("art span").style;
        assert_eq!(art_style.fg, Some(theme.brand_soft));
        let url_style = layout.lines[4].spans.last().expect("url span").style;
        assert_eq!(url_style.fg, Some(theme.brand_soft));
        assert!(url_style.add_modifier.contains(Modifier::UNDERLINED));

        // Narrower than the mark: the splash falls back to the left margin
        // instead of losing its head to padding.
        let narrow = tr.layout(&theme, 20, ' ', false);
        assert_eq!(row(&narrow, 1), "▄▀█ █▄▄ █▄█ █   ▄▀█ █▄▄");
    }

    #[test]
    fn banner_facts_ride_under_the_url_in_the_caption_tone() {
        let theme = Theme::dark();
        let mut tr = t("s");
        let facts = vec![
            ("version".to_string(), "0.1.4".to_string()),
            ("cwd".to_string(), "/home/kk/project/abylab".to_string()),
            ("permission".to_string(), "workspace-write".to_string()),
            ("model".to_string(), "deepseek-flash".to_string()),
        ];
        tr.push_banner(
            vec![
                "▄▀█ █▄▄ █▄█ █   ▄▀█ █▄▄".into(),
                "█▀█ █▄█  █  █▄▄ █▀█ █▄█".into(),
            ],
            "https://abylab.ai".into(),
            facts.clone(),
        );
        let row = |layout: &TranscriptLayout, i: usize| -> String {
            layout.lines[i]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        let lead = |s: &str| s.len() - s.trim_start().len();

        // 60 cols: mark rows, a blank, the URL, a blank, then one row per fact.
        let layout = tr.layout(&theme, 60, ' ', false);
        let start = 6;
        assert!(row(&layout, 3).trim().is_empty());
        assert!(row(&layout, 5).trim().is_empty(), "facts open a block");
        let column = lead(&row(&layout, 4));
        for (i, (label, value)) in facts.iter().enumerate() {
            let line = row(&layout, start + i);
            assert_eq!(line.trim_start(), format!("{label} · {value}"));
            assert_eq!(lead(&line), column, "facts share the URL's column");
        }
        // The label keeps the caption tone; the value is the readable one.
        let spans = &layout.lines[start].spans;
        assert_eq!(spans[1].style.fg, Some(theme.caption));
        assert_eq!(spans[2].style.fg, Some(theme.fg_secondary));

        // A pane too narrow for the row ellipsizes the value, never the label.
        let narrow = tr.layout(&theme, 24, ' ', false);
        let long = row(&narrow, start + 1);
        assert_eq!(long, "cwd · /home/kk/project/…");
        assert!(long.width() <= 24);
    }

    #[test]
    fn stats_accumulate_turns_steps_and_ttft() {
        let mut tr = t("s");
        tr.apply(UiEvent::TurnStart {
            session: "s".into(),
            turn: 1,
        });
        tr.apply(UiEvent::AssistantFinal {
            session: "s".into(),
            text: "hi".into(),
            model: None,
        });
        tr.apply(UiEvent::ToolCall {
            session: "s".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        tr.apply(UiEvent::ToolResult {
            session: "s".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "ok".into(),
            error: None,
        });
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "first".into(),
        });
        tr.apply(UiEvent::TextDelta {
            session: "s".into(),
            text: "second".into(),
        });
        tr.apply(UiEvent::TurnEnd {
            session: "s".into(),
            kind: "completed".into(),
        });

        assert_eq!(tr.stats.turns, 1);
        assert_eq!(tr.stats.steps, 1);
        assert_eq!(tr.stats.ttft_count, 1, "only the first delta samples TTFT");
        assert!(tr.stats.tool_millis <= tr.stats.turn_millis);
    }

    /// A pending steer is painted after the live output, under its own header,
    /// and falls back into its chronological slot once the agent takes it —
    /// deepseek-harness's `data-pending-steering` tail rows.
    #[test]
    fn pending_steering_renders_as_a_tail_section() {
        let mut tr = Transcript::new("s".into());
        tr.set_locale(Locale::En);
        let paint = |tr: &Transcript| -> (String, Vec<String>) {
            let lines: Vec<String> = tr
                .lines(&Theme::dark(), 40, '⠋')
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect();
            (lines.join("\n"), lines)
        };
        tr.push_user("typed first".into(), Delivery::Delivered);
        tr.apply(crate::events::UiEvent::AssistantFinal {
            session: "s".into(),
            text: "streaming answer".into(),
            model: None,
        });
        tr.push_user("change course".into(), Delivery::Steering);
        // Newer timeline content: a pending steer floats past it to the tail.
        tr.push_notice(NoticeLevel::Info, "later output".into());

        let (joined, lines) = paint(&tr);
        let header = lines
            .iter()
            .position(|line| line.contains("steering · lands"))
            .expect("the tail section names itself");
        let bubble = lines
            .iter()
            .position(|line| line.contains("change course"))
            .expect("the pending bubble");
        let answer = lines
            .iter()
            .position(|line| line.contains("streaming answer"))
            .expect("the live output");
        let later = lines
            .iter()
            .position(|line| line.contains("later output"))
            .expect("newer content");
        assert!(header < bubble, "the header opens the section:\n{joined}");
        assert!(
            answer < bubble && later < bubble,
            "the pending steer sits below everything else:\n{joined}"
        );
        assert!(
            joined.contains("1 steering"),
            "the header counts them:\n{joined}"
        );
        assert!(
            lines[bubble].contains("steering"),
            "the row still names its own state:\n{joined}"
        );
        // The chronological row kept its place: only the pending one moved.
        let first = lines
            .iter()
            .position(|line| line.contains("typed first"))
            .expect("the admitted row");
        assert!(first < answer, "an admitted row stays where it was typed");

        // Admitted: the bubble returns to its slot and the section disappears.
        tr.mark_prompt_delivered(&[2]);
        let (after, _) = paint(&tr);
        assert!(!after.contains("steering · lands"), "{after}");
        let bubble = after.find("change course").expect("bubble");
        let later = after.find("later output").expect("newer content");
        assert!(bubble < later, "back in its chronological slot:\n{after}");
    }
}
