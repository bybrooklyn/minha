use crate::app::{
    AgentView, App, COMMAND_PALETTE, Diagnostic, DrawerTab, Overlay, SystemTone, TranscriptItem,
};
use crate::editor::EditorLayout;
use minha_core::protocol::{AgentState, ClarificationStatus, IncidentSeverity, PlanTaskState, TodoState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;
const MUTED: Color = Color::DarkGray;
const ACTIVE: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RasterSurface {
    pub(crate) rect: Rect,
    pub(crate) fill: [u8; 3],
}

fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"))
        || std::env::var("TERM").is_ok_and(|value| value.contains("direct"))
}

fn background() -> Color {
    if truecolor() {
        Color::Rgb(5, 12, 24)
    } else {
        Color::Black
    }
}

fn surface() -> Color {
    if truecolor() {
        Color::Rgb(10, 24, 43)
    } else {
        Color::Indexed(17)
    }
}

fn surface_alt() -> Color {
    if truecolor() {
        Color::Rgb(15, 34, 57)
    } else {
        Color::Indexed(18)
    }
}

fn border() -> Color {
    if truecolor() {
        Color::Rgb(43, 70, 101)
    } else {
        Color::Indexed(67)
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(background())), area);
    if area.width < 36 || area.height < 8 {
        draw_tiny(frame, app, area);
        apply_theme(frame, app);
        return;
    }

    let composer_height = composer_height(app, area.width);
    let task_height = if app.tasks_visible && !app.plan.is_empty() {
        (app.plan.len() as u16 + 1).clamp(2, 6)
    } else {
        0
    };
    let activity_height = if app.running {
        1 + u16::from(app.todo_active + app.todo_blocked + app.todo_completed > 0)
    } else {
        0
    };
    let question_height = clarification_height(app, area.width.min(124));
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(task_height),
        Constraint::Length(activity_height),
        Constraint::Length(question_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, app, rows[0]);
    let wide_drawer = app.drawer_visible && area.width >= 110 && app.focused_agent.is_none();
    if wide_drawer {
        let columns = Layout::horizontal([Constraint::Min(52), Constraint::Length(48)])
            .split(centered_width(rows[1], 172));
        draw_transcript(frame, app, columns[0]);
        draw_drawer(frame, app, columns[1]);
    } else {
        draw_transcript(frame, app, centered_width(rows[1], 124));
    }
    if task_height > 0 {
        draw_tasks(frame, app, centered_width(rows[2], 124));
    }
    if activity_height > 0 {
        draw_live_status(frame, app, centered_width(rows[3], 124));
    }
    let composer = centered_width(rows[5], 124);
    draw_composer(frame, app, composer);
    draw_completion_popup(frame, app, composer, area);
    draw_footer(frame, app, rows[6]);

    if app.drawer_visible && !wide_drawer && app.focused_agent.is_none() {
        let overlay = if area.width < 70 {
            rows[1].inner(Margin::new(1, 0))
        } else {
            let width = area.width.saturating_sub(4).min(49);
            Rect {
                x: area.right().saturating_sub(width),
                y: rows[1].y,
                width,
                height: rows[1].height,
            }
        };
        frame.render_widget(Clear, overlay);
        draw_drawer(frame, app, overlay);
    }
    draw_overlay(frame, app, area);
    draw_clarification_modal(frame, app, area);
    apply_theme(frame, app);
}

pub(crate) fn raster_surfaces(app: &App, area: Rect) -> Vec<RasterSurface> {
    if app.active_surface_renderer != "kitty" || area.width < 36 || area.height < 8 {
        return Vec::new();
    }
    let composer_height = composer_height(app, area.width);
    let task_height = if app.tasks_visible && !app.plan.is_empty() {
        (app.plan.len() as u16 + 1).clamp(2, 6)
    } else {
        0
    };
    let activity_height = if app.running {
        1 + u16::from(app.todo_active + app.todo_blocked + app.todo_completed > 0)
    } else {
        0
    };
    let question_height = clarification_height(app, area.width.min(124));
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(task_height),
        Constraint::Length(activity_height),
        Constraint::Length(question_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .split(area);
    let mut surfaces = vec![RasterSurface {
        rect: centered_width(rows[5], 124),
        fill: [15, 34, 57],
    }];
    if app.items.is_empty() && app.overlay.is_none() {
        let transcript = centered_width(rows[1], 124).inner(Margin::new(2, 0));
        let width = transcript.width.min(68);
        surfaces.push(RasterSurface {
            rect: Rect {
                x: transcript.x + transcript.width.saturating_sub(width) / 2,
                y: transcript.y + transcript.height.saturating_sub(8) / 3,
                width,
                height: 8.min(transcript.height),
            },
            fill: [10, 24, 43],
        });
    }
    if let Some(overlay) = &app.overlay {
        let (max_width, max_height) = match overlay {
            Overlay::Status => (104, 32),
            Overlay::Context => (96, 24),
            Overlay::Books => (112, 26),
            Overlay::Doctor => (84, 22),
            _ => (70, 18),
        };
        surfaces.push(RasterSurface {
            rect: centered(
                area,
                max_width.min(area.width.saturating_sub(4)),
                max_height.min(area.height.saturating_sub(2)),
            ),
            fill: [10, 24, 43],
        });
    }
    if let Some(rect) = clarification_modal_rect(app, area) {
        surfaces.push(RasterSurface {
            rect,
            fill: [10, 24, 43],
        });
    }
    surfaces
}

fn centered_width(area: Rect, max_width: u16) -> Rect {
    let width = area.width.min(max_width);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y,
        width,
        height: area.height,
    }
}

fn rounded_surfaces(app: &App) -> bool {
    !matches!(app.active_surface_renderer.as_str(), "square") && !matches!(app.theme.as_str(), "no_color")
}

fn surface_block<'a>(app: &App, title: Line<'a>, accent: Color, fill: Color) -> Block<'a> {
    let rounded = rounded_surfaces(app);
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if rounded {
            BorderType::QuadrantInside
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(if rounded { fill } else { accent }))
        .style(Style::default().bg(fill))
}

fn finish_surface(frame: &mut Frame<'_>, app: &App, area: Rect, fill: Color) {
    if !rounded_surfaces(app) || area.width < 2 || area.height < 2 {
        return;
    }
    for (x, y) in [
        (area.x, area.y),
        (area.right().saturating_sub(1), area.y),
        (area.x, area.bottom().saturating_sub(1)),
        (area.right().saturating_sub(1), area.bottom().saturating_sub(1)),
    ] {
        frame.buffer_mut()[(x, y)].set_fg(fill).set_bg(background());
    }
}

fn apply_theme(frame: &mut Frame<'_>, app: &App) {
    let mut theme = app.theme.as_str();
    if theme == "auto" {
        theme = if std::env::var("COLORFGBG")
            .ok()
            .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
            .is_some_and(|background| background >= 8)
        {
            "light"
        } else {
            "dark"
        };
    }
    for cell in &mut frame.buffer_mut().content {
        match theme {
            "no_color" => {
                cell.set_fg(Color::Reset).set_bg(Color::Reset);
            }
            "light" => {
                cell.set_bg(Color::Reset);
                cell.set_fg(match cell.fg {
                    Color::White | Color::Gray => Color::Black,
                    Color::DarkGray => Color::Gray,
                    Color::Cyan => Color::Blue,
                    Color::Green => Color::DarkGray,
                    Color::Yellow => Color::Magenta,
                    color => color,
                });
            }
            "ansi16" => {
                cell.set_bg(Color::Reset);
                if matches!(cell.fg, Color::Rgb(..) | Color::Indexed(_)) {
                    cell.set_fg(Color::Gray);
                }
            }
            "high_contrast" => {
                cell.set_bg(Color::Reset);
                if matches!(cell.fg, Color::DarkGray) {
                    cell.set_fg(Color::Gray);
                } else if matches!(cell.fg, Color::Rgb(..) | Color::Indexed(_)) {
                    cell.set_fg(Color::White);
                }
            }
            _ => {}
        }
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let path = app
        .root
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| app.root.to_string_lossy());
    let focus = app
        .focused_agent
        .and_then(|id| app.agents.iter().find(|agent| agent.id == id))
        .map(|agent| format!(" / {}", short_role(&agent.role)))
        .unwrap_or_default();
    let left = Line::from(vec![
        Span::styled(" minha ", Style::default().fg(Color::Black).bg(BRIGHT).bold()),
        Span::styled(format!("  {path}{focus}"), Style::default().fg(MUTED)),
    ]);
    let right = format!("{}  {}", app.mode.label(), app.status);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(surface()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(left).style(Style::default().bg(surface())), inner);
    let width = right.chars().count() as u16;
    if width < inner.width {
        frame.render_widget(
            Paragraph::new(Line::styled(right, Style::default().fg(status_color(app.state))))
                .style(Style::default().bg(surface())),
            Rect {
                x: inner.right() - width,
                y: inner.y,
                width,
                height: 1,
            },
        );
    }
}

fn draw_transcript(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let inner = area.inner(Margin::new(2, 0));
    if app.items.is_empty() {
        draw_welcome(frame, app, inner);
        return;
    }
    let mut layout = app.transcript_layout.borrow_mut();
    if layout.width != inner.width
        || layout.revision != app.transcript_revision
        || layout.focused_agent != app.focused_agent
    {
        let mut lines = Vec::new();
        let visible = app.visible_items().collect::<Vec<_>>();
        let mut index = 0;
        while index < visible.len() {
            if let TranscriptItem::Tool { name, .. } = visible[index] {
                let kind = activity_kind(name);
                let mut end = index + 1;
                while end < visible.len()
                    && matches!(visible[end], TranscriptItem::Tool { name, .. } if activity_kind(name) == kind)
                {
                    end += 1;
                }
                lines.extend(activity_group_lines(&visible[index..end], kind, inner.width));
                index = end;
            } else {
                lines.extend(item_lines(visible[index], inner.width));
                index += 1;
            }
        }
        layout.width = inner.width;
        layout.revision = app.transcript_revision;
        layout.focused_agent = app.focused_agent;
        layout.lines = lines;
        layout.builds = layout.builds.saturating_add(1);
    }
    let estimated_height = layout.lines.len().min(usize::from(u16::MAX)) as u16;
    let max_scroll = estimated_height.saturating_sub(inner.height);
    layout.max_scroll = max_scroll;
    let scroll = if app.scroll == u16::MAX {
        max_scroll
    } else {
        app.scroll.min(max_scroll)
    };
    let start = usize::from(scroll).min(layout.lines.len());
    let end = start
        .saturating_add(usize::from(inner.height))
        .min(layout.lines.len());
    let viewport = layout.lines[start..end].to_vec();
    layout.last_viewport_lines = viewport.len();
    frame.render_widget(
        Paragraph::new(Text::from(viewport)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn activity_kind(name: &str) -> &'static str {
    match name {
        "read_files" => "Explored",
        "search" => "Searched",
        "apply_patch" => "Edited",
        "quality" | "exec" => "Ran checks",
        "hive" => "Delegated",
        _ => "Used tools",
    }
}

fn activity_group_lines(items: &[&TranscriptItem], kind: &str, width: u16) -> Vec<Line<'static>> {
    let mut running = false;
    let mut failed = false;
    let mut expanded = false;
    let mut targets = Vec::new();
    for item in items {
        if let TranscriptItem::Tool {
            arguments,
            exit_code,
            running: item_running,
            expanded: item_expanded,
            name,
            ..
        } = item
        {
            running |= *item_running;
            failed |= exit_code.is_some_and(|code| code != 0);
            expanded |= *item_expanded;
            let target = activity_target(name, arguments);
            if !target.is_empty() && !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    let marker = if running {
        "◐"
    } else if failed {
        "×"
    } else {
        "✓"
    };
    let color = if running {
        ACTIVE
    } else if failed {
        BAD
    } else {
        MUTED
    };
    let summary = if targets.is_empty() {
        format!(
            "{} operation{}",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        )
    } else {
        truncate_display(&targets.join(", "), usize::from(width.saturating_sub(18).max(8)))
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {marker} "), Style::default().fg(color)),
        Span::styled(kind.to_owned(), Style::default().fg(color).bold()),
        Span::styled(format!("  {summary}"), Style::default().fg(TEXT)),
    ])];
    if expanded {
        for item in items {
            if let TranscriptItem::Tool {
                name,
                arguments,
                output,
                exit_code,
                ..
            } = item
            {
                lines.push(Line::styled(
                    format!(
                        "    {name} · {}",
                        exit_code.map_or("done".into(), |code| format!("exit {code}"))
                    ),
                    Style::default().fg(MUTED),
                ));
                if !arguments.is_empty() {
                    lines.extend(
                        wrap_display(arguments, usize::from(width.saturating_sub(6).max(1)))
                            .into_iter()
                            .map(|line| Line::styled(format!("      {line}"), Style::default().fg(MUTED))),
                    );
                }
                lines.extend(
                    output
                        .lines()
                        .take(80)
                        .flat_map(|line| wrap_display(line, usize::from(width.saturating_sub(6).max(1))))
                        .map(|line| Line::styled(format!("      {line}"), Style::default().fg(TEXT))),
                );
            }
        }
    } else if items
        .iter()
        .any(|item| matches!(item, TranscriptItem::Tool { output, .. } if !output.is_empty()))
    {
        lines.push(Line::styled(
            "    Ctrl-O expands this activity",
            Style::default().fg(MUTED),
        ));
    }
    lines.push(Line::raw(""));
    lines
}

fn activity_target(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate_display(arguments, 80);
    };
    match name {
        "search" => value
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| name.into(), |query| format!("{query:?}")),
        "exec" | "quality" => value
            .get("argv")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| name.into()),
        _ => collect_paths(&value)
            .into_iter()
            .take(4)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn collect_paths(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .flat_map(|(key, value)| {
                if matches!(key.as_str(), "path" | "file" | "directory") {
                    value.as_str().map(str::to_owned).into_iter().collect()
                } else {
                    collect_paths(value)
                }
            })
            .collect(),
        serde_json::Value::Array(values) => values.iter().flat_map(collect_paths).collect(),
        _ => Vec::new(),
    }
}

fn truncate_display(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let next = UnicodeWidthStr::width(grapheme);
        if used + next + 1 > width {
            break;
        }
        output.push_str(grapheme);
        used += next;
    }
    output.push('…');
    output
}

fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = area.width.min(68);
    let welcome = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(8) / 3,
        width,
        height: 8.min(area.height),
    };
    let text = vec![
        Line::styled(
            "What should the hive work on?",
            Style::default().fg(BRIGHT).bold(),
        ),
        Line::raw(""),
        Line::styled(
            "Luna coordinates; Spark handles independent branches and audits.",
            Style::default().fg(TEXT),
        ),
        Line::styled(
            "Type normally, or use /plan, /audit, /review, !command, or ?.",
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
        Line::styled(
            format!("{} · no MCP · automatic compaction", app.model),
            Style::default().fg(MUTED),
        ),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .block(surface_block(
                app,
                Line::styled(" minha ", Style::default().fg(ACTIVE).bold()),
                border(),
                surface(),
            ))
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: true }),
        welcome,
    );
    finish_surface(frame, app, welcome, surface());
}

fn item_lines(item: &TranscriptItem, width: u16) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::User { text, steering } => {
            let label = if *steering { "steer" } else { "you" };
            boxed_lines(label, text, ACTIVE, surface_alt(), width)
        }
        TranscriptItem::Assistant {
            role,
            text,
            streaming,
            ..
        } => assistant_lines(role, text, *streaming, width),
        TranscriptItem::Tool {
            name,
            arguments,
            output,
            exit_code,
            running,
            expanded,
            ..
        } => {
            let color = if *running {
                ACTIVE
            } else if exit_code.is_some_and(|code| code != 0) {
                BAD
            } else {
                MUTED
            };
            let state = if *running {
                "running"
            } else if let Some(code) = exit_code {
                if *code == 0 { "done" } else { "failed" }
            } else {
                "done"
            };
            let mut body = Vec::new();
            if !arguments.is_empty() {
                body.push(arguments.clone());
            }
            if *expanded && !output.is_empty() {
                body.extend(output.lines().take(160).map(str::to_owned));
                if output.lines().count() > 160 {
                    body.push("… output truncated in view".into());
                }
            } else if !output.is_empty() {
                body.push("Ctrl-O expands output".into());
            }
            boxed_lines(
                &format!("tool · {name} · {state}"),
                &body.join("\n"),
                color,
                surface(),
                width,
            )
        }
        TranscriptItem::Diff { path, diff, expanded } => {
            let additions = diff
                .lines()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                .count();
            let deletions = diff
                .lines()
                .filter(|line| line.starts_with('-') && !line.starts_with("---"))
                .count();
            let mut body = Vec::new();
            if *expanded {
                body.extend(diff.lines().take(320).map(str::to_owned));
                if diff.lines().count() > 320 {
                    body.push("… diff truncated in view".into());
                }
            } else {
                body.push("Ctrl-O expands diff".into());
            }
            boxed_lines(
                &format!(
                    "diff · {} · +{additions} -{deletions}",
                    path.clone().unwrap_or_else(|| "working tree".into())
                ),
                &body.join("\n"),
                ACTIVE,
                surface(),
                width,
            )
        }
        TranscriptItem::System { tone, text } if matches!(tone, SystemTone::Warning | SystemTone::Error) => {
            boxed_lines(
                if *tone == SystemTone::Error {
                    "error"
                } else {
                    "warning"
                },
                text,
                tone_color(*tone),
                surface(),
                width,
            )
        }
        TranscriptItem::System { tone, text } => {
            let mut lines = styled_wrap(
                vec![StyledChunk::new(
                    text.clone(),
                    Style::default().fg(tone_color(*tone)),
                )],
                usize::from(width.max(1)),
                vec![Span::styled("  • ", Style::default().fg(tone_color(*tone)))],
                vec![Span::raw("    ")],
            );
            lines.push(Line::raw(""));
            lines
        }
        TranscriptItem::Status { lines } => boxed_lines(
            "status · local data",
            &lines.join("\n"),
            ACTIVE,
            surface_alt(),
            width,
        ),
        TranscriptItem::Coordination {
            room_id,
            sender,
            recipient,
            kind,
            summary,
        } => {
            let label = match kind.as_str() {
                "blocker" => "Blocked",
                "handoff" => "Handed off",
                "decision" | "artifact_reference" => "Integrated",
                "request" => "Coordinated",
                _ => "Coordinated",
            };
            let color = if kind == "blocker" { WARN } else { MUTED };
            let mut lines = styled_wrap(
                vec![StyledChunk::new(summary.clone(), Style::default().fg(TEXT))],
                usize::from(width.max(1)),
                vec![
                    Span::styled("  • ", Style::default().fg(color)),
                    Span::styled(format!("{label}  "), Style::default().fg(color).bold()),
                ],
                vec![Span::raw("    ")],
            );
            lines.push(Line::styled(
                format!("    {sender} → {recipient} · {room_id}"),
                Style::default().fg(MUTED),
            ));
            lines.push(Line::raw(""));
            lines
        }
    }
}

#[derive(Clone)]
struct StyledChunk {
    text: String,
    style: Style,
}

impl StyledChunk {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

fn assistant_lines(role: &str, text: &str, streaming: bool, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(if streaming { ACTIVE } else { GOOD })),
        Span::styled(
            truncate_display(&short_role(role), width.saturating_sub(2).max(1)),
            Style::default().fg(MUTED),
        ),
    ])];
    let control_stream = streaming_control_payload(text);
    let text = if control_stream { "" } else { text };
    let mut fenced = false;
    for source in normalize_markdown_lines(text) {
        let source = source.as_str();
        let trimmed = source.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            fenced = !fenced;
            if !language.trim().is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  code  ", Style::default().fg(ACTIVE).bold()),
                    Span::styled(language.trim().to_owned(), Style::default().fg(MUTED)),
                ]));
            }
            continue;
        }
        if source.is_empty() {
            lines.push(Line::raw(""));
            continue;
        }
        if fenced {
            for part in wrap_display(source, width.saturating_sub(4).max(1)) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(ACTIVE)),
                    Span::styled(part, Style::default().fg(BRIGHT)),
                ]));
            }
            continue;
        }
        let heading = trimmed
            .char_indices()
            .take_while(|(_, character)| *character == '#')
            .count();
        if (1..=6).contains(&heading) && trimmed.as_bytes().get(heading) == Some(&b' ') {
            let content = trimmed[heading + 1..].to_owned();
            lines.extend(styled_wrap(
                parse_inline(&content, Style::default().fg(BRIGHT).bold()),
                width,
                vec![Span::raw("  ")],
                vec![Span::raw("  ")],
            ));
            continue;
        }
        if let Some(content) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            let (marker, content) = if let Some(content) = content.strip_prefix("[x] ") {
                ("  ☑ ", content)
            } else if let Some(content) = content.strip_prefix("[X] ") {
                ("  ☑ ", content)
            } else if let Some(content) = content.strip_prefix("[ ] ") {
                ("  ☐ ", content)
            } else {
                ("  • ", content)
            };
            lines.extend(styled_wrap(
                parse_inline(content, Style::default().fg(TEXT)),
                width,
                vec![Span::styled(marker, Style::default().fg(ACTIVE))],
                vec![Span::raw("    ")],
            ));
            continue;
        }
        if let Some((marker, content)) = ordered_item(trimmed) {
            let continuation = " ".repeat(marker.width() + 2);
            lines.extend(styled_wrap(
                parse_inline(content, Style::default().fg(TEXT)),
                width,
                vec![Span::styled(format!("  {marker} "), Style::default().fg(ACTIVE))],
                vec![Span::raw(continuation)],
            ));
            continue;
        }
        if let Some(content) = trimmed.strip_prefix("> ") {
            lines.extend(styled_wrap(
                parse_inline(content, Style::default().fg(MUTED).italic()),
                width,
                vec![Span::styled("  │ ", Style::default().fg(ACTIVE))],
                vec![Span::raw("    ")],
            ));
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells = trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>();
            let separator = cells.iter().all(|cell| {
                let marker = cell.trim_matches(':');
                !marker.is_empty() && marker.chars().all(|character| character == '-')
            });
            if !separator {
                lines.extend(styled_wrap(
                    parse_inline(&cells.join(" │ "), Style::default().fg(TEXT)),
                    width,
                    vec![Span::raw("  ")],
                    vec![Span::raw("  ")],
                ));
            }
            continue;
        }
        lines.extend(styled_wrap(
            parse_inline(source, Style::default().fg(TEXT)),
            width,
            vec![Span::raw("  ")],
            vec![Span::raw("  ")],
        ));
    }
    if streaming {
        lines.push(Line::styled("  ▍", Style::default().fg(ACTIVE)));
    }
    lines.push(Line::raw(""));
    lines
}

fn normalize_markdown_lines(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut paragraph = String::new();
    let mut fenced = false;
    let flush = |output: &mut Vec<String>, paragraph: &mut String| {
        if !paragraph.is_empty() {
            output.push(std::mem::take(paragraph));
        }
    };
    for source in text.lines() {
        let trimmed = source.trim_start();
        let fence = trimmed.starts_with("```");
        let structural = fenced
            || fence
            || source.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("> ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
            || ordered_item(trimmed).is_some()
            || (trimmed.starts_with('|') && trimmed.ends_with('|'));
        if structural {
            flush(&mut output, &mut paragraph);
            output.push(source.to_owned());
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(trimmed);
        }
        if fence {
            fenced = !fenced;
        }
    }
    flush(&mut output, &mut paragraph);
    output
}

fn streaming_control_payload(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.is_empty()
        && ("<minha-".starts_with(trimmed)
            || trimmed.starts_with("<minha-")
            || trimmed.starts_with("</minha-"))
}

fn ordered_item(text: &str) -> Option<(&str, &str)> {
    let dot = text.find('.')?;
    let marker = &text[..=dot];
    if dot == 0 || !text[..dot].chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    Some((marker, text.get(dot + 1..)?.trim_start()))
}

fn parse_inline(text: &str, base: Style) -> Vec<StyledChunk> {
    let mut chunks = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            chunks.push(StyledChunk::new(&rest[..end], base.bold()));
            remaining = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("~~")
            && let Some(end) = rest.find("~~")
        {
            chunks.push(StyledChunk::new(&rest[..end], base.crossed_out()));
            remaining = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = remaining.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            chunks.push(StyledChunk::new(
                &rest[..end],
                Style::default().fg(BRIGHT).bg(surface_alt()),
            ));
            remaining = &rest[end + 1..];
            continue;
        }
        if let Some(rest) = remaining.strip_prefix('*')
            && let Some(end) = rest.find('*')
        {
            chunks.push(StyledChunk::new(&rest[..end], base.italic()));
            remaining = &rest[end + 1..];
            continue;
        }
        if let Some(label) = remaining.strip_prefix('[')
            && let Some(label_end) = label.find("](")
            && let Some(url_end) = label[label_end + 2..].find(')')
        {
            let url_start = label_end + 2;
            chunks.push(StyledChunk::new(
                &label[..label_end],
                Style::default().fg(ACTIVE).underlined(),
            ));
            let url = &label[url_start..url_start + url_end];
            chunks.push(StyledChunk::new(format!(" <{url}>"), Style::default().fg(MUTED)));
            remaining = &label[url_start + url_end + 1..];
            continue;
        }
        let next = remaining
            .char_indices()
            .skip(1)
            .find(|(_, character)| matches!(character, '*' | '~' | '`' | '['))
            .map_or(remaining.len(), |(index, _)| index);
        chunks.push(StyledChunk::new(&remaining[..next], base));
        remaining = &remaining[next..];
    }
    chunks
}

fn styled_wrap(
    chunks: Vec<StyledChunk>,
    width: usize,
    first_prefix: Vec<Span<'static>>,
    continuation_prefix: Vec<Span<'static>>,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let prefix_width = |spans: &[Span<'static>]| {
        spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>()
    };
    let mut lines = Vec::new();
    let mut spans = first_prefix;
    let mut used = prefix_width(&spans);
    let mut line_prefix_width = used;
    for chunk in chunks {
        for segment in chunk.text.split_word_bounds() {
            let mut segment = segment;
            let segment_width = UnicodeWidthStr::width(segment);
            if used + segment_width > width && used > line_prefix_width {
                lines.push(Line::from(std::mem::take(&mut spans)));
                spans = continuation_prefix.clone();
                used = prefix_width(&spans);
                line_prefix_width = used;
                segment = segment.trim_start();
            }
            if segment.is_empty() {
                continue;
            }
            if UnicodeWidthStr::width(segment) + used <= width {
                used += UnicodeWidthStr::width(segment);
                spans.push(Span::styled(segment.to_owned(), chunk.style));
                continue;
            }
            for grapheme in segment.graphemes(true) {
                let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
                if used + grapheme_width > width && used > line_prefix_width {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                    spans = continuation_prefix.clone();
                    used = prefix_width(&spans);
                    line_prefix_width = used;
                }
                spans.push(Span::styled(grapheme.to_owned(), chunk.style));
                used += grapheme_width;
            }
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn boxed_lines(title: &str, body: &str, accent: Color, background: Color, width: u16) -> Vec<Line<'static>> {
    let _ = background;
    let content_width = usize::from(width.saturating_sub(3).max(1));
    let mut lines = vec![Line::from(vec![
        Span::styled("  • ", Style::default().fg(accent)),
        Span::styled(title.to_owned(), Style::default().fg(accent).bold()),
    ])];
    for source in body.lines().chain(body.is_empty().then_some("")) {
        for part in wrap_display(source, content_width) {
            lines.push(Line::styled(format!("    {part}"), Style::default().fg(TEXT)));
        }
    }
    lines.push(Line::raw(""));
    lines
}

fn wrap_display(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for segment in text.split_word_bounds() {
        let segment_width = UnicodeWidthStr::width(segment);
        if current_width + segment_width <= width {
            current.push_str(segment);
            current_width += segment_width;
            continue;
        }
        if !current.is_empty() {
            lines.push(current.trim_end().to_owned());
            current.clear();
            current_width = 0;
        }
        let segment = segment.trim_start();
        if UnicodeWidthStr::width(segment) <= width {
            current.push_str(segment);
            current_width = UnicodeWidthStr::width(segment);
            continue;
        }
        for grapheme in segment.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            if current_width + grapheme_width > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(grapheme);
            current_width += grapheme_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn truncate_chars(text: &str, width: usize) -> String {
    let mut value = text.chars().take(width).collect::<String>();
    if text.chars().count() > width && width > 0 {
        value.pop();
        value.push('…');
    }
    value
}

fn draw_drawer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    match app.drawer_tab {
        DrawerTab::Activity => draw_agents(frame, app, area),
        DrawerTab::Work => draw_work(frame, app, area),
        DrawerTab::Board => draw_board(frame, app, area),
        DrawerTab::Problems => draw_problems(frame, app, area),
    }
}

fn draw_agents(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(drawer_tabs(DrawerTab::Activity))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(surface()));
    if app.agents.is_empty() {
        frame.render_widget(
            Paragraph::new("No agents spawned.\nLuna creates workers only when useful.")
                .block(block)
                .style(Style::default().fg(MUTED).bg(surface()))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let items = app.agents.iter().map(agent_item).collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.selected_agent));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .style(Style::default().bg(surface()))
            .highlight_style(Style::default().bg(surface_alt()).fg(BRIGHT))
            .highlight_symbol(" "),
        area,
        &mut state,
    );
}

fn draw_work(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(drawer_tabs(DrawerTab::Work))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(surface()));
    if app.plan.is_empty() && app.todos.values().all(Vec::is_empty) {
        frame.render_widget(
            Paragraph::new("No work plan yet.\nPlan tasks appear here when Luna decomposes a goal.")
                .block(block)
                .style(Style::default().fg(MUTED).bg(surface()))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let mut items = app
        .plan
        .iter()
        .map(|task| {
            let (marker, color) = task_marker(task.state);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(color)),
                    Span::styled(task.objective.clone(), Style::default().fg(BRIGHT)),
                ]),
                Line::styled(
                    format!(
                        "  {}",
                        task.agent_id
                            .map(|id| short_id(&id.to_string()))
                            .unwrap_or_else(|| "unassigned".into())
                    ),
                    Style::default().fg(MUTED),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let mut agent_todos = app.todos.iter().collect::<Vec<_>>();
    agent_todos.sort_by_key(|(agent_id, _)| agent_id.to_string());
    for (agent_id, todos) in agent_todos {
        let role = app.agents.iter().find(|agent| agent.id == *agent_id).map_or_else(
            || short_id(&agent_id.to_string()),
            |agent| short_role(&agent.role),
        );
        for todo in todos {
            let (marker, color) = match todo.state {
                TodoState::Pending => ("○", MUTED),
                TodoState::InProgress => ("◐", ACTIVE),
                TodoState::Completed => ("●", GOOD),
                TodoState::Blocked => ("!", WARN),
                TodoState::Dropped => ("−", MUTED),
            };
            let blocker = todo.blocker.as_deref().map_or_else(
                || format!("  {role} · r{}", todo.revision),
                |blocker| format!("  {role} · blocked: {blocker}"),
            );
            items.push(ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(color)),
                    Span::styled(todo.objective.clone(), Style::default().fg(TEXT)),
                ]),
                Line::styled(blocker, Style::default().fg(MUTED)),
            ]));
        }
    }
    let mut state = ListState::default().with_selected(Some(app.selected_task));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .style(Style::default().bg(surface()))
            .highlight_style(Style::default().bg(surface_alt()).fg(BRIGHT))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_board(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(drawer_tabs(DrawerTab::Board))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(surface()));
    if app.board.is_empty() {
        frame.render_widget(
            Paragraph::new("No notes yet.\n/note adds one; agents share durable findings here.")
                .block(block)
                .style(Style::default().fg(MUTED).bg(surface()))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let items = app
        .board
        .iter()
        .map(|entry| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{} ", entry.kind), Style::default().fg(ACTIVE).bold()),
                    Span::styled(entry.subject.clone(), Style::default().fg(BRIGHT)),
                ]),
                Line::styled(
                    format!("{} · {} · {}", entry.scope, entry.status, short_id(&entry.id)),
                    Style::default().fg(MUTED),
                ),
                Line::styled(truncate_chars(&entry.body, 90), Style::default().fg(TEXT)),
                Line::raw(""),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.selected_board));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .style(Style::default().bg(surface()))
            .highlight_style(Style::default().bg(surface_alt()).fg(BRIGHT))
            .highlight_symbol(" "),
        area,
        &mut state,
    );
}

fn draw_problems(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default()
        .title(drawer_tabs(DrawerTab::Problems))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(surface()));
    if app.incidents.is_empty() {
        frame.render_widget(
            Paragraph::new("No incidents recorded.\nRuntime warnings and retryable failures collect here.")
                .block(block)
                .style(Style::default().fg(MUTED).bg(surface()))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let items = app
        .incidents
        .iter()
        .map(|incident| {
            let color = incident_color(incident.severity);
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{} ", incident_marker(incident.severity)),
                        Style::default().fg(color),
                    ),
                    Span::styled(incident.code.clone(), Style::default().fg(BRIGHT).bold()),
                ]),
                Line::styled(truncate_chars(&incident.summary, 46), Style::default().fg(TEXT)),
                Line::styled(
                    format!(
                        "  {} · {}",
                        incident.category,
                        if incident.retryable {
                            "retryable"
                        } else {
                            "inspect"
                        }
                    ),
                    Style::default().fg(MUTED),
                ),
                Line::raw(""),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.selected_problem));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .style(Style::default().bg(surface()))
            .highlight_style(Style::default().bg(surface_alt()).fg(BRIGHT))
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn drawer_tabs(active: DrawerTab) -> Line<'static> {
    let tabs = [
        (DrawerTab::Activity, "activity"),
        (DrawerTab::Work, "work"),
        (DrawerTab::Board, "board"),
        (DrawerTab::Problems, "problems"),
    ];
    let mut spans = Vec::with_capacity(tabs.len() * 2);
    for (index, (tab, label)) in tabs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(border())));
        }
        let mut style = Style::default().fg(if tab == active { ACTIVE } else { MUTED });
        if tab == active {
            style = style.bold();
        }
        spans.push(Span::styled(format!(" {label} "), style));
    }
    Line::from(spans)
}

fn agent_item(agent: &AgentView) -> ListItem<'static> {
    ListItem::new(vec![
        Line::from(vec![
            Span::styled(
                state_marker(agent.state),
                Style::default().fg(agent_color(agent.state)),
            ),
            Span::styled(short_role(&agent.role), Style::default().fg(BRIGHT)),
        ]),
        Line::styled(
            format!("  {} · {}", short_model(&agent.model), agent.detail),
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ])
}

fn task_marker(state: PlanTaskState) -> (&'static str, Color) {
    match state {
        PlanTaskState::Pending => ("○", MUTED),
        PlanTaskState::Running => ("◐", ACTIVE),
        PlanTaskState::Completed => ("●", GOOD),
        PlanTaskState::Blocked => ("!", WARN),
        PlanTaskState::Failed => ("×", BAD),
    }
}

fn incident_marker(severity: IncidentSeverity) -> &'static str {
    match severity {
        IncidentSeverity::Info => "·",
        IncidentSeverity::Warning => "!",
        IncidentSeverity::Error => "×",
        IncidentSeverity::Critical => "‼",
    }
}

fn incident_color(severity: IncidentSeverity) -> Color {
    match severity {
        IncidentSeverity::Info => MUTED,
        IncidentSeverity::Warning => WARN,
        IncidentSeverity::Error | IncidentSeverity::Critical => BAD,
    }
}

fn draw_tasks(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = app.plan.iter().take(area.height as usize).map(|task| {
        let (marker, color) = match task.state {
            PlanTaskState::Pending => ("○", MUTED),
            PlanTaskState::Running => ("◐", ACTIVE),
            PlanTaskState::Completed => ("●", GOOD),
            PlanTaskState::Blocked => ("!", WARN),
            PlanTaskState::Failed => ("×", BAD),
        };
        Line::from(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(color)),
            Span::styled(task.objective.clone(), Style::default().fg(TEXT)),
        ])
    });
    frame.render_widget(
        Paragraph::new(Text::from(lines.collect::<Vec<_>>()))
            .block(surface_block(
                app,
                Line::styled(" tasks ", Style::default().fg(MUTED)),
                border(),
                surface(),
            ))
            .style(Style::default().bg(surface())),
        area,
    );
    finish_surface(frame, app, area, surface());
}

fn clarification_height(_app: &App, _width: u16) -> u16 {
    // Blocking questions are rendered as a modal and do not steal transcript
    // or composer height.
    0
}

struct InlineQuestionContent {
    lines: Vec<Line<'static>>,
    option_lines: Vec<(usize, usize, usize)>,
}

fn inline_question_lines(app: &App, width: u16) -> Option<InlineQuestionContent> {
    let clarification = app.clarification.as_ref()?;
    let width = usize::from(width.saturating_sub(4).max(12));
    let mut lines = Vec::new();
    let mut option_lines = Vec::new();
    if clarification.status == ClarificationStatus::Reviewing {
        lines.push(Line::from(vec![
            Span::styled("  ? ", Style::default().fg(ACTIVE).bold()),
            Span::styled("Check my understanding", Style::default().fg(MUTED)),
        ]));
        lines.extend(styled_wrap(
            vec![StyledChunk::new(
                "Does this describe what you want Minha to do?",
                Style::default().fg(BRIGHT).bold(),
            )],
            width,
            vec![Span::raw("    ")],
            vec![Span::raw("    ")],
        ));
        if let Some(brief) = &clarification.brief {
            lines.extend(styled_wrap(
                vec![StyledChunk::new(
                    brief.observed.clone(),
                    Style::default().fg(TEXT),
                )],
                width,
                vec![Span::styled("    Heard: ", Style::default().fg(MUTED))],
                vec![Span::raw("           ")],
            ));
            lines.extend(styled_wrap(
                vec![StyledChunk::new(
                    brief.expected.clone(),
                    Style::default().fg(TEXT),
                )],
                width,
                vec![Span::styled("    Expect: ", Style::default().fg(MUTED))],
                vec![Span::raw("            ")],
            ));
        }
        for (index, (label, description)) in [
            ("Confirm", "Start with this understanding."),
            ("Edit", "Attach a correction before continuing."),
            ("Keep clarifying", "Ask one more focused question."),
            ("Cancel", "Stop without starting work."),
        ]
        .into_iter()
        .enumerate()
        {
            let start = lines.len();
            lines.extend(question_option_lines(
                label,
                description,
                index == app.selected_clarification_option,
                width,
            ));
            option_lines.push((start, lines.len(), index));
        }
    } else {
        let batch = clarification.pending_batch.as_ref()?;
        let question = batch.questions.get(app.selected_clarification_question)?;
        lines.push(Line::from(vec![
            Span::styled("  ? ", Style::default().fg(ACTIVE).bold()),
            Span::styled(
                format!(
                    "{} · {} of {}",
                    question.header,
                    app.selected_clarification_question + 1,
                    batch.questions.len()
                ),
                Style::default().fg(MUTED),
            ),
        ]));
        lines.extend(styled_wrap(
            vec![StyledChunk::new(
                question.question.clone(),
                Style::default().fg(BRIGHT).bold(),
            )],
            width,
            vec![Span::raw("    ")],
            vec![Span::raw("    ")],
        ));
        for (index, option) in question.options.iter().enumerate() {
            let recommendation = if option.recommended { " · Recommended" } else { "" };
            let start = lines.len();
            lines.extend(question_option_lines(
                &option.label,
                &format!("{}{recommendation}", option.description),
                index == app.selected_clarification_option,
                width,
            ));
            option_lines.push((start, lines.len(), index));
        }
        let mut index = question.options.len();
        if question.allow_not_sure {
            let start = lines.len();
            lines.extend(question_option_lines(
                "Not sure",
                "Let Minha use the safest reasonable assumption.",
                index == app.selected_clarification_option,
                width,
            ));
            option_lines.push((start, lines.len(), index));
            index += 1;
        }
        if question.allow_free_text {
            let start = lines.len();
            lines.extend(question_option_lines(
                "Other",
                "Type your answer in the composer.",
                index == app.selected_clarification_option,
                width,
            ));
            option_lines.push((start, lines.len(), index));
        }
    }
    lines.push(Line::styled(
        "    ↑/↓ choose · Enter answer · or type a custom answer",
        Style::default().fg(MUTED),
    ));
    Some(InlineQuestionContent { lines, option_lines })
}

fn question_option_lines(label: &str, description: &str, selected: bool, width: usize) -> Vec<Line<'static>> {
    let marker = if selected { "  › " } else { "    " };
    let mut chunks = vec![StyledChunk::new(
        label.to_owned(),
        Style::default().fg(if selected { BRIGHT } else { TEXT }).bold(),
    )];
    chunks.push(StyledChunk::new(
        format!(" — {description}"),
        Style::default().fg(MUTED),
    ));
    styled_wrap(
        chunks,
        width,
        vec![Span::styled(marker, Style::default().fg(ACTIVE))],
        vec![Span::raw("      ")],
    )
}

fn clarification_modal_rect(app: &App, terminal: Rect) -> Option<Rect> {
    if !app.has_active_clarification() || terminal.width < 36 || terminal.height < 8 {
        return None;
    }
    let width = terminal.width.saturating_sub(4).min(92);
    let line_count = inline_question_lines(app, width.saturating_sub(2))?.lines.len() as u16;
    Some(centered(
        terminal,
        width,
        line_count
            .saturating_add(2)
            .min(terminal.height.saturating_sub(2)),
    ))
}

fn draw_clarification_modal(frame: &mut Frame<'_>, app: &App, terminal: Rect) {
    let Some(area) = clarification_modal_rect(app, terminal) else {
        return;
    };
    let block = surface_block(
        app,
        Line::styled(" question · answer required ", Style::default().fg(BRIGHT).bold()),
        ACTIVE,
        surface(),
    );
    let inner = block.inner(area);
    let Some(content) = inline_question_lines(app, inner.width) else {
        return;
    };
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(content.lines)
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: false }),
        inner,
    );
    finish_surface(frame, app, area, surface());
}

fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let targeted_title = app
        .message_target
        .as_ref()
        .map(|target| format!(" message {target} · Enter sends · /to clears "));
    let title = if let Some(title) = targeted_title.as_deref() {
        title
    } else if app.has_active_clarification() && !app.input.is_empty() {
        " custom answer · Enter submits · Esc clears "
    } else if app.has_active_clarification() {
        " ↑/↓ choose · Enter answers · typing writes a custom answer "
    } else if app.pending_request.is_some() {
        " answer required "
    } else if app.running {
        " steer lead · Enter queues · Esc Esc pauses · Ctrl-C interrupts "
    } else {
        " message · Enter sends · Shift-Enter newline "
    };
    let placeholder = if app.input.is_empty() {
        if app.has_active_clarification() {
            "Type a custom answer…"
        } else if app.running {
            "Type guidance while the hive works…"
        } else {
            "Ask about the repository or describe work…"
        }
    } else {
        &app.input
    };
    let style = if app.input.is_empty() { MUTED } else { BRIGHT };
    let accent = if app.pending_request.is_some() {
        WARN
    } else if app.running {
        ACTIVE
    } else {
        MUTED
    };
    let block = surface_block(
        app,
        Line::styled(title, Style::default().fg(MUTED)),
        accent,
        surface_alt(),
    );
    let inner = block.inner(area);
    app.composer_inner_width.set(usize::from(inner.width.max(1)));
    let layout = EditorLayout::new(&app.input, app.input_cursor, usize::from(inner.width.max(1)));
    let viewport_height = usize::from(inner.height.max(1));
    let viewport_start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(viewport_height);
    let mut visible = Vec::new();
    if app.input.is_empty() {
        visible.push(Line::styled(
            format!("› {placeholder}"),
            Style::default().fg(style),
        ));
    } else {
        for (row, line) in layout
            .lines
            .iter()
            .enumerate()
            .skip(viewport_start)
            .take(viewport_height)
        {
            visible.push(Line::styled(
                if row == 0 {
                    format!("› {}", line.text)
                } else {
                    line.text.clone()
                },
                Style::default().fg(style),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(visible)
            .block(block)
            .style(Style::default().fg(style).bg(surface_alt()))
            .wrap(Wrap { trim: false }),
        area,
    );
    finish_surface(frame, app, area, surface_alt());
    if !app.has_active_clarification() || !app.input.is_empty() {
        let prefix = if layout.cursor_row == 0 { 2 } else { 0 };
        frame.set_cursor_position((
            inner
                .x
                .saturating_add(prefix)
                .saturating_add(layout.cursor_column as u16)
                .min(inner.right().saturating_sub(1)),
            inner
                .y
                .saturating_add(layout.cursor_row.saturating_sub(viewport_start) as u16)
                .min(inner.bottom().saturating_sub(1)),
        ));
    }
}

fn draw_completion_popup(frame: &mut Frame<'_>, app: &App, composer: Rect, terminal: Rect) {
    if app.completion_items.is_empty() {
        return;
    }
    let height = (app.completion_items.len() as u16 + 2).min(10);
    let width = terminal.width.saturating_sub(4).min(64);
    let area = Rect {
        x: composer.x.saturating_add(2),
        y: composer.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, area);
    let lines = app
        .completion_items
        .iter()
        .take(8)
        .map(|(value, description)| {
            Line::from(vec![
                Span::styled(format!("{value:<20}"), Style::default().fg(BRIGHT)),
                Span::styled(description.clone(), Style::default().fg(MUTED)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(surface_block(app, Line::from(" completions "), ACTIVE, surface())),
        area,
    );
    finish_surface(frame, app, area, surface());
}

fn composer_height(app: &App, terminal_width: u16) -> u16 {
    let inner_width = usize::from(terminal_width.min(124).saturating_sub(2).max(1));
    let lines = EditorLayout::new(&app.input, app.input_cursor, inner_width)
        .lines
        .len();
    (lines as u16 + 2).clamp(4, 8)
}

fn draw_live_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let label = match app.phase {
        minha_core::protocol::RunPhase::Queued => "Queued",
        minha_core::protocol::RunPhase::Preflight => "Checking models",
        minha_core::protocol::RunPhase::Planning => "Planning",
        minha_core::protocol::RunPhase::Scheduling | minha_core::protocol::RunPhase::Working => "Working",
        minha_core::protocol::RunPhase::Integrating => "Integrating",
        minha_core::protocol::RunPhase::Judging => "Reviewing",
        minha_core::protocol::RunPhase::Compacting => "Compacting context",
        minha_core::protocol::RunPhase::Waiting | minha_core::protocol::RunPhase::Clarifying => {
            "Waiting for answer"
        }
        _ => "Working",
    };
    let elapsed = app.phase_started_at.elapsed().as_secs();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            if app.reduced_motion { "  • " } else { "  ◐ " },
            Style::default().fg(ACTIVE),
        ),
        Span::styled(label, Style::default().fg(BRIGHT).bold()),
        Span::styled(
            format!(" ({elapsed}s · Esc to interrupt)"),
            Style::default().fg(MUTED),
        ),
    ])];
    if area.height > 1 {
        let focus = app
            .todo_blocked_work
            .first()
            .map(|goal| format!("Blocked: {goal}"))
            .or_else(|| app.todo_active_goals.first().map(|goal| format!("Next: {goal}")))
            .or_else(|| {
                app.todo_recently_completed
                    .first()
                    .map(|goal| format!("Done: {goal}"))
            })
            .unwrap_or_else(|| "No agent TODOs yet".into());
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default().fg(MUTED)),
            Span::styled(
                format!(
                    "{focus} · {} active · {} blocked · {} complete{}",
                    app.todo_active,
                    app.todo_blocked,
                    app.todo_completed,
                    if app.todo_stale_agents > 0 {
                        format!(" · {} agent list(s) stale", app.todo_stale_agents)
                    } else {
                        String::new()
                    }
                ),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let active = app
        .agents
        .iter()
        .filter(|agent| {
            !matches!(
                agent.state,
                AgentState::Completed | AgentState::Failed | AgentState::Cancelled
            )
        })
        .count();
    let model = short_model(&app.model);
    let left = if area.width >= 100 {
        format!(
            " {model} · {:.0}% context left · {active}/{} agents · {} session tokens",
            app.context_left_percent(),
            app.agents.len(),
            format_tokens(app.input_tokens.saturating_add(app.output_tokens)),
        )
    } else if area.width >= 58 {
        format!(
            " {model} · {:.0}% context left · {active}/{} agents",
            app.context_left_percent(),
            app.agents.len()
        )
    } else {
        format!(" {model} · {:.0}% context", app.context_left_percent())
    };
    let right = if app.running {
        format!(
            "{} queued · Esc Esc pause · Ctrl-C interrupt ",
            app.queued_steering
        )
    } else {
        "Ctrl-P commands · Shift-Tab panels · Tab complete ".into()
    };
    let left_width = UnicodeWidthStr::width(left.as_str()) as u16;
    frame.render_widget(
        Paragraph::new(Line::styled(left, Style::default().fg(MUTED))),
        area,
    );
    let width = UnicodeWidthStr::width(right.as_str()) as u16;
    if width.saturating_add(left_width) < area.width {
        frame.render_widget(
            Paragraph::new(Line::styled(right, Style::default().fg(MUTED))),
            Rect {
                x: area.right() - width,
                y: area.y,
                width,
                height: 1,
            },
        );
    }
}

fn draw_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(overlay) = &app.overlay else { return };
    let (max_width, max_height) = match overlay {
        Overlay::Status => (104, 32),
        Overlay::Context => (96, 24),
        Overlay::Books => (112, 26),
        Overlay::Doctor => (84, 22),
        _ => (70, 18),
    };
    let rect = centered(
        area,
        max_width.min(area.width.saturating_sub(4)),
        max_height.min(area.height.saturating_sub(2)),
    );
    frame.render_widget(Clear, rect);
    match overlay {
        Overlay::Palette => {
            let lines = COMMAND_PALETTE
                .iter()
                .enumerate()
                .map(|(index, (command, description))| {
                    let selected = index == app.selected_palette;
                    Line::from(vec![
                        Span::styled(if selected { " › " } else { "   " }, Style::default().fg(ACTIVE)),
                        Span::styled(
                            format!("{command:<12}"),
                            Style::default().fg(if selected { BRIGHT } else { TEXT }).bold(),
                        ),
                        Span::styled(*description, Style::default().fg(MUTED)),
                    ])
                })
                .chain(std::iter::once(Line::styled(
                    "   ↑/↓ choose · Enter run · Esc close",
                    Style::default().fg(MUTED),
                )))
                .collect();
            draw_modal(frame, app, rect, " commands ", lines);
        }
        Overlay::Help => {
            let text = vec![
                Line::styled("Keyboard", Style::default().fg(BRIGHT).bold()),
                Line::raw(""),
                Line::styled(
                    "Enter send/steer   Shift-Enter newline   Esc Esc safe pause   Ctrl-C interrupt",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Tab complete   Shift-Tab panels   Ctrl-O nearest activity   Ctrl-R history",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Arrows edit multiline input   Alt-Arrows move by word   Ctrl-Z/Y undo/redo",
                    Style::default().fg(TEXT),
                ),
                Line::raw(""),
                Line::styled("Commands", Style::default().fg(BRIGHT).bold()),
                Line::styled(
                    "/new /resume /retry [--fresh] /fork /plan /implement /audit /review /diff",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "/activity /work /board /problems /status /context /memory /memories /doctor /books",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "/check /lint /test /docs /security /quality [ACTION] /gh [ACTION] [NUMBER]",
                    Style::default().fg(TEXT),
                ),
                Line::raw(""),
                Line::styled(
                    "! command runs locally without a model call; shell operators are rejected.",
                    Style::default().fg(MUTED),
                ),
            ];
            draw_modal(frame, app, rect, " help ", text);
        }
        Overlay::Sessions => {
            let lines = if app.sessions.is_empty() {
                vec![Line::styled("No saved sessions.", Style::default().fg(MUTED))]
            } else {
                app.sessions
                    .iter()
                    .enumerate()
                    .map(|(index, run)| {
                        let marker = if index == app.selected_session { "›" } else { " " };
                        Line::from(vec![
                            Span::styled(format!("{marker} "), Style::default().fg(ACTIVE)),
                            Span::styled(run.title.clone(), Style::default().fg(BRIGHT)),
                            Span::styled(
                                format!("  {}", crate::app::state_label(run.state)),
                                Style::default().fg(MUTED),
                            ),
                        ])
                    })
                    .collect()
            };
            draw_modal(
                frame,
                app,
                rect,
                " resume session · Enter opens · Esc closes ",
                lines,
            );
        }
        Overlay::Status => draw_status_dashboard(frame, app, rect),
        Overlay::Context => draw_context_dashboard(frame, app, rect),
        Overlay::Books => draw_books(frame, app, rect),
        Overlay::Doctor => draw_doctor(frame, app, rect),
        Overlay::Request => {
            let Some(request) = &app.pending_request else {
                return;
            };
            let mut lines = vec![Line::styled(
                request.question.clone(),
                Style::default().fg(BRIGHT).bold(),
            )];
            if let Some(reason) = &request.reason {
                lines.push(Line::raw(""));
                lines.push(Line::styled(reason.clone(), Style::default().fg(WARN)));
            }
            if let Some(command) = &request.command {
                lines.push(Line::styled(command.join(" "), Style::default().fg(TEXT)));
            }
            lines.push(Line::raw(""));
            lines.extend(request.options.iter().enumerate().map(|(index, option)| {
                Line::styled(format!("{}. {option}", index + 1), Style::default().fg(TEXT))
            }));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Type an answer below and press Enter.",
                Style::default().fg(MUTED),
            ));
            draw_modal(
                frame,
                app,
                rect,
                if request.approval {
                    " approval "
                } else {
                    " question "
                },
                lines,
            );
        }
        Overlay::LocalAnswer { question, answer } => {
            draw_modal(
                frame,
                app,
                rect,
                " local answer · no tokens used ",
                vec![
                    Line::styled(question.clone(), Style::default().fg(MUTED)),
                    Line::raw(""),
                    Line::styled(answer.clone(), Style::default().fg(BRIGHT)),
                ],
            );
        }
        Overlay::Login {
            verification_uri,
            user_code,
            message,
        } => {
            draw_modal(
                frame,
                app,
                rect,
                " ChatGPT Codex login ",
                vec![
                    Line::styled("Open this URL:", Style::default().fg(MUTED)),
                    Line::styled(verification_uri.clone(), Style::default().fg(ACTIVE)),
                    Line::raw(""),
                    Line::styled("Enter this one-time code:", Style::default().fg(MUTED)),
                    Line::styled(
                        format!("  {user_code}  "),
                        Style::default().fg(BRIGHT).bg(surface_alt()).bold(),
                    ),
                    Line::raw(""),
                    Line::styled(message.clone(), Style::default().fg(TEXT)),
                    Line::raw(""),
                    Line::styled(
                        "Minha never prints or stores this device code in the transcript.",
                        Style::default().fg(MUTED),
                    ),
                ],
            );
        }
    }
}

pub(crate) fn clarification_option_at(
    app: &App,
    column: u16,
    row: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Option<usize> {
    if !app.has_active_clarification() || terminal_width < 36 || terminal_height < 8 {
        return None;
    }
    let terminal = Rect::new(0, 0, terminal_width, terminal_height);
    let rect = clarification_modal_rect(app, terminal)?;
    let inner = rect.inner(Margin::new(1, 1));
    if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
        return None;
    }
    let content = inline_question_lines(app, inner.width)?;
    let line = usize::from(row.saturating_sub(inner.y));
    content
        .option_lines
        .into_iter()
        .find_map(|(start, end, option)| (line >= start && line < end).then_some(option))
}

pub(crate) fn composer_cursor_at(
    app: &App,
    column: u16,
    row: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Option<usize> {
    if terminal_width < 36 || terminal_height < 8 {
        return None;
    }
    let terminal = Rect::new(0, 0, terminal_width, terminal_height);
    let task_height = if app.tasks_visible && !app.plan.is_empty() {
        (app.plan.len() as u16 + 1).clamp(2, 6)
    } else {
        0
    };
    let activity_height = if app.running {
        1 + u16::from(app.todo_active + app.todo_blocked + app.todo_completed > 0)
    } else {
        0
    };
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(task_height),
        Constraint::Length(activity_height),
        Constraint::Length(clarification_height(app, terminal_width.min(124))),
        Constraint::Length(composer_height(app, terminal_width)),
        Constraint::Length(1),
    ])
    .split(terminal);
    let inner = centered_width(rows[5], 124).inner(Margin::new(1, 1));
    if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
        return None;
    }
    let layout = EditorLayout::new(&app.input, app.input_cursor, usize::from(inner.width.max(1)));
    let viewport_start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(usize::from(inner.height.max(1)));
    let visual_row = viewport_start + usize::from(row.saturating_sub(inner.y));
    let prefix = usize::from(visual_row == 0) * 2;
    let target = usize::from(column.saturating_sub(inner.x)).saturating_sub(prefix);
    Some(layout.byte_at_column(&app.input, visual_row, target))
}

fn draw_status_dashboard(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let outer = Block::default()
        .title(Line::styled(
            " status · inspector ",
            Style::default().fg(BRIGHT).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACTIVE))
        .style(Style::default().bg(background()));
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);
    if inner.width < 60 {
        draw_status_column(frame, app, inner);
        return;
    }
    let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    draw_status_column(frame, app, columns[0]);
    draw_health_column(frame, app, columns[1]);
}

fn draw_status_column(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let run = app
        .active_run
        .map(|id| short_id(&id.to_string()))
        .unwrap_or_else(|| "none".into());
    let clarification = app.clarification.as_ref().map_or_else(
        || "none".into(),
        |clarification| format!("{:?}", clarification.status).to_ascii_lowercase(),
    );
    let projected_deepseek = app.deepseek_estimated_usd
        + app
            .contexts
            .values()
            .filter_map(|context| {
                minha_core::deepseek::estimate_cost_usd(
                    &context.model,
                    context.forecast_tokens,
                    0,
                    context.output_allowance,
                )
            })
            .sum::<f64>();
    let lines = vec![
        Line::styled("session", Style::default().fg(ACTIVE).bold()),
        kv_line("run", &run),
        kv_line("state", crate::app::state_label(app.state)),
        kv_line("phase", &format!("{:?}", app.phase)),
        kv_line("model", short_model(&app.model)),
        kv_line("surface renderer", &app.active_surface_renderer),
        kv_line("issue clarity", &clarification),
        Line::raw(""),
        Line::styled("token accounting", Style::default().fg(ACTIVE).bold()),
        kv_line("input", &format_tokens(app.input_tokens)),
        kv_line("output", &format_tokens(app.output_tokens)),
        kv_line("cached input", &format_tokens(app.cached_input_tokens)),
        kv_line("cache write", &format_tokens(app.cache_write_tokens)),
        kv_line("reasoning", &format_tokens(app.reasoning_output_tokens)),
        kv_line(
            "DeepSeek cost",
            &format!(
                "${:.6} · projected ${projected_deepseek:.6}",
                app.deepseek_estimated_usd
            ),
        ),
        kv_line(
            "DS cache saved",
            &format!("${:.6}", app.deepseek_cache_savings_usd),
        ),
        kv_line(
            "DeepSeek balance",
            &match (&app.deepseek_balance, app.deepseek_reserve_percent) {
                (Some(balance), Some(percent)) => format!("{balance} · {percent:.1}% reserve"),
                (Some(balance), None) => balance.clone(),
                (None, _) => "unavailable".into(),
            },
        ),
        Line::raw(""),
        Line::styled("lifetime", Style::default().fg(ACTIVE).bold()),
        kv_line("input", &format_tokens(app.lifetime_input_tokens)),
        kv_line("output", &format_tokens(app.lifetime_output_tokens)),
        kv_line(
            "cached / write",
            &format!(
                "{} / {}",
                format_tokens(app.lifetime_cached_input_tokens),
                format_tokens(app.lifetime_cache_write_tokens)
            ),
        ),
        kv_line("reasoning", &format_tokens(app.lifetime_reasoning_output_tokens)),
    ];
    frame.render_widget(card(" session & usage ", lines), area);
}

fn draw_health_column(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let problem_count = app.incidents.len();
    let lines = vec![
        Line::styled("cache", Style::default().fg(ACTIVE).bold()),
        kv_line("entries", &app.cache_entries.to_string()),
        kv_line("bytes", &format_bytes(app.cache_bytes)),
        kv_line(
            "hits / misses",
            &format!("{} / {}", app.cache_hits, app.cache_misses),
        ),
        kv_line("saved tokens", &format_tokens(app.cache_saved_tokens)),
        Line::raw(""),
        Line::styled("office", Style::default().fg(ACTIVE).bold()),
        kv_line("active agents", &app.active_office_agents.to_string()),
        kv_line(
            "open / blocked",
            &format!("{} / {}", app.open_office_tasks, app.blocked_office_tasks),
        ),
        kv_line("consultations", &app.manager_consultations.to_string()),
        Line::raw(""),
        Line::styled("accounts", Style::default().fg(ACTIVE).bold()),
        kv_line("active", app.active_account.as_deref().unwrap_or("none")),
        kv_line("profiles", &app.account_profiles.to_string()),
        Line::raw(""),
        Line::styled("library", Style::default().fg(ACTIVE).bold()),
        kv_line("indexed", &app.indexed_books.to_string()),
        kv_line("manifest", &format!("{} entries", app.library.len())),
        Line::raw(""),
        Line::styled("signals", Style::default().fg(ACTIVE).bold()),
        kv_line("problems", &problem_count.to_string()),
        kv_line("queued steering", &app.queued_steering.to_string()),
        Line::raw(""),
        Line::styled("context", Style::default().fg(ACTIVE).bold()),
        kv_line(
            "current / limit",
            &format!(
                "{} / {}",
                format_tokens(app.current_context_tokens),
                format_tokens(app.context_limit)
            ),
        ),
        kv_line("compact at", &format_tokens(app.compact_at_tokens)),
    ];
    frame.render_widget(card(" cache · office · books ", lines), area);
}

fn draw_context_dashboard(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let outer = Block::default()
        .title(Line::styled(
            " context window ",
            Style::default().fg(BRIGHT).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACTIVE))
        .style(Style::default().bg(surface()));
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);
    let ratio = (app.current_context_tokens as f64 / app.context_limit.max(1) as f64).clamp(0.0, 1.0);
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).split(inner);
    frame.render_widget(
        Gauge::default()
            .ratio(ratio)
            .label(format!(
                "{} / {} · {:.1}% context left",
                format_tokens(app.current_context_tokens),
                format_tokens(app.context_limit),
                (1.0 - ratio) * 100.0
            ))
            .gauge_style(
                Style::default()
                    .fg(if ratio >= 0.9 {
                        BAD
                    } else if ratio >= 0.72 {
                        WARN
                    } else {
                        ACTIVE
                    })
                    .bg(surface_alt()),
            ),
        rows[0],
    );
    let mut lines = vec![
        Line::styled(
            "The context meter is an estimate of the active conversation window, not billed usage.",
            Style::default().fg(TEXT),
        ),
        Line::raw(""),
        kv_line("automatic compaction", &format_tokens(app.compact_at_tokens)),
        kv_line("durable checkpoints", &app.compaction_count.to_string()),
        kv_line("session input", &format_tokens(app.input_tokens)),
        kv_line("session output", &format_tokens(app.output_tokens)),
        Line::raw(""),
        Line::styled("per-agent forecasts", Style::default().fg(ACTIVE).bold()),
    ];
    let mut contexts = app.contexts.iter().collect::<Vec<_>>();
    contexts.sort_by_key(|(agent_id, _)| agent_id.to_string());
    for (agent_id, context) in contexts.into_iter().take(6) {
        let role = app
            .agents
            .iter()
            .find(|agent| agent.id == *agent_id)
            .map_or_else(|| "agent".into(), |agent| short_role(&agent.role));
        let left = context.advertised_limit.saturating_sub(context.estimated_tokens) as f64
            / context.advertised_limit.max(1) as f64
            * 100.0;
        lines.push(Line::styled(
            format!(
                "{role}: {} · {} used · {left:.0}% left · {} effective · {} forecast · {} output · {} reserve · {}",
                short_model(&context.model),
                format_tokens(context.estimated_tokens),
                format_tokens(context.effective_limit),
                format_tokens(context.forecast_tokens),
                format_tokens(context.output_allowance),
                format_tokens(context.protected_reserve),
                context.capability_source,
            ),
            Style::default().fg(TEXT),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT).bg(surface()))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

fn draw_books(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let outer = Block::default()
        .title(Line::styled(
            " books · bundled technical library ",
            Style::default().fg(BRIGHT).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACTIVE))
        .style(Style::default().bg(background()));
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);
    if app.library.is_empty() {
        frame.render_widget(
            Paragraph::new("Bundled manifest could not be loaded.").style(Style::default().fg(WARN)),
            inner,
        );
        return;
    }
    let columns = Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)]).split(inner);
    let items = app
        .library
        .iter()
        .map(|entry| {
            ListItem::new(vec![
                Line::styled(entry.title.clone(), Style::default().fg(BRIGHT)),
                Line::styled(
                    format!("{} · {}", entry.pack_id, entry.version),
                    Style::default().fg(MUTED),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.selected_book));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(" entries ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border())),
            )
            .style(Style::default().bg(surface()))
            .highlight_style(Style::default().bg(surface_alt()).fg(BRIGHT))
            .highlight_symbol("› "),
        columns[0],
        &mut state,
    );
    let entry = &app.library[app.selected_book.min(app.library.len() - 1)];
    let details = vec![
        Line::styled(entry.title.clone(), Style::default().fg(BRIGHT).bold()),
        Line::styled(
            format!("{} · {} · {}", entry.id, entry.pack_id, entry.version),
            Style::default().fg(ACTIVE),
        ),
        Line::styled(
            format!("{} · {:?}", format_taxonomy(&entry.taxonomy), entry.language),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            format!(
                "trust: {:?} · freshness: {:?}",
                entry.trust, entry.staleness.status
            ),
            Style::default().fg(GOOD),
        ),
        Line::raw(""),
        Line::styled(entry.abstract_text.clone(), Style::default().fg(TEXT)),
        Line::raw(""),
        Line::styled(
            format!("tags: {}", entry.tags.join(", ")),
            Style::default().fg(MUTED),
        ),
        Line::styled(format!("source: {}", entry.path), Style::default().fg(MUTED)),
        Line::raw(""),
        Line::styled("↑/↓ browse · Esc close", Style::default().fg(MUTED)),
    ];
    frame.render_widget(
        Paragraph::new(details)
            .block(
                Block::default()
                    .title(" entry ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border())),
            )
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: true }),
        columns[1],
    );
}

fn draw_doctor(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let lines = if app.diagnostics.is_empty() {
        vec![Line::styled(
            "No diagnostics have run yet.",
            Style::default().fg(MUTED),
        )]
    } else {
        app.diagnostics.iter().map(diagnostic_line).collect()
    };
    draw_modal(frame, app, rect, " doctor · local runtime checks ", lines);
}

fn diagnostic_line(diagnostic: &Diagnostic) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if diagnostic.ok { "✓ " } else { "× " },
            Style::default().fg(if diagnostic.ok { GOOD } else { BAD }),
        ),
        Span::styled(
            format!("{:<12}", diagnostic.label),
            Style::default().fg(BRIGHT).bold(),
        ),
        Span::styled(diagnostic.detail.clone(), Style::default().fg(TEXT)),
    ])
}

fn card(title: &'static str, lines: Vec<Line<'static>>) -> Paragraph<'static> {
    Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border())),
        )
        .style(Style::default().bg(surface()))
        .wrap(Wrap { trim: true })
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<18}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(TEXT)),
    ])
}

fn format_tokens(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_bytes(value: u64) -> String {
    if value >= 1 << 30 {
        format!("{:.1} GiB", value as f64 / (1 << 30) as f64)
    } else if value >= 1 << 20 {
        format!("{:.1} MiB", value as f64 / (1 << 20) as f64)
    } else if value >= 1 << 10 {
        format!("{:.1} KiB", value as f64 / (1 << 10) as f64)
    } else {
        format!("{value} B")
    }
}

fn format_taxonomy(taxonomy: &[minha_core::books::Taxonomy]) -> String {
    taxonomy
        .iter()
        .map(|item| format!("{item:?}").to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

fn draw_modal(frame: &mut Frame<'_>, app: &App, rect: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(surface_block(
                app,
                Line::styled(title.to_owned(), Style::default().fg(BRIGHT)),
                border(),
                surface(),
            ))
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: true }),
        rect,
    );
    finish_surface(frame, app, rect, surface());
}

fn draw_tiny(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(format!("minha · {}", app.status), Style::default().fg(BRIGHT)),
            Line::styled(format!("> {}", app.input), Style::default().fg(ACTIVE)),
            Line::styled("terminal too small", Style::default().fg(MUTED)),
        ])
        .wrap(Wrap { trim: true }),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn short_role(role: &str) -> String {
    role.replace("gpt-", "").chars().take(36).collect()
}

fn short_model(model: &str) -> &str {
    model.strip_prefix("gpt-").unwrap_or(model)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn state_marker(state: AgentState) -> &'static str {
    match state {
        AgentState::Completed => "● ",
        AgentState::Failed | AgentState::Cancelled => "× ",
        AgentState::Waiting => "? ",
        _ => "◐ ",
    }
}

fn agent_color(state: AgentState) -> Color {
    match state {
        AgentState::Completed => GOOD,
        AgentState::Failed | AgentState::Cancelled => BAD,
        AgentState::Waiting => WARN,
        _ => ACTIVE,
    }
}

fn tone_color(tone: SystemTone) -> Color {
    match tone {
        SystemTone::Info => MUTED,
        SystemTone::Success => GOOD,
        SystemTone::Warning => WARN,
        SystemTone::Error => BAD,
    }
}

fn status_color(state: minha_core::protocol::ExitState) -> Color {
    use minha_core::protocol::ExitState;
    match state {
        ExitState::Succeeded => GOOD,
        ExitState::Running => ACTIVE,
        ExitState::NeedsInput | ExitState::ApprovalRequired | ExitState::UsagePaused => WARN,
        ExitState::Failed
        | ExitState::Cancelled
        | ExitState::AuthUnavailable
        | ExitState::ModelUnavailable => BAD,
        _ => MUTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use minha_core::clarify::{analyze, make_fallback_batch};
    use minha_core::protocol::{EventAgentId, EventEnvelope, ItemId, RunId, RuntimeEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn populated_app() -> App {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let run = RunId::new();
        let agent = EventAgentId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            0,
            RuntimeEvent::SessionStarted {
                kind: "implement".into(),
                goal: "improve the tui".into(),
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            1,
            RuntimeEvent::UserMessage {
                text: "improve the tui".into(),
                steering: false,
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            2,
            RuntimeEvent::AgentStarted {
                agent_id: agent,
                role: "Luna integrator".into(),
                model: "gpt-5.6-luna".into(),
                parent: None,
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            3,
            RuntimeEvent::AssistantMessage {
                agent_id: agent,
                item_id: ItemId::new(),
                role: "Luna integrator".into(),
                model: "gpt-5.6-luna".into(),
                text: "I am inspecting the real runtime state.".into(),
            },
        ));
        app
    }

    fn render(width: u16, height: u16, app: &App) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test operation should succeed");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("test operation should succeed");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn transcript_layout_is_usable_at_common_widths() {
        let mut app = populated_app();
        app.drawer_visible = true;
        for width in [40, 80, 120, 160] {
            let screen = render(width, 28, &app);
            assert!(screen.contains("minha"));
            if width >= 80 {
                assert!(screen.contains("improve the tui"));
            } else {
                assert!(screen.contains("hive"));
            }
            assert!(screen.contains("message") || screen.contains("steer lead"));
        }
    }

    #[test]
    fn required_viewports_render_without_unbounded_layout() {
        let app = populated_app();
        for (width, height) in [(36, 8), (40, 20), (80, 24), (120, 30), (160, 40)] {
            let screen = render(width, height, &app);
            assert!(screen.contains("minha"));
            assert_eq!(screen.lines().count(), usize::from(height));
        }
    }

    #[test]
    fn long_transcripts_cache_layout_and_copy_only_the_viewport() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        for index in 0..2_000 {
            app.push_system(
                SystemTone::Info,
                format!("event {index}: {}", "detail ".repeat(8)),
            );
        }
        app.scroll = u16::MAX;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("first draw");
        {
            let cache = app.transcript_layout.borrow();
            assert_eq!(cache.builds, 1);
            assert!(cache.lines.len() > 2_000);
            assert!(cache.last_viewport_lines <= 16);
        }
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("animation redraw");
        assert_eq!(
            app.transcript_layout.borrow().builds,
            1,
            "unchanged animation frame reuses layout"
        );

        app.update(crate::app::AppAction::PageUp)
            .expect("inspect history");
        let inspected_scroll = app.scroll;
        assert!(!app.auto_follow);
        app.push_system(SystemTone::Success, "finished");
        terminal.draw(|frame| draw(frame, &app)).expect("updated draw");
        assert_eq!(app.transcript_layout.borrow().builds, 2);
        assert_eq!(
            app.scroll, inspected_scroll,
            "new activity must not snap an inspecting user to bottom"
        );

        let bottom = app.transcript_layout.borrow().max_scroll;
        app.scroll = bottom.saturating_sub(1);
        app.update(crate::app::AppAction::ScrollDown)
            .expect("return to bottom");
        assert!(app.auto_follow);
        assert_eq!(app.scroll, u16::MAX);
    }

    #[test]
    fn composer_cursor_and_wrapping_are_unicode_width_correct() {
        let wide = EditorLayout::new("a界", "a界".len(), 4);
        assert_eq!((wide.cursor_row, wide.cursor_column), (1, 2));
        let combining = EditorLayout::new("e\u{301}x", "e\u{301}".len(), 6);
        assert_eq!((combining.cursor_row, combining.cursor_column), (0, 1));
        let multiline = EditorLayout::new("ab\n界", "ab\n界".len(), 6);
        assert_eq!((multiline.cursor_row, multiline.cursor_column), (1, 2));
        for line in wrap_display("words stay together 界界", 8) {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 8);
        }
    }

    #[test]
    fn composer_mouse_hit_uses_the_same_wrapped_editor_geometry() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.input = "hello".into();
        app.input_cursor = app.input.len();
        let first_composer_row = (0..24)
            .find(|row| composer_cursor_at(&app, 3, *row, 80, 24).is_some())
            .expect("composer row");
        assert_eq!(composer_cursor_at(&app, 5, first_composer_row, 80, 24), Some(2));
    }

    #[test]
    fn no_color_theme_resets_every_rendered_cell() {
        let mut app = populated_app();
        app.theme = "no_color".into();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| { cell.fg == Color::Reset && cell.bg == Color::Reset })
        );
    }

    #[test]
    fn dark_canvas_and_quadrant_surface_keep_distinct_inside_outside_colors() {
        let mut app = populated_app();
        app.theme = "dark".into();
        app.active_surface_renderer = "quadrant".into();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 10)].bg, background(), "canvas uses Minha navy");
        assert_eq!(buffer[(0, 19)].symbol(), "▗");
        assert_eq!(buffer[(0, 19)].bg, background(), "corner outside uses canvas");
        assert_eq!(buffer[(0, 19)].fg, surface_alt(), "corner inside uses panel fill");
        assert_eq!(
            buffer[(1, 20)].bg,
            surface_alt(),
            "composer interior keeps its fill"
        );
    }

    #[test]
    fn assistant_markdown_is_formatted_and_prewrapped_to_display_width() {
        let item = TranscriptItem::Assistant {
            item_id: ItemId::new(),
            agent_id: EventAgentId::new(),
            role: "Luna integrator".into(),
            text: "# Result\n\nA soft-wrapped paragraph\nstays one paragraph.\n\n- [x] **Bold** and ~~old~~ words\n- ordinary item\n\n| Path | State |\n| --- | --- |\n| src/lib.rs | good |\n\n```rust\nlet wide = \"界界界界\";\n```"
                .into(),
            streaming: false,
        };
        let lines = item_lines(&item, 24);
        for line in &lines {
            let rendered = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                UnicodeWidthStr::width(rendered.as_str()) <= 24,
                "overwide line: {rendered:?}"
            );
        }
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Result"));
        assert!(rendered.contains("•"));
        assert!(rendered.contains("☑"));
        assert!(rendered.contains("Path │ State"));
        assert!(rendered.contains("paragraph stays"));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("~~"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn streaming_control_payload_is_hidden_before_its_closing_tag_arrives() {
        let lines = assistant_lines(
            "planner lead",
            "<minha-plan>{\"tasks\":[{\"id\":\"secret-internal\"}",
            true,
            80,
        );
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!rendered.contains("secret-internal"));
        assert!(!rendered.contains("minha-plan"));
    }

    #[test]
    fn every_theme_renders_working_and_reduced_motion_states() {
        for theme in ["auto", "dark", "light", "ansi16", "high_contrast", "no_color"] {
            let mut app = populated_app();
            app.theme = theme.into();
            app.running = true;
            app.phase = minha_core::protocol::RunPhase::Compacting;
            app.todo_active = 1;
            app.todo_active_goals = vec!["preserve Unicode cursor geometry".into()];
            for reduced_motion in [false, true] {
                app.reduced_motion = reduced_motion;
                let screen = render(80, 24, &app);
                assert!(screen.contains("Compacting context"));
                assert!(screen.contains("preserve Unicode"));
            }
        }
    }

    #[test]
    fn empty_session_has_sparse_welcome_not_fake_agents() {
        let app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let screen = render(100, 28, &app);
        assert!(screen.contains("What should the hive work on?"));
        assert!(!screen.contains("scout"));
        assert!(!screen.contains("builder"));
    }

    #[test]
    fn wide_terminal_centers_the_reading_rail() {
        let app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let screen = render(200, 30, &app);
        let welcome = screen
            .lines()
            .find(|line| line.contains("What should the hive work on?"))
            .expect("welcome line");
        let column = welcome.find("What should").expect("welcome column");
        assert!(
            column > 60 && column < 110,
            "welcome should be centered: {column}"
        );
        let composer = screen
            .lines()
            .find(|line| line.contains("message · Enter sends"))
            .expect("composer title");
        assert!(composer.starts_with(&" ".repeat(38)));
    }

    #[test]
    fn drawer_exposes_activity_work_board_and_problems() {
        let mut app = populated_app();
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Problems;
        let screen = render(120, 28, &app);
        assert!(screen.contains("activity"));
        assert!(screen.contains("work"));
        assert!(screen.contains("board"));
        assert!(screen.contains("problems"));
    }

    #[test]
    fn status_and_books_overlays_render_real_inspector_content() {
        let mut app = populated_app();
        app.overlay = Some(Overlay::Status);
        app.input_tokens = 1_234;
        app.lifetime_output_tokens = 8_765;
        app.cache_entries = 3;
        app.active_office_agents = 1;
        let status = render(120, 30, &app);
        assert!(status.contains("lifetime"));
        assert!(status.contains("cache"));
        assert!(status.contains("office"));

        app.overlay = Some(Overlay::Books);
        let books = render(120, 30, &app);
        assert!(books.contains("bundled technical library"));
        assert!(books.contains("Software Foundations"));
    }

    #[test]
    fn issue_clarifier_renders_one_inline_question_without_internal_meter() {
        let mut app = populated_app();
        let mut clarification = analyze("it doesn't work", "auto");
        clarification.pending_batch = Some(make_fallback_batch(&clarification));
        app.clarification = Some(clarification);
        app.running = false;

        let screen = render(120, 40, &app);
        assert!(screen.contains("1 of"));
        assert!(screen.contains("Something fails"));
        assert!(screen.contains("Not sure"));
        assert!(screen.contains("Recommended"));
        assert!(!screen.contains("ambiguity"));
        assert!(!screen.contains("reproduction"));
    }

    #[test]
    fn clarification_mouse_hits_follow_rendered_option_rows() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let mut clarification = analyze("it doesn't work", "auto");
        let mut batch = make_fallback_batch(&clarification);
        batch.questions.truncate(1);
        let expected = batch.questions[0].options.len()
            + usize::from(batch.questions[0].allow_not_sure)
            + usize::from(batch.questions[0].allow_free_text);
        clarification.pending_batch = Some(batch);
        app.clarification = Some(clarification);

        let mut hits = Vec::new();
        for row in 0..40 {
            if let Some(option) = clarification_option_at(&app, 60, row, 120, 40)
                && !hits.contains(&option)
            {
                hits.push(option);
            }
        }
        assert_eq!(hits, (0..expected).collect::<Vec<_>>());
        assert_eq!(clarification_option_at(&app, 0, 0, 120, 40), None);
    }
}
