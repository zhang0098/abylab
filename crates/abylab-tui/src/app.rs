//! App state and input handling — the grok-build interaction homage.
//!
//! Enter sends (or queues mid-turn, client-side); Ctrl+X steers the active
//! turn immediately; Esc cancels a running turn with the draft preserved, and
//! Esc owns interrupt; Ctrl+C clears a draft, then needs two empty presses to quit;
//! `/` opens the slash menu; Up recalls history on an empty prompt.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::bus::{
    permission_ask_default_sel, AppEvent, Cmd, CtlEvent, PermissionAskOption, PermissionAskReply,
    SessionListItem,
};
use crate::controller::Controller;
use crate::input::Action;
use crate::locale::{Locale, UiSettings};
use crate::runtime::{settings_path, RuntimeConfig};
use crate::theme::Theme;
use crate::transcript::{clamp_str, NoticeLevel, Transcript};

pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
const CTRL_C_QUIT_WINDOW: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy)]
struct CtrlCQuitChord {
    started: Instant,
    presses: u8,
    required: u8,
}
const TIP_TTL: Duration = Duration::from_secs(4);
/// How long the `↥` jump flash keeps the jumped user prompt background-washed
/// before it restores to normal (Martty's issue #103).
pub(crate) const PROMPT_FLASH_TTL: Duration = Duration::from_secs(5);
/// How often the composer cap re-checks the workspace git branch (tick
/// cadence). Catches checkouts made by the agent's shell tool or in another
/// terminal while the session is open.
const GIT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The startup wordmark: two half-block rows, painted verbatim by
/// `Transcript`'s banner cell (spacing is part of the art). Uppercase-free
/// lowercase shapes, small enough to survive an 80-col terminal with the
/// splash margin intact.
const LOGO_ART: [&str; 2] = ["▄▀█ █▄▄ █▄█ █   ▄▀█ █▄▄", "█▀█ █▄█  █  █▄▄ █▀█ █▄█"];

/// Project URL under the startup wordmark.
const SITE_URL: &str = "https://abylab.ai";

/// Build version the startup splash reports (`-V` prints the same number).
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Some terminal layers incorrectly wrap Kitty/CSI-u key reports in
/// bracketed-paste markers. Crossterm then exposes the key bytes as a paste,
/// so recover them only when the *entire* payload is made of CSI-u keys.
fn decode_leaked_csi_u_keys(text: &str) -> Option<Vec<KeyEvent>> {
    if text.is_empty() {
        return None;
    }

    let mut rest = text;
    let mut keys = Vec::new();
    while !rest.is_empty() {
        let encoded = rest.strip_prefix("\u{1b}[")?;
        let end = encoded.find('u')?;
        keys.push(decode_csi_u_key(&encoded[..end])?);
        rest = &encoded[end + 1..];
    }
    Some(keys)
}

fn decode_csi_u_key(params: &str) -> Option<KeyEvent> {
    let mut fields = params.split(';');
    let codepoint = fields.next()?.split(':').next()?.parse::<u32>().ok()?;
    let modifier_and_kind = fields.next();
    // Text-as-codepoints and any other trailing fields are deliberately not
    // recovered: falling back to ordinary paste is safer than guessing.
    if fields.next().is_some() {
        return None;
    }

    let (modifier_mask, kind) = match modifier_and_kind {
        Some(field) => {
            let mut parts = field.split(':');
            let mask = parts.next()?.parse::<u32>().ok()?;
            if mask == 0 {
                return None;
            }
            let kind = match parts.next() {
                None | Some("1") => KeyEventKind::Press,
                Some("2") => KeyEventKind::Repeat,
                Some("3") => KeyEventKind::Release,
                Some(_) => return None,
            };
            if parts.next().is_some() {
                return None;
            }
            (mask - 1, kind)
        }
        None => (0, KeyEventKind::Press),
    };

    let mut modifiers = KeyModifiers::NONE;
    if modifier_mask & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if modifier_mask & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if modifier_mask & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    if modifier_mask & 8 != 0 {
        modifiers |= KeyModifiers::SUPER;
    }
    if modifier_mask & 16 != 0 {
        modifiers |= KeyModifiers::HYPER;
    }
    if modifier_mask & 32 != 0 {
        modifiers |= KeyModifiers::META;
    }

    let ch = char::from_u32(codepoint)?;
    let code = match ch {
        '\u{1b}' => KeyCode::Esc,
        '\r' => KeyCode::Enter,
        '\t' if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        '\t' => KeyCode::Tab,
        '\u{7f}' => KeyCode::Backspace,
        _ => KeyCode::Char(ch),
    };
    Some(KeyEvent::new_with_kind(code, modifiers, kind))
}

pub struct SlashCommand {
    pub name: &'static str,
    pub usage: &'static str,
    pub desc: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        usage: "/help",
        desc: "show help and tips",
    },
    SlashCommand {
        name: "keys",
        usage: "/keys",
        desc: "keyboard shortcuts",
    },
    SlashCommand {
        name: "new",
        usage: "/new [id]",
        desc: "start a fresh session",
    },
    SlashCommand {
        name: "resume",
        usage: "/resume [id]",
        desc: "resume a durable session from this workspace",
    },
    SlashCommand {
        name: "compact",
        usage: "/compact",
        desc: "condense older history into a summary",
    },
    SlashCommand {
        name: "goal",
        usage: "/goal [objective|pause|resume|complete|clear]",
        desc: "set or control the long-running goal",
    },
    SlashCommand {
        name: "clear",
        usage: "/clear",
        desc: "clear the scrollback",
    },
    SlashCommand {
        name: "model",
        usage: "/model [id]",
        desc: "switch model · live over ACP",
    },
    SlashCommand {
        name: "effort",
        usage: "/effort [off|high|max]",
        desc: "reasoning effort for this session",
    },
    SlashCommand {
        name: "permission",
        usage: "/permission [preset]",
        desc: "permission preset picker · shift+tab cycles",
    },
    SlashCommand {
        name: "plan",
        usage: "/plan [on|off]",
        desc: "toggle host plan mode",
    },
    SlashCommand {
        name: "image",
        usage: "/image <path> [text]",
        desc: "send a local image (png/jpeg/webp/gif)",
    },
    SlashCommand {
        name: "clip",
        usage: "/clip [text]",
        desc: "attach the clipboard image (macOS/Linux)",
    },
    SlashCommand {
        name: "vim",
        usage: "/vim [on|off]",
        desc: "toggle vim modal editing in the composer",
    },
    SlashCommand {
        name: "theme",
        usage: "/theme [dark|light|id]",
        desc: "toggle mode or switch palette pack",
    },
    SlashCommand {
        name: "status",
        usage: "/status",
        desc: "run state, model and the live usage counters",
    },
    SlashCommand {
        name: "lang",
        usage: "/lang [zh|en]",
        desc: "switch interface language",
    },
    SlashCommand {
        name: "login",
        usage: "/login <apikey>",
        desc: "store the API key in the aby home",
    },
    SlashCommand {
        name: "logout",
        usage: "/logout",
        desc: "remove the stored API key",
    },
    SlashCommand {
        name: "skill",
        usage: "/skill <name> [args]",
        desc: "invoke a skill by name, builtin name or not",
    },
    SlashCommand {
        name: "quit",
        usage: "/quit",
        desc: "exit abylab",
    },
];

pub const MODEL_PRESETS: &[&str] = &["deepseek-flash", "deepseek-v4-pro"];

/// Where the agent reads skills from: the workspace's `.agents/skills`
/// directory, and nowhere else. Kept in step with abycore's discovery root —
/// the dialog points at this path, and the driver is what actually scans it.
const SKILLS_DIR: &str = ".agents/skills";

/// Stock composition presets served by `FetchCatalog` until a host
/// catalog replaces them.
/// The stock permission presets (id, one-line meaning) — the default table
/// `@deepseek-ai/dsh-permission-presets` ships. Shift+Tab cycles them;
/// `/permission <name>` passes any other id through for profiles with a
/// custom preset table (the host validates and lists what it knows).
pub const PERMISSION_PRESETS: &[(&str, &str)] = &[
    ("read-only", "read only — no file writes"),
    (
        "workspace-write",
        "write inside the workspace · wider actions ask for approval",
    ),
    (
        "danger-full-access",
        "full file access · approval prompts off — trusted dirs only",
    ),
];

/// Map common spellings onto the stock preset ids (`full` →
/// `danger-full-access`, `ws` → `workspace-write`, `ro` → `read-only`, …).
pub fn normalize_permission(arg: &str) -> Option<&'static str> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "read-only" | "readonly" | "read" | "ro" => Some("read-only"),
        "workspace-write" | "workspace" | "write" | "ws" | "safe" | "sandbox" => {
            Some("workspace-write")
        }
        "danger-full-access" | "full-access" | "full" | "danger" | "yolo" => {
            Some("danger-full-access")
        }
        _ => None,
    }
}

/// User-facing permission label, mirroring the Web's `displayPermissionPreset`:
/// `danger-full-access` → "Full access"; kebab-case keys are title-cased.
pub fn permission_label(id: &str) -> String {
    if id == "danger-full-access" {
        return "Full access".to_string();
    }
    let kebab = !id.is_empty()
        && id.split('-').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    if !kebab {
        return id.to_string();
    }
    id.split('-')
        .map(|seg| {
            let mut chars = seg.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One-line meaning of a stock permission preset in the interface language.
/// Only the three stock ids carry a translation; a custom preset the host
/// reports falls back to the English table (and then to nothing), so the
/// picker never invents a meaning the host didn't list.
pub fn permission_desc(locale: Locale, id: &str) -> Option<&'static str> {
    if locale == Locale::Zh {
        match id {
            "read-only" => return Some("只读 —— 不写文件"),
            "workspace-write" => return Some("只写工作区 · 更大的动作会先征求同意"),
            "danger-full-access" => return Some("完全文件访问 · 关闭审批 —— 仅限信任目录"),
            _ => {}
        }
    }
    PERMISSION_PRESETS
        .iter()
        .find(|(preset, _)| *preset == id)
        .map(|(_, desc)| *desc)
}

/// Map a file extension to the attachment media type the host accepts.
fn media_type_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Read the raster image currently on the system clipboard as (bytes, media
/// type). Terminals don't deliver image paste over stdin, so this shells out
/// to the platform clipboard tool instead.
#[cfg(target_os = "macos")]
fn read_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    let tmp = std::env::temp_dir().join(format!("dsh-clip-{}.png", std::process::id()));
    let tmp_s = tmp.to_str()?.to_string();
    let script = format!(
        "set out to \"{tmp_s}\"\n\
         set d to (the clipboard as «class PNGf»)\n\
         set h to open for access (POSIX file out) with write permission\n\
         write d to h as «class PNGf»\n\
         close access h\n\
         return out"
    );
    let out = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .ok()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let bytes = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);
    Some((bytes, "image/png"))
}

#[cfg(target_os = "linux")]
fn read_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    let attempts: &[(&str, &[&str])] = &[
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (cmd, args) in attempts {
        if let Ok(out) = std::process::Command::new(cmd).args(*args).output() {
            if out.status.success() && !out.stdout.is_empty() {
                return Some((out.stdout, "image/png"));
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_clipboard_image() -> Option<(Vec<u8>, &'static str)> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Starting,
    Running,
}

pub use crate::input::composer::ComposerEditor;

/// One endpoint of a mouse selection in chat-layout coordinates: `line`
/// indexes the full wrapped layout (`ChatView::lines`), `col` is a display
/// cell column within the chat pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelPoint {
    pub line: usize,
    pub col: usize,
}

/// In-app mouse selection — the grok-build gesture: drag highlights,
/// releasing the button copies (选中完即 copy). `anchor` is where the drag
/// started; `head` follows the pointer and may precede the anchor.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: SelPoint,
    pub head: SelPoint,
}

impl Selection {
    /// (start, end) in document order; `end` is inclusive (the cell under
    /// the pointer is part of the selection).
    pub fn ordered(&self) -> (SelPoint, SelPoint) {
        if (self.head.line, self.head.col) < (self.anchor.line, self.anchor.col) {
            (self.head, self.anchor)
        } else {
            (self.anchor, self.head)
        }
    }

    fn is_caret(&self) -> bool {
        self.anchor == self.head
    }
}

/// Composer drag-selection (the same gesture as the chat pane): both
/// endpoints are cells in the input well's text-area coordinates — `(row,
/// col)`, where the drag began and where the pointer is now. The covered char
/// range comes from [`App::input_selection_range`], which treats both endpoint
/// cells as inclusive, so a drag in either direction covers exactly the cells
/// the pointer crossed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputSel {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

/// Snapshot of the chat pane layout from the last draw — the seam that
/// mouse hit-testing and copy extraction read (grok-build's resolved
/// selection model, scaled way down): pane rect, index of the first
/// visible layout line, and the plain text of every layout line.
/// A thumbnail the terminal should draw over the chat pane (kitty graphics).
pub struct ThumbPlacement {
    pub id: u32,
    pub rect: ratatui::layout::Rect,
    pub data: std::sync::Arc<[u8]>,
}

#[derive(Default)]
pub struct ChatView {
    pub area: ratatui::layout::Rect,
    pub top: usize,
    /// Absolute layout line count for scroll math.
    pub total: usize,
    /// The frame's selection snapshot, **viewport-sized**: plain text of the
    /// layout lines this frame actually showed. Hit-testing and copy
    /// extraction never need more (L25).
    pub lines: Vec<String>,
    /// Per snapshot line (same viewport window), the transcript cell that
    /// owns it (only tool cells claim ownership) — the seam for
    /// click-to-expand.
    pub owners: Vec<Option<usize>>,
    /// Visible image thumbnails, filled by `ui::draw_chat` every frame.
    pub images: Vec<ThumbPlacement>,
}

impl ChatView {
    /// The frame's snapshot text for an absolute layout line — `None`
    /// outside the viewport this frame captured.
    pub fn line_text(&self, line: usize) -> Option<&str> {
        self.lines
            .get(line.checked_sub(self.top)?)
            .map(String::as_str)
    }

    /// The transcript cell owning an absolute layout line — `None` outside
    /// the viewport or for lines no tool cell owns.
    pub fn line_owner(&self, line: usize) -> Option<usize> {
        self.owners
            .get(line.checked_sub(self.top)?)
            .copied()
            .flatten()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Effort,
    Theme,
    Permission,
    Session,
    Subagent,
    /// `⌥↑`: pick one client-queued prompt to edit in the composer.
    Queue,
}

#[derive(Clone)]
pub struct PickerItem {
    pub id: String,
    pub label: String,
    pub meta: String,
    pub provider: Option<String>,
}

/// Fixed display width of the picker label column — rows pad/truncate to
/// this so the meta column lines up (char-based padding would misalign
/// CJK labels).
pub(crate) const PICKER_LABEL_COL: usize = 30;

/// One `/` menu entry: a builtin [`SlashCommand`] name row, or an argument
/// candidate under one. The command namespace is closed and resolved
/// client-side before a line ever becomes a prompt; host skills are not part
/// of it — they are only listed as `/skill`'s argument candidates.
#[derive(Clone)]
pub struct SlashEntry {
    pub name: String,
    pub usage: String,
    pub desc: String,
    /// Full composer text for an argument candidate. Command-name rows leave
    /// this empty and retain the historical `/name ` tab completion.
    pub completion: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
pub struct ViewOverlay {
    pub title: String,
    pub nodes: Vec<crate::slots::TuiNode>,
    #[serde(skip)]
    pub scroll: usize,
}

/// The clickable todo progress dialog: the full checklist behind the composer
/// cap row's `2/5 完成` chip. Its content is rendered from `App::plan` on every
/// frame, so a live `todo_write` update refreshes an open dialog in place.
pub struct TodoDialog {
    pub scroll: usize,
}

pub struct Picker {
    pub kind: PickerKind,
    pub title: String,
    pub sel: usize,
    pub items: Vec<PickerItem>,
}

pub struct SubagentView {
    pub id: String,
    pub parent: String,
    pub label: String,
    pub running: bool,
    pub transcript: Transcript,
}

/// Overlay for one ACP `session/request_permission` ask.
pub struct PermissionAskOverlay {
    pub title: String,
    pub sel: usize,
    pub options: Vec<PermissionAskOption>,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<PermissionAskReply>>,
}

impl Drop for PermissionAskOverlay {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(PermissionAskReply::Cancelled);
        }
    }
}

/// Folded per-session mode state (from the durable event stream — the same
/// facts the Web UI chips read). Cached per workspace so chips and pickers
/// show the last-known values immediately on launch.
#[derive(Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Modes {
    pub plan: bool,
    pub sandbox: Option<String>,
    pub approval: Option<String>,
    pub permission: Option<String>,
    /// Reasoning effort as last requested from this client (`/effort`,
    /// the post-model-pick effort picker); the host doesn't echo one.
    pub effort: Option<String>,
}

pub struct App {
    pub theme: Theme,
    pub locale: Locale,
    pub palettes: Vec<crate::theme::PalettePack>,
    pub active_palette_id: String,
    /// Palette currently only *previewed* — the theme dialog row or the
    /// `/theme ` slash candidate under the highlight — but not yet confirmed
    /// with Enter. A preview repaints `theme` without touching
    /// `active_palette_id` (or the settings file), so Esc or a moved
    /// highlight reverts to the committed theme.
    theme_preview: Option<String>,
    pub transcript: Transcript,
    pub subagents: Vec<SubagentView>,
    pub active_subagent: Option<String>,
    pub input: ComposerEditor,
    /// Display-cell width of the composer text well from the latest frame.
    pub(crate) composer_wrap_width: usize,
    /// The composer well's screen rect from the latest frame (prompt column
    /// included) — the seam mouse hit-testing reads between frames.
    pub(crate) composer_area: Rect,
    /// Composer drag-selection highlight. Cleared by a click, Esc, and every
    /// text edit (`App::dispatch`).
    pub(crate) input_sel: Option<InputSel>,
    /// A left-button drag that began inside the well is in progress.
    input_selecting: bool,
    /// First text row shown inside the well (the viewport scroll from
    /// `ui::draw_input`), mirrored from the editor's own scroll offset.
    pub(crate) input_top: usize,
    /// Absolute screen cell of the soft caret from the latest frame (the
    /// reversed block `ui::draw` paints in the composer well or the
    /// elicitation field). `main` repositions the *hidden* hardware cursor
    /// onto this cell after every draw, so IME candidate popups anchor at
    /// the caret instead of wherever the frame diff left the cursor;
    /// `None` when no caret was painted this frame.
    pub caret_cell: Option<(u16, u16)>,
    pub state: RunState,
    pub state_note: String,
    /// True when the terminal speaks the kitty graphics protocol: attachment
    /// image thumbnails emit real pixels (set by `main`).
    pub kitty_pixels: bool,
    pub run_started: Option<Instant>,
    pub spinner_idx: usize,
    pub scroll_up: usize, // lines above the bottom; 0 = follow
    /// Mouse selection over the chat pane (drag-to-select, copy on release).
    pub sel: Option<Selection>,
    /// A left-button drag is in progress.
    selecting: bool,
    last_click: Option<(Instant, u16, u16)>,
    /// Filled by `ui::draw_chat` every frame.
    pub chat_view: ChatView,
    /// Agent advertised `loadSession`.
    load_session: bool,
    /// Last ACP `session_info_update` title.
    session_title: Option<String>,
    pub slash_sel: usize,
    pub picker: Option<Picker>,
    /// Rows the open picker actually shows (`h - border`), recorded by
    /// `ui::draw_model_picker` — the page size for picker PageUp/PageDown.
    pub picker_page_rows: usize,
    /// Open `@file` browser menu (grammar + ratatui-explorer in `file_ref`).
    pub file_menu: Option<crate::file_ref::FileMenu>,
    /// Esc-dismissed `@` token tag: the browser stays closed until the
    /// token text changes.
    file_menu_dismissed: Option<String>,
    /// ACP tool permission ask (separate from `/permission` session modes).
    pub permission_ask: Option<PermissionAskOverlay>,
    /// Read-only modal rendered from a semantic TuiNode tree. Builtin chrome
    /// such as `/keys` uses this surface.
    pub view_overlay: Option<ViewOverlay>,
    /// Images staged in the composer as inline `[image N]` chips living in
    /// the draft text; editing a token away un-stages its image.
    pub pending_images: crate::attachments::Staged,
    /// Screen rects of the inline chips this frame (hover/cursor preview
    /// hit-testing; recorded by `ui::draw_input`).
    pub att_chips: Vec<(ratatui::layout::Rect, usize)>,
    /// Kitty-graphics placement for the hover-preview popup this frame.
    pub att_thumbs: Vec<ThumbPlacement>,
    /// Chip index under the mouse pointer (grok-style hover preview).
    pub hover_att: Option<usize>,
    pub modes: Modes,
    /// User-invocable host skills (`available_commands_update`). They never
    /// join the `/` menu — `/skill ` is their one listing surface, and a bare
    /// `/name` line still ships as a prompt the host expands.
    pub skills: Vec<crate::bus::SkillInfo>,
    /// Last advertised composition select (`/agent`).
    /// Last advertised ACP model select (`/model`).
    last_models: Vec<crate::bus::CatalogModel>,
    /// Last advertised session modes (`/permission`, shift+tab).
    /// Last advertised effort catalog for the current model.
    effort_choices: Vec<String>,
    pub tip: Option<(String, Instant)>,
    /// Live todo checklist from the driver's plan snapshots. While set (and no
    /// transient tip is showing) the composer cap row shows the task in
    /// progress plus completed/total — the transcript keeps the full digest.
    pub plan: Option<crate::events::PlanProgress>,
    /// The open todo progress dialog (`App::plan`'s checklist), if any.
    pub todo_dialog: Option<TodoDialog>,
    /// Screen rect of the cap row's clickable progress chip, recorded by
    /// `ui::draw_composer_box` every frame (`None` when it isn't drawn).
    pub(crate) plan_chip: Option<ratatui::layout::Rect>,
    /// The pointer rests on the progress chip: render it as clickable.
    pub(crate) hover_plan_chip: bool,
    /// Screen rect of the cap row's mouse-only `↥` user-prompt jump glyph,
    /// recorded by `ui::draw_composer_box` every frame.
    pub(crate) prompt_jump_btn: Option<ratatui::layout::Rect>,
    /// The pointer rests on the `↥` glyph: brighten it.
    pub(crate) hover_prompt_jump_btn: bool,
    /// Screen rect of the cap row's mouse-only `⛶` expand glyph, recorded by
    /// `ui::draw_composer_box` every frame.
    pub(crate) expand_btn: Option<ratatui::layout::Rect>,
    /// The pointer rests on the `⛶` glyph: brighten it.
    pub(crate) hover_expand_btn: bool,
    /// Screen rect of the meta row's `↓ N` scroll chip, recorded by the frame
    /// that draws it. `None` while the transcript follows the tail (no chip).
    pub(crate) scroll_btn: Option<ratatui::layout::Rect>,
    /// The pointer rests on the `↓ N` chip: brighten it.
    pub(crate) hover_scroll_btn: bool,
    /// The `⛶` click pins the well to the amplified height (issue #92) until
    /// the next click; the auto layout returns.
    pub(crate) composer_expanded: bool,
    /// Transcript cell of the user prompt the last `↥` click jumped to; the
    /// next click walks one prompt further back (the oldest wraps around).
    /// In-memory only, and it rides across clicks so jumping resumes where the
    /// previous jump stopped.
    pub(crate) prompt_jump_cell: Option<usize>,
    /// The `↥` jump flash: transcript cell of the prompt just jumped to, with
    /// the instant the wash expires.
    pub(crate) prompt_flash: Option<(usize, Instant)>,
    /// The flashing prompt's line span for the current frame, resolved by
    /// `ui::draw_chat` from the live layout (streaming can move it).
    pub(crate) prompt_flash_lines: Option<(usize, usize)>,
    /// DSH_TUI_KEYDEBUG=1: echo every delivered key event in the tip row.
    key_debug: bool,
    /// Optional vim modal editing for the composer (`/vim`).
    pub vim: crate::input::VimState,
    /// Which usage hint the next new session opens with. The composer cap row
    /// no longer rotates hints live; a session start shows one instead, so the
    /// index advances per session and cycles the whole set over time.
    session_tip_idx: usize,
    ctrl_c_armed: Option<CtrlCQuitChord>,
    /// The queued prompt whose `ctrl+d` in the queue picker already asked: the
    /// id of the armed row, cleared when the highlight moves or the list is
    /// rebuilt without it.
    queue_delete_armed: Option<String>,
    pub session_id: String,
    pub cfg: RuntimeConfig,
    /// Current branch of the workspace checkout, shown after the project path
    /// in the composer cap (`path:branch`). `None` covers a non-repo
    /// workspace, a detached HEAD, and a repo whose HEAD cannot be read.
    pub git_branch: Option<String>,
    /// Last time the workspace git branch was re-checked (tick throttle).
    git_check_at: Instant,
    /// Model explicitly picked this session (`/model`); wins over
    /// `transcript.last_model` in the chip until a turn realizes it.
    pub selected_model: Option<String>,
    /// A real `session/new` or `session/load` has supplied this session id.
    /// Cached session options stay hidden until this becomes true.
    pub session_bound: bool,
    /// Keep the old view until the driver commits a switch. Only one is in flight.
    session_switch: Option<SessionSwitch>,
    pub quit: bool,
    pub queued: usize,
    /// Transcript cells grouped by the client FIFO prompt that owns them.
    /// Client-owned FIFO of prompts waiting for the active turn to end; `queued`
    /// mirrors its length, and the queue is what `⌥↑` edits.
    prompt_queue: VecDeque<QueuedPrompt>,
    /// The queued prompt loaded back into the composer for editing (`⌥↑`).
    queue_edit: Option<QueueEditState>,
    /// Send Now bubbles awaiting the driver's settlement.
    pending_steer_cells: HashMap<u64, PendingSteer>,
    next_prompt_id: u64,
    /// A first prompt was handed to the controller but has not reached the
    /// ACP request task yet. Runtime startup alone does not make a turn busy.
    prompt_pending: bool,
    pub server_info: Option<String>,
    pub needs_redraw: bool,
}

enum SessionSwitch {
    New,
    Resume(String),
}

fn ui_session(event: &crate::events::UiEvent) -> Option<&str> {
    use crate::events::UiEvent;
    match event {
        UiEvent::SessionStatus { session, .. }
        | UiEvent::TurnStart { session, .. }
        | UiEvent::TurnEnd { session, .. }
        | UiEvent::TextDelta { session, .. }
        | UiEvent::ReasoningDelta { session, .. }
        | UiEvent::AssistantFinal { session, .. }
        | UiEvent::ToolCall { session, .. }
        | UiEvent::ToolCallDelta { session, .. }
        | UiEvent::ToolStarted { session, .. }
        | UiEvent::ToolResult { session, .. }
        | UiEvent::Usage { session, .. }
        | UiEvent::UserInjected { session, .. }
        | UiEvent::UserMessage { session, .. }
        | UiEvent::SessionTitle { session, .. }
        | UiEvent::Plan { session, .. }
        | UiEvent::PlanMode { session, .. }
        | UiEvent::SandboxMode { session, .. }
        | UiEvent::ApprovalPolicy { session, .. }
        | UiEvent::PermissionPreset { session, .. }
        | UiEvent::ApprovalAsked { session, .. }
        | UiEvent::ApprovalDecided { session, .. } => Some(session),
        UiEvent::SubagentStarted { .. } | UiEvent::SubagentFinished { .. } => None,
    }
}

/// Draft pieces after stripping `[image n]` chips, still in reading order.
enum StagedBlock {
    Text(String),
    Image(crate::attachments::Attachment),
}

/// One client-owned queued prompt: the blocks (text and/or staged images)
/// waiting for the active turn to end, plus the transcript cells echoing them
/// (marked queued until the prompt is actually sent).
///
/// The queue lives here rather than in the driver's command channel so a
/// queued prompt stays addressable: `⌥↑` lists it, Enter loads it back into
/// the composer for editing, and an empty-draft Enter promotes the head into
/// the active turn (Martty's client-owned FIFO).
pub(crate) struct QueuedPrompt {
    id: u64,
    blocks: Vec<StagedBlock>,
    cells: Vec<usize>,
}

/// A Send Now bubble awaiting settlement: the echo cells to re-tint (or to
/// hand back to the queue) plus the blocks themselves, so a deferred steer
/// requeues as the very prompt the user sent — images included.
struct PendingSteer {
    cells: Vec<usize>,
    blocks: Vec<StagedBlock>,
}

/// The queued prompt currently loaded into the composer for editing.
pub(crate) struct QueueEditState {
    prompt_id: u64,
    /// `ctrl+d` arms before it deletes: the first press asks, the second does.
    delete_confirm: bool,
}

fn token_spans_in(
    buf: &str,
    attachments: &[crate::attachments::Attachment],
) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    for (idx, att) in attachments.iter().enumerate() {
        if let Some(byte) = buf.find(&att.token) {
            let start = buf[..byte].chars().count();
            spans.push((start, start + att.token.chars().count(), idx));
        }
    }
    spans.sort_unstable();
    spans
}

/// 8-char id prefix — unique enough in the picker while staying readable;
/// `/resume <prefix>` still matches against the full id.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// One `/resume` row. The label is the session's human handle — the
/// harness title, else the first real prompt; the meta line carries the id
/// prefix plus age · turns (local logs) or the updated date (ACP rows with
/// no local log).
fn session_picker_row(id: &str, title: Option<&str>, updated_at: Option<&str>) -> PickerItem {
    let short = short_id(id);
    let label = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .map(|s| clamp_str(&s, PICKER_LABEL_COL))
        .unwrap_or_else(|| short.clone());
    let meta = format!(
        "{short:<8} · {}",
        updated_at
            .and_then(|u| u.get(..10))
            .filter(|d| !d.is_empty())
            .unwrap_or("?"),
    );
    PickerItem {
        id: id.to_string(),
        label,
        meta,
        provider: None,
    }
}

fn unique_session_list_match(
    locale: Locale,
    sessions: &[SessionListItem],
    prefix: &str,
) -> Result<String, String> {
    let matches: Vec<&SessionListItem> = sessions
        .iter()
        .filter(|s| s.id.starts_with(prefix))
        .collect();
    let list_hint = locale.tr("/resume lists them", "/resume 可以看到它们");
    match matches.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(if locale == Locale::Zh {
            format!("没有会话匹配 “{prefix}” —— {list_hint}")
        } else {
            format!("no session matches “{prefix}” — {list_hint}")
        }),
        many => match many.iter().find(|s| s.id == prefix) {
            Some(one) => Ok(one.id.clone()),
            None => Err(if locale == Locale::Zh {
                format!("“{prefix}” 有歧义（{} 个匹配）—— {list_hint}", many.len())
            } else {
                format!(
                    "“{prefix}” is ambiguous ({} matches) — {list_hint}",
                    many.len()
                )
            }),
        },
    }
}

fn trim_staged_blocks(blocks: &mut Vec<StagedBlock>) {
    while matches!(blocks.first(), Some(StagedBlock::Text(t)) if t.trim().is_empty()) {
        blocks.remove(0);
    }
    while matches!(blocks.last(), Some(StagedBlock::Text(t)) if t.trim().is_empty()) {
        blocks.pop();
    }
    if let Some(StagedBlock::Text(t)) = blocks.first_mut() {
        *t = t.trim().to_string();
    }
    if let Some(StagedBlock::Text(t)) = blocks.last_mut() {
        *t = t.trim().to_string();
    }
    blocks.retain(|b| !matches!(b, StagedBlock::Text(t) if t.is_empty()));
}

/// Split a composer draft on inline image chips, preserving 图文交替.
fn split_draft_into_staged_blocks(
    buf: &str,
    attachments: Vec<crate::attachments::Attachment>,
) -> Vec<StagedBlock> {
    let spans = token_spans_in(buf, &attachments);
    let chars: Vec<char> = buf.chars().collect();
    let mut slots: Vec<Option<crate::attachments::Attachment>> =
        attachments.into_iter().map(Some).collect();
    let mut blocks = Vec::new();
    let mut char_i = 0usize;
    for &(start, end, idx) in &spans {
        if start > char_i {
            let text: String = chars[char_i..start.min(chars.len())].iter().collect();
            if !text.is_empty() {
                blocks.push(StagedBlock::Text(text));
            }
        }
        if let Some(att) = slots.get_mut(idx).and_then(Option::take) {
            blocks.push(StagedBlock::Image(att));
        }
        char_i = end.min(chars.len());
    }
    if char_i < chars.len() {
        let text: String = chars[char_i..].iter().collect();
        if !text.is_empty() {
            blocks.push(StagedBlock::Text(text));
        }
    }
    trim_staged_blocks(&mut blocks);
    blocks
}

fn image_part_from(att: &crate::attachments::Attachment) -> crate::bus::ImagePart {
    crate::bus::ImagePart {
        data: crate::pet::base64(&att.data),
        media_type: att.media_type.clone(),
        name: att.name.clone(),
        path: att.path.clone(),
    }
}

/// One-line label for a queued prompt: its first non-blank text line plus a
/// count of the images riding along (Martty's queue rows).
fn queue_prompt_summary(blocks: &[StagedBlock]) -> String {
    let mut text = String::new();
    let mut images = 0usize;
    for block in blocks {
        match block {
            StagedBlock::Text(part) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(part);
            }
            StagedBlock::Image(_) => images += 1,
        }
    }
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut label = clamp_str(line, 48).to_string();
    match (images, label.is_empty()) {
        (0, _) => label,
        (n, true) => format!("[{n} image]"),
        (n, false) => {
            label.push_str(&format!(" [+{n} image]"));
            label
        }
    }
}

fn prompt_blocks_from_staged(staged: &[StagedBlock]) -> Vec<crate::bus::PromptBlock> {
    staged
        .iter()
        .map(|block| match block {
            StagedBlock::Text(text) => crate::bus::PromptBlock::Text(text.clone()),
            StagedBlock::Image(att) => crate::bus::PromptBlock::Image(image_part_from(att)),
        })
        .collect()
}

impl App {
    pub fn new(theme: Theme, cfg: RuntimeConfig, session_id: String) -> Self {
        let mut palettes = vec![crate::theme::PalettePack::builtin_default()];
        palettes.extend(crate::theme::PalettePack::builtin_gallery());
        let settings = Self::load_settings(&cfg);
        let locale = settings.language;
        // The persisted palette pack survives restarts; the appearance mode
        // arrives already resolved (flag > persisted > dark). A fresh install
        // opens on One, dark — the built-in DeepSeek pack stays selectable.
        let active_palette_id = settings
            .palette
            .clone()
            .filter(|id| palettes.iter().any(|pack| &pack.id == id))
            .unwrap_or_else(|| crate::theme::DEFAULT_PACK.into());
        let theme = palettes
            .iter()
            .find(|pack| pack.id == active_palette_id)
            .map(|pack| pack.theme(theme.mode))
            .unwrap_or(theme);
        // Persisted global preferences seed the per-workspace mode chips when
        // this workspace has no cached facts of its own.
        let mut modes = Self::load_modes_cache(&cfg).unwrap_or_default();
        if modes.effort.is_none() {
            modes.effort = settings.effort.clone();
        }
        if modes.permission.is_none() {
            modes.permission = settings.permission.clone();
        }
        // ":branch" after the project path in the composer cap. Seeded once
        // here; `tick` re-checks on a throttle so mid-session checkouts (the
        // agent's shell tool, another terminal) stay in sync.
        let git_branch = crate::ui::head_branch(&cfg.workspace);
        App {
            theme,
            locale,
            palettes,
            active_palette_id,
            theme_preview: None,
            transcript: {
                let mut transcript = Transcript::new(session_id.clone());
                transcript.set_locale(locale);
                transcript
            },
            subagents: Vec::new(),
            active_subagent: None,
            input: ComposerEditor::new(),
            composer_wrap_width: 80,
            composer_area: Rect::default(),
            input_sel: None,
            input_selecting: false,
            input_top: 0,
            caret_cell: None,
            state: RunState::Idle,
            state_note: String::new(),
            kitty_pixels: false,
            run_started: None,
            spinner_idx: 0,
            scroll_up: 0,
            sel: None,
            selecting: false,
            last_click: None,
            chat_view: ChatView::default(),
            load_session: false,
            session_title: None,
            slash_sel: 0,
            picker: None,
            picker_page_rows: 0,
            file_menu: None,
            file_menu_dismissed: None,
            permission_ask: None,
            view_overlay: None,
            pending_images: crate::attachments::Staged::default(),
            att_chips: Vec::new(),
            att_thumbs: Vec::new(),
            hover_att: None,
            modes,
            skills: Vec::new(),
            last_models: Vec::new(),
            effort_choices: Vec::new(),
            tip: None,
            plan: None,
            todo_dialog: None,
            plan_chip: None,
            hover_plan_chip: false,
            prompt_jump_btn: None,
            hover_prompt_jump_btn: false,
            expand_btn: None,
            hover_expand_btn: false,
            scroll_btn: None,
            hover_scroll_btn: false,
            composer_expanded: false,
            prompt_jump_cell: None,
            prompt_flash: None,
            prompt_flash_lines: None,
            key_debug: std::env::var("ABYLAB_KEYDEBUG").is_ok_and(|v| v == "1"),
            vim: crate::input::VimState::default(),
            session_tip_idx: 0,
            ctrl_c_armed: None,
            queue_delete_armed: None,
            session_id,
            cfg,
            git_branch,
            git_check_at: Instant::now(),
            selected_model: None,
            session_bound: true,
            session_switch: None,
            quit: false,
            queued: 0,
            prompt_queue: VecDeque::new(),
            queue_edit: None,
            pending_steer_cells: HashMap::new(),
            next_prompt_id: 1,
            prompt_pending: false,
            server_info: None,
            needs_redraw: true,
        }
    }

    pub fn spinner(&self) -> char {
        SPINNER[self.spinner_idx % SPINNER.len()]
    }

    pub fn displayed_transcript(&self) -> &Transcript {
        self.active_subagent
            .as_deref()
            .and_then(|id| self.subagents.iter().find(|view| view.id == id))
            .map(|view| &view.transcript)
            .unwrap_or(&self.transcript)
    }

    fn displayed_transcript_mut(&mut self) -> &mut Transcript {
        if let Some(index) = self
            .active_subagent
            .as_deref()
            .and_then(|id| self.subagents.iter().position(|view| view.id == id))
        {
            return &mut self.subagents[index].transcript;
        }
        &mut self.transcript
    }

    /// Re-detect the workspace git branch for the composer cap label. One
    /// in-process read of `.git/HEAD` (no subprocess), at most every
    /// `GIT_CHECK_INTERVAL`.
    fn refresh_git_branch(&mut self) {
        if self.git_check_at.elapsed() < GIT_CHECK_INTERVAL {
            return;
        }
        self.git_check_at = Instant::now();
        let branch = crate::ui::head_branch(&self.cfg.workspace);
        if branch != self.git_branch {
            self.git_branch = branch;
            self.needs_redraw = true;
        }
    }

    pub fn tick(&mut self) {
        // The cap's ":branch" label tracks mid-session checkouts on a
        // throttled cadence.
        self.refresh_git_branch();
        if self.state != RunState::Idle
            || self.transcript.streaming()
            || self
                .subagents
                .iter()
                .any(|view| view.running || view.transcript.streaming())
        {
            self.spinner_idx = self.spinner_idx.wrapping_add(1);
            self.needs_redraw = true;
        }
        if let Some((_, at)) = &self.tip {
            if at.elapsed() > TIP_TTL {
                self.tip = None;
                self.needs_redraw = true;
            }
        }
        // The ↥ jump flash restores the prompt to normal after its few seconds.
        if let Some((_, until)) = &self.prompt_flash {
            if Instant::now() >= *until {
                self.prompt_flash = None;
                self.prompt_flash_lines = None;
                self.needs_redraw = true;
            }
        }
        // disarm expired chords
        if let Some(chord) = self.ctrl_c_armed {
            if chord.started.elapsed() > CTRL_C_QUIT_WINDOW {
                self.ctrl_c_armed = None;
            }
        }
    }

    pub fn show_tip(&mut self, text: impl Into<String>) {
        self.tip = Some((text.into(), Instant::now()));
        self.needs_redraw = true;
    }

    /// Greet a new session with one usage hint.
    ///
    /// The hint rotation used to live in the composer cap row, competing with
    /// the draft and the todo checklist for the same line; it lands in the
    /// timeline once per session instead, and the index cycles so a user who
    /// keeps starting sessions still walks the whole set.
    pub fn push_session_tip(&mut self) {
        let hint = self.locale.session_tip(self.session_tip_idx);
        self.session_tip_idx = (self.session_tip_idx + 1) % crate::locale::TIP_COUNT;
        let label = self.locale.tr("Tip", "提示");
        self.transcript
            .push_markdown(format!("- **{label}** · {hint}"));
    }

    /// The two lines the shell gets once the alternate screen is gone: the
    /// session id and the ways back into it.
    ///
    /// The id is the one on screen at exit — a `/resume` switch moves it — so a
    /// session entered mid-run is the one named. Both routes need the launch's
    /// workspace and session root: resume from the same directory, or use
    /// `/resume`, which lists exactly the sessions this one can still open.
    pub fn exit_notice(&self) -> String {
        let id = &self.session_id;
        match self.locale {
            Locale::En => format!(
                "Session id: {id}\nResume it later: abylab --session-id {id} · or pick it with /resume"
            ),
            Locale::Zh => format!(
                "会话 id：{id}\n下次继续：abylab --session-id {id} · 也可以在程序里用 /resume 选择"
            ),
        }
    }

    /// The startup splash: the ASCII wordmark, the project URL and the launch
    /// facts — build version, working directory, permission preset and model —
    /// painted once per run at the top of the timeline (see `main`). `/new`
    /// keeps the timeline to the usage hint: the mark (and the facts it
    /// reports) belongs to the launch, not to every session the client opens.
    pub fn push_banner(&mut self) {
        let facts = vec![
            (
                self.locale.tr("version", "版本").to_string(),
                VERSION.to_string(),
            ),
            (
                self.locale.tr("cwd", "工作目录").to_string(),
                self.cfg.workspace.clone(),
            ),
            (
                self.locale.tr("permission", "权限").to_string(),
                self.current_permission().to_string(),
            ),
            (
                self.locale.tr("model", "模型").to_string(),
                self.model_fact(),
            ),
        ];
        self.transcript.push_banner(
            LOGO_ART.iter().map(|row| (*row).to_string()).collect(),
            SITE_URL.to_string(),
            facts,
        );
    }

    /// The splash's model value: the model id, with the requested reasoning
    /// effort riding along when the session has one.
    fn model_fact(&self) -> String {
        match self.modes.effort.as_deref().filter(|e| !e.is_empty()) {
            Some(effort) => format!(
                "{} · {} {effort}",
                self.cfg.model,
                self.locale.tr("effort", "推理强度")
            ),
            None => self.cfg.model.clone(),
        }
    }

    fn activate_palette(&mut self, id: &str) {
        if !self.palettes.iter().any(|p| p.id == id) {
            return;
        }
        // A commit (Enter, or `/theme <pack>`) supersedes any preview still
        // on screen — the row it painted *is* the committed pack now.
        self.theme_preview = None;
        self.active_palette_id = id.to_string();
        self.sync_theme_from_active();
        self.save_settings();
        self.show_tip(format!(
            "{}: {} {}",
            self.locale.tr("theme", "主题"),
            self.active_palette_id,
            self.theme.mode.as_str()
        ));
    }

    fn select_palette(&mut self, id: &str) {
        self.activate_palette(id);
    }

    fn sync_theme_from_active(&mut self) {
        let mode = self.theme.mode;
        if let Some(pack) = self
            .palettes
            .iter()
            .find(|p| p.id == self.active_palette_id)
        {
            self.theme = pack.theme(mode);
        }
    }

    /// Arrows over the theme dialog rows (or the `/theme ` slash candidates)
    /// only *preview*: the committed palette stays `active_palette_id` until
    /// Enter, and nothing is written to settings. Esc or a moved highlight
    /// restores the committed theme.
    fn preview_palette(&mut self, id: &str) {
        if self.active_palette_id == id {
            // Highlight back on the committed pack — nothing to preview.
            self.clear_theme_preview();
            return;
        }
        let Some(pack) = self.palettes.iter().find(|pack| pack.id == id) else {
            return;
        };
        let mode = self.theme.mode;
        self.theme = pack.theme(mode);
        self.theme_preview = Some(id.to_string());
        self.needs_redraw = true;
    }

    /// Drop a preview and repaint the committed theme.
    fn clear_theme_preview(&mut self) {
        if self.theme_preview.take().is_some() {
            self.sync_theme_from_active();
            self.needs_redraw = true;
        }
    }

    /// The open theme dialog paints the row under its highlight.
    fn preview_picker_theme(&mut self) {
        let Some(id) = self.picker.as_ref().and_then(|picker| {
            (picker.kind == PickerKind::Theme)
                .then(|| picker.items.get(picker.sel))
                .flatten()
                .map(|item| item.id.clone())
        }) else {
            return;
        };
        self.preview_palette(&id);
    }

    /// The open `/theme ` candidate popup previews the palette the highlight
    /// just landed on (the dark/light rows never name a pack).
    fn preview_slash_theme(&mut self) {
        if let Some(id) = self.slash_theme_candidate() {
            self.preview_palette(&id);
        }
    }

    /// The palette candidate under the open `/theme ` popup highlight, if
    /// that row names a registered pack.
    fn slash_theme_candidate(&self) -> Option<String> {
        let matches = self.slash_matches();
        let entry = matches.get(self.slash_sel.min(matches.len().checked_sub(1)?))?;
        if entry.name != "theme" {
            return None;
        }
        let id = entry
            .completion
            .as_deref()
            .and_then(|completion| completion.strip_prefix("/theme "))?;
        self.palettes
            .iter()
            .any(|pack| pack.id == id)
            .then(|| id.to_string())
    }

    /// A preview only lives while its row is still highlighted: Esc, a closed
    /// list, a moved highlight or an edited draft all land here and revert to
    /// the committed theme, so a preview never sticks.
    fn reconcile_theme_preview(&mut self) {
        let Some(preview) = self.theme_preview.clone() else {
            return;
        };
        let still_highlighted = self.picker.as_ref().is_some_and(|picker| {
            picker.kind == PickerKind::Theme
                && picker
                    .items
                    .get(picker.sel)
                    .is_some_and(|item| item.id == preview)
        }) || self.slash_theme_candidate().as_deref()
            == Some(preview.as_str());
        if !still_highlighted {
            self.clear_theme_preview();
        }
    }

    fn apply_theme_arg(&mut self, arg: &str) {
        match arg {
            "" => self.open_theme_picker(),
            "dark" => {
                self.theme = self.theme.with_mode(crate::theme::Mode::Dark);
                self.save_settings();
                self.show_tip(format!(
                    "{}: {} dark",
                    self.locale.tr("theme", "主题"),
                    self.active_palette_id
                ));
            }
            "light" => {
                self.theme = self.theme.with_mode(crate::theme::Mode::Light);
                self.save_settings();
                self.show_tip(format!(
                    "{}: {} light",
                    self.locale.tr("theme", "主题"),
                    self.active_palette_id
                ));
            }
            id => {
                if self.palettes.iter().any(|p| p.id == id) {
                    self.select_palette(id);
                } else {
                    let unknown = self.locale.tr("unknown palette", "未知主题包");
                    self.show_tip(format!("{unknown}: {id}"));
                    self.transcript
                        .push_notice(NoticeLevel::Warn, format!("{unknown} `{id}`"));
                }
            }
        }
    }

    fn open_theme_picker(&mut self) {
        let items = self
            .palettes
            .iter()
            .map(|pack| PickerItem {
                id: pack.id.clone(),
                label: pack.label.clone(),
                meta: pack.id.clone(),
                provider: None,
            })
            .collect::<Vec<_>>();
        let sel = items
            .iter()
            .position(|item| item.id == self.active_palette_id)
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Theme,
            title: self
                .locale
                .tr(
                    " theme · ↑↓ preview · enter apply · esc close · ctrl+t dark/light ",
                    " 主题 · ↑↓ 预览 · enter 应用 · esc 关闭 · ctrl+t 切换明暗 ",
                )
                .into(),
            sel,
            items,
        });
    }

    /// The `/` command menu owns the band above the composer while it has
    /// rows to show — the `@` browser never opens underneath it.
    pub(crate) fn slash_completion_open(&self) -> bool {
        !self.slash_matches().is_empty()
    }

    pub fn slash_matches(&self) -> Vec<SlashEntry> {
        if !self.input.buf().starts_with('/') {
            return Vec::new();
        }
        if let Some((name, arg)) = self.input.buf()[1..].split_once(' ') {
            return self.slash_argument_matches(name, arg);
        }
        let prefix = &self.input.buf()[1..];
        // Builtins only. Skills stay out of this list: `/` is the command
        // namespace, and a catalog of arbitrary user-chosen names would bury
        // it. The same skills list under the one command that runs them,
        // `/skill `.
        let mut out: Vec<SlashEntry> = SLASH_COMMANDS
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .map(|c| SlashEntry {
                name: c.name.to_string(),
                usage: c.usage.to_string(),
                desc: self.locale.command_desc(c.name, c.desc).to_string(),
                completion: None,
            })
            .collect();
        // An exact command must win over a longer name sharing its prefix.
        out.sort_by_key(|entry| entry.name != prefix);
        out
    }

    fn slash_argument_matches(&self, name: &str, arg: &str) -> Vec<SlashEntry> {
        let prefix = arg.trim_start();
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }

        self.builtin_argument_options(name)
            .into_iter()
            .filter(|(value, _, _)| value.starts_with(prefix))
            .map(|(value, label, desc)| SlashEntry {
                name: name.to_string(),
                usage: label,
                desc,
                completion: Some(format!("/{name} {value}")),
            })
            .collect()
    }

    fn builtin_argument_options(&self, name: &str) -> Vec<(String, String, String)> {
        let plain =
            |value: &str, desc: &str| (value.to_string(), value.to_string(), desc.to_string());
        match name {
            "model" => {
                if !self.last_models.is_empty() {
                    return self
                        .last_models
                        .iter()
                        .map(|model| (model.id.clone(), model.name.clone(), model.provider.clone()))
                        .collect();
                }
                let mut ids = MODEL_PRESETS
                    .iter()
                    .map(|value| (*value).to_string())
                    .collect::<Vec<_>>();
                if !ids.iter().any(|id| id == &self.cfg.model) {
                    ids.insert(0, self.cfg.model.clone());
                }
                ids.into_iter()
                    .map(|id| (id.clone(), id, String::new()))
                    .collect()
            }
            "effort" if !self.effort_choices.is_empty() => self
                .effort_choices
                .iter()
                .map(|effort| (effort.clone(), effort.clone(), String::new()))
                .collect(),
            "effort" => vec![
                plain(
                    "off",
                    self.locale.tr("disable extended reasoning", "关闭扩展推理"),
                ),
                plain(
                    "high",
                    self.locale.tr("high reasoning effort", "高推理强度"),
                ),
                plain(
                    "max",
                    self.locale.tr("maximum reasoning effort", "最高推理强度"),
                ),
            ],
            "permission" => PERMISSION_PRESETS
                .iter()
                .map(|(id, _)| plain(id, permission_desc(self.locale, id).unwrap_or_default()))
                .collect(),
            "plan" => vec![
                plain("on", self.locale.tr("enable plan mode", "打开计划模式")),
                plain("off", self.locale.tr("disable plan mode", "关闭计划模式")),
            ],
            "theme" => {
                let mut choices = vec![
                    plain("dark", self.locale.tr("dark appearance", "深色外观")),
                    plain("light", self.locale.tr("light appearance", "浅色外观")),
                ];
                choices.extend(self.palettes.iter().map(|palette| {
                    (
                        palette.id.clone(),
                        palette.label.clone(),
                        self.locale.tr("palette pack", "主题包").to_string(),
                    )
                }));
                choices
            }
            "lang" => vec![plain("zh", "中文"), plain("en", "English")],
            // A skill is what `/skill` takes. The list follows the typed
            // prefix, and the same description the menu and the model's index
            // show rides along as the row's second column.
            "skill" => self
                .skills
                .iter()
                .map(|skill| {
                    let label = match &skill.input_hint {
                        Some(hint) => format!("{} {hint}", skill.name),
                        None => skill.name.clone(),
                    };
                    (skill.name.clone(), label, skill.description.clone())
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Whether a `/skill` argument candidate names a skill that declares an
    /// argument placeholder. Those open a form — the line completes and waits
    /// for the argument — instead of running on the bare name.
    fn skill_candidate_awaits_args(&self, entry: &SlashEntry) -> bool {
        let Some(value) = entry
            .completion
            .as_deref()
            .and_then(|completion| completion.rsplit(' ').next())
        else {
            return false;
        };
        self.skills
            .iter()
            .any(|skill| skill.name == value && skill.input_hint.is_some())
    }

    pub fn handle(&mut self, ev: AppEvent, ctl: &Controller) {
        let modes_before = self.modes.clone();
        let model_before = self.cfg.model.clone();
        self.handle_inner(ev, ctl);
        // A previewed palette never sticks: after every event, one that is no
        // longer under a highlight reverts the painter to the committed theme.
        self.reconcile_theme_preview();
        // Persist mode-fact changes (chips survive restarts — the cache is
        // the landing state's source of truth until the host reports).
        if self.modes != modes_before {
            self.save_modes_cache();
        }
        // Durable preferences track the live facts: a host-confirmed
        // permission/effort switch or model bind survives restarts.
        if self.modes != modes_before || self.cfg.model != model_before {
            self.save_settings();
        }
        // A turn ran on the picked model → the stream is the truth again.
        if self.selected_model.is_some()
            && self.selected_model.as_deref() == self.transcript.last_model.as_deref()
        {
            self.selected_model = None;
        }
    }

    fn handle_inner(&mut self, ev: AppEvent, ctl: &Controller) {
        match ev {
            AppEvent::Terminate => {
                self.quit = true;
            }
            AppEvent::Term(term) => self.handle_term(term, ctl),
            AppEvent::Ui(ui) => {
                // The turn that a queued prompt waited behind has ended: hand
                // the FIFO head to the driver. Read the fact before `apply_ui`
                // folds it (the fold clears the run state).
                let idle = matches!(
                    &ui,
                    crate::events::UiEvent::SessionStatus { session, running: false }
                        if *session == self.session_id
                );
                self.apply_ui(ui);
                if idle {
                    self.dispatch_next_queued(ctl);
                }
            }
            AppEvent::RuntimeStderr(_line) => {
                // kept in proto's tail buffer for diagnostics; stay quiet here
            }
            AppEvent::RuntimeExited(code) => {
                self.session_switch = None;
                self.prompt_pending = false;
                self.queued = 0;
                // The runtime is gone and so is the queue's chance to be sent:
                // its echoes leave the timeline with it, exactly as a deleted
                // item does (they were never delivered).
                let dropped: Vec<usize> = self
                    .prompt_queue
                    .drain(..)
                    .flat_map(|prompt| prompt.cells)
                    .collect();
                self.withdraw_prompt_echo(&dropped);
                self.queue_edit = None;
                self.pending_steer_cells.clear();
                if self.state != RunState::Idle {
                    self.state = RunState::Idle;
                    self.run_started = None;
                }
                if let Some(c) = code {
                    if c != 0 {
                        self.transcript.push_notice(
                            NoticeLevel::Warn,
                            format!(
                                "{} ({c}) — {}",
                                self.locale
                                    .tr("runtime exited with code", "运行时退出，代码"),
                                self.locale
                                    .tr("the next prompt restarts it", "下一条消息会重新拉起")
                            ),
                        );
                    }
                }
                self.needs_redraw = true;
            }
            AppEvent::Ctl(ctl_ev) => {
                match ctl_ev {
                    CtlEvent::Starting { .. } => {
                        self.state = RunState::Starting;
                        self.run_started = Some(Instant::now());
                        self.state_note =
                            self.locale.tr("starting runtime", "正在启动运行时").into();
                    }
                    CtlEvent::Ready { server } => {
                        self.server_info = Some(server.clone());
                        if !self.prompt_pending {
                            self.state = RunState::Idle;
                            self.run_started = None;
                            self.state_note.clear();
                        }
                    }
                    CtlEvent::PromptQueued { .. } => {
                        // The driver picked a prompt up: the turn is running.
                        // Queued items in the client's FIFO are dispatched on
                        // the idle status, not here.
                        self.prompt_pending = false;
                        if self.state == RunState::Starting {
                            self.state = RunState::Running;
                        }
                        self.state_note.clear();
                    }
                    CtlEvent::SteerSettled {
                        message_id,
                        deferred,
                    } => {
                        if let Some(pending) = self.pending_steer_cells.remove(&message_id) {
                            if deferred {
                                self.transcript.mark_prompt_queued(&pending.cells);
                                self.enqueue_prompt(pending.blocks, pending.cells);
                                self.show_tip(self.locale.tr(
                                    "agent deferred Send Now — queued after the active turn",
                                    "Agent 推迟了立即发送 —— 已排到本轮之后",
                                ));
                            }
                        }
                    }
                    CtlEvent::Error(err) => {
                        self.prompt_pending = false;
                        self.state = RunState::Idle;
                        self.run_started = None;
                        self.transcript.push_notice(NoticeLevel::Error, err);
                    }
                    CtlEvent::CancelRequested => {
                        self.state_note = self.locale.tr("cancelling", "正在取消").into();
                        self.transcript.cancel_open_work();
                    }
                    CtlEvent::Interrupted => {
                        self.prompt_pending = false;
                        self.state = RunState::Idle;
                        self.run_started = None;
                        self.state_note.clear();
                        self.transcript.cancel_open_work();
                        self.transcript.push_notice(
                            NoticeLevel::Warn,
                            self.locale
                                .tr("interrupted — turn cancelled", "已中断 —— 本轮已取消")
                                .into(),
                        );
                    }
                    CtlEvent::Skills { skills } => {
                        self.skills = skills;
                    }
                    CtlEvent::Catalog { models } => {
                        if !models.is_empty() {
                            self.last_models = models.clone();
                        }
                        if let Some(picker) = &mut self.picker {
                            match picker.kind {
                                PickerKind::Model if !models.is_empty() => {
                                    let current = self.cfg.model.clone();
                                    let current_provider = self.cfg.provider.clone();
                                    picker.items = models
                                        .into_iter()
                                        .map(|m| PickerItem {
                                            id: m.id.clone(),
                                            label: m.id,
                                            meta: format!(
                                                "{} · {}{}",
                                                m.provider,
                                                m.name,
                                                if m.vision { " · vision" } else { "" }
                                            ),
                                            provider: Some(m.provider),
                                        })
                                        .collect();
                                    picker.sel = picker
                                        .items
                                        .iter()
                                        .position(|i| {
                                            i.id == current
                                                && i.provider.as_deref()
                                                    == Some(current_provider.as_str())
                                        })
                                        .unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                    }
                    CtlEvent::Efforts { efforts, default } => {
                        if !efforts.is_empty() {
                            self.effort_choices = efforts.clone();
                        }
                        self.open_effort_picker(efforts, default);
                    }
                    CtlEvent::TuiOpDone(desc) => {
                        self.transcript.push_notice(NoticeLevel::Info, desc);
                    }
                    CtlEvent::TuiOpFailed(desc) => {
                        self.transcript.push_notice(NoticeLevel::Warn, desc);
                    }
                    CtlEvent::SessionSwitchFailed(desc) => {
                        self.session_switch = None;
                        self.transcript.push_notice(NoticeLevel::Warn, desc);
                        if self.state == RunState::Idle && !self.prompt_pending {
                            self.dispatch_next_queued(ctl);
                        }
                    }
                    CtlEvent::AgentCaps { load_session } => {
                        self.load_session = load_session;
                    }
                    CtlEvent::SessionBound {
                        session_id,
                        notice,
                        model,
                        effort,
                    } => {
                        let switched = match &self.session_switch {
                            Some(SessionSwitch::New) => self.session_id != session_id,
                            Some(SessionSwitch::Resume(target)) => *target == session_id,
                            None => false,
                        };
                        let fresh =
                            switched && matches!(self.session_switch, Some(SessionSwitch::New));
                        if switched || (self.session_bound && self.session_id != session_id) {
                            self.session_switch = None;
                            self.reset_session_ui();
                        } else if self.session_id != session_id {
                            // Initial binding keeps already reported mode facts.
                            self.reset_subagent_views();
                        }
                        self.session_id = session_id.clone();
                        self.transcript.set_root_session(session_id);
                        self.session_bound = true;
                        if fresh {
                            self.push_session_tip();
                        }
                        // The driver is the source of truth for the bound
                        // session's model: a resumed session keeps its stored
                        // model, and a rejected /model never moves this row.
                        if let Some(model) = model {
                            self.cfg.model = model;
                        }
                        if let Some(effort) = effort {
                            self.modes.effort = Some(effort);
                        }
                        // A bound session owns the view: a resumed session's
                        // replayed history would sit under the welcome hero.
                        if let Some(notice) = notice {
                            self.transcript.push_notice(NoticeLevel::Info, notice);
                        }
                        ctl.send(Cmd::FetchSkills);
                    }
                    CtlEvent::SessionList { sessions, prefix } => {
                        self.on_acp_session_list(sessions, prefix, ctl);
                    }
                }
                self.needs_redraw = true;
            }
            AppEvent::PermissionAsk {
                title,
                options,
                reply,
            } => {
                self.open_permission_ask(title, options, reply);
            }
        }
    }

    /// Fold one decoded protocol fact into both client chrome and transcript.
    /// Direct ACP facts and JSON-RPC notifications must take the same path.
    fn apply_ui(&mut self, ui: crate::events::UiEvent) {
        use crate::events::UiEvent as E;

        if let E::SubagentStarted {
            parent,
            child,
            label,
        } = &ui
        {
            if !self.subagents.iter().any(|view| view.id == *child) {
                let fallback = format!(
                    "{} {}",
                    self.locale.tr("subagent", "子代理"),
                    self.subagents.len() + 1
                );
                let mut transcript = Transcript::new(child.clone());
                transcript.set_locale(self.locale);
                self.subagents.push(SubagentView {
                    id: child.clone(),
                    parent: parent.clone(),
                    label: label.clone().unwrap_or(fallback),
                    running: true,
                    transcript,
                });
            }
            if parent == &self.session_id {
                self.transcript.apply(ui);
            } else if let Some(view) = self.subagents.iter_mut().find(|view| view.id == *parent) {
                view.transcript.apply(ui);
            }
            self.needs_redraw = true;
            return;
        }

        if let E::SubagentFinished { child } = &ui {
            let parent = self
                .subagents
                .iter_mut()
                .find(|view| view.id == *child)
                .map(|view| {
                    view.running = false;
                    view.parent.clone()
                });
            if parent.as_deref() == Some(self.session_id.as_str()) {
                self.transcript.apply(ui);
            } else if let Some(parent) = parent {
                if let Some(view) = self.subagents.iter_mut().find(|view| view.id == parent) {
                    view.transcript.apply(ui);
                }
            }
            self.needs_redraw = true;
            return;
        }

        if let Some(session) = ui_session(&ui) {
            if session != self.session_id {
                if let Some(view) = self.subagents.iter_mut().find(|view| view.id == session) {
                    view.transcript.apply(ui);
                    self.needs_redraw = true;
                    return;
                }
            }
        }

        let mut apply_to_transcript = true;
        match &ui {
            E::PlanMode { session, active } if *session == self.session_id => {
                if self.modes.plan == *active {
                    apply_to_transcript = false;
                } else {
                    self.modes.plan = *active;
                }
            }
            // The permission facts fold the meta row's chips and stop there: a
            // switch is confirmed by the chip itself (the label brightens under
            // full access), so echoing the same facts into the timeline would
            // only repeat the composer chrome.
            E::SandboxMode { session, mode } if *session == self.session_id => {
                self.modes.sandbox = Some(mode.clone());
                apply_to_transcript = false;
            }
            E::ApprovalPolicy { session, policy } if *session == self.session_id => {
                self.modes.approval = Some(policy.clone());
                apply_to_transcript = false;
            }
            E::PermissionPreset { session, preset } if *session == self.session_id => {
                self.modes.permission = Some(preset.clone());
                apply_to_transcript = false;
            }
            E::SessionTitle { session, title } if *session == self.session_id => {
                self.session_title = Some(title.clone());
            }
            E::Plan { session, .. } if *session == self.session_id => {
                // The driver sends one snapshot per committed todo_write; an
                // empty checklist clears the composer's todo line with it, and
                // an open progress dialog has nothing left to show.
                self.plan = ui.plan_progress();
                if self.plan.is_none() {
                    self.todo_dialog = None;
                }
            }
            _ => {}
        }

        if let E::SessionStatus { session, running } = &ui {
            if *session == self.session_id {
                self.state = if *running {
                    RunState::Running
                } else {
                    RunState::Idle
                };
                if *running {
                    if self.run_started.is_none() {
                        self.run_started = Some(Instant::now());
                    }
                } else {
                    self.prompt_pending = false;
                    self.run_started = None;
                    self.state_note.clear();
                }
            }
        }
        if apply_to_transcript {
            self.transcript.apply(ui);
        }
        self.needs_redraw = true;
    }

    fn handle_term(&mut self, ev: Event, ctl: &Controller) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                // CG rescue first: a bare arrow/⌫ with ⌘/⌥ physically held
                // gets its modifier restored (macOS terminals drop them).
                self.handle_key(crate::input::rescue_key(key), ctl)
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(..) => self.needs_redraw = true,
            Event::Paste(text) => {
                if let Some(keys) = decode_leaked_csi_u_keys(&text) {
                    for key in keys {
                        if key.kind != KeyEventKind::Release {
                            self.handle_key(crate::input::rescue_key(key), ctl);
                        }
                    }
                    return;
                }
                // The composer is multi-line (soft wrap, ctrl+j), so a paste
                // keeps its line structure instead of being flattened to
                // spaces. `insert_str` understands `\n`; normalize the stray
                // CR-only endings some terminals (iTerm2 et al.) send.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                self.input.insert_str(&text);
                self.reconcile_attachments();
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    /// grok-build mouse semantics, scaled down: wheel scrolls; left-drag
    /// selects with a live highlight (auto-scrolling at the pane edges) and
    /// copies on release; double-click selects & copies a word. Shift+drag
    /// bypasses capture in most terminals → native selection still works.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.todo_dialog.is_some() {
                    self.todo_dialog_scroll_by(-3);
                } else if self.view_overlay.is_some() {
                    self.view_scroll_by(-3);
                } else if self.picker.is_some() {
                    self.picker_scroll_by(-1);
                } else {
                    self.mouse_scroll(3, mouse.column, mouse.row);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.todo_dialog.is_some() {
                    self.todo_dialog_scroll_by(3);
                } else if self.view_overlay.is_some() {
                    self.view_scroll_by(3);
                } else if self.picker.is_some() {
                    self.picker_scroll_by(1);
                } else {
                    self.mouse_scroll(-3, mouse.column, mouse.row);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.needs_redraw = true;
                // The cap row's progress chip opens the todo dialog. No other
                // modal may be up: the chip sits under an open dialog.
                if self.plan_chip_at(mouse.column, mouse.row) && !self.modal_open() {
                    self.input_sel = None;
                    self.input_selecting = false;
                    self.open_todo_dialog();
                    return;
                }
                // The `↥` glyph right of the project path walks the session's
                // user prompts (newest first, then back, then wrapping).
                if self.prompt_jump_btn_hit(mouse.column, mouse.row) && !self.modal_open() {
                    self.input_sel = None;
                    self.input_selecting = false;
                    self.jump_to_user_prompt();
                    return;
                }
                // Clicking a tool block toggles its expand/collapse instead of
                // starting a text selection.
                if let Some(ci) = self.tool_at(mouse.column, mouse.row) {
                    self.sel = None;
                    self.selecting = false;
                    self.last_click = None;
                    self.input_selecting = false;
                    self.toggle_tool(ci);
                    return;
                }
                // The mouse-only `⛶` glyph (issue #92) pins the well to the
                // amplified height and restores it on the next click.
                if self.expand_btn_hit(mouse.column, mouse.row) && !self.modal_open() {
                    self.input_sel = None;
                    self.input_selecting = false;
                    self.composer_expanded = !self.composer_expanded;
                    self.needs_redraw = true;
                    return;
                }
                // The `↓ N` chip in the meta row is the way back down: one
                // click drops the scroll and follows the tail again.
                if self.scroll_btn_hit(mouse.column, mouse.row) && !self.modal_open() {
                    self.input_sel = None;
                    self.input_selecting = false;
                    self.scroll_up = 0;
                    self.needs_redraw = true;
                    return;
                }
                // A click inside the composer well places the caret at the
                // clicked char and arms a drag-selection; the chat highlight is
                // dismissed first, like any click outside that pane.
                if !self.modal_open() && self.input_hit(mouse.column, mouse.row) {
                    self.sel = None;
                    self.selecting = false;
                    self.last_click = None;
                    let cell = self.input_cell_at(mouse.column, mouse.row);
                    let offset =
                        self.input
                            .screen_to_char(self.composer_wrap_width, cell.0, cell.1);
                    self.input.set_cursor_char(offset);
                    self.input_sel = Some(InputSel {
                        anchor: cell,
                        head: cell,
                    });
                    self.input_selecting = true;
                    self.refresh_file_menu();
                    return;
                }
                let Some(p) = self.chat_hit(mouse.column, mouse.row) else {
                    // Click outside the chat pane dismisses the highlight.
                    self.sel = None;
                    self.selecting = false;
                    self.input_sel = None;
                    self.input_selecting = false;
                    return;
                };
                self.input_sel = None;
                self.input_selecting = false;
                let double = self.last_click.take().is_some_and(|(at, x, y)| {
                    at.elapsed() < DOUBLE_CLICK_WINDOW
                        && x.abs_diff(mouse.column) <= 1
                        && y == mouse.row
                });
                self.last_click = Some((Instant::now(), mouse.column, mouse.row));
                if double {
                    self.select_word_at(p);
                } else {
                    self.sel = Some(Selection { anchor: p, head: p });
                    self.selecting = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                // Edge auto-scroll (grok: compute_autoscroll): dragging
                // past the pane keeps scrolling while events arrive.
                let a = self.chat_view.area;
                if mouse.row < a.y {
                    self.scroll_by(2);
                } else if mouse.row >= a.y.saturating_add(a.height) {
                    self.scroll_by(-2);
                }
                let head = self.chat_clamp(mouse.column, mouse.row);
                if let Some(sel) = &mut self.sel {
                    sel.head = head;
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting => {
                self.selecting = false;
                self.finish_selection();
            }
            MouseEventKind::Drag(MouseButton::Left) if self.input_selecting => {
                // The head snaps to the well's edges: a drag above the well
                // selects to its top visible row, below it to the bottom row.
                let head = self.input_cell_at(mouse.column, mouse.row);
                if let Some(sel) = &mut self.input_sel {
                    sel.head = head;
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Up(MouseButton::Left) if self.input_selecting => {
                self.input_selecting = false;
                self.finish_input_selection();
            }
            MouseEventKind::Moved => {
                // grok-style hover: track which inline chip the pointer is
                // over; redraw only on changes (mouse moves are a firehose).
                let hover = self.chip_at(mouse.column, mouse.row);
                if hover != self.hover_att {
                    self.hover_att = hover;
                    self.needs_redraw = true;
                }
                // The todo progress chip is a button: brighten it under the
                // pointer so the dialog is discoverable without a tooltip.
                let chip_hover = self.plan_chip_at(mouse.column, mouse.row) && !self.modal_open();
                if chip_hover != self.hover_plan_chip {
                    self.hover_plan_chip = chip_hover;
                    self.needs_redraw = true;
                }
                // Same affordance for the `↥` prompt-jump glyph.
                let jump_hover =
                    self.prompt_jump_btn_hit(mouse.column, mouse.row) && !self.modal_open();
                if jump_hover != self.hover_prompt_jump_btn {
                    self.hover_prompt_jump_btn = jump_hover;
                    self.needs_redraw = true;
                }
                // …and for the `⛶` expand glyph beside it.
                let expand_hover =
                    self.expand_btn_hit(mouse.column, mouse.row) && !self.modal_open();
                if expand_hover != self.hover_expand_btn {
                    self.hover_expand_btn = expand_hover;
                    self.needs_redraw = true;
                }
                // …and for the meta row's `↓ N` scroll chip.
                let scroll_hover =
                    self.scroll_btn_hit(mouse.column, mouse.row) && !self.modal_open();
                if scroll_hover != self.hover_scroll_btn {
                    self.hover_scroll_btn = scroll_hover;
                    self.needs_redraw = true;
                }
            }
            _ => {}
        }
    }

    /// Char-index spans of live `[image n]` tokens in the draft, sorted by
    /// position: `(start, end_exclusive, attachment idx)`.
    pub fn token_spans(&self) -> Vec<(usize, usize, usize)> {
        let buf = &self.input.buf();
        let mut spans = Vec::new();
        for (idx, att) in self.pending_images.iter().enumerate() {
            if let Some(byte) = buf.find(&att.token) {
                let start = buf[..byte].chars().count();
                spans.push((start, start + att.token.chars().count(), idx));
            }
        }
        spans.sort_unstable();
        spans
    }

    /// Cut the whole token when `cursor` deletes into one (backward: the
    /// char left of the cursor; forward: the char at it). Returns whether
    /// a token was cut.
    fn delete_token_at(&mut self, cursor: usize, backward: bool) -> bool {
        let probe = if backward {
            let Some(p) = cursor.checked_sub(1) else {
                return false;
            };
            p
        } else {
            cursor
        };
        let Some(&(start, end, idx)) = self
            .token_spans()
            .iter()
            .find(|(s, e, _)| probe >= *s && probe < *e)
        else {
            return false;
        };
        self.input.delete_char_range(start, end);
        if let Some(att) = self.pending_images.remove(idx) {
            self.show_tip(
                self.locale
                    .tr("removed {n}", "已移除 {n}")
                    .replace("{n}", &att.name),
            );
        }
        true
    }

    /// Drop attachments whose token no longer survives in the draft text.
    fn reconcile_attachments(&mut self) {
        if self.pending_images.reconcile(&self.input.buf()) > 0 {
            self.hover_att = None;
            self.needs_redraw = true;
        }
    }

    /// The chip to preview: mouse hover wins, else the chip the text
    /// cursor sits in or immediately after (“光标在附近”).
    pub fn preview_att(&self) -> Option<usize> {
        if let Some(idx) = self.hover_att {
            return Some(idx);
        }
        let c = self.input.cursor_char();
        self.token_spans()
            .iter()
            .find(|(s, e, _)| c >= *s && c <= *e)
            .map(|&(_, _, idx)| idx)
    }

    /// Hit-test a screen cell against the inline chips drawn this frame.
    fn chip_at(&self, col: u16, row: u16) -> Option<usize> {
        self.att_chips
            .iter()
            .find(|(r, _)| {
                col >= r.x
                    && col < r.x.saturating_add(r.width)
                    && row >= r.y
                    && row < r.y.saturating_add(r.height)
            })
            .map(|(_, idx)| *idx)
    }

    /// Hit-test a screen cell against the cap row's todo progress chip drawn
    /// this frame.
    fn plan_chip_at(&self, col: u16, row: u16) -> bool {
        self.plan_chip.is_some_and(|r| {
            col >= r.x
                && col < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// A modal owns the screen: clicks must not reach the chrome behind it.
    fn modal_open(&self) -> bool {
        self.todo_dialog.is_some()
            || self.view_overlay.is_some()
            || self.permission_ask.is_some()
            || self.picker.is_some()
    }

    /// Hit-test a screen cell against the cap row's `↥` prompt-jump glyph
    /// drawn this frame.
    fn prompt_jump_btn_hit(&self, col: u16, row: u16) -> bool {
        self.prompt_jump_btn.is_some_and(|r| {
            col >= r.x
                && col < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// Hit-test a screen cell against the cap row's `⛶` expand glyph drawn
    /// this frame.
    fn expand_btn_hit(&self, col: u16, row: u16) -> bool {
        self.expand_btn.is_some_and(|r| {
            col >= r.x
                && col < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// Hit-test a screen cell against the meta row's `↓ N` scroll chip drawn
    /// this frame (absent while the tail is already on screen).
    fn scroll_btn_hit(&self, col: u16, row: u16) -> bool {
        self.scroll_btn.is_some_and(|r| {
            col >= r.x
                && col < r.x.saturating_add(r.width)
                && row >= r.y
                && row < r.y.saturating_add(r.height)
        })
    }

    /// `↥` click: jump the chat view to a user prompt. The first click goes to
    /// the newest prompt, each further click walks one prompt back, and the
    /// oldest wraps to the newest again. The target is anchored to the top of
    /// the chat pane and flashed for [`PROMPT_FLASH_TTL`]; the last target is
    /// remembered in memory only, so jumping resumes where it stopped.
    fn jump_to_user_prompt(&mut self) {
        let area = self.chat_view.area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Same inputs as `ui::draw_chat`: the line indices must match the
        // frame the anchor lands on (thumbnail rows included).
        let layout = self.displayed_transcript().layout(
            &self.theme,
            area.width,
            self.spinner(),
            crate::pet::kitty_supported(),
        );
        if layout.users.is_empty() {
            self.show_tip(self.locale.tr(
                "no user prompts yet — ↥ finds them once you send one",
                "还没有用户输入 —— 发送后 ↥ 即可跳转",
            ));
            return;
        }
        // The running indicator rides as one extra tail line in `draw_chat`;
        // include it so the anchored viewport matches the next frame.
        let total = layout.lines.len() + usize::from(crate::ui::state_line_shown(self));
        let h = area.height as usize;
        let max_scroll = total.saturating_sub(h);
        let target = match self
            .prompt_jump_cell
            .and_then(|cell| layout.users.iter().position(|p| p.cell == cell))
        {
            // A previous jump: continue walking backward from it…
            Some(rank) if rank > 0 => layout.users[rank - 1],
            // …and the oldest prompt wraps back to the newest.
            Some(_) => layout.users[layout.users.len() - 1],
            // No previous jump (fresh session, view switch, or the target cell
            // is gone): start at the newest prompt.
            None => layout.users[layout.users.len() - 1],
        };
        let from_newest = layout
            .users
            .iter()
            .rposition(|p| p.cell == target.cell)
            .map(|rank| layout.users.len() - rank)
            .unwrap_or(1);
        self.prompt_jump_cell = Some(target.cell);
        // Anchor the prompt's first line to the top of the chat pane and flash
        // its rows for a few seconds.
        let start = target.line.min(max_scroll);
        let end = start.saturating_add(h).min(total);
        self.scroll_up = total.saturating_sub(end);
        self.prompt_flash = Some((target.cell, Instant::now() + PROMPT_FLASH_TTL));
        self.needs_redraw = true;
        self.show_tip(format!(
            "↥ {} {from_newest}/{} · {}",
            self.locale.tr("user prompt", "用户输入"),
            layout.users.len(),
            self.locale.tr("newest first", "从最新往前"),
        ));
    }

    /// Open the todo progress dialog for the live checklist (the cap row's
    /// progress chip). A cleared plan has nothing to show and never opens.
    fn open_todo_dialog(&mut self) {
        if self.plan.is_none() {
            return;
        }
        self.todo_dialog = Some(TodoDialog { scroll: 0 });
        self.needs_redraw = true;
    }

    /// Scroll the open todo dialog (wheel and keyboard share this path); the
    /// renderer clamps to the content height, so a large value reaches the end.
    fn todo_dialog_scroll_by(&mut self, delta: i64) {
        if let Some(dialog) = self.todo_dialog.as_mut() {
            if delta < 0 {
                dialog.scroll = dialog.scroll.saturating_sub(delta.unsigned_abs() as usize);
            } else {
                dialog.scroll = dialog.scroll.saturating_add(delta as usize);
            }
            self.needs_redraw = true;
        }
    }

    /// Hit-test a screen cell against the chat pane; `None` outside it.
    fn chat_hit(&self, col: u16, row: u16) -> Option<SelPoint> {
        let a = self.chat_view.area;
        if self.chat_view.lines.is_empty()
            || col < a.x
            || col >= a.x.saturating_add(a.width)
            || row < a.y
            || row >= a.y.saturating_add(a.height)
        {
            return None;
        }
        // The snapshot covers only the viewport: clamp the screen row to it
        // (content shorter than the pane) — absolute line = top + rel.
        let rel = ((row - a.y) as usize).min(self.chat_view.lines.len() - 1);
        Some(SelPoint {
            line: self.chat_view.top + rel,
            col: (col - a.x) as usize,
        })
    }

    /// Like `chat_hit`, but clamps to the pane so drags outside it still
    /// extend the selection to the nearest edge.
    fn chat_clamp(&self, col: u16, row: u16) -> SelPoint {
        let a = self.chat_view.area;
        let col = col.clamp(a.x, a.x.saturating_add(a.width.saturating_sub(1)));
        let row = row.clamp(a.y, a.y.saturating_add(a.height.saturating_sub(1)));
        self.chat_hit(col, row)
            .unwrap_or(SelPoint { line: 0, col: 0 })
    }

    /// The transcript cell that owns the line under a screen cell, if any.
    fn tool_at(&self, col: u16, row: u16) -> Option<usize> {
        let p = self.chat_hit(col, row)?;
        self.chat_view.line_owner(p.line)
    }

    /// Mouse wheel always scrolls the conversation, including over tool cards.
    fn mouse_scroll(&mut self, delta: i64, _col: u16, _row: u16) {
        self.scroll_by(delta);
    }

    /// Scroll the open plugin view overlay (wheel and keyboard share this
    /// path). The renderer clamps to the actual content height, so a large
    /// value (End) reliably reaches the bottom.
    fn view_scroll_by(&mut self, delta: i64) {
        if let Some(view) = self.view_overlay.as_mut() {
            if delta < 0 {
                view.scroll = view.scroll.saturating_sub(delta.unsigned_abs() as usize);
            } else {
                view.scroll = view.scroll.saturating_add(delta as usize);
            }
            self.needs_redraw = true;
        }
    }

    /// Toggle a tool between its collapsed viewport and full expansion.
    fn toggle_tool(&mut self, ci: usize) {
        let expanded = {
            let Some(cell) = self.displayed_transcript_mut().cells.get_mut(ci) else {
                return;
            };
            cell.expanded = !cell.expanded;
            cell.expanded
        };
        let label = self.locale.tr(
            if expanded { "expanded" } else { "collapsed" },
            if expanded { "已展开" } else { "已折叠" },
        );
        self.show_tip(format!(
            "{label} {}",
            self.locale
                .tr("tool output · click toggles", "工具输出 · 点击切换")
        ));
        self.needs_redraw = true;
    }

    /// grok `finish_text_drag`: reconstruct the dragged text and copy it —
    /// the highlight persists only when something actually reached the
    /// clipboard path. A plain click (caret) just clears the highlight.
    fn finish_selection(&mut self) {
        self.needs_redraw = true;
        let Some(sel) = self.sel else { return };
        if sel.is_caret() {
            self.sel = None;
            return;
        }
        let text = self.selection_text(sel);
        if text.trim().is_empty() {
            self.sel = None;
            return;
        }
        self.copy_text(&text);
    }

    fn copy_text(&mut self, text: &str) {
        let chars = text.chars().count();
        if crate::clipboard::copy(text) {
            self.show_tip(
                self.locale
                    .tr(
                        "✓ copied {n} chars — esc clears the highlight",
                        "✓ 已复制 {n} 个字符 —— esc 清除高亮",
                    )
                    .replace("{n}", &chars.to_string()),
            );
        } else {
            self.show_tip(self.locale.tr(
                "copy failed — hold shift and drag for the terminal's native selection",
                "复制失败 —— 按住 shift 拖动可用终端自带的选择",
            ));
        }
    }

    /// The composer well's text width — the same wrap width the widget was
    /// laid out with this frame (`ui::draw_input`).
    fn input_avail(&self) -> usize {
        self.composer_wrap_width.max(1)
    }

    /// Is this screen cell inside the composer well?
    fn input_hit(&self, col: u16, row: u16) -> bool {
        let a = self.composer_area;
        a.width > 0
            && a.height > 0
            && col >= a.x
            && col < a.right()
            && row >= a.y
            && row < a.bottom()
    }

    /// Map a screen cell to a well-local `(row, col)` in the same coordinates
    /// as [`ComposerEditor::screen_to_char`] (prompt column and the well's
    /// viewport scroll applied), clamped into the visible text area.
    fn input_cell_at(&self, col: u16, row: u16) -> (usize, usize) {
        let a = self.composer_area;
        let prompt = "❯ ".width() as u16;
        let rel_row = (row.saturating_sub(a.y) as usize)
            .min(a.height.saturating_sub(1) as usize)
            .saturating_add(self.input_top);
        let rel_col = (col.saturating_sub(a.x.saturating_add(prompt)) as usize)
            .min(self.input_avail().saturating_sub(1));
        (rel_row, rel_col)
    }

    /// Ordered char boundaries covered by the composer drag-selection — both
    /// endpoint cells inclusive, so either drag direction covers exactly the
    /// cells the pointer crossed. `None` without a selection.
    pub(crate) fn input_selection_range(&mut self) -> Option<(usize, usize)> {
        let sel = self.input_sel?;
        let (s, e) = if sel.anchor <= sel.head {
            (sel.anchor, sel.head)
        } else {
            (sel.head, sel.anchor)
        };
        let avail = self.input_avail();
        let start = self.input.screen_to_char(avail, s.0, s.1);
        let end = self.input.screen_to_char_end(avail, e.0, e.1);
        (start < end).then_some((start, end))
    }

    /// Copy the dragged composer selection; the highlight persists until the
    /// next click or Esc, mirroring the chat pane. A plain click (a caret)
    /// just clears the highlight.
    fn finish_input_selection(&mut self) {
        self.needs_redraw = true;
        let Some(sel) = self.input_sel else { return };
        if sel.anchor == sel.head {
            self.input_sel = None;
            return;
        }
        let Some((a, b)) = self.input_selection_range() else {
            self.input_sel = None;
            return;
        };
        let text = self.input.chars_between(a, b);
        if text.trim().is_empty() {
            self.input_sel = None;
            return;
        }
        self.copy_text(&text);
    }

    /// Extract the selected text from the layout snapshot: cell-range slices
    /// per line, trailing whitespace trimmed, joined with newlines.
    pub fn selection_text(&self, sel: Selection) -> String {
        let lines = &self.chat_view.lines;
        if lines.is_empty() {
            return String::new();
        }
        let top = self.chat_view.top;
        let (s, e) = sel.ordered();
        if e.line < top {
            return String::new(); // selection entirely above the viewport
        }
        let first = s.line.saturating_sub(top);
        let last = (e.line - top).min(lines.len() - 1);
        if first > last {
            return String::new(); // starts below the captured viewport
        }
        let mut out = Vec::with_capacity(last - first + 1);
        for (li, text) in lines.iter().enumerate().take(last + 1).skip(first) {
            let c0 = if li + top == s.line { s.col } else { 0 };
            let c1 = if li + top == e.line {
                e.col + 1
            } else {
                usize::MAX
            };
            out.push(slice_by_cells(text, c0, c1).trim_end().to_string());
        }
        out.join("\n")
    }

    /// Double-click: select the whitespace-delimited word under the pointer
    /// and copy it right away (grok's word select & copy).
    fn select_word_at(&mut self, p: SelPoint) {
        let Some(line) = self.chat_view.line_text(p.line) else {
            return;
        };
        let Some((col, width, word)) = word_span(line, p.col) else {
            self.sel = None;
            return;
        };
        self.sel = Some(Selection {
            anchor: SelPoint { line: p.line, col },
            head: SelPoint {
                line: p.line,
                col: col + width - 1,
            },
        });
        self.selecting = false;
        let word = word.clone();
        self.copy_text(&word);
    }

    /// Where the per-workspace mode cache lives (beside the session logs).
    fn modes_cache_path(cfg: &RuntimeConfig) -> std::path::PathBuf {
        std::path::Path::new(&cfg.home).join("abylab-modes.json")
    }

    /// Last-known mode facts for this workspace; `plan` never carries over
    /// (it is a per-session switch).
    fn load_modes_cache(cfg: &RuntimeConfig) -> Option<Modes> {
        let text = std::fs::read_to_string(Self::modes_cache_path(cfg)).ok()?;
        let root: serde_json::Value = serde_json::from_str(&text).ok()?;
        let entry = root.get("workspaces")?.get(&cfg.workspace)?;
        let mut modes: Modes = serde_json::from_value(entry.clone()).ok()?;
        modes.plan = false;
        Some(modes)
    }

    /// Merge this workspace's mode facts into the cache file; failures are
    /// silent (the cache is a convenience, never a requirement).
    fn save_modes_cache(&self) {
        let path = Self::modes_cache_path(&self.cfg);
        let mut root: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !root.is_object() {
            root = serde_json::json!({});
        }
        let Ok(entry) = serde_json::to_value(&self.modes) else {
            return;
        };
        root["workspaces"][&self.cfg.workspace] = entry;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, root.to_string());
    }

    fn locale_settings_path(cfg: &RuntimeConfig) -> std::path::PathBuf {
        settings_path(&cfg.home)
    }

    fn load_settings(cfg: &RuntimeConfig) -> UiSettings {
        UiSettings::load(&cfg.home)
    }

    /// Persist the durable UI preferences (language, model, effort,
    /// permission, appearance) into `$ABYLAB_HOME/settings.json`. Failures
    /// are silent: settings are a convenience, never a requirement.
    fn save_settings(&self) {
        let path = Self::locale_settings_path(&self.cfg);
        let settings = UiSettings {
            language: self.locale,
            model: Some(self.cfg.model.clone()),
            effort: self.modes.effort.clone(),
            permission: self.modes.permission.clone(),
            theme: Some(self.theme.mode.as_str().to_string()),
            palette: Some(self.active_palette_id.clone()),
        };
        let Ok(text) = serde_json::to_string_pretty(&settings) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }

    /// `/login <apikey>` — persist the key in the managed credential store
    /// and hand it to the running driver. Mirrors DeepSeek Harness: keys are
    /// write-only, so confirmations carry a redacted descriptor, never the
    /// literal value.
    fn login(&mut self, arg: &str, ctl: &Controller) {
        let key = arg.trim();
        if key.is_empty() {
            let status = match self.cfg.credential_source() {
                Some(source) => format!("api key present · {source}"),
                None => self
                    .locale
                    .tr(
                        "no key stored — usage: /login <apikey>",
                        "未存储 key — 用法：/login <apikey>",
                    )
                    .to_string(),
            };
            self.transcript.push_notice(NoticeLevel::Info, status);
            self.needs_redraw = true;
            return;
        }
        match crate::credentials::store_key(&self.cfg.home, crate::credentials::API_KEY_REF, key) {
            Ok(()) => {
                self.cfg.api_key = Some(key.to_string());
                self.cfg.key_origin = Some(crate::runtime::KeyOrigin::Stored);
                ctl.send(Cmd::SetApiKey {
                    key: Some(key.to_string()),
                });
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    format!(
                        "{} · {} · {}",
                        self.locale.tr("api key stored", "api key 已保存"),
                        crate::credentials::redact(key),
                        crate::credentials::CREDENTIALS_FILENAME,
                    ),
                );
            }
            Err(err) => {
                self.transcript.push_notice(
                    NoticeLevel::Error,
                    format!("{} · {err}", self.locale.tr("login failed", "login 失败")),
                );
            }
        }
        self.needs_redraw = true;
    }

    /// `/logout` — remove the stored credential from the managed store. A
    /// `--api-key` launch override is read-only by design (this run's
    /// explicit intent cannot be edited from inside), so it keeps running
    /// and a notice says so; a stored key that is live is dropped by the
    /// driver at the next `/new` or restart (abycore cannot run keyless).
    fn logout(&mut self, ctl: &Controller) {
        let deleted =
            match crate::credentials::delete_key(&self.cfg.home, crate::credentials::API_KEY_REF) {
                Ok(deleted) => deleted,
                Err(err) => {
                    self.transcript.push_notice(
                        NoticeLevel::Error,
                        format!("{} · {err}", self.locale.tr("logout failed", "logout 失败")),
                    );
                    self.needs_redraw = true;
                    return;
                }
            };
        let live_stored = matches!(self.cfg.key_origin, Some(crate::runtime::KeyOrigin::Stored));
        if live_stored {
            self.cfg.api_key = None;
            self.cfg.key_origin = None;
            ctl.send(Cmd::SetApiKey { key: None });
        }
        let text = if deleted {
            self.locale
                .tr("stored api key removed", "已删除保存的 api key")
        } else if live_stored {
            self.locale.tr(
                "no stored key — the live key was cleared",
                "没有已保存的 key — 已清除运行中的 key",
            )
        } else {
            self.locale.tr("no stored key", "没有已保存的 key")
        };
        let note = match self.cfg.key_origin {
            Some(crate::runtime::KeyOrigin::Flag) => {
                " · --api-key flag key keeps running".to_string()
            }
            _ => String::new(),
        };
        self.transcript
            .push_notice(NoticeLevel::Info, format!("{text}{note}"));
        self.needs_redraw = true;
    }

    fn set_locale(&mut self, arg: &str) {
        let next = if arg.trim().is_empty() {
            self.locale.alternate()
        } else if let Some(locale) = Locale::parse(arg) {
            locale
        } else {
            self.show_tip(
                self.locale
                    .tr("usage: /lang [zh|en]", "用法：/lang [zh|en]"),
            );
            return;
        };
        self.locale = next;
        self.transcript.set_locale(next);
        for view in &mut self.subagents {
            view.transcript.set_locale(next);
        }
        self.save_settings();
        self.show_tip(match next {
            Locale::En => "Language switched to English",
            Locale::Zh => "界面语言已切换为中文",
        });
        self.needs_redraw = true;
    }

    pub fn scroll_by(&mut self, delta: i64) {
        let cur = self.scroll_up as i64;
        self.scroll_up = (cur + delta).max(0) as usize; // clamped to content in ui::draw
        self.needs_redraw = true;
    }

    /// The todo dialog is a read-only review pane: arrows/wheel scroll it and
    /// esc/enter close it, exactly like the view overlay.
    fn handle_todo_dialog_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.todo_dialog_scroll_by(-1),
            KeyCode::Down => self.todo_dialog_scroll_by(1),
            KeyCode::PageUp => self.todo_dialog_scroll_by(-5),
            KeyCode::PageDown => self.todo_dialog_scroll_by(5),
            KeyCode::Home => {
                if let Some(dialog) = self.todo_dialog.as_mut() {
                    dialog.scroll = 0;
                    self.needs_redraw = true;
                }
            }
            KeyCode::End => {
                // Render clamps to the content height, so `usize::MAX` is
                // reliably the bottom of the checklist.
                if let Some(dialog) = self.todo_dialog.as_mut() {
                    dialog.scroll = usize::MAX;
                    self.needs_redraw = true;
                }
            }
            KeyCode::Esc | KeyCode::Enter if key.modifiers == KeyModifiers::NONE => {
                self.todo_dialog = None;
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    fn handle_view_key(&mut self, key: KeyEvent) {
        // Scroll keys work regardless of modifier bits: terminals that
        // report arrows with modifiers (kitty keyboard protocol and friends)
        // must still scroll the view. The view has no other arrow bindings.
        match key.code {
            KeyCode::Up => {
                self.view_scroll_by(-1);
                return;
            }
            KeyCode::Down => {
                self.view_scroll_by(1);
                return;
            }
            KeyCode::PageUp => {
                self.view_scroll_by(-5);
                return;
            }
            KeyCode::PageDown => {
                self.view_scroll_by(5);
                return;
            }
            KeyCode::Home => {
                if let Some(view) = self.view_overlay.as_mut() {
                    view.scroll = 0;
                    self.needs_redraw = true;
                }
                return;
            }
            KeyCode::End => {
                // Render clamps to the content height, so `usize::MAX` is
                // reliably the bottom of the view.
                if let Some(view) = self.view_overlay.as_mut() {
                    view.scroll = usize::MAX;
                    self.needs_redraw = true;
                }
                return;
            }
            _ => {}
        }
        if key.modifiers != KeyModifiers::NONE {
            return;
        }
        // Esc/Enter close the view: `/keys` and other builtin chrome own the
        // modal entirely; no host notification leaves the painter.
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
            self.view_overlay = None;
        }
    }

    fn handle_key(&mut self, key: KeyEvent, ctl: &Controller) {
        self.handle_key_inner(key, ctl);
        // Every handled key ends here: Esc, a closed list or a moved
        // highlight drops a stale theme preview (see `handle` for the
        // non-key events).
        self.reconcile_theme_preview();
    }

    fn handle_key_inner(&mut self, key: KeyEvent, ctl: &Controller) {
        self.needs_redraw = true;
        // DSH_TUI_KEYDEBUG=1: surface exactly what the terminal delivered
        // (after CG rescue) in the tip row — kills keybinding mysteries.
        if self.key_debug {
            self.show_tip(format!("key: {:?} + {:?}", key.modifiers, key.code));
        }

        // ACP tool permission sits above session pickers (Backchat ask panel).
        if self.permission_ask.is_some() {
            self.handle_permission_ask_key(key);
            return;
        }

        if self.todo_dialog.is_some() {
            self.handle_todo_dialog_key(key);
            return;
        }

        if self.view_overlay.is_some() {
            self.handle_view_key(key);
            return;
        }

        // --- model picker overlay steals input first (grok modal semantics)
        if self.picker.is_some() {
            self.handle_picker_key(key, ctl);
            return;
        }

        if self.active_subagent.is_some() {
            if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::NONE {
                self.handle_esc(ctl);
                return;
            }
            if key.code == KeyCode::Down && key.modifiers == KeyModifiers::NONE {
                self.open_subagent_switcher();
                return;
            }
            let ctx = crate::input::KeyCtx {
                input_empty: true,
                history_active: false,
            };
            if let Some(action) = crate::input::classify(&key, ctx) {
                match action {
                    Action::Esc
                    | Action::Quit
                    | Action::ToggleTheme
                    | Action::ScrollHalfUp
                    | Action::ScrollHalfDown
                    | Action::PageUp
                    | Action::PageDown
                    | Action::JumpTop
                    | Action::JumpTail => self.dispatch(action, ctl),
                    _ => {}
                }
            }
            return;
        }

        if key.code == KeyCode::Down
            && key.modifiers == KeyModifiers::NONE
            && self.active_subagent.is_none()
            && self.input.is_empty()
            && self.input.hist_pos.is_none()
            && !self.subagents.is_empty()
        {
            self.open_subagent_switcher();
            return;
        }

        // Vim mode: plain keys become vim commands, but only with no
        // modal above (overlays and forms keep their own key handling; a
        // vim Insert-mode Esc cancels the ask/picker instead of toggling
        // vim, and normal-mode letters never land in a hidden composer).
        if self.vim.is_active() && self.vim.handle_key(&key, &mut self.input) {
            self.reconcile_attachments();
            self.refresh_file_menu();
            return;
        }

        // The @file browser owns its navigation keys while open; everything
        // else falls through to normal editing (which re-syncs the browser
        // through `refresh_file_menu`). Enter settles, Tab drills into a
        // directory, → follows the explorer's enter semantics.
        if let Some(menu) = &mut self.file_menu {
            use crate::file_ref::{navigate, ExplorerInput as XIn};
            // ctrl+h toggles hidden files/dirs (the explorer's own binding
            // for ToggleShowHidden); it stays modal while the browser is
            // open, like the navigation keys.
            if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('h') {
                navigate(menu, XIn::ToggleShowHidden);
                return;
            }
            if key.modifiers == KeyModifiers::NONE {
                match key.code {
                    KeyCode::Up => {
                        navigate(menu, XIn::Up);
                        return;
                    }
                    KeyCode::Down => {
                        navigate(menu, XIn::Down);
                        return;
                    }
                    KeyCode::Home => {
                        navigate(menu, XIn::Home);
                        return;
                    }
                    KeyCode::End => {
                        navigate(menu, XIn::End);
                        return;
                    }
                    KeyCode::PageUp => {
                        navigate(menu, XIn::PageUp);
                        return;
                    }
                    KeyCode::PageDown => {
                        navigate(menu, XIn::PageDown);
                        return;
                    }
                    KeyCode::Left => {
                        navigate(menu, XIn::Left);
                        return;
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        self.file_menu_drill();
                        return;
                    }
                    KeyCode::Enter => {
                        self.file_menu_settle();
                        return;
                    }
                    KeyCode::Esc => {
                        self.dismiss_file_menu();
                        return;
                    }
                    _ => {}
                }
            }
        }

        // The slash menu owns vertical arrows while it is visible. Ordinary
        // non-empty drafts use them for visual-line cursor motion below.
        if key.modifiers == KeyModifiers::NONE && self.input.buf().starts_with('/') {
            let n = self.slash_matches().len();
            if n > 0 {
                match key.code {
                    KeyCode::Up => self.slash_sel = self.slash_sel.checked_sub(1).unwrap_or(n - 1),
                    KeyCode::Down => self.slash_sel = (self.slash_sel + 1) % n,
                    _ => {}
                }
                if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                    // A `/theme ` candidate row previews the palette the
                    // highlight just landed on (mirrors the theme dialog);
                    // the dark/light rows name no pack and revert instead.
                    self.preview_slash_theme();
                    return;
                }
            }
        }

        // The queue editor owns ctrl+d while an item is loaded (the keymap
        // would read it as delete-forward): first press arms, second deletes.
        if self.queue_edit.is_some()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('d')
        {
            self.delete_queue_edit(ctl);
            return;
        }

        let ctx = crate::input::KeyCtx {
            input_empty: self.input.is_empty(),
            // While a history entry is on screen, ↑/↓ keep browsing it instead
            // of moving inside the recalled draft; any edit clears `hist_pos`
            // in the editor and hands the arrows back to cursor motion.
            history_active: self.input.hist_pos.is_some(),
        };
        if let Some(action) = crate::input::classify(&key, ctx) {
            self.dispatch(action, ctl);
        }
        // Any edit may have cut an [image n] token — the tray follows the
        // text (grok's lexicon-scan model).
        self.reconcile_attachments();
        // Any edit may open/close/re-anchor the @file browser.
        self.refresh_file_menu();
    }

    /// Re-sync the @file browser with the draft: open on a fresh `@` token,
    /// re-anchor on edits, close when the token is gone.
    fn refresh_file_menu(&mut self) {
        // Vim normal mode is command editing — no mention browser.
        if self.vim.is_active() && self.vim.mode == crate::input::VimMode::Normal {
            self.file_menu = None;
            return;
        }
        // The slash menu and the @ menu are mutually exclusive.
        if self.slash_completion_open() {
            self.file_menu = None;
            return;
        }
        let (row, col) = self.input.char_to_rowcol(self.input.cursor_char());
        let Some(line) = self.input.lines().get(row).cloned() else {
            self.file_menu = None;
            self.file_menu_dismissed = None;
            return;
        };
        let Some(token) = crate::file_ref::active_at_token(&line, col) else {
            // The token is gone (draft cleared, caret left it): a fresh
            // `@` must be able to reopen the browser, so drop the
            // dismissal tag along with the menu.
            self.file_menu = None;
            self.file_menu_dismissed = None;
            return;
        };
        let tag = crate::file_ref::token_tag(token.quoted, &token.query);
        if let Some(menu) = &mut self.file_menu {
            if menu.row() != row || menu.start() != token.start || menu.end() != token.end {
                menu.retoken(row, &token);
            }
            menu.apply_query(&token.query);
        } else {
            // Esc-dismissed tokens stay closed until their text changes.
            if self.file_menu_dismissed.as_deref() == Some(tag.as_str()) {
                return;
            }
            self.file_menu_dismissed = None;
            if let Some(mut menu) = crate::file_ref::FileMenu::open(
                std::path::Path::new(&self.cfg.workspace),
                row,
                &token,
            ) {
                // The token may already carry a query (dismissed-token
                // reopen): drive the browser to it before showing.
                menu.apply_query(&token.query);
                self.file_menu = Some(menu);
            }
        }
    }

    /// `Enter` on the selected entry: replace the token with `@path` (or
    /// `@dir/` for directories) and close the browser.
    fn file_menu_settle(&mut self) {
        let Some(mention) = self
            .file_menu
            .as_ref()
            .and_then(|menu| menu.current_mention())
        else {
            return;
        };
        let (row, start, end) = {
            let menu = self.file_menu.as_ref().expect("checked above");
            (menu.row(), menu.start(), menu.end())
        };
        crate::file_ref::replace_span(&mut self.input, row, start, end, &mention);
        self.file_menu = None;
        self.reconcile_attachments();
    }

    /// `Tab` on a directory: rewrite the token to `@dir/` (quoted form
    /// keeps the quote open) and keep the browser inside the directory.
    /// `Tab` on a file settles like `Enter`.
    fn file_menu_drill(&mut self) {
        let (mention, row, start, end) = {
            let Some(menu) = self.file_menu.as_ref() else {
                return;
            };
            if menu.explorer().files().is_empty() {
                return;
            }
            let file = menu.explorer().current().clone();
            if !file.is_dir {
                self.file_menu_settle();
                return;
            }
            let rel = crate::file_ref::relative_path(menu.base(), &file.path);
            let Some(mention) = crate::file_ref::format_file_mention(&rel, true, menu.quoted())
            else {
                return;
            };
            (mention, menu.row(), menu.start(), menu.end())
        };
        crate::file_ref::replace_span(&mut self.input, row, start, end, &mention);
        // Re-derive the token from the edited line instead of parsing the
        // mention text by hand: the caret-token grammar owns the `@`/quote
        // stripping, and `refresh_file_menu` re-anchors the menu's span and
        // drives the browser to the new query (`@src/` descends into `src/`).
        self.refresh_file_menu();
    }

    /// Esc: close the browser and remember the token so it stays closed
    /// until its text changes.
    fn dismiss_file_menu(&mut self) {
        if let Some(menu) = &self.file_menu {
            self.file_menu_dismissed = Some(crate::file_ref::token_tag(
                menu.quoted(),
                menu.token_query(),
            ));
        }
        self.file_menu = None;
    }

    /// Apply one classified [`Action`] — the only place key semantics touch
    /// app state, so `input::keymap` stays a pure table.
    fn dispatch(&mut self, action: Action, ctl: &Controller) {
        if matches!(
            action,
            Action::Insert(_)
                | Action::Newline
                | Action::Backspace
                | Action::DeleteForward
                | Action::DeleteWordBack
                | Action::KillToEnd
                | Action::KillToStart
                | Action::KillLine
                | Action::Undo
                | Action::Redo
                | Action::YankPaste
                | Action::SelectLeft
                | Action::SelectRight
                | Action::SelectUp
                | Action::SelectDown
                | Action::SelectWordLeft
                | Action::SelectWordRight
                | Action::SelectLineStart
                | Action::SelectLineEnd
        ) {
            // Text edits invalidate the drag-selection highlight: the cells it
            // covered no longer describe the same text.
            self.input_sel = None;
        }
        match action {
            Action::Insert(ch) => {
                self.input.insert_char(ch);
                self.slash_sel = 0;
            }
            Action::Newline => self.input.insert_newline(),
            Action::Enter => {
                // The queue editor owns enter: it saves the edited item
                // instead of sending it.
                if self.queue_edit.is_some() {
                    self.save_queue_edit(ctl);
                    return;
                }
                // An empty draft promotes the FIFO head into the active turn.
                if self.input.is_empty()
                    && self.pending_images.is_empty()
                    && !self.prompt_queue.is_empty()
                {
                    self.send_queue_head_now(ctl);
                    return;
                }
                let menu = self.slash_matches();
                if !menu.is_empty() {
                    let entry = menu[self.slash_sel.min(menu.len() - 1)].clone();
                    self.accept_slash(&entry, ctl);
                } else {
                    self.submit(ctl);
                }
            }
            Action::TabComplete => {
                let menu = self.slash_matches();
                if !menu.is_empty() {
                    let entry = &menu[self.slash_sel.min(menu.len() - 1)];
                    self.input.set(
                        entry
                            .completion
                            .clone()
                            .unwrap_or_else(|| format!("/{} ", entry.name)),
                    );
                }
            }
            Action::Esc => self.handle_esc(ctl),
            Action::CtrlC => self.handle_ctrl_c(ctl),
            Action::Quit => self.quit = true,
            Action::ClearScrollback => {
                self.transcript.clear();
                self.sel = None;
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    self.locale.tr("scrollback cleared", "滚动区已清空").into(),
                );
            }
            Action::ToggleTheme => {
                self.theme = self.theme.toggled();
                self.save_settings();
                self.show_tip(format!(
                    "{}: {} {}",
                    self.locale.tr("theme", "主题"),
                    self.active_palette_id,
                    self.theme.mode.as_str()
                ));
            }
            Action::ToggleExpandAll => {
                self.transcript.expand_all = !self.transcript.expand_all;
                self.show_tip(if self.transcript.expand_all {
                    self.locale.tr(
                        "expanded all thoughts and tool results",
                        "已展开全部思考与工具结果",
                    )
                } else {
                    self.locale.tr(
                        "collapsed all thoughts and tool results",
                        "已折叠全部思考与工具结果",
                    )
                });
            }
            Action::SendNow => self.send_now(ctl),
            Action::EditQueuedPrompt => self.open_queue_selector(),
            Action::AttachClipboard => self.clip_image("", ctl),
            Action::ModelPicker => self.open_model_picker(ctl),
            Action::CyclePermission => self.cycle_permission(ctl),
            Action::HistoryPrev => self.history_prev(),
            Action::HistoryNext => self.history_next(),
            Action::ScrollHalfUp => self.scroll_by(10),
            Action::ScrollHalfDown => self.scroll_by(-10),
            Action::PageUp => self.scroll_by(20),
            Action::PageDown => self.scroll_by(-20),
            Action::JumpTop => self.scroll_up = usize::MAX,
            Action::JumpTail => self.scroll_up = 0,
            Action::CursorLeft => self.input.move_left(),
            Action::CursorRight => self.input.move_right(),
            Action::CursorUp => {
                // ↑ at the draft's first visual row recalls the input history
                // (the editor stashes the draft first, so ↓ restores it).
                let before = self.input.cursor_char();
                self.input.move_up();
                if self.input.cursor_char() == before {
                    self.history_prev_from_draft();
                }
            }
            Action::CursorDown => self.input.move_down(),
            Action::WordLeft => self.input.word_left(),
            Action::WordRight => self.input.word_right(),
            Action::LineStart => self.input.line_start(self.composer_wrap_width),
            Action::LineEnd => self.input.line_end(self.composer_wrap_width),
            Action::Backspace => {
                // Deleting into an inline chip cuts the whole [image n]
                // token (and un-stages that image) instead of one bracket.
                if !self.delete_token_at(self.input.cursor_char(), true) {
                    self.input.backspace();
                }
            }
            Action::DeleteForward => {
                if !self.delete_token_at(self.input.cursor_char(), false) {
                    self.input.delete_forward();
                }
            }
            Action::DeleteWordBack => self.input.delete_word_back(),
            Action::Undo => {
                self.input.undo();
            }
            Action::Redo => {
                self.input.redo();
            }
            Action::KillToEnd => self.input.kill_to_end(self.composer_wrap_width),
            Action::KillToStart => self.input.kill_to_start(self.composer_wrap_width),
            Action::KillLine => self.input.kill_line(),
            Action::YankPaste => {
                self.input.paste_yank();
            }
            Action::SelectLeft => self.input.select_left(),
            Action::SelectRight => self.input.select_right(),
            Action::SelectUp => self.input.select_up(),
            Action::SelectDown => self.input.select_down(),
            Action::SelectWordLeft => self.input.select_word_left(),
            Action::SelectWordRight => self.input.select_word_right(),
            Action::SelectLineStart => self.input.select_line_start(),
            Action::SelectLineEnd => self.input.select_line_end(),
            Action::CopySelection => {
                // The composer's keyboard selection, then its mouse drag.
                if let Some(text) = self.input.selection_text() {
                    if !text.trim().is_empty() {
                        self.input.copy_selection_to_yank();
                        self.copy_text(&text);
                    }
                } else if let Some((a, b)) = self.input_selection_range() {
                    let text = self.input.chars_between(a, b);
                    if !text.trim().is_empty() {
                        self.copy_text(&text);
                    }
                }
            }
            Action::CutSelection => {
                let nothing = self.locale.tr(
                    "nothing to cut — select with shift+arrows, or drag in the box",
                    "无可剪切 —— 用 shift+方向键或直接在输入框里拖选",
                );
                // The composer's keyboard selection, then its mouse drag.
                if let Some(text) = self.input.selection_text() {
                    if !text.trim().is_empty() {
                        self.input.cut_selection_to_yank();
                        self.copy_text(&text);
                    } else {
                        self.show_tip(nothing);
                    }
                } else if let Some((a, b)) = self.input_selection_range() {
                    let text = self.input.chars_between(a, b);
                    if !text.trim().is_empty() {
                        self.input.delete_char_range(a, b);
                        self.input_sel = None;
                        self.copy_text(&text);
                    } else {
                        self.show_tip(nothing);
                    }
                } else {
                    self.show_tip(nothing);
                }
            }
        }
    }

    fn handle_permission_ask_key(&mut self, key: KeyEvent) {
        let n = self
            .permission_ask
            .as_ref()
            .map(|ask| ask.options.len().max(1))
            .unwrap_or(1);
        match key.code {
            KeyCode::Esc => self.finish_permission_ask(PermissionAskReply::Cancelled),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.finish_permission_ask(PermissionAskReply::Cancelled);
            }
            KeyCode::Up => {
                if let Some(ask) = &mut self.permission_ask {
                    ask.sel = ask.sel.checked_sub(1).unwrap_or(n - 1);
                }
            }
            KeyCode::Down => {
                if let Some(ask) = &mut self.permission_ask {
                    ask.sel = (ask.sel + 1) % n;
                }
            }
            KeyCode::Enter => {
                let id = self
                    .permission_ask
                    .as_ref()
                    .and_then(|ask| ask.options.get(ask.sel).map(|o| o.option_id.clone()));
                self.finish_permission_ask(match id {
                    Some(id) => PermissionAskReply::Selected(id),
                    None => PermissionAskReply::Cancelled,
                });
            }
            _ => {}
        }
    }

    fn open_permission_ask(
        &mut self,
        title: String,
        options: Vec<PermissionAskOption>,
        reply: tokio::sync::oneshot::Sender<PermissionAskReply>,
    ) {
        let sel = permission_ask_default_sel(&options);
        self.permission_ask = Some(PermissionAskOverlay {
            title,
            sel,
            options,
            reply: Some(reply),
        });
        self.needs_redraw = true;
    }

    fn finish_permission_ask(&mut self, reply: PermissionAskReply) {
        if let Some(mut ask) = self.permission_ask.take() {
            if let Some(tx) = ask.reply.take() {
                let _ = tx.send(reply);
            }
        }
        self.needs_redraw = true;
    }

    fn handle_picker_key(&mut self, key: KeyEvent, ctl: &Controller) {
        // The theme dialog owns dark/light too (its title advertises it):
        // ctrl+t flips the *previewed* pack's mode and keeps browsing, and
        // the preview maps carry both modes, so the toggle stays in-pack.
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.kind == PickerKind::Theme)
            && matches!(
                crate::input::classify(
                    &key,
                    crate::input::KeyCtx {
                        input_empty: self.input.is_empty(),
                        history_active: self.input.hist_pos.is_some(),
                    },
                ),
                Some(Action::ToggleTheme)
            )
        {
            self.dispatch(Action::ToggleTheme, ctl);
            return;
        }
        let Some(picker) = &mut self.picker else {
            return;
        };
        let kind = picker.kind;
        let n = picker.items.len().max(1);
        // Page keys jump a screenful of the open popup (rows recorded by the
        // draw pass); they never wrap, unlike ↑/↓.
        let page = self.picker_page_rows.max(1);
        let sel_before = picker.sel;
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.queue_delete_armed = None;
            }
            KeyCode::Up => picker.sel = picker.sel.checked_sub(1).unwrap_or(n - 1),
            KeyCode::Down => picker.sel = (picker.sel + 1) % n,
            KeyCode::PageUp => picker.sel = picker.sel.saturating_sub(page),
            KeyCode::PageDown => picker.sel = picker.sel.saturating_add(page).min(n - 1),
            KeyCode::Home => picker.sel = 0,
            KeyCode::End => picker.sel = n - 1,
            // The queue list deletes rows in place (two presses, like the
            // editor's ctrl+d): the highlighted row is the one it names.
            KeyCode::Char('d')
                if kind == PickerKind::Queue && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let row = picker.items.get(picker.sel).map(|item| item.id.clone());
                if let Some(id) = row {
                    self.press_queue_delete(&id, ctl);
                }
            }
            KeyCode::Enter => {
                let Some(item) = picker.items.get(picker.sel).cloned() else {
                    self.picker = None;
                    return;
                };
                let kind = picker.kind;
                self.picker = None;
                match kind {
                    PickerKind::Model => self.select_model(item, ctl),
                    PickerKind::Theme => self.select_palette(&item.id),
                    PickerKind::Permission => self.set_permission(item.id, ctl),
                    PickerKind::Session => self.load_acp_session(&item.id, ctl),
                    PickerKind::Queue => self.begin_queue_edit(&item.id, ctl),
                    PickerKind::Subagent => {
                        self.active_subagent = if item.id == self.session_id {
                            None
                        } else if self.subagents.iter().any(|view| view.id == item.id) {
                            Some(item.id)
                        } else {
                            None
                        };
                        self.scroll_up = 0;
                        self.sel = None;
                        // The ↥ jump cursor indexes the displayed transcript's
                        // cells; it must not carry a cell from another view.
                        self.prompt_jump_cell = None;
                        self.prompt_flash = None;
                        self.prompt_flash_lines = None;
                    }
                    PickerKind::Effort => {
                        let effort = item.id;
                        self.modes.effort = Some(effort.clone());
                        ctl.send(Cmd::SelectModel {
                            session_id: self.session_id.clone(),
                            provider: None,
                            model: None,
                            effort: Some(effort.clone()),
                        });
                        self.transcript.push_notice(
                            NoticeLevel::Info,
                            format!(
                                "{} → {effort}",
                                self.locale.tr("reasoning effort", "推理强度")
                            ),
                        );
                    }
                }
            }
            _ => {}
        }
        // The theme dialog lives on its highlight: moving the selection
        // paints the row under it right away, without waiting for Enter.
        // Enter above still closes and commits (persisted preference), Esc
        // closes and `reconcile_theme_preview` puts the committed pack back.
        if kind == PickerKind::Theme
            && matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
            )
            && self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.kind == PickerKind::Theme && picker.sel != sel_before)
        {
            self.preview_picker_theme();
        }
        // Moving off an armed queue row disarms it: `ctrl+d` deletes what the
        // highlight is on, never a row the user has walked away from.
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.kind == PickerKind::Queue && picker.sel != sel_before)
            && self.queue_delete_armed.is_some()
        {
            self.queue_delete_armed = None;
            self.refresh_queue_picker();
        }
    }

    /// The wheel over an open picker walks its highlight (one notch = one
    /// ↑/↓ press), so the theme dialog previews the pack the wheel landed on.
    fn picker_scroll_by(&mut self, delta: i64) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let kind = picker.kind;
        let sel_before = picker.sel;
        let last = picker.items.len().saturating_sub(1);
        if delta < 0 {
            picker.sel = picker
                .sel
                .saturating_sub(delta.unsigned_abs() as usize)
                .min(last);
        } else {
            picker.sel = picker.sel.saturating_add(delta as usize).min(last);
        }
        self.needs_redraw = true;
        if kind == PickerKind::Theme
            && self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.sel != sel_before)
        {
            self.preview_picker_theme();
        }
        // The wheel walks the queue list too, so it disarms an armed row the
        // same way ↑/↓ does.
        if kind == PickerKind::Queue
            && self.queue_delete_armed.is_some()
            && self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.sel != sel_before)
        {
            self.queue_delete_armed = None;
            self.refresh_queue_picker();
            self.needs_redraw = true;
        }
    }

    fn open_subagent_switcher(&mut self) {
        let mut items = vec![PickerItem {
            id: self.session_id.clone(),
            label: self.locale.tr("main", "主会话").into(),
            meta: self.locale.tr("current session", "当前会话").into(),
            provider: None,
        }];
        items.extend(self.subagents.iter().map(|view| PickerItem {
            id: view.id.clone(),
            label: view.label.clone(),
            meta: if view.running {
                self.locale.tr("running", "运行中").into()
            } else {
                self.locale.tr("finished", "已完成").into()
            },
            provider: None,
        }));
        let current = self.active_subagent.as_deref().unwrap_or(&self.session_id);
        let current_index = items
            .iter()
            .position(|item| item.id == current)
            .unwrap_or(0);
        let sel = (current_index + 1) % items.len();
        self.picker = Some(Picker {
            kind: PickerKind::Subagent,
            title: self
                .locale
                .tr(
                    " agents · ↑/↓ select · enter open · esc close ",
                    " Agent · ↑/↓ 选择 · enter 打开 · esc 关闭 ",
                )
                .into(),
            sel,
            items,
        });
    }

    /// `⌥↑` — pick one queued prompt to edit (Martty's queue selector). The
    /// composer must be free: the chosen item is loaded into it. The same list
    /// deletes rows (`ctrl+d`), so it is also the place to drop a queued prompt
    /// you no longer mean to send.
    fn open_queue_selector(&mut self) {
        if self.prompt_queue.is_empty() {
            self.show_tip(
                self.locale
                    .tr("no queued prompt to edit", "没有可编辑的排队消息"),
            );
            return;
        }
        if !self.input.is_empty() || !self.pending_images.is_empty() {
            self.show_tip(self.locale.tr(
                "send or clear the current draft before editing the queue",
                "编辑队列前请先发送或清空当前草稿",
            ));
            return;
        }
        // A fresh look at the queue: nothing is armed until ctrl+d asks here.
        self.queue_delete_armed = None;
        self.picker = self.queue_picker(None);
    }

    /// One queue-picker frame: rows in FIFO order, the item being edited marked,
    /// and the armed row saying so. `sel` "None" opens on the natural row — the
    /// item being edited, else the head — while `Some` keeps a highlight that
    /// already exists (a delete rebuilds the list under it).
    fn queue_picker(&self, sel: Option<usize>) -> Option<Picker> {
        if self.prompt_queue.is_empty() {
            return None;
        }
        let editing = self.queue_edit.as_ref().map(|edit| edit.prompt_id);
        let items: Vec<PickerItem> = self
            .prompt_queue
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                let id = prompt.id.to_string();
                let armed = self.queue_delete_armed.as_deref() == Some(id.as_str());
                let state = if armed {
                    self.locale.tr("ctrl+d deletes", "再按 ctrl+d 删除")
                } else if editing == Some(prompt.id) {
                    self.locale.tr("editing", "编辑中")
                } else {
                    self.locale.tr("queued", "排队中")
                };
                PickerItem {
                    id,
                    label: queue_prompt_summary(&prompt.blocks),
                    meta: format!("#{} · {state}", index + 1),
                    provider: None,
                }
            })
            .collect();
        let last = items.len() - 1;
        let sel = match sel {
            Some(sel) => sel.min(last),
            None => editing
                .and_then(|id| items.iter().position(|item| item.id == id.to_string()))
                .unwrap_or(0),
        };
        Some(Picker {
            kind: PickerKind::Queue,
            title: self
                .locale
                .tr(
                    " queued prompts · ↑/↓ · enter edit · ctrl+d delete · esc close ",
                    " 排队消息 · ↑/↓ · enter 编辑 · ctrl+d 删除 · esc 关闭 ",
                )
                .into(),
            sel,
            items,
        })
    }

    /// Rebuild the open queue picker in place: a delete moves every row, and the
    /// dialog stays open so the neighbor is one highlight away. The last delete
    /// closes it — an empty list is not a dialog.
    fn refresh_queue_picker(&mut self) {
        let Some(sel) = self
            .picker
            .as_ref()
            .filter(|picker| picker.kind == PickerKind::Queue)
            .map(|picker| picker.sel)
        else {
            return;
        };
        self.picker = self.queue_picker(Some(sel));
        if self.picker.is_none() {
            self.queue_delete_armed = None;
        }
    }

    /// Load one queued prompt back into the composer. The item keeps its FIFO
    /// slot (marked "editing") together with its original blocks: save
    /// replaces them, delete drops them, cancel leaves them alone — nothing
    /// can be sent twice.
    fn begin_queue_edit(&mut self, id: &str, _ctl: &Controller) {
        /// What the rebuild needs from one queued block: attachments cannot be
        /// cloned (only their payloads are cheap `Arc`s), and `stage_image`
        /// wants `&mut self`, so the item is read out before the borrow ends.
        enum Piece {
            Text(String),
            Image(String, String, String, Vec<u8>),
        }
        let Some((prompt_id, pieces)) = self
            .prompt_queue
            .iter()
            .find(|prompt| prompt.id.to_string() == id)
            .map(|prompt| {
                let pieces: Vec<Piece> = prompt
                    .blocks
                    .iter()
                    .map(|block| match block {
                        StagedBlock::Text(text) => Piece::Text(text.clone()),
                        StagedBlock::Image(att) => Piece::Image(
                            att.name.clone(),
                            att.path.clone(),
                            att.media_type.clone(),
                            att.data.to_vec(),
                        ),
                    })
                    .collect();
                (prompt.id, pieces)
            })
        else {
            self.show_tip(
                self.locale
                    .tr("queued prompt already left the queue", "这条消息已离开队列"),
            );
            return;
        };
        // Rebuild the draft from the item's blocks. Images are staged afresh
        // (their tokens and the tray entries behind them are new), so the edit
        // owns its attachments outright and cancel can leave the queue alone.
        self.input.clear();
        for piece in pieces {
            match piece {
                Piece::Text(text) => self.input.insert_str(&text),
                Piece::Image(name, path, media_type, data) => {
                    self.stage_image(name, path, media_type, data, String::new())
                }
            }
        }
        self.queue_edit = Some(QueueEditState {
            prompt_id,
            delete_confirm: false,
        });
        self.reconcile_attachments();
        self.show_tip(self.locale.tr(
            "editing queued prompt · enter save · ctrl+d delete · esc cancel",
            "编辑排队消息 · enter 保存 · ctrl+d 删除 · esc 取消",
        ));
    }

    /// Enter while editing: replace the queued item with the edited draft.
    fn save_queue_edit(&mut self, ctl: &Controller) {
        let Some(edit) = self.queue_edit.as_ref() else {
            return;
        };
        let prompt_id = edit.prompt_id;
        if edit.delete_confirm {
            self.delete_queue_edit(ctl);
            return;
        }
        let raw = self.input.buf().trim().to_string();
        if raw.is_empty() && self.pending_images.is_empty() {
            self.show_tip(self.locale.tr(
                "queued prompt cannot be empty · ctrl+d deletes it",
                "排队消息不能为空 · ctrl+d 可删除",
            ));
            return;
        }
        let blocks = if self.pending_images.is_empty() {
            vec![StagedBlock::Text(raw)]
        } else {
            self.take_staged_blocks()
        };
        let Some(index) = self
            .prompt_queue
            .iter()
            .position(|prompt| prompt.id == prompt_id)
        else {
            self.finish_queue_edit();
            self.show_tip(
                self.locale
                    .tr("queued prompt already left the queue", "这条消息已离开队列"),
            );
            return;
        };
        self.prompt_queue[index].blocks = blocks;
        self.finish_queue_edit();
        self.show_tip(
            self.locale
                .tr("queued prompt #{n} updated", "排队消息 #{n} 已更新")
                .replace("{n}", &(index + 1).to_string()),
        );
        if self.state == RunState::Idle {
            self.dispatch_next_queued(ctl);
        }
    }

    /// `ctrl+d` while editing: the first press arms, the second deletes.
    fn delete_queue_edit(&mut self, ctl: &Controller) {
        let Some(edit) = self.queue_edit.as_mut() else {
            return;
        };
        if !edit.delete_confirm {
            edit.delete_confirm = true;
            self.show_tip(self.locale.tr(
                "ctrl+d again deletes this queued prompt · esc cancels",
                "再按一次 ctrl+d 删除这条排队消息 · esc 取消",
            ));
            return;
        }
        let prompt_id = edit.prompt_id;
        let Some(index) = self
            .prompt_queue
            .iter()
            .position(|prompt| prompt.id == prompt_id)
        else {
            self.finish_queue_edit();
            return;
        };
        // The editor closes before the drop: an idle client sends the next in
        // line, and the "queue paused for edit" guard must not hold it back.
        self.finish_queue_edit();
        self.drop_queued_prompt(index, ctl);
    }

    /// Delete one queued prompt for good: its echo leaves the timeline with it
    /// (the message was never sent), the counter follows, and an idle client
    /// sends the next in line. The editor's `ctrl+d` and the picker's land here.
    fn drop_queued_prompt(&mut self, index: usize, ctl: &Controller) {
        let Some(prompt) = self.prompt_queue.remove(index) else {
            return;
        };
        self.withdraw_prompt_echo(&prompt.cells);
        self.queued = self.prompt_queue.len();
        self.show_tip(
            self.locale
                .tr("queued prompt #{n} deleted", "排队消息 #{n} 已删除")
                .replace("{n}", &(index + 1).to_string()),
        );
        if self.state == RunState::Idle {
            self.dispatch_next_queued(ctl);
        }
    }

    /// `ctrl+d` in the queue picker: the first press arms the highlighted row,
    /// the second deletes it — the same two-press idiom the editor's `ctrl+d`
    /// uses, and the same "what is under the highlight" rule.
    fn press_queue_delete(&mut self, id: &str, ctl: &Controller) {
        if self.queue_delete_armed.as_deref() != Some(id) {
            self.queue_delete_armed = Some(id.to_string());
            self.refresh_queue_picker();
            return;
        }
        self.queue_delete_armed = None;
        // A row whose item already left (a session switch, another client's
        // delete) is simply gone: the rebuild below shows the queue as it is.
        if let Some(index) = self
            .prompt_queue
            .iter()
            .position(|prompt| prompt.id.to_string() == id)
        {
            self.drop_queued_prompt(index, ctl);
        }
        // The drop may have shipped the next item: the rows are rebuilt from
        // the queue as it stands, with the highlight on the row that took the
        // deleted one's place.
        self.refresh_queue_picker();
    }

    /// Take a queued prompt's echo out of the timeline.
    ///
    /// The prompt never left the client, so deleting it deletes its bubbles
    /// too: leaving them behind as `queued` (or as plain, delivered-looking
    /// prompts) would both be wrong. Removing cells shifts every index above
    /// them, so the transcript's own open-card maps and this client's cell
    /// bookkeeping — the queue, the pending steers, the `↥` jump — are remapped
    /// in the same step. The line selection is dropped: its anchors moved.
    fn withdraw_prompt_echo(&mut self, cells: &[usize]) {
        let mapping = self.transcript.remove_cells(cells);
        let remap = |cells: &mut Vec<usize>| {
            cells.retain_mut(|index| match mapping.get(*index).copied().flatten() {
                Some(at) => {
                    *index = at;
                    true
                }
                None => false,
            });
        };
        for prompt in &mut self.prompt_queue {
            remap(&mut prompt.cells);
        }
        for steer in self.pending_steer_cells.values_mut() {
            remap(&mut steer.cells);
        }
        self.prompt_jump_cell = self
            .prompt_jump_cell
            .and_then(|index| mapping.get(index).copied().flatten());
        self.prompt_flash = None;
        self.prompt_flash_lines = None;
        self.sel = None;
    }

    /// Leave edit mode: the draft (and the tray it resolved into) is dropped;
    /// the queued item keeps whatever it already had.
    fn finish_queue_edit(&mut self) {
        self.queue_edit = None;
        self.input.clear();
        self.pending_images.clear();
        self.reconcile_attachments();
    }

    /// Empty-draft Enter: promote the FIFO head into the active turn. While a
    /// turn runs this is the same steer the composer's ctrl+enter takes
    /// (interrupt + resend); idle it simply goes out now.
    fn send_queue_head_now(&mut self, ctl: &Controller) {
        if self.queue_edit.is_some() {
            return;
        }
        let running = self.turn_busy();
        let Some(prompt) = self.prompt_queue.pop_front() else {
            return;
        };
        self.queued = self.prompt_queue.len();
        self.scroll_up = 0;
        let message_id = prompt.id;
        let wire = prompt_blocks_from_staged(&prompt.blocks);
        if running {
            self.pending_steer_cells.insert(
                message_id,
                PendingSteer {
                    cells: prompt.cells,
                    blocks: prompt.blocks,
                },
            );
            self.show_tip(self.locale.tr(
                "queue head sent now — lands at the next agent step",
                "队首已立即发送 —— 在下一步 Agent 处生效",
            ));
            self.send_wire_prompt(wire, Some(message_id), ctl);
            return;
        }
        self.transcript.mark_prompt_delivered(&prompt.cells);
        self.prompt_pending = true;
        self.state = RunState::Starting;
        self.run_started = Some(Instant::now());
        self.state_note = self
            .locale
            .tr("sending queued followup", "正在发送排队消息")
            .into();
        self.send_wire_prompt(wire, None, ctl);
    }

    fn open_model_picker(&mut self, ctl: &Controller) {
        // Ask the driver for its catalog; seed the picker with the
        // stock presets meanwhile.
        ctl.send(Cmd::FetchCatalog);
        let mut items: Vec<PickerItem> = MODEL_PRESETS
            .iter()
            .map(|s| PickerItem {
                id: s.to_string(),
                label: s.to_string(),
                meta: String::new(),
                provider: None,
            })
            .collect();
        if !items.iter().any(|i| i.id == self.cfg.model) {
            items.insert(
                0,
                PickerItem {
                    id: self.cfg.model.clone(),
                    label: self.cfg.model.clone(),
                    meta: String::new(),
                    provider: None,
                },
            );
        }
        let sel = items
            .iter()
            .position(|i| i.id == self.cfg.model)
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Model,
            title: self
                .locale
                .tr(
                    " model · enter select · esc close ",
                    " 模型 · enter 选择 · esc 关闭 ",
                )
                .into(),
            sel,
            items,
        });
    }

    fn open_effort_picker(&mut self, efforts: Vec<String>, default: Option<String>) {
        let mut items: Vec<PickerItem> = efforts
            .into_iter()
            .map(|e| {
                let is_default = default.as_deref() == Some(e.as_str());
                PickerItem {
                    id: e.clone(),
                    label: e,
                    meta: if is_default {
                        self.locale.tr("default", "默认").into()
                    } else {
                        String::new()
                    },
                    provider: None,
                }
            })
            .collect();
        if items.is_empty() {
            items = ["off", "high", "max"]
                .iter()
                .map(|e| PickerItem {
                    id: e.to_string(),
                    label: e.to_string(),
                    meta: String::new(),
                    provider: None,
                })
                .collect();
        }
        self.picker = Some(Picker {
            kind: PickerKind::Effort,
            title: self
                .locale
                .tr(
                    " reasoning effort · enter select · esc close ",
                    " 推理强度 · enter 选择 · esc 关闭 ",
                )
                .into(),
            sel: 0,
            items,
        });
    }

    /// `/resume`: list this workspace's durable sessions in a picker.
    /// The abycore driver owns the workspace snapshot store.
    fn open_resume_picker(&mut self, ctl: &Controller) {
        ctl.send(Cmd::ListSessions { prefix: None });
        self.show_tip(self.locale.tr("listing sessions…", "正在列出会话…"));
    }

    /// Resume a durable session: replay its JSONL into the scrollback and
    /// point the next prompt at the same id — the runtime (or host dsh)
    /// keeps appending to the same log.
    fn reset_session_ui(&mut self) {
        self.session_switch = None;
        self.vim.reset_pending();
        self.reset_subagent_views();
        self.transcript.clear();
        self.plan = None;
        self.todo_dialog = None;
        self.plan_chip = None;
        self.hover_plan_chip = false;
        self.prompt_jump_cell = None;
        self.prompt_flash = None;
        self.prompt_flash_lines = None;
        self.modes = Self::load_modes_cache(&self.cfg).unwrap_or_default();
        self.selected_model = None;
        self.session_title = None;
        self.queued = 0;
        self.prompt_queue.clear();
        self.queue_edit = None;
        self.pending_steer_cells.clear();
        self.prompt_pending = false;
        self.sel = None;
        self.state = RunState::Idle;
        self.run_started = None;
        self.scroll_up = 0;
        self.session_bound = false;
    }

    fn reset_subagent_views(&mut self) {
        self.subagents.clear();
        self.active_subagent = None;
        if matches!(
            self.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Subagent)
        ) {
            self.picker = None;
        }
    }

    fn load_acp_session(&mut self, id: &str, ctl: &Controller) {
        if self.session_switch.is_some() || id == self.session_id {
            return;
        }
        // The old view and queue remain owned by the old session until ack.
        let after_turn = self.turn_busy();
        self.session_switch = Some(SessionSwitch::Resume(id.into()));
        ctl.send(Cmd::LoadSession {
            session_id: id.to_string(),
        });
        // The listing answers mid-turn (the driver serves store queries off
        // its turn loop), but a load needs that loop: say the turn has to end
        // instead of letting the tip imply the load is already running.
        let suffix = if after_turn {
            self.locale.tr(" (after this turn)", "（本轮结束后）")
        } else {
            ""
        };
        self.show_tip(format!(
            "{}{suffix}",
            self.locale
                .tr("session/load {n} …", "正在加载会话 {n} …")
                .replace("{n}", id)
        ));
        self.needs_redraw = true;
    }

    fn on_acp_session_list(
        &mut self,
        sessions: Vec<SessionListItem>,
        prefix: Option<String>,
        ctl: &Controller,
    ) {
        let skip = self.session_id.clone();
        let sessions: Vec<SessionListItem> =
            sessions.into_iter().filter(|s| s.id != skip).collect();
        if let Some(prefix) = prefix.as_deref().filter(|p| !p.is_empty()) {
            match unique_session_list_match(self.locale, &sessions, prefix) {
                Ok(id) => {
                    self.load_acp_session(&id, ctl);
                    return;
                }
                Err(msg) => {
                    self.transcript.push_notice(NoticeLevel::Warn, msg);
                    if sessions.is_empty() {
                        return;
                    }
                }
            }
        }
        if sessions.is_empty() {
            self.transcript.push_notice(
                NoticeLevel::Info,
                self.locale
                    .tr(
                        "no durable sessions for this workspace yet — finish a turn and /resume finds it",
                        "这个工作区还没有持久会话 —— 先跑完一轮，/resume 就能看到",
                    )
                    .into(),
            );
            return;
        }
        let items: Vec<PickerItem> = sessions
            .iter()
            .map(|s| session_picker_row(&s.id, s.title.as_deref(), s.updated_at.as_deref()))
            .collect();
        self.picker = Some(Picker {
            kind: PickerKind::Session,
            title: self
                .locale
                .tr(
                    " resume session · {n} sessions · enter select · esc close ",
                    " 恢复会话 · {n} 个会话 · enter 选择 · esc 关闭 ",
                )
                .replace("{n}", &items.len().to_string()),
            sel: 0,
            items,
        });
    }

    /// The effective composition id: the advertised agent preset, else empty.
    fn select_model(&mut self, item: PickerItem, ctl: &Controller) {
        let model = item.id;
        let provider = item.provider;
        let provider_changed = provider
            .as_deref()
            .is_some_and(|candidate| candidate != self.cfg.provider);
        if model != self.cfg.model || provider_changed {
            self.cfg.model = model.clone();
            self.selected_model = Some(model.clone());
            if let Some(p) = &provider {
                self.cfg.provider = p.clone();
            }
            ctl.send(Cmd::SelectModel {
                session_id: self.session_id.clone(),
                provider,
                model: Some(model.clone()),
                effort: None,
            });
        }
        // Stage 2: offer efforts for the chosen model.
        ctl.send(Cmd::FetchEfforts {
            provider: self.cfg.provider.clone(),
            model: self.cfg.model.clone(),
        });
    }

    fn set_model(&mut self, model: String, ctl: &Controller) {
        if model == self.cfg.model {
            return;
        }
        self.selected_model = Some(model.clone());
        ctl.send(Cmd::SelectModel {
            session_id: self.session_id.clone(),
            provider: None,
            model: Some(model),
            effort: None,
        });
    }

    /// grok: Shift+Tab cycles the permission preset.
    fn cycle_permission(&mut self, ctl: &Controller) {
        let current = self.current_permission().to_string();
        let idx = PERMISSION_PRESETS
            .iter()
            .position(|(p, _)| *p == current)
            .unwrap_or(0);
        let next = PERMISSION_PRESETS[(idx + 1) % PERMISSION_PRESETS.len()]
            .0
            .to_string();
        self.set_permission(next, ctl);
    }

    /// The effective permission preset: the folded `permission/preset` fact,
    /// or the trusted-directory default (danger-full-access) before the
    /// session reports.
    pub fn current_permission(&self) -> &str {
        self.modes
            .permission
            .as_deref()
            .unwrap_or("danger-full-access")
    }

    /// Ask the host to switch this session's permission preset; the durable
    /// `permission/preset` event echoes back and folds the ⛨ chip, which is
    /// the whole confirmation — the switch itself stays out of the timeline.
    /// Before the first prompt the host stages the switch and applies it when
    /// the session is created; until a session is bound the chips are hidden,
    /// so a staged switch borrows the tip line to stay visible.
    fn set_permission(&mut self, preset: String, ctl: &Controller) {
        if self.modes.permission.as_deref() == Some(preset.as_str()) {
            self.show_tip(
                self.locale
                    .tr("permission already {n}", "权限已经是 {n}")
                    .replace("{n}", &preset),
            );
            return;
        }
        ctl.send(Cmd::SetPermission {
            session_id: self.session_id.clone(),
            preset: preset.clone(),
        });
        if !self.session_bound {
            self.show_tip(
                self.locale
                    .tr("permission → {n} …", "权限 → {n} …")
                    .replace("{n}", &preset),
            );
        }
    }

    /// `/permission` — the two stock presets with their meaning, the current
    /// one preselected (picker twin of the blind shift+tab cycle).
    fn open_permission_picker(&mut self) {
        let reported = self.modes.permission.clone();
        let current = self.current_permission().to_string();
        let items: Vec<PickerItem> = PERMISSION_PRESETS
            .iter()
            .map(|(id, _)| {
                let mark = if reported.as_deref() == Some(*id) {
                    format!(" · {}", self.locale.tr("current", "当前"))
                } else if reported.is_none() && *id == current {
                    format!(" · {}", self.locale.tr("default", "默认"))
                } else {
                    String::new()
                };
                PickerItem {
                    id: id.to_string(),
                    label: permission_label(id),
                    meta: format!(
                        "{}{mark}",
                        permission_desc(self.locale, id).unwrap_or_default()
                    ),
                    provider: None,
                }
            })
            .collect();
        let sel = items.iter().position(|i| i.id == current).unwrap_or(0);
        self.picker = Some(Picker {
            kind: PickerKind::Permission,
            title: self
                .locale
                .tr(
                    " permission · enter apply · esc close ",
                    " 权限 · enter 应用 · esc 关闭 ",
                )
                .into(),
            sel,
            items,
        });
    }

    fn handle_esc(&mut self, ctl: &Controller) {
        if self.permission_ask.is_some() {
            self.finish_permission_ask(PermissionAskReply::Cancelled);
            return;
        }
        if self.picker.is_some() {
            self.picker = None;
            return;
        }
        if self.active_subagent.take().is_some() {
            self.scroll_up = 0;
            self.sel = None;
            self.needs_redraw = true;
            return;
        }
        // A queued prompt loaded for editing leaves the item untouched.
        if self.queue_edit.is_some() {
            self.finish_queue_edit();
            self.show_tip(
                self.locale
                    .tr("queued prompt edit cancelled", "已取消编辑排队消息"),
            );
            return;
        }
        // A lingering copy highlight is dismissed first (idle only — while
        // running, esc keeps its interrupt meaning and clears it in passing);
        // the composer's drag highlight follows the same rule.
        let had_input_sel = self.input_sel.take().is_some();
        if (self.sel.take().is_some() || had_input_sel) && matches!(self.state, RunState::Idle) {
            self.needs_redraw = true;
            return;
        }
        if self.input.buf().starts_with('/') && !self.slash_matches().is_empty() {
            self.input.clear();
            return;
        }
        match self.state {
            RunState::Running | RunState::Starting => {
                // grok: Esc cancels immediately; the draft survives.
                ctl.interrupt_now();
                ctl.send(Cmd::Interrupt {
                    session_id: self.session_id.clone(),
                });
                self.state_note = self.locale.tr("cancelling", "正在取消").into();
            }
            RunState::Idle => {
                // Esc clears the draft — inline [image n] chips live in it,
                // so staged images go with it (reconcile below).
                if !self.input.is_empty() {
                    self.input.history.push(self.input.buf().clone());
                    self.input.clear();
                    self.reconcile_attachments();
                    self.show_tip(
                        self.locale
                            .tr("draft cleared — ↑ recalls it", "草稿已清空 —— ↑ 可召回"),
                    );
                    return;
                }
                self.show_tip(self.locale.tr(
                    "esc — idle · a running turn is interrupted with esc",
                    "esc —— 空闲；运行中按 esc 会中断本轮",
                ));
            }
        }
    }

    fn handle_ctrl_c(&mut self, _ctl: &Controller) {
        if !self.input.is_empty() {
            // Clearing the draft never counts as the first press of the
            // double-Ctrl+C quit chord.
            self.ctrl_c_armed = None;
            self.input.history.push(self.input.buf().clone());
            self.input.clear();
            self.reconcile_attachments();
            self.show_tip(
                self.locale
                    .tr("draft cleared — ↑ recalls it", "草稿已清空 —— ↑ 可召回"),
            );
            return;
        }
        let required = 2;
        let mut chord = self.ctrl_c_armed.take().unwrap_or(CtrlCQuitChord {
            started: Instant::now(),
            presses: 0,
            required,
        });
        chord.presses += 1;
        if chord.presses >= chord.required {
            self.quit = true;
            return;
        }
        let remaining = chord.required - chord.presses;
        self.ctrl_c_armed = Some(chord);
        self.show_tip(if remaining == 1 {
            self.locale
                .tr("press ctrl+c again to exit", "再按一次 ctrl+c 退出")
                .to_string()
        } else {
            self.locale
                .tr(
                    "press ctrl+c {n} more times to exit while the agent is running",
                    "Agent 运行中：再按 {n} 次 ctrl+c 退出",
                )
                .replace("{n}", &remaining.to_string())
        });
    }

    fn history_prev(&mut self) {
        if !self.input.is_empty() && self.input.hist_pos.is_none() {
            return; // grok: history opens from an empty prompt
        }
        if self.input.history.is_empty() {
            return;
        }
        let pos = match self.input.hist_pos {
            None => {
                self.input.stash = self.input.buf().clone();
                self.input.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.input.hist_pos = Some(pos);
        self.input.set(self.input.history[pos].clone());
    }

    /// Recall history from a *non-empty* draft — ↑ at the draft's first visual
    /// row, or on a dismissed `/` line. Unlike [`Self::history_prev`] this one
    /// opens the history even while the draft holds text: the editor's stash
    /// keeps that draft, and `↓` past the newest entry restores it.
    fn history_prev_from_draft(&mut self) {
        if self.input.history.is_empty() {
            return;
        }
        if self.input.hist_pos.is_none() {
            self.input.stash = self.input.buf().clone();
        }
        let pos = match self.input.hist_pos {
            None => self.input.history.len() - 1,
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.input.hist_pos = Some(pos);
        self.input.set(self.input.history[pos].clone());
    }

    fn history_next(&mut self) {
        let Some(pos) = self.input.hist_pos else {
            return;
        };
        if pos + 1 >= self.input.history.len() {
            self.input.hist_pos = None;
            let stash = std::mem::take(&mut self.input.stash);
            self.input.set(stash);
        } else {
            self.input.hist_pos = Some(pos + 1);
            self.input.set(self.input.history[pos + 1].clone());
        }
    }

    fn accept_slash(&mut self, entry: &SlashEntry, ctl: &Controller) {
        if let Some(completion) = &entry.completion {
            self.input.set(completion.clone());
        }
        // A skill that declares an argument placeholder opens a form: the row
        // completes the line and waits. One that takes no arguments runs on the
        // spot, like every other argument pick.
        if entry.name == "skill" && self.skill_candidate_awaits_args(entry) {
            let line = format!("{} ", self.input.buf().trim_end());
            self.input.set(line);
            self.slash_sel = 0;
            return;
        }
        let line = self.input.buf().clone();
        let rest = line
            .strip_prefix('/')
            .and_then(|s| s.strip_prefix(entry.name.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        self.input.clear();
        self.slash_sel = 0;
        self.run_slash(&entry.name, &rest, ctl);
    }

    pub fn run_slash(&mut self, name: &str, arg: &str, ctl: &Controller) {
        match name {
            "help" => self.push_help(),
            "keys" => self.push_keys(),
            "lang" => self.set_locale(arg),
            "login" => self.login(arg, ctl),
            "logout" => self.logout(ctl),
            "compact" => {
                ctl.send(Cmd::Compact);
            }
            "goal" => {
                ctl.send(Cmd::Goal {
                    arg: arg.to_string(),
                });
            }
            "clear" => {
                self.transcript.clear();
                self.sel = None;
                self.transcript.push_notice(
                    NoticeLevel::Info,
                    self.locale.tr("scrollback cleared", "滚动区已清空").into(),
                );
            }
            "quit" => self.quit = true,
            "theme" => self.apply_theme_arg(arg),
            "vim" => {
                let on = match arg {
                    "on" | "1" => true,
                    "off" | "0" => false,
                    _ => !self.vim.is_active(),
                };
                self.vim.set(on);
                self.show_tip(self.locale.tr(
                    "vim mode — i insert · esc normal · /vim off",
                    "vim 模式 — i 插入 · esc 返回 normal · /vim off 关闭",
                ));
            }
            "model" => {
                if arg.is_empty() {
                    self.open_model_picker(ctl);
                } else {
                    self.set_model(arg.to_string(), ctl);
                }
            }
            "new" => {
                if self.session_switch.is_some() {
                    return;
                }
                self.session_switch = Some(SessionSwitch::New);
                ctl.send(Cmd::NewSession);
                self.show_tip(self.locale.tr("session/new …", "正在新建会话…"));
            }
            "status" => self.open_status_dialog(),
            "skill" => self.invoke_skill(arg, ctl),
            "resume" => {
                if arg.is_empty() {
                    self.open_resume_picker(ctl);
                } else {
                    ctl.send(Cmd::ListSessions {
                        prefix: Some(arg.to_string()),
                    });
                    self.show_tip(self.locale.tr("listing sessions…", "正在列出会话…"));
                }
            }
            "effort" => {
                if arg.is_empty() {
                    ctl.send(Cmd::FetchEfforts {
                        provider: self.cfg.provider.clone(),
                        model: self.cfg.model.clone(),
                    });
                } else {
                    ctl.send(Cmd::SelectModel {
                        session_id: self.session_id.clone(),
                        provider: None,
                        model: None,
                        effort: Some(arg.to_string()),
                    });
                    self.modes.effort = Some(arg.to_string());
                }
            }
            "permission" => {
                if arg.is_empty() {
                    self.open_permission_picker();
                } else if let Some(preset) = normalize_permission(arg) {
                    self.set_permission(preset.to_string(), ctl);
                } else {
                    // Not a stock spelling — pass through for custom preset
                    // tables; the host lists what it knows on a miss.
                    self.set_permission(arg.to_string(), ctl);
                }
            }
            "plan" => {
                let text = if arg.is_empty() {
                    "/plan".to_string()
                } else {
                    format!("/plan {arg}")
                };
                self.send_agent_text(text, ctl);
            }
            "image" => self.send_image(arg, ctl),
            "clip" => self.clip_image(arg, ctl),
            other => {
                self.transcript.push_notice(
                    NoticeLevel::Warn,
                    if self.locale == Locale::Zh {
                        format!("未知命令 /{other} — 使用 /help 查看命令")
                    } else {
                        format!("unknown command /{other} — /help lists commands")
                    },
                );
            }
        }
    }

    /// Startup guidance when no API key was detected: where to get one and
    /// how to store it. `/login` takes effect immediately (the driver
    /// rebuilds from its snapshot), so no restart is needed.
    pub fn push_no_key_onboarding(&mut self) {
        let text = if self.locale == Locale::Zh {
            "\
## 尚未检测到 API key

1. 打开 <https://platform.deepseek.com/> → API keys，创建并复制你的 key
2. 在输入框输入（保存后立刻生效，无需重启）：

   /login sk-xxxxxxxx

3. key 保存在 `~/.abylab/.credentials.yaml`（0600，仅本用户可读）；
   `/status` 查看凭据来源 · `/logout` 删除已保存的 key

本次运行也可以用 `--api-key <key>` 临时覆盖（不落盘）。"
                .to_string()
        } else {
            "\
## No API key detected

1. Open <https://platform.deepseek.com/> → API keys, create and copy a key
2. Enter in the composer (takes effect immediately — no restart needed):

   /login sk-xxxxxxxx

3. The key lands in `~/.abylab/.credentials.yaml` (0600, owner-only);
   `/status` shows its source · `/logout` removes the stored key

`--api-key <key>` can override for this run only (never persisted)."
                .to_string()
        };
        self.transcript.push_markdown(text);
        self.needs_redraw = true;
    }

    fn push_help(&mut self) {
        // A dialog, not a timeline entry: the border names it (`/help`), so
        // the body is the list itself — the same surface `/keys` uses.
        let text = if self.locale == Locale::Zh {
            "\
- enter · 发送；当前轮次运行时将后续消息排队（草稿为空时立即发送队首）
- ctrl+enter · 立即 steer 当前轮次（老终端会退化成普通 enter）
- ⌥↑ · 排队的后续消息：↑/↓ 选择 · enter 编辑 · 列表中 ctrl+d 连按两次删除 · esc 关闭
- ctrl+x · 剪切选区 · ctrl+shift+c · 复制选区
- esc · 中断（保留草稿）；空闲时清除草稿
- ctrl+c · 有草稿先清除；无草稿时连按 2 次退出（不中断）
- shift+tab · 轮换权限预设 · /permission 打开选择器
- ctrl+p · 打开模型选择器，然后选择推理强度
- /lang · 切换界面语言：/lang zh 或 /lang en
- /login · 保存 API key 到 aby 主目录，不回显明文
- /logout · 删除已保存的 API key
- /effort · 推理强度 · /permission 权限预设 · /plan 计划模式
- /vim · 切换 vim 模态编辑（/vim on|off，默认关闭）
- /resume · 恢复持久会话并继续写入原日志
- /image · 暂存本地图片：/image ./pic.png [说明]
- /clip · 暂存剪贴板图片；ctrl+v 同样可用
- !cmd · 在会话级本地 shell 中运行命令，不经过 Agent；初始目录为 workspace，cd/环境变量跨命令保留
- /<skill> · 手打的技能行由 Agent 注入技能正文（技能不进 / 菜单）
- /skill · 列出/调用本工作区的技能（`.agents/skills/`）：/skill <名字> [参数]；空格后是候选清单
- ctrl+o · 展开思考和工具输出 · ctrl+l · 清屏
- 编辑 · readline 组合键 + ⌘/⌥ 方向键 · 完整映射见 /keys
- 点击工具 · 展开/折叠 · 滚轮滚动对话
- pgup/pgdn · 翻页 · end 回到最新消息
- 鼠标拖动 · 选择并复制文本 · 双击复制单词

每轮会显示：流式思考与回答、工具调用与结果、注入上下文、Subagent 生命周期、
token 用量（含缓存命中）以及轮次结束原因。"
        } else {
            "\
- enter · send · queues a follow-up while a turn runs (an empty draft sends the queue head now)
- ctrl+enter · steer the active turn immediately (legacy terminals fall back to plain enter)
- ⌥↑ · queued follow-ups: ↑/↓ select · enter edit · ctrl+d twice deletes a row · esc close
- ctrl+x · cut the selection · ctrl+shift+c · copy it
- esc · interrupt (draft survives) · clears the draft when idle
- ctrl+c · clear a draft; 2× quits with no draft (never interrupts)
- shift+tab · cycle permission (workspace-write ⇄ full access) · /permission opens the preset picker
- ctrl+p · model picker → effort picker
- /effort · reasoning effort · /permission preset · /plan plan mode
- /vim · toggle vim modal editing (/vim on|off, off by default)
- /login · store the API key in the aby home (never echoed in full)
- /logout · remove the stored API key (a --api-key override keeps running)
- /resume · pick up a durable session — transcript replays, log continues
- /image · stage a local image — /image ./pic.png [caption]
- /clip · stage the clipboard image — /clip [caption] · ctrl+v also works
- !cmd · run in the session's local shell (not the agent); starts in the workspace, keeps cd/env across commands
- /<skill> · a hand-typed skill line — the agent injects the skill's body (skills never list under /)
- /skill · list or run this workspace's skills (`.agents/skills/`) — `/skill ` opens the catalog, Tab completes
- ctrl+o · expand thoughts + tool output · ctrl+l · clear
- editing · readline chords + ⌘/⌥ arrows (ctrl+arrows elsewhere) · full map in /keys
- click tool · expand/collapse that tool · wheel scrolls the conversation
- pgup/pgdn · scroll · mouse wheel works · end follows the tail
- mouse drag · select text — copied on release · 2×click copies a word

Per turn: streamed reasoning, answer, tool calls with results, injected
context, subagent lifecycles, token usage (incl. cache hits), end reason."
        };
        self.view_overlay = Some(ViewOverlay {
            title: self.locale.tr("Help", "帮助").to_string(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text: text.to_string(),
                streaming: false,
            }],
            scroll: 0,
        });
    }

    fn push_keys(&mut self) {
        let text = crate::input::keymap::keys_markdown(
            self.locale == Locale::Zh,
            cfg!(target_os = "macos"),
        );
        self.view_overlay = Some(ViewOverlay {
            title: self.locale.tr("Keyboard shortcuts", "快捷键").to_string(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text,
                streaming: false,
            }],
            scroll: 0,
        });
    }

    /// Where the running client's API key comes from: an explicit `/login`,
    /// the environment, or nothing. `/help` points the `/login` docs here.
    fn credential_line(&self) -> String {
        if !self.cfg.has_credentials() {
            return "no api key — /login <apikey> stores one".to_string();
        }
        match self.cfg.credential_source() {
            Some(src) => format!("api key present · {src}"),
            None => "api key present".to_string(),
        }
    }

    /// `/status` as a local modal: run state, painter-owned ACP facts and the
    /// live counters the composer's stats dock used to carry. Like `/help` and
    /// `/keys` it is chrome, not conversation — the card's border names it, so
    /// the body carries bullets only and the timeline stays untouched.
    ///
    /// The live `/status` is also a Client Plugin command (`status-view`): with
    /// a Client tree the plugin's semantic overlay renders the same facts from
    /// `acpSessionStats.current()`. This arm serves runs without one (demo,
    /// standalone painter) and reads the local accumulator instead — the rows
    /// it shows are the ones the dock used to paint, so nothing is lost when
    /// the dock stays out of the frame.
    fn open_status_dialog(&mut self) {
        let state = match self.state {
            RunState::Idle => self.locale.tr("idle", "空闲").to_string(),
            RunState::Starting => self.locale.tr("starting", "启动中").to_string(),
            RunState::Running => self.locale.tr("running", "工作中").to_string(),
        };
        let perm = self
            .modes
            .permission
            .clone()
            .or_else(|| self.modes.sandbox.clone())
            .unwrap_or_else(|| self.current_permission().to_string());
        let perm_label = if self.locale == Locale::Zh {
            match perm.as_str() {
                "read-only" => "只读".to_string(),
                "workspace-write" => "工作区可写".to_string(),
                "danger-full-access" => "完全访问".to_string(),
                _ => permission_label(&perm),
            }
        } else {
            permission_label(&perm)
        };
        let effort_line = self
            .modes
            .effort
            .as_deref()
            .map(|effort| format!("\n- effort · {effort}"))
            .unwrap_or_default();
        // Connection facts + the server banner when the runtime reported it.
        let mut text = format!("- state · {state}\n");
        // The live title rides the session row — it used to headline the
        // retired session card, and `/resume` lists the stored ones.
        let title = self
            .session_title
            .as_deref()
            .map(|title| format!(" · {title}"))
            .unwrap_or_default();
        text.push_str(&if self.session_bound {
            format!("- session · {}{title}\n", self.session_id)
        } else {
            "- session · unbound\n".to_string()
        });
        // The credential source rides here since the session card folded into
        // this one: `/help` still sends the `/login` reader to it.
        text.push_str(&format!("- credentials · {}\n", self.credential_line()));
        if let Some(server) = &self.server_info {
            text.push_str(&format!("- server · {server}\n"));
        }
        text.push_str(&format!(
            "- model · {}{}\n\
             - permission · {}\n\
             - plan · {}",
            self.cfg.model,
            effort_line,
            perm_label,
            if self.modes.plan { "on" } else { "off" },
        ));
        // The counter rows the composer dock used to render, in the same
        // bullet shape the other cards use. `usage.input` is total input
        // including cache reads, so the hit rate is a share of it.
        let u = self.transcript.usage;
        let s = self.transcript.stats;
        let cache_pct = if u.input > 0 {
            (u.cached as f64 / u.input as f64 * 100.0).round() as u64
        } else {
            0
        };
        text.push_str(&format!(
            "\n- tokens · ↑{} ↓{} · cache {}%\n\
             - turns · {} · steps · {}\n\
             - LLM · {} · tool · {}",
            fmt_tokens(u.input),
            fmt_tokens(u.output),
            cache_pct,
            s.turns,
            s.steps,
            fmt_duration(s.turn_millis.saturating_sub(s.tool_millis)),
            fmt_duration(s.tool_millis),
        ));
        if s.ttft_count > 0 {
            text.push_str(&format!(
                "\n- TTFT avg · {}",
                fmt_duration(s.ttft_total_millis.checked_div(s.ttft_count).unwrap_or(0))
            ));
        }
        self.view_overlay = Some(ViewOverlay {
            title: self.locale.tr("Status", "状态").to_string(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text,
                streaming: false,
            }],
            scroll: 0,
        });
    }

    /// `/skill` with no argument: what this workspace can invoke, and where
    /// each file lives (the same shape `/model` and `/permission` use for an
    /// empty argument). The catalog is discovered by the agent at startup, so
    /// the empty state names the directory instead of pretending the feature
    /// is missing.
    fn open_skills_dialog(&mut self) {
        let mut text = String::new();
        if self.skills.is_empty() {
            let dir = format!("{}/{SKILLS_DIR}", self.cfg.workspace);
            text.push_str(&format!(
                "- {}\n- {}\n",
                self.locale
                    .tr("no skills in this workspace", "本工作区没有技能"),
                match self.locale {
                    Locale::Zh => format!("技能放在 `{dir}/`：`<名字>.md` 或 `<名字>/SKILL.md`"),
                    Locale::En => {
                        format!("skills live in `{dir}/`: `<name>.md` or `<name>/SKILL.md`")
                    }
                },
            ));
            text.push_str(self.locale.tr(
                "A skill is a markdown file: optional `---` frontmatter with `name`, `description` and `input-hint`, then the instructions. abylab reads them at startup — restart after adding one.",
                "技能就是一个 markdown 文件：可选的 `---` frontmatter 写 `name`、`description`、`input-hint`，然后是正文指令。abylab 在启动时读取，加完要重启。",
            ));
        } else {
            for skill in &self.skills {
                let usage = match &skill.input_hint {
                    Some(hint) => format!("/{} {hint}", skill.name),
                    None => format!("/{}", skill.name),
                };
                let source = skill
                    .source
                    .as_deref()
                    .map(|path| format!("\n  `{path}`"))
                    .unwrap_or_default();
                text.push_str(&format!("- `{usage}` · {}{source}\n", skill.description));
            }
            text.push_str(self.locale.tr(
                "\nA skill line ships as a prompt; the agent injects that file's body. Skills never list under `/` — this catalog and `/skill ` candidates are the only place they show up.",
                "\n技能行会作为提示词发出，正文由 Agent 注入。技能不会出现在 / 菜单里：这里和 /skill 的习惯候选就是它们唯一的展示位置。",
            ));
        }
        self.view_overlay = Some(ViewOverlay {
            title: self.locale.tr("Skills", "技能").to_string(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text,
                streaming: false,
            }],
            scroll: 0,
        });
    }

    /// `/skill <name> [args]`: the same path a bare `/<name>` takes, with the
    /// name resolved client-side — the way to reach a skill whose name a
    /// builtin, or the menu's prefix rules, would otherwise swallow.
    fn invoke_skill(&mut self, arg: &str, ctl: &Controller) {
        let arg = arg.trim();
        if arg.is_empty() {
            self.open_skills_dialog();
            return;
        }
        let (name, rest) = match arg.split_once(char::is_whitespace) {
            Some((name, rest)) => (name, rest.trim()),
            None => (arg, ""),
        };
        if !self.skills.iter().any(|skill| skill.name == name) {
            let notice = match self.locale {
                Locale::Zh => format!("未知技能 {name} —— /skill 后打一个空格即可看到清单"),
                Locale::En => format!("unknown skill {name} — /skill then a space lists them"),
            };
            self.transcript.push_notice(NoticeLevel::Warn, notice);
            return;
        }
        let line = if rest.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {rest}")
        };
        self.send_agent_text(line, ctl);
    }
}

/// Compact token count: `1234` → `1.2K`, `1_500_000` → `1.5M`.
pub(crate) fn fmt_tokens(value: u64) -> String {
    if value < 1000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{:.1}K", value as f64 / 1000.0)
    } else {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    }
}

/// Compact duration: `1500ms` → `1.5s`, `135_000ms` → `2m15s`.
pub(crate) fn fmt_duration(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

impl App {
    fn submit(&mut self, ctl: &Controller) {
        if self.waiting_for_session_switch() {
            return;
        }
        let text = self.input.buf().trim().to_string();
        // Client namespaces don't take images — keep the chips editable
        // instead of silently dropping them.
        if !self.pending_images.is_empty() && text.starts_with('/') {
            self.show_tip(self.locale.tr(
                "send or delete the [image] chips first — /commands don't take images",
                "先发送或删除草稿里的 [image] 图片 —— / 命令不接收图片",
            ));
            return;
        }
        if let Some(cmdline) = text.strip_prefix('/') {
            let mut parts = cmdline.splitn(2, ' ');
            let name = parts.next().unwrap_or("").to_string();
            let arg = parts.next().unwrap_or("").trim().to_string();
            // A skill line ships as an ordinary prompt — the host's pre-step
            // boundary recognizes the leading /name and injects the body.
            // Builtins win a name; the menu never offered the skill rows, but
            // a hand-typed `/name` still resolves here.
            let builtin = SLASH_COMMANDS.iter().any(|c| c.name == name);
            if !builtin && self.skills.iter().any(|s| s.name == name) {
                self.input.history.push(text.clone());
                self.input.clear();
                self.send_agent_text(text, ctl);
                return;
            }
            self.input.history.push(text.clone());
            self.input.clear();
            self.run_slash(&name, &arg, ctl);
            return;
        }
        // Inline [image n] chips ride along with the prompt text (or send
        // alone): chip order = block order (图文交替).
        if !self.pending_images.is_empty() {
            self.input.history.push(self.input.buf().clone());
            let staged = self.take_staged_blocks();
            self.input.clear();
            self.send_staged(staged, ctl);
            return;
        }

        if text.is_empty() {
            return;
        }
        self.input.history.push(text.clone());
        self.input.clear();
        self.send_agent_text(text, ctl);
    }

    fn waiting_for_session_switch(&mut self) -> bool {
        if self.session_switch.is_none() {
            return false;
        }
        self.show_tip(self.locale.tr(
            "waiting for the session switch — your draft is kept",
            "正在等待会话切换，草稿已保留",
        ));
        true
    }

    /// Send raw text as an agent prompt (shared by submit and command
    /// passthroughs like /plan). While a turn runs the text joins the client's
    /// FIFO instead of the driver's channel, so `⌥↑` can still edit it.
    fn send_agent_text(&mut self, text: String, ctl: &Controller) {
        if self.waiting_for_session_switch() {
            return;
        }
        let running = self.turn_busy();
        let cell = self.transcript.cells.len();
        self.transcript.push_user(text.clone(), running);
        if running {
            self.enqueue_prompt(vec![StagedBlock::Text(text)], vec![cell]);
            self.show_tip(
                self.locale
                    .tr(
                        "queued ({n} waiting) — lands after this turn · ⌥↑ edits · ctrl+enter sends now",
                        "已排队（{n} 条等待）—— 本轮结束后送出 · ⌥↑ 编辑 · ctrl+enter 立即发送",
                    )
                    .replace("{n}", &self.queued.to_string()),
            );
            self.scroll_up = 0;
            return;
        }
        self.prompt_pending = true;
        self.state = RunState::Starting;
        self.run_started = Some(Instant::now());
        self.state_note = self
            .locale
            .tr("contacting runtime", "正在连接运行时")
            .into();
        self.scroll_up = 0;
        ctl.send(Cmd::Prompt {
            session_id: self.session_id.clone(),
            text,
        });
    }

    /// Is a turn in flight (or a prompt already handed over)? A runtime that is
    /// still *starting* with nothing in flight takes a prompt immediately —
    /// otherwise a first prompt could wait for an idle status that never comes.
    fn turn_busy(&self) -> bool {
        matches!(self.state, RunState::Running)
            || self.prompt_pending
            || !self.prompt_queue.is_empty()
    }

    /// Queue one prompt behind the active turn and mark its echo cells.
    fn enqueue_prompt(&mut self, blocks: Vec<StagedBlock>, cells: Vec<usize>) {
        let id = self.next_prompt_id();
        self.prompt_queue
            .push_back(QueuedPrompt { id, blocks, cells });
        self.queued = self.prompt_queue.len();
    }

    /// Send the FIFO head — the turn it waited behind has ended. The echo
    /// bubbles lose their queued tint (they were painted when queued), and the
    /// item leaves the queue before the driver sees it, so a `/clear` or a
    /// session switch can never double-send it.
    fn dispatch_next_queued(&mut self, ctl: &Controller) {
        if self.session_switch.is_some() {
            return;
        }
        if self.queue_edit.is_some() {
            // The item under edit keeps its slot and its pre-edit wording.
            self.state_note = self
                .locale
                .tr("queue paused for edit", "队列已暂停 · 正在编辑")
                .into();
            return;
        }
        let Some(prompt) = self.prompt_queue.pop_front() else {
            return;
        };
        self.queued = self.prompt_queue.len();
        self.transcript.mark_prompt_delivered(&prompt.cells);
        self.prompt_pending = true;
        self.state = RunState::Starting;
        self.run_started = Some(Instant::now());
        self.state_note = self
            .locale
            .tr("sending queued followup", "正在发送排队消息")
            .into();
        self.scroll_up = 0;
        let wire = prompt_blocks_from_staged(&prompt.blocks);
        self.send_wire_prompt(wire, None, ctl);
    }

    /// Fire one prompt at the driver: a lone text block takes the plain
    /// `Cmd::Prompt` (so a `/`-prefixed line still reaches the skill path),
    /// anything carrying images rides the image variants, and a steer carries
    /// its pending-bubble id.
    fn send_wire_prompt(
        &self,
        blocks: Vec<crate::bus::PromptBlock>,
        steer: Option<u64>,
        ctl: &Controller,
    ) {
        let text = match blocks.as_slice() {
            [crate::bus::PromptBlock::Text(text)] => Some(text.clone()),
            _ => None,
        };
        let session_id = self.session_id.clone();
        ctl.send(match (steer, text) {
            (Some(message_id), Some(text)) => Cmd::Steer {
                session_id,
                message_id,
                text,
            },
            (Some(message_id), None) => Cmd::SteerImages {
                session_id,
                message_id,
                blocks,
            },
            (None, Some(text)) => Cmd::Prompt { session_id, text },
            (None, None) => Cmd::PromptImages { session_id, blocks },
        });
    }

    /// `/image <path> [caption]` — stage a local raster in the composer; it is
    /// sent on the next Enter (caption becomes the prompt text).
    fn send_image(&mut self, arg: &str, _ctl: &Controller) {
        let (path, caption) = match arg.split_once(char::is_whitespace) {
            Some((p, rest)) => (p, rest.trim().to_string()),
            None => (arg, String::new()),
        };
        if path.is_empty() {
            self.show_tip(self.locale.tr(
                "/image needs a path — /image ./pic.png [caption]",
                "/image 需要路径 —— /image ./pic.png [说明]",
            ));
            return;
        }
        let Some(media_type) = media_type_for(path) else {
            self.show_tip(self.locale.tr(
                "unsupported image — use .png .jpg .jpeg .webp .gif",
                "不支持的图片格式 —— 请用 .png .jpg .jpeg .webp .gif",
            ));
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(err) => {
                self.show_tip(format!(
                    "{} {path}: {err}",
                    self.locale.tr("cannot read", "无法读取")
                ));
                return;
            }
        };
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let stored_path = std::fs::canonicalize(path)
            .or_else(|_| std::path::absolute(path))
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .into_owned();
        self.stage_image(name, stored_path, media_type.to_string(), bytes, caption);
    }

    /// `/clip [caption]` — stage the clipboard image in the composer.
    fn clip_image(&mut self, caption: &str, _ctl: &Controller) {
        match read_clipboard_image() {
            Some((bytes, media_type)) => self.stage_image(
                "clipboard.png".into(),
                "clipboard".into(),
                media_type.to_string(),
                bytes,
                caption.to_string(),
            ),
            None => self.show_tip(self.locale.tr(
                "clipboard has no image, or this platform isn't supported",
                "剪贴板里没有图片，或当前平台不支持",
            )),
        }
    }

    /// Stage an image as an inline `[image N]` chip at the cursor; up to
    /// [`crate::attachments::MAX_STAGED`] ride the next Enter with the text.
    fn stage_image(
        &mut self,
        name: String,
        path: String,
        media_type: String,
        data: Vec<u8>,
        caption: String,
    ) {
        let token = match self
            .pending_images
            .add(self.locale, name, path, media_type, data)
        {
            Ok(att) => att.token.clone(),
            Err(full) => {
                self.show_tip(full);
                return;
            }
        };
        if !caption.is_empty() {
            self.input.set(caption);
            self.input.insert_char(' ');
        } else if self.input.cursor_char() > 0
            && !self
                .input
                .buf()
                .chars()
                .nth(self.input.cursor_char() - 1)
                .is_none_or(char::is_whitespace)
        {
            self.input.insert_char(' ');
        }
        self.input.insert_str(&token);
        self.show_tip(self.locale.tr(
            "image staged — ⌫ deletes its chip · hover it to preview",
            "图片已暂存 —— ⌫ 删除它的筹码 · 悬停可预览",
        ));
        self.needs_redraw = true;
    }

    /// Drain the tray and split the draft on chip spans, in reading order.
    fn take_staged_blocks(&mut self) -> Vec<StagedBlock> {
        self.reconcile_attachments();
        let buf = self.input.buf().clone();
        split_draft_into_staged_blocks(&buf, self.pending_images.drain())
    }

    fn next_prompt_id(&mut self) -> u64 {
        let id = self.next_prompt_id;
        self.next_prompt_id = self.next_prompt_id.wrapping_add(1).max(1);
        id
    }

    /// Echo staged blocks in the transcript (text and image thumbnails in
    /// draft order) and send one prompt whose ACP blocks match that order.
    fn emit_staged_prompt(
        &mut self,
        staged: Vec<StagedBlock>,
        queued: bool,
        steer_message_id: Option<u64>,
        ctl: &Controller,
    ) {
        let first_cell = self.transcript.cells.len();
        for block in &staged {
            match block {
                StagedBlock::Text(text) => self.transcript.push_user(text.clone(), queued),
                StagedBlock::Image(att) => self.transcript.push_image(
                    att.name.clone(),
                    String::new(),
                    att.path.clone(),
                    att.data.clone(),
                    queued,
                ),
            }
        }
        let cells: Vec<usize> = (first_cell..self.transcript.cells.len()).collect();
        self.scroll_up = 0;
        if queued {
            self.enqueue_prompt(staged, cells);
            return;
        }
        // The wire form borrows the staged blocks (image payloads are `Arc`
        // clones), so a steer can hand the very same blocks to the pending
        // record for a deferred requeue.
        let wire = prompt_blocks_from_staged(&staged);
        if let Some(message_id) = steer_message_id {
            self.pending_steer_cells.insert(
                message_id,
                PendingSteer {
                    cells,
                    blocks: staged,
                },
            );
        }
        self.send_wire_prompt(wire, steer_message_id, ctl);
    }

    /// Submit path for the staged tray: set run state / queue bookkeeping,
    /// then emit the interleaved prompt.
    fn send_staged(&mut self, staged: Vec<StagedBlock>, ctl: &Controller) {
        if staged.is_empty() {
            return;
        }
        let n = staged
            .iter()
            .filter(|b| matches!(b, StagedBlock::Image(_)))
            .count();
        let running = self.turn_busy();
        if running {
            self.show_tip(if n <= 1 {
                self.locale
                    .tr(
                        "image queued ({n} waiting) — lands after this turn",
                        "图片已排队（{n} 条等待）—— 本轮结束后送出",
                    )
                    .replace("{n}", &(self.queued + 1).to_string())
            } else {
                self.locale
                    .tr(
                        "{n} images queued — land after this turn",
                        "已排队 {n} 张图片 —— 本轮结束后送出",
                    )
                    .replace("{n}", &n.to_string())
            });
        } else {
            self.prompt_pending = true;
            self.state = RunState::Starting;
            self.run_started = Some(Instant::now());
            self.state_note = if n <= 1 {
                self.locale.tr("sending image", "正在发送图片").into()
            } else {
                self.locale
                    .tr("sending {n} images", "正在发送 {n} 张图片")
                    .replace("{n}", &n.to_string())
            };
        }
        self.emit_staged_prompt(staged, running, None, ctl);
    }

    /// Send-now is ACP steering: issue another prompt immediately while the
    /// current turn remains active. Esc is the only cancellation path.
    fn send_now(&mut self, ctl: &Controller) {
        if self.waiting_for_session_switch() {
            return;
        }
        let raw = self.input.buf().trim().to_string();
        if !self.pending_images.is_empty() && raw.starts_with('/') {
            self.show_tip(self.locale.tr(
                "send or delete the [image] chips first — /commands don't take images",
                "先发送或删除草稿里的 [image] 图片 —— / 命令不接收图片",
            ));
            return;
        }
        let staged = if self.pending_images.is_empty() {
            if raw.is_empty() {
                return;
            }
            vec![StagedBlock::Text(raw)]
        } else {
            self.take_staged_blocks()
        };
        if staged.is_empty() {
            return;
        }
        let running = self.state == RunState::Running || self.prompt_pending || self.queued > 0;
        self.input.history.push(self.input.buf().clone());
        self.input.clear();
        let queued = false;
        let has_images = staged.iter().any(|b| matches!(b, StagedBlock::Image(_)));
        if has_images {
            let n = staged
                .iter()
                .filter(|b| matches!(b, StagedBlock::Image(_)))
                .count();
            if running {
                self.show_tip(self.locale.tr(
                    "steered with image — lands at the next agent step",
                    "已 steer（带图片）—— 在 Agent 下一步生效",
                ));
            } else {
                self.prompt_pending = true;
                self.state = RunState::Starting;
                self.run_started = Some(Instant::now());
                self.state_note = if n == 1 {
                    self.locale.tr("sending image", "正在发送图片").into()
                } else {
                    self.locale
                        .tr("sending {n} images", "正在发送 {n} 张图片")
                        .replace("{n}", &n.to_string())
                };
            }
            let steer_message_id = running.then(|| self.next_prompt_id());
            self.emit_staged_prompt(staged, queued, steer_message_id, ctl);
        } else {
            let text = match staged.into_iter().next() {
                Some(StagedBlock::Text(t)) => t,
                _ => return,
            };
            let cell = self.transcript.cells.len();
            self.transcript.push_user(text.clone(), queued);
            if running {
                self.show_tip(self.locale.tr(
                    "steered — lands at the next agent step",
                    "已 steer —— 在 Agent 下一步生效",
                ));
            } else {
                self.prompt_pending = true;
                self.state = RunState::Starting;
                self.run_started = Some(Instant::now());
            }
            self.scroll_up = 0;
            let message_id = if running {
                let message_id = self.next_prompt_id();
                self.pending_steer_cells.insert(
                    message_id,
                    PendingSteer {
                        cells: vec![cell],
                        blocks: vec![StagedBlock::Text(text.clone())],
                    },
                );
                Some(message_id)
            } else {
                None
            };
            self.send_wire_prompt(vec![crate::bus::PromptBlock::Text(text)], message_id, ctl);
        }
    }

    /// Submit a prompt programmatically (used by DSH_TUI_AUTOPROMPT).
    pub fn auto_prompt(&mut self, text: &str, ctl: &Controller) {
        self.input.set(text.to_string());
        self.submit(ctl);
    }
}

pub fn timestamp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{nanos:x}-{:x}-{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Slice `s` by display-cell range `[c0, c1)`: a char is included when its
/// cell span overlaps the range (so a double-width char straddling the
/// boundary is kept — matching what the highlight visually covers).
pub(crate) fn slice_by_cells(s: &str, c0: usize, c1: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0).max(1);
        if w + cw > c0 && w < c1 {
            out.push(ch);
        }
        w += cw;
        if w >= c1 {
            break;
        }
    }
    out
}

/// The whitespace-delimited word covering display column `col` of `line`:
/// `(start_col, cell_width, word)`. `None` on whitespace or past the end.
pub(crate) fn word_span(line: &str, col: usize) -> Option<(usize, usize, String)> {
    let cw = |ch: char| ch.width().unwrap_or(0).max(1);
    let chars: Vec<char> = line.chars().collect();
    let mut w = 0usize;
    let mut hit = None;
    for (i, ch) in chars.iter().enumerate() {
        if col < w + cw(*ch) {
            hit = Some(i);
            break;
        }
        w += cw(*ch);
    }
    let i = hit?;
    if chars[i].is_whitespace() {
        return None;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && !chars[a - 1].is_whitespace() {
        a -= 1;
    }
    while b + 1 < chars.len() && !chars[b + 1].is_whitespace() {
        b += 1;
    }
    let start_col: usize = chars[..a].iter().copied().map(cw).sum();
    let width: usize = chars[a..=b].iter().copied().map(cw).sum();
    Some((start_col, width, chars[a..=b].iter().collect()))
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use std::path::PathBuf;

    fn test_app_with_root(root: &str, workspace: &str) -> (App, Controller) {
        let cfg = RuntimeConfig {
            workspace: workspace.into(),
            home: root.into(),
            sessions_root: root.into(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let mut app = App::new(Theme::dark(), cfg, "dsh-current".into());
        app.locale = crate::locale::Locale::En;
        (app, crate::controller::test_controller().0)
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dsh-resume-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Leaving prints the session that was on screen — a `/resume` switch moves
    /// the id — plus the two ways back, in the interface language.
    #[test]
    fn the_exit_notice_names_the_session_and_the_way_back() {
        let root = tmp_root("exit-notice");
        let (mut app, _demo_ctl) = test_app_with_root(root.to_str().unwrap(), "/w");
        for (locale, id) in [
            (Locale::En, "dsh-current"),
            (Locale::Zh, "aby-20250101-1200-a1b2"),
        ] {
            app.locale = locale;
            app.session_id = id.into();
            let notice = app.exit_notice();
            assert_eq!(
                notice.lines().count(),
                2,
                "the id, then the way back: {notice}"
            );
            assert!(notice.contains(id), "{notice}");
            assert!(
                notice.contains(&format!("abylab --session-id {id}")),
                "the resume command carries the exact id: {notice}"
            );
            assert!(notice.contains("/resume"), "{notice}");
        }
    }

    #[test]
    fn keys_slash_opens_a_local_modal_without_polluting_the_timeline() {
        let root = tmp_root("keys");
        let (mut app, _demo_ctl) = test_app_with_root(root.to_str().unwrap(), "/w");
        let (ctl, commands) = crate::controller::test_controller();
        let cells_before = app.transcript.cells.len();
        app.input.set("/keys".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        assert_eq!(
            app.transcript.cells.len(),
            cells_before,
            "/keys is chrome and must not enter the conversation timeline"
        );
        assert!(app.view_overlay.is_some(), "/keys modal should open");
        let frame = crate::ui::dump_frame(&mut app, 100, 30);
        assert!(
            frame.contains("Keyboard shortcuts"),
            "modal title:\n{frame}"
        );
        assert!(frame.contains("ctrl+q"), "quit binding missing:\n{frame}");

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &ctl);
        // 44 rows: the card keeps `DIALOG_MARGIN_Y` off the screen edges and
        // stops above the composer, so this tail needs a taller terminal than
        // the 30 rows the pre-margin card got away with.
        let frame = crate::ui::dump_frame(&mut app, 100, 44);
        assert!(
            frame.contains("shift+tab") && frame.contains("permission"),
            "permission binding missing:\n{frame}"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.view_overlay.is_none(), "esc closes the local modal");
        assert!(
            matches!(
                commands.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "closing a builtin modal must not emit a plugin overlay event"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `/help` uses the same local modal as `/keys`: a dialog over the chat,
    /// never a timeline entry.
    #[test]
    fn help_slash_opens_a_local_modal_without_polluting_the_timeline() {
        let root = tmp_root("help");
        let (mut app, _demo_ctl) = test_app_with_root(root.to_str().unwrap(), "/w");
        let (ctl, commands) = crate::controller::test_controller();
        let cells_before = app.transcript.cells.len();
        app.input.set("/help".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        assert_eq!(
            app.transcript.cells.len(),
            cells_before,
            "/help is chrome and must not enter the conversation timeline"
        );
        let overlay = app.view_overlay.as_ref().expect("/help modal should open");
        assert_eq!(overlay.title, "Help");
        // 40 rows: the card is scrollable, and the body has to reach `!cmd`.
        let frame = crate::ui::dump_frame(&mut app, 100, 40);
        assert!(frame.contains("Help · ↑↓/wheel scroll"), "modal:\n{frame}");
        assert!(frame.contains("ctrl+enter"), "binding missing:\n{frame}");
        assert!(frame.contains("!cmd"), "shell hint missing:\n{frame}");
        assert!(
            !frame.contains("## help"),
            "the border names the dialog — no repeated heading:\n{frame}"
        );
        // The card covers the middle of the screen, not the composer: the
        // draft well (and its placeholder) is still painted.
        assert!(
            frame.contains("describe what you want to build"),
            "composer well:\n{frame}"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.view_overlay.is_none(), "esc closes the local modal");
        assert!(
            matches!(
                commands.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "closing a builtin modal must not emit a plugin overlay event"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_session_switch_preserves_history_identity_and_draft() {
        for new_session in [false, true] {
            let root = tmp_root("failed-switch");
            let (mut app, _) = test_app_with_root(root.to_str().unwrap(), "/w");
            let (ctl, commands) = crate::controller::test_controller();
            let previous = app.session_id.clone();
            app.transcript.push_user("original history".into(), false);
            if new_session {
                app.run_slash("new", "", &ctl);
                assert!(matches!(commands.try_recv(), Ok(Cmd::NewSession)));
            } else {
                app.load_acp_session("target", &ctl);
                assert!(
                    matches!(commands.try_recv(), Ok(Cmd::LoadSession { session_id }) if session_id == "target")
                );
            }
            app.input.set("unsent draft".into());
            app.submit(&ctl);
            app.send_now(&ctl);
            assert_eq!(app.input.buf(), "unsent draft");
            assert!(commands.try_recv().is_err(), "no prompts during a switch");
            app.handle(
                AppEvent::Ctl(CtlEvent::TuiOpFailed("unrelated operation".into())),
                &ctl,
            );
            assert!(
                app.session_switch.is_some(),
                "unrelated failures cannot acknowledge the switch"
            );
            app.handle(
                AppEvent::Ctl(CtlEvent::SessionSwitchFailed("cannot load target".into())),
                &ctl,
            );
            assert!(app.session_switch.is_none());
            assert_eq!(app.session_id, previous);
            assert!(app.session_bound);
            assert!(app.transcript.cells.iter().any(|cell| matches!(&cell.kind,
                crate::transcript::CellKind::User { text, .. } if text == "original history")));
            assert_eq!(app.input.buf(), "unsent draft");
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn resume_ack_commits_the_view_and_never_moves_the_old_queue_to_the_new_session() {
        let root = tmp_root("switch-queue");
        let (mut app, _) = test_app_with_root(root.to_str().unwrap(), "/w");
        let (ctl, commands) = crate::controller::test_controller();
        let previous = app.session_id.clone();
        app.state = RunState::Running;
        app.send_agent_text("queued for old session".into(), &ctl);
        app.load_acp_session("target", &ctl);
        assert!(matches!(commands.try_recv(), Ok(Cmd::LoadSession { .. })));
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: previous.clone(),
                running: false,
            }),
            &ctl,
        );
        assert_eq!(app.session_id, previous);
        assert_eq!(app.queued, 1);
        assert!(
            commands.try_recv().is_err(),
            "idle must not dispatch through a pending switch"
        );
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "target".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(app.session_id, "target");
        assert_eq!(app.queued, 0);
        assert!(app.transcript.cells.is_empty());
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::UserMessage {
                session: "target".into(),
                text: "restored history".into(),
            }),
            &ctl,
        );
        app.input.set("new draft".into());
        app.submit(&ctl);
        let sent: Vec<_> = commands.try_iter().collect();
        assert_eq!(
            sent.iter()
                .filter(|cmd| matches!(cmd, Cmd::Prompt { .. }))
                .count(),
            1
        );
        assert!(sent.iter().any(|cmd| matches!(cmd, Cmd::Prompt { session_id, text } if session_id == "target" && text == "new draft")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_switch_resumes_the_previous_sessions_queue() {
        let root = tmp_root("failed-switch-queue");
        let (mut app, _) = test_app_with_root(root.to_str().unwrap(), "/w");
        let (ctl, commands) = crate::controller::test_controller();
        let previous = app.session_id.clone();
        app.state = RunState::Running;
        app.send_agent_text("queued for old session".into(), &ctl);
        app.load_acp_session("target", &ctl);
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: previous.clone(),
                running: false,
            }),
            &ctl,
        );
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionSwitchFailed("cannot load target".into())),
            &ctl,
        );
        assert_eq!(app.session_id, previous);
        assert_eq!(app.queued, 0);
        assert!(commands.try_iter().any(|cmd| matches!(cmd, Cmd::Prompt { session_id, text } if session_id == previous && text == "queued for old session")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_picker_lists_sessions_and_prefix_resolves() {
        let root = tmp_root("picker");
        let (mut app, ctl) = test_app_with_root(root.to_str().unwrap(), "/w");

        app.run_slash("resume", "", &ctl);
        // The driver-backed path: the SessionList rows open the picker.
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![crate::bus::SessionListItem {
                    id: "dsh-alpha".into(),
                    title: Some("fix failing tests".into()),
                    updated_at: None,
                }],
                prefix: None,
            }),
            &ctl,
        );
        let picker = app.picker.as_ref().expect("picker opens");
        assert!(matches!(picker.kind, PickerKind::Session));
        assert_eq!(picker.items[0].id, "dsh-alpha");
        // The human handle is the label; the meta carries the short id.
        assert_eq!(picker.items[0].label, "fix failing tests", "title as label");
        assert!(
            picker.items[0].meta.contains("dsh-alp"),
            "short id in meta: {}",
            picker.items[0].meta
        );
        assert!(
            picker.title.contains('1'),
            "picker title counts sessions: {}",
            picker.title
        );
        app.picker = None;

        // unique prefix resolves; unknown id warns and keeps the session
        app.run_slash("resume", "dsh-al", &ctl);
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![crate::bus::SessionListItem {
                    id: "dsh-alpha".into(),
                    title: None,
                    updated_at: None,
                }],
                prefix: Some("dsh-al".into()),
            }),
            &ctl,
        );
        assert_ne!(
            app.session_id, "dsh-alpha",
            "wait for the driver's acknowledgement"
        );
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "dsh-alpha".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(app.session_id, "dsh-alpha");
        app.run_slash("resume", "nope", &ctl);
        assert_eq!(app.session_id, "dsh-alpha", "unknown prefix leaves session");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_with_no_sessions_notices_instead_of_picker() {
        let root = tmp_root("empty");
        let (mut app, ctl) = test_app_with_root(root.to_str().unwrap(), "/w");
        app.run_slash("resume", "", &ctl);
        assert!(app.picker.is_none(), "no picker without sessions");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn picker_page_keys_jump_a_screenful_and_home_end_pin_the_ends() {
        let root = tmp_root("page");
        let (mut app, ctl) = test_app_with_root(root.to_str().unwrap(), "/w");
        app.picker = Some(Picker {
            kind: PickerKind::Session,
            title: " resume session · 40 sessions · enter select · esc close ".into(),
            sel: 0,
            items: (0..40)
                .map(|i| PickerItem {
                    id: format!("s{i:02}"),
                    label: format!("session {i:02}"),
                    meta: String::new(),
                    provider: None,
                })
                .collect(),
        });
        // The draw pass records how many rows the popup shows; page keys
        // move exactly that far (a 10-row popup in this test).
        app.picker_page_rows = 10;

        let key = |app: &mut App, code: KeyCode| {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &ctl);
        };

        key(&mut app, KeyCode::PageDown);
        assert_eq!(app.picker.as_ref().unwrap().sel, 10, "page down");
        key(&mut app, KeyCode::PageDown);
        assert_eq!(app.picker.as_ref().unwrap().sel, 20, "page down again");
        key(&mut app, KeyCode::End);
        assert_eq!(app.picker.as_ref().unwrap().sel, 39, "end pins the tail");
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.picker.as_ref().unwrap().sel, 29, "page up");
        key(&mut app, KeyCode::Home);
        assert_eq!(app.picker.as_ref().unwrap().sel, 0, "home pins the head");
        key(&mut app, KeyCode::PageUp);
        assert_eq!(
            app.picker.as_ref().unwrap().sel,
            0,
            "page up sticks at head"
        );
        key(&mut app, KeyCode::Up);
        assert_eq!(app.picker.as_ref().unwrap().sel, 39, "↑ still wraps");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    /// Unique session root per call — keeps the modes cache from leaking
    /// between tests and runs.
    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dsh-tui-sel-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn view(lines: &[&str]) -> ChatView {
        ChatView {
            area: ratatui::layout::Rect::new(1, 0, 60, 10),
            top: 0,
            total: lines.len(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            owners: vec![None; lines.len()],
            images: Vec::new(),
        }
    }

    fn sel(a: (usize, usize), h: (usize, usize)) -> Selection {
        Selection {
            anchor: SelPoint {
                line: a.0,
                col: a.1,
            },
            head: SelPoint {
                line: h.0,
                col: h.1,
            },
        }
    }

    fn test_app() -> App {
        let cfg = RuntimeConfig {
            workspace: "/tmp".into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let mut app = App::new(crate::theme::Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        app
    }

    #[test]
    fn slice_by_cells_handles_wide_chars() {
        assert_eq!(slice_by_cells("hello world", 6, 11), "world");
        // 选=2 cells: [0,2) 中=[2,4) 即=[4,6)
        assert_eq!(slice_by_cells("选中即copy", 2, 6), "中即");
        // a boundary-straddling wide char is kept
        assert_eq!(slice_by_cells("选中", 1, 3), "选中");
        assert_eq!(slice_by_cells("abc", 0, usize::MAX), "abc");
    }

    #[test]
    fn selection_text_joins_lines_and_orders_reverse_drags() {
        let mut app = test_app();
        app.chat_view = view(&["first line  ", "second", "third"]);
        // forward drag: line0 col6 → line2 col2 (inclusive)
        let fwd = app.selection_text(sel((0, 6), (2, 2)));
        assert_eq!(fwd, "line\nsecond\nthi");
        // dragging upward yields the same text
        let rev = app.selection_text(sel((2, 2), (0, 6)));
        assert_eq!(fwd, rev);
    }

    /// A click in the well places the caret at the clicked char; a drag arms
    /// the composer highlight, and releasing copies the covered text.
    #[test]
    fn clicking_and_dragging_in_the_well_places_the_caret_and_selects() {
        let mut app = test_app();
        app.input.set("hello world".into());
        // The well: rows 10..13, prompt "❯ " in columns 0..2.
        app.composer_area = Rect::new(0, 10, 40, 3);
        app.composer_wrap_width = 38;

        // Click on the 4th text column → char 3 (right of "hel").
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.input.cursor_char(), 3, "the caret follows the click");
        assert!(app.input_selecting, "the click arms a drag");

        // Drag right to the 10th text column (cell 11 - prompt 2), inclusive.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 11,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        let (a, b) = app.input_selection_range().expect("a drag covers text");
        assert_eq!(
            app.input.chars_between(a, b),
            "lo worl",
            "cells inclusive, either direction"
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 11,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.input_selecting);
        assert!(
            app.input_sel.is_some(),
            "the highlight survives the release (esc clears it)"
        );

        // Ctrl+X cuts the dragged range: the highlight goes with it.
        let (ctl, _commands) = crate::controller::test_controller();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert_eq!(app.input.buf(), "held", "the selection was cut");
        assert!(app.input_sel.is_none(), "cut clears the highlight");
    }

    /// A click that never moves is a caret placement, not a selection, and a
    /// click outside the well dismisses a lingering highlight.
    #[test]
    fn a_caret_click_clears_the_highlight_and_outside_clicks_dismiss_it() {
        let mut app = test_app();
        app.input.set("hello".into());
        app.composer_area = Rect::new(0, 10, 40, 3);
        app.composer_wrap_width = 38;

        app.input_sel = Some(InputSel {
            anchor: (0, 0),
            head: (0, 3),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 3,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.input_sel.is_none(), "a plain click is just a caret");
        assert_eq!(app.input.cursor_char(), 1);

        app.input_sel = Some(InputSel {
            anchor: (0, 0),
            head: (0, 3),
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 39,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.input_sel.is_none(), "outside the well dismisses it");
        assert!(!app.input_selecting);
    }

    #[test]
    fn chat_hit_maps_screen_cells_to_layout_lines() {
        let mut app = test_app();
        app.chat_view = view(&["a", "b", "c", "d"]);
        app.chat_view.top = 2;
        let p = app.chat_hit(3, 1).expect("inside pane");
        assert_eq!((p.line, p.col), (3, 2)); // top=2 + row 1, col 3-x(1)
        assert!(app.chat_hit(0, 0).is_none(), "left of pane");
        assert!(app.chat_hit(3, 10).is_none(), "below pane");
    }

    #[test]
    fn word_span_finds_word_under_column() {
        let (col, width, word) = word_span("run cargo test now", 6).expect("word");
        assert_eq!((col, width, word.as_str()), (4, 5, "cargo"));
        assert!(word_span("run cargo", 3).is_none(), "whitespace");
        assert!(word_span("run", 99).is_none(), "past end");
        let (col, width, word) = word_span("选中即复制 ok", 4).expect("cjk word");
        assert_eq!((col, width, word.as_str()), (0, 10, "选中即复制"));
    }

    #[test]
    fn tool_click_toggles_output_expansion() {
        let mut app = test_app();
        app.transcript.apply(crate::events::UiEvent::ToolCall {
            session: "dsh-test".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        app.transcript.apply(crate::events::UiEvent::ToolResult {
            session: "dsh-test".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "a\nb\nc\nd\ne\nf\ng\nh".into(),
            error: None,
        });
        app.chat_view.area = ratatui::layout::Rect::new(1, 0, 40, 10);
        app.chat_view.top = 0;
        app.chat_view.lines = vec!["tool line".into()];
        app.chat_view.owners = vec![Some(0)];

        assert_eq!(app.tool_at(2, 0), Some(0), "tool line owns its cell");
        assert!(!app.transcript.cells[0].expanded);
        app.toggle_tool(0);
        assert!(app.transcript.cells[0].expanded, "click expands");
        app.toggle_tool(0);
        assert!(!app.transcript.cells[0].expanded, "click collapses");
    }

    #[test]
    fn wheel_over_collapsed_tool_scrolls_the_transcript() {
        let mut app = test_app();
        app.transcript.apply(crate::events::UiEvent::ToolCall {
            session: "dsh-test".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        app.transcript.apply(crate::events::UiEvent::ToolResult {
            session: "dsh-test".into(),
            call_id: "c1".into(),
            is_error: false,
            text: "a\nb\nc\nd\ne\nf\ng\nh".into(),
            error: None,
        });
        app.chat_view.area = ratatui::layout::Rect::new(1, 0, 40, 10);
        app.chat_view.top = 0;
        app.chat_view.lines = vec!["tool line".into()];
        app.chat_view.owners = vec![Some(0)];

        app.mouse_scroll(3, 2, 0);

        assert_eq!(app.scroll_up, 3, "tool cards must not swallow the wheel");
    }

    #[test]
    fn tool_click_in_a_child_view_targets_the_child_transcript() {
        let mut app = test_app();
        let mut transcript = Transcript::new("child-1".into());
        transcript.apply(crate::events::UiEvent::ToolCall {
            session: "child-1".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        });
        app.subagents.push(SubagentView {
            id: "child-1".into(),
            parent: "dsh-test".into(),
            label: "subagent 1".into(),
            running: true,
            transcript,
        });
        app.active_subagent = Some("child-1".into());

        app.toggle_tool(0);

        assert!(app.subagents[0].transcript.cells[0].expanded);
        assert!(app.transcript.cells.is_empty());
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;
    use crate::bus::SessionListItem;
    use std::sync::mpsc::Receiver;

    /// Unique session root per call — keeps the modes cache from leaking
    /// between tests and runs.
    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dsh-tui-mode-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn test_cfg() -> RuntimeConfig {
        RuntimeConfig {
            workspace: "/tmp".into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        }
    }

    fn test_app() -> (App, Controller, Receiver<AppEvent>) {
        let cfg = test_cfg();
        let (_tx, rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        (app, ctl, rx)
    }

    #[test]
    fn mode_facts_cache_per_workspace_across_instances() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let mut app = App::new(Theme::dark(), cfg.clone(), "s1".into());
        app.modes.approval = Some("ask".into());
        app.modes.permission = Some("workspace-write".into());
        app.modes.effort = Some("max".into());
        app.modes.plan = true;
        app.save_modes_cache();

        // A second instance in the same workspace boots with the cached
        // facts — except plan, which never carries over.
        let app2 = App::new(Theme::dark(), cfg.clone(), "s2".into());
        assert_eq!(app2.modes.approval.as_deref(), Some("ask"));
        assert_eq!(app2.modes.permission.as_deref(), Some("workspace-write"));
        assert_eq!(app2.modes.effort.as_deref(), Some("max"));
        assert!(!app2.modes.plan, "plan is per-session");

        // Another workspace in the same root stays untouched.
        let mut other = cfg;
        other.workspace = "/elsewhere".into();
        let app3 = App::new(Theme::dark(), other, "s3".into());
        assert!(app3.modes.permission.is_none(), "cache is per workspace");
    }

    /// The launch splash reports the four facts a user wants before the first
    /// prompt: build version, working directory, permission preset, model.
    /// A preset cached from an earlier run is what the splash names.
    #[test]
    fn splash_reports_version_cwd_permission_and_model() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let mut app = App::new(Theme::dark(), cfg, "s1".into());
        app.locale = crate::locale::Locale::En;
        app.modes.permission = Some("workspace-write".into());
        app.modes.effort = Some("max".into());
        app.push_banner();

        assert_eq!(
            banner_facts(&app),
            vec![
                ("version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
                ("cwd".to_string(), "/tmp".to_string()),
                ("permission".to_string(), "workspace-write".to_string()),
                (
                    "model".to_string(),
                    "deepseek-v4-flash · effort max".to_string()
                ),
            ]
        );

        // The labels follow the interface language; the values do not — the
        // one exception is the effort word, which is a label of its own.
        let mut zh = test_app().0;
        zh.locale = crate::locale::Locale::Zh;
        zh.modes.effort = Some("max".into());
        zh.push_banner();
        let facts = banner_facts(&zh);
        let labels: Vec<String> = facts.iter().map(|(l, _)| l.clone()).collect();
        assert_eq!(labels, ["版本", "工作目录", "权限", "模型"]);
        assert_eq!(
            facts[3].1, "deepseek-v4-flash · 推理强度 max",
            "zh names the effort"
        );
    }

    /// `/resume` mid-turn: the picker opens as soon as the driver's listing
    /// lands (the driver answers store queries off its turn loop), while the
    /// load itself waits for the turn and says so.
    #[test]
    fn resume_mid_turn_lists_now_and_says_when_the_load_lands() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::Zh;
        app.state = RunState::Running;
        app.run_started = Some(std::time::Instant::now());
        assert!(app.turn_busy());

        app.run_slash("resume", "", &ctl);
        assert_eq!(
            app.tip.as_ref().map(|(text, _)| text.as_str()),
            Some("正在列出会话…")
        );
        assert!(app.picker.is_none(), "the picker waits for the listing");

        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![crate::bus::SessionListItem {
                    id: "aby-1".into(),
                    title: Some("修复登录".into()),
                    updated_at: None,
                }],
                prefix: None,
            }),
            &ctl,
        );
        let picker = app.picker.as_ref().expect("the listing opens the picker");
        assert!(matches!(picker.kind, PickerKind::Session));
        assert_eq!(picker.items[0].id, "aby-1");

        // Picking one hands it to the driver, which is busy: the tip names the
        // wait instead of pretending the load already started.
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        let tip = app
            .tip
            .as_ref()
            .map(|(text, _)| text.clone())
            .unwrap_or_default();
        assert!(tip.starts_with("正在加载会话 aby-1 …"), "{tip}");
        assert!(tip.ends_with("（本轮结束后）"), "{tip}");
    }

    /// `/lang` retells every timeline the app paints — the root one and each
    /// subagent's — so the next notice already speaks the new language, and a
    /// subagent that starts later is born in it.
    #[test]
    fn lang_switch_retells_the_timelines() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::En;
        app.transcript.set_locale(Locale::En);
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });

        app.run_slash("lang", "zh", &ctl);
        assert_eq!(app.locale, Locale::Zh);

        app.apply_ui(crate::events::UiEvent::SessionTitle {
            session: "dsh-test".into(),
            title: "修复登录".into(),
        });
        app.apply_ui(crate::events::UiEvent::SessionTitle {
            session: "child-1".into(),
            title: "子会话".into(),
        });
        let notices = |tr: &crate::transcript::Transcript| -> Vec<String> {
            tr.cells
                .iter()
                .filter_map(|cell| match &cell.kind {
                    crate::transcript::CellKind::Notice { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        assert!(
            notices(&app.transcript).contains(&"会话 · 修复登录".to_string()),
            "{:?}",
            notices(&app.transcript)
        );
        let child = app
            .subagents
            .iter()
            .find(|view| view.id == "child-1")
            .expect("subagent view");
        assert!(
            notices(&child.transcript).contains(&"会话 · 子会话".to_string()),
            "{:?}",
            notices(&child.transcript)
        );

        // A subagent that starts after the switch is born in the new language.
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-2".into(),
            label: None,
        });
        app.apply_ui(crate::events::UiEvent::SessionTitle {
            session: "child-2".into(),
            title: "第二个".into(),
        });
        let fresh = app
            .subagents
            .iter()
            .find(|view| view.id == "child-2")
            .expect("second subagent view");
        assert!(
            notices(&fresh.transcript).contains(&"会话 · 第二个".to_string()),
            "the new timeline follows /lang: {:?}",
            notices(&fresh.transcript)
        );
    }

    /// Client-side command feedback speaks the interface language: the same
    /// commands an English session answers in English answer a Chinese one in
    /// Chinese (the values they carry stay identifiers).
    #[test]
    fn command_feedback_follows_the_interface_language() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::Zh;
        let tip = |app: &App| app.tip.as_ref().map(|(text, _)| text.clone());

        app.run_slash("image", "", &ctl);
        assert_eq!(
            tip(&app).as_deref(),
            Some("/image 需要路径 —— /image ./pic.png [说明]")
        );

        app.run_slash("image", "./missing.mp3", &ctl);
        assert_eq!(
            tip(&app).as_deref(),
            Some("不支持的图片格式 —— 请用 .png .jpg .jpeg .webp .gif")
        );

        app.run_slash("theme", "nope", &ctl);
        assert_eq!(tip(&app).as_deref(), Some("未知主题包: nope"));
        let notices: Vec<String> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices.contains(&"未知主题包 `nope`".to_string()),
            "{notices:?}"
        );

        app.run_slash("clear", "", &ctl);
        assert!(notices_tail(&app).contains(&"滚动区已清空".to_string()));

        // An empty store notices instead of opening a picker, in zh too.
        app.run_slash("resume", "", &ctl);
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![],
                prefix: None,
            }),
            &ctl,
        );
        assert!(
            notices_tail(&app)
                .contains(&"这个工作区还没有持久会话 —— 先跑完一轮，/resume 就能看到".to_string()),
            "{:?}",
            notices_tail(&app)
        );
    }

    /// The notices currently in the timeline, newest last.
    fn notices_tail(app: &App) -> Vec<String> {
        app.transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The facts the splash cell carries, in paint order.
    fn banner_facts(app: &App) -> Vec<(String, String)> {
        app.transcript
            .cells
            .iter()
            .find_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Banner { facts, .. } => Some(facts.clone()),
                _ => None,
            })
            .expect("splash banner")
    }

    #[test]
    fn selected_model_clears_once_a_turn_streams_on_it() {
        let (mut app, ctl, _rx) = test_app();
        app.set_model("deepseek-v4-pro".into(), &ctl);
        assert_eq!(app.selected_model.as_deref(), Some("deepseek-v4-pro"));
        // The next turn streams on the picked model → the pick is realized
        // and the stream fact takes over.
        app.transcript.last_model = Some("deepseek-v4-pro".into());
        app.handle(AppEvent::Ctl(CtlEvent::TuiOpDone("noop".into())), &ctl);
        assert_eq!(
            app.selected_model, None,
            "realized pick defers to the stream"
        );
    }

    #[test]
    fn slash_menu_offers_the_client_language_switch() {
        let (mut app, _ctl, _rx) = test_app();
        app.input.set("/lang".into());

        let matches = app.slash_matches();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "lang");
        assert_eq!(matches[0].usage, "/lang [zh|en]");
    }

    /// `/vim` was dispatch-only — typing it worked, but the `/` menu never
    /// listed it. Both halves are pinned here: the row exists (with its
    /// localized description) and the bare command still toggles.
    #[test]
    fn slash_menu_offers_the_vim_toggle() {
        let (mut app, ctl, _rx) = test_app();
        app.input.set("/vim".into());

        let matches = app.slash_matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "vim");
        assert_eq!(matches[0].usage, "/vim [on|off]");
        assert_eq!(matches[0].desc, "toggle vim modal editing in the composer");

        app.locale = crate::locale::Locale::Zh;
        assert_eq!(
            app.slash_matches()[0].desc,
            "切换 vim 模态编辑（默认关闭）",
            "the menu uses the zh description"
        );

        app.run_slash("vim", "", &ctl);
        assert!(app.vim.is_active(), "a bare /vim toggles it on");
        app.run_slash("vim", "off", &ctl);
        assert!(!app.vim.is_active(), "/vim off turns it off");
    }

    #[test]
    fn lang_switch_repaints_immediately_and_persists_for_the_workspace() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg.clone(), "s1".into());

        app.run_slash("lang", "zh", &ctl);

        let frame = crate::ui::dump_frame(&mut app, 100, 24);
        assert!(
            frame.replace(' ', "").contains("描述你想构建的内容"),
            "{frame}"
        );

        let mut restarted = App::new(Theme::dark(), cfg, "s2".into());
        let frame = crate::ui::dump_frame(&mut restarted, 100, 24);
        assert!(
            frame.replace(' ', "").contains("描述你想构建的内容"),
            "{frame}"
        );
        let current = std::path::Path::new(&restarted.cfg.home).join("settings.json");
        let saved: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&current).expect("read persisted settings"),
        )
        .unwrap();
        assert_eq!(
            saved["language"], "zh",
            "/lang persists the language for the workspace"
        );
    }

    #[test]
    fn child_session_updates_do_not_enter_the_parent_transcript() {
        let (mut app, _ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        app.apply_ui(crate::events::UiEvent::TextDelta {
            session: "child-1".into(),
            text: "child-only output".into(),
        });

        let rendered = app
            .transcript
            .lines(&Theme::dark(), 80, '⠋')
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(
            !rendered.contains("child-only output"),
            "child content must stay out of the parent transcript: {rendered}"
        );
    }

    #[test]
    fn a_running_subagent_keeps_the_spinner_advancing() {
        let (mut app, _ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        let before = app.spinner_idx;

        app.tick();

        assert_ne!(app.spinner_idx, before);
    }

    #[test]
    fn down_on_an_empty_prompt_opens_the_agent_switcher() {
        let (mut app, ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);

        let picker = app.picker.as_ref().expect("agent switcher opens");
        assert!(picker.title.contains("agents"));
        let ids: Vec<&str> = picker.items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["dsh-test", "child-1"]);
    }

    #[test]
    fn enter_from_the_agent_switcher_opens_the_child_transcript() {
        let (mut app, ctl, _rx) = test_app();
        app.transcript.push_user("main-only text".into(), false);
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        app.apply_ui(crate::events::UiEvent::TextDelta {
            session: "child-1".into(),
            text: "child-only output".into(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        let frame = crate::ui::dump_frame(&mut app, 100, 24);
        assert!(frame.contains("child-only output"), "{frame}");
        assert!(!frame.contains("main-only text"), "{frame}");
    }

    #[test]
    fn esc_from_a_child_transcript_returns_to_main() {
        let (mut app, ctl, _rx) = test_app();
        app.transcript.push_user("main-only text".into(), false);
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        app.apply_ui(crate::events::UiEvent::TextDelta {
            session: "child-1".into(),
            text: "child-only output".into(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);

        let frame = crate::ui::dump_frame(&mut app, 100, 24);
        assert!(frame.contains("main-only text"), "{frame}");
        assert!(!frame.contains("child-only output"), "{frame}");
    }

    #[test]
    fn child_transcript_view_does_not_accept_composer_input() {
        let (mut app, ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        assert!(
            app.input.is_empty(),
            "child view must not edit the main draft"
        );
        assert!(
            app.transcript.cells.iter().all(|cell| !matches!(
                &cell.kind,
                crate::transcript::CellKind::User { text, .. } if text == "x"
            )),
            "child view must not submit a main-session prompt"
        );
    }

    /// A child view replaces the composer, so a frame without a well must
    /// clear the mouse seam: a click where the parent's well used to be can
    /// neither move the main draft's caret nor arm a drag.
    #[test]
    fn a_child_view_frame_clears_the_well_hit_target() {
        let (mut app, _ctl, _rx) = test_app();
        app.input.set("parent draft".into());
        let _ = crate::ui::dump_frame(&mut app, 100, 24);
        let well = app.composer_area;
        assert!(
            well.height > 0,
            "the parent frame records the well: {well:?}"
        );

        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        // The switcher's ↓ only opens on an empty draft, so take the view the
        // way it lands it and keep the parent draft in place.
        app.active_subagent = Some("child-1".into());
        assert_eq!(app.active_subagent.as_deref(), Some("child-1"));

        let _ = crate::ui::dump_frame(&mut app, 100, 24);
        assert_eq!(app.composer_area, Rect::default(), "the well is gone");

        let caret = app.input.cursor_char();
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: well.x + 4,
            row: well.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.input_selecting, "no composer drag in a child view");
        assert!(app.input_sel.is_none());
        assert_eq!(app.input.cursor_char(), caret, "the draft caret stays put");
    }

    #[test]
    fn down_from_a_child_view_preselects_the_next_agent() {
        let (mut app, ctl, _rx) = test_app();
        for child in ["child-1", "child-2"] {
            app.apply_ui(crate::events::UiEvent::SubagentStarted {
                parent: "dsh-test".into(),
                child: child.into(),
                label: None,
            });
        }
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        assert_eq!(app.active_subagent.as_deref(), Some("child-1"));

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);

        let picker = app.picker.as_ref().expect("agent switcher reopens");
        assert_eq!(picker.items[picker.sel].id, "child-2");
    }

    #[test]
    fn q_from_a_child_transcript_returns_to_main() {
        let (mut app, ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &ctl);

        assert!(app.active_subagent.is_none());
    }

    #[test]
    fn a_new_session_clears_the_previous_agent_views() {
        let (mut app, ctl, _rx) = test_app();
        app.apply_ui(crate::events::UiEvent::SubagentStarted {
            parent: "dsh-test".into(),
            child: "child-1".into(),
            label: None,
        });
        assert_eq!(app.subagents.len(), 1);

        app.run_slash("new", "fresh", &ctl);

        assert_eq!(
            app.subagents.len(),
            1,
            "keep the old view until the switch succeeds"
        );
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "new-session".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert!(app.subagents.is_empty());
        assert!(app.active_subagent.is_none());
    }

    #[test]
    fn cmd_right_moves_to_the_current_wrapped_line_end_not_the_draft_end() {
        let (mut app, ctl, _rx) = test_app();
        app.input.set("abcdefghij".into());
        app.input.set_cursor_char(6);
        app.composer_wrap_width = 4;

        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER), &ctl);

        assert_eq!(
            app.input.cursor_char(),
            8,
            "second visual row ends after 'h'"
        );
        // The layout mirror keeps the upstream line-end affinity: the caret
        // sits at the end of the second visual row (row 1, display col 4).
        let layout = app.input.layout(4);
        let row = layout
            .rows
            .iter()
            .position(|r| r.start_char <= 8 && 8 <= r.end_char)
            .expect("cursor row in the layout mirror");
        assert_eq!(row, 1, "second visual row");
        assert_eq!(8 - layout.rows[row].start_char, 4, "display col 4");
    }

    #[test]
    fn selecting_same_model_id_from_another_provider_switches_provider() {
        let (mut app, ctl, _rx) = test_app();
        app.cfg.provider = "coding-plan-a".into();
        app.cfg.model = "deepseek-v4".into();

        app.select_model(
            PickerItem {
                id: "deepseek-v4".into(),
                label: "deepseek-v4".into(),
                meta: "coding-plan-b · DeepSeek V4".into(),
                provider: Some("coding-plan-b".into()),
            },
            &ctl,
        );

        assert_eq!(app.cfg.provider, "coding-plan-b");
        assert_eq!(app.selected_model.as_deref(), Some("deepseek-v4"));
    }

    #[test]
    fn live_acp_new_does_not_invent_a_local_id() {
        let (mut app, ctl, _rx) = test_app();
        let before = app.session_id.clone();
        app.run_slash("new", "fresh", &ctl);
        assert_eq!(app.session_id, before, "ACP /new waits for session/new");
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "acp-9".into(),
                notice: Some("new session · acp-9".into()),
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(app.session_id, "acp-9");
    }

    #[test]
    fn acp_session_list_opens_picker_and_prefix_loads() {
        let (mut app, ctl, _rx) = test_app();
        app.load_session = true;
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![
                    SessionListItem {
                        id: "s-old".into(),
                        title: Some("hello".into()),
                        updated_at: Some("yesterday".into()),
                    },
                    SessionListItem {
                        id: "s-other".into(),
                        title: None,
                        updated_at: None,
                    },
                ],
                prefix: None,
            }),
            &ctl,
        );
        let picker = app.picker.as_ref().expect("ACP resume picker");
        assert_eq!(picker.items[0].label, "hello");
        assert_eq!(picker.items[1].label, "s-other");

        app.picker = None;
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionList {
                sessions: vec![SessionListItem {
                    id: "s-old".into(),
                    title: None,
                    updated_at: None,
                }],
                prefix: Some("s-old".into()),
            }),
            &ctl,
        );
        assert_ne!(app.session_id, "s-old", "listing only requests the switch");
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "s-old".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(app.session_id, "s-old");
        assert!(app.picker.is_none());
    }

    #[test]
    fn agent_caps_gate_resume_to_session_list() {
        let (mut app, ctl, _rx) = test_app();
        app.handle(
            AppEvent::Ctl(CtlEvent::AgentCaps { load_session: true }),
            &ctl,
        );
        assert!(app.load_session);
        app.run_slash("resume", "", &ctl);
        assert!(
            app.picker.is_none(),
            "live ACP /resume waits for session/list"
        );
    }

    #[test]
    fn slash_permission_opens_picker_marking_current() {
        let (mut app, ctl, _rx) = test_app();
        app.run_slash("permission", "", &ctl);
        let picker = app.picker.as_ref().expect("permission picker opens");
        assert!(matches!(picker.kind, PickerKind::Permission));
        let ids: Vec<&str> = picker.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["read-only", "workspace-write", "danger-full-access"]);
        assert_eq!(picker.sel, 2, "danger-full-access is the default");
        assert!(
            picker.items[2].meta.contains("default"),
            "unreported → marked default"
        );

        app.modes.permission = Some("danger-full-access".into());
        app.run_slash("permission", "", &ctl);
        let picker = app.picker.as_ref().expect("picker reopens");
        assert_eq!(picker.sel, 2, "selection lands on the reported preset");
        assert!(picker.items[2].meta.contains("current"));
    }

    /// The picker's meanings and markers follow the interface language; the
    /// preset ids never do.
    #[test]
    fn permission_picker_rows_speak_the_interface_language() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::Zh;
        app.run_slash("permission", "", &ctl);
        let picker = app.picker.as_ref().expect("permission picker opens");
        assert_eq!(picker.sel, 2, "danger-full-access is the default");
        assert!(
            picker.items[0].meta.contains("只读"),
            "{}",
            picker.items[0].meta
        );
        assert!(
            picker.items[2].meta.contains("完全文件访问") && picker.items[2].meta.contains("默认"),
            "zh meaning + marker: {}",
            picker.items[2].meta
        );
        assert_eq!(
            picker
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["read-only", "workspace-write", "danger-full-access"],
            "ids stay the ids /permission takes"
        );
        assert_eq!(
            permission_desc(Locale::Zh, "danger-full-access"),
            Some("完全文件访问 · 关闭审批 —— 仅限信任目录")
        );
        assert_eq!(
            permission_desc(Locale::En, "read-only"),
            Some("read only — no file writes")
        );
        // A preset outside the stock table carries no invented meaning.
        assert_eq!(permission_desc(Locale::Zh, "custom-preset"), None);

        app.modes.permission = Some("read-only".into());
        app.run_slash("permission", "", &ctl);
        let picker = app.picker.as_ref().expect("picker reopens");
        assert!(
            picker.items[0].meta.contains("当前"),
            "{}",
            picker.items[0].meta
        );
    }

    /// The slash menu's argument hints are chrome, so they translate too.
    #[test]
    fn slash_argument_hints_follow_the_interface_language() {
        let (mut app, _ctl, _rx) = test_app();
        app.input.set("/effort ".into());
        let en: Vec<String> = app.slash_matches().into_iter().map(|e| e.desc).collect();
        assert!(en[0].contains("disable extended reasoning"), "{en:?}");

        app.locale = Locale::Zh;
        app.input.set("/effort ".into());
        let zh: Vec<String> = app.slash_matches().into_iter().map(|e| e.desc).collect();
        assert_eq!(zh, ["关闭扩展推理", "高推理强度", "最高推理强度"]);

        app.input.set("/theme ".into());
        let zh: Vec<String> = app.slash_matches().into_iter().map(|e| e.desc).collect();
        assert_eq!(zh[0], "深色外观");
        assert_eq!(zh[1], "浅色外观");
        assert!(zh[2..].iter().all(|desc| desc == "主题包"), "{zh:?}");
    }

    #[test]
    fn permission_aliases_normalize() {
        assert_eq!(normalize_permission("full"), Some("danger-full-access"));
        assert_eq!(normalize_permission("YOLO"), Some("danger-full-access"));
        assert_eq!(normalize_permission(" ws "), Some("workspace-write"));
        assert_eq!(normalize_permission("read-only"), Some("read-only"));
        assert_eq!(normalize_permission("RO"), Some("read-only"));
        assert_eq!(permission_label("read-only"), "Read Only");
        assert_eq!(permission_label("workspace-write"), "Workspace Write");
        assert_eq!(permission_label("danger-full-access"), "Full access");
    }

    #[test]
    fn image_media_type_maps_extensions() {
        assert_eq!(media_type_for("a.png"), Some("image/png"));
        assert_eq!(media_type_for("a.JPEG"), Some("image/jpeg"));
        assert_eq!(media_type_for("/tmp/x.webp"), Some("image/webp"));
        assert_eq!(media_type_for("x.gif"), Some("image/gif"));
        assert_eq!(media_type_for("notes.txt"), None);
    }

    #[test]
    fn slash_permission_alias_round_trips_the_durable_event() {
        let (mut app, ctl, _rx) = test_app();
        app.run_slash("permission", "full", &ctl);
        // The driver echoes the permission facts; folding them flips the chip.
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::PermissionPreset {
                session: app.session_id.clone(),
                preset: "danger-full-access".into(),
            }),
            &ctl,
        );
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::ApprovalPolicy {
                session: app.session_id.clone(),
                policy: "never".into(),
            }),
            &ctl,
        );
        assert_eq!(app.modes.permission.as_deref(), Some("danger-full-access"));
        assert_eq!(app.modes.approval.as_deref(), Some("never"));
    }

    #[test]
    fn shift_tab_cycles_between_the_stock_presets() {
        let (mut app, ctl, _rx) = test_app();
        assert_eq!(
            app.current_permission(),
            "danger-full-access",
            "assumed default"
        );

        // shift+tab sends the preset; the ack event folds the mode.
        for expected in ["read-only", "workspace-write", "danger-full-access"] {
            app.cycle_permission(&ctl);
            app.handle(
                AppEvent::Ui(crate::events::UiEvent::PermissionPreset {
                    session: app.session_id.clone(),
                    preset: expected.into(),
                }),
                &ctl,
            );
            assert_eq!(app.current_permission(), expected);
        }
    }

    #[test]
    fn permission_facts_fold_the_chip_without_touching_the_timeline() {
        let (mut app, ctl, _rx) = test_app();
        let cells_before = app.transcript.cells.len();
        for ui in [
            crate::events::UiEvent::SandboxMode {
                session: app.session_id.clone(),
                mode: "read-only".into(),
            },
            crate::events::UiEvent::ApprovalPolicy {
                session: app.session_id.clone(),
                policy: "ask".into(),
            },
            crate::events::UiEvent::PermissionPreset {
                session: app.session_id.clone(),
                preset: "read-only".into(),
            },
        ] {
            app.handle(AppEvent::Ui(ui), &ctl);
        }
        assert_eq!(app.modes.sandbox.as_deref(), Some("read-only"));
        assert_eq!(app.modes.approval.as_deref(), Some("ask"));
        assert_eq!(app.modes.permission.as_deref(), Some("read-only"));
        assert_eq!(
            app.transcript.cells.len(),
            cells_before,
            "a switch is confirmed by the meta row's chips, not by timeline lines"
        );
    }

    #[test]
    fn a_switch_borrows_the_tip_only_while_the_chips_are_hidden() {
        let (mut app, ctl, _rx) = test_app();
        // A bound session shows the permission chip, so the switch is silent.
        app.set_permission("read-only".into(), &ctl);
        assert!(app.tip.is_none());
        // A keyless boot has no session to chip: the staged switch keeps the
        // tip line, which is the only place left to say it.
        app.session_bound = false;
        app.set_permission("workspace-write".into(), &ctl);
        assert!(app.tip.is_some());
    }

    #[test]
    fn staged_images_live_as_inline_tokens_and_esc_clears_them() {
        let (mut app, ctl, _rx) = test_app();
        assert!(app.pending_images.is_empty());
        app.stage_image(
            "clipboard.png".into(),
            "clipboard".into(),
            "image/png".into(),
            vec![0u8; 8],
            String::new(),
        );
        app.stage_image(
            "shot-2.png".into(),
            "clipboard".into(),
            "image/png".into(),
            vec![1u8; 8],
            String::new(),
        );
        assert_eq!(
            app.pending_images.len(),
            2,
            "images stage instead of sending"
        );
        assert!(app.input.buf().contains("[image 1]") && app.input.buf().contains("[image 2]"));
        app.handle_esc(&ctl);
        assert!(app.input.is_empty(), "esc clears the draft");
        assert!(app.pending_images.is_empty(), "chips go with the draft");
    }

    #[test]
    fn backspace_on_a_chip_cuts_the_whole_token() {
        let (mut app, ctl, _rx) = test_app();
        app.stage_image(
            "a.png".into(),
            "p".into(),
            "image/png".into(),
            vec![0u8; 4],
            String::new(),
        );
        app.stage_image(
            "b.png".into(),
            "p".into(),
            "image/png".into(),
            vec![1u8; 4],
            String::new(),
        );
        // Cursor sits right after "[image 2]" — one backspace eats the
        // whole token and un-stages that image only.
        app.handle_key(
            crossterm::event::KeyEvent::new(
                KeyCode::Backspace,
                crossterm::event::KeyModifiers::NONE,
            ),
            &ctl,
        );
        assert_eq!(
            app.pending_images.len(),
            1,
            "backspace pops the chip under the cursor"
        );
        assert_eq!(app.pending_images.get(0).unwrap().name, "a.png");
        assert!(app.input.buf().contains("[image 1]"));
        assert!(!app.input.buf().contains("[image 2]"));
    }

    #[test]
    fn editing_a_token_away_unstages_its_image() {
        let (mut app, ctl, _rx) = test_app();
        app.stage_image(
            "a.png".into(),
            "p".into(),
            "image/png".into(),
            vec![0u8; 4],
            String::new(),
        );
        // Simulate a kill that leaves a broken token, then any key event.
        app.input.set("[image 1".into());
        app.handle_key(
            crossterm::event::KeyEvent::new(
                KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &ctl,
        );
        assert!(
            app.pending_images.is_empty(),
            "broken token reconciles the tray"
        );
    }

    #[test]
    fn esc_clears_draft_when_idle() {
        let (mut app, ctl, _rx) = test_app();
        app.input.set("hello".into());
        app.handle_esc(&ctl);
        assert!(app.input.is_empty(), "single esc clears the draft");
    }

    #[test]
    fn cancel_requested_stops_in_flight_tools() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.transcript.apply(crate::events::UiEvent::ToolCall {
            session: app.session_id.clone(),
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"grep"}"#.into(),
        });
        app.handle(AppEvent::Ctl(CtlEvent::CancelRequested), &ctl);
        assert_eq!(app.state_note, "cancelling");
        assert!(
            matches!(app.state, RunState::Running),
            "prompt has not unwound yet"
        );
        match &app.transcript.cells.last().unwrap().kind {
            crate::transcript::CellKind::Tool { ok, error, .. } => {
                assert_eq!(*ok, Some(false));
                assert_eq!(error.as_deref(), Some("cancelled"));
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn send_now_while_running_is_a_steer_not_a_cancelled_queue_item() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.input.set("change course".into());

        app.send_now(&ctl);

        assert!(matches!(app.state, RunState::Running));
        assert_eq!(app.queued, 0);
        assert_ne!(app.state_note, "cancelling");
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User { text, queued: false })
                if text == "change course"
        ));
    }

    #[test]
    fn rejected_send_now_becomes_visible_fifo_without_stopping_the_active_turn() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.input.set("change course".into());

        app.send_now(&ctl);
        let message_id = *app
            .pending_steer_cells
            .keys()
            .next()
            .expect("tracked steer command");
        app.handle(
            AppEvent::Ctl(CtlEvent::SteerSettled {
                message_id,
                deferred: true,
            }),
            &ctl,
        );

        assert!(matches!(app.state, RunState::Running));
        assert_eq!(app.queued, 1);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User {
                text,
                queued: true,
            }) if text == "change course"
        ));
    }

    #[test]
    fn send_now_with_an_image_keeps_the_active_turn_running() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.stage_image(
            "shot.png".into(),
            "clipboard".into(),
            "image/png".into(),
            vec![1, 2, 3],
            String::new(),
        );

        app.send_now(&ctl);

        assert!(matches!(app.state, RunState::Running));
        assert_eq!(app.queued, 0);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::Image { queued: false, .. })
        ));
    }

    #[test]
    fn first_prompt_after_agent_ready_is_not_marked_queued() {
        let (mut app, ctl, _rx) = test_app();
        app.handle(
            AppEvent::Ctl(CtlEvent::Starting {
                runtime: "dsh-acp".into(),
            }),
            &ctl,
        );
        app.handle(
            AppEvent::Ctl(CtlEvent::Ready {
                server: "dsh-acp".into(),
            }),
            &ctl,
        );

        app.send_agent_text("first".into(), &ctl);

        assert_eq!(app.queued, 0);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User { queued: false, .. })
        ));
    }

    #[test]
    fn startup_lifecycle_updates_state_without_adding_transcript_rows() {
        let (mut app, ctl, _rx) = test_app();
        let cells_before = app.transcript.cells.len();

        app.handle(
            AppEvent::Ctl(CtlEvent::Starting {
                runtime: "/usr/local/bin/dsh-acp".into(),
            }),
            &ctl,
        );
        assert!(matches!(app.state, RunState::Starting));
        assert_eq!(app.transcript.cells.len(), cells_before);

        app.handle(
            AppEvent::Ctl(CtlEvent::Ready {
                server: "dsh-acp".into(),
            }),
            &ctl,
        );
        assert!(matches!(app.state, RunState::Idle));
        assert_eq!(app.server_info.as_deref(), Some("dsh-acp"));
        assert_eq!(app.transcript.cells.len(), cells_before);
    }

    #[test]
    fn first_prompt_during_runtime_start_is_active_and_only_the_second_queues() {
        let (mut app, ctl, _rx) = test_app();
        app.handle(
            AppEvent::Ctl(CtlEvent::Starting {
                runtime: "dsh-acp".into(),
            }),
            &ctl,
        );

        app.send_agent_text("first".into(), &ctl);
        app.send_agent_text("second".into(), &ctl);

        assert_eq!(app.queued, 1);
        let users = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::User { text, queued } => {
                    Some((text.as_str(), *queued))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(users, [("first", false), ("second", true)]);
    }

    #[test]
    fn terminal_lifecycle_events_release_a_pending_first_prompt() {
        let events = vec![
            AppEvent::RuntimeExited(None),
            AppEvent::Ctl(CtlEvent::Error("failed".into())),
            AppEvent::Ctl(CtlEvent::Interrupted),
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
        ];

        for event in events {
            let (mut app, ctl, _rx) = test_app();
            app.send_agent_text("first".into(), &ctl);
            app.handle(event, &ctl);
            app.send_agent_text("retry".into(), &ctl);

            assert_eq!(app.queued, 0);
            assert!(matches!(
                app.transcript.cells.last().map(|cell| &cell.kind),
                Some(crate::transcript::CellKind::User {
                    text,
                    queued: false
                }) if text == "retry"
            ));
        }
    }

    #[test]
    fn bracketed_paste_wrapped_csi_u_ctrl_c_still_quits() {
        let (mut app, ctl, _rx) = test_app();

        for _ in 0..2 {
            app.handle(
                AppEvent::Term(Event::Paste("\u{1b}[99;5u".to_string())),
                &ctl,
            );
        }

        assert!(app.quit, "two Ctrl+C presses should quit from idle");
        assert!(
            app.input.is_empty(),
            "the CSI-u bytes must never enter the composer"
        );
    }

    #[test]
    fn ordinary_and_mixed_paste_payloads_are_not_treated_as_keys() {
        let (mut app, ctl, _rx) = test_app();

        app.handle(
            AppEvent::Term(Event::Paste("hello\nworld".to_string())),
            &ctl,
        );
        app.handle(
            AppEvent::Term(Event::Paste(" literal \u{1b}[99;5u".to_string())),
            &ctl,
        );

        // The composer is multi-line, so the pasted break survives (a CRLF or
        // a bare CR from iTerm2-style terminals lands as a plain `\n`).
        assert_eq!(
            app.input.buf(),
            "hello\nworld literal \u{1b}[99;5u",
            "a paste keeps its line structure"
        );
        assert!(!app.quit);
    }

    #[test]
    fn a_pasted_crlf_lands_as_one_newline_and_never_sends() {
        let (mut app, ctl, _rx) = test_app();

        app.handle(
            AppEvent::Term(Event::Paste("fn main() {\r\n    run();\r\n}\r".to_string())),
            &ctl,
        );

        assert_eq!(
            app.input.buf(),
            "fn main() {\n    run();\n}\n",
            "CRLF and a trailing CR normalize to `\\n`"
        );
        assert!(
            app.transcript.cells.is_empty(),
            "a multi-line paste must not submit the draft"
        );
    }

    /// ↑ recalls the input history from a *non-empty* draft once the caret is
    /// already on the first visual row, and ↓ past the newest entry puts the
    /// stashed draft back.
    #[test]
    fn up_at_the_first_row_recalls_history_and_down_restores_the_draft() {
        let (mut app, ctl, _rx) = test_app();
        app.input.history.push("first prompt".into());
        app.input.history.push("second prompt".into());

        app.input.set("half-typed".into());
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "second prompt");
        assert_eq!(app.input.hist_pos, Some(1));

        // While browsing, ↑/↓ keep walking the history (no caret motion).
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "first prompt");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "half-typed", "the draft was stashed");
        assert_eq!(app.input.hist_pos, None);

        // A multi-line draft moves the caret first; only the top row recalls.
        app.input.set("one\ntwo".into());
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "one\ntwo", "the caret stayed in the draft");
        assert_eq!(app.input.hist_pos, None, "no recall while the caret moves");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "second prompt");
    }

    #[test]
    fn resetting_the_session_releases_a_pending_first_prompt() {
        let (mut app, ctl, _rx) = test_app();
        app.send_agent_text("old session".into(), &ctl);

        app.reset_session_ui();
        app.send_agent_text("new session".into(), &ctl);

        assert_eq!(app.queued, 0);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User {
                text,
                queued: false
            }) if text == "new session"
        ));
    }

    #[test]
    fn resetting_the_session_discards_unsettled_steer_bookkeeping() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.input.set("old steer".into());
        app.send_now(&ctl);
        let message_id = *app
            .pending_steer_cells
            .keys()
            .next()
            .expect("steer is awaiting settlement");

        app.reset_session_ui();
        app.handle(
            AppEvent::Ctl(CtlEvent::SteerSettled {
                message_id,
                deferred: true,
            }),
            &ctl,
        );

        assert!(app.pending_steer_cells.is_empty());
        assert_eq!(app.queued, 0, "late settlement cannot taint a new session");
        assert!(app.prompt_queue.is_empty());
    }

    #[test]
    fn runtime_exit_discards_delivery_state_owned_by_the_dead_actor() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.send_agent_text("queued followup".into(), &ctl);
        app.input.set("unsettled steer".into());
        app.send_now(&ctl);
        assert_eq!(app.queued, 1);
        assert_eq!(app.pending_steer_cells.len(), 1);

        app.handle(AppEvent::RuntimeExited(Some(1)), &ctl);

        assert_eq!(app.queued, 0);
        assert!(app.prompt_queue.is_empty());
        assert!(app.pending_steer_cells.is_empty());
        assert!(
            app.transcript.cells.iter().all(|cell| !matches!(
                &cell.kind,
                crate::transcript::CellKind::User { queued: true, .. }
            )),
            "a dead queue's echoes stop claiming they are queued"
        );
    }

    #[test]
    fn interrupted_keeps_client_followups_queued() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.state_note = "cancelling".into();
        app.send_agent_text("first followup".into(), &ctl);
        app.handle(AppEvent::Ctl(CtlEvent::Interrupted), &ctl);
        assert!(matches!(app.state, RunState::Idle));
        assert_eq!(app.queued, 1);
        assert!(app.state_note.is_empty());

        app.send_agent_text("second followup".into(), &ctl);
        assert_eq!(
            app.queued, 2,
            "new input joins the surviving FIFO while the actor advances it"
        );
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User { queued: true, .. })
        ));
    }

    #[test]
    fn interrupted_turn_renders_one_specific_terminal_notice() {
        let (mut app, ctl, _rx) = test_app();
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::TurnEnd {
                session: app.session_id.clone(),
                kind: "interrupted".into(),
            }),
            &ctl,
        );
        app.handle(AppEvent::Ctl(CtlEvent::Interrupted), &ctl);

        let notices = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } if text.contains("interrupt") => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices, ["interrupted — turn cancelled"]);
    }

    #[test]
    fn staged_input_joins_a_surviving_fifo_after_interrupt() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.send_agent_text("first followup".into(), &ctl);
        app.handle(AppEvent::Ctl(CtlEvent::Interrupted), &ctl);

        app.send_staged(
            vec![StagedBlock::Image(crate::attachments::Attachment {
                id: crate::attachments::KITTY_ID_BASE + 1,
                token: "[image 1]".into(),
                name: "shot.png".into(),
                path: "clipboard".into(),
                media_type: "image/png".into(),
                data: std::sync::Arc::from([1_u8, 2, 3]),
            })],
            &ctl,
        );

        assert_eq!(app.queued, 2);
        assert_eq!(app.prompt_queue.len(), 2);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::Image { queued: true, .. })
        ));
    }

    #[test]
    fn send_now_can_steer_while_a_surviving_fifo_is_advancing() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.send_agent_text("ordinary followup".into(), &ctl);
        app.handle(AppEvent::Ctl(CtlEvent::Interrupted), &ctl);
        app.input.set("urgent correction".into());

        app.send_now(&ctl);

        assert_eq!(app.queued, 1);
        assert_eq!(app.pending_steer_cells.len(), 1);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User {
                text,
                queued: false,
            }) if text == "urgent correction"
        ));
    }

    /// `⌥↑` lists the queued follow-ups; entering one loads it into the
    /// composer, and enter saves the edit back into the same FIFO slot.
    #[test]
    fn alt_up_edits_a_queued_prompt_in_place() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.state = RunState::Running;
        app.send_agent_text("first followup".into(), &ctl);
        app.send_agent_text("second followup".into(), &ctl);
        let kept_id = app.prompt_queue[1].id;

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &ctl);
        let picker = app.picker.as_ref().expect("queue picker");
        assert!(matches!(picker.kind, PickerKind::Queue));
        assert_eq!(picker.items.len(), 2);
        assert_eq!(picker.items[1].label, "second followup");
        assert!(
            picker.items[1].meta.contains("#2"),
            "{}",
            picker.items[1].meta
        );

        // ↓ then enter opens the second item for editing.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        assert!(app.picker.is_none(), "the picker closed on enter");
        assert_eq!(app.input.buf(), "second followup");
        assert!(app.queue_edit.is_some());
        assert_eq!(app.prompt_queue.len(), 2, "the item keeps its slot");

        // Enter saves: the item is replaced in place and nothing was sent.
        app.input.set("second followup, revised".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        assert!(app.queue_edit.is_none());
        assert!(app.input.is_empty());
        assert_eq!(app.queued, 2);
        assert_eq!(app.prompt_queue[1].id, kept_id, "the slot is unchanged");
        assert!(
            matches!(&app.prompt_queue[1].blocks[..], [StagedBlock::Text(text)] if text == "second followup, revised"),
            "the edit replaced the blocks"
        );
        assert!(
            commands.try_recv().is_err(),
            "editing a queued prompt never sends anything"
        );
    }

    /// Esc cancels an edit and leaves the item alone; ctrl+d arms and then
    /// deletes it. Neither path sends anything.
    #[test]
    fn queue_edit_cancels_or_deletes_without_sending() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.state = RunState::Running;
        app.send_agent_text("keep me".into(), &ctl);
        let id = app.prompt_queue[0].id;
        app.begin_queue_edit(&id.to_string(), &ctl);
        app.input.set("changed my mind".into());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.queue_edit.is_none(), "esc cancelled the edit");
        assert!(app.input.is_empty(), "the draft is dropped");
        assert!(
            matches!(&app.prompt_queue[0].blocks[..], [StagedBlock::Text(text)] if text == "keep me"),
            "the queued item is untouched"
        );
        assert_eq!(app.queued, 1);

        // ctrl+d asks once, then deletes.
        app.begin_queue_edit(&id.to_string(), &ctl);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert!(app.queue_edit.is_some(), "the first press only arms");
        assert_eq!(app.prompt_queue.len(), 1);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert!(app.queue_edit.is_none());
        assert!(app.prompt_queue.is_empty(), "the second press deleted it");
        assert_eq!(app.queued, 0);
        assert!(commands.try_recv().is_err(), "nothing was sent");
    }

    /// Deleting a queued prompt takes its echo out of the timeline with it: the
    /// bubble never left the client, so leaving it behind as `queued` (or as a
    /// plain, delivered-looking prompt) would both be wrong. The prompts behind
    /// it keep their own bubbles, and the queue is remapped onto their new
    /// indices.
    #[test]
    fn deleting_a_queued_prompt_drops_its_echo_and_remaps_the_rest() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        let queued_echoes = |app: &App| -> Vec<String> {
            app.transcript
                .cells
                .iter()
                .filter_map(|cell| match &cell.kind {
                    crate::transcript::CellKind::User { text, queued: true } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        app.state = RunState::Running;
        app.send_agent_text("delete me".into(), &ctl);
        app.send_agent_text("keep me".into(), &ctl);
        assert_eq!(queued_echoes(&app), ["delete me", "keep me"]);

        let id = app.prompt_queue[0].id;
        app.begin_queue_edit(&id.to_string(), &ctl);
        for _ in 0..2 {
            app.handle_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &ctl,
            );
        }

        assert_eq!(app.queued, 1, "one item left in the queue");
        assert_eq!(app.prompt_queue.len(), 1);
        assert_eq!(
            queued_echoes(&app),
            ["keep me"],
            "the deleted echo left the timeline, the survivor stayed"
        );
        assert_eq!(app.transcript.cells.len(), 1, "only its own bubble is left");
        for cell in app.prompt_queue[0].cells.clone() {
            assert!(
                matches!(
                    &app.transcript.cells[cell].kind,
                    crate::transcript::CellKind::User { text, queued: true } if text == "keep me"
                ),
                "the queue still points at its own bubble (cell {cell})"
            );
        }
        assert!(commands.try_recv().is_err(), "nothing was sent");
    }

    /// The queue list deletes rows itself, without the editor round-trip:
    /// `ctrl+d` arms the highlighted row (the row says so), the second press
    /// drops it, and the dialog stays open on the row that took its place — so
    /// several queued prompts can go without re-opening the selector.
    #[test]
    fn the_queue_picker_deletes_the_highlighted_row_in_place() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        let queued_echoes = |app: &App| -> Vec<String> {
            app.transcript
                .cells
                .iter()
                .filter_map(|cell| match &cell.kind {
                    crate::transcript::CellKind::User { text, queued: true } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        let ctrl_d = || KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.state = RunState::Running;
        app.send_agent_text("drop me".into(), &ctl);
        app.send_agent_text("keep me".into(), &ctl);
        app.send_agent_text("keep me too".into(), &ctl);

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), &ctl);
        let head = app.picker.as_ref().expect("queue picker").items[0]
            .id
            .clone();
        app.handle_key(ctrl_d(), &ctl);
        assert_eq!(
            app.queue_delete_armed.as_deref(),
            Some(head.as_str()),
            "the first press arms the highlighted row"
        );
        assert!(
            app.picker.as_ref().unwrap().items[0]
                .meta
                .contains("ctrl+d"),
            "the armed row says what the next press does"
        );
        assert_eq!(app.prompt_queue.len(), 3, "the first press only asks");

        // ↓ walks off the row and takes the arming with it: `ctrl+d` names the
        // row under the highlight, never the one the user left behind.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
        assert!(
            app.queue_delete_armed.is_none(),
            "moving the highlight disarms"
        );
        app.handle_key(ctrl_d(), &ctl);
        assert_eq!(app.prompt_queue.len(), 3, "arming again deletes nothing");

        // Second press: the row goes, its echo leaves the timeline with it, and
        // the dialog stays open on the row that moved up into its place.
        app.handle_key(ctrl_d(), &ctl);
        assert_eq!(app.queued, 2);
        assert_eq!(queued_echoes(&app), ["drop me", "keep me too"]);
        let picker = app.picker.as_ref().expect("the list stays open");
        assert_eq!(picker.sel, 1, "the highlight follows the survivor");
        assert_eq!(picker.items[1].label, "keep me too");
        assert!(app.queue_delete_armed.is_none(), "the arming is spent");

        // Two more rounds empty the queue; the last delete closes the dialog.
        for _ in 0..2 {
            app.handle_key(ctrl_d(), &ctl);
            app.handle_key(ctrl_d(), &ctl);
        }
        assert!(app.prompt_queue.is_empty());
        assert_eq!(app.queued, 0);
        assert!(queued_echoes(&app).is_empty(), "no echo outlives its item");
        assert!(app.picker.is_none(), "an empty list is not a dialog");
        assert!(
            commands.try_recv().is_err(),
            "deleting never sends anything"
        );
    }

    /// While an item is loaded for editing the FIFO holds: a turn end must not
    /// ship the head out from under the editor.
    #[test]
    fn the_queue_pauses_dispatch_while_an_item_is_edited() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.state = RunState::Running;
        app.send_agent_text("head".into(), &ctl);
        app.send_agent_text("tail".into(), &ctl);
        let head = app.prompt_queue[0].id;
        app.begin_queue_edit(&head.to_string(), &ctl);

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );

        assert_eq!(app.queued, 2, "the queue held");
        assert!(commands.try_recv().is_err(), "nothing went out");
        assert!(app.state_note.contains("paused"), "{}", app.state_note);

        // Closing the editor lets the next idle status ship the head.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );
        assert_eq!(app.queued, 1);
        assert!(matches!(
            commands.try_recv(),
            Ok(Cmd::Prompt { text, .. }) if text == "head"
        ));
    }

    /// An empty draft's enter promotes the FIFO head: a steer while the turn
    /// runs, and it waits for the idle status like any other send otherwise.
    #[test]
    fn empty_enter_sends_the_queue_head_now() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.state = RunState::Running;
        app.send_agent_text("head".into(), &ctl);
        app.send_agent_text("tail".into(), &ctl);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);

        assert!(matches!(
            commands.try_recv(),
            Ok(Cmd::Steer { text, .. }) if text == "head"
        ));
        assert_eq!(app.queued, 1, "the tail stays queued");
        assert_eq!(app.prompt_queue.len(), 1);
        assert!(
            matches!(&app.prompt_queue[0].blocks[..], [StagedBlock::Text(text)] if text == "tail")
        );
        assert_eq!(
            app.pending_steer_cells.len(),
            1,
            "the steer awaits settlement"
        );
    }

    /// The idle status is what hands the FIFO head to the driver: it is not
    /// merely kept (that was the driver-channel queue's job), it goes out —
    /// exactly one item, whose echo loses the queued tint.
    #[test]
    fn an_idle_status_dispatches_the_client_owned_fifo_head() {
        let (mut app, _demo, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.state = RunState::Running;
        app.send_agent_text("followup".into(), &ctl);
        assert_eq!(app.queued, 1);
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User { queued: true, .. })
        ));

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );

        assert_eq!(app.queued, 0);
        assert!(app.prompt_queue.is_empty(), "the head left the queue");
        assert!(matches!(
            app.transcript.cells.last().map(|cell| &cell.kind),
            Some(crate::transcript::CellKind::User { queued: false, .. })
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(Cmd::Prompt { text, .. }) if text == "followup"
        ));
        assert!(app.prompt_pending, "the dispatched turn is armed");
    }

    #[test]
    fn an_idle_status_delivers_exactly_one_queued_prompt_group() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.send_staged(
            vec![
                StagedBlock::Text("look".into()),
                StagedBlock::Image(crate::attachments::Attachment {
                    id: crate::attachments::KITTY_ID_BASE + 1,
                    token: "[image 1]".into(),
                    name: "shot.png".into(),
                    path: "clipboard".into(),
                    media_type: "image/png".into(),
                    data: std::sync::Arc::from([1_u8, 2, 3]),
                }),
            ],
            &ctl,
        );
        app.send_agent_text("after".into(), &ctl);

        assert_eq!(app.queued, 2);

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );
        assert_eq!(app.queued, 1, "only the head went out");

        // The driver's own acceptance of a prompt never moves the queue.
        app.handle(
            AppEvent::Ctl(CtlEvent::PromptQueued {
                message_id: "dsh-test".into(),
            }),
            &ctl,
        );
        assert_eq!(app.queued, 1);
        let queued = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::User { queued, .. }
                | crate::transcript::CellKind::Image { queued, .. } => Some(*queued),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(queued, [false, false, true]);
    }

    #[test]
    fn ctrl_c_with_a_draft_clears_it_before_starting_a_fresh_double_press_to_quit() {
        let (mut app, ctl, _rx) = test_app();
        app.ctrl_c_armed = Some(CtrlCQuitChord {
            started: Instant::now(),
            presses: 1,
            required: 2,
        });
        app.input.set("unfinished draft".into());

        app.handle_ctrl_c(&ctl);
        assert!(app.input.is_empty());
        assert!(
            app.ctrl_c_armed.is_none(),
            "clearing is not the first quit press"
        );
        assert!(!app.quit);

        app.handle_ctrl_c(&ctl);
        assert!(app.ctrl_c_armed.is_some());
        assert!(!app.quit);

        app.handle_ctrl_c(&ctl);
        assert!(app.quit);
    }

    #[test]
    fn ctrl_c_while_starting_without_a_prompt_quits_after_two_empty_presses() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Starting;

        app.handle_ctrl_c(&ctl);
        assert!(!app.quit, "the first empty Ctrl+C arms the idle quit chord");

        app.handle_ctrl_c(&ctl);
        assert!(
            app.quit,
            "startup without an active turn uses the two-press chord"
        );
    }

    #[test]
    fn ctrl_c_while_running_never_interrupts_and_two_empty_presses_quit() {
        let (mut app, _demo_ctl, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_interruptible_controller();
        app.state = RunState::Running;

        app.handle_ctrl_c(&ctl);
        assert!(
            app.ctrl_c_armed.is_some(),
            "an empty Ctrl+C should arm quit even while the turn is running"
        );
        assert!(!app.quit);
        assert_eq!(app.state, RunState::Running);
        assert!(
            matches!(
                commands.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "Ctrl+C must not send Cmd::Interrupt"
        );

        app.handle_ctrl_c(&ctl);
        assert!(app.quit);
    }

    /// Skills are `/skill`'s business only: the `/` menu lists builtins and
    /// nothing else, even when the catalog holds a name no builtin claims.
    #[test]
    fn skills_stay_out_of_the_slash_menu() {
        let (mut app, _ctl, _rx) = test_app();
        app.skills = vec![
            crate::bus::SkillInfo {
                name: "commit-helper".into(),
                description: "draft a commit".into(),
                input_hint: None,
                source: None,
            },
            crate::bus::SkillInfo {
                name: "help".into(),
                description: "shadowed by builtin".into(),
                input_hint: None,
                source: None,
            },
        ];
        app.input.set("/".into());
        let menu = app.slash_matches();
        assert_eq!(
            menu.len(),
            SLASH_COMMANDS.len(),
            "the bare `/` menu is the builtin list, unmerged"
        );
        assert!(
            !menu.iter().any(|e| e.name == "commit-helper"),
            "a skill never becomes a `/` row"
        );
        // Not even as a prefix: `/commit` matches no builtin, so the menu is
        // empty and the line is left to ship as a skill prompt instead.
        app.input.set("/commit".into());
        assert!(app.slash_matches().is_empty());
        // The catalog is one command away.
        app.input.set("/skill ".into());
        assert_eq!(app.slash_matches().len(), 2, "both skills are candidates");
    }

    #[test]
    fn tab_completes_a_slash_argument_without_running_it() {
        let (mut app, _demo_ctl, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.input.set("/plan o".into());

        let menu = app.slash_matches();
        assert_eq!(
            menu.iter()
                .map(|entry| entry.usage.as_str())
                .collect::<Vec<_>>(),
            ["on", "off"]
        );

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "/plan on");
        assert!(matches!(
            commands.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn direct_plan_mode_facts_fold_once_into_client_state() {
        let (mut app, ctl, _rx) = test_app();

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::PlanMode {
                session: "dsh-test".into(),
                active: true,
            }),
            &ctl,
        );
        assert!(app.modes.plan);
        let cells_after_first = app.transcript.cells.len();

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::PlanMode {
                session: "dsh-test".into(),
                active: true,
            }),
            &ctl,
        );
        assert_eq!(
            app.transcript.cells.len(),
            cells_after_first,
            "the same config_option_update is idempotent"
        );
    }

    #[test]
    fn initial_default_plan_mode_does_not_add_an_off_notice() {
        let (mut app, ctl, _rx) = test_app();
        let cells_before = app.transcript.cells.len();

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::PlanMode {
                session: "dsh-test".into(),
                active: false,
            }),
            &ctl,
        );

        assert_eq!(app.transcript.cells.len(), cells_before);
    }

    /// The direct turn facts still drive the client lifecycle — the queue
    /// itself is only moved by the idle status now.
    #[test]
    fn direct_ui_turn_facts_update_client_lifecycle() {
        let (mut app, ctl, _rx) = test_app();
        app.state = RunState::Running;
        app.run_started = Some(Instant::now());
        app.state_note = "working".into();
        app.prompt_queue.push_back(QueuedPrompt {
            id: 1,
            blocks: vec![StagedBlock::Text("followup".into())],
            cells: vec![],
        });
        app.queued = 1;

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::TurnStart {
                session: "dsh-test".into(),
                turn: 1,
            }),
            &ctl,
        );
        assert_eq!(app.queued, 1, "a turn start never moves the queue");
        app.handle(
            AppEvent::Ctl(CtlEvent::PromptQueued {
                message_id: "dsh-test".into(),
            }),
            &ctl,
        );
        assert_eq!(app.queued, 1, "acceptance never moves the queue either");

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );
        assert_eq!(app.queued, 0, "the idle status dispatched the queued head");
        assert!(matches!(app.state, RunState::Starting));
        assert!(app.run_started.is_some(), "the dispatched turn is timed");
        assert!(app.state_note.contains("queued"), "{}", app.state_note);

        // The next turn end has an empty queue and settles back to idle.
        app.handle(
            AppEvent::Ctl(CtlEvent::PromptQueued {
                message_id: "dsh-test".into(),
            }),
            &ctl,
        );
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::SessionStatus {
                session: "dsh-test".into(),
                running: false,
            }),
            &ctl,
        );
        assert!(matches!(app.state, RunState::Idle));
        assert!(app.run_started.is_none());
        assert!(app.state_note.is_empty());
    }

    #[test]
    fn plan_message_keeps_the_slash_prompt_transport() {
        let (mut app, ctl, _rx) = test_app();
        app.skills = vec![crate::bus::SkillInfo {
            name: "plan".into(),
            description: "Enter plan mode".into(),
            input_hint: None,
            source: None,
        }];

        app.run_slash("plan", "focus on the parser", &ctl);

        assert!(matches!(app.state, RunState::Starting));
        assert!(matches!(
            &app.transcript.cells[0].kind,
            crate::transcript::CellKind::User { text, .. }
                if text == "/plan focus on the parser"
        ));
    }

    #[test]
    fn no_key_onboarding_guides_the_platform_and_login() {
        let (mut app, _ctl, _rx) = test_app();
        assert!(!app.cfg.has_credentials());

        app.push_no_key_onboarding();

        let cards: Vec<&str> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::MarkdownNotice { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert!(
            cards[0].contains("platform.deepseek.com"),
            "the platform URL guides the user: {}",
            cards[0]
        );
        assert!(
            cards[0].contains("/login sk-"),
            "how to operate: {}",
            cards[0]
        );
        assert!(
            cards[0].contains("credentials.yaml"),
            "where the key lands: {}",
            cards[0]
        );

        // A key present: the same helper still renders (used by tests only),
        // but main.rs gates the call on has_credentials — covered upstream.
    }

    #[test]
    fn login_without_argument_reports_status_never_the_value() {
        let (mut app, ctl, _rx) = test_app();

        app.run_slash("login", "", &ctl);

        let notices: Vec<&str> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|text| text.contains("usage: /login")),
            "{notices:?}"
        );
    }

    #[test]
    fn login_stores_the_key_and_rotates_the_running_driver() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        let key = "sk-abcdefgh1234567890";

        app.run_slash("login", key, &ctl);

        // The driver gets the rotation command.
        let sent: Vec<Cmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            sent.iter().any(|cmd| matches!(
                cmd,
                crate::bus::Cmd::SetApiKey { key: Some(key) } if key == "sk-abcdefgh1234567890"
            )),
            "{sent:?}"
        );

        // The key landed in the store; the confirmation is redacted.
        assert_eq!(
            crate::credentials::stored_key(&app.cfg.home, crate::credentials::API_KEY_REF,)
                .unwrap(),
            Some("sk-abcdefgh1234567890".into())
        );
        let notices: Vec<String> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|text| text.contains("sk-…7890")),
            "redacted descriptor: {notices:?}"
        );
        assert!(
            !notices
                .iter()
                .any(|text| text.contains("sk-abcdefgh1234567890")),
            "the literal key must not appear in the transcript: {notices:?}"
        );
        assert_eq!(app.cfg.key_origin, Some(crate::runtime::KeyOrigin::Stored));
        assert!(app.cfg.has_credentials());
    }

    #[test]
    fn login_replaces_a_previously_stored_key() {
        let (mut app, ctl, _rx) = test_app();
        app.run_slash("login", "sk-first0000001", &ctl);
        app.run_slash("login", "sk-second0002", &ctl);

        assert_eq!(
            crate::credentials::stored_key(&app.cfg.home, crate::credentials::API_KEY_REF,)
                .unwrap(),
            Some("sk-second0002".into())
        );
    }

    #[test]
    fn logout_removes_the_stored_key_and_clears_the_live_driver() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.run_slash("login", "sk-abcdefgh1234567890", &ctl);
        let _ = std::iter::from_fn(|| commands.try_recv().ok()).collect::<Vec<_>>();

        app.run_slash("logout", "", &ctl);

        // The stored entry is gone and the driver gets the clear command.
        assert_eq!(
            crate::credentials::stored_key(&app.cfg.home, crate::credentials::API_KEY_REF,)
                .unwrap(),
            None
        );
        let sent: Vec<Cmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            sent.iter()
                .any(|cmd| matches!(cmd, crate::bus::Cmd::SetApiKey { key: None })),
            "{sent:?}"
        );
        assert_eq!(app.cfg.api_key, None);
        assert_eq!(app.cfg.key_origin, None);
        assert!(!app.cfg.has_credentials());
    }

    #[test]
    fn logout_when_nothing_is_stored_is_a_quiet_noop() {
        let (mut app, ctl, _rx) = test_app();
        assert_eq!(app.cfg.key_origin, None);
        let cells_before = app.transcript.cells.len();

        app.run_slash("logout", "", &ctl);

        let notices: Vec<&str> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|text| text.contains("no stored key")),
            "{notices:?}"
        );
        assert_eq!(app.transcript.cells.len(), cells_before + 1);
    }

    #[test]
    fn logout_keeps_an_api_key_override_running() {
        let cfg = test_cfg();
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        // An older stored key loses to the launch override, so both exist.
        crate::credentials::store_key(
            &app.cfg.home,
            crate::credentials::API_KEY_REF,
            "sk-stored000001",
        )
        .unwrap();
        app.cfg.api_key = Some("sk-override0001".into());
        app.cfg.key_origin = Some(crate::runtime::KeyOrigin::Flag);

        app.run_slash("logout", "", &ctl);

        // The stored record is gone, but the run keeps its explicit override…
        assert_eq!(
            crate::credentials::stored_key(&app.cfg.home, crate::credentials::API_KEY_REF,)
                .unwrap(),
            None
        );
        assert_eq!(app.cfg.api_key, Some("sk-override0001".into()));
        assert_eq!(app.cfg.key_origin, Some(crate::runtime::KeyOrigin::Flag));
        // …and the driver is not disturbed.
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok())
                .collect::<Vec<Cmd>>()
                .is_empty(),
            "no SetApiKey may reach the driver for a read-only launch override"
        );
        let notices: Vec<String> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|text| text.contains("--api-key flag key keeps running")),
            "{notices:?}"
        );
    }

    /// The menu never rows a skill, but the line still reaches the host: a
    /// hand-typed `/name` ships as a prompt and the agent injects the body.
    #[test]
    fn skill_line_ships_as_prompt_not_unknown_command() {
        let (mut app, ctl, _rx) = test_app();
        app.skills = vec![crate::bus::SkillInfo {
            name: "commit-helper".into(),
            description: "draft a commit".into(),
            input_hint: None,
            source: None,
        }];
        app.input.set("/commit-helper for the last change".into());
        assert!(
            app.slash_matches().is_empty(),
            "no completer row — the skill is not a command"
        );
        app.submit(&ctl);
        assert!(
            matches!(app.state, RunState::Starting),
            "skill line starts a turn"
        );
        assert!(app.input.is_empty());
    }

    /// `/skill` with no argument renders the catalog the agent discovered,
    /// source file and all; its empty state names the directory instead of
    /// reading as "this build has no skills".
    #[test]
    fn bare_skill_command_lists_the_catalog_or_the_root() {
        let (mut app, _ctl, _rx) = test_app();
        let (ctl, _commands) = crate::controller::test_controller();
        app.skills = vec![
            crate::bus::SkillInfo {
                name: "deploy".into(),
                description: "ship it".into(),
                input_hint: Some("<env>".into()),
                source: Some("/w/.abylab/skills/deploy/SKILL.md".into()),
            },
            crate::bus::SkillInfo {
                name: "triage".into(),
                description: "sort the inbox".into(),
                input_hint: None,
                source: None,
            },
        ];
        app.run_slash("skill", "", &ctl);
        let overlay = app.view_overlay.as_ref().expect("/skill opens a card");
        assert!(
            !SLASH_COMMANDS
                .iter()
                .any(|command| command.name == "skills"),
            "the listing is `/skill`'s empty-argument surface, not its own command"
        );
        assert_eq!(overlay.title, "Skills");
        let frame = crate::ui::dump_frame(&mut app, 100, 40);
        assert!(frame.contains("/deploy <env>"), "usage missing:\n{frame}");
        assert!(frame.contains("ship it"), "description missing:\n{frame}");
        assert!(
            frame.contains("deploy/SKILL.md"),
            "the source file is shown:\n{frame}"
        );
        assert!(
            frame.contains("/triage"),
            "a hintless skill shows bare:\n{frame}"
        );

        let (mut empty, _empty_ctl, _rx) = test_app();
        let (empty_ctl, _commands) = crate::controller::test_controller();
        empty.run_slash("skill", "", &empty_ctl);
        let frame = crate::ui::dump_frame(&mut empty, 100, 40);
        assert!(frame.contains("no skills in this workspace"), "{frame}");
        assert!(
            frame.contains(&format!("{}/.agents/skills", empty.cfg.workspace)),
            "the one discovery root is named:\n{frame}"
        );
        assert!(
            !frame.contains(&empty.cfg.home),
            "the home directory is not a skills root any more:\n{frame}"
        );
    }

    /// `/skill <name> [args]` ships the plain `/<name> [args]` line the agent
    /// expands — including for a name a builtin shadows, which is the whole
    /// point of the two-word form.
    #[test]
    fn skill_invocation_resolves_the_name_before_shipping() {
        let (mut app, _ctl, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.skills = vec![
            crate::bus::SkillInfo {
                name: "deploy".into(),
                description: "ship it".into(),
                input_hint: None,
                source: None,
            },
            crate::bus::SkillInfo {
                name: "plan".into(),
                description: "shadowed by the builtin".into(),
                input_hint: None,
                source: None,
            },
        ];
        app.run_slash("skill", "deploy prod", &ctl);
        assert!(
            matches!(app.state, RunState::Starting),
            "the skill line leaves as a prompt"
        );
        let sent: Vec<Cmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            matches!(&sent[..], [Cmd::Prompt { text, .. }] if text == "/deploy prod"),
            "{sent:?}"
        );
        assert!(
            app.transcript.cells.iter().any(|cell| matches!(
                &cell.kind,
                crate::transcript::CellKind::User { text, .. } if text == "/deploy prod"
            )),
            "the shipped line is what the transcript shows"
        );

        // `/plan on` would be swallowed by the builtin; the two-word form is
        // how a skill of that name still runs. (The first send marked the app
        // busy, so settle it back to idle first.)
        app.state = RunState::Idle;
        app.prompt_pending = false;
        app.run_slash("skill", "plan on", &ctl);
        let sent: Vec<Cmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            matches!(&sent[..], [Cmd::Prompt { text, .. }] if text == "/plan on"),
            "{sent:?}"
        );

        // No argument at all is the listing, not an error.
        let (mut app, _ctl, _rx) = test_app();
        let (ctl, _commands) = crate::controller::test_controller();
        app.run_slash("skill", "", &ctl);
        assert!(app.view_overlay.is_some(), "an empty name lists instead");
    }

    /// An unknown name is caught client-side: shipping `/nope` would just ask
    /// the model about a slash command.
    #[test]
    fn unknown_skill_names_warn_without_shipping() {
        let (mut app, _ctl, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.skills = vec![crate::bus::SkillInfo {
            name: "deploy".into(),
            description: "ship it".into(),
            input_hint: None,
            source: None,
        }];
        app.run_slash("skill", "nope now", &ctl);
        assert!(commands.try_recv().is_err(), "nothing is sent");
        assert!(matches!(app.state, RunState::Idle));
        assert!(
            app.transcript.cells.iter().any(|cell| matches!(
                &cell.kind,
                crate::transcript::CellKind::Notice { text, .. }
                    if text.contains("unknown skill nope")
            )),
            "the miss is named"
        );
    }

    /// `/skill ` lists the catalog as argument candidates, filtered by what is
    /// typed, with the hint and the description on the row.
    #[test]
    fn slash_skill_offers_the_catalog_as_candidates() {
        let (mut app, _ctl, _rx) = test_app();
        let (ctl, commands) = crate::controller::test_controller();
        app.skills = vec![
            crate::bus::SkillInfo {
                name: "audit".into(),
                description: "审核研究报告".into(),
                input_hint: Some("jp:{code}".into()),
                source: None,
            },
            crate::bus::SkillInfo {
                name: "triage".into(),
                description: "sort the inbox".into(),
                input_hint: None,
                source: None,
            },
        ];

        app.input.set("/skill ".into());
        let menu = app.slash_matches();
        assert_eq!(menu.len(), 2, "every skill is a candidate");
        assert_eq!(menu[0].name, "skill", "the row belongs to the builtin");
        assert_eq!(menu[0].usage, "audit jp:{code}", "the hint rides the row");
        assert_eq!(menu[0].desc, "审核研究报告");
        assert_eq!(menu[0].completion.as_deref(), Some("/skill audit"));
        // The band above the composer is what the user asked to see. (The CJK
        // description renders glyph-spaced and clipped, so the ASCII rows are
        // what a frame assertion can rely on.)
        let frame = crate::ui::dump_frame(&mut app, 100, 40);
        assert!(frame.contains("audit jp:{code}"), "{frame}");
        assert!(frame.contains("sort the inbox"), "{frame}");

        app.input.set("/skill tr".into());
        let menu = app.slash_matches();
        assert_eq!(menu.len(), 1, "the typed prefix filters");
        assert_eq!(menu[0].usage, "triage");

        // A skill that takes no arguments runs on the pick.
        app.accept_slash(&menu[0].clone(), &ctl);
        let sent: Vec<Cmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            matches!(&sent[..], [Cmd::Prompt { text, .. }] if text == "/triage"),
            "{sent:?}"
        );

        // One that declares a placeholder completes and waits instead.
        app.prompt_pending = false;
        app.state = RunState::Idle;
        app.input.set("/skill au".into());
        let menu = app.slash_matches();
        app.accept_slash(&menu[0].clone(), &ctl);
        assert_eq!(
            app.input.buf(),
            "/skill audit ",
            "the argument is the user's to type"
        );
        assert!(commands.try_recv().is_err(), "nothing was sent yet");
    }

    /// `/login` resolves in the TUI even when the agent advertises a skill of
    /// that name — skills add no `/` row, so there is no race to lose.
    #[test]
    fn login_is_a_tui_builtin_and_shadows_the_agent_skill() {
        let (mut app, ctl, _rx) = test_app();
        app.skills = vec![crate::bus::SkillInfo {
            name: "login".into(),
            description: "Save a DeepSeek API key into the harness credential store".into(),
            input_hint: None,
            source: None,
        }];
        app.input.set("/log".into());
        let candidates = app.slash_matches();
        let rows: Vec<&str> = candidates.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            rows,
            ["login", "logout"],
            "the two builtins, in table order — the skill adds no rows"
        );
        app.input.set("/login sk-test".into());
        app.submit(&ctl);
        assert!(
            !matches!(app.state, RunState::Starting),
            "the builtin is a local op, never a prompt"
        );
        assert_eq!(
            crate::credentials::stored_key(&app.cfg.home, crate::credentials::API_KEY_REF,)
                .unwrap(),
            Some("sk-test".into())
        );
    }

    /// Both twins stay local ops; the agent's same-named skills are reachable
    /// only as `/skill login` / `/skill logout`.
    #[test]
    fn logout_is_a_tui_builtin_and_shadows_the_agent_skill() {
        let (mut app, _ctl, _rx) = test_app();
        app.skills = vec![
            crate::bus::SkillInfo {
                name: "logout".into(),
                description: "sign out".into(),
                input_hint: None,
                source: None,
            },
            crate::bus::SkillInfo {
                name: "login".into(),
                description: "agent login".into(),
                input_hint: None,
                source: None,
            },
        ];
        app.input.set("/".into());
        let menu = app.slash_matches();
        assert_eq!(
            menu.len(),
            SLASH_COMMANDS.len(),
            "the bare `/` menu is the builtin table, nothing appended"
        );
        for row in &menu {
            assert!(
                SLASH_COMMANDS.iter().any(|c| c.name == row.name),
                "{} is not a builtin",
                row.name
            );
        }
        assert!(menu.iter().any(|e| e.name == "logout"));
        assert!(menu.iter().any(|e| e.name == "login"));
        // The skills did not disappear — `/skill ` still offers both.
        app.input.set("/skill ".into());
        let candidates = app.slash_matches();
        let mut catalog: Vec<&str> = candidates.iter().map(|e| e.usage.as_str()).collect();
        catalog.sort_unstable();
        assert_eq!(catalog, ["login", "logout"], "the catalog keeps them both");
    }

    #[test]
    fn staged_images_send_together_with_token_free_caption() {
        let (mut app, ctl, _rx) = test_app();
        app.stage_image(
            "clipboard.png".into(),
            "clipboard".into(),
            "image/png".into(),
            vec![0u8; 8],
            String::new(),
        );
        app.stage_image(
            "shot-2.png".into(),
            "clipboard".into(),
            "image/png".into(),
            vec![1u8; 8],
            String::new(),
        );
        app.input.insert_str("look");
        app.submit(&ctl);
        assert!(app.pending_images.is_empty(), "tray cleared after send");
        assert!(app.input.is_empty());
        assert!(
            matches!(app.state, RunState::Starting),
            "sending starts the turn"
        );
    }

    #[test]
    fn draft_split_keeps_text_and_images_interleaved() {
        let mut staged = crate::attachments::Staged::default();
        staged
            .add(
                crate::locale::Locale::En,
                "a.png".into(),
                "/tmp/a.png".into(),
                "image/png".into(),
                vec![1],
            )
            .unwrap();
        staged
            .add(
                crate::locale::Locale::En,
                "b.png".into(),
                "/tmp/b.png".into(),
                "image/png".into(),
                vec![2],
            )
            .unwrap();
        let blocks =
            split_draft_into_staged_blocks("see [image 1] then [image 2] done", staged.drain());
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], StagedBlock::Text(t) if t == "see"));
        assert!(matches!(&blocks[1], StagedBlock::Image(a) if a.name == "a.png"));
        assert!(matches!(&blocks[2], StagedBlock::Text(t) if t == " then "));
        assert!(matches!(&blocks[3], StagedBlock::Image(a) if a.name == "b.png"));
        assert!(matches!(&blocks[4], StagedBlock::Text(t) if t == "done"));
        let prompt = prompt_blocks_from_staged(&blocks);
        assert!(matches!(&prompt[0], crate::bus::PromptBlock::Text(t) if t == "see"));
        assert!(matches!(&prompt[1], crate::bus::PromptBlock::Image(a) if a.path == "/tmp/a.png"));
        assert!(matches!(&prompt[2], crate::bus::PromptBlock::Text(t) if t == " then "));
        assert!(matches!(&prompt[3], crate::bus::PromptBlock::Image(a) if a.path == "/tmp/b.png"));
        assert!(matches!(&prompt[4], crate::bus::PromptBlock::Text(t) if t == "done"));
    }

    #[test]
    fn draft_split_does_not_append_chips_missing_from_the_draft() {
        let mut staged = crate::attachments::Staged::default();
        staged
            .add(
                crate::locale::Locale::En,
                "kept.png".into(),
                "/tmp/kept.png".into(),
                "image/png".into(),
                vec![1],
            )
            .unwrap();
        staged
            .add(
                crate::locale::Locale::En,
                "orphan.png".into(),
                "/tmp/orphan.png".into(),
                "image/png".into(),
                vec![2],
            )
            .unwrap();
        let blocks = split_draft_into_staged_blocks("hello [image 1]", staged.drain());
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], StagedBlock::Text(t) if t == "hello"));
        assert!(matches!(&blocks[1], StagedBlock::Image(a) if a.name == "kept.png"));
    }

    #[test]
    fn submit_echoes_interleaved_transcript_not_caption_then_images() {
        let (mut app, ctl, _rx) = test_app();
        app.stage_image(
            "a.png".into(),
            "/tmp/a.png".into(),
            "image/png".into(),
            vec![0u8; 4],
            String::new(),
        );
        app.stage_image(
            "b.png".into(),
            "/tmp/b.png".into(),
            "image/png".into(),
            vec![1u8; 4],
            String::new(),
        );
        app.input.set("see [image 1] then [image 2] done".into());
        app.submit(&ctl);
        let kinds: Vec<String> = app
            .transcript
            .cells
            .iter()
            .map(|c| match &c.kind {
                crate::transcript::CellKind::User { text, .. } => format!("text:{text}"),
                crate::transcript::CellKind::Image { name, caption, .. } => {
                    format!("image:{name}:{caption}")
                }
                _ => "other".into(),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "text:see",
                "image:a.png:",
                "text: then ",
                "image:b.png:",
                "text:done"
            ]
        );
    }

    fn ask_options() -> Vec<crate::bus::PermissionAskOption> {
        vec![
            crate::bus::PermissionAskOption {
                option_id: "reject".into(),
                kind: "reject_once".into(),
                name: "Reject".into(),
            },
            crate::bus::PermissionAskOption {
                option_id: "allow".into(),
                kind: "allow_once".into(),
                name: "Allow once".into(),
            },
        ]
    }

    #[test]
    fn acp_permission_ask_enter_selects_option_id() {
        let (mut app, ctl, _rx) = test_app();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.handle(
            AppEvent::PermissionAsk {
                title: "bash".into(),
                options: ask_options(),
                reply: tx,
            },
            &ctl,
        );
        let ask = app.permission_ask.as_ref().expect("overlay opens");
        assert_eq!(ask.title, "bash");
        assert_eq!(ask.sel, 1, "allow_once is preselected, not auto-chosen");
        assert_eq!(ask.options[0].name, "Reject");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        assert!(app.permission_ask.is_none());
        assert_eq!(
            rx.blocking_recv().expect("reply"),
            crate::bus::PermissionAskReply::Selected("allow".into())
        );
    }

    #[test]
    fn acp_permission_ask_esc_cancels() {
        let (mut app, ctl, _rx) = test_app();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.handle(
            AppEvent::PermissionAsk {
                title: "bash".into(),
                options: ask_options(),
                reply: tx,
            },
            &ctl,
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.permission_ask.is_none());
        assert_eq!(
            rx.blocking_recv().expect("reply"),
            crate::bus::PermissionAskReply::Cancelled
        );
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;
    use crate::theme::DEEPSEEK_450;
    use ratatui::style::Color;
    use std::sync::mpsc::Receiver;

    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dsh-tui-palette-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn test_app() -> (App, Controller, Receiver<AppEvent>) {
        let cfg = RuntimeConfig {
            workspace: "/tmp".into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let (_tx, rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        (app, ctl, rx)
    }

    /// A fresh install opens on One, dark. The built-in DeepSeek pack is not
    /// gone — it is a row of the gallery, not the starting point.
    #[test]
    fn starts_on_the_one_pack() {
        let (app, _ctl, _rx) = test_app();
        assert_eq!(app.active_palette_id, "one");
        assert_eq!(app.theme.mode, crate::theme::Mode::Dark);
        assert_eq!(app.theme.brand, pack_brand(&app, "one"));
        assert_eq!(
            pack_brand(&app, "default"),
            DEEPSEEK_450,
            "the built-in pack still paints DeepSeek blue"
        );
    }

    #[test]
    fn slash_theme_dark_light_stay_in_active_pack() {
        let (mut app, ctl, _rx) = test_app();
        let pack = crate::theme::PalettePack::from_json(
            &serde_json::from_str::<serde_json::Value>(include_str!("palettes/one.json")).unwrap(),
        )
        .expect("one pack");
        app.palettes.push(pack);
        app.run_slash("theme", "one", &ctl);
        app.run_slash("theme", "light", &ctl);
        assert_eq!(app.active_palette_id, "one");
        assert_eq!(app.theme.mode, crate::theme::Mode::Light);
        assert_eq!(app.theme.brand, Color::Rgb(47, 90, 243));
        app.run_slash("theme", "dark", &ctl);
        assert_eq!(app.theme.mode, crate::theme::Mode::Dark);
        assert_eq!(app.theme.brand, Color::Rgb(97, 175, 239));
    }

    #[test]
    fn slash_theme_usage_mentions_pack_ids() {
        let theme = SLASH_COMMANDS.iter().find(|c| c.name == "theme").unwrap();
        assert!(
            theme.usage.contains("id"),
            "usage should mention pack ids, got {}",
            theme.usage
        );
    }

    /// The brand a gallery pack paints in `mode` — the value live previews and
    /// commits must land on.
    fn pack_brand_in(app: &App, id: &str, mode: crate::theme::Mode) -> Color {
        app.palettes
            .iter()
            .find(|pack| pack.id == id)
            .unwrap_or_else(|| panic!("pack {id}"))
            .theme(mode)
            .brand
    }

    fn pack_brand(app: &App, id: &str) -> Color {
        pack_brand_in(app, id, crate::theme::Mode::Dark)
    }

    fn key(app: &mut App, ctl: &Controller, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), ctl);
    }

    /// Walk the open theme dialog's highlight onto `id`'s row, ↓ one row at a
    /// time so the tests do not depend on where the gallery puts it.
    fn highlight_pack(app: &mut App, ctl: &Controller, id: &str) {
        let highlighted = |app: &App| {
            app.picker
                .as_ref()
                .and_then(|picker| picker.items.get(picker.sel))
                .map(|item| item.id.clone())
        };
        for _ in 0..app.palettes.len() {
            if highlighted(app).as_deref() == Some(id) {
                return;
            }
            key(app, ctl, KeyCode::Down);
        }
        panic!("the dialog never reached {id}");
    }

    fn wheel(app: &mut App, ctl: &Controller, kind: MouseEventKind) {
        app.handle(
            AppEvent::Term(Event::Mouse(MouseEvent {
                kind,
                column: 40,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })),
            ctl,
        );
    }

    /// The theme dialog lives on its highlight: arrows paint the row under it
    /// immediately, but only Enter commits — the committed pack and
    /// `settings.json` wait for the confirmation.
    #[test]
    fn theme_dialog_arrows_preview_and_only_enter_commits() {
        let (mut app, ctl, _rx) = test_app();
        let one = pack_brand(&app, "one");
        let ayu = pack_brand(&app, "ayu");
        assert_ne!(ayu, one, "the test pack must differ from the committed one");
        app.run_slash("theme", "", &ctl);
        {
            let picker = app.picker.as_ref().expect("the dialog opens");
            assert_eq!(
                picker.items[picker.sel].id, app.active_palette_id,
                "the committed row opens highlighted"
            );
        }

        // ↓ onto ayu: the painter follows right away, the committed pack does
        // not.
        highlight_pack(&mut app, &ctl, "ayu");
        assert_eq!(app.theme.brand, ayu);
        assert_eq!(app.active_palette_id, "one", "arrows only preview");
        assert!(app.picker.is_some(), "preview must keep browsing open");
        assert_eq!(app.theme_preview.as_deref(), Some("ayu"));

        // Back onto the committed row → the preview is gone.
        highlight_pack(&mut app, &ctl, "one");
        assert_eq!(app.theme.brand, one);
        assert!(app.theme_preview.is_none());

        // Onto ayu once more, then Enter confirms: the dialog closes, ayu
        // commits.
        highlight_pack(&mut app, &ctl, "ayu");
        assert_eq!(app.theme.brand, ayu);
        assert_eq!(app.active_palette_id, "one", "still only previewed");
        key(&mut app, &ctl, KeyCode::Enter);
        assert!(app.picker.is_none());
        assert_eq!(app.active_palette_id, "ayu");
        assert_eq!(app.theme.brand, ayu);
        assert!(app.theme_preview.is_none(), "the commit clears the preview");
    }

    /// Esc and the wheel follow the same rule: the wheel previews, Esc closes
    /// the dialog and the committed pack comes back — nothing sticks.
    #[test]
    fn theme_dialog_esc_and_wheel_revert_to_the_committed_pack() {
        let (mut app, ctl, _rx) = test_app();
        let one = pack_brand(&app, "one");
        let ayu = pack_brand(&app, "ayu");
        app.run_slash("theme", "", &ctl);

        wheel(&mut app, &ctl, MouseEventKind::ScrollDown);
        assert_ne!(app.theme.brand, one, "one notch previews the next row");
        assert!(app.picker.is_some(), "the wheel keeps the dialog open");
        wheel(&mut app, &ctl, MouseEventKind::ScrollUp);
        assert_eq!(app.theme.brand, one, "back on the committed row");

        highlight_pack(&mut app, &ctl, "ayu");
        assert_eq!(app.theme.brand, ayu);
        key(&mut app, &ctl, KeyCode::Esc);
        assert!(app.picker.is_none());
        assert_eq!(app.active_palette_id, "one");
        assert_eq!(
            app.theme.brand, one,
            "Esc must revert the preview — arrows never confirm"
        );
    }

    /// A preview must never reach `settings.json`; the Enter that follows it
    /// must.
    #[test]
    fn theme_preview_paints_without_persisting() {
        let (mut app, ctl, _rx) = test_app();
        let path = App::locale_settings_path(&app.cfg);
        let ayu = pack_brand(&app, "ayu");
        app.run_slash("theme", "", &ctl);

        highlight_pack(&mut app, &ctl, "ayu");
        assert_eq!(app.theme.brand, ayu);
        let saved = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !saved.contains("ayu"),
            "a preview must not land in settings.json, got {saved}"
        );

        key(&mut app, &ctl, KeyCode::Enter);
        let saved = std::fs::read_to_string(&path).expect("enter persists the pack");
        assert!(
            saved.contains("\"palette\": \"ayu\""),
            "the commit persists the pack, got {saved}"
        );
    }

    /// ctrl+t inside the dialog (the title advertises it) flips the mode of
    /// the *previewed* pack and keeps browsing.
    #[test]
    fn theme_dialog_ctrl_t_toggles_the_previewed_pack() {
        let (mut app, ctl, _rx) = test_app();
        let one = pack_brand(&app, "one");
        let ayu_dark = pack_brand(&app, "ayu");
        let ayu_light = pack_brand_in(&app, "ayu", crate::theme::Mode::Light);
        assert_ne!(ayu_dark, ayu_light, "the test pack must differ per mode");
        app.run_slash("theme", "", &ctl);
        highlight_pack(&mut app, &ctl, "ayu");

        app.handle_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert_eq!(app.theme.mode, crate::theme::Mode::Light);
        assert_eq!(app.theme.brand, ayu_light, "the toggle stays in-pack");
        assert_eq!(app.active_palette_id, "one", "still only previewed");
        assert!(app.picker.is_some(), "ctrl+t must not close the dialog");

        // Esc drops the preview back to the committed pack — in the mode the
        // toggle left behind.
        key(&mut app, &ctl, KeyCode::Esc);
        assert_eq!(app.theme.mode, crate::theme::Mode::Light);
        assert_eq!(
            app.theme.brand,
            pack_brand_in(&app, "one", crate::theme::Mode::Light),
            "One light, not ayu"
        );
        assert_ne!(app.theme.brand, one, "…in the mode, not the old one");
    }

    /// The `/theme ` candidate popup previews the palette under its
    /// highlight and reverts when the popup closes — arrows never confirm.
    #[test]
    fn slash_theme_popup_previews_and_reverts_without_enter() {
        let (mut app, ctl, _rx) = test_app();
        let one = pack_brand(&app, "one");
        let ayu = pack_brand(&app, "ayu");
        app.input.set("/theme ".into());

        // Rows: dark · light · default · ayu …
        key(&mut app, &ctl, KeyCode::Down);
        assert_eq!(app.theme.brand, one, "the `light` row previews nothing");
        key(&mut app, &ctl, KeyCode::Down);
        assert_eq!(
            app.theme.brand, DEEPSEEK_450,
            "the `default` row previews the built-in pack"
        );
        key(&mut app, &ctl, KeyCode::Down);
        assert_eq!(app.theme.brand, ayu, "the ayu row previews it");
        assert_eq!(app.active_palette_id, "one", "arrows only preview");
        assert_eq!(app.input.buf(), "/theme ", "the draft survives the preview");

        // Esc dismisses the popup and reverts with it.
        key(&mut app, &ctl, KeyCode::Esc);
        assert!(app.input.is_empty());
        assert_eq!(app.theme.brand, one, "Esc reverts the popup preview");
    }

    /// Enter on the highlighted row is the confirmation there too.
    #[test]
    fn slash_theme_popup_enter_commits_the_previewed_pack() {
        let (mut app, ctl, _rx) = test_app();
        let ayu = pack_brand(&app, "ayu");
        app.input.set("/theme ".into());
        for _ in 0..3 {
            key(&mut app, &ctl, KeyCode::Down);
        }
        assert_eq!(app.theme.brand, ayu);
        assert_eq!(app.active_palette_id, "one");

        key(&mut app, &ctl, KeyCode::Enter);
        assert_eq!(app.active_palette_id, "ayu");
        assert_eq!(app.theme.brand, ayu);
        assert!(
            app.input.is_empty(),
            "Enter runs the command and clears the draft"
        );
        assert!(app.theme_preview.is_none());
    }
}

#[cfg(test)]
mod right_slot_tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    fn test_app() -> (App, Controller, Receiver<AppEvent>) {
        let cfg = RuntimeConfig {
            workspace: "/tmp".into(),
            home: std::env::temp_dir()
                .join(format!("abylab-right-slot-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            sessions_root: std::env::temp_dir()
                .join(format!("abylab-right-slot-sessions-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let (_tx, rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        (app, ctl, rx)
    }

    #[test]
    fn status_slash_opens_a_modal_with_state_and_the_usage_counters() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::En;
        app.modes.effort = Some("high".into());
        app.session_title = Some("fix the login flow".into());
        // The counter rows the composer dock used to paint: the modal renders
        // them from the transcript accumulator instead.
        app.transcript.usage.input = 1834;
        app.transcript.usage.output = 412;
        app.transcript.usage.cached = 1200;
        app.transcript.stats.turns = 3;
        app.transcript.stats.steps = 47;
        app.transcript.stats.turn_millis = 135_000;
        app.transcript.stats.tool_millis = 8_000;
        app.transcript.stats.ttft_total_millis = 4_500;
        app.transcript.stats.ttft_count = 3;
        let cells_before = app.transcript.cells.len();

        app.run_slash("status", "", &ctl);

        assert_eq!(
            app.transcript.cells.len(),
            cells_before,
            "/status is chrome and must not enter the conversation timeline"
        );
        let overlay = app
            .view_overlay
            .as_ref()
            .expect("/status modal should open");
        assert_eq!(overlay.title, "Status");
        let crate::slots::TuiNode::Markdown { text, .. } = &overlay.nodes[0] else {
            panic!("/status should render markdown, got {:?}", overlay.nodes[0]);
        };
        // The border names the card, so the body carries bullets only.
        assert!(!text.contains("## status"), "{text}");
        assert!(text.contains("- state · "), "{text}");
        // The session row carries the live title, and the credential source
        // (env vs stored vs none) landed here when `/session` retired.
        assert!(
            text.contains("- session · dsh-test · fix the login flow"),
            "{text}"
        );
        assert!(
            text.contains("- credentials · no api key — /login <apikey> stores one"),
            "{text}"
        );
        assert!(text.contains("- model · deepseek-v4-flash"), "{text}");
        assert!(text.contains("- effort · high"), "{text}");
        assert!(text.contains("- permission · "), "{text}");
        assert!(text.contains("- plan · "), "{text}");
        // Tokens, turns/steps, timing and TTFT ride the same card.
        assert!(text.contains("- tokens · ↑1.8K ↓412 · cache 65%"), "{text}");
        assert!(text.contains("- turns · 3 · steps · 47"), "{text}");
        assert!(text.contains("- LLM · 2m7s · tool · 8.0s"), "{text}");
        assert!(text.contains("- TTFT avg · 1.5s"), "{text}");

        // The card is drawn over the chat and esc closes it.
        let frame = crate::ui::dump_frame(&mut app, 100, 30);
        assert!(
            frame.contains("Status · ↑↓/wheel scroll"),
            "modal:\n{frame}"
        );
        assert!(frame.contains("tokens · ↑1.8K ↓412"), "counters:\n{frame}");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.view_overlay.is_none(), "esc closes the modal");
    }

    /// A zero-data session still lists the counters as zeros (no dropped
    /// row) while an unsampled TTFT stays out of the card — and the modal is
    /// localized like the rest of the built-in chrome.
    #[test]
    fn status_slash_keeps_the_counter_row_for_a_fresh_session() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::Zh;

        app.run_slash("status", "", &ctl);

        let overlay = app
            .view_overlay
            .as_ref()
            .expect("/status modal should open");
        assert_eq!(overlay.title, "状态");
        let crate::slots::TuiNode::Markdown { text, .. } = &overlay.nodes[0] else {
            panic!("/status should render markdown, got {:?}", overlay.nodes[0]);
        };
        assert!(text.contains("- state · 空闲"), "{text}");
        assert!(text.contains("- tokens · ↑0 ↓0 · cache 0%"), "{text}");
        assert!(text.contains("- turns · 0 · steps · 0"), "{text}");
        assert!(text.contains("- LLM · 0.0s · tool · 0.0s"), "{text}");
        assert!(!text.contains("- TTFT avg ·"), "{text}");
    }

    /// The `/` menu lists `/status` (it was dispatch-only) with its localized
    /// description.
    #[test]
    fn slash_menu_offers_the_status_dialog() {
        let (mut app, _ctl, _rx) = test_app();
        app.input.set("/status".into());

        let matches = app.slash_matches();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "status");
        assert_eq!(matches[0].usage, "/status");
        assert_eq!(
            matches[0].desc,
            "run state, model and the live usage counters"
        );

        app.locale = crate::locale::Locale::Zh;
        assert_eq!(app.slash_matches()[0].desc, "状态、模型和实时用量统计");
    }

    /// A new session greets with one usage hint in the timeline — the line the
    /// composer cap row used to rotate live — and the next `/new` walks on to
    /// the following hint instead of repeating the first.
    #[test]
    fn new_session_greets_with_the_next_usage_hint() {
        let (mut app, ctl, _rx) = test_app();
        app.locale = Locale::En;
        app.transcript.push_user("a previous prompt".into(), false);
        let greeting = |app: &App| -> String {
            let first = app.transcript.cells.first().expect("greeting cell");
            let crate::transcript::CellKind::MarkdownNotice { text } = &first.kind else {
                panic!("the greeting should be markdown, got {:?}", first.kind);
            };
            text.clone()
        };

        app.run_slash("new", "", &ctl);
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "first-new".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(
            greeting(&app),
            "- **Tip** · esc interrupts a running turn — your draft survives"
        );

        app.run_slash("new", "", &ctl);
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "second-new".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );
        assert_eq!(
            greeting(&app),
            "- **Tip** · enter queues a follow-up; ctrl+enter steers the active turn now"
        );
    }
}

#[cfg(test)]
mod at_menu_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::mpsc::Receiver;

    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "abylab-at-menu-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn test_cfg() -> RuntimeConfig {
        RuntimeConfig {
            workspace: "/tmp".into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        }
    }

    fn test_app() -> (App, Controller, Receiver<AppEvent>) {
        test_app_in("/tmp")
    }

    /// Same app, but rooted at a fixture workspace (the browser lists real
    /// directories, so the `@` tests need a real tree).
    fn test_app_in(workspace: &str) -> (App, Controller, Receiver<AppEvent>) {
        let cfg = RuntimeConfig {
            workspace: workspace.into(),
            ..test_cfg()
        };
        let (_tx, rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        (app, ctl, rx)
    }

    #[test]
    fn at_opens_the_browser_and_enter_inserts_the_mention() {
        let (mut app, ctl, _rx) = test_app();
        let before = app.input.buf().clone();
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        assert!(
            app.file_menu.is_some(),
            "the @file browser opens on a fresh token"
        );
        // Enter replaces the token with the workspace mention.
        app.file_menu_settle();
        assert!(app.file_menu.is_none());
        let after = app.input.buf();
        let mention = after.strip_prefix(&before).unwrap_or("");
        assert!(mention.starts_with('@'), "mention inserted: {mention:?}");
    }

    #[test]
    fn esc_dismiss_stays_closed_until_the_token_changes() {
        let (mut app, ctl, _rx) = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &ctl);
        assert!(app.file_menu.is_none(), "esc closes the browser");
        // The same token stays closed.
        app.refresh_file_menu();
        assert!(app.file_menu.is_none(), "dismissed token stays closed");
        // Typing more query text re-opens the browser.
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &ctl);
        assert!(
            app.file_menu.is_some(),
            "a changed token reopens the browser"
        );
        app.dismiss_file_menu();
        assert!(app.file_menu.is_none());
    }

    #[test]
    fn caret_leaving_the_token_closes_the_menu() {
        let (mut app, ctl, _rx) = test_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        assert!(app.file_menu.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &ctl);
        assert!(app.file_menu.is_none(), "whitespace ends the token");
        assert!(app.file_menu_dismissed.is_none());
    }

    /// A fixture workspace with a hidden `.env` the browser can reveal.
    struct Workspace {
        root: std::path::PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "abylab-at-menu-ws-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("src/nested")).expect("create tree");
            std::fs::write(root.join("src/main.rs"), "").expect("write");
            std::fs::write(root.join("README.md"), "").expect("write");
            std::fs::write(root.join("notes.txt"), "").expect("write");
            Self { root }
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// ctrl+h is the explorer's own hidden-entries toggle, and it stays
    /// modal while the browser is open (the draft keeps its text).
    #[test]
    fn ctrl_h_toggles_hidden_files_in_the_browser() {
        let ws = Workspace::new();
        std::fs::write(ws.root.join(".env"), "").expect("write");
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());

        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        fn hidden_state(app: &App) -> bool {
            app.file_menu
                .as_ref()
                .expect("menu open")
                .explorer()
                .show_hidden()
        }
        fn has_env(app: &App) -> bool {
            app.file_menu
                .as_ref()
                .expect("menu open")
                .explorer()
                .files()
                .iter()
                .any(|f| f.name == ".env")
        }
        assert!(!hidden_state(&app));
        assert!(!has_env(&app), ".env hidden by default");

        app.handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert!(hidden_state(&app), "ctrl+h reveals hidden entries");
        assert!(has_env(&app), ".env listed while hidden shown");
        assert_eq!(app.input.buf(), "@", "the toggle is modal, not typed");

        // Toggling back hides them again and the browser still navigates.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
            &ctl,
        );
        assert!(!hidden_state(&app));
        assert!(!has_env(&app), ".env hidden again");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl); // src/
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl); // README.md
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "@README.md");
    }

    /// The `/` menu and the `@` browser are mutually exclusive: a banner
    /// command draft never opens the mention browser under it.
    #[test]
    fn slash_draft_does_not_open_the_browser() {
        let (mut app, ctl, _rx) = test_app();
        for ch in "/th".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &ctl);
        }
        assert!(app.slash_completion_open(), "the / menu owns the band");

        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        assert!(
            app.file_menu.is_none(),
            "slash context never opens the @ menu"
        );

        // Drop the command prefix and the browser takes over.
        for _ in 0..4 {
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &ctl);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        assert!(app.file_menu.is_some(), "a plain @ opens the browser");
    }

    /// Send-now (ctrl+enter) clears the draft — the browser must not
    /// survive the send and float over the busy transcript.
    #[test]
    fn send_now_closes_the_browser() {
        let ws = Workspace::new();
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);
        assert!(app.file_menu.is_some());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL), &ctl);
        assert!(app.file_menu.is_none(), "the send closes the browser");
    }

    /// Typing narrows the listing itself (Martty's live filter), not just
    /// the highlight: non-matching entries leave the popup.
    #[test]
    fn browser_filter_narrows_the_listing() {
        let ws = Workspace::new();
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
        for ch in "@read".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &ctl);
        }

        let frame = crate::ui::dump_frame(&mut app, 100, 24);
        assert!(frame.contains("README.md"), "the match stays:\n{frame}");
        assert!(
            !frame.contains("notes.txt") && !frame.contains("src/"),
            "non-matching entries leave the listing:\n{frame}"
        );
    }

    /// The browser's hint row names the modal keys, ctrl+h included.
    #[test]
    fn browser_hint_names_the_hidden_toggle() {
        let ws = Workspace::new();
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
        app.handle_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE), &ctl);

        let frame = crate::ui::dump_frame(&mut app, 100, 24);
        assert!(frame.contains("ctrl+h hidden"), "hint row:\n{frame}");
        assert!(frame.contains("enter pick"), "hint row:\n{frame}");
    }

    /// Tab / → on a directory rewrites the token to `@dir/` *and* takes the
    /// browser inside: the listing must be the child's. The drill used to
    /// hand the browser a query that still carried the `@` (`@src/`), so the
    /// filter matched nothing, the popup emptied down to `../` and the cwd
    /// never moved.
    #[test]
    fn tab_and_right_drill_into_the_selected_directory() {
        fn names(app: &App) -> Vec<String> {
            app.file_menu
                .as_ref()
                .expect("browser open")
                .explorer()
                .files()
                .iter()
                .map(|f| f.name.clone())
                .collect()
        }
        for key in [KeyCode::Tab, KeyCode::Right] {
            let ws = Workspace::new();
            let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
            for ch in "@src".chars() {
                app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &ctl);
            }
            assert_eq!(
                app.file_menu
                    .as_ref()
                    .expect("browser open")
                    .explorer()
                    .current()
                    .name,
                "src/",
                "the query preselected the directory"
            );

            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE), &ctl);

            assert_eq!(app.input.buf(), "@src/", "{key:?} keeps drilling open");
            let menu = app.file_menu.as_ref().expect("browser stays open");
            assert_eq!(
                menu.explorer().cwd(),
                &ws.root.join("src"),
                "{key:?} enters src/"
            );
            assert_eq!(names(&app), ["../", "nested/", "main.rs"], "{key:?}");
            let frame = crate::ui::dump_frame(&mut app, 100, 24);
            assert!(
                frame.contains("nested/") && !frame.contains("README.md"),
                "{key:?} paints the child listing:\n{frame}"
            );

            // Enter settles the drilled dir into a mention from the child.
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctl);
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &ctl);
            assert_eq!(app.input.buf(), "@src/nested/", "{key:?}");
        }
    }

    /// A quoted token keeps its quote open across the drill (`@"dir with
    /// space/`), and the browser lands inside it.
    #[test]
    fn tab_drills_into_a_quoted_directory() {
        let ws = Workspace::new();
        std::fs::create_dir_all(ws.root.join("with space")).expect("create dir");
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
        for ch in "@with".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &ctl);
        }
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &ctl);

        assert_eq!(app.input.buf(), "@\"with space/");
        let menu = app.file_menu.as_ref().expect("browser stays open");
        assert_eq!(menu.explorer().cwd(), &ws.root.join("with space"));
        assert!(menu.quoted(), "the quoted form survives the drill");
    }

    /// The follow search (`@nested` with no `nested*` in the root hops the
    /// browser into `src/`) plus the drill: the token jumps to the full
    /// workspace-relative path and the listing is the nested directory's.
    #[test]
    fn tab_drills_after_the_follow_search() {
        let ws = Workspace::new();
        let (mut app, ctl, _rx) = test_app_in(&ws.root.to_string_lossy());
        for ch in "@nested".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &ctl);
        }
        assert_eq!(
            app.file_menu
                .as_ref()
                .expect("browser open")
                .explorer()
                .cwd(),
            &ws.root.join("src"),
            "the follow search landed in src/"
        );

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &ctl);
        assert_eq!(app.input.buf(), "@src/nested/");
        assert_eq!(
            app.file_menu
                .as_ref()
                .expect("browser stays open")
                .explorer()
                .cwd(),
            &ws.root.join("src/nested")
        );
    }
}

#[cfg(test)]
mod resume_replay_tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "abylab-resume-replay-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    fn test_cfg() -> RuntimeConfig {
        RuntimeConfig {
            workspace: "/tmp".into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        }
    }

    fn test_app() -> (App, Controller, Receiver<AppEvent>) {
        let cfg = test_cfg();
        let (_tx, rx) = std::sync::mpsc::channel::<AppEvent>();
        let (ctl, _commands) = crate::controller::test_controller();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        (app, ctl, rx)
    }

    #[test]
    fn plan_snapshots_drive_the_composer_todo_line() {
        let (mut app, ctl, _rx) = test_app();
        let todos = vec![
            crate::events::PlanItem {
                content: "inspect".into(),
                status: crate::events::PlanStatus::Completed,
            },
            crate::events::PlanItem {
                content: "patch".into(),
                status: crate::events::PlanStatus::InProgress,
            },
            crate::events::PlanItem {
                content: "test".into(),
                status: crate::events::PlanStatus::Pending,
            },
        ];

        app.handle(
            AppEvent::Ui(crate::events::UiEvent::Plan {
                session: "dsh-test".into(),
                summary: "1 of 3 done · now: patch · 1 pending".into(),
                todos: todos.clone(),
                active: Some("patch".into()),
                active_extra: 1,
                completed: 1,
                total: 3,
            }),
            &ctl,
        );
        assert_eq!(
            app.plan,
            Some(crate::events::PlanProgress {
                todos: todos.clone(),
                active: Some("patch".into()),
                active_extra: 1,
                completed: 1,
                total: 3,
            }),
            "a committed todo_write reaches the cap row"
        );

        // The driver's blank plan event (no list, or an explicit clear) takes
        // the todo line away again.
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::Plan {
                session: "dsh-test".into(),
                summary: String::new(),
                todos: Vec::new(),
                active: None,
                active_extra: 0,
                completed: 0,
                total: 0,
            }),
            &ctl,
        );
        assert_eq!(app.plan, None);

        // A plan may never survive into the next session.
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::Plan {
                session: "dsh-test".into(),
                summary: "1 of 3 done · now: patch · 1 pending".into(),
                todos: todos.clone(),
                active: Some("patch".into()),
                active_extra: 0,
                completed: 1,
                total: 3,
            }),
            &ctl,
        );
        app.reset_session_ui();
        assert_eq!(app.plan, None);
    }

    /// The cap row's `⛶` glyph is a mouse-only toggle: each click pins the
    /// well to the amplified height or hands it back to the auto layout.
    #[test]
    fn the_expand_glyph_pins_and_restores_the_well_height() {
        let (mut app, _ctl, _rx) = test_app();
        // The frame that draws the glyph records its hit target.
        app.expand_btn = Some(ratatui::layout::Rect::new(80, 4, 3, 1));

        for expected in [true, false] {
            app.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 81,
                row: 4,
                modifiers: KeyModifiers::NONE,
            });
            assert_eq!(app.composer_expanded, expected, "click {expected}");
        }

        // A click that misses the glyph leaves the height alone.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.composer_expanded);
    }

    /// The driver's live catalog fills an open `/model` picker (a query it now
    /// answers mid-turn), keeps the configured model selected when the
    /// provider lists it, and is remembered for the next picker opening.
    #[test]
    fn the_catalog_fills_the_open_model_picker_and_is_remembered() {
        let (mut app, ctl, _rx) = test_app();
        app.cfg.model = "deepseek-v4-flash".into();
        app.cfg.provider = "deepseek-official".into();
        app.open_model_picker(&ctl);
        assert!(matches!(
            app.picker.as_ref().expect("picker opens").kind,
            PickerKind::Model
        ));
        let stock = app.picker.as_ref().expect("picker").items.len();
        assert!(stock > 1, "the picker opens on its stock rows");

        app.handle(
            AppEvent::Ctl(CtlEvent::Catalog {
                models: vec![
                    crate::bus::CatalogModel {
                        provider: "coding-plan-b".into(),
                        id: "deepseek-v4-pro".into(),
                        name: "DeepSeek V4 Pro".into(),
                        vision: false,
                    },
                    crate::bus::CatalogModel {
                        provider: "deepseek-official".into(),
                        id: "deepseek-v4-flash".into(),
                        name: "DeepSeek V4 Flash".into(),
                        vision: true,
                    },
                ],
            }),
            &ctl,
        );
        let picker = app.picker.as_ref().expect("picker stays open");
        let ids: Vec<&str> = picker.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["deepseek-v4-pro", "deepseek-v4-flash"]);
        assert_eq!(
            picker.sel, 1,
            "the configured model keeps the selection (provider included)"
        );
        assert_eq!(
            picker.items[1].meta, "deepseek-official · DeepSeek V4 Flash · vision",
            "the meta row names the provider, the label and vision"
        );

        // A reopened popup seeds the stock rows again (the driver re-fetches),
        // while the cached listing is what the `/model ` candidates offer.
        app.picker = None;
        app.open_model_picker(&ctl);
        assert_eq!(
            app.picker.as_ref().expect("picker reopens").items.len(),
            3,
            "stock presets + the configured model"
        );
        app.input.set("/model ".into());
        let candidates = app.slash_matches();
        assert_eq!(
            candidates
                .iter()
                .map(|e| e.desc.as_str())
                .collect::<Vec<_>>(),
            ["coding-plan-b", "deepseek-official"],
            "the provider rides the meta"
        );
        assert_eq!(
            candidates
                .iter()
                .filter_map(|e| e.completion.as_deref())
                .collect::<Vec<_>>(),
            ["/model deepseek-v4-pro", "/model deepseek-v4-flash"],
            "the completion inserts the id"
        );
    }

    /// The meta row's `↓ N` chip is the way back down: clicking it drops the
    /// scroll and follows the newest line again. The frame that draws the chip
    /// records the cell, hover brightens it, and an open modal swallows the
    /// click like it does for the other glyph buttons.
    #[test]
    fn clicking_the_scroll_chip_follows_the_tail_again() {
        let (mut app, ctl, _rx) = test_app();
        for i in 0..40 {
            app.transcript.push_user(format!("line {i}"), false);
        }
        app.scroll_by(20);
        let _ = crate::ui::dump_frame(&mut app, 100, 14);
        let chip = app.scroll_btn.expect("the scrolled frame records the chip");
        assert!(app.scroll_up > 0);

        // Hover: the chip brightens (the pointer rests on it).
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: chip.x + 1,
            row: chip.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.hover_scroll_btn, "hover lands on the chip");

        // A modal owns the screen: the click is not the chip's while one is up.
        app.view_overlay = Some(crate::app::ViewOverlay {
            title: "status".into(),
            nodes: Vec::new(),
            scroll: 0,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chip.x,
            row: chip.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.scroll_up > 0, "a modal swallows the click");
        app.view_overlay = None;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chip.x,
            row: chip.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.scroll_up, 0, "the click follows the tail");
        assert!(app.needs_redraw);

        // A click that misses the chip leaves the scroll where it was.
        app.scroll_by(20);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: chip.x.saturating_sub(6),
            row: chip.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.scroll_up > 0, "a miss keeps the scroll");

        // With the tail on screen the chip is gone, so nothing is clickable.
        app.scroll_up = 0;
        let _ = crate::ui::dump_frame(&mut app, 100, 14);
        assert!(app.scroll_btn.is_none());
        let _ = ctl;
    }

    /// The cap row's progress chip opens the checklist dialog; esc closes it,
    /// and a cleared checklist takes an open dialog down with it.
    #[test]
    fn todo_progress_chip_opens_and_closes_the_dialog() {
        let (mut app, ctl, _rx) = test_app();
        app.plan = Some(crate::events::PlanProgress {
            todos: vec![crate::events::PlanItem {
                content: "patch".into(),
                status: crate::events::PlanStatus::InProgress,
            }],
            active: Some("patch".into()),
            active_extra: 0,
            completed: 0,
            total: 1,
        });
        // The frame that draws the chip records its hit target.
        app.plan_chip = Some(ratatui::layout::Rect::new(20, 14, 8, 1));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 24,
            row: 14,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.todo_dialog.is_some(), "the progress chip is a button");

        app.handle(
            AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
            &ctl,
        );
        assert!(app.todo_dialog.is_none(), "esc closes the dialog");

        app.open_todo_dialog();
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::Plan {
                session: "dsh-test".into(),
                summary: String::new(),
                todos: Vec::new(),
                active: None,
                active_extra: 0,
                completed: 0,
                total: 0,
            }),
            &ctl,
        );
        assert!(
            app.todo_dialog.is_none(),
            "a cleared checklist closes the dialog"
        );
    }

    /// The `↥` button walks the session's user prompts: newest first, then one
    /// prompt back per click, and the oldest wraps to the newest. Each jump
    /// anchors the prompt at the top of the pane and flashes its rows.
    #[test]
    fn prompt_jump_walks_user_prompts_newest_first() {
        let (mut app, _ctl, _rx) = test_app();
        for text in ["first prompt", "second prompt", "third prompt"] {
            app.transcript.push_user(text.into(), false);
            // Long bodies make each bubble several rows, so the anchor math
            // has room to move the viewport.
            app.transcript.apply(crate::events::UiEvent::TextDelta {
                session: "dsh-test".into(),
                text: "answer\n".repeat(6),
            });
        }
        app.chat_view.area = ratatui::layout::Rect::new(1, 0, 60, 4);
        let layout = app.transcript.layout(&app.theme, 60, app.spinner(), false);
        assert_eq!(layout.users.len(), 3);
        let newest = layout.users[2].cell;
        let middle = layout.users[1].cell;
        let oldest = layout.users[0].cell;

        app.jump_to_user_prompt();
        assert_eq!(
            app.prompt_jump_cell,
            Some(newest),
            "the newest prompt first"
        );
        let h = 4usize;
        let total = layout.lines.len();
        let anchor = |app: &App| total - app.scroll_up - h;
        assert_eq!(
            anchor(&app),
            layout.users[2].line.min(total - h),
            "the jumped prompt is anchored at the top"
        );
        assert_eq!(app.prompt_flash.map(|(cell, _)| cell), Some(newest));

        app.jump_to_user_prompt();
        assert_eq!(app.prompt_jump_cell, Some(middle), "then one prompt back");
        app.jump_to_user_prompt();
        assert_eq!(app.prompt_jump_cell, Some(oldest), "down to the oldest");
        app.jump_to_user_prompt();
        assert_eq!(
            app.prompt_jump_cell,
            Some(newest),
            "the oldest wraps around"
        );

        // The flash expires on the next tick after its window.
        app.prompt_flash = Some((newest, Instant::now() - Duration::from_millis(1)));
        app.tick();
        assert!(app.prompt_flash.is_none());
        assert!(app.prompt_flash_lines.is_none());
    }

    /// A session with no prompts says so instead of moving the viewport.
    #[test]
    fn prompt_jump_without_prompts_tips_instead_of_scrolling() {
        let (mut app, _ctl, _rx) = test_app();
        app.transcript.push_notice(
            crate::transcript::NoticeLevel::Info,
            "a notice, not a prompt".into(),
        );
        app.chat_view.area = ratatui::layout::Rect::new(1, 0, 60, 10);
        app.scroll_up = 3;

        app.jump_to_user_prompt();

        assert_eq!(app.scroll_up, 3, "the viewport stays put");
        assert!(app.prompt_jump_cell.is_none());
        assert!(
            app.tip
                .as_ref()
                .is_some_and(|(text, _)| text.contains("no user prompts")),
            "{:?}",
            app.tip
        );
    }

    #[test]
    fn session_bound_then_user_message_renders_replay() {
        let (mut app, ctl, _rx) = test_app();
        app.handle(
            AppEvent::Ctl(CtlEvent::SessionBound {
                session_id: "persist-demo".into(),
                notice: Some("resumed · 1 turns".into()),
                model: None,
                effort: None,
            }),
            &ctl,
        );
        app.handle(
            AppEvent::Ui(crate::events::UiEvent::UserMessage {
                session: "persist-demo".into(),
                text: "hello persistence".into(),
            }),
            &ctl,
        );
        let users: Vec<&str> = app
            .transcript
            .cells
            .iter()
            .filter_map(|cell| match &cell.kind {
                crate::transcript::CellKind::User { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            users.iter().any(|t| t.contains("hello persistence")),
            "replayed user line lands in the transcript: {users:?}"
        );
    }
}

/// The composer cap's `:branch` suffix: seeded from the workspace once at
/// startup, then re-checked on the tick throttle so a checkout made
/// mid-session (the agent's shell tool, another terminal) reaches the label.
#[cfg(test)]
mod git_branch_tests {
    use super::*;

    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dsh-tui-git-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
    }

    /// A workspace directory whose `.git/HEAD` points at `branch`.
    fn repo(branch: &str) -> String {
        let workspace = fresh_root();
        std::fs::create_dir_all(std::path::Path::new(&workspace).join(".git")).unwrap();
        std::fs::write(
            std::path::Path::new(&workspace).join(".git/HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
        .unwrap();
        workspace
    }

    fn test_app(workspace: &str) -> App {
        let cfg = RuntimeConfig {
            workspace: workspace.into(),
            home: fresh_root(),
            sessions_root: fresh_root(),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let (_tx, _rx) = std::sync::mpsc::channel::<AppEvent>();
        App::new(crate::theme::Theme::dark(), cfg, "dsh-test".into())
    }

    #[test]
    fn startup_seeds_the_branch_from_the_workspace_head() {
        assert_eq!(test_app(&repo("main")).git_branch.as_deref(), Some("main"));
        // A workspace that is not a checkout keeps the cap path-only.
        assert_eq!(test_app("/work/acme/not-a-repo").git_branch, None);
    }

    #[test]
    fn tick_picks_up_a_checkout_on_the_throttled_cadence() {
        let workspace = repo("main");
        let mut app = test_app(&workspace);
        std::fs::write(
            std::path::Path::new(&workspace).join(".git/HEAD"),
            "ref: refs/heads/feature/next\n",
        )
        .unwrap();

        // Inside the window: no re-read, and no repaint either.
        app.git_check_at = Instant::now();
        app.needs_redraw = false;
        app.tick();
        assert_eq!(
            app.git_branch.as_deref(),
            Some("main"),
            "the throttled tick must not re-read HEAD"
        );
        assert!(!app.needs_redraw);

        // Past it, the cap label follows the checkout.
        app.git_check_at = Instant::now() - GIT_CHECK_INTERVAL;
        app.needs_redraw = false;
        app.tick();
        assert_eq!(app.git_branch.as_deref(), Some("feature/next"));
        assert!(app.needs_redraw, "a branch change repaints the cap");
    }
}
