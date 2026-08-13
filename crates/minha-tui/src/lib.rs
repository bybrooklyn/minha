//! Transcript-first Ratatui interface for Minha.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod app;
mod commands;
mod editor;
mod keymap;
mod kitty;
mod settings;
mod ui;

pub use app::{App, AppAction};

use anyhow::{Context, Result};
use app::{Diagnostic, Submission, SystemTone, VimMode, identity_model_label};
use chrono::Utc;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use minha_core::auth::{
    CodexOAuthClient, active_account_profile, list_account_profiles, openai_oauth_config, save_default_auth,
};
use minha_core::executor::{ToolExecutor, ToolOutcome};
use minha_core::facts::{BoardEntry, BoardKind, BoardStatus};
use minha_core::instructions::discover_skills;
use minha_core::office::{
    CoordinationKind, OFFICE_ENVELOPE_SCHEMA_VERSION, OfficeEnvelopeV1, RUN_ROOM_ID, Recipient,
};
use minha_core::protocol::ExitState;
use minha_core::protocol::{EventAgentId, RuntimeEvent};
use minha_core::provider_credentials::{
    default_path as provider_credentials_path, load_deepseek_key, load_xiaomi_mimo,
};
use minha_core::store::SCHEMA_VERSION;
use minha_core::worktree::GitRepo;
use minha_core::{Harness, RunOutcome};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use std::sync::Once;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

enum RuntimeMessage {
    Finished(Box<Result<RunOutcome, String>>),
    LocalFinished {
        display: String,
        result: Result<(String, Option<i32>), String>,
        persisted: bool,
    },
    LoginStarted {
        verification_uri: String,
        user_code: String,
    },
    LoginFinished(Result<(), String>),
}

/// Build the only new office write the TUI is allowed to make. Keeping this
/// conversion at the UI boundary prevents an arbitrary JSON payload from
/// reaching the store and makes the compact envelope limit explicit before a
/// runtime event is emitted.
fn direct_office_request_envelope(
    message_id: String,
    recipient: &str,
    summary: String,
) -> Result<OfficeEnvelopeV1> {
    let recipient = Recipient::parse(recipient)
        .with_context(|| format!("invalid direct-message recipient `{recipient}`"))?;
    let envelope = OfficeEnvelopeV1 {
        schema_version: OFFICE_ENVELOPE_SCHEMA_VERSION,
        id: message_id,
        room_id: RUN_ROOM_ID.to_owned(),
        sender: "user".into(),
        recipient,
        kind: CoordinationKind::Request,
        task_id: None,
        summary,
        artifact_refs: Vec::new(),
        evidence: Vec::new(),
        requested_action: Some("respond_to_user".into()),
        sent_at: Utc::now(),
    };
    envelope
        .validate()
        .context("direct message does not satisfy the office envelope contract")?;
    Ok(envelope)
}

static TERMINAL_PANIC_RECOVERY: Once = Once::new();

/// A panic must never strand the user in raw mode or leave kitty/mouse/paste
/// capture active.  The normal `Drop` path covers ordinary errors; this hook
/// is the last-resort crash view and deliberately chains the previous hook so
/// diagnostics remain available to bug reports.
fn install_terminal_panic_recovery() {
    TERMINAL_PANIC_RECOVERY.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut stdout = io::stdout();
            let _ = disable_raw_mode();
            let _ = restore_after_start_failure(&mut stdout);
            eprintln!(
                "\nMinha recovered your terminal after an unexpected crash. \
                 Your persisted session can be reopened with /resume; include the panic details below when reporting it."
            );
            previous(info);
        }));
    });
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn start(mouse_enabled: bool) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Bracketed paste must be enabled for Event::Paste to fire; without
        // it, pasted newlines submit partial messages and tabs trigger
        // completion. The Enter* calls are best-effort so every failure path
        // still restores the terminal.
        if let Err(error) = enter_terminal_state(&mut stdout, mouse_enabled) {
            let _ = restore_after_start_failure(&mut stdout);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                let _ = restore_after_start_failure(&mut stdout);
                Err(error.into())
            }
        }
    }
}

/// Enter the alternate screen and bracketed paste; mouse capture joins only
/// when requested. Split from `start` so the exact escape sequences are
/// regression-tested without a terminal.
fn enter_terminal_state<W: io::Write>(writer: &mut W, mouse_enabled: bool) -> io::Result<()> {
    if mouse_enabled {
        execute!(
            writer,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
    } else {
        execute!(writer, EnterAlternateScreen, EnableBracketedPaste)
    }
}

/// Best-effort restoration shared by every `start` failure path: mouse
/// capture, bracketed paste, and the alternate screen must all be undone
/// even when only some of them were enabled.
fn restore_after_start_failure<W: io::Write>(writer: &mut W) -> io::Result<()> {
    execute!(
        writer,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

/// Outcome of checking the single run slot before a run-switching submission.
#[derive(Debug, Eq, PartialEq)]
enum RunGate {
    Open,
    Busy(&'static str),
}

/// Start, Continue, Resume, and Fork must wait until the live run task
/// drains: a second launch would let the old run's streaming events leak
/// into the replayed session, and a silently dropped message looks like a
/// lost keystroke.
fn run_gate(active_task: bool, busy_warning: &'static str) -> RunGate {
    if active_task {
        RunGate::Busy(busy_warning)
    } else {
        RunGate::Open
    }
}

/// Drain the next deferred answer, but only while no run task is active.
/// Answers queue in FIFO order so a second question can never overwrite the
/// first answer before its task even starts.
fn pop_deferred_answer(
    active_task: bool,
    deferred: &mut std::collections::VecDeque<(minha_core::RunId, String)>,
) -> Option<(minha_core::RunId, String)> {
    if active_task {
        return None;
    }
    deferred.pop_front()
}

/// Where a typed answer must go: a pending request needs its answer even
/// when the run is also usage-paused; only pure resumes take the
/// pause-only path.
fn answer_path(paused: bool, has_pending_request: bool) -> AnswerPath {
    if paused && !has_pending_request {
        AnswerPath::ResumePaused
    } else {
        AnswerPath::ResumeWithAnswer
    }
}

#[derive(Debug, Eq, PartialEq)]
enum AnswerPath {
    ResumePaused,
    ResumeWithAnswer,
}

/// Run the responsive control room against the in-process runtime actor.
pub async fn run(harness: Harness) -> Result<()> {
    install_terminal_panic_recovery();
    let mut session = TerminalSession::start(harness.config.tui.mouse)?;
    let mut app = App::new(
        harness.root().to_owned(),
        harness.config.context.context_limit.unwrap_or(272_000) as u64,
    );
    app.details_expanded = harness.config.tui.tool_detail.eq_ignore_ascii_case("expanded");
    let fallback_settings = settings::TuiSettingsV1::with_legacy_defaults(
        harness.config.tui.theme.clone(),
        harness.config.tui.surface_renderer.clone(),
        harness.config.tui.reduced_motion,
    );
    let (local_settings, settings_notice) = settings::load_user_settings(fallback_settings);
    app.apply_tui_settings(local_settings, std::env::var_os("NO_COLOR").is_some());
    if let Some(notice) = settings_notice {
        app.push_system(SystemTone::Warning, notice);
    }
    let _ = app.take_surface_renderer_reload();
    let mut surface_renderer =
        kitty::SurfaceRenderer::new(&app.surface_renderer, app.effective_theme(), app.canvas_rgb());
    app.set_active_surface_renderer(surface_renderer.active_name());
    app.sync_drawer_visibility(session.terminal.size()?.width);
    app.set_sessions(harness.store.list_runs(100)?);
    let usage = harness.store.usage_totals(None)?;
    app.set_usage_totals(usage);
    let cache = harness.store.cache_totals(harness.workspace_id())?;
    app.cache_entries = cache.entries;
    app.cache_bytes = cache.bytes;
    app.cache_hits = cache.hits;
    app.cache_misses = cache.misses;
    app.cache_saved_tokens = cache.saved_input_tokens;
    app.indexed_books = harness.store.indexed_book_count()?;
    app.account_profiles = list_account_profiles().await.map_or(0, |profiles| profiles.len());
    app.active_account = active_account_profile()
        .await
        .ok()
        .flatten()
        .map(|profile| profile.label);
    let mut events = harness.store.subscribe();
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let mut active_task: Option<JoinHandle<()>> = None;
    let mut deferred_answers: std::collections::VecDeque<(minha_core::RunId, String)> =
        std::collections::VecDeque::new();
    let mut quit = false;
    let mut dirty = true;
    let mut last_animation_tick = Instant::now();

    while !quit {
        // Ghostty can change font/DPI/zoom without emitting a row/column
        // resize. Refresh explicit Kitty graphics opportunistically so stale
        // rounded-image placements cannot survive that scale transition.
        dirty |= surface_renderer.refresh_geometry();
        dirty |= app.expire_toast();
        while let Ok(envelope) = events.try_recv() {
            app.apply_event(&envelope);
            dirty = true;
        }
        while let Ok(message) = result_rx.try_recv() {
            match message {
                RuntimeMessage::Finished(result) => {
                    active_task = None;
                    if let Err(error) = *result {
                        app.running = false;
                        app.show_recovery("run failed", error.clone());
                        app.push_system(SystemTone::Error, error);
                    }
                    app.set_sessions(harness.store.list_runs(100)?);
                }
                RuntimeMessage::LocalFinished {
                    display,
                    result,
                    persisted,
                } => match result {
                    Ok((output, exit_code)) if !persisted => {
                        app.push_shell_result(display, output, exit_code);
                    }
                    Ok(_) => {}
                    Err(error) => app.push_system(SystemTone::Error, error),
                },
                RuntimeMessage::LoginStarted {
                    verification_uri,
                    user_code,
                } => app.set_login_overlay(verification_uri, user_code, "Waiting for browser confirmation…"),
                RuntimeMessage::LoginFinished(result) => match result {
                    Ok(()) => {
                        app.update_login_message("Signed in. Press Esc to close.");
                        app.push_system(SystemTone::Success, "ChatGPT Codex login complete");
                    }
                    Err(error) => {
                        app.update_login_message(format!("Login failed: {error}"));
                        app.push_system(SystemTone::Error, error);
                    }
                },
            }
            dirty = true;
        }
        if let Some((run_id, text)) = pop_deferred_answer(active_task.is_some(), &mut deferred_answers) {
            active_task = Some(spawn_run(
                harness.clone(),
                result_tx.clone(),
                async move |harness| harness.resume_with_answer(run_id, &text).await,
            ));
        }

        if dirty {
            let mut surfaces = Vec::new();
            session.terminal.draw(|frame| {
                ui::draw(frame, &app);
                surfaces = ui::raster_surfaces(&app, frame.area());
            })?;
            surface_renderer.render(session.terminal.backend_mut(), &surfaces)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(100))? {
            let terminal_size = session.terminal.size()?;
            app.sync_drawer_visibility(terminal_size.width);
            let event = event::read()?;
            let action = map_event(event, &app, terminal_size.width, terminal_size.height);
            if app.update(action)? {
                quit = true;
            }
            if app.take_surface_renderer_reload() {
                surface_renderer = kitty::SurfaceRenderer::new(
                    &app.surface_renderer,
                    app.effective_theme(),
                    app.canvas_rgb(),
                );
                app.set_active_surface_renderer(surface_renderer.active_name());
            }
            dirty = true;
        }
        if app.running && (!app.reduced_motion || last_animation_tick.elapsed() >= Duration::from_secs(1)) {
            dirty = true;
            last_animation_tick = Instant::now();
        }

        if let Some(submission) = app.take_submission() {
            dirty = true;
            match submission {
                Submission::Quit => quit = true,
                Submission::Start { kind, text } => {
                    match run_gate(
                        active_task.is_some(),
                        "a run is still finishing; your message was not sent — try again",
                    ) {
                        RunGate::Open => {
                            active_task = Some(spawn_run(
                                harness.clone(),
                                result_tx.clone(),
                                async move |harness| harness.run(kind, &text).await,
                            ));
                        }
                        RunGate::Busy(warning) => {
                            app.push_system(SystemTone::Warning, warning);
                        }
                    }
                }
                Submission::Continue { run_id, text } => {
                    match run_gate(
                        active_task.is_some(),
                        "a run is still finishing; your message was not sent — try again",
                    ) {
                        RunGate::Open => {
                            active_task = Some(spawn_run(
                                harness.clone(),
                                result_tx.clone(),
                                async move |harness| harness.continue_session(run_id, &text).await,
                            ));
                        }
                        RunGate::Busy(warning) => {
                            app.push_system(SystemTone::Warning, warning);
                        }
                    }
                }
                Submission::Steer { run_id, text } => {
                    if let Err(error) = harness.queue_steering(run_id, &text) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                }
                Submission::AgentMessage {
                    run_id,
                    recipient,
                    text,
                } => {
                    let message_id = uuid::Uuid::now_v7().to_string();
                    match direct_office_request_envelope(message_id.clone(), &recipient, text.clone()) {
                        Ok(envelope) => match harness.store.insert_office_envelope(run_id, &envelope, None) {
                            Ok(stored_id) => {
                                harness.store.record_runtime_event(
                                    run_id,
                                    RuntimeEvent::OfficeMessageChanged {
                                        message_id: stored_id.clone(),
                                        room_id: "run".into(),
                                        sender: "user".into(),
                                        recipient,
                                        kind: "request".into(),
                                        summary: text,
                                        deduplicated: stored_id != message_id,
                                    },
                                )?;
                            }
                            Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                        },
                        Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                    }
                }
                Submission::Answer { run_id, text } => {
                    if active_task.is_none() {
                        let paused = harness
                            .store
                            .run(run_id)?
                            .is_some_and(|run| run.state == ExitState::UsagePaused);
                        let has_pending_request = app.pending_request.is_some();
                        active_task = Some(spawn_run(
                            harness.clone(),
                            result_tx.clone(),
                            async move |harness| {
                                // A pending request needs its answer even
                                // when the run is also usage-paused; only
                                // pure resumes use the pause-only path.
                                match answer_path(paused, has_pending_request) {
                                    AnswerPath::ResumePaused => harness.resume_paused(run_id).await,
                                    AnswerPath::ResumeWithAnswer => {
                                        harness.resume_with_answer(run_id, &text).await
                                    }
                                }
                            },
                        ));
                    } else {
                        deferred_answers.push_back((run_id, text));
                        app.push_system(
                            SystemTone::Info,
                            "answer queued; independent agents will finish before the blocked task resumes",
                        );
                    }
                }
                Submission::Clarify { run_id, answers } => {
                    if active_task.is_none() {
                        active_task = Some(spawn_run(
                            harness.clone(),
                            result_tx.clone(),
                            async move |harness| {
                                harness.resume_with_clarification_answers(run_id, &answers).await
                            },
                        ));
                    }
                }
                Submission::Interrupt { run_id } => {
                    if let Err(error) = harness.interrupt(run_id) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                    if let Some(task) = active_task.take() {
                        task.abort();
                    }
                }
                Submission::Pause { run_id } => {
                    if let Err(error) = harness.pause(run_id) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                }
                Submission::Shell { argv, display } => {
                    spawn_local_tool(
                        &harness,
                        app.active_run,
                        result_tx.clone(),
                        "exec",
                        json!({"argv": argv, "timeout_ms": 120_000}),
                        display,
                    );
                }
                Submission::Quality { action } => {
                    let display = format!("quality {action}");
                    spawn_local_tool(
                        &harness,
                        app.active_run,
                        result_tx.clone(),
                        "quality",
                        json!({
                            "action": action,
                            "suite": "auto",
                            "timeout_ms": 600_000,
                            "max_output_bytes": 131_072
                        }),
                        display,
                    );
                }
                Submission::GitHub { action, number } => {
                    let display = number.map_or_else(
                        || format!("github {action}"),
                        |number| format!("github {action} {number}"),
                    );
                    spawn_local_tool(
                        &harness,
                        app.active_run,
                        result_tx.clone(),
                        "github",
                        json!({"action": action, "number": number, "limit": 20}),
                        display,
                    );
                }
                Submission::Resume { run_id } => {
                    match run_gate(
                        active_task.is_some(),
                        "a run is still active; wait for it to finish before resuming a session",
                    ) {
                        RunGate::Open => match harness.store.run(run_id)? {
                            Some(run) => {
                                let replay = harness.store.events(run_id)?;
                                app.load_session(&run, &replay);
                            }
                            None => app.push_system(SystemTone::Error, "session not found"),
                        },
                        RunGate::Busy(warning) => app.push_system(SystemTone::Warning, warning),
                    }
                }
                Submission::Fork { run_id } => {
                    match run_gate(
                        active_task.is_some(),
                        "a run is still active; wait for it to finish before forking a session",
                    ) {
                        RunGate::Open => match harness.store.fork_run(run_id) {
                            Ok(fork) => {
                                let replay = harness.store.events(fork.id)?;
                                app.load_session(&fork, &replay);
                                app.push_system(SystemTone::Info, "forked session");
                            }
                            Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                        },
                        RunGate::Busy(warning) => app.push_system(SystemTone::Warning, warning),
                    }
                }
                Submission::Rename { run_id, title } => {
                    if let Err(error) = harness.store.rename_run(run_id, &title) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                    app.set_sessions(harness.store.list_runs(100)?);
                }
                Submission::Archive { run_id } => {
                    if let Err(error) = harness.store.archive_run(run_id) {
                        app.push_system(SystemTone::Error, error.to_string());
                    } else {
                        app.push_system(SystemTone::Info, "session archived");
                    }
                    app.set_sessions(harness.store.list_runs(100)?);
                }
                Submission::Compact { run_id } => {
                    if let Err(error) = harness.request_compaction(run_id) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                }
                Submission::Retry { run_id, fresh } => {
                    if active_task.is_none() {
                        if fresh {
                            let Some(run) = harness.store.run(run_id)? else {
                                app.push_system(SystemTone::Error, "session not found");
                                continue;
                            };
                            let goal = run.goal;
                            let kind = app.mode.run_kind();
                            app.prepare_fresh_session();
                            app.push_system(
                                SystemTone::Info,
                                "starting a fresh session with the same goal; prior transcript remains in history",
                            );
                            active_task = Some(spawn_run(
                                harness.clone(),
                                result_tx.clone(),
                                async move |harness| harness.run_fresh(kind, &goal).await,
                            ));
                        } else {
                            active_task = Some(spawn_run(
                                harness.clone(),
                                result_tx.clone(),
                                async move |harness| harness.retry_session(run_id).await,
                            ));
                        }
                    }
                }
                Submission::Clean => {
                    let removed = harness
                        .store
                        .prune_cache(harness.workspace_id(), harness.config.cache.max_bytes)?;
                    let leases = harness.store.reclaim_expired_leases()?;
                    let cache = harness.store.cache_totals(harness.workspace_id())?;
                    app.cache_entries = cache.entries;
                    app.cache_bytes = cache.bytes;
                    app.cache_hits = cache.hits;
                    app.cache_misses = cache.misses;
                    app.cache_saved_tokens = cache.saved_input_tokens;
                    app.push_system(
                        SystemTone::Success,
                        format!(
                            "cleaned local state: {removed} cache entries removed, {leases} expired leases reclaimed"
                        ),
                    );
                }
                Submission::Doctor => {
                    let root_ok = harness.root().is_dir();
                    let database_ok = harness.config.database_path.is_file();
                    let schema = harness.store.schema_version()?;
                    let journal = harness.store.journal_mode()?;
                    let books = harness.store.indexed_book_count()?;
                    app.set_diagnostics(vec![
                        Diagnostic {
                            label: "workspace".into(),
                            ok: root_ok,
                            detail: if root_ok {
                                harness.root().display().to_string()
                            } else {
                                "workspace path is not a directory".into()
                            },
                        },
                        Diagnostic {
                            label: "database".into(),
                            ok: database_ok,
                            detail: if database_ok {
                                format!("{} · schema {schema}", harness.config.database_path.display())
                            } else {
                                "database file is not present yet".into()
                            },
                        },
                        Diagnostic {
                            label: "schema".into(),
                            ok: schema == SCHEMA_VERSION,
                            detail: format!("{schema} · expected {SCHEMA_VERSION}"),
                        },
                        Diagnostic {
                            label: "journal".into(),
                            ok: journal.eq_ignore_ascii_case("wal"),
                            detail: journal,
                        },
                        Diagnostic {
                            label: "books".into(),
                            ok: books > 0,
                            detail: format!("{books} bundled entries indexed"),
                        },
                        Diagnostic {
                            label: "cache".into(),
                            ok: harness.config.cache.enabled,
                            detail: if harness.config.cache.enabled {
                                format!("enabled · max {}", format_bytes(harness.config.cache.max_bytes))
                            } else {
                                "disabled by configuration".into()
                            },
                        },
                    ]);
                }
                Submission::ShowProviders => app.push_system(SystemTone::Info, provider_summary().await),
                Submission::Login => {
                    let sender = result_tx.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let client = CodexOAuthClient::new(openai_oauth_config())?;
                            let device = client.begin_device_authorization().await?;
                            let _ = sender.send(RuntimeMessage::LoginStarted {
                                verification_uri: device.verification_uri.clone(),
                                user_code: device.user_code.clone(),
                            });
                            let auth = client.complete_device_authorization(&device).await?;
                            if auth.account_id.is_none() {
                                anyhow::bail!("login response did not include a ChatGPT account id");
                            }
                            save_default_auth(&auth).await?;
                            Ok::<(), anyhow::Error>(())
                        }
                        .await
                        .map_err(|error| error.to_string());
                        let _ = sender.send(RuntimeMessage::LoginFinished(result));
                    });
                }
                Submission::ShowStatus => {
                    let usage = harness.store.usage_totals(app.active_run)?;
                    app.set_usage_totals(usage);
                    let cache = harness.store.cache_totals(harness.workspace_id())?;
                    app.cache_entries = cache.entries;
                    app.cache_bytes = cache.bytes;
                    app.cache_hits = cache.hits;
                    app.cache_misses = cache.misses;
                    app.cache_saved_tokens = cache.saved_input_tokens;
                    app.indexed_books = harness.store.indexed_book_count()?;
                    app.account_profiles = list_account_profiles().await.map_or(0, |profiles| profiles.len());
                    app.active_account = active_account_profile()
                        .await
                        .ok()
                        .flatten()
                        .map(|profile| profile.label);
                    let (tasks, agents, board) = if let Some(run_id) = app.active_run {
                        (
                            harness.store.tasks(run_id)?,
                            harness.store.agents(run_id)?,
                            harness
                                .store
                                .board_entries(harness.workspace_id(), Some(run_id), None, 200)?,
                        )
                    } else {
                        (Vec::new(), Vec::new(), Vec::new())
                    };
                    let active = agents
                        .iter()
                        .filter(|agent| {
                            !matches!(
                                agent.state,
                                minha_core::protocol::AgentState::Completed
                                    | minha_core::protocol::AgentState::Failed
                                    | minha_core::protocol::AgentState::Cancelled
                            )
                        })
                        .count();
                    let (office_active, office_open, office_blocked) = app
                        .active_run
                        .map(|run_id| harness.store.office_health(run_id))
                        .transpose()?
                        .unwrap_or_default();
                    app.active_office_agents = office_active;
                    app.open_office_tasks = office_open;
                    app.blocked_office_tasks = office_blocked;
                    app.push_status_card(vec![
                        format!("session: {} · phase {:?}", app.status, app.phase),
                        format!(
                            "tokens: {} in ({} cached, {} written) + {} out ({} reasoning) = {} session · {} lifetime",
                            usage.session_input,
                            usage.session_cached_input,
                            usage.session_cache_write,
                            usage.session_output,
                            usage.session_reasoning_output,
                            usage.session_input.saturating_add(usage.session_output),
                            usage.lifetime_input.saturating_add(usage.lifetime_output)
                        ),
                        format!(
                            "context: {} / {} ({:.1}%) · compact at {}",
                            app.current_context_tokens,
                            app.context_limit,
                            app.context_percent(),
                            app.compact_at_tokens
                        ),
                        format!(
                            "office: {office_active} active · {office_open} open · {office_blocked} blocked · visible {active}/{} agents · {} tasks · {} board notes",
                            agents.len(),
                            tasks.len(),
                            board.len()
                        ),
                        format!(
                            "cache: {} entries · {} bytes · {} hits · {} books indexed",
                            cache.entries, cache.bytes, cache.hits, app.indexed_books
                        ),
                        format!(
                            "models: lead {} · workers {}",
                            identity_model_label(Some("Mina"), &app.model),
                            app.worker_models_summary()
                        ),
                    ]);
                }
                Submission::ShowBoard => {
                    let board =
                        harness
                            .store
                            .board_entries(harness.workspace_id(), app.active_run, None, 200)?;
                    app.set_board(board.iter().map(BoardEntry::view).collect());
                }
                Submission::AddNote { text } => {
                    if let Some(run_id) = app.active_run {
                        let entry = BoardEntry::session(
                            harness.workspace_id(),
                            run_id,
                            BoardKind::Finding,
                            text.lines().next().unwrap_or("User note"),
                            &text,
                        );
                        harness.store.insert_board_entry(&entry)?;
                        harness.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::BoardChanged { entry: entry.view() },
                        )?;
                        app.push_system(SystemTone::Success, "note added to the session board");
                    } else {
                        app.push_system(SystemTone::Info, "start or resume a session before /note");
                    }
                }
                Submission::PinBoard { id } => match harness.store.pin_board_entry(&id)? {
                    Some(entry) => {
                        if let Some(run_id) = app.active_run {
                            harness.store.record_runtime_event(
                                run_id,
                                RuntimeEvent::BoardChanged { entry: entry.view() },
                            )?;
                        }
                        app.push_system(SystemTone::Success, "decision or constraint pinned to project");
                    }
                    None => app.push_system(
                        SystemTone::Warning,
                        "pin needs an existing decision or constraint id",
                    ),
                },
                Submission::ResolveBoard { id } => {
                    match harness
                        .store
                        .revise_board_entry(&id, None, Some(BoardStatus::Resolved), None)?
                    {
                        Some(entry) => {
                            if let Some(run_id) = app.active_run {
                                harness.store.record_runtime_event(
                                    run_id,
                                    RuntimeEvent::BoardChanged { entry: entry.view() },
                                )?;
                            }
                            app.push_system(SystemTone::Success, "board entry resolved");
                        }
                        None => app.push_system(SystemTone::Warning, "board entry not found"),
                    }
                }
                Submission::Export { path } => match export_transcript(&app, path) {
                    Ok(path) => app.push_system(
                        SystemTone::Success,
                        format!("transcript exported to {}", path.display()),
                    ),
                    Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                },
                Submission::RefreshSessions => {
                    app.set_sessions(harness.store.list_runs(100)?);
                }
                Submission::ShowDiff => match GitRepo::new(harness.root()).diff() {
                    Ok(diff) => {
                        if let Some(run_id) = app.active_run {
                            let _ = harness.store.record_runtime_event(
                                run_id,
                                RuntimeEvent::FileChange {
                                    agent_id: None,
                                    path: None,
                                    diff,
                                },
                            );
                        } else {
                            app.push_diff(diff);
                        }
                    }
                    Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                },
                Submission::ShowSkills => match discover_skills(harness.root()) {
                    Ok(skills) => app.push_system(
                        SystemTone::Info,
                        skills
                            .iter()
                            .map(|skill| format!("${}: {}", skill.name, skill.description))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                },
                Submission::ShowMemories { query } => {
                    let settings = harness.store.memory_settings(harness.workspace_id())?;
                    if let Some(query) = query {
                        let hits = harness.store.search_memories(
                            harness.workspace_id(),
                            app.active_run,
                            &query,
                            10,
                        )?;
                        let text = if hits.is_empty() {
                            format!("No durable memories matched {query:?}.")
                        } else {
                            hits.iter()
                                .map(|hit| {
                                    format!(
                                        "{} · {} · {}\n  {}",
                                        hit.memory.id,
                                        hit.memory.scope.as_str(),
                                        hit.memory.subject,
                                        hit.memory.body
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        };
                        app.push_system(SystemTone::Info, text);
                    } else {
                        app.push_system(
                            SystemTone::Info,
                            format!(
                                "memory enabled={} · use={} · generate={} · retrieval limit={}\nUse /memories enabled|use|generate on|off, /memory QUERY, /memory pin ID, or /memory delete ID.",
                                settings.enabled,
                                settings.use_memory,
                                settings.generate,
                                harness.config.memory.retrieval_limit,
                            ),
                        );
                    }
                }
                Submission::SetMemories { setting, enabled } => {
                    let mut settings = harness.store.memory_settings(harness.workspace_id())?;
                    match setting.as_str() {
                        "enabled" | "enable" => settings.enabled = enabled,
                        "use" | "retrieve" | "retrieval" => settings.use_memory = enabled,
                        "generate" | "generation" => settings.generate = enabled,
                        _ => {
                            app.push_system(
                                SystemTone::Warning,
                                "memory setting must be enabled, use, or generate",
                            );
                            continue;
                        }
                    }
                    harness
                        .store
                        .set_memory_settings(harness.workspace_id(), settings)?;
                    app.push_system(SystemTone::Success, format!("memory {setting} set to {enabled}"));
                }
                Submission::MemoryPin { id } => {
                    let changed = harness.store.set_memory_state(&id, Some(true), None)?;
                    app.push_system(
                        if changed {
                            SystemTone::Success
                        } else {
                            SystemTone::Warning
                        },
                        if changed {
                            "memory pinned"
                        } else {
                            "memory not found"
                        },
                    );
                }
                Submission::MemoryDelete { id } => {
                    let changed = harness.store.set_memory_state(&id, None, Some(true))?;
                    app.push_system(
                        if changed {
                            SystemTone::Success
                        } else {
                            SystemTone::Warning
                        },
                        if changed {
                            "memory deleted"
                        } else {
                            "memory not found"
                        },
                    );
                }
            }
        }
    }

    if let Some(task) = active_task {
        task.abort();
    }
    drop(session);
    Ok(())
}

fn spawn_run<F, Fut>(
    harness: Harness,
    sender: mpsc::UnboundedSender<RuntimeMessage>,
    operation: F,
) -> JoinHandle<()>
where
    F: FnOnce(Harness) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<RunOutcome, minha_core::runtime::HarnessError>> + Send + 'static,
{
    tokio::spawn(async move {
        let result = operation(harness).await.map_err(|error| error.to_string());
        let _ = sender.send(RuntimeMessage::Finished(Box::new(result)));
    })
}

fn spawn_local_tool(
    harness: &Harness,
    run_id: Option<minha_core::protocol::RunId>,
    sender: mpsc::UnboundedSender<RuntimeMessage>,
    name: &'static str,
    arguments: serde_json::Value,
    display: String,
) {
    let root = harness.root().to_owned();
    let local_agent = EventAgentId::new();
    let call_id = format!("local-{local_agent}");
    if let Some(run_id) = run_id {
        let _ = harness.store.record_runtime_event(
            run_id,
            RuntimeEvent::ToolStarted {
                agent_id: local_agent,
                call_id: call_id.clone(),
                name: name.into(),
                arguments: arguments.clone(),
            },
        );
    }
    let store = harness.store.clone();
    tokio::spawn(async move {
        let result = run_local_tool(root, name, arguments)
            .await
            .map_err(|error| error.to_string());
        if let Some(run_id) = run_id {
            let (stdout, stderr, exit_code) = match &result {
                Ok((output, exit_code)) => (output.clone(), String::new(), *exit_code),
                Err(error) => (String::new(), error.clone(), None),
            };
            let _ = store.record_runtime_event(
                run_id,
                RuntimeEvent::ToolOutput {
                    agent_id: local_agent,
                    call_id,
                    name: name.into(),
                    stdout,
                    stderr,
                    exit_code,
                    truncated: false,
                },
            );
        }
        let _ = sender.send(RuntimeMessage::LocalFinished {
            display,
            result,
            persisted: run_id.is_some(),
        });
    });
}

async fn run_local_tool(
    root: PathBuf,
    name: &'static str,
    arguments: serde_json::Value,
) -> Result<(String, Option<i32>)> {
    tokio::task::spawn_blocking(move || {
        let executor = ToolExecutor::new(root, false)?;
        match executor.execute(name, &arguments)? {
            ToolOutcome::Output(output) => {
                let text = if output.stderr.is_empty() {
                    output.stdout
                } else if output.stdout.is_empty() {
                    output.stderr
                } else {
                    format!("{}\n{}", output.stdout, output.stderr)
                };
                Ok((text, output.exit_code))
            }
            ToolOutcome::NeedsInput(_) => anyhow::bail!("local command unexpectedly requested input"),
        }
    })
    .await
    .context("local command task failed")?
}

/// Read-only view of provider configuration, mirroring `minha provider list`.
///
/// Deliberately read-only: adding or removing a credential needs a no-echo
/// secret prompt, which the TUI has no safe surface for, so `/provider` reports
/// state and points at the CLI verbs for mutation. It never prints a key.
async fn provider_summary() -> String {
    let mut lines = vec!["Providers".to_owned()];
    let account = active_account_profile()
        .await
        .ok()
        .flatten()
        .map(|profile| profile.label);
    lines.push(match account {
        Some(label) => format!("  chatgpt_codex · oauth · signed in as {label}"),
        None => "  chatgpt_codex · oauth · not signed in; run /login".to_owned(),
    });
    match provider_credentials_path() {
        Some(path) => {
            match load_deepseek_key(&path) {
                Ok(Some(_)) => lines.push("  deepseek · api key · configured".to_owned()),
                Ok(None) => lines.push(
                    "  deepseek · api key · not configured; run `minha provider add deepseek`".to_owned(),
                ),
                Err(error) => lines.push(format!("  deepseek · api key · unreadable: {error}")),
            }
            match load_xiaomi_mimo(&path) {
                Ok(Some(credential)) => lines.push(format!(
                    "  xiaomi_mimo · api key · configured · {} · quota unavailable by API",
                    credential.base_url
                )),
                Ok(None) => lines.push(
                    "  xiaomi_mimo · api key · not configured; run `minha provider add xiaomi`".to_owned(),
                ),
                Err(error) => lines.push(format!("  xiaomi_mimo · api key · unreadable: {error}")),
            }
        }
        None => {
            lines.push("  deepseek · api key · no user configuration directory".to_owned());
            lines.push("  xiaomi_mimo · api key · no user configuration directory".to_owned());
        }
    }
    lines.push(
        "Use `minha provider add|test|remove NAME` to change credentials; \
         the key prompt is intentionally CLI-only."
            .to_owned(),
    );
    lines.join("\n")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn export_transcript(app: &App, path: Option<PathBuf>) -> Result<PathBuf> {
    let path = path.unwrap_or_else(|| {
        let name = app
            .active_run
            .map(|id| format!("{id}.md"))
            .unwrap_or_else(|| "transcript.md".into());
        app.root.join(".minha/transcripts").join(name)
    });
    let path = if path.is_absolute() {
        path
    } else {
        app.root.join(path)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, app.transcript_text())?;
    Ok(path)
}

/// True when Up/Down/Enter/Esc belong to a list rather than to the editor.
fn list_has_focus(app: &App) -> bool {
    app.completion_open()
        || app.drawer_interactive()
        || app.overlay_scrolls()
        || matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books))
}

/// True when Up/Down/Enter/Esc and scroll should move the selection on the
/// inline decision card (clarification, mid-run question, or exec approval)
/// rather than the composer — i.e. one is on screen and nothing's typed yet.
fn decision_card_has_focus(app: &App) -> bool {
    (app.has_active_clarification() || app.pending_request.is_some()) && app.input.is_empty()
}

/// The opt-in Vim layer is deliberately local and bounded: it edits only the
/// composer, leaves command/approval/list surfaces alone, and keeps an empty
/// Normal-mode composer useful for transcript travel.
fn vim_action(key: crossterm::event::KeyEvent, app: &App) -> Option<AppAction> {
    if !app.vim_scroll_enabled()
        || app.completion_open()
        || app.drawer_interactive()
        || app.overlay.is_some()
        || app.focused_agent.is_some()
        || app.has_active_clarification()
        || app.pending_request.is_some()
    {
        return None;
    }
    if key.code == KeyCode::Esc {
        return Some(AppAction::VimNormal);
    }
    // In Normal (and an unfinished `d`/`y`) Enter must not fall through to
    // the ordinary composer submit path. The user enters Insert mode before
    // a deliberate send, keeping Vim navigation strictly local.
    if key.code == KeyCode::Enter && app.vim_mode() != VimMode::Insert {
        return Some(AppAction::VimNormal);
    }
    if key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
        return (app.vim_mode() == VimMode::Normal).then_some(AppAction::Redo);
    }
    if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
        return None;
    }
    let KeyCode::Char(character) = key.code else {
        return None;
    };
    let upper = character.is_ascii_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT);
    let lower = character.to_ascii_lowercase();
    match app.vim_mode() {
        VimMode::Insert => None,
        VimMode::DeletePending => match lower {
            'd' => Some(AppAction::VimDeleteLine),
            _ => Some(AppAction::VimNormal),
        },
        VimMode::YankPending => match lower {
            'y' => Some(AppAction::VimYankLine),
            _ => Some(AppAction::VimNormal),
        },
        VimMode::Normal => match (lower, upper) {
            ('h', false) => Some(AppAction::CursorLeft),
            ('j', false) => Some(AppAction::VimMoveDown),
            ('k', false) => Some(AppAction::VimMoveUp),
            ('l', false) => Some(AppAction::CursorRight),
            ('w', false) => Some(AppAction::VimWordForward),
            ('b', false) => Some(AppAction::VimWordBackward),
            ('e', false) => Some(AppAction::VimWordEnd),
            ('0', false) => Some(AppAction::CursorHome),
            ('$', _) => Some(AppAction::CursorEnd),
            ('i', false) => Some(AppAction::VimInsert),
            ('a', false) => Some(AppAction::VimAppend),
            ('i', true) => Some(AppAction::VimInsertLineStart),
            ('a', true) => Some(AppAction::VimAppendLineEnd),
            ('x', false) => Some(AppAction::VimDeleteChar),
            ('d', false) => Some(AppAction::VimDeletePending),
            ('d', true) => Some(AppAction::VimDeleteToLineEnd),
            ('c', true) => Some(AppAction::VimChangeToLineEnd),
            ('y', false) => Some(AppAction::VimYankPending),
            ('p', false) => Some(AppAction::VimPasteLine),
            ('o', false) => Some(AppAction::VimOpenBelow),
            ('o', true) => Some(AppAction::VimOpenAbove),
            ('u', false) => Some(AppAction::Undo),
            ('g', false) => Some(AppAction::ScrollTop),
            ('g', true) => Some(AppAction::ScrollBottom),
            _ => Some(AppAction::None),
        },
    }
}

fn map_event(event: Event, app: &App, terminal_width: u16, terminal_height: u16) -> AppAction {
    match event {
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && let Some(action) = vim_action(key, app) =>
        {
            action
        }
        // With an empty composer, Home/End are transcript navigation rather
        // than no-op line movement.  A populated editor keeps its familiar
        // start/end-of-line bindings through the keymap below.
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && app.input.is_empty()
                && !list_has_focus(app)
                && !decision_card_has_focus(app)
                && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Home | KeyCode::End) =>
        {
            if key.code == KeyCode::Home {
                AppAction::ScrollTop
            } else {
                AppAction::ScrollBottom
            }
        }
        // Key repeat counts as input: holding Backspace must keep deleting.
        Event::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && let Some(action) = keymap::resolve(key) =>
        {
            action
        }
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    if app.running {
                        AppAction::Interrupt
                    } else {
                        AppAction::Quit
                    }
                }
                (KeyCode::Char('d'), modifiers)
                    if modifiers.contains(KeyModifiers::CONTROL) && app.input.is_empty() =>
                {
                    AppAction::Quit
                }
                (KeyCode::Char('o'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::ToggleDetails
                }
                (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::CommandPalette
                }
                (KeyCode::Char('t'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::ToggleTasks
                }
                (KeyCode::Char('j'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::Newline
                }
                (KeyCode::Char('r'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::HistoryPrevious
                }
                (KeyCode::Char('?'), _) if app.input.is_empty() => AppAction::Help,
                (KeyCode::Tab, _) => AppAction::Complete,
                (KeyCode::BackTab, _) => AppAction::ToggleDrawer,
                (KeyCode::Enter, modifiers)
                    if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    AppAction::Newline
                }
                // Enter accepts the highlighted completion; the composer only sees it
                // once no list has focus.
                (KeyCode::Enter, _) if app.completion_open() => AppAction::Activate,
                (KeyCode::Enter, _)
                    if app.drawer_interactive()
                        || matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books))
                        || decision_card_has_focus(app) =>
                {
                    AppAction::Activate
                }
                (KeyCode::Enter, _) => AppAction::Submit,
                (KeyCode::Backspace, _) => AppAction::Backspace,
                (KeyCode::Delete, _) => AppAction::Delete,
                (KeyCode::Left, _) => AppAction::CursorLeft,
                (KeyCode::Right, _) => AppAction::CursorRight,
                (KeyCode::Esc, _) => AppAction::Escape,
                (KeyCode::Up, _) if list_has_focus(app) || decision_card_has_focus(app) => {
                    AppAction::SelectUp
                }
                (KeyCode::Down, _) if list_has_focus(app) || decision_card_has_focus(app) => {
                    AppAction::SelectDown
                }
                (KeyCode::Up, _) => AppAction::CursorUp,
                (KeyCode::Down, _) => AppAction::CursorDown,
                (KeyCode::PageUp, _) => AppAction::PageUp,
                (KeyCode::PageDown, _) => AppAction::PageDown,
                (KeyCode::Char(character), modifiers) if !modifiers.contains(KeyModifiers::CONTROL) => {
                    AppAction::Input(character)
                }
                _ => AppAction::None,
            }
        }
        Event::Paste(text) => AppAction::Paste(text),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp if app.completion_open() => AppAction::SelectUp,
            MouseEventKind::ScrollDown if app.completion_open() => AppAction::SelectDown,
            // A visible help/keymap surface owns wheel input just as it owns
            // keyboard arrows. Without this branch the hidden transcript
            // scrolled behind a stationary overlay.
            MouseEventKind::ScrollUp if app.overlay_scrolls() => AppAction::SelectUp,
            MouseEventKind::ScrollDown if app.overlay_scrolls() => AppAction::SelectDown,
            MouseEventKind::ScrollUp
                if matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books)) =>
            {
                AppAction::SelectUp
            }
            MouseEventKind::ScrollDown
                if matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books)) =>
            {
                AppAction::SelectDown
            }
            // A non-scrollable modal still owns the pointer. Do not mutate a
            // hidden decision card, drawer, or transcript behind it.
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if app.overlay.is_some() => AppAction::None,
            MouseEventKind::ScrollUp if app.has_active_clarification() || app.pending_request.is_some() => {
                AppAction::SelectUp
            }
            MouseEventKind::ScrollDown if app.has_active_clarification() || app.pending_request.is_some() => {
                AppAction::SelectDown
            }
            MouseEventKind::ScrollUp
                if app.drawer_interactive()
                    && ui::drawer_rect(
                        app,
                        ratatui::layout::Rect::new(0, 0, terminal_width, terminal_height),
                    )
                    .is_some_and(|drawer| {
                        mouse.column >= drawer.x
                            && mouse.column < drawer.right()
                            && mouse.row >= drawer.y
                            && mouse.row < drawer.bottom()
                    }) =>
            {
                AppAction::SelectUp
            }
            MouseEventKind::ScrollDown
                if app.drawer_interactive()
                    && ui::drawer_rect(
                        app,
                        ratatui::layout::Rect::new(0, 0, terminal_width, terminal_height),
                    )
                    .is_some_and(|drawer| {
                        mouse.column >= drawer.x
                            && mouse.column < drawer.right()
                            && mouse.row >= drawer.y
                            && mouse.row < drawer.bottom()
                    }) =>
            {
                AppAction::SelectDown
            }
            // Modal surfaces draw last and own pointer input.  Without this
            // shield a click on their opaque cells could activate a hidden
            // approval option, composer cursor, or operations-drawer item
            // behind the modal.
            MouseEventKind::Down(MouseButton::Left) if app.overlay.is_some() => AppAction::None,
            MouseEventKind::Down(MouseButton::Left)
                if (app.has_active_clarification() || app.pending_request.is_some())
                    && ui::decision_card_option_at(
                        app,
                        mouse.column,
                        mouse.row,
                        terminal_width,
                        terminal_height,
                    )
                    .is_some() =>
            {
                ui::decision_card_option_at(app, mouse.column, mouse.row, terminal_width, terminal_height)
                    .map_or(AppAction::None, AppAction::ActivateClarificationOption)
            }
            MouseEventKind::Down(MouseButton::Left)
                if ui::composer_cursor_at(app, mouse.column, mouse.row, terminal_width, terminal_height)
                    .is_some() =>
            {
                ui::composer_cursor_at(app, mouse.column, mouse.row, terminal_width, terminal_height)
                    .map_or(AppAction::None, AppAction::CursorSet)
            }
            MouseEventKind::ScrollUp => AppAction::ScrollUp,
            MouseEventKind::ScrollDown => AppAction::ScrollDown,
            MouseEventKind::Down(MouseButton::Left)
                if app.drawer_visible
                    && ui::drawer_hit(app, mouse.column, mouse.row, terminal_width, terminal_height)
                        .is_some() =>
            {
                let index = ui::drawer_hit(app, mouse.column, mouse.row, terminal_width, terminal_height)
                    .unwrap_or(0);
                AppAction::ActivateIndex(index)
            }
            _ => AppAction::None,
        },
        _ => AppAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn terminal_state_enters_bracketed_paste_plus_alt_screen() {
        let mut bytes = Vec::new();
        enter_terminal_state(&mut bytes, false).expect("test write should succeed");
        let entered = String::from_utf8_lossy(&bytes);
        assert!(
            entered.contains("\x1b[?1049h"),
            "alternate screen must be entered"
        );
        assert!(
            entered.contains("\x1b[?2004h"),
            "bracketed paste must be enabled or Event::Paste never fires"
        );
        assert!(!entered.contains("\x1b[?1000h"), "mouse capture must stay opt-in");

        let mut bytes = Vec::new();
        enter_terminal_state(&mut bytes, true).expect("test write should succeed");
        assert!(String::from_utf8_lossy(&bytes).contains("\x1b[?1000h"));
    }

    #[test]
    fn start_failure_restores_mouse_paste_and_alt_screen() {
        let mut bytes = Vec::new();
        restore_after_start_failure(&mut bytes).expect("test write should succeed");
        let restored = String::from_utf8_lossy(&bytes);
        assert!(restored.contains("\x1b[?1000l"));
        assert!(restored.contains("\x1b[?2004l"));
        assert!(
            restored.contains("\x1b[?1049l"),
            "a failed start must never leave the alternate screen enabled"
        );
        assert!(restored.contains("\x1b[?25h"), "the cursor must be restored too");
    }

    #[test]
    fn run_gate_refuses_launches_while_a_task_is_live() {
        assert!(matches!(run_gate(false, "busy"), RunGate::Open));
        assert_eq!(
            run_gate(true, "a run is still finishing"),
            RunGate::Busy("a run is still finishing")
        );
    }

    #[test]
    fn deferred_answers_drain_fifo_only_while_idle() {
        let mut deferred = VecDeque::new();
        deferred.push_back((minha_core::RunId::new(), "first".into()));
        deferred.push_back((minha_core::RunId::new(), "second".into()));

        assert_eq!(
            pop_deferred_answer(true, &mut deferred),
            None,
            "no answer may launch while a run task is active"
        );
        assert_eq!(deferred.len(), 2);

        let (first_id, first) =
            pop_deferred_answer(false, &mut deferred).expect("an idle slot must drain the first answer");
        assert_eq!(first, "first");
        let (_, second) =
            pop_deferred_answer(false, &mut deferred).expect("the second answer must not be lost");
        assert_eq!(second, "second");
        assert!(deferred.is_empty());
        assert_ne!(first_id, minha_core::RunId::new());
    }

    #[test]
    fn answer_routing_prefers_the_pending_request_over_pause() {
        assert_eq!(answer_path(true, false), AnswerPath::ResumePaused);
        assert_eq!(answer_path(true, true), AnswerPath::ResumeWithAnswer);
        assert_eq!(answer_path(false, true), AnswerPath::ResumeWithAnswer);
        assert_eq!(answer_path(false, false), AnswerPath::ResumeWithAnswer);
    }

    #[test]
    fn direct_messages_build_a_validated_typed_office_envelope() {
        let envelope = direct_office_request_envelope(
            "message-1".into(),
            "agent:worker-1",
            "check the parser boundary".into(),
        )
        .expect("a valid recipient and compact summary should build an envelope");

        assert_eq!(envelope.schema_version, OFFICE_ENVELOPE_SCHEMA_VERSION);
        assert_eq!(envelope.room_id, RUN_ROOM_ID);
        assert_eq!(envelope.sender, "user");
        assert_eq!(envelope.recipient, Recipient::Agent("worker-1".into()));
        assert_eq!(envelope.kind, CoordinationKind::Request);
        assert_eq!(envelope.requested_action.as_deref(), Some("respond_to_user"));
        assert!(envelope.validate().is_ok());
        assert!(
            direct_office_request_envelope(
                "message-2".into(),
                "not-an-office-address",
                "check the parser boundary".into(),
            )
            .is_err()
        );
    }

    #[test]
    fn vim_keys_are_opt_in_and_route_only_the_bounded_composer_commands() {
        let plain = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('h'),
                    KeyModifiers::NONE
                )),
                &plain,
                100,
                30,
            ),
            AppAction::Input('h')
        );

        let mut vim = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        vim.tui_settings.vim_scroll = true;
        vim.vim_mode = VimMode::Normal;
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('h'),
                    KeyModifiers::NONE
                )),
                &vim,
                100,
                30,
            ),
            AppAction::CursorLeft
        );
        for (key, expected) in [
            ('w', AppAction::VimWordForward),
            ('b', AppAction::VimWordBackward),
            ('e', AppAction::VimWordEnd),
            ('0', AppAction::CursorHome),
            ('$', AppAction::CursorEnd),
        ] {
            assert_eq!(
                map_event(
                    Event::Key(crossterm::event::KeyEvent::new(
                        KeyCode::Char(key),
                        if key == '$' {
                            KeyModifiers::SHIFT
                        } else {
                            KeyModifiers::NONE
                        },
                    )),
                    &vim,
                    100,
                    30,
                ),
                expected,
                "Vim Normal-mode {key:?} must be routed locally"
            );
        }
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('D'),
                    KeyModifiers::SHIFT
                )),
                &vim,
                100,
                30,
            ),
            AppAction::VimDeleteToLineEnd
        );
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE
                )),
                &vim,
                100,
                30,
            ),
            AppAction::VimNormal,
            "Normal-mode Enter must never dispatch the composer"
        );
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                &vim,
                100,
                30,
            ),
            AppAction::VimNormal
        );

        vim.vim_mode = VimMode::DeletePending;
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(
                    KeyCode::Char('d'),
                    KeyModifiers::NONE
                )),
                &vim,
                100,
                30,
            ),
            AppAction::VimDeleteLine
        );

        vim.focused_agent = Some(EventAgentId::new());
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                &vim,
                100,
                30,
            ),
            AppAction::Escape,
            "an agent inspector owns Esc before the Vim composer does"
        );
    }

    #[test]
    fn empty_composer_home_and_end_navigate_the_transcript() {
        let empty = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        for (key, expected) in [
            (KeyCode::Home, AppAction::ScrollTop),
            (KeyCode::End, AppAction::ScrollBottom),
        ] {
            assert_eq!(
                map_event(
                    Event::Key(crossterm::event::KeyEvent::new(key, KeyModifiers::NONE)),
                    &empty,
                    100,
                    30,
                ),
                expected
            );
        }

        let mut populated = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        populated.input = "draft".into();
        populated.input_cursor = populated.input.len();
        assert_eq!(
            map_event(
                Event::Key(crossterm::event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
                &populated,
                100,
                30,
            ),
            AppAction::CursorHome
        );
    }

    #[test]
    fn mouse_wheel_follows_the_visible_overlay_or_list() {
        let mouse = |kind| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut help = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        help.overlay = Some(app::Overlay::Help);
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &help, 80, 24),
            AppAction::SelectDown
        );
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollUp), &help, 80, 24),
            AppAction::SelectUp
        );

        let mut completion = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        completion
            .update(AppAction::Input('/'))
            .expect("slash opens completion");
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &completion, 80, 24),
            AppAction::SelectDown
        );

        let mut books = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        books.overlay = Some(app::Overlay::Books);
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &books, 80, 24),
            AppAction::SelectDown
        );

        let mut context = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        context.overlay = Some(app::Overlay::Context);
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &context, 80, 24),
            AppAction::SelectDown
        );

        let mut status = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        status.overlay = Some(app::Overlay::Status);
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &status, 80, 24),
            AppAction::SelectDown
        );

        let mut drawer = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        drawer.sync_drawer_visibility(80);
        drawer.set_drawer_visible(true);
        drawer.agents.push(app::AgentView {
            id: EventAgentId::new(),
            role: "worker".into(),
            model: "gpt-5.6-luna".into(),
            state: minha_core::protocol::AgentState::Working,
            detail: String::new(),
        });
        assert_eq!(
            map_event(mouse(MouseEventKind::ScrollDown), &drawer, 80, 24),
            AppAction::ScrollDown,
            "scrolling outside the drawer must leave its selection alone"
        );
        let drawer_rect =
            ui::drawer_rect(&drawer, ratatui::layout::Rect::new(0, 0, 80, 24)).expect("visible drawer");
        let drawer_mouse = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: drawer_rect.x + 1,
            row: drawer_rect.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(map_event(drawer_mouse, &drawer, 80, 24), AppAction::SelectDown);
    }

    #[test]
    fn modal_owns_left_clicks_over_hidden_drawer_controls() {
        let click = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut app = App::new(std::path::PathBuf::from("/tmp/minha"), 128_000);
        app.set_drawer_visible(true);
        app.drawer_tab = app::DrawerTab::Activity;
        app.agents.push(app::AgentView {
            id: EventAgentId::new(),
            role: "worker".into(),
            model: "gpt-5.6-luna".into(),
            state: minha_core::protocol::AgentState::Working,
            detail: String::new(),
        });
        let drawer = ui::drawer_rect(&app, ratatui::layout::Rect::new(0, 0, 120, 30))
            .expect("wide drawer must render");
        let column = drawer.x + 1;
        let row = drawer.y + 1;
        assert_eq!(
            map_event(click(column, row), &app, 120, 30),
            AppAction::ActivateIndex(0)
        );

        app.overlay = Some(app::Overlay::Status);
        assert_eq!(
            map_event(click(column, row), &app, 120, 30),
            AppAction::None,
            "an opaque modal must shield its hidden drawer from pointer activation"
        );
    }
}
