//! Transcript-first Ratatui interface for Minha.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

mod app;
mod ui;

pub use app::{App, AppAction};

use anyhow::{Context, Result};
use app::{Diagnostic, Submission, SystemTone};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use minha_core::auth::{
    CodexOAuthClient, active_account_profile, list_account_profiles, openai_oauth_config, save_default_auth,
};
use minha_core::executor::{ToolExecutor, ToolOutcome};
use minha_core::facts::{BoardEntry, BoardKind, BoardStatus};
use minha_core::instructions::discover_skills;
use minha_core::protocol::ExitState;
use minha_core::protocol::{EventAgentId, RuntimeEvent};
use minha_core::store::SCHEMA_VERSION;
use minha_core::worktree::GitRepo;
use minha_core::{Harness, RunOutcome};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

enum RuntimeMessage {
    Finished(Result<RunOutcome, String>),
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

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    mouse_enabled: bool,
}

impl TerminalSession {
    fn start(mouse_enabled: bool) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        let entered = if mouse_enabled {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        } else {
            execute!(stdout, EnterAlternateScreen)
        };
        if let Err(error) = entered {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                mouse_enabled,
            }),
            Err(error) => {
                let _ = disable_raw_mode();
                let mut stdout = io::stdout();
                if mouse_enabled {
                    let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
                } else {
                    let _ = execute!(stdout, LeaveAlternateScreen);
                }
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.mouse_enabled {
            let _ = execute!(
                self.terminal.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
        } else {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        let _ = self.terminal.show_cursor();
    }
}

/// Run the responsive control room against the in-process runtime actor.
pub async fn run(harness: Harness) -> Result<()> {
    let mut session = TerminalSession::start(harness.config.tui.mouse)?;
    let mut app = App::new(
        harness.root().to_owned(),
        harness.config.context.context_limit as u64,
    );
    app.details_expanded = harness.config.tui.tool_detail.eq_ignore_ascii_case("expanded");
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
    let mut deferred_answer: Option<(minha_core::RunId, String)> = None;
    let mut quit = false;
    let mut dirty = true;

    while !quit {
        while let Ok(envelope) = events.try_recv() {
            app.apply_event(&envelope);
            dirty = true;
        }
        while let Ok(message) = result_rx.try_recv() {
            match message {
                RuntimeMessage::Finished(result) => {
                    active_task = None;
                    if let Err(error) = result {
                        app.running = false;
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
        if active_task.is_none()
            && let Some((run_id, text)) = deferred_answer.take()
        {
            active_task = Some(spawn_run(
                harness.clone(),
                result_tx.clone(),
                async move |harness| harness.resume_with_answer(run_id, &text).await,
            ));
        }

        if dirty {
            session.terminal.draw(|frame| ui::draw(frame, &app))?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(100))? {
            let terminal_width = session.terminal.size()?.width;
            let action = map_event(event::read()?, &app, terminal_width);
            if app.update(action)? {
                quit = true;
            }
            dirty = true;
        }

        if let Some(submission) = app.take_submission() {
            dirty = true;
            match submission {
                Submission::Quit => quit = true,
                Submission::Start { kind, text } => {
                    if active_task.is_none() {
                        active_task = Some(spawn_run(
                            harness.clone(),
                            result_tx.clone(),
                            async move |harness| harness.run(kind, &text).await,
                        ));
                    }
                }
                Submission::Continue { run_id, text } => {
                    if active_task.is_none() {
                        active_task = Some(spawn_run(
                            harness.clone(),
                            result_tx.clone(),
                            async move |harness| harness.continue_session(run_id, &text).await,
                        ));
                    }
                }
                Submission::Steer { run_id, text } => {
                    if let Err(error) = harness.queue_steering(run_id, &text) {
                        app.push_system(SystemTone::Error, error.to_string());
                    }
                }
                Submission::Answer { run_id, text } => {
                    if active_task.is_none() {
                        let paused = harness
                            .store
                            .run(run_id)?
                            .is_some_and(|run| run.state == ExitState::UsagePaused);
                        active_task = Some(spawn_run(
                            harness.clone(),
                            result_tx.clone(),
                            async move |harness| {
                                if paused {
                                    harness.resume_paused(run_id).await
                                } else {
                                    harness.resume_with_answer(run_id, &text).await
                                }
                            },
                        ));
                    } else {
                        deferred_answer = Some((run_id, text));
                        app.push_system(
                            SystemTone::Info,
                            "answer queued; independent agents will finish before the blocked task resumes",
                        );
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
                Submission::Resume { run_id } => match harness.store.run(run_id)? {
                    Some(run) => {
                        let replay = harness.store.events(run_id)?;
                        app.load_session(&run, &replay);
                    }
                    None => app.push_system(SystemTone::Error, "session not found"),
                },
                Submission::Fork { run_id } => match harness.store.fork_run(run_id) {
                    Ok(fork) => {
                        let replay = harness.store.events(fork.id)?;
                        app.load_session(&fork, &replay);
                        app.push_system(SystemTone::Info, "forked session");
                    }
                    Err(error) => app.push_system(SystemTone::Error, error.to_string()),
                },
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
                        format!("models: lead {} · workers gpt-5.3-codex-spark", app.model),
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
        let _ = sender.send(RuntimeMessage::Finished(result));
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

fn map_event(event: Event, app: &App, terminal_width: u16) -> AppAction {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => match (key.code, key.modifiers) {
            (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                if app.running {
                    AppAction::Escape
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
            (KeyCode::Tab, _) => AppAction::ToggleDrawer,
            (KeyCode::Enter, modifiers) if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                AppAction::Newline
            }
            (KeyCode::Enter, _)
                if app.drawer_visible
                    || matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books)) =>
            {
                AppAction::Activate
            }
            (KeyCode::Enter, _) => AppAction::Submit,
            (KeyCode::Backspace, _) => AppAction::Backspace,
            (KeyCode::Esc, _) => AppAction::Escape,
            (KeyCode::Up, _)
                if app.drawer_visible
                    || matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books)) =>
            {
                AppAction::SelectUp
            }
            (KeyCode::Down, _)
                if app.drawer_visible
                    || matches!(app.overlay, Some(app::Overlay::Sessions | app::Overlay::Books)) =>
            {
                AppAction::SelectDown
            }
            (KeyCode::Up, _) => AppAction::ScrollUp,
            (KeyCode::Down, _) => AppAction::ScrollDown,
            (KeyCode::PageUp, _) => AppAction::PageUp,
            (KeyCode::PageDown, _) => AppAction::PageDown,
            (KeyCode::Char(character), modifiers) if !modifiers.contains(KeyModifiers::CONTROL) => {
                AppAction::Input(character)
            }
            _ => AppAction::None,
        },
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => AppAction::ScrollUp,
            MouseEventKind::ScrollDown => AppAction::ScrollDown,
            MouseEventKind::Down(MouseButton::Left)
                if app.drawer_visible && mouse.column >= terminal_width.saturating_sub(40) =>
            {
                AppAction::Activate
            }
            _ => AppAction::None,
        },
        _ => AppAction::None,
    }
}
