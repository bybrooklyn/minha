#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use clap::{Args, Parser, Subcommand};
use minha_core::runtime::HarnessError;
use minha_core::{
    Config, ExitState, Harness, RunId, RunKind, RunOutcome, Store,
    auth::{
        CodexOAuthClient, active_account_profile, default_auth_status, list_account_profiles,
        load_default_auth, logout_default, openai_oauth_config, remove_account_profile, save_account_profile,
        set_account_profile_enabled, set_active_account_profile,
    },
    store::state_name,
    worktree::GitRepo,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};

const EXIT_OK: u8 = 0;
const EXIT_AUTH: u8 = 3;
const EXIT_BLOCKED: u8 = 4;
const EXIT_ERROR: u8 = 5;

#[derive(Parser, Debug)]
#[command(
    name = "minha",
    version,
    about = "A fast, token-conscious multi-agent coding harness"
)]
struct Cli {
    #[arg(long, global = true, help = "Emit stable machine-readable output")]
    json: bool,
    #[arg(long, global = true, help = "Emit typed runtime events as JSON Lines")]
    jsonl: bool,
    #[command(subcommand)]
    command: Option<CommandLine>,
}

#[derive(Subcommand, Debug)]
enum CommandLine {
    /// Open the interactive terminal interface.
    Tui,
    /// Authenticate or manage named ChatGPT Codex account profiles.
    Login(LoginArgs),
    /// Remove the active local ChatGPT Codex credential.
    Logout,
    /// List exact model slugs available to the active account.
    Models,
    /// Plan, branch, implement, integrate, and judge a coding task.
    Run(TaskArgs),
    /// Inspect and produce a plan without editing the workspace.
    Plan(TaskArgs),
    /// Run parallel read-only Spark audit lenses.
    Audit(OptionalTask),
    /// Review the current workspace or a supplied review goal.
    Review(OptionalTask),
    /// Answer a persisted blocking agent question.
    Answer(AnswerArgs),
    /// Continue a run paused at the configured account-usage reserve.
    Pickup(RunArgs),
    /// Show run, task, agent, context, cache, and token status.
    Status(RunArgs),
    /// Show session and lifetime token usage.
    Usage(RunArgs),
    /// Print persisted typed events for a run.
    Events(RunArgs),
    /// Show the durable transcript and state for a run.
    Show(RunArgs),
    /// List recent persisted sessions.
    Sessions,
    /// Resume a persisted session, optionally with new steering.
    Resume(ResumeArgs),
    /// Fork a persisted session into a new run.
    Fork(RunArgs),
    /// Rename a persisted session.
    Rename(RenameArgs),
    /// Archive a persisted session without deleting it.
    Archive(RunArgs),
    /// Print the current repository diff.
    Diff,
    /// Check local repository, config, state, tools, and authentication.
    Doctor,
    /// Print Minha's version and active workspace root.
    #[command(name = "version")]
    Version,
    /// Check or install the latest checksum-verified GitHub Release.
    Update(UpdateArgs),
}

#[derive(Args, Debug)]
struct TaskArgs {
    #[arg(value_name = "TASK", help = "Issue or goal for the harness")]
    task: String,
}

#[derive(Args, Debug)]
struct OptionalTask {
    #[arg(value_name = "TASK", help = "Optional audit or review focus")]
    task: Option<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    #[arg(value_name = "UUID")]
    id: Option<String>,
    #[arg(long, value_name = "UUID")]
    run: Option<String>,
}

#[derive(Args, Debug)]
struct AnswerArgs {
    #[arg(value_name = "TEXT", help = "Answer to the pending question")]
    text: String,
    #[arg(long, value_name = "UUID")]
    run: Option<String>,
}

#[derive(Args, Debug)]
struct ResumeArgs {
    #[arg(value_name = "UUID")]
    id: Option<String>,
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,
}

#[derive(Args, Debug)]
struct RenameArgs {
    #[arg(value_name = "TITLE", help = "New session title")]
    title: String,
    #[arg(long, value_name = "UUID")]
    run: Option<String>,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    #[arg(long, help = "Only check whether a newer GitHub release exists")]
    check: bool,
    #[arg(long, value_name = "OWNER/REPOSITORY", help = "GitHub repository override")]
    repo: Option<String>,
}

#[derive(Args, Debug)]
struct LoginArgs {
    #[arg(long, default_value = "default", help = "Profile name to create or update")]
    profile: String,
    #[arg(long, help = "Human-readable profile label")]
    label: Option<String>,
    #[command(subcommand)]
    command: Option<LoginCommand>,
}

#[derive(Subcommand, Debug)]
enum LoginCommand {
    /// Show authentication state for the active profile.
    Status,
    /// List local account profiles without exposing credentials.
    List,
    /// Make an enabled profile active.
    Use { name: String },
    /// Include a profile in deterministic worker rotation.
    Enable { name: String },
    /// Remove a profile from rotation without deleting it.
    Disable { name: String },
    /// Delete a named local credential profile.
    Remove { name: String },
}

#[derive(Serialize)]
struct Envelope {
    ok: bool,
    state: String,
    data: Value,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_output = cli.json;
    let jsonl = cli.jsonl;
    let result = dispatch(cli.command, json_output, jsonl).await;
    emit(json_output || jsonl, result)
}

struct ResultData {
    code: u8,
    state: ExitState,
    data: Value,
    error: Option<String>,
}

async fn dispatch(command: Option<CommandLine>, json_output: bool, jsonl: bool) -> ResultData {
    let Some(command) = command else {
        return tui(json_output).await;
    };
    match command {
        CommandLine::Tui => tui(json_output).await,
        CommandLine::Login(args) => login(args, json_output, jsonl).await,
        CommandLine::Logout => match logout_default().await {
            Ok(removed) => success(ExitState::Succeeded, json!({"logged_out": removed})),
            Err(error) => auth_error(error.to_string()),
        },
        CommandLine::Models => {
            with_harness(|h| async move {
                let models = h.models().await?;
                Ok(success(
                    ExitState::Succeeded,
                    json!({"models": models.iter().map(|m| &m.slug).collect::<Vec<_>>() }),
                ))
            })
            .await
        }
        CommandLine::Run(args) => execute(RunKind::Implement, args.task, jsonl).await,
        CommandLine::Plan(args) => execute(RunKind::Plan, args.task, jsonl).await,
        CommandLine::Audit(args) => execute_optional(RunKind::Audit, args.task, jsonl).await,
        CommandLine::Review(args) => execute_optional(RunKind::Review, args.task, jsonl).await,
        CommandLine::Answer(args) => answer(args).await,
        CommandLine::Pickup(args) => pickup(args).await,
        CommandLine::Status(args) => inspect(args, InspectKind::Status).await,
        CommandLine::Usage(args) => inspect(args, InspectKind::Usage).await,
        CommandLine::Events(args) => inspect(args, InspectKind::Events).await,
        CommandLine::Show(args) => inspect(args, InspectKind::Show).await,
        CommandLine::Sessions => sessions().await,
        CommandLine::Resume(args) => resume(args, jsonl).await,
        CommandLine::Fork(args) => fork(args).await,
        CommandLine::Rename(args) => rename(args).await,
        CommandLine::Archive(args) => archive(args).await,
        CommandLine::Diff => match GitRepo::new(current_dir()).diff() {
            Ok(diff) => success(ExitState::Succeeded, json!({"diff": diff})),
            Err(error) => failure(error.to_string()),
        },
        CommandLine::Doctor => doctor().await,
        CommandLine::Version => success(
            ExitState::Succeeded,
            json!({
                "version": env!("CARGO_PKG_VERSION"), "root": current_dir()
            }),
        ),
        CommandLine::Update(args) => update(args),
    }
}

fn update(args: UpdateArgs) -> ResultData {
    match minha_core::update::check_or_update(&minha_core::update::UpdateOptions {
        repository: args.repo,
        check: args.check,
    }) {
        Ok(result) => success(ExitState::Succeeded, json!(result)),
        Err(error) => failure(error),
    }
}

async fn tui(_json_output: bool) -> ResultData {
    match Harness::open(current_dir()) {
        Ok(harness) => match minha_tui::run(harness).await {
            Ok(()) => success(ExitState::Succeeded, json!({"mode": "tui"})),
            Err(error) => failure(error.to_string()),
        },
        Err(error) => harness_error(error),
    }
}

async fn login(args: LoginArgs, json_output: bool, jsonl: bool) -> ResultData {
    if matches!(args.command.as_ref(), Some(LoginCommand::Status)) {
        return match load_default_auth().await {
            Ok(Some(auth)) => {
                let active = active_account_profile().await.ok().flatten();
                success(
                    ExitState::Succeeded,
                    json!({
                        "authenticated": true, "account_id_present": auth.account_id.is_some(),
                        "email": auth.email, "active_profile": active
                    }),
                )
            }
            Ok(None) => ResultData {
                code: EXIT_AUTH,
                state: ExitState::AuthUnavailable,
                data: json!({"authenticated": false}),
                error: Some("login required; run `minha login`".into()),
            },
            Err(error) => auth_error(error.to_string()),
        };
    }
    match args.command.as_ref() {
        Some(LoginCommand::List) => {
            return match list_account_profiles().await {
                Ok(profiles) => success(ExitState::Succeeded, json!({"profiles": profiles})),
                Err(error) => auth_error(error.to_string()),
            };
        }
        Some(LoginCommand::Use { name }) => {
            return match set_active_account_profile(name).await {
                Ok(()) => success(ExitState::Succeeded, json!({"active_profile": name})),
                Err(error) => auth_error(error.to_string()),
            };
        }
        Some(LoginCommand::Enable { name }) => {
            return match set_account_profile_enabled(name, true).await {
                Ok(()) => success(ExitState::Succeeded, json!({"profile": name, "enabled": true})),
                Err(error) => auth_error(error.to_string()),
            };
        }
        Some(LoginCommand::Disable { name }) => {
            return match set_account_profile_enabled(name, false).await {
                Ok(()) => success(ExitState::Succeeded, json!({"profile": name, "enabled": false})),
                Err(error) => auth_error(error.to_string()),
            };
        }
        Some(LoginCommand::Remove { name }) => {
            return match remove_account_profile(name).await {
                Ok(removed) => success(ExitState::Succeeded, json!({"profile": name, "removed": removed})),
                Err(error) => auth_error(error.to_string()),
            };
        }
        Some(LoginCommand::Status) => {
            return auth_error("could not resolve login status".into());
        }
        None => {}
    }
    let client = match CodexOAuthClient::new(openai_oauth_config()) {
        Ok(client) => client,
        Err(error) => return auth_error(error.to_string()),
    };
    let device = match client.begin_device_authorization().await {
        Ok(device) => device,
        Err(error) => return auth_error(error.to_string()),
    };
    let data = json!({"verification_uri": device.verification_uri, "user_code": device.user_code});
    if jsonl {
        let encoded = match serde_json::to_string(&Envelope {
            ok: true,
            state: "awaiting_authorization".into(),
            data: data.clone(),
            error: None,
        }) {
            Ok(encoded) => encoded,
            Err(error) => return failure(format!("could not serialize login prompt: {error}")),
        };
        println!("{encoded}");
    } else if json_output {
        eprintln!(
            "Open {} and enter code {}.",
            device.verification_uri, device.user_code
        );
    } else {
        println!(
            "Open {} and enter code {}.",
            device.verification_uri, device.user_code
        );
    }
    match client.complete_device_authorization(&device).await {
        Ok(auth) if auth.account_id.is_some() => {
            let label = args.label.as_deref().unwrap_or(&args.profile);
            match save_account_profile(&args.profile, label, &auth, true).await {
                Ok(()) => success(
                    ExitState::Succeeded,
                    json!({"authenticated": true, "account_id_present": true, "profile": args.profile}),
                ),
                Err(error) => auth_error(error.to_string()),
            }
        }
        Ok(_) => auth_error("login response did not include a ChatGPT account id".into()),
        Err(error) => auth_error(error.to_string()),
    }
}

async fn execute(kind: RunKind, task: String, jsonl: bool) -> ResultData {
    with_harness(|h| async move {
        let result = if jsonl {
            run_jsonl(h, move |harness| async move { harness.run(kind, &task).await }).await
        } else {
            h.run(kind, &task).await
        };
        Ok(outcome(result))
    })
    .await
}

async fn execute_optional(kind: RunKind, task: Option<String>, jsonl: bool) -> ResultData {
    with_harness(|h| async move {
        let task = task.unwrap_or_else(|| match kind {
            RunKind::Audit => "Audit the current repository comprehensively.".into(),
            RunKind::Review => "Review the current repository diff.".into(),
            _ => "Inspect the current repository.".into(),
        });
        let result = if jsonl {
            run_jsonl(h, move |harness| async move { harness.run(kind, &task).await }).await
        } else {
            h.run(kind, &task).await
        };
        Ok(outcome(result))
    })
    .await
}

async fn run_jsonl<F, Fut>(harness: Harness, operation: F) -> Result<RunOutcome, HarnessError>
where
    F: FnOnce(Harness) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<RunOutcome, HarnessError>> + Send + 'static,
{
    let mut events = harness.store.subscribe();
    let mut task = Box::pin(tokio::spawn(operation(harness)));
    loop {
        tokio::select! {
            result = events.recv() => {
                if let Ok(event) = result {
                    println!("{}", serde_json::to_string(&event).unwrap_or_else(|_| "{\"type\":\"serialization_error\"}".into()));
                }
            }
            result = &mut task => {
                while let Ok(event) = events.try_recv() {
                    println!("{}", serde_json::to_string(&event).unwrap_or_else(|_| "{\"type\":\"serialization_error\"}".into()));
                }
                return result.map_err(|error| HarnessError::Io(std::io::Error::other(error.to_string())))?;
            }
        }
    }
}

async fn answer(args: AnswerArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref())?;
        Ok(outcome(h.resume_with_answer(run.id, &args.text).await))
    })
    .await
}

async fn sessions() -> ResultData {
    with_harness(|h| async move {
        Ok(success(
            ExitState::Succeeded,
            json!({"sessions": h.store.list_runs(100)?}),
        ))
    })
    .await
}

async fn resume(args: ResumeArgs, jsonl: bool) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.id.as_deref())?;
        let result = if let Some(prompt) = args.prompt {
            if jsonl {
                run_jsonl(h, move |harness| async move {
                    harness.continue_session(run.id, &prompt).await
                })
                .await
            } else {
                h.continue_session(run.id, &prompt).await
            }
        } else if run.state == ExitState::UsagePaused {
            h.resume_paused(run.id).await
        } else {
            return Ok(success(
                run.state,
                json!({"run": run, "messages": h.store.messages(run.id)?, "events": h.store.events(run.id)?}),
            ));
        };
        Ok(outcome(result))
    })
    .await
}

async fn fork(args: RunArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref().or(args.id.as_deref()))?;
        let fork = h.store.fork_run(run.id)?;
        Ok(success(ExitState::Pending, json!({"session": fork})))
    })
    .await
}

async fn rename(args: RenameArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref())?;
        h.store.rename_run(run.id, &args.title)?;
        Ok(success(run.state, json!({"session": h.store.run(run.id)?})))
    })
    .await
}

async fn archive(args: RunArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref().or(args.id.as_deref()))?;
        h.store.archive_run(run.id)?;
        Ok(success(run.state, json!({"archived": run.id})))
    })
    .await
}

async fn pickup(args: RunArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref().or(args.id.as_deref()))?;
        if run.state == ExitState::UsagePaused {
            return Ok(outcome(h.resume_paused(run.id).await));
        }
        if run.state == ExitState::NeedsInput {
            return Ok(ResultData {
                code: EXIT_BLOCKED,
                state: ExitState::NeedsInput,
                data: json!({"run_id": run.id, "question": run.pending_question}),
                error: Some("pending question requires an answer; use `minha answer TEXT`".into()),
            });
        }
        blocked("pickup applies only to a run needing input or paused by the usage reserve")
    })
    .await
}

enum InspectKind {
    Status,
    Usage,
    Events,
    Show,
}

async fn inspect(args: RunArgs, kind: InspectKind) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref().or(args.id.as_deref()))?;
        let data = match kind {
            InspectKind::Status => {
                let usage = h.store.usage_totals(Some(run.id))?;
                let cache = h.store.cache_totals(h.workspace_id())?;
                let (active_agents, open_tasks, blocked_tasks) = h.store.office_health(run.id)?;
                let accounts = list_account_profiles().await?;
                let active_account = active_account_profile().await?;
                json!({
                    "run": run,
                    "usage": usage,
                    "cache": cache,
                    "office": {
                        "active_agents": active_agents,
                        "open_tasks": open_tasks,
                        "blocked_tasks": blocked_tasks,
                    },
                    "books": {"indexed": h.store.indexed_book_count()?},
                    "accounts": {"active": active_account, "profiles": accounts},
                })
            }
            InspectKind::Usage => {
                let events = h.store.events(run.id)?;
                let account = events
                    .iter()
                    .rev()
                    .find(|event| {
                        matches!(
                            event.event,
                            minha_core::protocol::RuntimeEvent::AccountUsage { .. }
                        )
                    })
                    .map(minha_core::protocol::EventEnvelope::payload);
                json!({"run_id": run.id, "tokens": h.store.usage_totals(Some(run.id))?, "account": account})
            }
            InspectKind::Events => json!({"run_id": run.id, "events": h.store.events(run.id)?}),
            InspectKind::Show => {
                json!({"run": run, "messages": h.store.messages(run.id)?, "events": h.store.events(run.id)?})
            }
        };
        Ok(success(run.state, data))
    })
    .await
}

async fn doctor() -> ResultData {
    let root = current_dir();
    let git = GitRepo::new(&root).is_inside_work_tree();
    let rg = Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    let auth = default_auth_status().await.unwrap_or(false);
    let config = Config::discover(&root).is_ok();
    let store = Config::discover(&root)
        .ok()
        .and_then(|c| Store::open(c.database_path).ok())
        .is_some();
    let healthy = git && rg && config && store;
    let data = json!({"healthy": healthy, "ready_for_model_calls": healthy && auth, "git_repository": git, "rg": rg, "authenticated": auth, "config": config, "store": store});
    if healthy {
        success(ExitState::Succeeded, data)
    } else {
        ResultData {
            code: EXIT_ERROR,
            state: ExitState::Failed,
            data,
            error: Some("one or more local prerequisites are unavailable".into()),
        }
    }
}

async fn with_harness<F, Fut>(f: F) -> ResultData
where
    F: FnOnce(Harness) -> Fut,
    Fut: std::future::Future<Output = Result<ResultData, HarnessError>>,
{
    match Harness::open(current_dir()) {
        Ok(harness) => match f(harness).await {
            Ok(result) => result,
            Err(error) => harness_error(error),
        },
        Err(error) => harness_error(error),
    }
}

fn selected_run(store: &Store, id: Option<&str>) -> Result<minha_core::store::RunRecord, HarnessError> {
    match id {
        Some(id) => {
            let id = id.parse::<RunId>().map_err(|error| {
                HarnessError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    error.to_string(),
                ))
            })?;
            store.run(id)?
        }
        None => store.latest_run()?,
    }
    .ok_or_else(|| HarnessError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "run not found")))
}

fn outcome(result: Result<RunOutcome, HarnessError>) -> ResultData {
    match result {
        Ok(outcome) => ResultData {
            code: code_for_state(outcome.state),
            state: outcome.state,
            data: json!(outcome),
            error: None,
        },
        Err(error) => harness_error(error),
    }
}

fn harness_error(error: HarnessError) -> ResultData {
    let state = match error {
        HarnessError::LoginRequired | HarnessError::MissingAccountId | HarnessError::Auth(_) => {
            ExitState::AuthUnavailable
        }
        HarnessError::ModelUnavailable(_) => ExitState::ModelUnavailable,
        _ => ExitState::Failed,
    };
    let code = code_for_state(state);
    ResultData {
        code,
        state,
        data: json!({}),
        error: Some(error.to_string()),
    }
}

fn success(state: ExitState, data: Value) -> ResultData {
    ResultData {
        code: code_for_state(state),
        state,
        data,
        error: None,
    }
}
fn failure(error: String) -> ResultData {
    ResultData {
        code: EXIT_ERROR,
        state: ExitState::Failed,
        data: json!({}),
        error: Some(error),
    }
}
fn auth_error(error: String) -> ResultData {
    ResultData {
        code: EXIT_AUTH,
        state: ExitState::AuthUnavailable,
        data: json!({}),
        error: Some(error),
    }
}
fn blocked(error: &str) -> Result<ResultData, HarnessError> {
    Ok(ResultData {
        code: EXIT_BLOCKED,
        state: ExitState::Blocked,
        data: json!({}),
        error: Some(error.into()),
    })
}

fn code_for_state(state: ExitState) -> u8 {
    match state {
        ExitState::Succeeded => EXIT_OK,
        ExitState::AuthUnavailable => EXIT_AUTH,
        ExitState::Blocked
        | ExitState::Inconclusive
        | ExitState::NeedsInput
        | ExitState::ApprovalRequired => EXIT_BLOCKED,
        ExitState::Pending | ExitState::Running | ExitState::UsagePaused => EXIT_BLOCKED,
        _ => EXIT_ERROR,
    }
}

fn emit(json_output: bool, result: ResultData) -> ExitCode {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&Envelope {
                ok: result.code == EXIT_OK,
                state: state_name(result.state).into(),
                data: result.data,
                error: result.error
            })
            .unwrap_or_else(|_| {
                "{\"ok\":false,\"state\":\"failed\",\"data\":{},\"error\":\"serialization failure\"}".into()
            })
        );
    } else if let Some(error) = result.error {
        eprintln!("error: {error}");
    } else if let Some(text) = result.data.get("text").and_then(Value::as_str) {
        println!("{text}");
        if let Some(question) = result.data.get("question").and_then(Value::as_object) {
            if let Some(q) = question.get("question").and_then(Value::as_str) {
                println!("\nQuestion: {q}");
            }
            if let Some(options) = question.get("options").and_then(Value::as_array) {
                for option in options {
                    println!("  - {}", option.as_str().unwrap_or(""));
                }
            }
        }
    } else {
        println!("{}", result.data);
    }
    ExitCode::from(result.code)
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_exit_mapping() {
        assert_eq!(code_for_state(ExitState::Succeeded), EXIT_OK);
        assert_eq!(code_for_state(ExitState::NeedsInput), EXIT_BLOCKED);
        assert_eq!(code_for_state(ExitState::AuthUnavailable), EXIT_AUTH);
        assert_eq!(code_for_state(ExitState::Failed), EXIT_ERROR);
    }
    #[test]
    fn envelope_keeps_stable_error_field() {
        let encoded = serde_json::to_value(Envelope {
            ok: true,
            state: "succeeded".into(),
            data: json!({"x": 1}),
            error: None,
        })
        .expect("test operation should succeed");
        assert_eq!(
            encoded,
            json!({"ok": true, "state": "succeeded", "data": {"x": 1}, "error": null})
        );
    }
}
