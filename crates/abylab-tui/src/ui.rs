//! Rendering: scrollback, tips row, status bar, prompt, hints, overlays.

use ratatui::layout::{Constraint, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, FrameExt, HighlightSpacing, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState,
};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, RunState};
use crate::theme::Theme;

/// Composer card height for a terminal `height` rows tall.
/// Composer height: the input well plus one bottom meta row (state ·
/// mode/permission chips · model). Taller terminals get a taller well.
fn composer_height(height: u16) -> u16 {
    if height >= 15 {
        4
    } else if height >= 10 {
        3
    } else {
        2
    }
}

/// Grow with hard/soft-wrapped draft rows, while leaving at least half of a
/// normal terminal to the conversation. Beyond the cap, `draw_input` keeps a
/// cursor-following viewport inside the composer.
fn resolved_composer_height(area: Rect, app: &App) -> u16 {
    let minimum = composer_height(area.height);
    let inner_width = area.width.saturating_sub(2);
    let prompt_width = "❯ ".width() as u16;
    let wrap_width = inner_width.saturating_sub(prompt_width).max(1) as usize;
    let maximum = (area.height / 2).max(minimum).min(12);
    let desired = app
        .input
        .visual_row_count(wrap_width)
        .saturating_add(1)
        .min(maximum as usize) as u16;
    desired.max(minimum).min(maximum)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.theme;
    // The soft caret's screen cell is rebuilt every frame; a frame without
    // a painted caret (overlay owns input) leaves it `None` and `main`
    // parks the hidden hardware cursor nowhere.
    app.caret_cell = None;
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
    // The stats dock rides below the box when the terminal is tall enough.
    let stats_h = if !child_view && main.height >= 20 {
        1
    } else {
        0
    };
    let chat_h = main
        .height
        .saturating_sub(composer_h + cap_h + stats_h + agents_h + gap_h);

    let chat = Rect::new(main.x, main.y, main.width, chat_h);
    let chrome_y = main.y + chat_h + gap_h;
    let agents = Rect::new(main.x, chrome_y, main.width, agents_h);
    let composer_box = Rect::new(main.x, chrome_y + agents_h, main.width, cap_h + composer_h);
    let composer = Rect::new(main.x, composer_box.y + cap_h, main.width, composer_h);
    let stats_dock = Rect::new(
        main.x,
        composer_box.y + composer_box.height,
        main.width,
        stats_h,
    );

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
    if stats_h > 0 {
        draw_stats_dock(f, app, stats_dock);
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
    draw_model_picker(f, app, area);
    draw_view_overlay(f, app, area);
    draw_permission_ask(f, app, area);
}

fn draw_view_overlay(f: &mut Frame, app: &mut App, screen: Rect) {
    let theme = app.theme;
    let Some(view) = app.view_overlay.as_mut() else {
        return;
    };
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

/// The bottom stats dock: one compact row under the composer box — token
/// flow, cache hit rate, turn/step counts and timing. Narrow terminals
/// drop tail sections; zero-data sessions still render the row (zeros).
fn draw_stats_dock(f: &mut Frame, app: &App, area: Rect) {
    let theme = app.theme;
    f.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    if inner.width < 12 {
        return;
    }
    f.render_widget(
        Paragraph::new(Line::from(stats_dock_spans(app, inner.width as usize))),
        inner,
    );
}

/// Compact key/value spans for the dock; the caller caps the width by
/// dropping trailing sections.
fn stats_dock_spans(app: &App, width: usize) -> Vec<Span<'static>> {
    let theme = app.theme;
    let u = app.transcript.usage;
    let s = app.transcript.stats;
    // u.input is total input including cache reads (driver maps Usage::input_tokens).
    let cache_pct = if u.input > 0 {
        (u.cached as f64 / u.input as f64 * 100.0).round() as u64
    } else {
        0
    };
    let sections: Vec<(String, String)> = vec![
        (
            "tokens".into(),
            format!(
                "↑{} ↓{} · cache {}%",
                crate::app::fmt_tokens(u.input),
                crate::app::fmt_tokens(u.output),
                cache_pct
            ),
        ),
        (
            "turns".into(),
            format!("{} turns · {} steps", s.turns, s.steps),
        ),
        (
            "timing".into(),
            format!(
                "LLM {} · tool {}",
                crate::app::fmt_duration(s.turn_millis),
                crate::app::fmt_duration(s.tool_millis)
            ),
        ),
        (
            "ttft".into(),
            format!(
                "TTFT avg {}",
                s.ttft_total_millis
                    .checked_div(s.ttft_count)
                    .map_or_else(|| "—".to_string(), crate::app::fmt_duration)
            ),
        ),
    ];
    let mut spans = vec![Span::raw(" ")];
    let mut used = 2;
    for (index, (key, value)) in sections.iter().enumerate() {
        let mut section_width = key.width() + 3 + value.width();
        if index > 0 {
            section_width += 3; // " | "
        }
        if used + section_width > width.saturating_sub(2) {
            break; // tail sections drop first on narrow terminals
        }
        if index > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(theme.border)));
        }
        spans.push(Span::styled(
            format!("{key} · "),
            Style::default().fg(theme.caption),
        ));
        spans.push(Span::styled(
            value.clone(),
            Style::default().fg(theme.fg_secondary),
        ));
        used += section_width;
    }
    spans.push(Span::raw(" "));
    spans
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
/// input well. A brand-blue edge bar glows while working. Tall enough
/// terminals get `draw_composer_box` instead — the same surface wrapped
/// in the rounded frame that also carries the cap row.
fn draw_composer(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let running = !matches!(app.state, RunState::Idle);

    // Surface fill: contrast against the chat bg does the framing.
    f.render_widget(
        Block::default().style(Style::default().bg(theme.panel)),
        area,
    );
    // Left edge bar: the working "glow" (the old border used to do this).
    if running {
        let bar: Vec<Line> = (0..area.height).map(|_| Line::from("▎")).collect();
        f.render_widget(
            Paragraph::new(bar).style(Style::default().fg(theme.brand)),
            Rect::new(area.x, area.y, 1, area.height),
        );
    }

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
            Style::default().fg(theme.brand_soft),
        ));
        let compact_width = span_widths(&compact);
        if lw + compact_width + 2 <= width {
            right_spans = compact;
        } else if lw + shown_model.width() + 3 <= width {
            right_spans = vec![Span::styled(
                format!("{shown_model} "),
                Style::default().fg(theme.brand_soft),
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
    let key_style = Style::default()
        .fg(theme.brand_soft)
        .add_modifier(Modifier::BOLD);
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
    spans.push(Span::styled(" shift+tab", key_style));
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
        // Working with a draft: enter queues; ^x steers without cancellation.
        (true, false) => vec![
            ("⏎", app.locale.tr("queue", "排队")),
            ("^x", "steer"),
            ("esc", app.locale.tr("interrupt", "中断")),
        ],
        // Idle, empty: point at the full shortcut list.
        (false, true) => vec![("^K", app.locale.tr("keys", "快捷键"))],
        // Idle with a draft: enter's meaning follows the prefix.
        (false, false) if app.input.buf().starts_with('/') => {
            vec![("⏎", app.locale.tr("command", "命令"))]
        }
        (false, false) => vec![("⏎", app.locale.tr("send", "发送"))],
    };
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

/// Meta row, right side: contextual shortcut hints, model chip (brand
/// accent), and the requested reasoning effort. Token flow lives in the
/// usage footer; the session id lives in `/session`.
fn status_right(app: &App) -> Vec<Span<'static>> {
    let theme = app.theme;
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
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
            Style::default().fg(theme.brand_soft),
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
    draw_selection_overlay(f, app, inner, start);
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

fn tip_line(app: &App) -> Line<'static> {
    let theme = app.theme;
    let transient = app.tip.is_some();
    let text = match &app.tip {
        Some((t, _)) => t.clone(),
        None => app.locale.ambient_tip(app.ambient_tip_idx).to_string(),
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    if transient {
        // Action feedback reads brighter than the rotating hints.
        spans.push(Span::styled(text, Style::default().fg(theme.fg)));
    } else {
        spans.push(Span::styled(
            app.locale.tr("Tip", "提示").to_string(),
            Style::default()
                .fg(theme.brand_soft)
                .add_modifier(Modifier::BOLD),
        ));
        // Rotating hints read a tier below the chat body text above — the
        // banner is furniture, not content. Gray-blue keeps it on-brand.
        spans.push(Span::styled(
            format!(" · {text}"),
            Style::default().fg(theme.hint),
        ));
    }
    spans.push(Span::raw(" "));

    Line::from(spans)
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

fn workspace_cap_title(app: &App, area_width: usize) -> Line<'static> {
    let title_width = (area_width / 2).clamp(8, 64);
    let path_width = title_width.saturating_sub(4);
    Line::from(Span::styled(
        format!(" · {} ", compact_workspace(&app.cfg.workspace, path_width)),
        Style::default().fg(app.theme.caption),
    ))
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
/// The brand glow replaces the left border while a turn runs.
fn draw_composer_box(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let running = !matches!(app.state, RunState::Idle);
    let workspace = workspace_cap_title(app, area.width as usize);
    let workspace_width = span_widths(&workspace.spans);
    let title_budget = (area.width as usize).saturating_sub(2 + workspace_width + 1);
    let title = ellipsize_line(
        tip_line(app),
        title_budget,
        Style::default().fg(theme.caption),
    );
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

    // Running glow: the brand bar replaces the left border (corners and
    // both title rows stay intact).
    if running {
        let bar: Vec<Line> = (0..inner.height).map(|_| Line::from("▎")).collect();
        f.render_widget(
            Paragraph::new(bar).style(Style::default().fg(theme.brand)),
            Rect::new(area.x, area.y + 1, 1, inner.height),
        );
    }

    let content = inner;

    // Draft first: the well owns every inner row — the meta row lives on
    // the bottom border.
    app.att_chips.clear();
    app.att_thumbs.clear();
    app.composer_wrap_width = content.width.saturating_sub("❯ ".width() as u16).max(1) as usize;
    draw_input(f, app, content);
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
    if area.width < 4 || area.height == 0 {
        return;
    }
    let prompt = "❯ ";
    let pw = prompt.width();
    // The prompt doubles as the working indicator that used to be the brand
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
                    "queue a follow-up — ctrl+x steers now",
                    "输入后续消息 — ctrl+x 立即 steer",
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
    if menu.explorer().files().is_empty() {
        return;
    }
    let theme = app.theme;
    let locale = app.locale;
    menu.apply_chrome(&theme, locale, &app.cfg.workspace);
    let n = menu.explorer().files().len();
    let vis = FILE_MENU_ROWS
        .min(n)
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
    f.render_widget_ref(menu.explorer().widget(), area);
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
        // 80x20: the stats dock owns row 19, so the rounded box spans rows
        // 14..18 — top border 14 (tip + · workspace), well 15..17, bottom
        // border 18 carrying the meta row (`╰· Standard … ╯`).
        assert_eq!(buf[(0, 14)].symbol(), "╭", "top-left corner");
        assert_eq!(buf[(79, 14)].symbol(), "╮", "top-right corner");
        assert_eq!(buf[(0, 18)].symbol(), "╰", "bottom-left corner");
        assert_eq!(buf[(79, 18)].symbol(), "╯", "bottom-right corner");
        assert_eq!(buf[(4, 14)].bg, theme.panel, "border row on the card");
        assert_eq!(buf[(40, 15)].bg, theme.panel, "input well on panel surface");
        assert_eq!(buf[(40, 16)].bg, theme.panel, "input well on panel surface");
        assert_eq!(buf[(4, 17)].bg, theme.panel, "well fills the inner rows");
        assert_eq!(
            buf[(1, 18)].symbol(),
            "·",
            "meta row rides the bottom border with small dots"
        );
        // The stats dock owns the row under the box.
        assert!(
            buf[(2, 19)].symbol() != "╭" && buf[(1, 19)].bg != theme.panel,
            "stats dock rides row 19, not the card surface"
        );
        assert_eq!(buf[(4, 9)].bg, theme.bg, "chat keeps the base background");
    }

    #[test]
    fn composer_cap_persistently_shows_the_workspace() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/projects/deepseek-harness-tui-plan-view".into();

        let frame = dump_frame(&mut app, 120, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("Tip"))
            .expect("composer cap");

        assert!(
            cap.contains("· /work/acme/projects/deepseek-harness-tui-plan-view"),
            "{cap}"
        );
    }

    #[test]
    fn composer_cap_preserves_the_workspace_tail_on_narrow_terminals() {
        let mut app = test_app();
        app.cfg.workspace = "/work/acme/very-long-directory-name/deepseek-harness".into();

        let frame = dump_frame(&mut app, 60, 20);
        let cap = frame
            .lines()
            .find(|line| line.contains("Tip"))
            .expect("composer cap");

        assert!(cap.contains("· …/deepseek-harness"), "{cap}");
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
        let pending = flat_spans(status_right(&app));
        assert!(pending.contains("^K keys"), "{pending}");
        assert!(!pending.contains("deepseek-chat"), "{pending}");
        assert!(!pending.contains("high"), "{pending}");

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

    #[test]
    fn status_shortcut_hints_follow_their_values_and_use_key_styling() {
        let flat = |line: &Line| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let mut app = test_app();
        // Pin the fact under test: the hint renders the reported preset's
        // label, independent of the launch default.
        app.modes.permission = Some("workspace-write".into());

        app.locale = crate::locale::Locale::Zh;
        let zh_line = status_title(&app);
        let zh = flat(&zh_line);
        assert!(zh.contains("· 工作区可写 shift+tab"), "{zh}");
        assert!(!zh.contains("permission"), "{zh}");
        assert!(!zh.contains("权限"), "{zh}");

        app.locale = crate::locale::Locale::En;
        let en_line = status_title(&app);
        let en = flat(&en_line);
        assert!(en.contains("· Workspace Write shift+tab"), "{en}");
        assert!(!en.contains("permission"), "{en}");
        assert!(!en.contains("access"), "{en}");

        let key_spans = en_line
            .spans
            .iter()
            .filter(|span| span.content.contains('+'))
            .collect::<Vec<_>>();
        assert_eq!(key_spans.len(), 1, "{en}");
        assert!(key_spans.iter().all(|span| {
            span.style.fg == Some(app.theme.brand_soft)
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        let value_spans = en_line
            .spans
            .iter()
            .filter(|span| span.content.contains("Workspace Write"))
            .collect::<Vec<_>>();
        assert_eq!(value_spans.len(), 1, "{en}");
        assert!(value_spans
            .iter()
            .all(|span| span.style.fg == Some(app.theme.fg_tertiary)));
        assert_ne!(key_spans[0].style.fg, value_spans[0].style.fg);
    }

    #[test]
    fn meta_row_hints_follow_the_state_machine() {
        let flat = |spans: Vec<Span>| -> String {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        let mut app = test_app();
        // idle · empty → discovery hint
        assert!(flat(context_hints(&app)).contains("^K keys"));
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
            s.contains("⏎ queue") && s.contains("^x steer") && s.contains("esc interrupt"),
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

        // 24-row terminal: the popup caps at 22 rows and must scroll.
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
        assert_eq!(app.picker_page_rows, 20, "page size = visible rows");
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
