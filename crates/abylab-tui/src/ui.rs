//! Rendering: scrollback, tips row, status bar, prompt, hints, overlays.

use std::time::Instant;

use ratatui::layout::{Constraint, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, FrameExt, HighlightSpacing, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState,
};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, RunState};
use crate::theme::Theme;
use crate::transcript::wrap;

/// The `↥` prompt-jump glyph's hit width: the glyph cell plus the margin cell
/// left of it (Martty's two-cell button).
const PROMPT_JUMP_BTN_W: u16 = 2;
/// The `⛶` expand glyph's hit width: a margin, the glyph, and one more cell so
/// the corner `╮` keeps its room (Martty's issue #92 button).
const EXPAND_BTN_W: u16 = 3;

/// Composer card height for a terminal `height` rows tall.
/// Composer height: the input well plus one bottom meta row (state ·
/// mode/permission chips · model). Taller terminals get a taller well — two
/// rows more than the old 4/3/2 ladder, so a long prompt or a multi-line draft
/// can be read without scrolling the well.
fn composer_height(height: u16) -> u16 {
    if height >= 15 {
        6
    } else if height >= 10 {
        5
    } else {
        4
    }
}

/// Grow with hard/soft-wrapped draft rows, while leaving at least half of a
/// normal terminal to the conversation. Beyond the cap, `draw_input` keeps a
/// cursor-following viewport inside the composer. The mouse-only expand button
/// (`⛶`) pins the well to the amplified height instead.
fn resolved_composer_height(area: Rect, app: &App) -> u16 {
    let minimum = composer_height(area.height);
    let inner_width = area.width.saturating_sub(2);
    let prompt_width = "❯ ".width() as u16;
    let wrap_width = inner_width.saturating_sub(prompt_width).max(1) as usize;
    // Expanded: up to 5/8 of the frame and no compact cap — the conversation
    // keeps the rest. Auto: half the screen, ≤ 14.
    let maximum = if app.composer_expanded {
        (area.height * 5 / 8)
            .max(minimum)
            .min(area.height.saturating_sub(4).max(minimum))
    } else {
        (area.height / 2).max(minimum).min(14)
    };
    let desired = if app.composer_expanded {
        maximum
    } else {
        app.input
            .visual_row_count(wrap_width)
            .saturating_add(1)
            .min(maximum as usize) as u16
    };
    desired.max(minimum).min(maximum)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.theme;
    // The soft caret's screen cell is rebuilt every frame; a frame without
    // a painted caret (overlay owns input) leaves it `None` and `main`
    // parks the hidden hardware cursor nowhere.
    app.caret_cell = None;
    // Same for the cap row's mouse-only hit targets: only a frame that draws
    // them may leave a target behind.
    app.plan_chip = None;
    app.prompt_jump_btn = None;
    app.expand_btn = None;
    f.render_widget(
        Block::default().style(Style::default().bg(theme.bg).fg(theme.fg)),
        area,
    );
    if area.height < 6 || area.width < 24 {
        f.render_widget(
            Paragraph::new("terminal too small — need ≥ 24x6")
                .style(Style::default().fg(theme.warn)),
            area,
        );
        return;
    }

    let main = area;

    // Composer card: one rounded box wrapping the cap row (dock / tip /
    // · workspace title) and the native input surface (input well on top,
    // one meta row — run state + mode/permission chips + model — at the
    // bottom). The old shortcut-hints row is gone (the tip banner and
    // /keys carry that).
    let child_view = app.active_subagent.is_some();
    let agents_h = if app.subagents.is_empty() { 0 } else { 1 };
    // Keep the conversation visually detached from the composer chrome.
    // This row is intentionally left untouched so the canvas/background
    // shows through instead of becoming another panel-colored separator.
    let gap_h = if child_view { 0 } else { 1 };
    // Exactly one cap row tops the box (the `╭ … ─╮` border line) and the
    // tip line carries it.
    let cap_h = if !child_view && main.height >= 16 {
        1
    } else {
        0
    };
    // The box's top border is the cap row and its bottom border carries
    // the meta row, so the box costs no extra rows — the input well stays
    // exactly as tall as the borderless layout.
    let composer_h = if child_view {
        1
    } else {
        resolved_composer_height(main, app)
    };
    // The live counters the dock used to show (tokens, turns, timing) live in
    // `/status` now, so the box's bottom row is the last row of the frame.
    let chat_h = main
        .height
        .saturating_sub(composer_h + cap_h + agents_h + gap_h);

    let chat = Rect::new(main.x, main.y, main.width, chat_h);
    let chrome_y = main.y + chat_h + gap_h;
    let agents = Rect::new(main.x, chrome_y, main.width, agents_h);
    let composer_box = Rect::new(main.x, chrome_y + agents_h, main.width, cap_h + composer_h);
    let composer = Rect::new(main.x, composer_box.y + cap_h, main.width, composer_h);

    draw_chat(f, app, chat);
    if agents_h > 0 {
        draw_agent_rail(f, app, agents);
    }
    if child_view {
        draw_child_navigation(f, app, composer);
    } else if cap_h > 0 {
        draw_composer_box(f, app, composer_box);
    } else {
        draw_composer(f, app, composer);
    }
    if !child_view {
        draw_slash_menu(f, app, composer, chat);
        // The @file browser rides the same anchor; drawn after the slash
        // menu (the two are mutually exclusive, but the browser owns the
        // space above the composer either way).
        draw_file_menu(f, app, composer, chat);
        // Hover/cursor preview for inline [image n] chips sits above the
        // composer (drawn last so it tops the menu-free chat area).
        draw_attachment_preview(f, app, composer, area);
    }
    // Every modal card (pickers, `/keys`, `/help`, the todo dialog, the ACP
    // permission ask) floats over the *conversation*: the band stops at the
    // composer's top row, so the tip line and the draft well always stay
    // readable — a long card scrolls instead of covering them. The floor
    // keeps a card on absurdly short terminals, where the composer leaves
    // nothing.
    let cards = Rect::new(
        area.x,
        area.y,
        area.width,
        composer_box
            .y
            .saturating_sub(area.y)
            .max(DIALOG_MIN_H + DIALOG_MARGIN_Y * 2)
            .min(area.height),
    );
    draw_model_picker(f, app, cards);
    draw_view_overlay(f, app, cards);
    draw_todo_dialog(f, app, cards);
    draw_permission_ask(f, app, cards);
}

/// Outer breathing room for the modal cards (pickers, `/keys`, `/help`, the
/// todo dialog, the permission ask): they float over the chat instead of
/// kissing the screen edge. Margins give way first on tiny terminals — the
/// card keeps its readable minimum, so the layout below never shrinks past
/// it. `screen` is already the band above the composer (`draw`), so the
/// bottom margin is measured from the composer's cap row.
const DIALOG_MARGIN_X: u16 = 4;
const DIALOG_MARGIN_Y: u16 = 2;
/// The card's own minimum size (border included); the margins above never
/// eat into it.
const DIALOG_MIN_W: u16 = 24;
const DIALOG_MIN_H: u16 = 4;

/// The screen inset by [`DIALOG_MARGIN_X`] / [`DIALOG_MARGIN_Y`] — the box
/// the modal review panes lay themselves out inside.
fn dialog_area(screen: Rect) -> Rect {
    let dx = DIALOG_MARGIN_X.min(screen.width.saturating_sub(DIALOG_MIN_W) / 2);
    let dy = DIALOG_MARGIN_Y.min(screen.height.saturating_sub(DIALOG_MIN_H) / 2);
    Rect::new(
        screen.x + dx,
        screen.y + dy,
        screen.width.saturating_sub(dx * 2),
        screen.height.saturating_sub(dy * 2),
    )
}

fn draw_view_overlay(f: &mut Frame, app: &mut App, screen: Rect) {
    let theme = app.theme;
    let Some(view) = app.view_overlay.as_mut() else {
        return;
    };
    // Lay the card out inside the inset, keeping its own 2-column gutter.
    let screen = dialog_area(screen);
    // A review pane, not a snackbar: wide terminals get up to 2/3 of the
    // screen (the old 84-column cap made long plans feel cramped).
    let width = screen
        .width
        .saturating_sub(4)
        .min((screen.width.saturating_mul(2) / 3).max(84))
        .max(24);
    let inner_width = width.saturating_sub(2) as usize;
    let lines = crate::slots::render_nodes(&view.nodes, &theme, inner_width);
    let height = (lines.len() as u16 + 2)
        .min(screen.height.saturating_sub(4))
        .max(4);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 3,
        width,
        height,
    );
    // Clamp the scroll to the content: End / wheel overscroll stops at the
    // last content row instead of showing blank space below the review.
    // The stored offset is normalized here too, so scrolling back up starts
    // from the visible bottom (End parks at `usize::MAX` until the next draw).
    let max_scroll = lines
        .len()
        .saturating_sub(area.height.saturating_sub(2) as usize);
    let scroll = view.scroll.min(max_scroll) as u16;
    view.scroll = view.scroll.min(max_scroll);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.brand))
        .title(Span::styled(
            format!(" {} · ↑↓/wheel scroll · esc close ", view.title),
            Style::default().fg(theme.fg),
        ))
        .style(Style::default().bg(theme.panel).fg(theme.fg));
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), area);
}

/// The clickable todo dialog: the task in progress plus every checklist row,
/// with the same scroll/esc affordances as the view overlay — and the same
/// inset (`dialog_area`), so the two cards line up instead of one of them
/// hugging the screen edge. Rendered from `App::plan` on every frame, so a
/// live `todo_write` update refreshes an open dialog in place — and a cleared
/// checklist (`App::plan == None`) draws nothing even if the dialog state
/// lingers.
fn draw_todo_dialog(f: &mut Frame, app: &mut App, screen: Rect) {
    let theme = app.theme;
    let Some(plan) = app.plan.clone() else {
        return;
    };
    if app.todo_dialog.is_none() {
        return;
    }
    let screen = dialog_area(screen);
    let width = screen
        .width
        .saturating_sub(4)
        .min((screen.width.saturating_mul(2) / 3).max(56))
        .max(24);
    let inner_width = width.saturating_sub(2) as usize;
    let lines = todo_dialog_lines(&plan, &theme, inner_width, app.locale);
    let height = (lines.len() as u16 + 2)
        .min(screen.height.saturating_sub(4))
        .max(4);
    let area = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 3,
        width,
        height,
    );
    let max_scroll = lines
        .len()
        .saturating_sub(area.height.saturating_sub(2) as usize);
    if let Some(dialog) = app.todo_dialog.as_mut() {
        dialog.scroll = dialog.scroll.min(max_scroll);
    }
    let scroll = app.todo_dialog.as_ref().map_or(0, |d| d.scroll) as u16;
    f.render_widget(Clear, area);
    let title = format!(
        " {} · {}/{} {} · {} ",
        app.locale.tr("Todo progress", "任务进度"),
        plan.completed,
        plan.total,
        app.locale.tr("done", "完成"),
        app.locale
            .tr("↑↓/wheel scroll · esc close", "↑↓/滚轮 滚动 · esc 关闭"),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.brand))
        .title(Span::styled(title, Style::default().fg(theme.fg)))
        .style(Style::default().bg(theme.panel).fg(theme.fg));
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), area);
}

/// The dialog body: the task in progress, then one line per checklist row —
/// the `✓`/`▶`/`○` status glyphs carry the state without extra chrome. The
/// title already reports completed/total.
fn todo_dialog_lines(
    plan: &crate::events::PlanProgress,
    theme: &Theme,
    width: usize,
    locale: crate::locale::Locale,
) -> Vec<Line<'static>> {
    use crate::events::PlanStatus;

    let mut lines = Vec::new();
    if let Some(active) = &plan.active {
        let extra = if plan.active_extra > 0 {
            format!(" (+{})", plan.active_extra)
        } else {
            String::new()
        };
        lines.extend(indent_wrapped(
            &format!("{active}{extra}"),
            width,
            locale.tr("now: ", "进行中: "),
            Style::default().fg(theme.fg),
            theme.brand,
        ));
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }

    for todo in &plan.todos {
        let (icon, icon_color, text_style) = match todo.status {
            PlanStatus::Completed => ("✓", theme.ok, Style::default().fg(theme.fg_tertiary)),
            PlanStatus::InProgress => (
                "▶",
                theme.brand,
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            PlanStatus::Pending => ("○", theme.caption, Style::default().fg(theme.fg_secondary)),
        };
        for (i, row) in wrap(&todo.content, width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let prefix = if i == 0 {
                format!("{icon} ")
            } else {
                "  ".into()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(icon_color)),
                Span::styled(row, text_style),
            ]));
        }
    }
    lines
}

/// One labeled, wrapped value in a dialog body (continuation rows align under
/// the value, not under the label).
fn indent_wrapped(
    text: &str,
    width: usize,
    label: &str,
    style: Style,
    label_color: Color,
) -> Vec<Line<'static>> {
    let label_width = label.width();
    wrap(text, width.saturating_sub(label_width))
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let prefix = if i == 0 {
                label.to_string()
            } else {
                " ".repeat(label_width)
            };
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(label_color)),
                Span::styled(row, style),
            ])
        })
        .collect()
}

fn draw_child_navigation(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let label = app
        .active_subagent
        .as_deref()
        .and_then(|id| app.subagents.iter().find(|view| view.id == id))
        .map(|view| view.label.as_str())
        .unwrap_or("subagent");
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {label} · read-only"),
                Style::default().fg(theme.fg_secondary),
            ),
            Span::styled(
                "   esc back · ↓ switch agents",
                Style::default().fg(theme.caption),
            ),
        ]))
        .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn draw_agent_rail(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    let mut spans = vec![Span::styled(
        " agents  ",
        Style::default().fg(theme.caption),
    )];
    let main_active = app.active_subagent.is_none();
    spans.push(Span::styled(
        if main_active { "▸ main" } else { "  main" },
        Style::default()
            .fg(if main_active {
                theme.brand
            } else {
                theme.fg_secondary
            })
            .add_modifier(if main_active {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    ));
    for view in &app.subagents {
        let active = app.active_subagent.as_deref() == Some(view.id.as_str());
        let marker = if view.running { '●' } else { '✓' };
        spans.push(Span::styled(
            format!(
                "  {}{marker} {}",
                if active { "▸ " } else { "" },
                view.label
            ),
            Style::default()
                .fg(if view.running { theme.brand } else { theme.ok })
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
    }
    spans.push(Span::styled(
        "  ↓ switch",
        Style::default().fg(theme.caption),
    ));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.surface)),
        area,
    );
}

/// The composer card fallback for short terminals (no cap row fits): a
/// borderless tinted surface (panel bg) that owns the status row and the
/// input well. The amber prompt owns the working indicator (the old
/// brand-blue edge bar is gone, issue #27). Tall enough terminals get
/// `draw_composer_box` instead — the same surface wrapped in the rounded
/// frame that also carries the cap row.
fn draw_composer(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;

    // Surface fill: contrast against the chat bg does the framing.
    f.render_widget(
        Block::default().style(Style::default().bg(theme.panel)),
        area,
    );

    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    if inner.width == 0 || inner.height < 2 {
        return;
    }

    // Draft first: the input well fills everything above the meta row.
    // Inline [image n] chips live in the draft text; draw_input restyles
    // them and records their rects for hover/preview hit-tests.
    app.att_chips.clear();
    app.att_thumbs.clear();
    let well = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    app.composer_wrap_width = inner.width.saturating_sub("❯ ".width() as u16).max(1) as usize;
    draw_input(f, app, well);
    draw_meta_row(
        f,
        app,
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

/// Meta row: run state + mode/permission chips left, model right,
/// collision-aware. The boxed layout renders this line on the bottom
/// border (`title_bottom`); the borderless fallback draws it as an inner
/// row.
fn meta_line(app: &App, width: usize) -> Line<'static> {
    let theme = app.theme;
    let left = status_title(app);
    let mut right_spans = status_right(app);
    let lw = left.width();
    let rw: usize = right_spans.iter().map(|s| s.content.width()).sum();
    if lw + rw + 2 > width {
        // Drop the contextual hints first, but keep the scroll position
        // beside the model so a longer left-side mode label never hides it.
        let shown_model = app
            .transcript
            .last_model
            .clone()
            .unwrap_or_else(|| app.cfg.model.clone());
        let mut compact = Vec::new();
        if app.scroll_up > 0 {
            compact.push(Span::styled(
                format!("▲{} · ", app.scroll_up),
                Style::default().fg(theme.caption),
            ));
        }
        compact.push(Span::styled(
            format!("{shown_model} "),
            Style::default().fg(theme.fg_tertiary),
        ));
        let compact_width = span_widths(&compact);
        if lw + compact_width + 2 <= width {
            right_spans = compact;
        } else if lw + shown_model.width() + 3 <= width {
            right_spans = vec![Span::styled(
                format!("{shown_model} "),
                Style::default().fg(theme.fg_tertiary),
            )];
        } else {
            right_spans = Vec::new();
        }
    }
    let rw: usize = right_spans.iter().map(|s| s.content.width()).sum();
    let mut spans = left.spans;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(lw + rw))));
    spans.extend(right_spans);
    Line::from(spans)
}

fn draw_meta_row(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(Paragraph::new(meta_line(app, area.width as usize)), area);
}

/// Whether `draw_chat` will append the live work row to the transcript this
/// frame — the `↥` jump's scroll math must count exactly the lines it draws.
pub(crate) fn state_line_shown(app: &App) -> bool {
    state_line(app).is_some()
}

/// The active run-state line — rendered as the transcript's always-last line
/// while work is in progress. Quiet sessions need no redundant `● idle` row.
fn state_line(app: &App) -> Option<Line<'static>> {
    let theme = app.theme;
    let mut spans: Vec<Span> = Vec::new();
    let active_child = app
        .active_subagent
        .as_deref()
        .and_then(|id| app.subagents.iter().find(|view| view.id == id));
    let transcript = app.displayed_transcript();
    let state = active_child
        .map(|view| {
            if view.running {
                RunState::Running
            } else {
                RunState::Idle
            }
        })
        .unwrap_or(app.state);
    // Open reasoning/assistant cells already own the live presentation
    // (`thinking…` or the streaming cursor). Adding another `streaming` row
    // made the same phase alternate between one and two status rows.
    if state == RunState::Running && transcript.streaming() {
        return None;
    }
    match state {
        RunState::Idle => return None,
        RunState::Starting | RunState::Running => {
            spans.push(Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(theme.brand),
            ));
            let label = if state == RunState::Starting {
                if app.state_note.is_empty() {
                    app.locale.tr("starting", "启动中").to_string()
                } else {
                    app.state_note.clone()
                }
            } else if !app.state_note.is_empty() {
                app.state_note.clone()
            } else {
                app.locale.tr("working", "工作中").to_string()
            };
            spans.push(Span::styled(label, Style::default().fg(theme.brand_soft)));
            if active_child.is_none() {
                if let Some(t0) = app.run_started {
                    spans.push(Span::styled(
                        format!(" {}s", t0.elapsed().as_secs()),
                        Style::default().fg(theme.caption),
                    ));
                }
            }
            if active_child.is_none() && app.queued > 0 {
                spans.push(Span::styled(
                    format!(" · {} {}", app.queued, app.locale.tr("queued", "条排队中")),
                    Style::default().fg(theme.warn_soft()),
                ));
            }
        }
    }
    Some(Line::from(spans))
}

/// Meta row, left side: the session's mode chips only (run state lives at
/// the transcript tail). Chips lead with a plain dot instead of emoji —
/// the color carries the meaning (permission turns warn under full access).
fn status_title(app: &App) -> Line<'static> {
    if !app.session_bound {
        return Line::default();
    }
    let theme = app.theme;
    let mut spans: Vec<Span> = Vec::new();
    // The label stands alone: the `shift+tab` hint that used to follow it is
    // gone (the `/help` card and `/permission` still document the binding).
    // Mode chips: folded from the durable event stream (same facts as the
    // Web UI chips). Stock defaults render until the host reports its own
    // facts, so the landing screen still advertises the permission preset.
    let perm = app
        .modes
        .permission
        .clone()
        .or_else(|| app.modes.sandbox.clone())
        .unwrap_or_else(|| app.current_permission().to_string());
    let label = if app.locale == crate::locale::Locale::Zh {
        match perm.as_str() {
            "read-only" => "只读".to_string(),
            "workspace-write" => "工作区可写".to_string(),
            "danger-full-access" => "完全访问".to_string(),
            _ => crate::app::permission_label(&perm),
        }
    } else {
        crate::app::permission_label(&perm)
    };
    spans.push(Span::styled(
        format!("· {label}"),
        Style::default().fg(if perm == "danger-full-access" {
            theme.warn_soft()
        } else {
            theme.fg_tertiary
        }),
    ));
    if let Some(approval) = &app.modes.approval {
        spans.push(Span::styled(
            format!(" · {approval}"),
            Style::default().fg(theme.fg_tertiary),
        ));
    }
    if app.modes.plan {
        spans.push(Span::styled(
            " · plan".to_string(),
            Style::default().fg(theme.brand_soft),
        ));
    }
    Line::from(spans)
}

/// Contextual shortcut hints — a tiny state machine over (run state ×
/// draft): what Enter does *right now*, how to interrupt, how to steer
/// immediately. Idle+empty falls back to the `^K keys` discovery hint.
fn context_hints(app: &App) -> Vec<Span<'static>> {
    let theme = app.theme;
    let key = Style::default()
        .fg(theme.fg_tertiary)
        .add_modifier(Modifier::BOLD);
    let lbl = Style::default().fg(theme.caption);
    let running = !matches!(app.state, RunState::Idle);
    let pairs: Vec<(&str, &str)> = match (running, app.input.is_empty()) {
        // Working, nothing typed: the only move is stopping it.
        (true, true) => vec![("esc", app.locale.tr("interrupt", "中断"))],
        // Working with a draft: enter queues; ctrl+⏎ steers without
        // cancellation (ctrl+x cuts the selection instead).
        (true, false) => vec![
            ("⏎", app.locale.tr("queue", "排队")),
            ("ctrl+⏎", "steer"),
            ("esc", app.locale.tr("interrupt", "中断")),
        ],
        // Idle, empty: nothing to hint at. The `^K keys` discovery chip that
        // used to sit here crowded the model id for no new information.
        (false, true) => Vec::new(),
        // Idle with a draft: enter's meaning follows the prefix.
        (false, false) if app.input.buf().starts_with('/') => {
            vec![("⏎", app.locale.tr("command", "命令"))]
        }
        (false, false) => vec![("⏎", app.locale.tr("send", "发送"))],
    };
    // No hints (idle, empty draft): return nothing at all — the model id
    // follows, and a bare ` · ` separator would read as a stray bullet.
    if pairs.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for (i, (k, l)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " · ".to_string(),
                Style::default().fg(theme.border),
            ));
        }
        spans.push(Span::styled(k.to_string(), key));
        spans.push(Span::styled(format!(" {l}"), lbl));
    }
    spans.push(Span::styled(
        " · ".to_string(),
        Style::default().fg(theme.border),
    ));
    spans
}

/// Meta row, right side: contextual shortcut hints, the vim mode chip, the
/// model id and the requested reasoning effort — plain chrome tones, no accent.
/// Token flow and the session identity live in `/status`.
fn status_right(app: &App) -> Vec<Span<'static>> {
    let theme = app.theme;
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    // `/vim` is modal, so the row has to say which mode the keys are in:
    // normal mode swallows the letters a reader would expect to type.
    if app.vim.is_active() {
        let (label, color) = match app.vim.mode {
            crate::input::VimMode::Insert => ("-- INSERT --", theme.caption),
            _ => ("-- NORMAL --", theme.brand),
        };
        spans.push(Span::styled(
            format!("{label} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    if app.scroll_up > 0 {
        spans.push(Span::styled(
            format!("▲{} · ", app.scroll_up),
            Style::default().fg(theme.caption),
        ));
    }
    spans.extend(context_hints(app));
    if app.session_bound {
        // Chip precedence: an explicit /model pick (until a turn realizes it)
        // → the model that actually streamed last → the configured default.
        let shown_model = app
            .selected_model
            .clone()
            .or_else(|| app.transcript.last_model.clone())
            .unwrap_or_else(|| app.cfg.model.clone());
        spans.push(Span::styled(
            shown_model,
            Style::default().fg(theme.fg_tertiary),
        ));
        if let Some(effort) = &app.modes.effort {
            spans.push(Span::styled(
                format!(" · {effort}"),
                Style::default().fg(theme.caption),
            ));
        }
    }
    spans.push(Span::raw(" "));
    spans
}

fn span_widths(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let mut owners: Vec<Option<usize>> = Vec::new();
    let thumbs = crate::pet::kitty_supported();
    let layout = app
        .displayed_transcript()
        .layout(&theme, inner.width, app.spinner(), thumbs);
    let users = layout.users;
    let mut lines = layout.lines;
    owners.extend(layout.owners);
    // Active work rides as the transcript's last line — hugging the newest
    // message (no separator; it scrolls with the list). Idle draws nothing.
    if let Some(line) = state_line(app) {
        lines.push(line);
        owners.push(None);
    }

    let total = lines.len();
    let h = inner.height as usize;
    let max_scroll = total.saturating_sub(h);
    if app.scroll_up > max_scroll {
        app.scroll_up = max_scroll;
    }
    let end = total - app.scroll_up.min(total);
    let start = end.saturating_sub(h);
    let visible: Vec<Line> = lines[start..end].to_vec();

    // Layout snapshot for mouse selection: hit-testing and copy extraction
    // read exactly what this frame showed (grok-build's resolved selection
    // model, scaled down to plain text per wrapped line).
    // Layout snapshot for mouse selection: hit-testing and copy extraction
    // read exactly what this frame showed (grok-build's resolved selection
    // model, scaled down to plain text per wrapped line). Viewport-sized
    // only: the absolute→relative seam lives in `ChatView::line_text`/
    // `line_owner`; `total` keeps scroll math whole.
    app.chat_view.area = inner;
    app.chat_view.top = start;
    app.chat_view.total = total;
    app.chat_view.lines = lines[start..end]
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    app.chat_view.owners = owners[start..end].to_vec();
    // The ↥ jump flash (issue #103): resolve the flashing transcript cell to
    // its current line span every frame — streaming can move the prompt — and
    // drop it once the few-second window expired.
    app.prompt_flash_lines = app
        .prompt_flash
        .filter(|(_, until)| Instant::now() < *until)
        .and_then(|(cell, _)| users.iter().find(|p| p.cell == cell))
        .map(|prompt| (prompt.line, prompt.end));

    // Visible image thumbnails → screen rects (partially visible ones clip
    // to the pane).
    app.chat_view.images = layout
        .images
        .iter()
        .filter_map(|shot| {
            let rel = shot.line as isize - start as isize;
            if rel < 0 || rel as usize >= h {
                return None;
            }
            let top = inner.y + rel as u16;
            let rows = (shot.rows as u16).min(inner.height.saturating_sub(rel as u16));
            if rows == 0 {
                return None;
            }
            Some(crate::app::ThumbPlacement {
                id: shot.id,
                rect: ratatui::layout::Rect::new(inner.x, top, shot.cols as u16, rows),
                data: shot.data.clone(),
            })
        })
        .collect();

    f.render_widget(Paragraph::new(visible), inner);
    // The transient ↥ jump wash paints under an active copy-selection so the
    // user's own highlight always wins.
    draw_prompt_flash(f, app, inner, start);
    draw_selection_overlay(f, app, inner, start);
}

/// The transient `↥` jump highlight: right after a jump the jumped prompt's
/// rows are washed with the chip background and the brand tone for a few
/// seconds, then the ordinary bubble look returns (the expiry lives in
/// `App::tick`, which clears the state and requests a repaint).
fn draw_prompt_flash(f: &mut Frame, app: &App, inner: Rect, start: usize) {
    let Some((s, e)) = app.prompt_flash_lines else {
        return;
    };
    let theme = app.theme;
    let buf = f.buffer_mut();
    for r in 0..inner.height {
        let li = start + r as usize;
        if li < s || li >= e {
            continue;
        }
        let Some(text) = app.chat_view.line_text(li) else {
            continue;
        };
        let lw = text.trim_end().width();
        if lw == 0 {
            continue;
        }
        for c in 0..lw.min(inner.width as usize) {
            if let Some(cell) = buf.cell_mut((inner.x + c as u16, inner.y + r)) {
                let style = cell
                    .style()
                    .patch(Style::default().fg(theme.brand).bg(theme.chip_bg));
                cell.set_style(style);
            }
        }
    }
}

/// Paint the in-app mouse selection as reversed cells — the live highlight
/// for grok-style drag-select-copy. Reversal is theme-agnostic and reads
/// like a native terminal selection.
fn draw_selection_overlay(f: &mut Frame, app: &App, inner: Rect, start: usize) {
    let Some(sel) = app.sel else { return };
    let (s, e) = sel.ordered();
    let buf = f.buffer_mut();
    for r in 0..inner.height {
        let li = start + r as usize;
        if li < s.line || li > e.line {
            continue;
        }
        let Some(text) = app.chat_view.line_text(li) else {
            continue;
        };
        let lw = text.trim_end().width();
        let c0 = if li == s.line { s.col } else { 0 };
        let mut c1 = (if li == e.line { e.col + 1 } else { lw }).min(lw);
        if c1 <= c0 {
            if c0 == 0 {
                c1 = 1; // 1-cell sliver keeps empty rows visually continuous
            } else {
                continue;
            }
        }
        for c in c0..c1.min(inner.width as usize) {
            if let Some(cell) = buf.cell_mut((inner.x + c as u16, inner.y + r)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

/// The composer cap row as drawn, plus the cell range of its clickable chip
/// (offset and width from the line's first cell; only the todo line has one).
struct CapLine {
    line: Line<'static>,
    chip: Option<(u16, u16)>,
}

/// Priority: transient action feedback (a few seconds) → the live todo
/// checklist. The rotating usage hints that used to fall through here are
/// gone — a new session greets with one in the timeline instead
/// (`App::push_session_tip`) — so an idle cap line stays empty and only the
/// workspace title on the right marks the row.
fn cap_line(app: &App) -> CapLine {
    let theme = app.theme;
    if let Some((text, _)) = &app.tip {
        // Action feedback reads brighter than the other cap states.
        return CapLine {
            line: Line::from(vec![
                Span::raw(" "),
                Span::styled(text.clone(), Style::default().fg(theme.fg)),
                Span::raw(" "),
            ]),
            chip: None,
        };
    }
    if let Some(plan) = &app.plan {
        return plan_line(app, plan);
    }

    CapLine {
        line: Line::default(),
        chip: None,
    }
}

/// The live todo checklist in the cap row: the task in progress (plus any
/// concurrent ones) and the completed/total chip — the clickable button that
/// opens the progress dialog. Wording follows the active locale; an empty
/// checklist never reaches here (`App::plan` is cleared instead).
fn plan_line(app: &App, plan: &crate::events::PlanProgress) -> CapLine {
    let theme = app.theme;
    let mut spans: Vec<Span> = vec![
        Span::raw(" "),
        Span::styled(
            app.locale.tr("Todo", "任务").to_string(),
            Style::default()
                .fg(theme.brand_soft)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(active) = &plan.active {
        let extra = if plan.active_extra > 0 {
            format!(" (+{})", plan.active_extra)
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!(" · {}{active}{extra}", app.locale.tr("now: ", "进行中: ")),
            Style::default().fg(theme.fg),
        ));
    }
    spans.push(Span::styled(
        " · ".to_string(),
        Style::default().fg(theme.hint),
    ));
    let chip_offset = span_widths(&spans) as u16;
    // Hover is the only affordance a terminal has: the chip brightens to the
    // brand accent while the pointer rests on it.
    let chip_style = if app.hover_plan_chip {
        Style::default()
            .fg(theme.brand)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(theme.hint)
    };
    let chip = Span::styled(
        format!(
            "{}/{} {}",
            plan.completed,
            plan.total,
            app.locale.tr("done", "完成")
        ),
        chip_style,
    );
    let chip_width = chip.content.width() as u16;
    spans.push(chip);
    spans.push(Span::raw(" "));

    CapLine {
        line: Line::from(spans),
        chip: Some((chip_offset, chip_width)),
    }
}

fn compact_workspace(path: &str, max_width: usize) -> String {
    let path = shorten_home(path);
    if path.width() <= max_width {
        return path;
    }
    let leaf = path
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path.as_str());
    let leaf_path = format!("…/{leaf}");
    if leaf_path.width() <= max_width {
        return leaf_path;
    }
    if max_width <= 1 {
        return "…".into();
    }
    let mut suffix = String::new();
    let mut used = 0;
    for ch in leaf.chars().rev() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > max_width - 1 {
            break;
        }
        suffix.insert(0, ch);
        used += width;
    }
    format!("…{suffix}")
}

/// Parse the current branch from the workspace's `.git/HEAD` without
/// spawning git (":branch" rides after the project path in the composer cap,
/// colon-tight — Martty's cap label). `ref: refs/heads/<branch>` →
/// Some(branch); a raw commit hash (detached HEAD), a missing `.git`, or a
/// missing HEAD → None. Handles worktrees and submodules whose `.git` is a
/// `gitdir:` pointer file; relative pointers resolve against the `.git`
/// file's parent. Git itself is never invoked.
pub fn head_branch(workspace: &str) -> Option<String> {
    let dot_git = std::path::Path::new(workspace).join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let p = std::path::Path::new(pointer.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            dot_git.parent()?.join(p)
        }
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.strip_prefix("ref: refs/heads/")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The cap row's right side: the project path with the `:branch` suffix, plus
/// the mouse-only `⛶` expand button (Martty's issue #92) and, one cell left of
/// it, the `↥` user prompt jump glyph (issue #103). Both keep one cell of
/// margin from the corner; hovering brightens them to the strongest
/// foreground.
fn workspace_cap_title(app: &App, area_width: usize) -> Line<'static> {
    let title_width = (area_width / 2).clamp(8, 64);
    let path_width = title_width.saturating_sub(4);
    let tone = if app.hover_prompt_jump_btn {
        app.theme.fg
    } else {
        app.theme.caption
    };
    let expand_tone = if app.hover_expand_btn {
        app.theme.fg
    } else {
        app.theme.caption
    };
    let mut text = format!(" · {} ", compact_workspace(&app.cfg.workspace, path_width));
    // The git branch follows the project path, colon-tight ("path:branch"),
    // only while both still fit the cap budget — a narrow terminal (or a
    // long path) keeps just the path.
    if let Some(branch) = &app.git_branch {
        let branch_tag = format!(":{branch} ");
        if text.width() - 1 + branch_tag.width() <= title_width {
            text.pop(); // drop the space between the path and the colon
            text.push_str(&branch_tag);
        }
    }
    Line::from(vec![
        Span::styled(text, Style::default().fg(app.theme.caption)),
        Span::raw(" "),
        Span::styled("↥", Style::default().fg(tone)),
        Span::raw(" "),
        Span::styled("⛶", Style::default().fg(expand_tone)),
        Span::raw(" "),
    ])
    .right_aligned()
}

fn ellipsize_line(line: Line<'static>, max_width: usize, style: Style) -> Line<'static> {
    if span_widths(&line.spans) <= max_width {
        return line;
    }
    if max_width == 0 {
        return Line::default();
    }
    let mut spans = Vec::new();
    let mut remaining = max_width - 1;
    for span in line.spans {
        if remaining == 0 {
            break;
        }
        let mut text = String::new();
        for ch in span.content.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width > remaining {
                break;
            }
            text.push(ch);
            remaining -= width;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    spans.push(Span::styled("…", style));
    Line::from(spans)
}

/// The composer card as one rounded box: the cap row doubles as the top
/// border (the tip line, plus the right-aligned · workspace title) and the
/// meta row rides the bottom border — the input well owns every inner row.
/// The amber prompt owns the working indicator (the old brand glow that
/// replaced the left border is gone, issue #27).
fn draw_composer_box(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let workspace = workspace_cap_title(app, area.width as usize);
    let workspace_width = span_widths(&workspace.spans);
    let title_budget = (area.width as usize).saturating_sub(2 + workspace_width + 1);
    let cap = cap_line(app);
    // The todo progress chip stays clickable as long as any of it survives
    // the ellipsis; the block paints titles starting inside the left border.
    app.plan_chip = cap.chip.and_then(|(offset, width)| {
        let start = (offset as usize).min(title_budget);
        let end = (offset as usize)
            .saturating_add(width as usize)
            .min(title_budget);
        (end > start).then(|| Rect::new(area.x + 1 + start as u16, area.y, (end - start) as u16, 1))
    });
    let title = ellipsize_line(cap.line, title_budget, Style::default().fg(theme.caption));
    // Both cap-row glyphs ride the right-aligned workspace title with a
    // trailing space before the corner, so their cells are fixed: `⛶` two
    // cells left of the corner, `↥` two cells left of that. Each hit target
    // adds the margin cell on its left (Martty's two-cell buttons).
    if area.width > EXPAND_BTN_W + PROMPT_JUMP_BTN_W + 4 {
        app.expand_btn = Some(Rect::new(
            area.x + area.width - EXPAND_BTN_W - 1,
            area.y,
            EXPAND_BTN_W,
            1,
        ));
        app.prompt_jump_btn = Some(Rect::new(
            area.x + area.width - EXPAND_BTN_W - PROMPT_JUMP_BTN_W - 1,
            area.y,
            PROMPT_JUMP_BTN_W,
            1,
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .title(title)
        .title(workspace)
        .title_bottom(meta_line(app, area.width.saturating_sub(2) as usize))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 2 {
        return;
    }

    // Running state is painted by the prompt tint alone — no edge bar
    // replaces the left border (issue #27).

    // Draft first: the well owns every inner row — the meta row lives on
    // the bottom border.
    app.att_chips.clear();
    app.att_thumbs.clear();
    app.composer_wrap_width = inner.width.saturating_sub("❯ ".width() as u16).max(1) as usize;
    draw_input(f, app, inner);
}

/// grok-style hover preview: when the pointer rests on an inline chip (or
/// the text cursor sits in one), pop a card above the composer with a
/// kitty thumbnail (PNG + pixel terminals) and basic metadata.
fn draw_attachment_preview(f: &mut Frame, app: &mut App, composer: Rect, screen: Rect) {
    let Some(idx) = app.preview_att() else {
        return;
    };
    let Some(att) = app.pending_images.get(idx) else {
        return;
    };
    let theme = app.theme;
    let dims = crate::pet::image_dims(&att.data);
    let pixels = app.kitty_pixels && att.is_png();
    let thumb_h: u16 = if pixels { 5 } else { 0 };
    let w: u16 = 36.min(screen.width.saturating_sub(2));
    let h: u16 = thumb_h + 3 + 2; // meta lines + borders
    if screen.height <= h || w < 12 {
        return;
    }
    // Anchor near the chip, clamped on-screen, sitting above the composer.
    let anchor_x = app
        .att_chips
        .iter()
        .find(|(_, i)| *i == idx)
        .map(|(r, _)| r.x)
        .unwrap_or(composer.x + 2);
    let x = anchor_x.min(screen.x + screen.width - w);
    let y = composer.y.saturating_sub(h);
    let area = Rect::new(x, y, w, h);
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    for _ in 0..thumb_h {
        lines.push(Line::default()); // reserved rows the kitty image covers
    }
    lines.push(Line::from(Span::styled(
        att.name.clone(),
        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
    )));
    let dims_txt = dims
        .map(|(iw, ih)| format!("{iw}×{ih} px"))
        .unwrap_or_else(|| "unknown size".into());
    let kb = att.data.len() as f64 / 1024.0;
    let size_txt = if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.0} KB")
    };
    lines.push(Line::from(Span::styled(
        format!("{dims_txt} · {size_txt} · {}", att.media_type),
        Style::default().fg(theme.caption),
    )));
    lines.push(Line::from(Span::styled(
        "⌫ on the chip removes · enter sends".to_string(),
        Style::default().fg(theme.caption),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" {} ", att.token),
            Style::default().fg(theme.brand_soft),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines), inner);
    if pixels && thumb_h > 0 {
        // Aspect-fit the thumbnail into the reserved rows (cells ≈ 1:2).
        let cols = dims
            .filter(|&(_, ih)| ih > 0)
            .map(|(iw, ih)| ((thumb_h as u64 * 2 * iw as u64 + ih as u64 / 2) / ih as u64) as u16)
            .unwrap_or(10)
            .clamp(4, inner.width.max(4));
        app.att_thumbs.push(crate::app::ThumbPlacement {
            id: att.id,
            rect: Rect::new(inner.x, inner.y, cols.min(inner.width), thumb_h),
            data: att.data.clone(),
        });
    }
}

/// The input well. Long prompts wrap across the (now taller) well, and the
/// cursor follows the wrap; a `/` prefix recolors the whole line.
/// Inline `[image n]` tokens render as chips (no icon — the token itself is
/// the chip) and their screen rects land in `app.att_chips` for hover.
///
/// The draft itself is a `ratatui-textarea` widget rendered in the columns
/// right of the prompt; the wrap layout mirror (`ComposerEditor::layout`)
/// drives chip restyling, and the caret cell, and
/// `ComposerEditor::update_scroll_top` keeps the mirrored scroll offset in
/// lockstep with the widget's own viewport.
fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    // Overlays that own input never paint the composer caret.
    let composer_owns_cursor = true;
    // The well's rect is the seam mouse hit-testing reads between frames
    // (placed before the early return: an empty draft is still clickable).
    app.composer_area = area;
    if area.width < 4 || area.height == 0 {
        return;
    }
    let prompt = "❯ ";
    let pw = prompt.width();
    // The prompt carries the working indicator that used to be the brand
    // glow bar: amber while working, brand blue when idle (issue #27).
    let working = !matches!(app.state, RunState::Idle);
    let prompt_style = Style::default()
        .fg(if working { theme.warn } else { theme.brand })
        .add_modifier(Modifier::BOLD);

    if app.input.is_empty() {
        let placeholder = match app.state {
            RunState::Idle => app
                .locale
                .tr("describe what you want to build…", "描述你想构建的内容…")
                .to_string(),
            _ => app
                .locale
                .tr(
                    "queue a follow-up — ctrl+enter steers now",
                    "输入后续消息 — ctrl+enter 立即 steer",
                )
                .to_string(),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prompt.to_string(), prompt_style),
                Span::styled(
                    placeholder.to_string(),
                    Style::default()
                        .fg(theme.caption)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])),
            area,
        );
        if composer_owns_cursor {
            paint_caret(f, area.x + pw as u16, area.y);
            app.caret_cell = Some((area.x + pw as u16, area.y));
        }
        return;
    }

    let buf = app.input.buf();
    let style = if buf.starts_with('/') {
        Style::default().fg(theme.brand_soft)
    } else {
        Style::default().fg(theme.fg)
    };

    let avail = (area.width as usize).saturating_sub(pw).max(1);

    // Prompt column: "❯ " on the first row, blank continuation rows keep
    // the wrapped draft aligned with the first line.
    {
        let b = f.buffer_mut();
        b.set_string(area.x, area.y, prompt, prompt_style);
        for i in 1..area.height {
            b.set_string(area.x, area.y + i, " ".repeat(pw), Style::default());
        }
    }

    // The widget renders into the columns right of the prompt; its wrap
    // width is exactly `avail`. The caret is the widget's own reversed
    // cursor cell: it rides the frame diff like any other cell, so scroll
    // redraws can never blink it (the old hardware cursor was hidden for
    // the whole diff write). While an overlay owns input, the composer
    // shows no caret at all.
    let text_area = Rect::new(
        area.x + pw as u16,
        area.y,
        area.width - pw as u16,
        area.height,
    );
    {
        let ta = app.input.textarea_mut();
        ta.set_style(style);
        ta.set_cursor_style(if composer_owns_cursor {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        });
        f.render_widget(&*ta, text_area);
    }

    // Chip restyling is a buffer patch on top of the widget's own
    // rendering; the layout mirror supplies screen cells per grapheme. The
    // layout borrows `app.input`, so the char spans are computed first.
    let spans = app.token_spans();
    let chip_style = Style::default()
        .fg(theme.bubble_fg)
        .bg(theme.bubble_bg)
        .add_modifier(Modifier::BOLD);
    let top = app.input_top;
    let h = area.height as usize;
    let mut chip_rects: Vec<(Rect, usize)> = Vec::new();
    {
        let layout = app.input.layout(avail);
        let buf = f.buffer_mut();
        for &(cs, ce, idx) in &spans {
            // Contiguous grapheme runs per screen row.
            let mut runs: Vec<(usize, usize, usize)> = Vec::new(); // (row, col0, col1)
            let mut run: Option<(usize, usize, usize)> = None;
            for (row, r) in layout.rows.iter().enumerate() {
                for g in &r.graphemes {
                    if g.start_char < ce && g.end_char > cs {
                        match &mut run {
                            Some((r0, c0, c1)) if *r0 == row => *c1 = g.start_col + g.width,
                            _ => {
                                if let Some((r0, c0, c1)) = run.take() {
                                    runs.push((r0, c0, c1));
                                }
                                run = Some((row, g.start_col, g.start_col + g.width));
                            }
                        }
                    } else if run.is_some() {
                        runs.push(run.take().expect("run"));
                    }
                }
                if let Some((r0, c0, c1)) = run.take() {
                    runs.push((r0, c0, c1));
                }
            }
            for (row, c0, c1) in runs {
                if row < top || row >= top + h {
                    continue;
                }
                let y = area.y + (row - top) as u16;
                for c in c0..c1 {
                    if let Some(cell) = buf.cell_mut((text_area.x + c as u16, y)) {
                        cell.set_style(chip_style);
                    }
                }
                chip_rects.push((
                    Rect::new(text_area.x + c0 as u16, y, (c1 - c0) as u16, 1),
                    idx,
                ));
            }
        }
        // Drag selection: reversed cells over the covered graphemes — the same
        // treatment as the chat pane's highlight. Drawn after the chips so a
        // drag across an inline `[image n]` reads as one selection.
        if let Some((a, b)) = app.input_selection_range() {
            let layout = app.input.layout(avail);
            for (row, r) in layout.rows.iter().enumerate() {
                if row < top || row >= top + h {
                    continue;
                }
                let y = area.y + (row - top) as u16;
                for g in &r.graphemes {
                    if g.start_char < b && g.end_char > a {
                        for c in g.start_col..g.start_col + g.width {
                            if let Some(cell) = buf.cell_mut((text_area.x + c as u16, y)) {
                                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                            }
                        }
                    }
                }
            }
        }
    }
    app.att_chips = chip_rects;

    app.input.update_scroll_top(area.height);
    // `input_top` is the app-side mirror mouse hit-testing reads between
    // frames; keep it in lockstep with the editor's own scroll mirror.
    app.input_top = app.input.scroll_top;
    // Track the painted caret cell so `main` can park the hidden hardware
    // cursor on it after the frame (IME popups anchor there; the frame diff
    // leaves the cursor wherever its last cell write happened). The widget's
    // own screen cursor is authoritative — it is the cell the reversed
    // caret style landed on.
    if composer_owns_cursor {
        let (row, col) = app.input.screen_cursor();
        let top = app.input.scroll_top;
        if row >= top && row - top < area.height as usize {
            app.caret_cell = Some((
                text_area.x.saturating_add(col as u16),
                text_area.y.saturating_add((row - top) as u16),
            ));
        }
    }
}

/// The caret rendered as buffer cells instead of the terminal's hardware
/// cursor: a steady reversed block that moves atomically with the frame
/// diff. The hardware cursor had to be hidden for the whole diff write and
/// re-shown at frame end, so any scroll-heavy redraw (streaming auto-follow,
/// user scrolling) held it off long enough to blink the input caret; as
/// buffer cells it can never flicker or teleport, and the terminal cursor
/// stays hidden for the whole session. Wide graphemes cover both cells.
fn paint_caret(f: &mut Frame, x: u16, y: u16) {
    let buf = f.buffer_mut();
    let style = Style::default().add_modifier(Modifier::REVERSED);
    let Some(cell) = buf.cell_mut((x, y)) else {
        return;
    };
    let wide = UnicodeWidthStr::width(cell.symbol()) > 1;
    cell.set_style(style);
    if wide {
        if let Some(cell) = buf.cell_mut((x + 1, y)) {
            cell.set_style(style);
        }
    }
}

/// Rows of menu items shown at once; the window follows the selection
/// (↑/↓ wrap) and the bottom title shows the position when clipped.
const SLASH_MENU_ROWS: usize = 12;
const FILE_MENU_ROWS: usize = 12;

/// The live `@file` browser panel, anchored above the composer like the
/// slash menu; the explorer's chrome is rebuilt every frame from the app
/// theme so palette packs and locale changes land immediately.
fn draw_file_menu(f: &mut Frame, app: &mut App, input: Rect, chat: Rect) {
    let Some(menu) = &mut app.file_menu else {
        return;
    };
    let theme = app.theme;
    let locale = app.locale;
    let n = menu.explorer().files().len();
    let vis = FILE_MENU_ROWS
        .min(n.max(1))
        .min(chat.height.saturating_sub(2) as usize);
    if vis == 0 {
        return;
    }
    // Wider than the slash menu — entries carry paths; the List truncates
    // long names instead of hard-clipping.
    let h = vis as u16 + 2;
    let w = 72.min(input.width.saturating_sub(2));
    let y = input.y.saturating_sub(h);
    let area = Rect::new(input.x + 2, y, w, h);
    f.render_widget(Clear, area);
    if n == 0 {
        // The live filter left nothing here (and the follow search found
        // nothing): keep the frame up with a no-match hint instead of
        // making the menu vanish mid-typing.
        let title = menu.chrome_title(&app.cfg.workspace);
        let hint = menu.chrome_hint(locale);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.panel))
            .title(Span::styled(title, Style::default().fg(theme.caption)))
            .title_bottom(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.caption),
            )));
        f.render_widget(block, area);
        force_full_rewrite(f, area);
        return;
    }
    // Rebuild the explorer chrome from the app theme every frame: palette
    // packs and locale can switch under us, and the title factories need
    // the current cwd.
    menu.apply_chrome(&theme, locale, &app.cfg.workspace);
    f.render_widget_ref(menu.explorer().widget(), area);
    // Wide (CJK) names scroll through the explorer window and orphan
    // trailing cells on the real screen; force a full rewrite so the diff
    // never trusts a half-erased cell.
    force_full_rewrite(f, area);
}

/// Mark every cell of `area` as always-dirty: overlay content that swaps
/// between list and empty states (or scrolls wide glyphs) must not be
/// diffed against the previous frame's half-erased cells.
fn force_full_rewrite(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    // Overlays compute their rect from screen math that can saturate at the
    // top edge; never index a cell outside the buffer (widgets clip via
    // `intersection`, direct indexing would panic).
    let area = area.intersection(Rect::new(0, 0, buf.area.width, buf.area.height));
    if area.is_empty() {
        return;
    }
    for pos in area.positions() {
        buf[pos].set_diff_option(ratatui::buffer::CellDiffOption::AlwaysUpdate);
    }
}

fn draw_slash_menu(f: &mut Frame, app: &App, input: Rect, chat: Rect) {
    let matches = app.slash_matches();
    if matches.is_empty() {
        return;
    }
    let theme = app.theme;
    let n = matches.len();
    let sel = app.slash_sel.min(n - 1);
    // Cap the popup: tall skill catalogs scroll instead of swallowing the
    // chat pane.
    let vis = SLASH_MENU_ROWS
        .min(n)
        .min(chat.height.saturating_sub(2) as usize);
    if vis == 0 {
        return;
    }
    // Follow-window: selection stays visible, window pinned to the ends.
    let start = if sel < vis { 0 } else { sel + 1 - vis }.min(n - vis);

    // Name column sized to what's listed (padded, never glued to the desc).
    let name_w = matches
        .iter()
        .map(|m| m.usage.width())
        .max()
        .unwrap_or(14)
        .clamp(14, 26);

    let h = vis as u16 + 2;
    let w = 64.min(input.width.saturating_sub(2));
    let y = input.y.saturating_sub(h);
    let area = Rect::new(input.x + 2, y, w, h);
    f.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (i, cmd) in matches.iter().enumerate().skip(start).take(vis) {
        let selected = i == sel;
        let marker = if selected { "▸ " } else { "  " };
        let name_style = if selected {
            Style::default()
                .fg(theme.brand)
                .add_modifier(Modifier::BOLD)
        } else if cmd.skill {
            // Host skills read one shade apart from the builtins — the
            // gray-blue hint tone, not a loud accent.
            Style::default().fg(theme.hint)
        } else {
            Style::default().fg(theme.fg_secondary)
        };
        let desc = if cmd.skill {
            format!("{} · {}", cmd.desc, app.locale.tr("skill", "技能"))
        } else {
            cmd.desc.to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), Style::default().fg(theme.brand)),
            Span::styled(pad_or_ellipsize(&cmd.usage, name_w), name_style),
            Span::styled(format!(" {desc}"), Style::default().fg(theme.caption)),
        ]));
    }
    let title = if matches.iter().any(|m| m.completion.is_some()) {
        app.locale.tr(" options ", " 候选 ")
    } else if matches.iter().any(|m| m.skill) {
        app.locale.tr(" commands · skills ", " 命令 · 技能 ")
    } else {
        app.locale.tr(" commands ", " 命令 ")
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(theme.caption)))
        .style(Style::default().bg(theme.panel));
    if n > vis {
        // Clipped: show position + scroll affordance in the bottom border.
        let above = start > 0;
        let below = start + vis < n;
        let arrows = match (above, below) {
            (true, true) => "↑↓",
            (true, false) => "↑",
            _ => "↓",
        };
        block = block.title_bottom(Span::styled(
            format!(" {}/{n} {arrows} ", sel + 1),
            Style::default().fg(theme.caption),
        ));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Pad `s` to exactly `w` display cells, ellipsizing when longer — keeps
/// the desc column aligned even for long skill names.
fn pad_or_ellipsize(s: &str, w: usize) -> String {
    let sw = s.width();
    if sw <= w {
        return format!("{s}{}", " ".repeat(w - sw));
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > w.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    let ow = out.width();
    format!("{out}{}", " ".repeat(w.saturating_sub(ow)))
}

/// Left-pad to a fixed display width so picker columns line up
/// (`{:<n}` pads by chars, which misaligns CJK labels).
fn pad_to_width(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

fn draw_model_picker(f: &mut Frame, app: &mut App, screen: Rect) {
    let theme = app.theme;
    let Some(picker) = &app.picker else {
        return;
    };
    // Every `/model` · `/permission` · `/resume` · `/theme` popup lays itself
    // out inside the same inset the review panes use, so no picker hugs the
    // screen edge either.
    let screen = dialog_area(screen);
    // The active model is identified by provider + id because multiple
    // coding plans can expose the same upstream model id.
    let kind = picker.kind;
    let title = picker.title.clone();
    let sel = picker.sel;
    let items = picker.items.clone();
    // Current-identity ids, precomputed so the row builder below never
    // borrows `app` (the ListView render needs a mutable picker).
    let current_model = app.cfg.model.clone();
    let current_provider = app.cfg.provider.clone();
    let current_mode = app.current_mode();
    let current_palette = app.active_palette_id.clone();
    let current_permission = app.current_permission().to_string();
    let is_current = move |item: &crate::app::PickerItem| match kind {
        crate::app::PickerKind::Model => {
            item.id == current_model
                && item
                    .provider
                    .as_deref()
                    .is_none_or(|provider| provider == current_provider)
        }
        crate::app::PickerKind::Mode => item.id == current_mode,
        crate::app::PickerKind::Theme => item.id == current_palette,
        crate::app::PickerKind::Permission => item.id == current_permission,
        crate::app::PickerKind::Effort
        | crate::app::PickerKind::Session
        | crate::app::PickerKind::Subagent => false,
    };
    // The popup caps at the screen; `ListView` scrolls the overflow instead
    // of clipping it out of reach.
    let h = (items.len() as u16 + 2).min(screen.height.saturating_sub(2));
    // Fit the widest row (marker + padded label + ✓ + meta); cap to the screen.
    let needed = items
        .iter()
        .map(|item| {
            let label_w = item.label.width() + if is_current(item) { 2 } else { 0 };
            2 + label_w.max(crate::app::PICKER_LABEL_COL) + item.meta.width()
        })
        .max()
        .unwrap_or(0) as u16;
    let cap = screen.width.saturating_sub(4).max(24);
    let overflow = items.len() as u16 + 2 > h;
    // The scrollbar gets its own column inside the popup when rows overflow,
    // so the rounded border never has glyphs drawn over its corners.
    let w = if overflow {
        ((needed + 2).max(58) + 1).min(cap)
    } else {
        (needed + 2).max(58).min(cap)
    };
    let x = screen.x + (screen.width - w) / 2;
    let y = screen.y + (screen.height - h) / 3;
    let area = Rect::new(x, y, w, h);
    f.render_widget(Clear, area);
    let item_count = items.len();
    // Rows as a ratatui Table: the selection column is always reserved
    // (marker `▸ ` on the picked row), the label column is fixed-width so
    // metas line up, and the meta column absorbs the remaining width —
    // row highlight paints the whole line, scrollbar gutter included.
    let mut rows = Vec::with_capacity(item_count);
    for item in &items {
        // The current model/mode gets a ✓ pinned to its label — it survives
        // narrow terminals, unlike a right-edge tag.
        let label = if is_current(item) {
            format!("{} ✓", item.label)
        } else {
            item.label.clone()
        };
        rows.push(Row::new(vec![
            Cell::from(Span::styled(
                pad_to_width(&label, crate::app::PICKER_LABEL_COL),
                Style::default().fg(theme.fg_secondary),
            )),
            Cell::from(Span::styled(
                item.meta.clone(),
                Style::default().fg(theme.caption),
            )),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.brand))
        .title(Span::styled(title, Style::default().fg(theme.caption)))
        .style(Style::default().bg(theme.panel));
    // TableState follows the selection into view (the viewport stays pinned
    // to the ends), matching the old ListView behavior.
    let table = Table::new(
        rows,
        [
            Constraint::Length(crate::app::PICKER_LABEL_COL as u16),
            Constraint::Min(0),
        ],
    )
    .block(block)
    .column_spacing(0)
    .highlight_symbol("▸ ")
    .highlight_spacing(HighlightSpacing::Always)
    .row_highlight_style(
        Style::default()
            .fg(theme.brand)
            .bg(theme.chip_bg)
            .add_modifier(Modifier::BOLD),
    );
    let mut table_state =
        TableState::new().with_selected(Some(sel.min(items.len().saturating_sub(1))));
    f.render_stateful_widget(table, area, &mut table_state);
    if overflow {
        let inner = area.inner(Margin::new(1, 1));
        let mut sb_state = ScrollbarState::new(item_count).position(table_state.offset());
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .style(Style::default().fg(theme.border)),
            inner,
            &mut sb_state,
        );
    }
    // The viewport is the source of truth for what is visible; selection
    // moves (mouse later, programmatic now) land back in the picker.
    if let Some(picker) = &mut app.picker {
        if let Some(selected) = table_state.selected() {
            picker.sel = selected;
        }
        // Page keys jump a screenful: the rows the popup actually shows.
        app.picker_page_rows = h.saturating_sub(2) as usize;
    }
}

fn draw_permission_ask(f: &mut Frame, app: &App, screen: Rect) {
    let Some(ask) = &app.permission_ask else {
        return;
    };
    let theme = app.theme;
    let screen = dialog_area(screen);
    let h = (ask.options.len() as u16 + 2).min(screen.height.saturating_sub(2));
    let needed = ask
        .options
        .iter()
        .map(|opt| 2 + opt.name.width().max(24) + 1 + opt.kind.width())
        .max()
        .unwrap_or(0) as u16;
    let cap = screen.width.saturating_sub(4).max(24);
    let w = (needed + 2).max(58).min(cap);
    let x = screen.x + (screen.width - w) / 2;
    let y = screen.y + (screen.height - h) / 3;
    let area = Rect::new(x, y, w, h);
    f.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (i, opt) in ask.options.iter().enumerate() {
        let selected = i == ask.sel;
        let marker = if selected { "▸ " } else { "  " };
        let reject = opt.kind.starts_with("reject");
        let style = if selected {
            Style::default()
                .fg(if reject { theme.err } else { theme.brand })
                .add_modifier(Modifier::BOLD)
        } else if reject {
            Style::default().fg(theme.err)
        } else {
            Style::default().fg(theme.fg_secondary)
        };
        lines.push(Line::from(vec![
            Span::styled(marker.to_string(), Style::default().fg(theme.brand)),
            Span::styled(format!("{:<24}", opt.name), style),
            Span::styled(opt.kind.clone(), Style::default().fg(theme.caption)),
        ]));
    }
    let title = format!(" approval · {} · enter select · esc cancel ", ask.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warn))
        .title(Span::styled(title, Style::default().fg(theme.caption)))
        .style(Style::default().bg(theme.panel));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn shorten_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Render one frame into a plain string (test renderer).
#[cfg(test)]
pub(crate) fn dump_frame(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| draw(f, app)).expect("draw frame");
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for row in 0..buffer.area.height {
        let mut line = String::new();
        for col in 0..buffer.area.width {
            line.push_str(buffer[(col, row)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The composer cap row inside a dumped frame: the box's top border, which
/// carries the left-hand title (feedback / todo) and the workspace on the
/// right. Tests locate it by the corner glyph — the row has no fixed label
/// any more, an idle cap is blank.
#[cfg(test)]
pub(crate) fn cap_row(frame: &str) -> &str {
    frame
        .lines()
        .find(|line| line.contains('╭'))
        .expect("composer cap row")
}

#[allow(dead_code)]
pub fn theme_for(name: &str) -> Theme {
    match name {
        "light" => Theme::light(),
        _ => Theme::dark(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeConfig;
    use std::sync::mpsc;

    /// Unique session root per call — keeps the modes cache from leaking
    /// between tests and runs.
    fn fresh_root() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dsh-tui-ui-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.to_string_lossy().into_owned()
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
        let (_tx, _rx) = mpsc::channel::<crate::bus::AppEvent>();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        app
    }

    fn live_test_app() -> App {
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
        let (_tx, _rx) = mpsc::channel::<crate::bus::AppEvent>();
        let mut app = App::new(Theme::dark(), cfg, "pending".into());
        app.locale = crate::locale::Locale::En;
        app
    }

    #[test]
    fn composer_is_a_rounded_box_surface() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = test_app();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");
        let buf = terminal.backend().buffer().clone();
        let theme = app.theme;
        // 80x20: the box is the last thing on screen — rows 13..19, top border
        // 13 (tip + · workspace), a five-row well, bottom border 19 carrying
        // the meta row (`╰· Standard … ╯`).
        assert_eq!(buf[(0, 13)].symbol(), "╭", "top-left corner");
        assert_eq!(buf[(79, 13)].symbol(), "╮", "top-right corner");
        assert_eq!(buf[(0, 19)].symbol(), "╰", "bottom-left corner");
        assert_eq!(buf[(79, 19)].symbol(), "╯", "bottom-right corner");
        assert_eq!(buf[(4, 13)].bg, theme.panel, "border row on the card");
        assert_eq!(buf[(40, 14)].bg, theme.panel, "input well on panel surface");
        assert_eq!(buf[(40, 18)].bg, theme.panel, "well fills the inner rows");
        assert_eq!(
            buf[(1, 19)].symbol(),
            "·",
            "meta row rides the bottom border with small dots"
        );
        // Five well rows above the meta row: the two extra input rows the
        // composer grew by, with no chrome row sneaking back in below the box.
        for row in 14..=18 {
            assert_eq!(
                buf[(40, row)].bg,
                theme.panel,
                "row {row} belongs to the input well"
            );
        }
        assert_eq!(buf[(4, 12)].bg, theme.bg, "gap row stays plain background");
        assert_eq!(buf[(4, 9)].bg, theme.bg, "chat keeps the base background");
    }

    /// Issue #27: the running state is the amber prompt alone — no brand
    /// `▎` edge bar replaces the composer border.
    #[test]
    fn composer_glow_bar_is_gone_and_the_prompt_tints_while_working() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = test_app();
        app.state = RunState::Idle;
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let theme = app.theme;

        // Idle: brand-blue prompt on a plain rounded border.
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");
        let buf = terminal.backend().buffer().clone();
        let prompt = |buf: &ratatui::buffer::Buffer| {
            buf.content
                .iter()
                .find(|cell| cell.symbol() == "❯")
                .expect("composer prompt")
                .clone()
        };
        assert_eq!(prompt(&buf).fg, theme.brand, "idle prompt stays brand blue");

        // Running: the old glow bar must not paint over the left border,
        // and the prompt carries the working state instead (amber).
        app.state = RunState::Running;
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");
        let buf = terminal.backend().buffer().clone();
        assert!(
            buf.content.iter().all(|cell| cell.symbol() != "▎"),
            "no edge bar may render while running"
        );
        assert_eq!(
            buf[(0, 16)].symbol(),
            "│",
            "the left border column stays a border"
        );
        assert_eq!(buf[(0, 16)].fg, theme.border);
        assert_eq!(prompt(&buf).fg, theme.warn, "working prompt turns amber");

        // Short terminals take the borderless fallback — the bar stayed out
        // of that card too (cap_h is 0 below 16 rows).
        assert!(
            !dump_frame(&mut app, 80, 14).contains("▎"),
            "the fallback card draws no edge bar either"
        );
    }

    #[test]
    fn composer_cap_persistently_shows_the_workspace() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/projects/deepseek-harness-tui-plan-view".into();

        let frame = dump_frame(&mut app, 120, 20);
        let cap = cap_row(&frame);

        assert!(
            cap.contains("· /work/acme/projects/deepseek-harness-tui-plan-view"),
            "{cap}"
        );
    }

    /// The cap reads the branch straight out of `.git/HEAD` — no git process.
    #[test]
    fn head_branch_reads_the_ref_and_skips_non_branches() {
        let root = fresh_root();
        let workspace = std::path::Path::new(&root);
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(head_branch(&root).as_deref(), Some("main"));

        // A detached HEAD holds a raw commit hash, not a branch.
        std::fs::write(workspace.join(".git/HEAD"), "0f1e2d3c4b5a6978\n").unwrap();
        assert_eq!(head_branch(&root), None);
        // An empty ref is not a branch either, and an unwritable/absent
        // `.git` (a plain directory) reads as no repo at all.
        std::fs::write(workspace.join(".git/HEAD"), "ref: refs/heads/\n").unwrap();
        assert_eq!(head_branch(&root), None);
        assert_eq!(head_branch("/work/acme/definitely-not-a-repo"), None);
    }

    /// Worktrees keep the real git dir elsewhere and leave a `gitdir:` pointer
    /// file in place of `.git`; a relative pointer resolves against it.
    #[test]
    fn head_branch_follows_a_worktree_pointer() {
        let root = fresh_root();
        let workspace = std::path::Path::new(&root).join("checkout");
        std::fs::create_dir_all(&workspace).unwrap();
        let git_dir = std::path::Path::new(&root).join("main.git/worktrees/checkout");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/wt/topic\n").unwrap();
        std::fs::write(
            workspace.join(".git"),
            "gitdir: ../main.git/worktrees/checkout\n",
        )
        .unwrap();

        let workspace = workspace.to_string_lossy().into_owned();
        assert_eq!(head_branch(&workspace).as_deref(), Some("wt/topic"));

        // A `.git` file without the `gitdir:` prefix is not a pointer.
        std::fs::write(
            std::path::Path::new(&workspace).join(".git"),
            "../main.git\n",
        )
        .unwrap();
        assert_eq!(head_branch(&workspace), None);
    }

    /// The cap label is `path:branch`, colon-tight (Martty's composer cap).
    #[test]
    fn composer_cap_appends_the_git_branch_colon_tight() {
        let mut app = test_app();
        let workspace = fresh_root();
        std::fs::create_dir_all(std::path::Path::new(&workspace).join(".git")).unwrap();
        std::fs::write(
            std::path::Path::new(&workspace).join(".git/HEAD"),
            "ref: refs/heads/feature/cap\n",
        )
        .unwrap();
        app.cfg.workspace = workspace.clone();
        app.git_branch = head_branch(&workspace);

        let frame = dump_frame(&mut app, 120, 20);
        let cap = cap_row(&frame);

        assert!(cap.contains(":feature/cap"), "{cap}");
        // Colon-tight: no space between the project path and the branch.
        let leaf = workspace.rsplit('/').next().expect("workspace leaf");
        assert!(
            cap.contains(&format!("{leaf}:feature/cap")),
            "the path stays left of the colon: {cap}"
        );
        assert!(
            cap.find('↥').unwrap() > cap.find(":feature/cap").unwrap(),
            "the jump glyph stays right of the branch: {cap}"
        );
    }

    /// A long path (or a narrow terminal) keeps the path alone: the branch
    /// only rides along while both fit the cap budget.
    #[test]
    fn composer_cap_drops_the_branch_when_the_path_owns_the_budget() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/very-long-directory-name/deepseek-harness".into();
        app.git_branch = Some("feature/a-very-long-branch-name".into());

        let frame = dump_frame(&mut app, 60, 20);
        let cap = cap_row(&frame);

        assert!(cap.contains("· …/deepseek-harness"), "{cap}");
        assert!(
            !cap.contains(":feature"),
            "the branch must not crowd out the path: {cap}"
        );
    }

    #[test]
    fn composer_cap_preserves_the_workspace_tail_on_narrow_terminals() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/very-long-directory-name/deepseek-harness".into();

        let frame = dump_frame(&mut app, 60, 20);
        let cap = cap_row(&frame);

        assert!(cap.contains("· …/deepseek-harness"), "{cap}");
    }

    /// The `⛶` expand glyph sits right of the `↥` jump glyph on the cap row
    /// (two cells left of the corner), its hit rect covers it, and a click
    /// pins the well to the amplified height until the next one.
    #[test]
    fn the_expand_glyph_rides_the_cap_row_and_pins_the_well() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/deepseek-harness".into();

        let frame = dump_frame(&mut app, 100, 20);
        let row = frame
            .lines()
            .position(|line| line.contains('⛶'))
            .expect("cap row with the ⛶ glyph");
        let cap = frame.lines().nth(row).unwrap();
        assert!(
            cap.find('⛶').unwrap() > cap.find('↥').unwrap(),
            "⛶ follows ↥: {cap}"
        );
        assert!(
            cap.find('↥').unwrap() > cap.find("deepseek-harness").unwrap(),
            "both glyphs follow the path: {cap}"
        );

        let btn = app.expand_btn.expect("expand button rect");
        assert_eq!(btn.y as usize, row, "the hit rect rides the cap row");
        assert_eq!(btn.right(), 99, "it ends one cell short of the corner");
        let glyph = cap.chars().position(|c| c == '⛶').unwrap() as u16;
        assert!(
            glyph >= btn.x && glyph < btn.x + btn.width,
            "glyph at {glyph} inside {btn:?}"
        );

        // Hover is the only affordance: it brightens, like `↥`.
        let tone = |app: &App| -> Style {
            workspace_cap_title(app, 100)
                .spans
                .iter()
                .find(|span| span.content.contains('⛶'))
                .expect("glyph span")
                .style
        };
        let idle = tone(&app);
        app.hover_expand_btn = true;
        let hovered = tone(&app);
        assert_ne!(idle, hovered, "hover must be visible");
        assert_eq!(hovered.fg, Some(app.theme.fg));

        // Clicking pins the well: 5/8 of the frame, past the auto cap.
        let area = Rect::new(0, 0, 100, 40);
        let auto = resolved_composer_height(area, &app);
        app.composer_expanded = true;
        let pinned = resolved_composer_height(area, &app);
        assert_eq!(pinned, 25, "5/8 of a 40-row frame");
        assert!(pinned > auto, "the click amplifies: {auto} → {pinned}");
    }

    /// The `↥` prompt-jump button rides the cap row right of the project path,
    /// and the recorded hit rect covers the glyph cell.
    #[test]
    fn composer_cap_puts_the_prompt_jump_glyph_after_the_path() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/deepseek-harness".into();

        let frame = dump_frame(&mut app, 100, 20);
        let row = frame
            .lines()
            .position(|line| line.contains('↥'))
            .expect("cap row with the ↥ glyph");
        let cap = frame.lines().nth(row).unwrap();
        assert!(cap.contains("deepseek-harness"), "{cap}");
        assert!(
            cap.find('↥').unwrap() > cap.find("deepseek-harness").unwrap(),
            "the glyph follows the path: {cap}"
        );

        let btn = app.prompt_jump_btn.expect("jump button rect");
        assert_eq!(btn.y as usize, row, "the hit rect rides the cap row");
        let glyph = cap.chars().position(|c| c == '↥').unwrap() as u16;
        assert!(
            glyph >= btn.x && glyph < btn.x + btn.width,
            "glyph at {glyph} inside {btn:?}"
        );
    }

    /// Hover is the only affordance the glyph has: it brightens.
    #[test]
    fn prompt_jump_glyph_brightens_on_hover() {
        let mut app = test_app();
        let tone = |app: &App| -> Style {
            workspace_cap_title(app, 100)
                .spans
                .iter()
                .find(|span| span.content.contains('↥'))
                .expect("glyph span")
                .style
        };
        let idle = tone(&app);
        app.hover_prompt_jump_btn = true;
        let hovered = tone(&app);
        assert_ne!(idle, hovered, "hover must be visible");
        assert_eq!(hovered.fg, Some(app.theme.fg));
    }

    /// A `↥` jump washes the jumped prompt's bubble rows with the chip
    /// background until the flash expires.
    #[test]
    fn prompt_jump_flash_washes_the_jumped_prompt() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::time::Duration;
        let mut app = test_app();
        app.transcript.push_user("first prompt".into(), false);
        app.transcript.push_user("second prompt".into(), false);
        let layout = app.transcript.layout(&app.theme, 78, app.spinner(), false);
        let target = layout.users[0];
        app.prompt_flash = Some((target.cell, Instant::now() + Duration::from_secs(5)));

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");

        assert_eq!(app.prompt_flash_lines, Some((target.line, target.end)));
        let row = target.line - app.chat_view.top;
        let text = app.chat_view.line_text(target.line).expect("visible row");
        let col = text.find("first prompt").expect("prompt text on its row");
        let buf = terminal.backend().buffer();
        let cell = &buf[(
            app.chat_view.area.x + col as u16,
            app.chat_view.area.y + row as u16,
        )];
        assert_eq!(cell.bg, app.theme.chip_bg, "the jumped prompt is washed");
        assert_eq!(cell.fg, app.theme.brand);
    }

    use crate::events::{PlanItem, PlanProgress, PlanStatus};

    /// A checklist whose counts and active task are derived from its rows, the
    /// way the driver derives them from abycore's `PlanView`.
    fn plan(items: &[(&str, PlanStatus)]) -> PlanProgress {
        let todos: Vec<PlanItem> = items
            .iter()
            .map(|(content, status)| PlanItem {
                content: (*content).to_string(),
                status: *status,
            })
            .collect();
        let mut active = None;
        let mut in_progress: usize = 0;
        for todo in &todos {
            if todo.status == PlanStatus::InProgress {
                in_progress += 1;
                if active.is_none() {
                    active = Some(todo.content.clone());
                }
            }
        }
        PlanProgress {
            completed: todos
                .iter()
                .filter(|todo| todo.status == PlanStatus::Completed)
                .count(),
            total: todos.len(),
            active,
            active_extra: in_progress.saturating_sub(1),
            todos,
        }
    }

    /// 2 of 5 done, one task in progress — the shape of a mid-turn checklist.
    fn parser_plan() -> PlanProgress {
        plan(&[
            ("read the driver", PlanStatus::Completed),
            ("patch the parser", PlanStatus::Completed),
            ("wire the cap chip", PlanStatus::InProgress),
            ("run the tests", PlanStatus::Pending),
            ("update the docs", PlanStatus::Pending),
        ])
    }

    /// A live todo checklist takes over the composer cap with the task in
    /// progress plus completed/total.
    #[test]
    fn composer_cap_shows_the_live_todo_checklist() {
        let mut app = test_app();
        app.plan = Some(parser_plan());

        let frame = dump_frame(&mut app, 120, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("Todo"))
            .expect("todo cap line");

        assert!(cap.contains("now: wire the cap chip"), "{cap}");
        assert!(cap.contains("2/5 done"), "{cap}");
        assert!(!cap.contains("Tip"), "{cap}");
    }

    /// Transient action feedback owns the cap row for its TTL, then the todo
    /// checklist reclaims it; with neither, the row goes blank — the usage
    /// hints it used to rotate now greet a new session in the timeline.
    #[test]
    fn composer_cap_prefers_feedback_then_the_todo_checklist() {
        let mut app = test_app();
        app.plan = Some(plan(&[
            ("patch the parser", PlanStatus::Completed),
            ("wire the cap chip", PlanStatus::InProgress),
            ("check the locale", PlanStatus::InProgress),
        ]));
        app.show_tip("copied 5 chars");

        let frame = dump_frame(&mut app, 120, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("copied 5 chars"))
            .expect("feedback cap line");
        assert!(!cap.contains("Todo"), "{cap}");
        assert!(!cap.contains("Tip"), "{cap}");

        app.tip = None;
        let frame = dump_frame(&mut app, 120, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("Todo"))
            .expect("todo cap line");
        assert!(cap.contains("wire the cap chip (+1)"), "{cap}");

        app.plan = None;
        let frame = dump_frame(&mut app, 120, 20);
        let cap = cap_row(&frame);
        assert!(
            !cap.contains("Tip") && !cap.contains("Todo") && !cap.contains("copied"),
            "an idle cap carries no title:\n{cap}"
        );
        assert!(
            cap.contains("/tmp"),
            "the workspace title still marks the row:\n{cap}"
        );
    }

    /// The cap row is localized like the rest of the built-in chrome.
    #[test]
    fn composer_cap_words_the_todo_checklist_in_the_active_locale() {
        let flat = |cap: CapLine| -> String {
            cap.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let mut app = test_app();
        app.locale = crate::locale::Locale::Zh;
        assert_eq!(
            flat(plan_line(
                &app,
                &plan(&[
                    ("调研", PlanStatus::Completed),
                    ("修复登录", PlanStatus::InProgress),
                    ("测试", PlanStatus::Pending),
                ])
            )),
            " 任务 · 进行中: 修复登录 · 1/3 完成 "
        );
        // Nothing in progress: the progress chip still reports the checklist.
        assert_eq!(
            flat(plan_line(
                &app,
                &plan(&[
                    ("调研", PlanStatus::Completed),
                    ("修复登录", PlanStatus::Completed),
                ])
            )),
            " 任务 · 2/2 完成 "
        );
        app.locale = crate::locale::Locale::En;
        assert_eq!(
            flat(plan_line(
                &app,
                &plan(&[
                    ("inspect", PlanStatus::Completed),
                    ("fix login", PlanStatus::InProgress),
                ])
            )),
            " Todo · now: fix login · 1/2 done "
        );
    }

    /// The progress chip is a real button: the frame records its hit rect, and
    /// clicking it opens the dialog while the rest of the cap row stays inert.
    #[test]
    fn todo_progress_chip_opens_the_dialog() {
        use crate::controller::test_controller;
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.plan = Some(parser_plan());
        let (ctl, _commands) = test_controller();

        let frame = dump_frame(&mut app, 120, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("Todo"))
            .expect("todo cap line");
        let chip = app.plan_chip.expect("the cap row records its chip rect");
        assert!(cap.contains("2/5 done"), "{cap}");

        // The recorded rect sits on the cap row, inside the drawn title.
        assert_eq!(
            chip.y as usize,
            frame.lines().position(|l| l.contains("Todo")).unwrap()
        );
        assert!(
            chip.x >= 1 && chip.width >= "2/5 done".len() as u16,
            "{chip:?}"
        );

        app.handle(
            crate::bus::AppEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: chip.x + 1,
                row: chip.y,
                modifiers: KeyModifiers::NONE,
            })),
            &ctl,
        );
        assert!(app.todo_dialog.is_some(), "the chip opens the dialog");

        app.todo_dialog = None;
        app.handle(
            crate::bus::AppEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: chip.y,
                modifiers: KeyModifiers::NONE,
            })),
            &ctl,
        );
        assert!(
            app.todo_dialog.is_none(),
            "the left border is not part of the chip"
        );

        // Hover is the affordance: the chip brightens under the pointer.
        let chip_style = |app: &App| -> Style {
            plan_line(app, app.plan.as_ref().unwrap())
                .line
                .spans
                .iter()
                .find(|span| span.content.starts_with("2/5"))
                .expect("progress chip span")
                .style
        };
        let idle = chip_style(&app);
        app.hover_plan_chip = true;
        let hovered = chip_style(&app);
        assert_ne!(idle, hovered, "hover must be visible");
        assert!(hovered.add_modifier.contains(Modifier::UNDERLINED));
    }

    /// The dialog lists every row with its status glyph (no progress bar: the
    /// title carries the counts), and tracks live updates while it stays open.
    #[test]
    fn todo_dialog_shows_the_full_checklist() {
        let mut app = test_app();
        app.plan = Some(parser_plan());
        app.todo_dialog = Some(crate::app::TodoDialog { scroll: 0 });

        let frame = dump_frame(&mut app, 100, 24);
        assert!(frame.contains("Todo progress · 2/5 done"), "{frame}");
        assert!(frame.contains("✓ read the driver"), "{frame}");
        assert!(frame.contains("▶ wire the cap chip"), "{frame}");
        assert!(frame.contains("○ run the tests"), "{frame}");
        assert!(frame.contains("now: wire the cap chip"), "{frame}");
        assert!(
            !frame.contains('█') && !frame.contains('░'),
            "no progress bar in the dialog:\n{frame}"
        );
        assert!(!frame.contains("Tip"), "{frame}");

        // A live todo_write commit repaints the open dialog in place.
        app.plan = Some(plan(&[
            ("read the driver", PlanStatus::Completed),
            ("patch the parser", PlanStatus::Completed),
            ("wire the cap chip", PlanStatus::Completed),
        ]));
        let frame = dump_frame(&mut app, 100, 24);
        assert!(frame.contains("Todo progress · 3/3 done"), "{frame}");
        assert!(frame.contains("✓ wire the cap chip"), "{frame}");
        assert!(!frame.contains("○ run the tests"), "{frame}");
    }

    /// Both review cards float over the chat: `/keys` (the view overlay) and
    /// the todo dialog keep `DIALOG_MARGIN_*` of chat around their border,
    /// instead of one border kissing the screen edge. A short card and a long
    /// one (which clamps to the inset) both hold the gap.
    #[test]
    fn review_panes_keep_their_border_margin() {
        use crate::app::TodoDialog;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        // Corners of the one rounded card on screen: the dialog is drawn last
        // and its chat background is empty here, so the topmost `╭` row is its
        // top border and the first `╰` below is its bottom. Corner positions
        // are display columns — box glyphs and titles are multi-byte.
        fn card_bounds(frame: &str) -> (usize, usize, usize, usize) {
            use unicode_width::UnicodeWidthStr;
            let column = |line: &str, needle: char, last: bool| -> usize {
                let byte = if last {
                    line.rfind(needle).expect("corner")
                } else {
                    line.find(needle).expect("corner")
                };
                line[..byte].width()
            };
            let lines: Vec<&str> = frame.lines().collect();
            let top = lines
                .iter()
                .position(|line| line.contains('╭'))
                .expect("card top border");
            let left = column(lines[top], '╭', false);
            let right = column(lines[top], '╮', true);
            let bottom = (top..lines.len())
                .find(|&row| lines[row].contains('╰'))
                .expect("card bottom border");
            (top, left, right, bottom)
        }

        let assert_margin = |app: &mut App, long: bool| {
            let (w, h) = (100u16, 30u16);
            let theme = app.theme;
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
            terminal.draw(|f| draw(f, app)).expect("draw frame");
            let buf = terminal.backend().buffer().clone();
            let mut frame = String::new();
            for row in 0..h {
                for col in 0..w {
                    frame.push_str(buf[(col, row)].symbol());
                }
                frame.push('\n');
            }
            let (top, left, right, bottom) = card_bounds(&frame);
            assert!(
                top >= DIALOG_MARGIN_Y as usize,
                "top margin: card starts on row {top}:\n{frame}"
            );
            assert!(
                left >= DIALOG_MARGIN_X as usize,
                "left margin: card starts in column {left}:\n{frame}"
            );
            assert!(
                right <= (w - 1 - DIALOG_MARGIN_X) as usize,
                "right margin: card ends in column {right}:\n{frame}"
            );
            assert!(
                bottom <= (h - 1 - DIALOG_MARGIN_Y) as usize,
                "bottom margin: card ends on row {bottom}:\n{frame}"
            );
            // …and the card never covers the composer: its bottom border
            // stays at least a row above the composer's cap line (the last
            // rounded top border on screen — the card sits above it).
            let cap = frame
                .lines()
                .collect::<Vec<_>>()
                .iter()
                .rposition(|line| line.contains('╭'))
                .expect("composer cap row");
            assert!(
                bottom + 1 < cap,
                "card ends on row {bottom}, the composer cap row is {cap}:\n{frame}"
            );
            // The ring around the border is still chat, not card surface — a
            // "margin" painted panel bg would be no margin at all.
            for col in 0..left as u16 {
                assert_eq!(buf[(col, top as u16)].bg, theme.bg, "left of the card");
            }
            for row in 0..top as u16 {
                assert_eq!(buf[(left as u16, row)].bg, theme.bg, "above the card");
            }
            if long {
                assert!(
                    bottom - top > 12,
                    "the long card clamps to the inset, got {} rows",
                    bottom - top
                );
            }
        };

        // `/keys`-style review pane, then the todo dialog — short and long.
        let mut app = test_app();
        app.view_overlay = Some(crate::app::ViewOverlay {
            title: "Plan".into(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text: "## Plan\n\n- [x] Inspect\n- [ ] Implement".into(),
                streaming: false,
            }],
            scroll: 0,
        });
        assert_margin(&mut app, false);

        let mut app = test_app();
        app.plan = Some(parser_plan());
        app.todo_dialog = Some(TodoDialog { scroll: 0 });
        assert_margin(&mut app, false);

        // A 40-row checklist clamps to the inset — the card is as tall as the
        // margin allows, never taller.
        let mut app = test_app();
        let todos: Vec<crate::events::PlanItem> = (0..40)
            .map(|i| crate::events::PlanItem {
                content: format!("step {i:02} with a reasonably long label"),
                status: PlanStatus::Pending,
            })
            .collect();
        app.plan = Some(crate::events::PlanProgress {
            total: todos.len(),
            todos,
            active: None,
            active_extra: 0,
            completed: 0,
        });
        app.todo_dialog = Some(TodoDialog { scroll: 0 });
        assert_margin(&mut app, true);

        // The pickers (`/model`, `/permission`, `/resume`, `/theme`) float in
        // the same inset: a 40-item list clamps to it too.
        let mut app = test_app();
        app.picker = Some(crate::app::Picker {
            kind: crate::app::PickerKind::Session,
            title: " resume session · 40 sessions · enter select · esc close ".into(),
            sel: 0,
            items: (0..40)
                .map(|i| crate::app::PickerItem {
                    id: format!("sess-{i:02}"),
                    label: format!("session {i:02}"),
                    meta: format!("meta {i:02}"),
                    provider: None,
                })
                .collect(),
        });
        assert_margin(&mut app, true);
    }

    #[test]
    fn model_chip_prefers_fresh_pick_until_a_turn_realizes_it() {
        let flat = |spans: Vec<Span>| -> String {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        let mut app = test_app();
        // A previous turn streamed on pro; the user just picked flash.
        app.transcript.last_model = Some("deepseek-v4-pro".into());
        app.selected_model = Some("deepseek-v4-flash".into());
        let s = flat(status_right(&app));
        assert!(s.contains("deepseek-v4-flash"), "{s}");
        assert!(!s.contains("deepseek-v4-pro"), "{s}");
        // Without a pick, the streamed model rules.
        app.selected_model = None;
        let s = flat(status_right(&app));
        assert!(s.contains("deepseek-v4-pro"), "{s}");
    }

    #[test]
    fn live_meta_row_hides_session_options_until_session_bound() {
        let flat_line = |line: Line| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let flat_spans = |spans: Vec<Span>| -> String {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let mut app = live_test_app();
        app.modes.permission = Some("danger-full-access".into());
        app.modes.effort = Some("high".into());
        // Simulate the pre-bind window (SessionBound flips this on live).
        app.session_bound = false;

        assert_eq!(flat_line(status_title(&app)), "");
        // Nothing on the right either: no hint chips and no model until the
        // session binds.
        let pending = flat_spans(status_right(&app));
        assert!(pending.trim().is_empty(), "{pending}");

        let (ctl, _commands) = crate::controller::test_controller();
        app.handle(
            crate::bus::AppEvent::Ctl(crate::bus::CtlEvent::SessionBound {
                session_id: "acp-session".into(),
                notice: None,
                model: None,
                effort: None,
            }),
            &ctl,
        );

        let bound_left = flat_line(status_title(&app));
        let bound_right = flat_spans(status_right(&app));
        assert!(bound_left.contains("Full access"), "{bound_left}");
        assert!(bound_right.contains("deepseek-chat"), "{bound_right}");
        assert!(bound_right.contains("high"), "{bound_right}");
    }

    /// `/vim` is modal: the meta row names the mode while vim editing is on
    /// (normal mode swallows the letters a reader would expect to type), and
    /// says nothing once it is off again.
    #[test]
    fn the_meta_row_names_the_vim_mode_while_it_is_on() {
        let flat = |spans: Vec<Span>| -> String {
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let mut app = test_app();

        assert!(!flat(status_right(&app)).contains("INSERT"));

        app.vim.set(true);
        let insert = flat(status_right(&app));
        assert!(insert.contains("-- INSERT --"), "{insert}");

        app.vim.mode = crate::input::VimMode::Normal;
        let normal = flat(status_right(&app));
        assert!(normal.contains("-- NORMAL --"), "{normal}");
        assert!(!normal.contains("INSERT"), "{normal}");

        app.vim.set(false);
        let off = flat(status_right(&app));
        assert!(!off.contains("NORMAL") && !off.contains("INSERT"), "{off}");
    }

    /// A composer drag-selection paints reversed cells over the covered
    /// graphemes — the same treatment as the chat pane's highlight.
    #[test]
    fn a_composer_drag_selection_paints_reversed_cells() {
        use ratatui::backend::TestBackend;
        use ratatui::style::Modifier;
        use ratatui::Terminal;

        let (w, h) = (80u16, 20u16);
        let mut app = live_test_app();
        app.input.set("hello world".into());
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");

        // The first frame records the well; the drag covers chars 1..=4
        // ("ello") in well-local cells `(row 0, col 1..=4)`.
        let area = app.composer_area;
        assert!(area.height > 0 && area.width > 8, "well: {area:?}");
        app.input_sel = Some(crate::app::InputSel {
            anchor: (0, 1),
            head: (0, 4),
        });
        terminal.draw(|f| draw(f, &mut app)).expect("draw frame");

        let buf = terminal.backend().buffer().clone();
        let x = area.x + 2; // the "❯ " prompt column
        for col in 1..=4u16 {
            let cell = &buf[(x + col, area.y)];
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "cell {col} must be highlighted: {:?}",
                cell.symbol()
            );
        }
        for col in [0u16, 5, 10] {
            let cell = &buf[(x + col, area.y)];
            assert!(
                !cell.modifier.contains(Modifier::REVERSED),
                "cell {col} stays plain: {:?}",
                cell.symbol()
            );
        }
    }

    /// The mode label stands alone: the `shift+tab` key that used to follow it
    /// is gone from the meta row (the binding still works, and `/help` and
    /// `/permission` still document it), leaving one plain chrome tone.
    #[test]
    fn status_title_shows_the_mode_label_alone() {
        let flat = |line: &Line| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let mut app = test_app();
        // Pin the fact under test: the label follows the reported preset,
        // independent of the launch default.
        app.modes.permission = Some("workspace-write".into());

        app.locale = crate::locale::Locale::Zh;
        let zh = flat(&status_title(&app));
        assert_eq!(zh, "· 工作区可写", "{zh}");

        app.locale = crate::locale::Locale::En;
        let en_line = status_title(&app);
        let en = flat(&en_line);
        assert_eq!(en, "· Workspace Write", "{en}");
        assert!(!en.contains("shift"), "{en}");
        assert!(!en.contains('+'), "{en}");
        assert!(!en.contains("permission"), "{en}");

        let value_spans = en_line
            .spans
            .iter()
            .filter(|span| span.content.contains("Workspace Write"))
            .collect::<Vec<_>>();
        assert_eq!(value_spans.len(), 1, "{en}");
        assert_eq!(
            value_spans[0].style.fg,
            Some(app.theme.fg_tertiary),
            "plain tone on the label"
        );
        assert!(
            !value_spans[0].style.add_modifier.contains(Modifier::BOLD),
            "no emphasis on the label"
        );
    }

    /// The model id on the meta row's right side is plain chrome too: no
    /// accent in the full row and none in the compact fallback.
    #[test]
    fn meta_row_model_id_renders_plain() {
        let mut app = test_app();
        app.cfg.model = "deepseek-chat".into();
        app.input
            .set("a draft long enough to squeeze the right side ".repeat(4));
        // 200 cols keeps the whole right side, 30 takes the model-only
        // fallback, 28 drops it (left chrome + id no longer fit) — the id is
        // plain wherever it survives.
        for (width, shown) in [(200usize, true), (30, true), (28, false)] {
            let line = meta_line(&app, width);
            let spans: Vec<(String, Style)> = line
                .spans
                .iter()
                .filter(|span| span.content.contains("deepseek"))
                .map(|span| (span.content.to_string(), span.style))
                .collect();
            assert_eq!(spans.is_empty(), !shown, "model at width {width}");
            for (content, style) in spans {
                assert_eq!(
                    style.fg,
                    Some(app.theme.fg_tertiary),
                    "{content:?} stays plain at width {width}"
                );
                assert!(
                    !style.add_modifier.contains(Modifier::BOLD),
                    "{content:?} is not emphasised at width {width}"
                );
            }
        }
    }

    #[test]
    fn meta_row_hints_follow_the_state_machine() {
        let flat = |spans: Vec<Span>| -> String {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        let mut app = test_app();
        // idle · empty → nothing at all: the row is the model id's alone
        assert!(context_hints(&app).is_empty());
        // idle · draft → enter sends
        app.input.set("hello".into());
        let s = flat(context_hints(&app));
        assert!(s.contains("⏎ send"), "{s}");
        assert!(!s.contains("keys"), "{s}");
        // idle · slash drafts relabel enter
        app.input.set("/mo".into());
        assert!(flat(context_hints(&app)).contains("⏎ command"));
        // running · empty → interrupt only
        app.state = RunState::Running;
        app.input.clear();
        let s = flat(context_hints(&app));
        assert!(s.contains("esc interrupt"), "{s}");
        assert!(!s.contains("⏎"), "{s}");
        // running · draft → queue + send-now + interrupt
        app.input.set("follow-up".into());
        let s = flat(context_hints(&app));
        assert!(
            s.contains("⏎ queue") && s.contains("ctrl+⏎ steer") && s.contains("esc interrupt"),
            "{s}"
        );
    }

    #[test]
    fn agent_rail_lists_created_subagents_and_their_status() {
        let mut app = test_app();
        app.subagents.push(crate::app::SubagentView {
            id: "child-1".into(),
            parent: "dsh-test".into(),
            label: "subagent 1".into(),
            running: true,
            transcript: crate::transcript::Transcript::new("child-1".into()),
        });
        app.subagents.push(crate::app::SubagentView {
            id: "child-2".into(),
            parent: "dsh-test".into(),
            label: "subagent 2".into(),
            running: false,
            transcript: crate::transcript::Transcript::new("child-2".into()),
        });

        let frame = dump_frame(&mut app, 100, 24);
        assert!(frame.contains("agents"), "{frame}");
        assert!(frame.contains("● subagent 1"), "{frame}");
        assert!(frame.contains("✓ subagent 2"), "{frame}");
    }

    #[test]
    fn child_view_replaces_the_composer_with_read_only_navigation() {
        let mut app = test_app();
        let mut transcript = crate::transcript::Transcript::new("child-1".into());
        transcript.apply(crate::events::UiEvent::TextDelta {
            session: "child-1".into(),
            text: "child-only output".into(),
        });
        app.subagents.push(crate::app::SubagentView {
            id: "child-1".into(),
            parent: "dsh-test".into(),
            label: "subagent 1".into(),
            running: true,
            transcript,
        });
        app.active_subagent = Some("child-1".into());

        let frame = dump_frame(&mut app, 100, 24);
        assert!(frame.contains("child-only output"), "{frame}");
        assert!(frame.contains("read-only"), "{frame}");
        assert!(frame.contains("esc back"), "{frame}");
        assert!(
            !frame.contains("describe what you want to build"),
            "{frame}"
        );
        assert!(!frame.contains("send a prompt"), "{frame}");
    }

    #[test]
    fn active_running_child_does_not_show_the_main_idle_state() {
        let mut app = test_app();
        app.subagents.push(crate::app::SubagentView {
            id: "child-1".into(),
            parent: "dsh-test".into(),
            label: "subagent 1".into(),
            running: true,
            transcript: crate::transcript::Transcript::new("child-1".into()),
        });
        app.active_subagent = Some("child-1".into());

        let frame = dump_frame(&mut app, 100, 24);
        assert!(frame.contains("working"), "{frame}");
        assert!(!frame.contains("● idle"), "{frame}");
    }

    #[test]
    fn long_input_wraps_in_the_well() {
        let mut app = test_app();
        app.input.set("a".repeat(50));
        // 40x12 → composer height 4 → a 2-row input well, 36 text cols wide.
        let frame = dump_frame(&mut app, 40, 12);
        let wrapped = frame.lines().filter(|l| l.contains("aaaa")).count();
        assert!(wrapped >= 2, "input should wrap across well rows:\n{frame}");
    }

    #[test]
    fn multiline_input_breaks_on_newlines() {
        let mut app = test_app();
        let hello_world = "hello\nworld".to_string();
        app.input.set(hello_world.clone());
        app.input.set_cursor_char(hello_world.chars().count());
        let frame = dump_frame(&mut app, 40, 12);
        let hello = frame
            .lines()
            .position(|l| l.contains("hello"))
            .expect("first line");
        let world = frame
            .lines()
            .position(|l| l.contains("world"))
            .expect("second line");
        assert!(
            world > hello,
            "hard newline pushes the second line down:\n{frame}"
        );
    }

    /// The input well is five rows tall on a normal terminal — two more than
    /// the old ladder gave it — and the growth cap moved up with it so a
    /// wrapped draft still has somewhere to go.
    #[test]
    fn composer_keeps_five_well_rows_on_a_normal_terminal() {
        let app = test_app();

        assert_eq!(composer_height(30), 6, "tall terminal");
        assert_eq!(composer_height(15), 6, "threshold");
        assert_eq!(composer_height(12), 5, "mid terminal");
        assert_eq!(composer_height(8), 4, "short terminal");
        assert_eq!(
            resolved_composer_height(Rect::new(0, 0, 100, 30), &app),
            6,
            "an empty draft still gets the minimum well"
        );
        // The ladder and the cap stay in step: the well can grow past the
        // minimum, up to 14 rows on a tall terminal.
        let tall = Rect::new(0, 0, 100, 60);
        assert_eq!(resolved_composer_height(tall, &app), 6);
    }

    #[test]
    fn composer_grows_to_show_a_multiline_draft_until_its_cap() {
        let mut app = test_app();
        let _ = dump_frame(&mut app, 100, 30);
        let empty_chat_height = app.chat_view.area.height;

        app.input.set("one\ntwo\nthree\nfour\nfive\nsix".into());
        let frame = dump_frame(&mut app, 100, 30);

        assert!(
            app.chat_view.area.height < empty_chat_height,
            "composer should take rows from chat as the draft grows:\n{frame}"
        );
        assert!(frame.contains("one") && frame.contains("six"), "{frame}");
    }

    #[test]
    fn session_picker_rows_align_label_and_meta_columns() {
        use crate::app::{Picker, PickerItem, PickerKind, PICKER_LABEL_COL};
        let mut app = test_app();
        app.picker = Some(Picker {
            kind: PickerKind::Session,
            title: " resume session · 2 sessions · enter select · esc close ".into(),
            sel: 0,
            items: vec![
                PickerItem {
                    id: "276b7574-b12c-488e-958b-f9673b67fba9".into(),
                    label: "查看session历史命令的可行性".into(),
                    meta: "276b7574 · 2h · 3 turns".into(),
                    provider: None,
                },
                PickerItem {
                    id: "dsh-alp".into(),
                    label: "fix failing tests".into(),
                    meta: "dsh-alp  · just now · 1 turn".into(),
                    provider: None,
                },
            ],
        });
        let frame = dump_frame(&mut app, 100, 30);
        let ascii_row = frame
            .lines()
            .find(|l| l.contains("fix failing tests"))
            .expect("ascii row");
        // Wide chars dump as char + continuation cell, so locate the CJK
        // row by its meta instead of the raw label.
        let cjk_row = frame
            .lines()
            .find(|l| l.contains("276b7574 · 2h"))
            .unwrap_or_else(|| panic!("cjk row missing:\n{frame}"));
        // Dump cells contribute exactly one char each, so the column of a
        // marker is the char count before its byte offset (`find` alone
        // returns byte indices, which differ for multi-byte CJK labels).
        let meta_col = |row: &str| row.find('·').map(|i| row[..i].chars().count()).unwrap_or(0);
        assert_eq!(
            meta_col(ascii_row),
            meta_col(cjk_row),
            "meta column lines up:\n{ascii_row}\n{cjk_row}\ncols: {} vs {}",
            meta_col(ascii_row),
            meta_col(cjk_row),
        );
        assert!(
            ascii_row.chars().count() >= 2 + PICKER_LABEL_COL + 8,
            "label column padded to {PICKER_LABEL_COL}"
        );
    }

    #[test]
    fn picker_window_follows_the_selection_and_shows_a_scrollbar() {
        use crate::app::{Picker, PickerItem, PickerKind};
        let mut app = test_app();
        let items: Vec<PickerItem> = (0..40)
            .map(|i| PickerItem {
                id: format!("sess-{i:02}"),
                label: format!("session {i:02}"),
                meta: format!("meta {i:02}"),
                provider: None,
            })
            .collect();
        app.picker = Some(Picker {
            kind: PickerKind::Session,
            title: " resume session · 40 sessions · enter select · esc close ".into(),
            sel: 0,
            items,
        });

        // 24-row terminal: the popup caps at 12 rows (the inset band above
        // the composer) and must scroll.
        let top = dump_frame(&mut app, 100, 24);
        assert!(top.contains("session 00"), "head row visible:\n{top}");
        assert!(!top.contains("session 39"), "tail not visible yet:\n{top}");
        // The rounded border stays intact: the scrollbar lives in its own
        // column inside the popup, between the rows and the right border.
        assert!(top.contains("╮"), "top-right corner intact:\n{top}");
        assert!(top.contains("║"), "scrollbar track shown:\n{top}");
        assert!(
            top.lines()
                .any(|l| l.contains("session 00") && l.trim_end().ends_with('│')),
            "right border column intact next to the scrollbar:\n{top}"
        );

        // Jump to the tail: the window follows, so the last row is
        // reachable instead of being clipped out of the paragraph.
        app.picker.as_mut().unwrap().sel = 39;
        let tail = dump_frame(&mut app, 100, 24);
        assert!(tail.contains("session 39"), "tail row visible:\n{tail}");
        assert!(!tail.contains("session 00"), "head scrolled away:\n{tail}");
        assert!(tail.contains("█"), "scrollbar thumb shown:\n{tail}");
        assert!(tail.contains("╰"), "bottom corners intact:\n{tail}");
        // 9 rows: the inset band above the composer, two rows shorter since
        // the composer grew by two.
        assert_eq!(app.picker_page_rows, 9, "page size = visible rows");
    }

    #[test]
    fn picker_selection_highlights_the_whole_row() {
        use crate::app::{Picker, PickerItem, PickerKind};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = test_app();
        app.picker = Some(Picker {
            kind: PickerKind::Session,
            title: " resume session · 2 sessions ".into(),
            sel: 0,
            items: vec![
                PickerItem {
                    id: "a".into(),
                    label: "first".into(),
                    meta: "meta a".into(),
                    provider: None,
                },
                PickerItem {
                    id: "b".into(),
                    label: "second".into(),
                    meta: "meta b".into(),
                    provider: None,
                },
            ],
        });
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| draw(f, &mut app)).expect("draw");
        let buf = terminal.backend().buffer();
        let chip = app.theme.chip_bg;
        // Cells hold single symbols, so search rows by joining them first.
        let row_of = |needle: &str| {
            (0..20u16)
                .find(|&r| {
                    (0..80u16)
                        .map(|c| buf[(c, r)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .unwrap_or_else(|| panic!("row with {needle:?} not found"))
        };
        // The selected row: marker, label, meta and the tail padding all
        // carry the chip background — the highlight spans the whole line.
        let sel_row = row_of("first");
        let marker_col = (0..80u16)
            .find(|&c| buf[(c, sel_row)].symbol() == "▸")
            .expect("selection marker");
        let right_border = (marker_col..80u16)
            .find(|&c| buf[(c, sel_row)].symbol() == "│")
            .expect("popup right border");
        for c in marker_col..right_border {
            assert_eq!(
                buf[(c, sel_row)].style().bg,
                Some(chip),
                "selected row cell {c} carries the highlight bg"
            );
        }
        // The unselected row keeps the panel background.
        let other_row = row_of("second");
        for c in 0..80u16 {
            let cell = &buf[(c, other_row)];
            if !cell.symbol().trim().is_empty() {
                assert_ne!(
                    cell.style().bg,
                    Some(chip),
                    "unselected row cell {c} must not carry the highlight bg"
                );
            }
        }
    }

    #[test]
    fn model_picker_marks_only_the_current_provider_model_pair() {
        use crate::app::{Picker, PickerItem, PickerKind};
        let mut app = test_app();
        app.cfg.provider = "coding-plan-b".into();
        app.cfg.model = "deepseek-v4".into();
        app.picker = Some(Picker {
            kind: PickerKind::Model,
            title: " model ".into(),
            sel: 1,
            items: vec![
                PickerItem {
                    id: "deepseek-v4".into(),
                    label: "deepseek-v4".into(),
                    meta: "coding-plan-a · DeepSeek V4".into(),
                    provider: Some("coding-plan-a".into()),
                },
                PickerItem {
                    id: "deepseek-v4".into(),
                    label: "deepseek-v4".into(),
                    meta: "coding-plan-b · DeepSeek V4".into(),
                    provider: Some("coding-plan-b".into()),
                },
            ],
        });

        let frame = dump_frame(&mut app, 100, 24);
        let marked: Vec<&str> = frame.lines().filter(|line| line.contains("✓")).collect();
        assert_eq!(
            marked.len(),
            1,
            "only one provider/model row is current:\n{frame}"
        );
        assert!(
            marked[0].contains("coding-plan-b"),
            "current marker follows the provider: {}",
            marked[0]
        );
    }

    #[test]
    fn scroll_up_survives_draw_and_shows_indicator() {
        let mut app = test_app();
        for i in 0..40 {
            app.transcript.push_user(format!("line {i}"), false);
        }
        app.scroll_by(20);
        let frame = dump_frame(&mut app, 100, 14);
        assert!(app.scroll_up > 0, "scroll_up clamped to zero");
        assert!(frame.contains("▲"), "scroll indicator missing:\n{frame}");
    }

    #[test]
    fn selection_overlay_reverses_cells() {
        use crate::app::{SelPoint, Selection};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = test_app();
        app.transcript
            .push_user("hello selection world".into(), false);
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        // First draw fills chat_view; then select and draw again.
        terminal.draw(|f| draw(f, &mut app)).expect("warmup");
        let line = app
            .chat_view
            .lines
            .iter()
            .position(|l| l.contains("hello"))
            .expect("user line in layout");
        app.sel = Some(Selection {
            anchor: SelPoint { line, col: 0 },
            head: SelPoint { line, col: 8 },
        });
        terminal.draw(|f| draw(f, &mut app)).expect("redraw");
        let buf = terminal.backend().buffer();
        let row = app.chat_view.area.y + (line - app.chat_view.top) as u16;
        let x = app.chat_view.area.x;
        let reversed = (0..9u16)
            .filter(|c| {
                buf[(x + c, row)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            })
            .count();
        assert_eq!(reversed, 9, "anchor..=head cells are highlighted");
        assert_eq!(
            app.selection_text(app.sel.unwrap()),
            crate::app::slice_by_cells(&app.chat_view.lines[line], 0, 9).trim_end(),
            "copied text matches the highlighted cells"
        );
    }

    #[test]
    fn permission_ask_overlay_lists_kind_name_and_title() {
        use crate::app::PermissionAskOverlay;
        use crate::bus::PermissionAskOption;
        let mut app = test_app();
        app.permission_ask = Some(PermissionAskOverlay {
            title: "bash".into(),
            sel: 1,
            options: vec![
                PermissionAskOption {
                    option_id: "reject".into(),
                    kind: "reject_once".into(),
                    name: "Reject".into(),
                },
                PermissionAskOption {
                    option_id: "allow".into(),
                    kind: "allow_once".into(),
                    name: "Allow once".into(),
                },
            ],
            reply: None,
        });
        let frame = dump_frame(&mut app, 100, 30);
        assert!(frame.contains("bash"), "tool title in overlay\n{frame}");
        assert!(frame.contains("Reject"), "option name\n{frame}");
        assert!(frame.contains("Allow once"), "option name\n{frame}");
        assert!(frame.contains("reject_once"), "option kind\n{frame}");
        assert!(frame.contains("allow_once"), "option kind\n{frame}");
        let allow_row = frame
            .lines()
            .find(|l| l.contains("Allow once"))
            .expect("allow row");
        assert!(
            allow_row.contains("▸"),
            "selection on allow_once: {allow_row}"
        );
    }

    #[test]
    fn plugin_view_overlay_renders_markdown_and_uses_wide_screens() {
        use crate::app::ViewOverlay;
        let mut app = test_app();
        app.view_overlay = Some(ViewOverlay {
            title: "Plan".into(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text: "## Plan · 1/2\n\n- [x] Inspect · priority · high\n- [ ] **Implement** · priority · medium"
                    .into(),
                streaming: false,
            }],
            scroll: 0,
        });
        let frame = dump_frame(&mut app, 160, 30);
        // The plan review renders through the full markdown pipeline: the
        // heading loses its `#` markers, checkboxes become status glyphs,
        // and the task list survives.
        assert!(frame.contains("Plan · 1/2"), "heading:\n{frame}");
        assert!(frame.contains("✓ Inspect"), "checked item:\n{frame}");
        assert!(frame.contains("○ Implement"), "pending item:\n{frame}");
        assert!(frame.contains("Implement"), "pending item:\n{frame}");
        // Wide terminals: the review pane exceeds the old 84-column cap.
        let title_line = frame
            .lines()
            .find(|l| l.contains("esc close"))
            .expect("overlay title row");
        let left = title_line.find('╭').expect("left corner");
        let right = title_line.rfind('╮').expect("right corner");
        assert!(
            right - left > 84,
            "wide overlay expected, got {} cols: {title_line}",
            right - left
        );
    }
}

#[cfg(test)]
mod rpc_probe {
    use super::*;
    use crate::runtime::RuntimeConfig;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    use std::sync::mpsc;

    fn probe_app() -> App {
        let cfg = RuntimeConfig {
            workspace: "/w".into(),
            home: std::env::temp_dir()
                .join(format!("abylab-rpc-probe-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            sessions_root: std::env::temp_dir()
                .join(format!("abylab-rpc-probe-sessions-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            max_tokens: None,
            base_url: None,
            api_key: None,
            key_origin: None,
        };
        let (_tx, _rx) = mpsc::channel::<crate::bus::AppEvent>();
        let mut app = App::new(Theme::dark(), cfg, "dsh-test".into());
        app.locale = crate::locale::Locale::En;
        app
    }

    fn push_view(app: &mut App, text: &str) {
        app.view_overlay = Some(crate::app::ViewOverlay {
            title: "Plan".into(),
            nodes: vec![crate::slots::TuiNode::Markdown {
                text: text.into(),
                streaming: false,
            }],
            scroll: 0,
        });
    }

    #[test]
    fn real_rpc_path_renders_markdown_and_scrolls() {
        use crate::controller::test_controller;
        let mut app = probe_app();
        let (ctl, _commands) = test_controller();
        let long = (0..80)
            .map(|i| format!("- [{}] task {i:02}", if i % 3 == 0 { 'x' } else { ' ' }))
            .collect::<Vec<_>>()
            .join("\n");
        push_view(&mut app, &format!("## Plan · 27/80\n\n{long}"));
        assert!(app.view_overlay.is_some(), "view overlay opened");

        let frame = dump_frame(&mut app, 100, 30);
        assert!(frame.contains("Plan · 27/80"), "heading rendered:\n{frame}");
        assert!(frame.contains("task 00"), "task list rendered:\n{frame}");
        assert!(frame.contains("✓ task 00"), "checked glyph:\n{frame}");
        assert!(frame.contains("○ task 01"), "open glyph:\n{frame}");

        let before = app.view_overlay.as_ref().unwrap().scroll;
        app.handle(
            crate::bus::AppEvent::Term(Event::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            ))),
            &ctl,
        );
        let after = app.view_overlay.as_ref().unwrap().scroll;
        assert_eq!(after, before + 1, "Down scrolls the view");
        let frame2 = dump_frame(&mut app, 100, 30);
        assert!(
            !frame2.lines().any(|l| l.contains("Plan · 27/80")),
            "scrolling pushed the heading out:\n{frame2}"
        );
        assert!(
            frame2.contains("task 02"),
            "content followed the scroll:\n{frame2}"
        );
    }

    #[test]
    fn arrow_keys_with_modifiers_still_scroll_the_view() {
        use crate::controller::test_controller;
        let mut app = probe_app();
        let (ctl, _commands) = test_controller();
        push_view(
            &mut app,
            &format!(
                "## Plan\n\n{}",
                (0..60)
                    .map(|i| format!("- [ ] task {i:02}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
        let before = app.view_overlay.as_ref().unwrap().scroll;
        // Some terminals report arrows with modifier bits (kitty keyboard
        // protocol); those must still scroll the view.
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
        ] {
            app.handle(
                crate::bus::AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Down, modifiers))),
                &ctl,
            );
        }
        assert_eq!(
            app.view_overlay.as_ref().unwrap().scroll,
            before + 3,
            "modified arrows scroll"
        );
    }

    #[test]
    fn wheel_scrolls_the_view_overlay() {
        use crate::controller::test_controller;
        let mut app = probe_app();
        let (ctl, _commands) = test_controller();
        push_view(
            &mut app,
            &format!(
                "## Plan\n\n{}",
                (0..60)
                    .map(|i| format!("- [ ] task {i:02}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
        let before = app.view_overlay.as_ref().unwrap().scroll;
        app.handle(
            crate::bus::AppEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 50,
                row: 15,
                modifiers: KeyModifiers::NONE,
            })),
            &ctl,
        );
        assert_eq!(
            app.view_overlay.as_ref().unwrap().scroll,
            before + 3,
            "wheel scrolls the view"
        );
        app.handle(
            crate::bus::AppEvent::Term(Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 50,
                row: 15,
                modifiers: KeyModifiers::NONE,
            })),
            &ctl,
        );
        assert_eq!(
            app.view_overlay.as_ref().unwrap().scroll,
            before,
            "wheel up reverses the view scroll"
        );
    }

    #[test]
    fn end_clamps_the_view_to_the_last_content_row() {
        use crate::controller::test_controller;
        let mut app = probe_app();
        let (ctl, _commands) = test_controller();
        let long = (0..80)
            .map(|i| format!("- [ ] task {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        push_view(&mut app, &format!("## Plan\n\n{long}"));
        app.handle(
            crate::bus::AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))),
            &ctl,
        );
        let frame = dump_frame(&mut app, 100, 30);
        assert!(frame.contains("task 79"), "bottom row visible:\n{frame}");
        // Overscroll stays clamped: End then more Down shows no blank tail.
        for _ in 0..10 {
            app.handle(
                crate::bus::AppEvent::Term(Event::Key(KeyEvent::new(
                    KeyCode::Down,
                    KeyModifiers::NONE,
                ))),
                &ctl,
            );
        }
        let frame2 = dump_frame(&mut app, 100, 30);
        assert!(frame2.contains("task 79"), "still at the bottom:\n{frame2}");
        assert!(
            frame2.lines().any(|l| l.contains("task 79")),
            "last row remains visible:\n{frame2}"
        );
    }
}
