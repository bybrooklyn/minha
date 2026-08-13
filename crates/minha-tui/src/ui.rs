use crate::app::{
    AgentView, App, Diagnostic, DrawerTab, Overlay, PendingRequest, SystemTone, TranscriptItem,
    WIDE_DRAWER_MIN_WIDTH, identity_model_label, persona_for,
};
use crate::commands::{self, Category};
use crate::editor::EditorLayout;
use crate::keymap;
use minha_core::protocol::{
    AgentState, ClarificationStatus, ExitState, IncidentSeverity, PlanTaskState, TerminationReason, TodoState,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::border::Set as BorderSet;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use std::path::Path;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;
const MUTED: Color = Color::DarkGray;
const ACTIVE: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const COMPACT_RAIL_MAX_WIDTH: u16 = 124;
const WIDE_RAIL_MAX_WIDTH: u16 = 156;
const WIDE_RAIL_MIN_WIDTH: u16 = 132;
const WIDE_RAIL_GUTTER: u16 = 4;
const WIDE_DRAWER_WIDTH: u16 = 48;
const MIN_FULL_WIDTH: u16 = 36;
const MIN_FULL_HEIGHT: u16 = 10;
const HEADER_HEIGHT: u16 = 2;
const FOOTER_HEIGHT: u16 = 1;
const MIN_INLINE_CARD_HEIGHT: u16 = 3;
const BLANK_BORDER_SET: BorderSet<'static> = BorderSet {
    top_left: " ",
    top_right: " ",
    bottom_left: " ",
    bottom_right: " ",
    vertical_left: " ",
    vertical_right: " ",
    horizontal_top: " ",
    horizontal_bottom: " ",
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RasterSurface {
    pub(crate) rect: Rect,
    pub(crate) fill: [u8; 3],
}

pub(crate) fn truecolor() -> bool {
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
    if area.width < MIN_FULL_WIDTH || area.height < MIN_FULL_HEIGHT {
        draw_tiny(frame, app, area);
        apply_theme(frame, app);
        return;
    }

    let rows = vertical_rows(app, area);
    let task_height = rows[2].height;
    let activity_height = rows[3].height;
    let card_height = rows[4].height;

    draw_header(frame, app, full_width_row(area, rows[0]));
    let wide_drawer = wide_drawer_active(app, area);
    if wide_drawer {
        // The drawer is a discrete operational surface. Leave the transcript
        // canvas open behind it instead of tinting an entire empty column into
        // a giant, visually undifferentiated panel.
        if let Some(drawer) = drawer_rect(app, area) {
            draw_drawer(frame, app, drawer);
            finish_kitty_surface(frame, app, drawer, surface());
        }
    }
    if rows[1].height > 0 {
        draw_transcript(frame, app, conversation_rail(rows[1]));
    }
    if task_height > 0 {
        draw_tasks(frame, app, conversation_rail(rows[2]));
    }
    if activity_height > 0 {
        draw_live_status(frame, app, conversation_rail(rows[3]));
    }
    if card_height > 0 {
        draw_decision_card(frame, app, conversation_rail(rows[4]));
    }
    let composer = conversation_rail(rows[5]);
    draw_composer(frame, app, composer);
    // A narrow drawer is a real overlay, but completion and notice controls
    // must remain actionable when it happens to be open.  Draw it before
    // those transient surfaces so the cell layer agrees with the Kitty image
    // layer (whose later placements sit on top of earlier ones).
    if let Some(overlay) = drawer_rect(app, area).filter(|_| !wide_drawer) {
        clear_to_canvas(frame, overlay);
        draw_drawer(frame, app, overlay);
        finish_kitty_surface(frame, app, overlay, surface());
    }
    draw_completion_popup(frame, app, composer, rows[0].bottom());
    draw_footer(frame, app, full_width_row(area, rows[6]));
    draw_toast(frame, app, area);
    draw_overlay(frame, app, area);
    apply_theme(frame, app);
}

/// The seven-row primary-column layout shared by drawing, mouse hit testing,
/// and drawer geometry. In wide mode its width excludes the persistent
/// operations sidebar so no main surface can run underneath it.
fn vertical_rows(app: &App, area: Rect) -> [Rect; 7] {
    let primary = primary_column(app, area);
    let content_width = conversation_rail(primary).width;
    // The editor and its status line are the escape hatch from every other
    // UI state. Budget the fixed chrome first, then fit secondary rails into
    // what remains so a long approval reason cannot shrink the composer to a
    // single unusable row on a compact terminal.
    let composer_height = composer_height(app, content_width).min(
        primary
            .height
            .saturating_sub(HEADER_HEIGHT.saturating_add(FOOTER_HEIGHT)),
    );
    let body_budget = primary
        .height
        .saturating_sub(HEADER_HEIGHT)
        .saturating_sub(composer_height)
        .saturating_sub(FOOTER_HEIGHT);
    let requested_task_height = if app.tasks_visible && !app.plan.is_empty() {
        (app.plan.len() as u16 + 1).clamp(2, 6)
    } else {
        0
    };
    let activity_height = if app.running {
        1 + u16::from(app.todo_active + app.todo_blocked + app.todo_completed > 0)
    } else if paused_run(app) {
        // A reserve pause is an intelligible state, not an absent activity
        // stream. Keep room for both the reason and its admission evidence.
        2
    } else {
        0
    };
    let requested_question_height = clarification_height(app, content_width);
    let question_height = requested_question_height.min(body_budget);
    // A bordered card needs an inner row to communicate anything useful. If
    // that does not fit, leave the composer visible with its answer-required
    // hint rather than rendering a broken one- or two-cell sliver.
    let question_height = if requested_question_height > 0 && question_height >= MIN_INLINE_CARD_HEIGHT {
        question_height
    } else {
        0
    };
    let remaining_body = body_budget.saturating_sub(question_height);
    let task_height = requested_task_height.min(remaining_body);
    let activity_height = activity_height.min(remaining_body.saturating_sub(task_height));
    let split = Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        // Under pressure this may collapse completely: transcript history is
        // still reachable by scrolling after the current decision, whereas a
        // hidden composer would strand the user at an answer-required state.
        Constraint::Min(0),
        Constraint::Length(task_height),
        Constraint::Length(activity_height),
        Constraint::Length(question_height),
        Constraint::Length(composer_height),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .split(primary);
    [
        split[0], split[1], split[2], split[3], split[4], split[5], split[6],
    ]
}

fn wide_drawer_active(app: &App, area: Rect) -> bool {
    app.drawer_visible && area.width >= WIDE_DRAWER_MIN_WIDTH && app.focused_agent.is_none()
}

/// The main column is calculated once from the terminal, then reused by every
/// primary surface. A deliberately opened drawer owns the right rail; without
/// one, a large terminal centers the conversation rather than pinning it to a
/// mostly empty left edge.
fn primary_column(app: &App, area: Rect) -> Rect {
    if wide_drawer_active(app, area) {
        let column =
            Layout::horizontal([Constraint::Min(52), Constraint::Length(WIDE_DRAWER_WIDTH)]).split(area)[0];
        if column.width < WIDE_RAIL_MIN_WIDTH {
            return column;
        }
        let gutter = WIDE_RAIL_GUTTER.min(column.width.saturating_sub(1));
        return Rect {
            x: column.x.saturating_add(gutter),
            y: column.y,
            width: column.width.saturating_sub(gutter).min(WIDE_RAIL_MAX_WIDTH),
            height: column.height,
        };
    }
    centered_width(area, WIDE_RAIL_MAX_WIDTH)
}

fn wide_drawer_column(app: &App, area: Rect) -> Option<Rect> {
    wide_drawer_active(app, area).then(|| {
        Layout::horizontal([Constraint::Min(52), Constraint::Length(WIDE_DRAWER_WIDTH)]).split(area)[1]
    })
}

fn full_width_row(area: Rect, row: Rect) -> Rect {
    Rect {
        x: area.x,
        y: row.y,
        width: area.width,
        height: row.height,
    }
}

fn paused_run(app: &App) -> bool {
    app.state == ExitState::UsagePaused
        || matches!(
            app.termination_reason,
            Some(TerminationReason::BudgetTarget | TerminationReason::ProviderReserve)
        )
}

/// The drawer's rendered rectangle, or None when the drawer is hidden. This
/// is the single source of truth for both drawing and mouse hit testing, so
/// clicks can never land on an approximation of the panel.
pub(crate) fn drawer_rect(app: &App, area: Rect) -> Option<Rect> {
    if !app.drawer_visible
        || app.focused_agent.is_some()
        || area.width < MIN_FULL_WIDTH
        || area.height < MIN_FULL_HEIGHT
        // A narrow drawer and the command picker both need most of the same
        // small terminal. Let completion own that space temporarily instead
        // of stacking two bordered panels through each other; the user's
        // drawer choice remains intact and returns when completion closes.
        || (!wide_drawer_active(app, area) && app.completion_open())
    {
        return None;
    }
    let rows = vertical_rows(app, area);
    if let Some(column) = wide_drawer_column(app, area) {
        let available = rows[5].y.saturating_sub(rows[1].y);
        Some(Rect {
            x: column.x,
            y: rows[1].y,
            width: column.width,
            height: wide_drawer_height(app, available),
        })
    } else if area.width < 70 {
        Some(rows[1].inner(Margin::new(1, 0)))
    } else {
        let width = area.width.saturating_sub(4).min(49);
        Some(Rect {
            x: area.right().saturating_sub(width),
            y: rows[1].y,
            width,
            height: rows[1].height,
        })
    }
}

/// Keep short list drawers compact instead of making one completed agent look
/// like a full-height dashboard. Static inspectors and long lists retain the
/// available transcript height so their information stays usable.
fn wide_drawer_height(app: &App, available: u16) -> u16 {
    let preferred = match drawer_list_metrics(app) {
        Some((item_height, item_count)) => item_height
            .saturating_mul(item_count)
            .saturating_add(2)
            .min(usize::from(u16::MAX)) as u16,
        None if matches!(
            app.drawer_tab,
            DrawerTab::Activity | DrawerTab::Work | DrawerTab::Board | DrawerTab::Problems
        ) =>
        {
            5
        }
        None => available,
    };
    preferred.clamp(available.min(3), available)
}

/// Map a click to a drawer item index when it lands on the rendered panel.
pub(crate) fn drawer_hit(
    app: &App,
    column: u16,
    row: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Option<usize> {
    let area = Rect::new(0, 0, terminal_width, terminal_height);
    let rect = drawer_rect(app, area)?;
    let inner = rect.inner(Margin::new(1, 1));
    if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
        return None;
    }
    let (item_height, item_count) = drawer_list_metrics(app)?;
    // Item rows start below the bordered block's top edge.
    let inner_row = usize::from(row.saturating_sub(inner.y));
    let index = drawer_list_offset(app, rect, drawer_selected_index(app)) + inner_row / item_height;
    (index < item_count).then_some(index)
}

/// The fixed-height list measurements shared by painting and mouse hit
/// testing. The visible rows can be scrolled by ratatui, so a click must be
/// translated from the rendered viewport rather than treated as item zero.
fn drawer_list_metrics(app: &App) -> Option<(usize, usize)> {
    match app.drawer_tab {
        DrawerTab::Activity => (!app.agents.is_empty()).then_some((3, app.agents.len())),
        DrawerTab::Work => {
            let count = app.work_item_count();
            (count > 0).then_some((2, count))
        }
        DrawerTab::Board => (!app.board.is_empty()).then_some((4, app.board.len())),
        DrawerTab::Problems => (!app.incidents.is_empty()).then_some((4, app.incidents.len())),
        // Static key/value panels, nothing to select.
        DrawerTab::Route | DrawerTab::Usage | DrawerTab::Settings | DrawerTab::Help => None,
    }
}

fn drawer_selected_index(app: &App) -> usize {
    match app.drawer_tab {
        DrawerTab::Activity => app.selected_agent,
        DrawerTab::Work => app.selected_task,
        DrawerTab::Board => app.selected_board,
        DrawerTab::Problems => app.selected_problem,
        DrawerTab::Route | DrawerTab::Usage | DrawerTab::Settings | DrawerTab::Help => 0,
    }
}

fn drawer_list_offset(app: &App, area: Rect, selected: usize) -> usize {
    let Some((item_height, item_count)) = drawer_list_metrics(app) else {
        return 0;
    };
    let visible_items = (usize::from(area.height.saturating_sub(2)) / item_height).max(1);
    selected
        .min(item_count.saturating_sub(1))
        .saturating_sub(visible_items.saturating_sub(1))
}

fn drawer_list_state(app: &App, area: Rect, selected: usize) -> ListState {
    let selected = app
        .drawer_interactive()
        .then(|| drawer_list_metrics(app).map(|(_, item_count)| selected.min(item_count.saturating_sub(1))));
    let selected = selected.flatten();
    ListState::default()
        .with_selected(selected)
        .with_offset(selected.map_or(0, |selected| drawer_list_offset(app, area, selected)))
}

pub(crate) fn raster_surfaces(app: &App, area: Rect) -> Vec<RasterSurface> {
    if !kitty_surface_backing(app) || area.width < MIN_FULL_WIDTH || area.height < MIN_FULL_HEIGHT {
        return Vec::new();
    }

    let rows = vertical_rows(app, area);
    let composer = conversation_rail(rows[5]);
    let mut surfaces = Vec::new();
    let mut push = |rect: Rect, fill: Color| {
        if rect.width >= 2 && rect.height >= 2 {
            surfaces.push(RasterSurface {
                rect,
                fill: surface_fill_rgb(app, fill),
            });
        }
    };

    let wide_drawer = wide_drawer_active(app, area);
    if wide_drawer && let Some(drawer) = drawer_rect(app, area) {
        push(drawer, surface());
    }
    if app.items.is_empty() && app.focused_agent.is_none() && !app.completion_open() {
        push(welcome_rect(conversation_rail(rows[1])), surface());
    }
    if rows[2].height > 0 {
        push(conversation_rail(rows[2]), surface());
    }
    if rows[4].height > 0 {
        push(conversation_rail(rows[4]), surface());
    }
    push(composer, surface_alt());
    if !wide_drawer && let Some(drawer) = drawer_rect(app, area) {
        push(drawer, surface());
    }
    if let Some(popup) = completion_popup_rect(app, composer, rows[0].bottom()) {
        push(popup, surface());
    }
    if let Some(toast) = toast_rect(app, area) {
        push(toast, surface());
    }
    if let Some(overlay) = &app.overlay {
        let (max_width, max_height) = overlay_size(overlay);
        push(
            modal_rect(
                app,
                area,
                max_width.min(area.width.saturating_sub(4)),
                max_height.min(area.height.saturating_sub(2)),
            ),
            surface(),
        );
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

/// Compact layouts retain a centered 124-cell measure; wide layouts use a
/// centered 156-cell maximum. `primary_column` reserves the left breathing
/// room when an explicit operations drawer is open.
fn conversation_rail(area: Rect) -> Rect {
    if area.width < WIDE_RAIL_MIN_WIDTH {
        return centered_width(area, COMPACT_RAIL_MAX_WIDTH);
    }
    centered_width(area, WIDE_RAIL_MAX_WIDTH)
}

/// Cell-only renderers retain the lightweight surface geometry so titles,
/// padding, and hit testing stay stable. Kitty-backed controls opt into their
/// RGBA rounded corners separately through `kitty_surface_block`.
fn rounded_surfaces(app: &App) -> bool {
    !matches!(app.active_surface_renderer.as_str(), "square") && !matches!(app.effective_theme(), "no_color")
}

fn kitty_surface_backing(app: &App) -> bool {
    app.active_surface_renderer == "kitty" && app.effective_theme() != "no_color"
}

fn surface_fill_rgb(app: &App, fill: Color) -> [u8; 3] {
    if app.is_imported_theme_active() {
        return match fill {
            Color::Indexed(18) | Color::Rgb(15, 34, 57) => app.theme_palette.surface_alt,
            _ => app.theme_palette.surface,
        };
    }
    match fill {
        Color::Indexed(18) | Color::Rgb(15, 34, 57) => [15, 34, 57],
        _ => [10, 24, 43],
    }
}

fn surface_block<'a>(app: &App, title: Line<'a>, _accent: Color, _fill: Color) -> Block<'a> {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(if rounded_surfaces(app) {
            BorderType::QuadrantInside
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(background()));

    // `NO_COLOR` resets the foreground after rendering, so a canvas-colored
    // frame would become visible. Keep the border's layout reservation, but
    // render literal spaces instead of control glyphs in that mode.
    if app.effective_theme() == "no_color" {
        block.border_set(BLANK_BORDER_SET)
    } else {
        block
    }
}

/// These are the deliberately bounded controls that receive a real Kitty
/// rounded backing. The terminal-cell fallback stays flat; Ghostty/Kitty gets
/// the matching RGBA corner images after the normal cell draw.
fn kitty_surface_block<'a>(app: &App, title: Line<'a>, accent: Color, fill: Color) -> Block<'a> {
    let block = surface_block(app, title, accent, fill);
    if kitty_surface_backing(app) {
        block
            .border_style(Style::default().fg(fill))
            .style(Style::default().bg(fill))
    } else {
        block
    }
}

fn finish_surface(frame: &mut Frame<'_>, app: &App, area: Rect, _fill: Color) {
    if !rounded_surfaces(app) || area.width < 2 || area.height < 2 {
        return;
    }
    for (x, y) in [
        (area.x, area.y),
        (area.right().saturating_sub(1), area.y),
        (area.x, area.bottom().saturating_sub(1)),
        (area.right().saturating_sub(1), area.bottom().saturating_sub(1)),
    ] {
        frame.buffer_mut()[(x, y)]
            .set_fg(background())
            .set_bg(background());
    }
}

fn finish_kitty_surface(frame: &mut Frame<'_>, app: &App, area: Rect, fill: Color) {
    if !kitty_surface_backing(app) || area.width < 2 || area.height < 2 {
        finish_surface(frame, app, area, fill);
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

fn clear_to_canvas(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(background())), area);
}

fn apply_theme(frame: &mut Frame<'_>, app: &App) {
    let mut theme = app.effective_theme();
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
        if app.is_imported_theme_active() {
            remap_imported_theme_cell(cell, app);
            continue;
        }
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

/// Opaline import support maps the existing semantic Ratatui palette after a
/// normal draw. This keeps the default output exactly unchanged while making
/// every semantic text, list, and transcript style participate in a custom
/// theme without a risky renderer-wide rewrite.
fn remap_imported_theme_cell(cell: &mut ratatui::buffer::Cell, app: &App) {
    let palette = app.theme_palette;
    let map_foreground = |color| match color {
        Color::Black | Color::Rgb(5, 12, 24) => Color::Rgb(
            palette.background[0],
            palette.background[1],
            palette.background[2],
        ),
        Color::Gray => Color::Rgb(palette.text[0], palette.text[1], palette.text[2]),
        Color::White => Color::Rgb(palette.bright[0], palette.bright[1], palette.bright[2]),
        Color::DarkGray => Color::Rgb(palette.muted[0], palette.muted[1], palette.muted[2]),
        Color::Cyan => Color::Rgb(palette.active[0], palette.active[1], palette.active[2]),
        Color::Green => Color::Rgb(palette.good[0], palette.good[1], palette.good[2]),
        Color::Yellow => Color::Rgb(palette.warn[0], palette.warn[1], palette.warn[2]),
        Color::Red => Color::Rgb(palette.bad[0], palette.bad[1], palette.bad[2]),
        Color::Indexed(67) => Color::Rgb(palette.border[0], palette.border[1], palette.border[2]),
        Color::Rgb(43, 70, 101) => Color::Rgb(palette.border[0], palette.border[1], palette.border[2]),
        // Older persisted layouts can still carry semantic surface foregrounds.
        // Preserve their import mapping without making flat controls opaque.
        Color::Indexed(17) | Color::Rgb(10, 24, 43) => {
            Color::Rgb(palette.surface[0], palette.surface[1], palette.surface[2])
        }
        Color::Indexed(18) | Color::Rgb(15, 34, 57) => Color::Rgb(
            palette.surface_alt[0],
            palette.surface_alt[1],
            palette.surface_alt[2],
        ),
        color => color,
    };
    let map_background = |color| match color {
        Color::Black | Color::Rgb(5, 12, 24) => Color::Rgb(
            palette.background[0],
            palette.background[1],
            palette.background[2],
        ),
        Color::Indexed(17) | Color::Rgb(10, 24, 43) => {
            Color::Rgb(palette.surface[0], palette.surface[1], palette.surface[2])
        }
        Color::Indexed(18) | Color::Rgb(15, 34, 57) => Color::Rgb(
            palette.surface_alt[0],
            palette.surface_alt[1],
            palette.surface_alt[2],
        ),
        color => color,
    };
    cell.set_fg(map_foreground(cell.fg));
    cell.set_bg(map_background(cell.bg));
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let path = workspace_path_label(&app.root, dirs::home_dir().as_deref());
    let focus = app
        .focused_agent
        .and_then(|id| app.agents.iter().find(|agent| agent.id == id))
        .map(|agent| format!(" / {}", short_role(&agent.role)))
        .unwrap_or_default();
    let left = Line::styled(format!("{path}{focus}"), Style::default().fg(MUTED));
    let model = identity_model_label(Some("Mina"), &app.model);
    let mode = app.mode.label();
    let status = app.status.as_str();
    let right = format!("{model} · {mode}  {status}");
    let left_width = UnicodeWidthStr::width(format!("{path}{focus}").as_str()) as u16;
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(border()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(left), inner);
    let width = UnicodeWidthStr::width(right.as_str()) as u16;
    if width.saturating_add(left_width).saturating_add(2) <= inner.width {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(model, Style::default().fg(TEXT)),
                Span::styled(" · ", Style::default().fg(MUTED)),
                Span::styled(mode, Style::default().fg(MUTED)),
                Span::styled("  ", Style::default().fg(MUTED)),
                Span::styled(
                    status.to_owned(),
                    Style::default().fg(status_color(app.state)).bold(),
                ),
            ])),
            Rect {
                x: inner.right() - width,
                y: inner.y,
                width,
                height: 1,
            },
        );
    }
}

fn workspace_path_label(root: &Path, home: Option<&Path>) -> String {
    home.and_then(|home| root.strip_prefix(home).ok()).map_or_else(
        || root.display().to_string(),
        |relative| {
            if relative.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", relative.display())
            }
        },
    )
}

fn draw_transcript(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let inner = area.inner(Margin::new(2, 0));
    if app.items.is_empty() && app.focused_agent.is_none() && !app.completion_open() {
        // The empty state is itself a primary conversation surface: keep its
        // border on the rail's 3–5-cell outer gutter instead of indenting it
        // again as if it were transcript prose.
        draw_welcome(frame, app, area);
        return;
    }
    let mut layout = app.transcript_layout.borrow_mut();
    if layout.width != inner.width
        || layout.revision != app.transcript_revision
        || layout.focused_agent != app.focused_agent
        || layout.raw_transcript != app.raw_transcript_enabled()
    {
        let mut lines = Vec::new();
        if let Some(agent_id) = app.focused_agent {
            lines.extend(specialist_card_lines(app, agent_id, inner.width));
        }
        let visible = app.visible_items().collect::<Vec<_>>();
        let mut index = 0;
        while index < visible.len() {
            if matches!(visible[index], TranscriptItem::Tool { .. }) {
                // A completed tool burst is one piece of evidence even when it
                // alternates between searching, checking, and reading. Grouping
                // only identical adjacent names turned ordinary audits into a
                // long debug log. A running or failed call remains isolated so
                // it cannot be hidden in an otherwise successful receipt.
                let mut end = index + 1;
                while end < visible.len()
                    && matches!(visible[end], TranscriptItem::Tool { running: false, exit_code, .. } if !exit_code.is_some_and(|code| code != 0))
                {
                    end += 1;
                }
                lines.extend(activity_group_lines(&visible[index..end], inner.width));
                index = end;
            } else {
                lines.extend(item_lines(
                    visible[index],
                    inner.width,
                    &app.agents,
                    app.raw_transcript_enabled(),
                ));
                index += 1;
            }
        }
        layout.width = inner.width;
        layout.revision = app.transcript_revision;
        layout.focused_agent = app.focused_agent;
        layout.raw_transcript = app.raw_transcript_enabled();
        layout.lines = lines;
        layout.builds = layout.builds.saturating_add(1);
    }
    let estimated_height = layout.lines.len().min(usize::from(u16::MAX)) as u16;
    let max_scroll = estimated_height.saturating_sub(inner.height);
    layout.max_scroll = max_scroll;
    let scroll = app.scroll_state.offset_for(max_scroll);
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

/// A focused agent gets an evidence-backed specialist card before its compact
/// activity stream.  The card deliberately reads the dispatch receipt and
/// context events, never role-name heuristics or a raw sibling transcript.
fn specialist_card_lines(
    app: &App,
    agent_id: minha_core::protocol::EventAgentId,
    _width: u16,
) -> Vec<Line<'static>> {
    let Some(agent) = app.agents.iter().find(|agent| agent.id == agent_id) else {
        return Vec::new();
    };
    let receipt = app
        .dispatch_receipts
        .iter()
        .rev()
        .find(|receipt| receipt.agent_id == agent_id);
    let context = app.contexts.get(&agent_id);
    let identity = receipt
        .filter(|receipt| !receipt.book_sources.is_empty())
        .map(|receipt| {
            deterministic_specialist_identity(&receipt.role, &receipt.task_id, &receipt.book_sources)
        })
        .unwrap_or_else(|| persona_for(Some(&agent.role), &agent.model));
    // The model remains a separate, exact dispatch fact. It is never folded
    // into the deterministic specialist identity derived from role/task/books.
    let model = receipt
        .map(|receipt| identity_model_label(Some(&receipt.role), &receipt.model))
        .unwrap_or_else(|| identity_model_label(Some(&agent.role), &agent.model));
    let mut lines = vec![
        Line::styled(" specialist card ", Style::default().fg(ACTIVE).bold()),
        kv_line("identity", &identity),
        kv_line("model", &model),
        kv_line("state", &format!("{:?} · {}", agent.state, agent.detail)),
    ];
    if let Some(receipt) = receipt {
        lines.extend([
            kv_line("task", &receipt.task_id),
            kv_line("check", &receipt.acceptance_check),
            kv_line("lease", &receipt.lease_resources.join(", ")),
            kv_line(
                "dispatch",
                &format!(
                    "{} · {} / {} · {}",
                    receipt.provider,
                    format_tokens(receipt.session_used_tokens),
                    format_tokens(receipt.session_target_tokens),
                    receipt.budget_pressure
                ),
            ),
        ]);
        if !receipt.book_sources.is_empty() {
            lines.push(kv_line("book cards", &receipt.book_sources.join(", ")));
        }
    } else {
        lines.push(kv_line("dispatch", "awaiting a persisted receipt"));
    }
    if let Some(context) = context {
        lines.push(kv_line(
            "context",
            &format!(
                "{} / {} · {} reserve",
                format_tokens(context.estimated_tokens),
                format_tokens(context.effective_limit),
                format_tokens(context.protected_reserve)
            ),
        ));
    }
    if let Some(todos) = app.todos.get(&agent_id)
        && let Some(todo) = todos.iter().find(|todo| todo.state != TodoState::Completed)
    {
        lines.push(kv_line("next", &todo.objective));
    }
    lines.push(Line::styled(
        " activity below is scoped to this agent · Esc returns to the run",
        Style::default().fg(MUTED),
    ));
    lines.push(Line::raw(""));
    lines
}

/// Stable visible identity for a receipt-backed specialist. Receipt source
/// order is not part of an agent identity, so normalize it before display.
fn deterministic_specialist_identity(role: &str, task_id: &str, book_sources: &[String]) -> String {
    let mut sources = book_sources
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .collect::<Vec<_>>();
    sources.sort_unstable();
    sources.dedup();
    format!("{} · {} · {}", role.trim(), task_id.trim(), sources.join(", "))
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

fn activity_group_lines(items: &[&TranscriptItem], width: u16) -> Vec<Line<'static>> {
    let mut running = false;
    let mut failed = false;
    let mut expanded = false;
    let mut targets = Vec::new();
    let mut kinds = Vec::<(&str, usize)>::new();
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
            let kind = activity_kind(name);
            if let Some((_, count)) = kinds.iter_mut().find(|(seen, _)| *seen == kind) {
                *count += 1;
            } else {
                kinds.push((kind, 1));
            }
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
        GOOD
    };
    let count = items.len();
    let label = if kinds.len() == 1 { kinds[0].0 } else { "Activity" };
    let kinds_summary = (kinds.len() > 1).then(|| {
        kinds
            .iter()
            .map(|(kind, count)| format!("{kind} × {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    });
    let target = targets.last().cloned();
    let summary = match (kinds_summary, target) {
        (Some(kinds), Some(target)) => format!("{kinds} · {target}"),
        (Some(kinds), None) => kinds,
        (None, Some(target)) => target,
        (None, None) => format!("{} operation{}", count, if count == 1 { "" } else { "s" }),
    };
    let summary = truncate_display(&summary, usize::from(width.saturating_sub(24).max(8)));
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("  {marker} "), Style::default().fg(color)),
        Span::styled(label.to_owned(), Style::default().fg(color).bold()),
        Span::styled(
            if count == 1 {
                format!(" · {summary}")
            } else {
                format!(" · {count} items · {summary}")
            },
            Style::default().fg(TEXT),
        ),
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
        lines.push(Line::raw(""));
    }
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
    let welcome = welcome_rect(area);
    let text = if welcome.width < 72 {
        vec![
            Line::styled("Start with a request.", Style::default().fg(BRIGHT).bold()),
            Line::styled("Try /plan, /audit, or /review.", Style::default().fg(MUTED)),
            Line::styled("Ctrl-P commands · ? help", Style::default().fg(MUTED)),
        ]
    } else {
        vec![
            Line::styled("Start with a request.", Style::default().fg(BRIGHT).bold()),
            Line::styled(
                "Describe a change, ask for an audit, or choose a local control.",
                Style::default().fg(MUTED),
            ),
            Line::raw(""),
            Line::from(vec![
                Span::styled("PLAN ", Style::default().fg(ACTIVE).bold()),
                Span::styled("/plan    ", Style::default().fg(TEXT)),
                Span::styled("AUDIT ", Style::default().fg(ACTIVE).bold()),
                Span::styled("/audit    ", Style::default().fg(TEXT)),
                Span::styled("REVIEW ", Style::default().fg(ACTIVE).bold()),
                Span::styled("/review", Style::default().fg(TEXT)),
            ]),
            Line::styled(
                format!(
                    "{} · automatic compaction · Ctrl-P commands · ? help",
                    identity_model_label(Some("Mina"), &app.model)
                ),
                Style::default().fg(MUTED),
            ),
        ]
    };
    frame.render_widget(
        Paragraph::new(text)
            .block(kitty_surface_block(
                app,
                Line::styled(" minha ", Style::default().fg(ACTIVE).bold()),
                border(),
                surface(),
            ))
            .wrap(Wrap { trim: true }),
        welcome,
    );
    finish_kitty_surface(frame, app, welcome, surface());
}

/// A deliberately sized empty-state surface gives an idle terminal a stable
/// center of gravity. It shares the composer's center axis while remaining
/// vertically centered in the transcript, so a small onboarding card reads as
/// intentional empty-state content rather than a stray left-side panel.
fn welcome_rect(area: Rect) -> Rect {
    let preferred_height = if area.width >= 72 { 7 } else { 5 };
    let height = preferred_height.min(area.height);
    let width = area.width.min(76);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn item_lines(
    item: &TranscriptItem,
    width: u16,
    agents: &[AgentView],
    raw_transcript: bool,
) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::User { text, steering } => {
            let label = if *steering { "steer" } else { "you" };
            boxed_lines(label, text, ACTIVE, surface_alt(), width)
        }
        TranscriptItem::Assistant {
            agent_id,
            role,
            text,
            streaming,
            ..
        } => {
            let model = agents
                .iter()
                .find(|agent| agent.id == *agent_id)
                .map(|agent| agent.model.as_str());
            if raw_transcript {
                raw_assistant_lines(role, model, text, *streaming, width)
            } else {
                assistant_lines(role, model, text, *streaming, width)
            }
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
            diff_lines(
                &format!(
                    "diff · {} · +{additions} -{deletions}",
                    path.clone().unwrap_or_else(|| "working tree".into())
                ),
                diff,
                *expanded,
                width,
                raw_transcript,
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
            // One label per kind: the previous mapping collapsed seven kinds
            // into "Blocked", "Handed off", and two spellings of "Coordinated".
            let label = match kind.as_str() {
                "blocker" => "Blocked",
                "handoff" => "Handed off",
                "decision" => "Decided",
                "artifact_reference" => "Shared",
                "request" => "Asked",
                "finding" => "Found",
                "progress" => "Progress",
                _ => "Coordinated",
            };
            let color = match kind.as_str() {
                "blocker" => WARN,
                "request" | "handoff" => ACTIVE,
                _ => MUTED,
            };
            let mut lines = styled_wrap(
                vec![StyledChunk::new(summary.clone(), Style::default().fg(TEXT))],
                usize::from(width.max(1)),
                vec![
                    Span::styled("  • ", Style::default().fg(color)),
                    Span::styled(format!("{label}  "), Style::default().fg(color).bold()),
                ],
                vec![Span::raw("    ")],
            );
            // The run room is where coordination normally happens, so only a
            // private room is worth the extra column.
            let room = if room_id == "run" {
                String::new()
            } else {
                format!(" · room {room_id}")
            };
            lines.push(Line::styled(
                format!(
                    "    {} → {}{room}",
                    address_label(agents, sender),
                    address_label(agents, recipient)
                ),
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

fn assistant_lines(
    role: &str,
    model: Option<&str>,
    text: &str,
    streaming: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(if streaming { ACTIVE } else { GOOD })),
        Span::styled(
            truncate_display(
                &identity_model_label(Some(role), model.unwrap_or("model unavailable")),
                width.saturating_sub(2).max(1),
            ),
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
        if is_horizontal_rule(trimmed) {
            lines.push(Line::styled(
                format!("  {}", "─".repeat(width.saturating_sub(2).max(1))),
                Style::default().fg(border()),
            ));
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

fn raw_assistant_lines(
    role: &str,
    model: Option<&str>,
    text: &str,
    streaming: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = boxed_lines(
        &format!(
            "{} · raw{}",
            identity_model_label(Some(role), model.unwrap_or("model unavailable")),
            if streaming { " · streaming" } else { "" },
        ),
        text,
        ACTIVE,
        surface(),
        width,
    );
    if streaming {
        lines.insert(1, Line::styled("    streaming…", Style::default().fg(ACTIVE)));
    }
    lines
}

fn diff_lines(title: &str, diff: &str, expanded: bool, width: u16, raw: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("  • ", Style::default().fg(ACTIVE)),
        Span::styled(title.to_owned(), Style::default().fg(ACTIVE).bold()),
    ])];
    if !expanded {
        lines.push(Line::styled(
            "    Ctrl-O expands diff",
            Style::default().fg(MUTED),
        ));
    } else {
        let cap = 320;
        for source in diff.lines().take(cap) {
            let color = if raw {
                TEXT
            } else if source.starts_with("+++") || source.starts_with("---") {
                MUTED
            } else if source.starts_with('+') {
                GOOD
            } else if source.starts_with('-') {
                BAD
            } else if source.starts_with("@@") {
                ACTIVE
            } else if source.starts_with("diff ") || source.starts_with("index ") {
                BRIGHT
            } else {
                TEXT
            };
            for part in wrap_display(source, usize::from(width.saturating_sub(4).max(1))) {
                lines.push(Line::styled(format!("    {part}"), Style::default().fg(color)));
            }
        }
        if diff.lines().count() > cap {
            lines.push(Line::styled(
                "    … diff truncated in view",
                Style::default().fg(MUTED),
            ));
        }
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
            || is_horizontal_rule(trimmed)
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

fn is_horizontal_rule(text: &str) -> bool {
    let markers = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<Vec<_>>();
    markers.len() >= 3
        && matches!(markers.first(), Some('-' | '*' | '_'))
        && markers.windows(2).all(|pair| pair[0] == pair[1])
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
            chunks.push(StyledChunk::new(&rest[..end], Style::default().fg(BRIGHT).bold()));
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

/// Expand logical styled lines into the physical terminal rows they occupy.
/// Scroll offsets use rendered rows, so doing this before publishing an
/// inspector's range keeps wrapped content reachable on compact terminals.
fn wrap_styled_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| {
            let chunks = line
                .spans
                .into_iter()
                .map(|span| StyledChunk::new(span.content.into_owned(), span.style))
                .collect::<Vec<_>>();
            if chunks.iter().all(|chunk| chunk.text.is_empty()) {
                vec![Line::raw("")]
            } else {
                styled_wrap(chunks, width.max(1), Vec::new(), Vec::new())
            }
        })
        .collect()
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
        DrawerTab::Route => draw_route(frame, app, area),
        DrawerTab::Usage => draw_usage(frame, app, area),
        DrawerTab::Settings => draw_settings(frame, app, area),
        DrawerTab::Help => draw_help_drawer(frame, app, area),
    }
}

/// Operations panels share the rounded-surface contract with the composer and
/// inspectors: Kitty receives RGBA corners, while cell-only renderers retain
/// the compact flat fallback.
fn drawer_block<'a>(app: &App, title: Line<'a>) -> Block<'a> {
    kitty_surface_block(app, title, ACTIVE, surface())
}

fn draw_agents(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Activity));
    if app.agents.is_empty() {
        frame.render_widget(
            Paragraph::new("No agents spawned.\nMina creates workers only when useful.")
                .block(block)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let items = app.agents.iter().map(agent_item).collect::<Vec<_>>();
    let mut state = drawer_list_state(app, area, app.selected_agent);
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::default().fg(ACTIVE).bold())
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_work(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Work));
    if app.plan.is_empty() && app.todos.values().all(Vec::is_empty) {
        frame.render_widget(
            Paragraph::new("No work plan yet.\nPlan tasks appear here when Mina decomposes a goal.")
                .block(block)
                .style(Style::default().fg(MUTED))
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
    let mut state = drawer_list_state(app, area, app.selected_task);
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::default().fg(ACTIVE).bold())
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_board(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Board));
    if app.board.is_empty() {
        frame.render_widget(
            Paragraph::new("No notes yet.\n/note adds one; agents share durable findings here.")
                .block(block)
                .style(Style::default().fg(MUTED))
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
                // `author_agent_id` is written on every agent post but was
                // never rendered, so the board read as authorless.
                Line::styled(
                    format!(
                        "{} · {} · {} · {}",
                        entry.scope,
                        entry.status,
                        entry.author_agent_id.map_or_else(
                            || "you".to_owned(),
                            |author| agent_label(&app.agents, &author.to_string()),
                        ),
                        short_id(&entry.id)
                    ),
                    Style::default().fg(MUTED),
                ),
                Line::styled(truncate_chars(&entry.body, 90), Style::default().fg(TEXT)),
                Line::raw(""),
            ])
        })
        .collect::<Vec<_>>();
    let mut state = drawer_list_state(app, area, app.selected_board);
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::default().fg(ACTIVE).bold())
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_problems(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Problems));
    if app.incidents.is_empty() {
        frame.render_widget(
            Paragraph::new("No incidents recorded.\nRuntime warnings and retryable failures collect here.")
                .block(block)
                .style(Style::default().fg(MUTED))
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
    let mut state = drawer_list_state(app, area, app.selected_problem);
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(Style::default().fg(ACTIVE).bold())
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

fn draw_route(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Route));
    if app.last_routing.is_none() && app.dispatch_receipts.is_empty() {
        frame.render_widget(
            Paragraph::new("No routing decision yet.\nThe first message picks a mode; worker dispatches will then show their recorded admission evidence here.")
                .block(block)
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let mut lines = Vec::new();
    if let Some(route) = &app.last_routing {
        lines.extend([
            Line::styled("last mode decision", Style::default().fg(ACTIVE).bold()),
            kv_line("mode", &route.mode),
            kv_line("reason", &route.reason),
            kv_line("provider", &route.provider),
            kv_line(
                "model",
                &identity_model_label(Some("Mina"), route.model.as_deref().unwrap_or("n/a")),
            ),
        ]);
    }
    if let Some(receipt) = app.dispatch_receipts.last() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.extend([
            Line::styled("last worker dispatch", Style::default().fg(ACTIVE).bold()),
            kv_line("recorded", &format!("{} receipt(s)", app.dispatch_receipts.len())),
            kv_line("receipt", &receipt.receipt_id),
            kv_line("generation", &receipt.generation.to_string()),
            kv_line("task", &receipt.task_id),
            kv_line("role", &receipt.role),
            kv_line("provider", &receipt.provider),
            kv_line(
                "model",
                &identity_model_label(Some(&receipt.role), &receipt.model),
            ),
            kv_line(
                "budget",
                &format!(
                    "{} / {} · {}",
                    format_tokens(receipt.session_used_tokens),
                    format_tokens(receipt.session_target_tokens),
                    receipt.budget_pressure
                ),
            ),
            kv_line("parallel", &receipt.parallelism_reason),
            kv_line("check", &receipt.acceptance_check),
        ]);
        if receipt.estimated_input_tokens > 0 {
            lines.push(kv_line(
                "estimated input",
                &format_tokens(receipt.estimated_input_tokens),
            ));
        }
        if !receipt.lease_resources.is_empty() {
            lines.push(kv_line("leases", &receipt.lease_resources.join(", ")));
        }
        if !receipt.book_sources.is_empty() {
            lines.push(kv_line("book sources", &receipt.book_sources.join(", ")));
        }
        if receipt.candidates.is_empty() {
            lines.push(Line::styled(
                "  no candidate comparison was recorded",
                Style::default().fg(MUTED),
            ));
        } else {
            lines.push(Line::styled(
                "  admission candidates",
                Style::default().fg(ACTIVE).bold(),
            ));
            for candidate in &receipt.candidates {
                let selected = candidate.provider == receipt.provider && candidate.model == receipt.model;
                let accepted = candidate.eligible && selected;
                let marker = if accepted {
                    "✓"
                } else if candidate.eligible {
                    "·"
                } else {
                    "×"
                };
                let color = if accepted {
                    GOOD
                } else if candidate.eligible {
                    ACTIVE
                } else {
                    MUTED
                };
                let reason = if candidate.reason.trim().is_empty() {
                    if accepted {
                        "selected".to_owned()
                    } else if candidate.eligible {
                        "eligible, not selected".to_owned()
                    } else {
                        "not eligible".to_owned()
                    }
                } else {
                    candidate.reason.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {marker} "), Style::default().fg(color).bold()),
                    Span::styled(
                        identity_model_label(Some(&receipt.role), &candidate.model),
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        format!(" · {} · {reason}", candidate.provider),
                        Style::default().fg(MUTED),
                    ),
                ]));
            }
        }
        let previous = app
            .dispatch_receipts
            .iter()
            .rev()
            .skip(1)
            .take(3)
            .collect::<Vec<_>>();
        if !previous.is_empty() {
            lines.push(Line::styled(
                "  recent earlier dispatches",
                Style::default().fg(MUTED).bold(),
            ));
            for earlier in previous {
                lines.push(Line::styled(
                    format!(
                        "    {} · {}",
                        earlier.task_id,
                        identity_model_label(Some(&earlier.role), &earlier.model)
                    ),
                    Style::default().fg(MUTED),
                ));
            }
        }
    }
    if let Some(warning) = &app.last_warning {
        lines.push(Line::raw(""));
        lines.push(Line::styled("last warning", Style::default().fg(WARN).bold()));
        lines.push(Line::styled(warning.clone(), Style::default().fg(TEXT)));
    }
    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

fn draw_usage(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Usage));
    let balance = match (&app.deepseek_balance, app.deepseek_reserve_percent) {
        (Some(balance), Some(percent)) => format!("{balance} · {percent:.1}% reserve"),
        (Some(balance), None) => balance.clone(),
        (None, _) => "unavailable".into(),
    };
    let mut lines = vec![
        Line::styled("session tokens", Style::default().fg(ACTIVE).bold()),
        kv_line("input", &format_tokens(app.input_tokens)),
        kv_line("output", &format_tokens(app.output_tokens)),
        kv_line("cached input", &format_tokens(app.cached_input_tokens)),
        kv_line("cache writes", &format_tokens(app.cache_write_tokens)),
        kv_line("reasoning", &format_tokens(app.reasoning_output_tokens)),
        Line::raw(""),
        Line::styled("reference pricing", Style::default().fg(ACTIVE).bold()),
        kv_line("DeepSeek", &format!("${:.6}", app.deepseek_estimated_usd)),
        kv_line(
            "MiMo",
            &format!("${:.6} · quota unavailable by API", app.mimo_estimated_usd),
        ),
        Line::raw(""),
        Line::styled("context", Style::default().fg(ACTIVE).bold()),
        kv_line("used", &format!("{:.0}% estimated", app.context_percent())),
        Line::raw(""),
        Line::styled("provider balance", Style::default().fg(ACTIVE).bold()),
        kv_line(
            if app.balance_provider.is_empty() {
                "balance"
            } else {
                app.balance_provider.as_str()
            },
            &balance,
        ),
    ];
    append_account_usage(&mut lines, app);
    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

/// A compact, anchored editor for the versioned user-local settings document.
/// It keeps the active tab behavior intact while making its persistence scope,
/// contrast result, preview state, and mutation grammar visible in one place.
fn draw_settings(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = drawer_block(app, drawer_tabs(DrawerTab::Settings));
    let contrast = app.theme_palette.contrast_report();
    let theme_source = app
        .tui_settings
        .imported_theme
        .as_ref()
        .map_or("built in", |theme| theme.source.as_str());
    let persisted = app.settings_path.as_ref().map_or_else(
        || "unavailable on this host".into(),
        |path| path.display().to_string(),
    );
    let preview = if app.preview_settings.is_some() {
        "preview active · /theme apply or /theme reset"
    } else if app.settings_dirty {
        "save pending"
    } else {
        "saved"
    };
    let mut lines = vec![
        settings_section("appearance", &format!("{} · {preview}", app.effective_theme())),
        settings_section("theme source", theme_source),
        settings_section(
            "contrast",
            &format!(
                "text {:.2}:1 {} · muted {:.2}:1 {} · accent {:.2}:1 {}",
                contrast.normal,
                if contrast.normal_passes() { "AA" } else { "low" },
                contrast.muted,
                if contrast.muted_passes() { "AA" } else { "low" },
                contrast.active,
                if contrast.active_passes() { "pass" } else { "low" },
            ),
        ),
        settings_section(
            "surface",
            &format!("{} → {}", app.surface_renderer, app.active_surface_renderer),
        ),
        settings_section(
            "layout",
            "transcript-first panes · renderer choice applies on the next frame",
        ),
        settings_section(
            "scrolling",
            &format!(
                "{} lines · Vim {} · h/j/k/l, i/a/I/A, x, dd/D/C, yy/p, o/O, u/Ctrl-R, Esc",
                app.tui_settings.scroll_lines,
                if app.tui_settings.vim_scroll { "on" } else { "off" },
            ),
        ),
        settings_section(
            "input & keybindings",
            "Vim is local-only; composer edits never dispatch a run by themselves",
        ),
        settings_section(
            "transcript",
            if app.tui_settings.raw_transcript {
                "raw copy view · Ctrl-O expands large paste"
            } else {
                "formatted Markdown/diffs · /settings raw on for copy view"
            },
        ),
        settings_section(
            "accessibility",
            &format!(
                "{} motion · {} · NO_COLOR overrides every theme",
                if app.reduced_motion { "reduced" } else { "standard" },
                if app.no_color_forced {
                    "NO_COLOR active"
                } else {
                    "color allowed"
                }
            ),
        ),
        settings_section(
            "providers",
            &format!(
                "{} · routing and credentials remain workspace/runtime data",
                identity_model_label(Some("Mina"), &app.model)
            ),
        ),
        settings_section(
            "usage",
            &format!(
                "{} in · {} out · context {} / {}",
                format_tokens(app.input_tokens),
                format_tokens(app.output_tokens),
                format_tokens(app.current_context_tokens),
                format_tokens(app.context_limit),
            ),
        ),
        settings_section(
            "advanced",
            &format!(
                "settings schema v{} · renderer reloads safely",
                crate::settings::SETTINGS_VERSION
            ),
        ),
        settings_section("storage", &format!("user-local only · {persisted}")),
        settings_section("edit", "/theme import|export|contrast|preview · /settings help"),
    ];
    if let Some(notice) = &app.settings_notice {
        lines.push(settings_section("last change", notice));
    }
    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

fn settings_section(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} · "), Style::default().fg(ACTIVE).bold()),
        Span::styled(value.to_owned(), Style::default().fg(TEXT)),
    ])
}

fn drawer_tabs(active: DrawerTab) -> Line<'static> {
    // A `Block` title is a single clipped line.  Rendering all labels meant
    // Route/Usage could be active but invisible on the normal 48-column
    // Operations drawer.  Keep the active semantic label and position first;
    // slash commands remain the discoverable, direct tab switcher.
    Line::from(vec![
        Span::styled(" operations · ", Style::default().fg(MUTED)),
        Span::styled(
            format!("[{}]", active.label()),
            Style::default().fg(ACTIVE).bold(),
        ),
        Span::styled(
            format!(" · {}/{}", active.position(), DrawerTab::count()),
            Style::default().fg(MUTED),
        ),
    ])
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
            format!(
                "  {} · {}",
                identity_model_label(Some(&agent.role), &agent.model),
                agent.detail
            ),
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
        Paragraph::new(Text::from(lines.collect::<Vec<_>>())).block(kitty_surface_block(
            app,
            Line::styled(" tasks ", Style::default().fg(MUTED)),
            border(),
            surface(),
        )),
        area,
    );
    finish_kitty_surface(frame, app, area, surface());
}

/// Height of the inline decision-card row: zero when nothing needs an
/// answer, otherwise the card's content plus its border, so the reserved
/// layout seam (`Constraint::Length(question_height)` in `vertical_rows` and
/// `raster_surfaces`) actually gets space instead of always collapsing.
fn clarification_height(app: &App, width: u16) -> u16 {
    match active_card_content(app, width.saturating_sub(2)) {
        Some(content) => content.lines.len() as u16 + 2,
        None => 0,
    }
}

struct InlineQuestionContent {
    lines: Vec<Line<'static>>,
    option_lines: Vec<(usize, usize, usize)>,
}

/// Content for whichever decision card is currently active: a pre-run
/// clarification takes priority (mirroring `has_active_clarification`'s use
/// elsewhere), otherwise a mid-run question or exec-approval request. The two
/// sources render through the same visual grammar — question, then
/// `question_option_lines` per option — so the inline card, its height, and
/// its mouse hit-testing never have to special-case which source is active.
fn active_card_content(app: &App, inner_width: u16) -> Option<InlineQuestionContent> {
    if app.has_active_clarification() {
        return inline_question_lines(app, inner_width);
    }
    Some(pending_request_lines(
        app,
        app.pending_request.as_ref()?,
        inner_width,
    ))
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
    lines.extend(indented_muted_lines(
        "↑/↓ choose · Enter answer · or type a custom answer",
        width,
    ));
    Some(InlineQuestionContent { lines, option_lines })
}

/// One option row within a decision card. `description` may be empty — a
/// mid-run question's options usually have no elaboration beyond their
/// label, unlike clarification options and approve/decline, which do.
fn question_option_lines(label: &str, description: &str, selected: bool, width: usize) -> Vec<Line<'static>> {
    let marker = if selected { "  › " } else { "    " };
    let mut chunks = vec![StyledChunk::new(
        label.to_owned(),
        Style::default().fg(if selected { BRIGHT } else { TEXT }).bold(),
    )];
    if !description.is_empty() {
        chunks.push(StyledChunk::new(
            format!(" — {description}"),
            Style::default().fg(MUTED),
        ));
    }
    styled_wrap(
        chunks,
        width,
        vec![Span::styled(marker, Style::default().fg(ACTIVE))],
        vec![Span::raw("      ")],
    )
}

/// Pre-wrap the card's static instructional text at the same width used for
/// questions and options. Leaving it as one long `Line` let `Paragraph` wrap
/// it later, after height calculation and mouse hit-testing had already
/// assumed it occupied one row.
fn indented_muted_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    styled_wrap(
        vec![StyledChunk::new(text.to_owned(), Style::default().fg(MUTED))],
        width,
        vec![Span::raw("    ")],
        vec![Span::raw("    ")],
    )
}

/// Wrap a labeled field (e.g. "Run: <command>") with a continuation prefix
/// sized to match, so wrapped lines line up under the value rather than the
/// label.
/// Word-wraps `value` under a `"Label: "` prefix. Splits on embedded newlines
/// first (`styled_wrap` alone does not — it word-wraps as one run, so a
/// multi-paragraph value like the integration-approval scope report loses
/// all of its line breaks and reads as one wall of text) so each source line
/// gets its own wrap pass, matching how `boxed_lines` already handles this.
fn labeled_field(label: &str, value: String, width: usize) -> Vec<Line<'static>> {
    let prefix = format!("    {label}: ");
    let continuation = " ".repeat(prefix.chars().count());
    let mut lines = Vec::new();
    let mut first = true;
    for source_line in value.lines().chain(value.is_empty().then_some("")) {
        let (first_prefix, continuation_prefix) = if first {
            (
                vec![Span::styled(prefix.clone(), Style::default().fg(MUTED))],
                vec![Span::raw(continuation.clone())],
            )
        } else {
            (
                vec![Span::raw(continuation.clone())],
                vec![Span::raw(continuation.clone())],
            )
        };
        lines.extend(styled_wrap(
            vec![StyledChunk::new(
                source_line.to_owned(),
                Style::default().fg(TEXT),
            )],
            width,
            first_prefix,
            continuation_prefix,
        ));
        first = false;
    }
    lines
}

/// Inline card content for a mid-run question or exec-approval request
/// (`PendingRequest`) — the same visual grammar as `inline_question_lines`
/// (question, then one `question_option_lines` row per option), built from a
/// different data source. For approval specifically this states the effect
/// (the command) and the evidence (the reason) using only what's already on
/// the event; it does not invent scope data (file/check counts) that isn't
/// there yet.
fn pending_request_lines(app: &App, request: &PendingRequest, width: u16) -> InlineQuestionContent {
    let width = usize::from(width.saturating_sub(4).max(12));
    let mut lines = Vec::new();
    let mut option_lines = Vec::new();

    let (icon, icon_color, kind) = if request.approval {
        ("! ", WARN, "Approval required")
    } else {
        ("? ", ACTIVE, "Question")
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {icon}"), Style::default().fg(icon_color).bold()),
        Span::styled(kind, Style::default().fg(MUTED)),
    ]));
    lines.extend(styled_wrap(
        vec![StyledChunk::new(
            request.question.clone(),
            Style::default().fg(BRIGHT).bold(),
        )],
        width,
        vec![Span::raw("    ")],
        vec![Span::raw("    ")],
    ));
    if let Some(command) = &request.command {
        lines.extend(labeled_field("Run", command.join(" "), width));
    }
    if let Some(reason) = &request.reason {
        lines.extend(labeled_field("Why", reason.clone(), width));
    }
    if request.approval {
        let caption = if request.command.is_some() {
            "Only the command above runs; nothing else is affected by this approval."
        } else {
            "This approval has no fixed command; read Why above for its actual scope."
        };
        lines.extend(indented_muted_lines(caption, width));
    }
    for (index, option) in request.options.iter().enumerate() {
        let (label, description) = pending_option_wording(request, option);
        let start = lines.len();
        lines.extend(question_option_lines(
            &label,
            &description,
            index == app.selected_clarification_option,
            width,
        ));
        option_lines.push((start, lines.len(), index));
    }
    lines.extend(indented_muted_lines(
        "↑/↓ choose · Enter answers · or type a custom answer",
        width,
    ));
    InlineQuestionContent { lines, option_lines }
}

/// Nicer display wording for a `PendingRequest` option, without changing the
/// underlying value that gets submitted as the answer.
fn pending_option_wording(request: &PendingRequest, option: &str) -> (String, String) {
    if request.approval {
        let has_command = request.command.is_some();
        match option {
            "yes" if has_command => ("Approve".to_owned(), "Run the command above.".to_owned()),
            "yes" => (
                "Approve".to_owned(),
                "Proceed; see Why above for scope.".to_owned(),
            ),
            "no" if has_command => ("Decline".to_owned(), "Skip it; nothing runs.".to_owned()),
            "no" => (
                "Decline".to_owned(),
                "Stop here; nothing further runs.".to_owned(),
            ),
            other => (other.to_owned(), String::new()),
        }
    } else {
        (option.to_owned(), String::new())
    }
}

/// Center a modal inside the transcript region (below the header, above the
/// composer) so the composer and its "answer required" hint stay visible and
/// typed answers are never hidden behind the dialog.
fn modal_rect(app: &App, area: Rect, width: u16, height: u16) -> Rect {
    let rows = vertical_rows(app, area);
    // The header is a persistent landmark: modal content belongs in the
    // transcript region beneath it, not in the terminal's full rectangle.
    // Starting at `area.y` made every inspector erase the workspace/model
    // identity at compact sizes.
    let top = rows[0].bottom();
    let bottom = rows[5].y.saturating_sub(1).max(top);
    let available = bottom.saturating_sub(top).max(1);
    let height = height.min(available).max(1);
    let width = width.min(area.width.saturating_sub(4)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: top + available.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Renders the decision card into its own reserved row — directly above the
/// composer, sized by `clarification_height` — instead of a centered modal.
/// `area` is expected to already be the row rect (see `draw`).
fn draw_decision_card(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(content) = active_card_content(app, area.width.saturating_sub(2)) else {
        return;
    };
    let approval = app
        .pending_request
        .as_ref()
        .is_some_and(|request| request.approval);
    let (title, accent) = if approval {
        (" approval · action required ", WARN)
    } else {
        (" question · answer required ", ACTIVE)
    };
    let block = kitty_surface_block(
        app,
        Line::styled(title, Style::default().fg(BRIGHT).bold()),
        accent,
        surface(),
    );
    let inner = block.inner(area);
    let scroll = decision_card_scroll(
        &content,
        app.selected_clarification_option,
        usize::from(inner.height),
    );
    clear_to_canvas(frame, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(content.lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
    finish_kitty_surface(frame, app, area, surface());
}

/// Keep the currently selected answer visible when an inline card is capped
/// to protect the composer. This is derived from card content rather than
/// stored state, so keyboard movement, redraws, and mouse hit-testing share
/// one stable viewport without another mutable scroll offset to drift.
fn decision_card_scroll(content: &InlineQuestionContent, selected: usize, visible: usize) -> usize {
    if visible == 0 || content.lines.len() <= visible {
        return 0;
    }
    let Some((start, end, _)) = content
        .option_lines
        .iter()
        .find(|(_, _, option)| *option == selected)
        .copied()
    else {
        return 0;
    };
    if end <= visible {
        return 0;
    }
    let last_scroll = content.lines.len().saturating_sub(visible);
    if end.saturating_sub(start) >= visible {
        start.min(last_scroll)
    } else {
        end.saturating_sub(visible).min(last_scroll)
    }
}

fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // A clarification and a pending request share the same inline-card
    // grammar (arrow keys choose, Enter answers, typing overrides), so the
    // composer treats them identically here too.
    let awaiting_card_answer = app.has_active_clarification() || app.pending_request.is_some();
    let paused = paused_run(app);
    let targeted_title = app
        .message_target
        .as_ref()
        .map(|target| format!(" message {target} · Enter sends · /to clears "));
    let title = if let Some(title) = targeted_title.as_deref() {
        title.to_owned()
    } else if awaiting_card_answer && !app.input.is_empty() {
        " custom answer · Enter submits · Esc clears ".into()
    } else if awaiting_card_answer {
        " ↑/↓ choose · Enter answers · typing writes a custom answer ".into()
    } else if app.running {
        " steer lead · Enter queues · Esc Esc pauses · Ctrl-C interrupts ".into()
    } else if paused {
        match app.termination_reason {
            Some(TerminationReason::BudgetTarget) => {
                " session budget paused · 5% recovery reserve held · /usage explains admission ".into()
            }
            Some(TerminationReason::ProviderReserve) => {
                " provider reserve paused · /usage shows the account window ".into()
            }
            _ => " usage paused · /usage shows the held reserve ".into(),
        }
    } else {
        " message · Enter sends · Shift-Enter newline ".into()
    };
    let title = if let Some(mode) = app.vim_mode_label() {
        // Ratatui truncates long border titles on compact terminals, so the
        // mode must lead rather than be the first useful detail clipped.
        format!(" VIM {mode} ·{title}")
    } else {
        title
    };
    let collapsed_paste = app.paste_summary.as_ref().filter(|summary| !summary.expanded);
    let placeholder = if let Some(summary) = collapsed_paste {
        format!(
            "[Pasted {} lines / {} graphemes · Ctrl-O expands · Enter sends full text]",
            summary.lines, summary.graphemes
        )
    } else if app.input.is_empty() {
        if awaiting_card_answer {
            "Type a custom answer…".into()
        } else if app.running {
            "Type guidance while the hive works…".into()
        } else if paused {
            "This run is paused safely; inspect /usage before continuing…".into()
        } else {
            "Ask about the repository or describe work…".into()
        }
    } else {
        app.input.clone()
    };
    let style = if app.input.is_empty() || collapsed_paste.is_some() {
        MUTED
    } else {
        BRIGHT
    };
    let accent = if app.pending_request.is_some() {
        WARN
    } else if app.running {
        ACTIVE
    } else if paused {
        WARN
    } else {
        MUTED
    };
    let block = kitty_surface_block(
        app,
        Line::styled(title, Style::default().fg(if paused { WARN } else { MUTED })),
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
    if app.input.is_empty() || collapsed_paste.is_some() {
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
            .style(Style::default().fg(style))
            .wrap(Wrap { trim: false }),
        area,
    );
    finish_kitty_surface(frame, app, area, surface_alt());
    if (!awaiting_card_answer || !app.input.is_empty()) && collapsed_paste.is_none() {
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

/// Rows of the completion popup that are visible, and where the selection sits
/// within them. The list scrolls so the highlighted entry is always on screen.
fn completion_window(app: &App, capacity: usize) -> std::ops::Range<usize> {
    let total = app.completion_items.len();
    if total <= capacity {
        return 0..total;
    }
    let start = app
        .selected_completion
        .saturating_add(1)
        .saturating_sub(capacity)
        .min(total.saturating_sub(capacity));
    start..start + capacity
}

/// Geometry shared by the command popup, drawing, and hit testing. Keeping the
/// popup inside the composer rail prevents it from spilling into the wide
/// operations drawer, while its top and bottom edges preserve the persistent
/// header and editor respectively.
fn completion_popup_rect(app: &App, composer: Rect, content_top: u16) -> Option<Rect> {
    if !app.completion_open() || composer.width < 6 {
        return None;
    }
    let available = composer.y.saturating_sub(content_top);
    if available < 3 {
        return None;
    }
    let capacity = usize::from(available.saturating_sub(3).clamp(1, 10));
    let height = (completion_window(app, capacity).len() as u16 + 3).min(available);
    let width = composer.width.saturating_sub(4).min(72);
    if width < 12 {
        return None;
    }
    Some(Rect {
        x: composer.x + composer.width.saturating_sub(width) / 2,
        y: composer.y.saturating_sub(height).max(content_top),
        width,
        height,
    })
}

fn draw_completion_popup(frame: &mut Frame<'_>, app: &App, composer: Rect, content_top: u16) {
    let Some(area) = completion_popup_rect(app, composer, content_top) else {
        return;
    };
    // Leave a row for the footer hint, and never grow past the content space
    // above the composer so the popup cannot cover the header or editor.
    let available = composer.y.saturating_sub(content_top);
    let capacity = usize::from(available.saturating_sub(3).clamp(1, 10));
    let window = completion_window(app, capacity);
    clear_to_canvas(frame, area);
    let inner_width = usize::from(area.width.saturating_sub(2).max(1));
    let name_width = (inner_width / 3).clamp(9, 28);
    let mut lines = app.completion_items[window.clone()]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            let selected = window.start + offset == app.selected_completion;
            let name_style = if entry.unavailable.is_some() {
                Style::default().fg(MUTED)
            } else if selected {
                Style::default().fg(BRIGHT).bold()
            } else {
                Style::default().fg(TEXT)
            };
            let mut tag = entry
                .unavailable
                .map(|reason| (format!(" · {reason}"), Style::default().fg(WARN)));
            if tag.is_none() && entry.network {
                tag = Some((" · network".into(), Style::default().fg(MUTED)));
            }
            let prefix_width = 3_usize;
            let separator_width = 2_usize;
            let mut description_width = inner_width.saturating_sub(
                prefix_width
                    .saturating_add(name_width)
                    .saturating_add(separator_width)
                    .saturating_add(
                        tag.as_ref()
                            .map_or(0, |(text, _)| UnicodeWidthStr::width(text.as_str())),
                    ),
            );
            // On the smallest usable popup, an availability reason is less
            // useful than a legible command description. Drop the secondary
            // tag before letting either field spill past the border.
            if description_width < 6 && tag.is_some() {
                tag = None;
                description_width = inner_width.saturating_sub(
                    prefix_width
                        .saturating_add(name_width)
                        .saturating_add(separator_width),
                );
            }
            let name = truncate_display(&entry.display, name_width);
            let name_padding = " ".repeat(name_width.saturating_sub(UnicodeWidthStr::width(name.as_str())));
            let description = (!entry.description.is_empty() && description_width > 0)
                .then(|| truncate_display(&entry.description, description_width));
            let mut spans = vec![
                Span::styled(if selected { " › " } else { "   " }, Style::default().fg(ACTIVE)),
                Span::styled(format!("{name}{name_padding}"), name_style),
            ];
            if let Some(description) = description {
                spans.push(Span::styled("  ", Style::default().fg(MUTED)));
                spans.push(Span::styled(description, Style::default().fg(MUTED)));
            }
            if let Some((tag, style)) = tag {
                spans.push(Span::styled(tag, style));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    let selected_category = app
        .completion_items
        .get(app.selected_completion)
        .and_then(|entry| entry.category)
        .map(Category::label)
        .unwrap_or("paths");
    let footer = if inner_width < 48 {
        format!(
            "   {selected_category} · {}/{} · ↑↓ · Enter",
            app.selected_completion + 1,
            app.completion_items.len()
        )
    } else if inner_width < 64 {
        format!(
            "   {selected_category} · {}/{} · ↑↓ choose · Enter run · Esc",
            app.selected_completion + 1,
            app.completion_items.len()
        )
    } else {
        format!(
            "   {selected_category} · {}/{} · ↑↓ choose · Tab complete · Enter run · Esc close",
            app.selected_completion + 1,
            app.completion_items.len()
        )
    };
    lines.push(Line::styled(footer, Style::default().fg(MUTED)));
    frame.render_widget(
        Paragraph::new(lines).block(kitty_surface_block(
            app,
            Line::from(" commands "),
            ACTIVE,
            surface(),
        )),
        area,
    );
    finish_kitty_surface(frame, app, area, surface());
}

fn composer_height(app: &App, composer_width: u16) -> u16 {
    if app
        .paste_summary
        .as_ref()
        .is_some_and(|summary| !summary.expanded)
    {
        return 4;
    }
    // This must use the same full rail width as `draw_composer`. Capping it
    // at the compact 124-column measure reserved blank editor rows on wide
    // terminals because layout wrapped more eagerly than the real widget.
    let inner_width = usize::from(composer_width.saturating_sub(2).max(1));
    let lines = EditorLayout::new(&app.input, app.input_cursor, inner_width)
        .lines
        .len();
    (lines as u16 + 2).clamp(4, 8)
}

fn draw_live_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if paused_run(app) {
        let (label, detail) = match app.termination_reason {
            Some(TerminationReason::BudgetTarget) => (
                "Session budget paused",
                session_budget_summary(app)
                    .unwrap_or_else(|| "5% recovery reserve held; completed evidence is preserved".into()),
            ),
            Some(TerminationReason::ProviderReserve) => (
                "Provider reserve paused",
                "Account quota reserve is held; /usage shows the current window".into(),
            ),
            _ => (
                "Usage paused",
                "This run is paused safely; completed evidence is preserved".into(),
            ),
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled("  ! ", Style::default().fg(WARN).bold()),
                    Span::styled(label, Style::default().fg(BRIGHT).bold()),
                    Span::styled(" · reserve held", Style::default().fg(WARN)),
                ]),
                Line::styled(format!("    {detail}"), Style::default().fg(TEXT)),
            ])
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

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
    // A full-width footer closes the composition and keeps the status hints
    // from floating in the black canvas beneath the composer/sidebar split.
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
    let model = identity_model_label(Some("Mina"), &app.model);
    let pause_summary = paused_footer_summary(app);
    let left = if let Some(summary) = &pause_summary {
        if area.width >= 100 {
            format!(" {summary}")
        } else {
            format!(" {}", summary.split(" · ").next().unwrap_or(summary))
        }
    } else if area.width >= 100 {
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
    let right = if paused_run(app) {
        " /usage reserve details · completed evidence preserved ".into()
    } else if app.running {
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

/// The runtime records the configured session target on dispatch receipts.
/// When it stops new turns at the fixed 95% admission boundary, render that
/// distinction from confirmed usage instead of calling the state generically
/// "usage paused".
fn session_budget_summary(app: &App) -> Option<String> {
    if app.termination_reason != Some(TerminationReason::BudgetTarget) {
        return None;
    }
    let target =
        app.dispatch_receipts.iter().rev().find_map(|receipt| {
            (receipt.session_target_tokens > 0).then_some(receipt.session_target_tokens)
        })?;
    let confirmed = app.input_tokens.saturating_add(app.output_tokens);
    let admission_cap = target.saturating_mul(95) / 100;
    Some(format!(
        "session budget paused · {} confirmed / {} admission cap ({} configured target)",
        format_tokens(confirmed),
        format_tokens(admission_cap),
        format_tokens(target),
    ))
}

fn paused_footer_summary(app: &App) -> Option<String> {
    match app.termination_reason {
        Some(TerminationReason::BudgetTarget) => session_budget_summary(app)
            .or_else(|| Some("session budget paused · 5% recovery reserve held".into())),
        Some(TerminationReason::ProviderReserve) => {
            Some("provider reserve paused · account quota reserve held".into())
        }
        _ if app.state == ExitState::UsagePaused => Some("usage paused · reserve held".into()),
        _ => None,
    }
}

fn draw_toast(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(toast) = app.toast.as_ref().filter(|toast| !toast.is_expired()) else {
        return;
    };
    let Some(rect) = toast_rect(app, area) else { return };
    let tone = match toast.tone {
        SystemTone::Info => ("notice", ACTIVE),
        SystemTone::Success => ("saved", GOOD),
        SystemTone::Warning => ("warning", WARN),
        SystemTone::Error => ("error", BAD),
    };
    clear_to_canvas(frame, rect);
    frame.render_widget(
        Paragraph::new(toast.text.clone())
            .block(kitty_surface_block(
                app,
                Line::styled(format!(" {} ", tone.0), Style::default().fg(tone.1).bold()),
                tone.1,
                surface(),
            ))
            .style(Style::default().fg(TEXT))
            .wrap(Wrap { trim: true }),
        rect,
    );
    finish_kitty_surface(frame, app, rect, surface());
}

/// Place transient notices in spare conversation space, never on the active
/// composer, an answer-required card, or an open completion list.  A toast is
/// useful only while it preserves the controls needed to act on it.
fn toast_rect(app: &App, area: Rect) -> Option<Rect> {
    let toast = app.toast.as_ref().filter(|toast| !toast.is_expired())?;
    if area.width < MIN_FULL_WIDTH || area.height < MIN_FULL_HEIGHT {
        return None;
    }
    let rows = vertical_rows(app, area);
    let rail = conversation_rail(rows[1]);
    let composer = conversation_rail(rows[5]);
    let bottom = completion_popup_rect(app, composer, rows[0].bottom())
        .map(|popup| popup.y)
        .unwrap_or_else(|| if rows[4].height > 0 { rows[4].y } else { composer.y });
    let available = bottom.saturating_sub(rows[1].y);
    if rail.width < 6 || available < 3 {
        return None;
    }
    let width = rail.width.saturating_sub(4).min(76);
    let content_width = usize::from(width.saturating_sub(2).max(1));
    let content_lines = toast
        .text
        .lines()
        .flat_map(|line| wrap_display(line, content_width))
        .count()
        .max(1);
    let height = (content_lines as u16 + 2).clamp(3, 5).min(available);
    Some(Rect {
        x: rail.x + rail.width.saturating_sub(width) / 2,
        y: bottom.saturating_sub(height),
        width,
        height,
    })
}

/// Wrap a static help paragraph before it reaches `Paragraph`.  Help's
/// scroll range is expressed in logical rows, so leaving word wrapping to the
/// widget would make the bottom of a narrow drawer unreachable.
fn help_text_lines(text: impl Into<String>, style: Style, width: usize) -> Vec<Line<'static>> {
    styled_wrap(
        vec![StyledChunk::new(text.into(), style)],
        width.max(1),
        Vec::new(),
        Vec::new(),
    )
}

/// A help row that stays readable in the fixed-width operations drawer.
/// Wide modals retain a compact two-column treatment; narrow panels stack the
/// description under its binding instead of letting a long label run directly
/// into the first word of its explanation.
fn help_entry_lines(
    label: &str,
    label_style: Style,
    detail: Vec<StyledChunk>,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let label_width = UnicodeWidthStr::width(label);
    if width >= 58 && label_width.saturating_add(16) <= width {
        let column_width = label_width.max(18);
        let first_prefix = vec![
            Span::raw("  "),
            Span::styled(
                format!("{label}{}", " ".repeat(column_width.saturating_sub(label_width))),
                label_style,
            ),
            Span::raw("  "),
        ];
        let continuation = vec![Span::raw(" ".repeat(column_width + 4))];
        return styled_wrap(detail, width, first_prefix, continuation);
    }

    let mut lines = styled_wrap(
        vec![StyledChunk::new(label.to_owned(), label_style)],
        width,
        vec![Span::raw("  ")],
        vec![Span::raw("    ")],
    );
    lines.extend(styled_wrap(
        detail,
        width,
        vec![Span::raw("    ")],
        vec![Span::raw("    ")],
    ));
    lines
}

/// One row per bound editor action, read out of the resolved keymap so help can
/// never describe a binding the editor does not actually have.
fn keymap_lines(width: u16) -> Vec<Line<'static>> {
    keymap::describe()
        .into_iter()
        .flat_map(|(keys, description)| {
            help_entry_lines(
                &keys,
                Style::default().fg(BRIGHT),
                vec![StyledChunk::new(description, Style::default().fg(MUTED))],
                usize::from(width),
            )
        })
        .collect()
}

/// Help is derived entirely from the command registry and the resolved keymap,
/// so a command can never be dispatchable while being missing from help.
fn draw_help(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    draw_scrollable_modal(
        frame,
        app,
        rect,
        " help ",
        help_lines(app, rect.width.saturating_sub(2)),
    );
}

fn help_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let context = app.command_context();
    let mut lines = vec![Line::styled("Keyboard", Style::default().fg(BRIGHT).bold())];
    lines.extend(keymap_lines(width as u16));
    lines.push(Line::raw(""));
    lines.extend(help_text_lines(
        "Commands · type / in the composer or press Ctrl-P to search them",
        Style::default().fg(BRIGHT).bold(),
        width,
    ));
    for category in Category::ALL {
        let specs = commands::by_category(*category);
        if specs.is_empty() {
            continue;
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            category.label().to_owned(),
            Style::default().fg(ACTIVE),
        ));
        for spec in specs {
            let unavailable = spec.availability(context).reason();
            let style = if unavailable.is_some() {
                Style::default().fg(MUTED)
            } else {
                Style::default().fg(TEXT)
            };
            let mut detail = vec![StyledChunk::new(spec.description, Style::default().fg(MUTED))];
            if let Some(reason) = unavailable {
                detail.push(StyledChunk::new(
                    format!("  · {reason}"),
                    Style::default().fg(WARN),
                ));
            }
            let display = spec.display();
            lines.extend(help_entry_lines(&display, style, detail, width));
        }
    }
    lines.push(Line::raw(""));
    lines.extend(help_text_lines(
        "! command runs locally without a model call; shell operators are rejected.",
        Style::default().fg(MUTED),
        width,
    ));
    lines
}

fn draw_help_drawer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = help_lines(app, area.width.saturating_sub(2));
    let visible = area.height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    app.overlay_scroll_max.set(max_scroll);
    let offset = app.overlay_scroll.min(max_scroll);
    let mut title = drawer_tabs(DrawerTab::Help);
    if max_scroll > 0 {
        title.spans.push(Span::styled(
            format!(" · {}/{}", offset + 1, max_scroll + 1),
            Style::default().fg(MUTED),
        ));
    }
    let block = drawer_block(app, title);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}

/// Preferred size of each overlay, shared by drawing and layout so the widget
/// and its hit targets can never disagree about how big a modal is.
fn overlay_size(overlay: &Overlay) -> (u16, u16) {
    match overlay {
        Overlay::Status => (104, 32),
        Overlay::Context => (96, 24),
        Overlay::Books => (112, 26),
        Overlay::Doctor => (84, 22),
        Overlay::Recovery { .. } => (84, 18),
        Overlay::Help => (104, 40),
        Overlay::Keymap => (86, 32),
        _ => (70, 18),
    }
}

fn draw_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(overlay) = &app.overlay else { return };
    // Each draw publishes the current overlay's actual scroll range. Without
    // clearing this first, a short modal could retain Help's old range and
    // accept invisible navigation after the overlay changes.
    app.overlay_scroll_max.set(0);
    let (max_width, max_height) = overlay_size(overlay);
    let rect = modal_rect(
        app,
        area,
        max_width.min(area.width.saturating_sub(4)),
        max_height.min(area.height.saturating_sub(2)),
    );
    clear_to_canvas(frame, rect);
    match overlay {
        Overlay::Help => draw_help(frame, app, rect),
        Overlay::Keymap => {
            let mut lines = vec![Line::styled(
                format!("Resolved keymap · {} preset", keymap::active_preset().label()),
                Style::default().fg(BRIGHT).bold(),
            )];
            lines.push(Line::raw(""));
            lines.extend(keymap_lines(rect.width.saturating_sub(2)));
            draw_scrollable_modal(frame, app, rect, " keymap ", lines);
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
                                format!(
                                    "  {} · {}",
                                    crate::app::state_label(run.state),
                                    identity_model_label(
                                        Some("Mina"),
                                        run.model.as_deref().unwrap_or("model unavailable")
                                    )
                                ),
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
        Overlay::Recovery { title, detail } => {
            draw_modal(
                frame,
                app,
                rect,
                " recovery needed ",
                vec![
                    Line::styled(title.clone(), Style::default().fg(BAD).bold()),
                    Line::raw(""),
                    Line::styled(detail.clone(), Style::default().fg(TEXT)),
                    Line::raw(""),
                    Line::styled(
                        "/retry tries the saved session · /resume replays it · /doctor checks the local runtime",
                        Style::default().fg(MUTED),
                    ),
                    Line::styled(
                        "Esc closes this view without discarding the session.",
                        Style::default().fg(MUTED),
                    ),
                ],
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
                    Line::styled(format!("  {user_code}  "), Style::default().fg(BRIGHT).bold()),
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

/// Maps a click to an option index within whichever decision card is
/// currently on screen (clarification, mid-run question, or exec approval) —
/// the same fixed row `draw_decision_card` renders into, rather than a
/// centered modal rect.
pub(crate) fn decision_card_option_at(
    app: &App,
    column: u16,
    row: u16,
    terminal_width: u16,
    terminal_height: u16,
) -> Option<usize> {
    if (!app.has_active_clarification() && app.pending_request.is_none())
        || terminal_width < MIN_FULL_WIDTH
        || terminal_height < MIN_FULL_HEIGHT
    {
        return None;
    }
    let terminal = Rect::new(0, 0, terminal_width, terminal_height);
    let area = conversation_rail(vertical_rows(app, terminal)[4]);
    if area.height == 0 {
        return None;
    }
    let inner = area.inner(Margin::new(1, 1));
    if column < inner.x || column >= inner.right() || row < inner.y || row >= inner.bottom() {
        return None;
    }
    let content = active_card_content(app, area.width.saturating_sub(2))?;
    let line = usize::from(row.saturating_sub(inner.y)).saturating_add(decision_card_scroll(
        &content,
        app.selected_clarification_option,
        usize::from(inner.height),
    ));
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
    if terminal_width < MIN_FULL_WIDTH || terminal_height < MIN_FULL_HEIGHT {
        return None;
    }
    let terminal = Rect::new(0, 0, terminal_width, terminal_height);
    let rows = vertical_rows(app, terminal);
    let inner = conversation_rail(rows[5]).inner(Margin::new(1, 1));
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
    let status_lines = status_column_lines(app);
    let health_lines = health_column_lines(app);
    let outer = kitty_surface_block(
        app,
        Line::styled(
            " status · inspector · ↑/↓ scroll ",
            Style::default().fg(BRIGHT).bold(),
        ),
        ACTIVE,
        surface(),
    );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);
    if inner.width < 60 {
        let mut lines = status_lines;
        lines.push(Line::raw(""));
        lines.extend(health_lines);
        draw_scrolled_inspector_lines(frame, app, inner, lines);
        finish_kitty_surface(frame, app, rect, surface());
        return;
    }
    let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    let health_block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(border()));
    let status_lines = wrap_styled_lines(status_lines, usize::from(columns[0].width.max(1)));
    let health_lines = wrap_styled_lines(
        health_lines,
        usize::from(health_block.inner(columns[1]).width.max(1)),
    );
    let visible = usize::from(columns[0].height.max(1));
    let max_scroll = status_lines
        .len()
        .max(health_lines.len())
        .saturating_sub(visible)
        .min(usize::from(u16::MAX)) as u16;
    app.overlay_scroll_max.set(max_scroll);
    let offset = app.overlay_scroll.min(max_scroll);
    frame.render_widget(Paragraph::new(status_lines).scroll((offset, 0)), columns[0]);
    frame.render_widget(
        Paragraph::new(health_lines)
            .block(health_block)
            .scroll((offset, 0)),
        columns[1],
    );
    finish_kitty_surface(frame, app, rect, surface());
}

fn draw_scrolled_inspector_lines(frame: &mut Frame<'_>, app: &App, area: Rect, lines: Vec<Line<'static>>) {
    let lines = wrap_styled_lines(lines, usize::from(area.width.max(1)));
    let visible = usize::from(area.height.max(1));
    let max_scroll = lines.len().saturating_sub(visible).min(usize::from(u16::MAX)) as u16;
    app.overlay_scroll_max.set(max_scroll);
    frame.render_widget(
        Paragraph::new(lines).scroll((app.overlay_scroll.min(max_scroll), 0)),
        area,
    );
}

fn status_column_lines(app: &App) -> Vec<Line<'static>> {
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
    let projected_mimo = app.mimo_estimated_usd
        + app
            .contexts
            .values()
            .filter_map(|context| {
                minha_core::mimo::estimate_cost_usd(
                    &context.model,
                    context.forecast_tokens,
                    0,
                    context.output_allowance,
                )
            })
            .sum::<f64>();
    vec![
        Line::styled("session", Style::default().fg(ACTIVE).bold()),
        kv_line("run", &run),
        kv_line("state", crate::app::state_label(app.state)),
        kv_line("phase", &format!("{:?}", app.phase)),
        kv_line("model", &identity_model_label(Some("Mina"), &app.model)),
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
            "external prices",
            &format!(
                "DS ${:.6} · MiMo ${:.6}",
                app.deepseek_estimated_usd, app.mimo_estimated_usd
            ),
        ),
        kv_line(
            "next forecast",
            &format!("DS ${projected_deepseek:.6} · MiMo ${projected_mimo:.6}"),
        ),
        kv_line(
            "provider quota",
            &format!(
                "DS {} · MiMo unavailable by API",
                match (&app.deepseek_balance, app.deepseek_reserve_percent) {
                    (Some(balance), Some(percent)) => format!("{balance} ({percent:.1}% reserve)"),
                    (Some(balance), None) => balance.clone(),
                    (None, _) => "unavailable".into(),
                }
            ),
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
    ]
}

fn health_column_lines(app: &App) -> Vec<Line<'static>> {
    let problem_count = app.incidents.len();
    vec![
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
        kv_line("quota windows", &account_usage_summary(app)),
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
    ]
}

fn append_account_usage(lines: &mut Vec<Line<'static>>, app: &App) {
    if app.account_usage.is_empty() {
        return;
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("account quota", Style::default().fg(ACTIVE).bold()));
    for snapshot in app.account_usage.iter().take(3) {
        let label = snapshot
            .limit_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                if snapshot.limit_id.trim().is_empty() {
                    "account"
                } else {
                    snapshot.limit_id.as_str()
                }
            });
        let mut windows = Vec::new();
        if let Some(primary) = &snapshot.primary {
            windows.push(format!("primary {}", quota_window_label(primary)));
        }
        if let Some(secondary) = &snapshot.secondary {
            windows.push(format!("secondary {}", quota_window_label(secondary)));
        }
        if let Some(credits) = &snapshot.credits {
            if credits.unlimited {
                windows.push("credits unlimited".into());
            } else if let Some(balance) = &credits.balance {
                windows.push(format!("credits {balance}"));
            } else if credits.has_credits {
                windows.push("credits available".into());
            }
        }
        if !windows.is_empty() {
            lines.push(kv_line(label, &windows.join(" · ")));
        }
    }
}

fn account_usage_summary(app: &App) -> String {
    app.account_usage
        .first()
        .and_then(|snapshot| snapshot.primary.as_ref())
        .map_or_else(
            || "unavailable".into(),
            |primary| format!("primary {}", quota_window_label(primary)),
        )
}

fn quota_window_label(window: &minha_core::usage::RateLimitWindow) -> String {
    let mut value = format!("{:.0}%", window.used_percent);
    if let Some(minutes) = window.window_minutes {
        value.push_str(&format!(" / {minutes}m"));
    }
    if window.resets_at.is_some() {
        value.push_str(" reset scheduled");
    }
    value
}

fn draw_context_dashboard(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let outer = kitty_surface_block(
        app,
        Line::styled(" context window ", Style::default().fg(BRIGHT).bold()),
        ACTIVE,
        surface(),
    );
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
            .gauge_style(Style::default().fg(if ratio >= 0.9 {
                BAD
            } else if ratio >= 0.72 {
                WARN
            } else {
                ACTIVE
            })),
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
                identity_model_label(Some(&role), &context.model),
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
    let lines = wrap_styled_lines(lines, usize::from(rows[1].width.max(1)));
    let visible = rows[1].height;
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    app.overlay_scroll_max.set(max_scroll);
    let offset = app.overlay_scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT))
            .scroll((offset, 0)),
        rows[1],
    );
    finish_kitty_surface(frame, app, rect, surface());
}

fn draw_books(frame: &mut Frame<'_>, app: &App, rect: Rect) {
    let outer = kitty_surface_block(
        app,
        Line::styled(
            " books · bundled technical library ",
            Style::default().fg(BRIGHT).bold(),
        ),
        ACTIVE,
        surface(),
    );
    let inner = outer.inner(rect);
    frame.render_widget(outer, rect);
    if app.library.is_empty() {
        frame.render_widget(
            Paragraph::new("Bundled manifest could not be loaded.").style(Style::default().fg(WARN)),
            inner,
        );
        finish_kitty_surface(frame, app, rect, surface());
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
            .block(surface_block(app, Line::raw(" entries "), ACTIVE, surface()))
            .highlight_style(Style::default().fg(ACTIVE).bold())
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
            .block(surface_block(app, Line::raw(" entry "), ACTIVE, surface()))
            .wrap(Wrap { trim: true }),
        columns[1],
    );
    finish_kitty_surface(frame, app, rect, surface());
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
    draw_scrollable_modal(frame, app, rect, " doctor · local runtime checks ", lines);
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
            .block(kitty_surface_block(
                app,
                Line::styled(title.to_owned(), Style::default().fg(BRIGHT)),
                border(),
                surface(),
            ))
            .wrap(Wrap { trim: true }),
        rect,
    );
    finish_kitty_surface(frame, app, rect, surface());
}

/// A modal whose content can outgrow the frame, scrolled by `app.overlay_scroll`.
///
/// Publishes the largest useful offset back to the app so Up/Down/PageDown stop
/// at the end of the text instead of scrolling into empty space.
fn draw_scrollable_modal(
    frame: &mut Frame<'_>,
    app: &App,
    rect: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
) {
    let inner = rect.inner(Margin::new(1, 1));
    let lines = wrap_styled_lines(lines, usize::from(inner.width.max(1)));
    let visible = inner.height;
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    app.overlay_scroll_max.set(max_scroll);
    let offset = app.overlay_scroll.min(max_scroll);
    let mut title = title.to_owned();
    if max_scroll > 0 {
        title = format!("{title}· {}/{} ", offset + 1, max_scroll + 1);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(kitty_surface_block(
                app,
                Line::styled(title, Style::default().fg(BRIGHT)),
                border(),
                surface(),
            ))
            .scroll((offset, 0)),
        rect,
    );
    finish_kitty_surface(frame, app, rect, surface());
}

fn draw_tiny(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let input = app.input.lines().next().unwrap_or_default();
    let preview_width = usize::from(area.width.saturating_sub(2).max(1));
    let preview = truncate_display(input, preview_width);
    let decision = tiny_decision_preview(app, preview_width);
    let title = decision
        .as_ref()
        .map(|(heading, _)| heading.as_str())
        .unwrap_or(app.status.as_str());
    let mut lines = vec![
        Line::styled(format!("minha · {title}"), Style::default().fg(BRIGHT)),
        Line::styled(format!("> {preview}"), Style::default().fg(ACTIVE)),
    ];
    if let Some((heading, choice)) = decision {
        // Tiny terminals cannot fit the complete card, but they must never
        // hide an approval or question while keyboard handling still requires
        // an answer. Keep the selected action and its controls on screen.
        if area.height >= 6 {
            lines.push(Line::styled(heading, Style::default().fg(WARN).bold()));
            lines.push(Line::styled(choice, Style::default().fg(BRIGHT)));
            lines.push(Line::styled(
                "↑/↓ choose · Enter answer",
                Style::default().fg(MUTED),
            ));
        } else if area.height >= 3 {
            lines.push(Line::styled(
                format!("{heading} · resize to answer"),
                Style::default().fg(WARN),
            ));
        }
    }
    if lines.len() < usize::from(area.height) {
        lines.push(Line::styled("terminal too small", Style::default().fg(MUTED)));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
    // A resize into the compact fallback used to leave the hardware cursor at
    // its previous full-layout position. Keep it on the visible one-line
    // editor instead, even when the original draft spans several lines.
    if area.width > 0 && area.height > 1 {
        let input_width = UnicodeWidthStr::width(preview.as_str()) as u16;
        frame.set_cursor_position((
            area.x
                .saturating_add(2)
                .saturating_add(input_width)
                .min(area.right().saturating_sub(1)),
            area.y.saturating_add(1),
        ));
    }
}

/// Return a compact, actionable summary for the fallback renderer.  Reuse the
/// same card content used by the normal layout so its selected option cannot
/// drift from the answer submitted by the keyboard path.
fn tiny_decision_preview(app: &App, width: usize) -> Option<(String, String)> {
    let content = active_card_content(app, width.min(u16::MAX as usize) as u16)?;
    let (_, _, selected) = content
        .option_lines
        .iter()
        .find(|(_, _, option)| *option == app.selected_clarification_option)?;
    let (start, _, _) = content
        .option_lines
        .iter()
        .find(|(_, _, option)| *option == *selected)
        .copied()?;
    let choice = content.lines.get(start)?;
    let choice = choice
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let heading = if app
        .pending_request
        .as_ref()
        .is_some_and(|request| request.approval)
        && !app.has_active_clarification()
    {
        "approval required"
    } else {
        "answer required"
    };
    Some((heading.into(), truncate_display(choice.trim(), width)))
}

fn short_role(role: &str) -> String {
    role.replace("gpt-", "").chars().take(36).collect()
}

/// Resolve an agent id to the same role label every other panel shows, falling
/// back to a short id when the agent is not in the roster.
fn agent_label(agents: &[AgentView], agent_id: &str) -> String {
    agents
        .iter()
        .find(|agent| agent.id.to_string() == agent_id)
        .map_or_else(|| short_id(agent_id), |agent| short_role(&agent.role))
}

/// Resolve a hive wire address (`agent:{id}`, `group:all`, `leader`) to a
/// human label. Coordination rows previously printed these verbatim, so a
/// reader saw `agent:018f3a2e-…` where every other panel shows a role.
fn address_label(agents: &[AgentView], address: &str) -> String {
    if let Some(id) = address.strip_prefix("agent:") {
        return agent_label(agents, id);
    }
    match address {
        "group:all" => "everyone".to_owned(),
        "leader" | "manager" => address.to_owned(),
        other => other.strip_prefix("group:").unwrap_or(other).to_owned(),
    }
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
    use minha_core::protocol::{DispatchReceiptV1, EventAgentId, EventEnvelope, ItemId, RunId, RuntimeEvent};
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
                role: "Mina, integrating".into(),
                model: "gpt-5.6-mina".into(),
                parent: None,
            },
        ));
        app.apply_event(&EventEnvelope::new(
            run,
            3,
            RuntimeEvent::AssistantMessage {
                agent_id: agent,
                item_id: ItemId::new(),
                role: "Mina, integrating".into(),
                model: "gpt-5.6-mina".into(),
                text: "I am inspecting the real runtime state.".into(),
            },
        ));
        app
    }

    fn render_buffer(width: u16, height: u16, app: &App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test operation should succeed");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("test operation should succeed");
        terminal.backend().buffer().clone()
    }

    fn render(width: u16, height: u16, app: &App) -> String {
        let buffer = render_buffer(width, height, app);
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
            assert!(screen.contains("minha"), "{screen}");
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
            assert!(screen.contains("minha"), "{screen}");
            assert_eq!(screen.lines().count(), usize::from(height));
        }
    }

    #[test]
    fn sub_ten_row_terminal_uses_the_compact_fallback_before_drawers_overlap_controls() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.drawer_visible = true;
        app.drawer_override_narrow = Some(true);
        app.last_terminal_width = 36;
        let screen = render(36, 8, &app);
        assert!(screen.contains("terminal too small"), "{screen}");
        assert!(
            !screen.contains("operations"),
            "the compact fallback must not draw a partial drawer over the composer: {screen}"
        );
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
        app.scroll_state.follow();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("first draw");
        {
            let cache = app.transcript_layout.borrow();
            assert_eq!(cache.builds, 1);
            assert!(cache.lines.len() > 2_000);
            let transcript_height = vertical_rows(&app, Rect::new(0, 0, 80, 24))[1].height;
            assert!(cache.last_viewport_lines <= usize::from(transcript_height));
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
        let inspected_scroll = app
            .scroll_state
            .offset_for(app.transcript_layout.borrow().max_scroll);
        assert!(!app.scroll_state.auto_follow);
        app.push_system(SystemTone::Success, "finished");
        terminal.draw(|frame| draw(frame, &app)).expect("updated draw");
        assert_eq!(app.transcript_layout.borrow().builds, 2);
        assert_eq!(
            app.scroll_state
                .offset_for(app.transcript_layout.borrow().max_scroll),
            inspected_scroll,
            "new activity must not snap an inspecting user to bottom"
        );

        let bottom = app.transcript_layout.borrow().max_scroll;
        app.scroll_state.set_manual(bottom.saturating_sub(1));
        app.update(crate::app::AppAction::ScrollDown)
            .expect("return to bottom");
        assert!(app.scroll_state.auto_follow);
        assert_eq!(app.scroll_state.offset_for(bottom), bottom);
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
    fn composer_height_uses_the_full_wide_reading_rail() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.input = "x".repeat(700);
        app.input_cursor = app.input.len();
        let inner_width = 156usize - 2;
        let expected = (EditorLayout::new(&app.input, app.input_cursor, inner_width)
            .lines
            .len() as u16
            + 2)
        .clamp(4, 8);
        assert_eq!(composer_height(&app, 156), expected);
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
    fn vim_mode_stays_visible_in_a_compact_composer_title() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.tui_settings.vim_scroll = true;
        app.vim_mode = crate::app::VimMode::Normal;

        let screen = render(36, 12, &app);
        assert!(screen.contains("VIM NORMAL"), "{screen}");
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
    fn no_color_theme_keeps_flat_control_frames_blank() {
        let mut app = populated_app();
        app.theme = "no_color".into();
        let area = Rect::new(2, 2, 24, 6);
        let backend = TestBackend::new(32, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(surface_block(&app, Line::raw("control"), ACTIVE, surface()), area);
                apply_theme(frame, &app);
            })
            .expect("draw");
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let frame_glyphs = [
            '┌', '┐', '└', '┘', '│', '─', '╭', '╮', '╰', '╯', '▘', '▝', '▖', '▗', '▀', '▄', '▌', '▐',
        ];
        assert!(
            !screen.chars().any(|glyph| frame_glyphs.contains(&glyph)),
            "NO_COLOR must not expose hidden surface frames:\n{screen}"
        );
    }

    #[test]
    fn no_color_runtime_override_wins_over_saved_preferences_and_kitty_surfaces() {
        let mut app = populated_app();
        let settings = crate::settings::TuiSettingsV1::default();
        app.apply_tui_settings(settings, true);
        app.active_surface_renderer = "kitty".into();
        assert_eq!(app.effective_theme(), "no_color");
        assert!(raster_surfaces(&app, Rect::new(0, 0, 80, 24)).is_empty());

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
    fn imported_opaline_theme_changes_the_live_tui_palette() {
        let mut app = populated_app();
        let imported = crate::settings::ImportedThemeV1 {
            name: "Test palette".into(),
            source: "test.toml".into(),
            toml: r##"[meta]
name = "Test palette"
variant = "dark"

[tokens]
background = "#101820"
surface = "#172b3a"
surface_alt = "#203a4d"
border = "#4b718f"
text = "#e5eef5"
bright = "#ffffff"
muted = "#aabbcc"
active = "#55ddff"
good = "#66dd99"
warn = "#ffcc66"
bad = "#ff6688"
"##
            .into(),
        };
        let mut settings = crate::settings::TuiSettingsV1::default();
        settings
            .set_imported_theme(imported)
            .expect("valid imported theme");
        app.apply_tui_settings(settings, false);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        assert_eq!(
            terminal.backend().buffer()[(0, 10)].bg,
            Color::Rgb(16, 24, 32),
            "the imported Opaline canvas must drive the actual frame"
        );
    }

    #[test]
    fn flat_controls_keep_the_canvas_continuous_across_renderers() {
        let mut app = populated_app();
        app.theme = "dark".into();
        app.active_surface_renderer = "quadrant".into();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 10)].bg, background(), "canvas uses Minha navy");
        assert_eq!(buffer[(0, 19)].symbol(), "▗");
        assert_eq!(buffer[(0, 19)].bg, background(), "corner stays on the canvas");
        assert_eq!(
            buffer[(0, 19)].fg,
            background(),
            "corner frame is visually transparent"
        );
        assert_eq!(
            buffer[(1, 20)].bg,
            background(),
            "composer interior must not become a filled card"
        );
    }

    #[test]
    fn tiny_layout_keeps_the_cursor_on_the_visible_composer_preview() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.input = "hi".into();
        app.input_cursor = app.input.len();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &app)).expect("draw");
        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(4, 1)
        );
    }

    #[test]
    fn assistant_markdown_is_formatted_and_prewrapped_to_display_width() {
        let item = TranscriptItem::Assistant {
            item_id: ItemId::new(),
            agent_id: EventAgentId::new(),
            role: "Mina, integrating".into(),
            text: "# Result\n\nA soft-wrapped paragraph\nstays one paragraph.\n\n---\n\n- [x] **Bold** and ~~old~~ words\n- ordinary item\n\n| Path | State |\n| --- | --- |\n| src/lib.rs | good |\n\n```rust\nlet wide = \"界界界界\";\n```"
                .into(),
            streaming: false,
        };
        let lines = item_lines(&item, 24, &[], false);
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
        assert!(rendered.contains('─'));
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains("~~"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn wrapping_preserves_indic_emoji_and_tabs_at_grapheme_boundaries() {
        let text = "नमस्ते\t👩\u{200d}💻\t界";
        let lines = wrap_display(text, 80);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 80)
        );
        assert_eq!(lines.concat(), text);

        let layout = EditorLayout::new(text, "नमस्ते\t👩\u{200d}💻".len(), 5);
        let byte = layout.byte_at_column(text, layout.cursor_row, layout.cursor_column);
        assert_eq!(byte, "नमस्ते\t👩\u{200d}💻".len());
    }

    #[test]
    fn streaming_control_payload_is_hidden_before_its_closing_tag_arrives() {
        let lines = assistant_lines(
            "planner lead",
            None,
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
        assert!(screen.contains("Start with a request."));
        assert!(!screen.contains("scout"));
        assert!(!screen.contains("builder"));
    }

    #[test]
    fn empty_state_has_a_centered_composition_and_compact_fallback() {
        let app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let area = Rect::new(0, 0, 80, 24);
        let transcript = conversation_rail(vertical_rows(&app, area)[1]);
        let welcome = welcome_rect(transcript);
        let composer = conversation_rail(vertical_rows(&app, area)[5]);
        assert_eq!(
            welcome.x + welcome.width / 2,
            composer.x + composer.width / 2,
            "the welcome and composer must share one center axis"
        );
        assert!(
            welcome.y > transcript.y,
            "welcome must not stick to transcript top"
        );
        assert!(
            welcome.bottom() < composer.y,
            "welcome needs breathing room above the composer"
        );
        assert!(render(80, 24, &app).contains("PLAN /plan"));

        let compact = render(40, 20, &app);
        assert!(compact.contains("Try /plan, /audit, or /review."), "{compact}");
        assert!(compact.contains("Ctrl-P commands"), "{compact}");
    }

    #[test]
    fn wide_terminal_centers_the_reading_rail_without_an_operations_panel() {
        let app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let area = Rect::new(0, 0, 200, 30);
        let welcome_rect = welcome_rect(conversation_rail(vertical_rows(&app, area)[1]));
        let composer_rect = conversation_rail(vertical_rows(&app, area)[5]);
        assert_eq!(
            welcome_rect.x + welcome_rect.width / 2,
            composer_rect.x + composer_rect.width / 2,
            "the welcome card must center within the reading rail, not inherit its left edge"
        );
        let screen = render(area.width, area.height, &app);
        let welcome = screen
            .lines()
            .find(|line| line.contains("Start with a request."))
            .expect("welcome line");
        assert!(welcome.contains("Start with a request."));
        let composer = screen
            .lines()
            .find(|line| line.contains("message · Enter sends"))
            .expect("composer title");
        assert!(composer.starts_with(&" ".repeat(usize::from(composer_rect.x))));
    }

    #[test]
    fn wide_idle_layout_keeps_the_blank_canvas_outside_real_surfaces() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.sync_drawer_visibility(200);
        let area = Rect::new(0, 0, 200, 52);
        let rows = vertical_rows(&app, area);
        let welcome = welcome_rect(conversation_rail(rows[1]));
        let composer = conversation_rail(rows[5]);
        assert!(!app.drawer_visible, "wide sessions begin transcript-first");
        assert!(
            conversation_rail(rows[1]).x >= 20,
            "the idle rail should be centered rather than pinned to the left"
        );
        assert_eq!(
            welcome.x + welcome.width / 2,
            composer.x + composer.width / 2,
            "a wide idle screen must not strand the welcome card against the rail's left edge"
        );
        assert_eq!(
            welcome.y,
            rows[1].y + rows[1].height.saturating_sub(welcome.height) / 2,
            "the sparse idle state must remain vertically centered in its transcript row"
        );
        let buffer = render_buffer(area.width, area.height, &app);
        assert_eq!(
            buffer[(1, rows[1].y + 1)].bg,
            background(),
            "empty body canvas must not become a giant surface-colored panel"
        );
    }

    #[test]
    fn wide_paused_run_keeps_the_sidebar_body_and_reserve_evidence_visible() {
        let mut app = populated_app();
        let agent = app.agents[0].clone();
        app.drawer_visible = true;
        app.running = false;
        app.state = ExitState::UsagePaused;
        app.termination_reason = Some(TerminationReason::BudgetTarget);
        app.status = "session budget paused — 5% recovery reserve held".into();
        app.input_tokens = 90_000;
        app.output_tokens = 4_200;
        app.dispatch_receipts.push(DispatchReceiptV1 {
            schema_version: 1,
            receipt_id: "receipt-budget".into(),
            task_id: "tui-visual-pass".into(),
            generation: 1,
            agent_id: agent.id,
            role: agent.role,
            provider: "chatgpt".into(),
            model: "gpt-5.6-mina".into(),
            candidates: Vec::new(),
            lease_resources: Vec::new(),
            acceptance_check: "cargo test -p minha-tui".into(),
            estimated_input_tokens: 0,
            session_used_tokens: 94_200,
            session_target_tokens: 100_000,
            budget_pressure: "paused".into(),
            parallelism_reason: "reserve test".into(),
            book_sources: Vec::new(),
            issued_at: chrono::Utc::now(),
        });

        let area = Rect::new(0, 0, 200, 52);
        let rows = vertical_rows(&app, area);
        let drawer = drawer_rect(&app, area).expect("wide drawer");
        let composer = conversation_rail(rows[5]);
        assert_eq!(drawer.y, rows[1].y);
        assert!(
            drawer.bottom() <= rows[5].y,
            "the sidebar must not turn the composer-height body into empty panel chrome"
        );
        assert!(
            composer.right() <= drawer.x,
            "composer must not run under the sidebar"
        );

        let screen = render(200, 52, &app);
        assert!(screen.contains("Session budget paused"), "{screen}");
        assert!(screen.contains("94.2k confirmed / 95.0k admission cap (100.0k configured target)"));
        assert!(screen.contains("session budget paused"), "{screen}");
        assert!(screen.contains("operations"), "{screen}");

        for (width, height) in [(120, 30), (80, 24)] {
            let normal = render(width, height, &app);
            assert_eq!(normal.lines().count(), usize::from(height), "{width}x{height}");
            assert!(normal.contains("budget paused"), "{width}x{height}: {normal}");
        }
    }

    #[test]
    fn closed_activity_groups_are_one_readable_evidence_line() {
        let agent_id = EventAgentId::new();
        let entries = [
            TranscriptItem::Tool {
                agent_id,
                call_id: "read-one".into(),
                name: "read_files".into(),
                arguments: r#"{"path":"src/app.rs"}"#.into(),
                output: "done".into(),
                exit_code: Some(0),
                running: false,
                expanded: false,
            },
            TranscriptItem::Tool {
                agent_id,
                call_id: "read-two".into(),
                name: "read_files".into(),
                arguments: r#"{"path":"src/ui.rs"}"#.into(),
                output: "done".into(),
                exit_code: Some(0),
                running: false,
                expanded: false,
            },
        ];
        let refs = entries.iter().collect::<Vec<_>>();
        let lines = activity_group_lines(&refs, 80);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(lines.len(), 1);
        assert!(text.contains("2 items"));
        assert!(!text.contains("Ctrl-O"));
    }

    #[test]
    fn mixed_completed_tool_burst_is_one_compact_activity_receipt() {
        let agent_id = EventAgentId::new();
        let entries = [
            TranscriptItem::Tool {
                agent_id,
                call_id: "search".into(),
                name: "search".into(),
                arguments: r#"{"query":"TODO"}"#.into(),
                output: "done".into(),
                exit_code: Some(0),
                running: false,
                expanded: false,
            },
            TranscriptItem::Tool {
                agent_id,
                call_id: "check".into(),
                name: "quality".into(),
                arguments: r#"{"argv":["cargo","test"]}"#.into(),
                output: "done".into(),
                exit_code: Some(0),
                running: false,
                expanded: false,
            },
        ];
        let refs = entries.iter().collect::<Vec<_>>();
        let lines = activity_group_lines(&refs, 100);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(lines.len(), 1);
        assert!(text.contains("Activity"));
        assert!(text.contains("Searched × 1"));
        assert!(text.contains("Ran checks × 1"));
    }

    /// A run with two named agents, so identity resolution has something to
    /// resolve against.
    fn coordinated_app() -> (App, RunId, EventAgentId, EventAgentId) {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let run = RunId::new();
        let lead = EventAgentId::new();
        let worker = EventAgentId::new();
        app.apply_event(&EventEnvelope::new(
            run,
            0,
            RuntimeEvent::SessionStarted {
                kind: "implement".into(),
                goal: "improve the hive".into(),
            },
        ));
        for (sequence, (agent_id, role)) in [(lead, "Mina session lead"), (worker, "spark worker")]
            .into_iter()
            .enumerate()
        {
            app.apply_event(&EventEnvelope::new(
                run,
                sequence as u64 + 1,
                RuntimeEvent::AgentStarted {
                    agent_id,
                    role: role.into(),
                    model: "deepseek/deepseek-v4-flash".into(),
                    parent: None,
                },
            ));
        }
        (app, run, lead, worker)
    }

    #[test]
    fn coordination_rows_resolve_addresses_to_roles_not_raw_uuids() {
        let (mut app, run, lead, worker) = coordinated_app();
        app.apply_event(&EventEnvelope::new(
            run,
            10,
            RuntimeEvent::OfficeMessageChanged {
                message_id: "message-1".into(),
                room_id: "run".into(),
                sender: format!("agent:{lead}"),
                recipient: format!("agent:{worker}"),
                kind: "handoff".into(),
                summary: "the parser clamp is yours now".into(),
                deduplicated: false,
            },
        ));
        let screen = render(120, 30, &app);
        assert!(
            screen.contains("Mina session lead → spark worker"),
            "addresses should resolve like every other panel: {screen}"
        );
        assert!(
            !screen.contains(&short_id(&worker.to_string())),
            "no raw uuid fragment should survive: {screen}"
        );
        assert!(
            !screen.contains("agent:"),
            "no wire address should reach the human: {screen}"
        );
        // The run room is implicit, so it is not worth a column.
        assert!(screen.contains("Handed off"), "{screen}");
    }

    #[test]
    fn coordination_rows_label_each_kind_and_name_private_rooms() {
        let (mut app, run, lead, worker) = coordinated_app();
        for (sequence, (kind, label)) in [
            ("finding", "Found"),
            ("decision", "Decided"),
            ("request", "Asked"),
            ("blocker", "Blocked"),
            ("artifact_reference", "Shared"),
        ]
        .into_iter()
        .enumerate()
        {
            app.apply_event(&EventEnvelope::new(
                run,
                sequence as u64 + 10,
                RuntimeEvent::OfficeMessageChanged {
                    message_id: format!("message-{sequence}"),
                    room_id: "lead-worker".into(),
                    sender: format!("agent:{worker}"),
                    recipient: format!("agent:{lead}"),
                    kind: kind.into(),
                    summary: format!("a {kind} to report"),
                    deduplicated: false,
                },
            ));
            let screen = render(120, 40, &app);
            assert!(
                screen.contains(label),
                "{kind} should render as {label}: {screen}"
            );
        }
        let screen = render(120, 40, &app);
        assert!(
            screen.contains("room lead-worker"),
            "a private room is named, unlike the implicit run room: {screen}"
        );
    }

    #[test]
    fn board_entries_show_their_resolved_author() {
        let (mut app, run, _lead, worker) = coordinated_app();
        app.apply_event(&EventEnvelope::new(
            run,
            10,
            RuntimeEvent::BoardChanged {
                entry: minha_core::protocol::BoardEntryView {
                    id: "entry-1".into(),
                    scope: "session".into(),
                    kind: "finding".into(),
                    subject: "Parser location".into(),
                    body: "The parser lives in src/parser.rs".into(),
                    task_id: Some("task-a".into()),
                    author_agent_id: Some(worker),
                    confidence: 90,
                    status: "open".into(),
                },
            },
        ));
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Board;
        let screen = render(120, 28, &app);
        assert!(screen.contains("Parser location"), "{screen}");
        assert!(
            screen.contains("spark worker"),
            "the author is written on every post and should be shown: {screen}"
        );
    }

    #[test]
    fn drawer_title_keeps_the_active_tab_visible_at_normal_widths() {
        let mut app = populated_app();
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Problems;
        let screen = render(120, 28, &app);
        assert!(screen.contains("problems"));
        assert!(screen.contains("4/8"));

        app.drawer_tab = DrawerTab::Usage;
        let narrow = render(70, 28, &app);
        assert!(
            narrow.contains("usage"),
            "active tab must never clip away: {narrow}"
        );
        assert!(narrow.contains("6/8"), "drawer still signals tab count: {narrow}");
    }

    #[test]
    fn route_tab_shows_the_last_routing_decision() {
        let mut app = populated_app();
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Route;
        app.last_routing = Some(crate::app::RouteView {
            mode: "implement".into(),
            reason: "bounded no-tool classifier".into(),
            provider: "chatgpt_codex".into(),
            model: Some("gpt-5.6-terra".into()),
        });
        let screen = render(120, 28, &app);
        assert!(screen.contains("implement"));
        assert!(screen.contains("bounded no-tool classifier"));
        assert!(screen.contains("gpt-5.6-terra"));
    }

    #[test]
    fn usage_tab_shows_session_tokens_and_labeled_balance() {
        let mut app = populated_app();
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Usage;
        app.input_tokens = 4_200;
        app.balance_provider = "deepseek".into();
        app.deepseek_balance = Some("USD 12.50".into());
        app.deepseek_reserve_percent = Some(64.0);
        let screen = render(120, 28, &app);
        assert!(screen.contains("deepseek"));
        assert!(screen.contains("12.50"));
    }

    #[test]
    fn settings_are_anchored_and_cover_the_operational_sections() {
        let mut app = populated_app();
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Settings;
        app.reduced_motion = true;
        let screen = render(120, 32, &app);
        for section in [
            "appearance",
            "layout",
            "input & keybindings",
            "accessibility",
            "providers",
            "usage",
            "advanced",
        ] {
            assert!(screen.contains(section), "missing {section}: {screen}");
        }
        assert!(screen.contains("reduced"));
    }

    #[test]
    fn focused_agent_uses_a_dispatch_backed_specialist_card() {
        let mut app = populated_app();
        let agent = app.agents[0].clone();
        app.focused_agent = Some(agent.id);
        app.dispatch_receipts.push(DispatchReceiptV1 {
            schema_version: 1,
            receipt_id: "receipt-1".into(),
            task_id: "parser-contract".into(),
            generation: 1,
            agent_id: agent.id,
            role: agent.role,
            provider: "deepseek".into(),
            model: "deepseek/deepseek-v4-flash".into(),
            candidates: Vec::new(),
            lease_resources: vec!["path:src/parser.rs".into()],
            acceptance_check: "cargo test -p parser".into(),
            estimated_input_tokens: 1_200,
            session_used_tokens: 2_000,
            session_target_tokens: 100_000,
            budget_pressure: "normal".into(),
            parallelism_reason: "one-agent default".into(),
            book_sources: vec![
                "zeta@1.0.0#appendix".into(),
                "alpha@1.0.0#syntax".into(),
                "zeta@1.0.0#appendix".into(),
            ],
            issued_at: chrono::Utc::now(),
        });
        let screen = render(120, 32, &app);
        assert!(screen.contains("specialist card"), "{screen}");
        assert!(screen.contains("parser-contract"), "{screen}");
        assert!(
            screen.contains("alpha@1.0.0#syntax, zeta@1.0.0#appendix"),
            "the specialist identity must sort and deduplicate book sources: {screen}"
        );
        assert!(screen.contains("Mina · deepseek/deepseek-v4-flash"), "{screen}");
        assert!(screen.contains("src/parser.rs"), "{screen}");
        assert!(screen.contains("cargo test -p parser"), "{screen}");
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
    fn scrollable_inspectors_keep_wrapped_tail_reachable() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.overlay = Some(Overlay::Doctor);
        app.diagnostics.push(Diagnostic {
            label: "terminal".into(),
            ok: true,
            detail: format!("{}final-tail", "detail ".repeat(80)),
        });

        let initial = render(40, 12, &app);
        let max_scroll = app.overlay_scroll_max.get();
        assert!(
            max_scroll > 0,
            "the inspector must count physical wrapped rows: {initial}"
        );
        assert!(
            !initial.contains("final-tail"),
            "the tail should start below the compact viewport: {initial}"
        );

        app.overlay_scroll = max_scroll;
        let final_view = render(40, 12, &app);
        assert!(
            final_view.contains("final-tail"),
            "the final physical row must be reachable by scrolling: {final_view}"
        );
    }

    #[test]
    fn inspector_overlays_keep_the_flat_canvas_contract() {
        for overlay in [Overlay::Status, Overlay::Context, Overlay::Books] {
            let mut app = populated_app();
            app.active_surface_renderer = "quadrant".into();
            app.overlay = Some(overlay.clone());
            let backend = TestBackend::new(120, 40);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal.draw(|frame| draw(frame, &app)).expect("draw");

            let (width, height) = overlay_size(&overlay);
            let rect = modal_rect(&app, Rect::new(0, 0, 120, 40), width, height);
            let corner = &terminal.backend().buffer()[(rect.x, rect.y)];
            assert_eq!(corner.symbol(), "▗", "{overlay:?} preserves control geometry");
            assert_eq!(corner.fg, background(), "{overlay:?} must not draw a box corner");
            assert_eq!(corner.bg, background(), "{overlay:?} stays on the canvas");
        }
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
            if let Some(option) = decision_card_option_at(&app, 60, row, 120, 40)
                && !hits.contains(&option)
            {
                hits.push(option);
            }
        }
        assert_eq!(hits, (0..expected).collect::<Vec<_>>());
        assert_eq!(decision_card_option_at(&app, 0, 0, 120, 40), None);
    }

    #[test]
    fn pending_request_mouse_hits_follow_rendered_option_rows() {
        let app = pending_approval_app();

        let mut hits = Vec::new();
        for row in 0..40 {
            if let Some(option) = decision_card_option_at(&app, 60, row, 120, 40)
                && !hits.contains(&option)
            {
                hits.push(option);
            }
        }
        assert_eq!(
            hits,
            vec![0, 1],
            "both approve and decline rows must be clickable"
        );
        assert_eq!(decision_card_option_at(&app, 0, 0, 120, 40), None);
    }

    #[test]
    fn exec_approval_card_renders_inline_with_effect_and_evidence() {
        let app = pending_approval_app();

        let screen = render(120, 32, &app);
        assert!(screen.contains("approval"));
        assert!(screen.contains("Approve this risky action?"));
        assert!(screen.contains("rm -rf build/"));
        assert!(screen.contains("matches an allowlisted destructive pattern"));
        assert!(screen.contains("Approve"));
        assert!(screen.contains("Decline"));
        // The card sits in its own reserved row above the composer, not
        // centered over the transcript: both must be visible in the same
        // frame, in that order.
        let card_row = screen
            .lines()
            .position(|line| line.contains("Approve this risky action?"))
            .expect("card question must render");
        // Distinct from the card's own footer hint ("...or type a custom
        // answer"), so this can only match the composer's border title.
        let composer_row = screen
            .lines()
            .position(|line| line.contains("typing writes a custom answer"))
            .expect("composer hint must render");
        assert!(card_row < composer_row, "card must render above the composer");
    }

    #[test]
    fn commandless_approval_card_does_not_claim_a_command_that_does_not_exist() {
        // Regression: the integration-approval gate (runtime.rs) emits an
        // Approval request with command: None and a multi-paragraph `reason`
        // (task list + check summary + recovery note, joined by '\n'). The
        // card used to unconditionally say "Only the command above runs" even
        // with no command shown, and collapsed the paragraphs into one
        // word-wrapped run because labeled_field didn't split on '\n' first.
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.active_run = Some(RunId::new());
        app.pending_request = Some(crate::app::PendingRequest {
            id: minha_core::protocol::RequestId::new(),
            question: "Approve integrating this work?".into(),
            options: vec!["yes".into(), "no".into()],
            approval: true,
            reason: Some("Tasks:\nsrc/slug.rs\nsrc/stats.rs\n\nChecks: 2 passed, 0 failed".into()),
            command: None,
        });

        let screen = render(120, 32, &app);
        assert!(screen.contains("Approve integrating this work?"));
        assert!(!screen.contains("Only the command above runs"));
        assert!(!screen.contains("Run the command above"));
        assert!(screen.contains("src/slug.rs"));
        assert!(screen.contains("src/stats.rs"));
        // Each source paragraph must land on its own screen row, not run
        // together as one wrapped block.
        let slug_row = screen
            .lines()
            .position(|line| line.contains("src/slug.rs"))
            .expect("first task line must render");
        let stats_row = screen
            .lines()
            .position(|line| line.contains("src/stats.rs"))
            .expect("second task line must render");
        assert!(
            stats_row > slug_row,
            "each '\\n'-separated source line must occupy its own rendered row"
        );
    }

    fn pending_approval_app() -> App {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.active_run = Some(RunId::new());
        app.pending_request = Some(crate::app::PendingRequest {
            id: minha_core::protocol::RequestId::new(),
            question: "Approve this risky action?".into(),
            options: vec!["yes".into(), "no".into()],
            approval: true,
            reason: Some("matches an allowlisted destructive pattern".into()),
            command: Some(vec!["rm".into(), "-rf".into(), "build/".into()]),
        });
        app
    }

    #[test]
    fn question_height_seam_is_zero_when_idle_and_sized_when_a_card_is_active() {
        let idle = App::new(PathBuf::from("/workspace/minha"), 128_000);
        let area = Rect::new(0, 0, 120, 40);
        assert_eq!(
            vertical_rows(&idle, area)[4].height,
            0,
            "the reserved seam must collapse when nothing needs an answer"
        );

        let with_request = pending_approval_app();
        let height = vertical_rows(&with_request, area)[4].height;
        assert!(height > 0, "an active request must claim real height");

        // A decision card keeps its cell geometry and receives the matching
        // Kitty corner placement when that renderer is active.
        let mut kitty = pending_approval_app();
        kitty.active_surface_renderer = "kitty".into();
        let card = conversation_rail(vertical_rows(&kitty, area)[4]);
        assert!(
            raster_surfaces(&kitty, area)
                .iter()
                .any(|raster| raster.rect == card && raster.fill == surface_fill_rgb(&kitty, surface())),
            "the decision card must retain its Kitty rounded backing"
        );
    }

    #[test]
    fn long_inline_cards_do_not_displace_the_compact_composer() {
        let mut app = pending_approval_app();
        app.pending_request.as_mut().expect("approval request").reason = Some("evidence ".repeat(400));
        for (width, height) in [(36, 8), (40, 12), (80, 12), (80, 24)] {
            let area = Rect::new(0, 0, width, height);
            let rows = vertical_rows(&app, area);
            assert!(
                rows[5].height >= 4,
                "{width}x{height}: a long decision card must not consume the composer: {rows:?}"
            );
            assert!(
                rows[5].bottom() <= area.bottom(),
                "{width}x{height}: composer must remain on screen: {rows:?}"
            );
        }

        let compact = render(40, 12, &app);
        assert!(
            compact.contains("Approve"),
            "a capped card must scroll to the selected answer: {compact}"
        );
        let visible_options = (0..12)
            .filter_map(|row| decision_card_option_at(&app, 20, row, 40, 12))
            .collect::<Vec<_>>();
        assert!(
            visible_options.contains(&0),
            "mouse hit-testing must use the same capped-card scroll offset"
        );

        app.selected_clarification_option = 1;
        let compact = render(40, 12, &app);
        assert!(
            compact.contains("Decline"),
            "moving the selection must scroll the next answer into view: {compact}"
        );
        let visible_options = (0..12)
            .filter_map(|row| decision_card_option_at(&app, 20, row, 40, 12))
            .collect::<Vec<_>>();
        assert!(visible_options.contains(&1));
    }

    #[test]
    fn tiny_layout_keeps_pending_approval_actionable() {
        let app = pending_approval_app();
        let screen = render(36, 8, &app);
        assert!(
            screen.contains("approval required"),
            "the fallback must disclose its blocked approval state: {screen}"
        );
        assert!(
            screen.contains("Approve"),
            "the currently selected action must remain visible: {screen}"
        );
        assert!(
            screen.contains("Enter answer"),
            "the fallback must explain how to respond: {screen}"
        );
    }

    #[test]
    fn transient_popups_stay_out_of_active_controls_with_matching_kitty_backdrops() {
        let area = Rect::new(0, 0, 120, 32);
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.active_surface_renderer = "kitty".into();
        app.update(crate::app::AppAction::Input('/'))
            .expect("slash opens command completion");
        assert!(app.completion_open());
        let composer = conversation_rail(vertical_rows(&app, area)[5]);
        let popup = completion_popup_rect(&app, composer, vertical_rows(&app, area)[0].bottom())
            .expect("completion popup rect");
        assert!(
            popup.bottom() <= composer.y,
            "popup must not cover the composer: {popup:?}"
        );
        assert!(
            raster_surfaces(&app, area)
                .iter()
                .any(|raster| raster.rect == popup && raster.fill == surface_fill_rgb(&app, surface())),
            "the completion popup must retain its Kitty rounded backing"
        );

        app.close_completion();
        app.push_system(
            SystemTone::Warning,
            "A notification must remain readable without blocking the editor.",
        );
        let toast = toast_rect(&app, area).expect("toast has safe transcript space");
        assert!(
            toast.bottom() <= composer.y,
            "toast must not cover composer: {toast:?}"
        );
        assert!(
            raster_surfaces(&app, area)
                .iter()
                .any(|raster| raster.rect == toast && raster.fill == surface_fill_rgb(&app, surface())),
            "the toast must retain its Kitty rounded backing"
        );

        let mut approval = pending_approval_app();
        approval.active_surface_renderer = "kitty".into();
        approval.push_system(SystemTone::Error, "Action needs attention.");
        let card = conversation_rail(vertical_rows(&approval, area)[4]);
        let toast = toast_rect(&approval, area).expect("toast has space above the card");
        assert!(
            toast.bottom() <= card.y,
            "toast must not cover approval card: {toast:?}"
        );
    }

    #[test]
    fn completion_popup_preserves_the_header_on_compact_terminals() {
        for (width, height) in [(40, 12), (60, 14), (80, 24)] {
            let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
            app.update(crate::app::AppAction::Input('/'))
                .expect("slash opens completion");
            let area = Rect::new(0, 0, width, height);
            let rows = vertical_rows(&app, area);
            let composer = conversation_rail(rows[5]);
            let popup = completion_popup_rect(&app, composer, rows[0].bottom())
                .expect("completion popup must fit the compact layout");
            assert!(
                popup.y >= rows[0].bottom(),
                "{width}x{height}: popup {popup:?} must stay below header {:?}",
                rows[0]
            );
            let screen = render(width, height, &app);
            assert!(
                screen
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains("/workspace/minha")),
                "{width}x{height}: completion must not overwrite the header: {screen}"
            );
            if width == 40 {
                let command_line = screen
                    .lines()
                    .find(|line| line.contains("/help"))
                    .expect("the compact popup must show its selected command");
                assert!(
                    command_line.contains('…'),
                    "compact descriptions must truncate visibly instead of clipping: {command_line}"
                );
                assert!(
                    screen.contains("↑↓ · Enter"),
                    "the compact footer must fit its usable controls: {screen}"
                );
            }
        }
    }

    #[test]
    fn narrow_drawer_yields_to_completion_with_ordered_kitty_layers() {
        let area = Rect::new(0, 0, 80, 24);
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.active_surface_renderer = "kitty".into();
        app.drawer_visible = true;
        app.drawer_override_narrow = Some(true);
        app.last_terminal_width = area.width;
        let drawer = drawer_rect(&app, area).expect("narrow drawer before completion");
        app.update(crate::app::AppAction::Input('/'))
            .expect("slash opens completion");
        app.push_system(SystemTone::Warning, "Layering notice remains visible.");

        let composer = conversation_rail(vertical_rows(&app, area)[5]);
        let popup = completion_popup_rect(&app, composer, vertical_rows(&app, area)[0].bottom())
            .expect("completion popup");
        let toast = toast_rect(&app, area).expect("toast");
        assert!(popup.bottom() <= composer.y, "popup stays above the composer");
        assert!(toast.bottom() <= composer.y, "toast stays above the composer");
        assert_eq!(
            drawer_rect(&app, area),
            None,
            "completion temporarily owns the narrow drawer's space"
        );

        let surfaces = raster_surfaces(&app, area);
        assert!(
            surfaces
                .iter()
                .any(|raster| raster.rect == composer && raster.fill == surface_fill_rgb(&app, surface_alt()))
                && surfaces
                    .iter()
                    .any(|raster| raster.rect == popup && raster.fill == surface_fill_rgb(&app, surface()))
                && surfaces
                    .iter()
                    .any(|raster| raster.rect == toast && raster.fill == surface_fill_rgb(&app, surface())),
            "completion and toast must keep their real Kitty surfaces: {surfaces:?}"
        );

        let screen = render(area.width, area.height, &app);
        assert!(
            screen.contains("/help"),
            "completion must remain visible: {screen}"
        );
        assert!(
            screen.contains("Layering notice remains visible."),
            "toast must remain visible: {screen}"
        );
        assert!(
            !screen.contains("operations"),
            "completion must not leave the narrow drawer peeking through: {screen}"
        );
        assert!(
            !screen.contains("Start with a request."),
            "the idle card yields to a focused completion surface: {screen}"
        );

        app.close_completion();
        assert!(
            drawer_rect(&app, area) == Some(drawer),
            "the narrow drawer returns when completion closes"
        );
        assert!(
            raster_surfaces(&app, area)
                .iter()
                .any(|surface| surface.rect == drawer),
            "restoring a drawer must restore its Kitty rounded surface"
        );

        app.overlay = Some(Overlay::Status);
        let (max_width, max_height) = overlay_size(app.overlay.as_ref().expect("status overlay"));
        let overlay = modal_rect(
            &app,
            area,
            max_width.min(area.width.saturating_sub(4)),
            max_height.min(area.height.saturating_sub(2)),
        );
        assert!(
            raster_surfaces(&app, area)
                .iter()
                .any(|surface| surface.rect == overlay),
            "an overlay must retain its Kitty rounded surface"
        );
    }

    #[test]
    fn kitty_mode_emits_primary_surface_backdrops() {
        let area = Rect::new(0, 0, 100, 30);
        let mut focused = App::new(PathBuf::from("/workspace/minha"), 128_000);
        focused.active_surface_renderer = "kitty".into();
        focused.focused_agent = Some(EventAgentId::new());
        let focused_composer = conversation_rail(vertical_rows(&focused, area)[5]);
        assert!(
            raster_surfaces(&focused, area)
                .iter()
                .any(|surface| surface.rect == focused_composer
                    && surface.fill == surface_fill_rgb(&focused, surface_alt()))
        );

        let mut overlay = App::new(PathBuf::from("/workspace/minha"), 128_000);
        overlay.active_surface_renderer = "kitty".into();
        overlay.overlay = Some(Overlay::Status);
        let (max_width, max_height) = overlay_size(overlay.overlay.as_ref().expect("status overlay"));
        let overlay_rect = modal_rect(
            &overlay,
            area,
            max_width.min(area.width.saturating_sub(4)),
            max_height.min(area.height.saturating_sub(2)),
        );
        assert!(
            raster_surfaces(&overlay, area)
                .iter()
                .any(|surface| surface.rect == overlay_rect)
        );

        let mut tasks = App::new(PathBuf::from("/workspace/minha"), 128_000);
        tasks.active_surface_renderer = "kitty".into();
        tasks.plan.push(minha_core::protocol::PlanTask {
            id: "task-1".into(),
            objective: "round the task rail".into(),
            paths: Vec::new(),
            dependencies: Vec::new(),
            state: PlanTaskState::Pending,
            agent_id: None,
        });
        let task_rect = conversation_rail(vertical_rows(&tasks, area)[2]);
        assert!(
            raster_surfaces(&tasks, area)
                .iter()
                .any(|surface| surface.rect == task_rect)
        );
    }

    #[test]
    fn modal_never_covers_the_composer_on_small_terminals() {
        let app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        for height in 10..=24 {
            let area = Rect::new(0, 0, 80, height);
            let modal = modal_rect(&app, area, 72, 18);
            let composer = vertical_rows(&app, area)[5];
            assert!(
                modal.bottom() <= composer.y,
                "80x{height}: modal {modal:?} must sit above the composer at {composer:?}"
            );
            assert!(
                modal.width >= 1 && modal.height >= 1,
                "80x{height}: modal must stay visible"
            );
            let status_row = vertical_rows(&app, area)[6];
            assert!(
                modal.bottom() <= status_row.y,
                "80x{height}: modal {modal:?} must not spill into the status row at {status_row:?}"
            );
            assert!(
                modal.y >= HEADER_HEIGHT,
                "80x{height}: modal {modal:?} must preserve the header"
            );
        }
    }

    #[test]
    fn modals_preserve_the_header_at_normal_viewports() {
        for overlay in [Overlay::Help, Overlay::Status] {
            let (max_width, max_height) = overlay_size(&overlay);
            for (width, height) in [(40, 12), (60, 14), (80, 24)] {
                let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
                app.overlay = Some(overlay.clone());
                let area = Rect::new(0, 0, width, height);
                let modal = modal_rect(
                    &app,
                    area,
                    max_width.min(area.width.saturating_sub(4)),
                    max_height.min(area.height.saturating_sub(2)),
                );
                assert!(
                    modal.y >= HEADER_HEIGHT,
                    "{overlay:?} at {width}x{height}: {modal:?} must start below the header"
                );
                let screen = render(width, height, &app);
                assert!(
                    screen
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("/workspace/minha")),
                    "{overlay:?} at {width}x{height}: the header must stay readable: {screen}"
                );
            }
        }
    }

    #[test]
    fn help_drawer_wraps_bindings_without_colliding_with_their_descriptions() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Help;
        let screen = render(120, 30, &app);
        assert!(
            screen.contains("Cmd-Left"),
            "the binding must remain visible: {screen}"
        );
        assert!(
            screen.contains("Move to start"),
            "the binding description must remain visible: {screen}"
        );
        assert!(
            !screen.contains("Cmd-LeftMove"),
            "long bindings must not run into their description: {screen}"
        );
        assert!(
            app.overlay_scroll_max.get() > 0,
            "the pre-wrapped rows must publish a usable drawer scroll range"
        );
    }

    #[test]
    fn flat_composer_corners_do_not_use_a_surface_fill() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.theme = "imported".into();
        app.active_surface_renderer = "quadrant".into();
        app.theme_palette = crate::settings::ThemePalette {
            background: [1, 2, 3],
            surface: [4, 5, 6],
            surface_alt: [7, 8, 9],
            border: [10, 11, 12],
            text: [13, 14, 15],
            bright: [16, 17, 18],
            muted: [19, 20, 21],
            active: [22, 23, 24],
            good: [25, 26, 27],
            warn: [28, 29, 30],
            bad: [31, 32, 33],
        };
        let area = Rect::new(0, 0, 80, 24);
        let composer = conversation_rail(vertical_rows(&app, area)[5]);
        let buffer = render_buffer(area.width, area.height, &app);
        for (x, y) in [
            (composer.x, composer.y),
            (composer.right() - 1, composer.y),
            (composer.x, composer.bottom() - 1),
            (composer.right() - 1, composer.bottom() - 1),
        ] {
            assert_eq!(buffer[(x, y)].fg, Color::Rgb(1, 2, 3));
            assert_eq!(buffer[(x, y)].bg, Color::Rgb(1, 2, 3));
        }
    }

    #[test]
    fn kitty_composer_cells_match_the_raster_corner_mask_and_imported_palette() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.theme = "imported".into();
        app.active_surface_renderer = "kitty".into();
        app.theme_palette = crate::settings::ThemePalette {
            background: [1, 2, 3],
            surface: [4, 5, 6],
            surface_alt: [7, 8, 9],
            border: [10, 11, 12],
            text: [13, 14, 15],
            bright: [16, 17, 18],
            muted: [19, 20, 21],
            active: [22, 23, 24],
            good: [25, 26, 27],
            warn: [28, 29, 30],
            bad: [31, 32, 33],
        };
        let area = Rect::new(0, 0, 80, 24);
        let composer = conversation_rail(vertical_rows(&app, area)[5]);
        let buffer = render_buffer(area.width, area.height, &app);
        for (x, y) in [
            (composer.x, composer.y),
            (composer.right() - 1, composer.y),
            (composer.x, composer.bottom() - 1),
            (composer.right() - 1, composer.bottom() - 1),
        ] {
            assert_eq!(buffer[(x, y)].fg, Color::Rgb(7, 8, 9));
            assert_eq!(buffer[(x, y)].bg, Color::Rgb(1, 2, 3));
        }
        assert_eq!(buffer[(composer.x + 1, composer.y + 1)].bg, Color::Rgb(7, 8, 9));
        assert!(
            raster_surfaces(&app, area)
                .iter()
                .any(|surface| surface.rect == composer && surface.fill == [7, 8, 9]),
            "Kitty corners and the cell backing must share the imported surface color"
        );
    }

    #[test]
    fn kitty_mode_keeps_overlays_backed_at_every_viewport() {
        // The RGBA corner placements must use the same `modal_rect` geometry
        // as the widget on every usable terminal size.
        for overlay in [
            Overlay::Status,
            Overlay::Context,
            Overlay::Books,
            Overlay::Doctor,
            Overlay::Keymap,
            Overlay::Help,
        ] {
            let (max_width, max_height) = overlay_size(&overlay);
            for (width, height) in [(120u16, 40u16), (80, 24), (200, 60), (60, 14)] {
                let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
                app.active_surface_renderer = "kitty".into();
                app.overlay = Some(overlay.clone());
                let area = Rect::new(0, 0, width, height);
                let rect = modal_rect(
                    &app,
                    area,
                    max_width.min(area.width.saturating_sub(4)),
                    max_height.min(area.height.saturating_sub(2)),
                );
                assert!(
                    rect.width > 0 && rect.height > 0,
                    "{overlay:?} at {width}x{height}: overlay geometry must remain usable"
                );
                assert!(
                    raster_surfaces(&app, area)
                        .iter()
                        .any(|surface| surface.rect == rect),
                    "{overlay:?} at {width}x{height}: overlay must emit a matching Kitty surface"
                );
            }
        }
    }

    #[test]
    fn drawer_hit_testing_matches_the_rendered_drawer() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.drawer_visible = true;
        app.drawer_tab = DrawerTab::Activity;
        for index in 0..2 {
            app.agents.push(AgentView {
                id: EventAgentId::new(),
                role: format!("worker {index}"),
                model: "gpt-5.6-luna".into(),
                state: AgentState::Working,
                detail: String::new(),
            });
        }

        let wide = Rect::new(0, 0, 180, 40);
        let rect = drawer_rect(&app, wide).expect("wide drawer must render");
        assert_eq!(rect.right(), 180, "wide drawer must be right-anchored: {rect:?}");
        assert_eq!(rect.width, 48, "wide drawer keeps its stable reading width");
        assert_eq!(
            drawer_hit(&app, rect.x + 1, rect.y + 1, 180, 40),
            Some(0),
            "the left edge of a wide drawer must be clickable"
        );
        assert_eq!(drawer_hit(&app, rect.x + 1, rect.y + 4, 180, 40), Some(1));
        assert_eq!(
            drawer_hit(&app, rect.x + 1, rect.y, 180, 40),
            None,
            "the drawer title border is not its first list row"
        );
        assert_eq!(
            drawer_hit(&app, rect.x.saturating_sub(1), rect.y + 1, 180, 40),
            None,
            "clicks left of the drawer must never hit a drawer item"
        );
        assert_eq!(drawer_hit(&app, rect.x + 1, rect.bottom(), 180, 40), None);

        let mid = Rect::new(0, 0, 80, 24);
        let rect = drawer_rect(&app, mid).expect("mid drawer must render");
        assert_eq!(rect.width, 49);
        assert_eq!(rect.right(), 80);
        assert_eq!(drawer_hit(&app, rect.x + 1, rect.y + 1, 80, 24), Some(0));
        assert_eq!(
            drawer_hit(&app, rect.x.saturating_sub(1), rect.y + 1, 80, 24),
            None
        );

        let narrow = Rect::new(0, 0, 60, 20);
        let rect = drawer_rect(&app, narrow).expect("narrow drawer must render");
        assert_eq!(rect.x, 1, "narrow drawer keeps the left margin");
        assert!(drawer_hit(&app, rect.x + 1, rect.y + 1, 60, 20).is_some());
        assert_eq!(drawer_hit(&app, 0, rect.y + 1, 60, 20), None);

        app.drawer_visible = false;
        assert_eq!(drawer_rect(&app, Rect::new(0, 0, 180, 40)), None);
        assert_eq!(drawer_hit(&app, 60, 10, 180, 40), None);
    }

    #[test]
    fn short_wide_drawers_are_compact_and_use_real_rounded_corners() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.active_surface_renderer = "quadrant".into();
        app.sync_drawer_visibility(200);
        app.set_drawer_visible(true);
        app.agents.push(AgentView {
            id: EventAgentId::new(),
            role: "conversation lead".into(),
            model: "gpt-5.6-luna".into(),
            state: AgentState::Completed,
            detail: "completed in one turn".into(),
        });

        let area = Rect::new(0, 0, 200, 52);
        let rows = vertical_rows(&app, area);
        let drawer = drawer_rect(&app, area).expect("explicit activity drawer");
        assert_eq!(
            drawer.height, 5,
            "one activity item needs only its three rows and border"
        );
        assert!(
            drawer.bottom() <= rows[5].y,
            "drawer must stop above the composer"
        );

        let buffer = render_buffer(area.width, area.height, &app);
        assert_eq!(
            buffer[(drawer.x, drawer.y)].symbol(),
            "▗",
            "drawer corners must match the rounded surface contract before finish_surface adjusts them"
        );
    }

    #[test]
    fn drawer_hits_follow_the_rendered_list_scroll_and_skip_empty_space() {
        let mut app = App::new(PathBuf::from("/workspace/minha"), 128_000);
        app.drawer_visible = true;
        app.drawer_override_narrow = Some(true);
        app.last_terminal_width = 80;
        app.drawer_tab = DrawerTab::Activity;
        for index in 0..6 {
            app.agents.push(AgentView {
                id: EventAgentId::new(),
                role: format!("worker {index}"),
                model: "gpt-5.6-luna".into(),
                state: AgentState::Working,
                detail: String::new(),
            });
        }
        app.selected_agent = 4;
        let area = Rect::new(0, 0, 80, 16);
        let rect = drawer_rect(&app, area).expect("narrow drawer");
        assert_eq!(drawer_list_offset(&app, rect, app.selected_agent), 3);
        assert_eq!(
            drawer_hit(&app, rect.x + 1, rect.y + 1, area.width, area.height),
            Some(3),
            "the top visible row must map to the top rendered item, not item zero"
        );

        app.agents.truncate(1);
        app.selected_agent = 0;
        assert_eq!(
            drawer_hit(&app, rect.x + 1, rect.y + 7, area.width, area.height),
            None,
            "blank rows below a short list are not selectable"
        );
    }
}
