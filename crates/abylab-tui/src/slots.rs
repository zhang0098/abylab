//! Terminal-native rendering for the read-only view overlay (`/keys` and
//! other builtin chrome).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;

use crate::theme::Theme;
use crate::transcript::wrap;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum TuiNode {
    Ascii {
        lines: Vec<String>,
        #[serde(default)]
        tone: Option<String>,
    },
    Text {
        text: String,
        #[serde(default)]
        tone: Option<String>,
    },
    Group {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        tone: Option<String>,
        children: Vec<TuiNode>,
    },
    Markdown {
        text: String,
        #[serde(default)]
        streaming: bool,
    },
    Reasoning {
        text: String,
        done: bool,
        #[serde(default)]
        seconds: Option<f64>,
    },
    User {
        text: String,
        #[serde(default)]
        queued: bool,
    },
    Generic {
        title: String,
        body: String,
        #[serde(default)]
        status: Option<String>,
    },
    Terminal {
        title: String,
        body: String,
        #[serde(default)]
        exit: Option<i64>,
    },
    Diff {
        title: String,
        unified: String,
        #[serde(default)]
        path: Option<String>,
    },
    Image {
        name: String,
        mime: String,
        #[serde(rename = "dataBase64", default)]
        data_base64: Option<String>,
    },
    Notice {
        level: String,
        text: String,
    },
    Unknown {
        want: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        detail: Option<String>,
    },
}

fn styled_wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        for line in wrap(raw, width.max(1)) {
            lines.push(Line::from(Span::styled(line, style)));
        }
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

fn indent(lines: Vec<Line<'static>>, prefix: &str, color: Color) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|mut line| {
            let mut spans = vec![Span::styled(prefix.to_string(), Style::default().fg(color))];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}

fn token_color(theme: &Theme, name: &str) -> Color {
    match name {
        "bg" => theme.bg,
        "surface" => theme.surface,
        "panel" => theme.panel,
        "fg" => theme.fg,
        "fg_secondary" => theme.fg_secondary,
        "fg_tertiary" => theme.fg_tertiary,
        "caption" => theme.caption,
        "brand" => theme.brand,
        "brand_soft" => theme.brand_soft,
        "bubble_bg" => theme.bubble_bg,
        "bubble_fg" => theme.bubble_fg,
        "border" => theme.border,
        "code_bg" => theme.code_bg,
        "ok" => theme.ok,
        "warn" => theme.warn,
        "err" => theme.err,
        "hint" => theme.hint,
        "chip_bg" => theme.chip_bg,
        _ => theme.fg_secondary,
    }
}

fn render_node(node: &TuiNode, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    match node {
        TuiNode::Ascii { lines, tone, .. } => styled_wrapped(
            &lines.join("\n"),
            width,
            Style::default().fg(tone
                .as_deref()
                .map(|name| token_color(theme, name))
                .unwrap_or(theme.fg)),
        ),
        TuiNode::Text { text, tone, .. } => styled_wrapped(
            text,
            width,
            Style::default().fg(tone
                .as_deref()
                .map(|name| token_color(theme, name))
                .unwrap_or(theme.fg_secondary)),
        ),
        TuiNode::Group {
            title,
            tone: accent_tone,
            children,
            ..
        } => {
            let accent = accent_tone
                .as_deref()
                .map(|name| token_color(theme, name))
                .unwrap_or(theme.brand);
            let mut lines = Vec::new();
            if let Some(title) = title {
                lines.push(Line::from(vec![
                    Span::styled("◆ ", Style::default().fg(accent)),
                    Span::styled(
                        title.clone(),
                        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                    ),
                ]));
            }
            for child in children {
                lines.extend(indent(
                    render_node(child, theme, width.saturating_sub(2)),
                    "  ",
                    theme.border,
                ));
            }
            lines
        }
        TuiNode::Markdown {
            text, streaming, ..
        } => {
            let mut lines = crate::markdown::render(text, theme, width);
            if *streaming {
                lines.push(Line::from(Span::styled(
                    "…",
                    Style::default().fg(theme.brand),
                )));
            }
            lines
        }
        TuiNode::Reasoning {
            text,
            done,
            seconds,
            ..
        } => {
            let label = seconds
                .map(|seconds| format!("thought · {seconds:.1}s"))
                .unwrap_or_else(|| "thought".into());
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    if *done { "✦ " } else { "✦ … " },
                    Style::default().fg(theme.brand),
                ),
                Span::styled(
                    label,
                    Style::default()
                        .fg(theme.caption)
                        .add_modifier(Modifier::ITALIC),
                ),
            ])];
            lines.extend(styled_wrapped(
                text,
                width,
                Style::default()
                    .fg(theme.fg_tertiary)
                    .add_modifier(Modifier::ITALIC),
            ));
            lines
        }
        TuiNode::User { text, queued, .. } => {
            let prefix = if *queued { "› queued · " } else { "› " };
            indent(
                styled_wrapped(
                    text,
                    width.saturating_sub(prefix.len()),
                    Style::default().fg(theme.bubble_fg),
                ),
                prefix,
                theme.brand,
            )
        }
        TuiNode::Generic {
            title,
            body,
            status,
            ..
        } => {
            let (icon, color) = match status.as_deref() {
                Some("running") => ("●", theme.brand),
                Some("ok") => ("✓", theme.ok),
                Some("err") => ("×", theme.err),
                _ => ("◇", theme.caption),
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(color)),
                Span::styled(
                    title.clone(),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
            ])];
            lines.extend(indent(
                styled_wrapped(
                    body,
                    width.saturating_sub(2),
                    Style::default().fg(theme.fg_secondary),
                ),
                "  ",
                theme.border,
            ));
            lines
        }
        TuiNode::Terminal {
            title, body, exit, ..
        } => {
            let suffix = exit
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let mut lines = vec![Line::from(vec![
                Span::styled("$ ", Style::default().fg(theme.brand)),
                Span::styled(
                    format!("{title}{suffix}"),
                    Style::default().fg(theme.caption),
                ),
            ])];
            lines.extend(styled_wrapped(
                body,
                width,
                Style::default().fg(theme.fg_secondary).bg(theme.code_bg),
            ));
            lines
        }
        TuiNode::Diff {
            title,
            unified,
            path,
            ..
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("Δ ", Style::default().fg(theme.brand)),
                Span::styled(
                    path.as_deref().unwrap_or(title).to_string(),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
            ])];
            for raw in unified.lines() {
                let color = if raw.starts_with('+') && !raw.starts_with("+++") {
                    theme.ok
                } else if raw.starts_with('-') && !raw.starts_with("---") {
                    theme.err
                } else {
                    theme.fg_tertiary
                };
                lines.extend(styled_wrapped(
                    raw,
                    width,
                    Style::default().fg(color).bg(theme.code_bg),
                ));
            }
            lines
        }
        TuiNode::Image {
            name,
            mime,
            data_base64,
            ..
        } => vec![Line::from(vec![
            Span::styled("▧ ", Style::default().fg(theme.brand)),
            Span::styled(name.clone(), Style::default().fg(theme.fg)),
            Span::styled(
                format!(
                    " · {mime}{}",
                    data_base64
                        .as_ref()
                        .map(|data| format!(" · {} chars", data.len()))
                        .unwrap_or_default()
                ),
                Style::default().fg(theme.caption),
            ),
        ])],
        TuiNode::Notice { level, text, .. } => {
            let (icon, color) = match level.as_str() {
                "warn" => ("!", theme.warn),
                "error" => ("×", theme.err),
                _ => ("i", theme.hint),
            };
            indent(
                styled_wrapped(
                    text,
                    width.saturating_sub(2),
                    Style::default().fg(theme.fg_secondary),
                ),
                &format!("{icon} "),
                color,
            )
        }
        TuiNode::Unknown {
            want,
            title,
            detail,
            ..
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("? ", Style::default().fg(theme.warn)),
                Span::styled(
                    title.as_deref().unwrap_or(want).to_string(),
                    Style::default().fg(theme.fg),
                ),
            ])];
            if let Some(detail) = detail {
                lines.extend(indent(
                    styled_wrapped(
                        detail,
                        width.saturating_sub(2),
                        Style::default().fg(theme.caption),
                    ),
                    "  ",
                    theme.border,
                ));
            }
            lines
        }
    }
}

pub fn render_nodes(nodes: &[TuiNode], theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.extend(render_node(node, theme, width));
    }
    lines
}
