use crate::app::{AgentView, App, Diagnostic, DrawerTab, Overlay, SystemTone, TranscriptItem};
use minha_core::protocol::{AgentState, IncidentSeverity, PlanTaskState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};

const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;
const MUTED: Color = Color::DarkGray;
const ACTIVE: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

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
        return;
    }

    let composer_height = (app.input.lines().count() as u16 + 3).clamp(4, 8);
    let task_height = if app.tasks_visible && !app.plan.is_empty() {
        (app.plan.len() as u16 + 1).clamp(2, 6)
    } else {
        0
    };
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(task_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .split(area);

    draw_header(frame, app, rows[0]);
    let wide_drawer = app.drawer_visible && area.width >= 110 && app.focused_agent.is_none();
    if wide_drawer {
        let columns = Layout::horizontal([Constraint::Min(52), Constraint::Length(48)]).split(rows[1]);
        draw_transcript(frame, app, columns[0]);
        draw_drawer(frame, app, columns[1]);
    } else {
        draw_transcript(frame, app, rows[1]);
    }
    if task_height > 0 {
        draw_tasks(frame, app, rows[2]);
    }
    draw_composer(frame, app, rows[3]);
    draw_footer(frame, app, rows[4]);

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
    let mut lines = Vec::new();
    for item in app.visible_items() {
        lines.extend(item_lines(item, inner.width));
    }
    let estimated_height = lines.len() as u16;
    let max_scroll = estimated_height.saturating_sub(inner.height);
    let scroll = if app.scroll == u16::MAX {
        max_scroll
    } else {
        app.scroll.min(max_scroll)
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = area.width.min(68);
    let welcome = Rect {
        x: area.x,
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
            .block(
                Block::default()
                    .title(Line::styled(" minha ", Style::default().fg(ACTIVE).bold()))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border())),
            )
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: true }),
        welcome,
    );
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
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("● ", Style::default().fg(if *streaming { ACTIVE } else { GOOD })),
                Span::styled(short_role(role), Style::default().fg(MUTED)),
            ])];
            lines.extend(
                text.lines()
                    .map(|line| Line::styled(format!("  {line}"), Style::default().fg(TEXT))),
            );
            if *streaming {
                lines.push(Line::styled("  ▍", Style::default().fg(ACTIVE)));
            }
            lines.push(Line::raw(""));
            lines
        }
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
        TranscriptItem::System { tone, text } => vec![
            Line::from(vec![
                Span::styled("  • ", Style::default().fg(tone_color(*tone))),
                Span::styled(text.clone(), Style::default().fg(tone_color(*tone))),
            ]),
            Line::raw(""),
        ],
        TranscriptItem::Status { lines } => boxed_lines(
            "status · local data",
            &lines.join("\n"),
            ACTIVE,
            surface_alt(),
            width,
        ),
    }
}

fn boxed_lines(title: &str, body: &str, accent: Color, background: Color, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(8));
    let content_width = width.saturating_sub(4).max(1);
    let title = truncate_chars(title, content_width.saturating_sub(2));
    let top_fill = "─".repeat(width.saturating_sub(title.chars().count() + 5));
    let mut lines = vec![Line::styled(
        format!("╭─ {title} {top_fill}╮"),
        Style::default().fg(accent).bg(background).bold(),
    )];
    for source in body.lines().chain(body.is_empty().then_some("")) {
        for part in wrap_chars(source, content_width) {
            let padding = " ".repeat(content_width.saturating_sub(part.chars().count()));
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(accent).bg(background)),
                Span::styled(
                    format!("{part}{padding}"),
                    Style::default().fg(TEXT).bg(background),
                ),
                Span::styled(" │", Style::default().fg(accent).bg(background)),
            ]));
        }
    }
    lines.push(Line::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        Style::default().fg(accent).bg(background),
    ));
    lines.push(Line::raw(""));
    lines
}

fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars = text.chars().collect::<Vec<_>>();
    chars
        .chunks(width.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
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
    if app.plan.is_empty() {
        frame.render_widget(
            Paragraph::new("No work plan yet.\nPlan tasks appear here when Luna decomposes a goal.")
                .block(block)
                .style(Style::default().fg(MUTED).bg(surface()))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let items = app
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
            .block(
                Block::default()
                    .title(Line::styled(" tasks ", Style::default().fg(MUTED)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border())),
            )
            .style(Style::default().bg(surface())),
        area,
    );
}

fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = if app.pending_request.is_some() {
        " answer required "
    } else if app.running {
        " steer Luna · Enter queues · Esc interrupts "
    } else {
        " message · Enter sends · Shift-Enter newline "
    };
    let prompt = if app.input.is_empty() {
        if app.running {
            "Type guidance while the hive works…"
        } else {
            "Ask about the repository or describe work…"
        }
    } else {
        &app.input
    };
    let style = if app.input.is_empty() { MUTED } else { BRIGHT };
    frame.render_widget(
        Paragraph::new(format!("› {prompt}"))
            .block(
                Block::default()
                    .title(Line::styled(title, Style::default().fg(MUTED)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if app.pending_request.is_some() {
                        WARN
                    } else if app.running {
                        ACTIVE
                    } else {
                        MUTED
                    })),
            )
            .style(Style::default().fg(style).bg(surface_alt()))
            .wrap(Wrap { trim: false }),
        area,
    );
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
    let left = format!(
        " {} · {:.0}% ctx · {active}/{} agents",
        short_model(&app.model),
        app.context_percent(),
        app.agents.len()
    );
    let right = if app.running {
        format!("{} queued · esc interrupt ", app.queued_steering)
    } else {
        "tab activity/work/board/problems · ? help ".into()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(left, Style::default().fg(MUTED))),
        area,
    );
    let width = right.chars().count() as u16;
    if width < area.width {
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
        Overlay::Context => (84, 14),
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
        Overlay::Help => {
            let text = vec![
                Line::styled("Keyboard", Style::default().fg(BRIGHT).bold()),
                Line::raw(""),
                Line::styled(
                    "Enter send/steer   Shift-Enter newline   Esc close/interrupt",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Tab drawer tabs   Enter opens selection   Ctrl-O details   Ctrl-R history",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "Up/Down scroll or select   PgUp/PgDn faster   ? close help",
                    Style::default().fg(TEXT),
                ),
                Line::raw(""),
                Line::styled("Commands", Style::default().fg(BRIGHT).bold()),
                Line::styled(
                    "/new /resume /retry [--fresh] /fork /plan /implement /audit /review /diff",
                    Style::default().fg(TEXT),
                ),
                Line::styled(
                    "/activity /work /board /problems /status /context /doctor /clean /books",
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
            draw_modal(frame, rect, " help ", text);
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
            draw_modal(frame, rect, " resume session · Enter opens · Esc closes ", lines);
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
    let lines = vec![
        Line::styled("session", Style::default().fg(ACTIVE).bold()),
        kv_line("run", &run),
        kv_line("state", crate::app::state_label(app.state)),
        kv_line("phase", &format!("{:?}", app.phase)),
        kv_line("model", short_model(&app.model)),
        Line::raw(""),
        Line::styled("token accounting", Style::default().fg(ACTIVE).bold()),
        kv_line("input", &format_tokens(app.input_tokens)),
        kv_line("output", &format_tokens(app.output_tokens)),
        kv_line("cached input", &format_tokens(app.cached_input_tokens)),
        kv_line("cache write", &format_tokens(app.cache_write_tokens)),
        kv_line("reasoning", &format_tokens(app.reasoning_output_tokens)),
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
                "{} / {} · {:.1}%",
                format_tokens(app.current_context_tokens),
                format_tokens(app.context_limit),
                ratio * 100.0
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "The context meter is an estimate of the active conversation window, not billed usage.",
                Style::default().fg(TEXT),
            ),
            Line::raw(""),
            kv_line("automatic compaction", &format_tokens(app.compact_at_tokens)),
            kv_line("durable checkpoints", &app.compaction_count.to_string()),
            kv_line("session input", &format_tokens(app.input_tokens)),
            kv_line("session output", &format_tokens(app.output_tokens)),
        ])
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
    draw_modal(frame, rect, " doctor · local runtime checks ", lines);
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

fn draw_modal(frame: &mut Frame<'_>, rect: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(Line::styled(title.to_owned(), Style::default().fg(BRIGHT)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border())),
            )
            .style(Style::default().bg(surface()))
            .wrap(Wrap { trim: true }),
        rect,
    );
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
            assert!(screen.contains("message") || screen.contains("steer Luna"));
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
}
