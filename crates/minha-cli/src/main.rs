#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use clap::{Args, Parser, Subcommand, ValueEnum};
use minha_core::runtime::HarnessError;
use minha_core::{
    Config, ExitState, Harness, RunId, RunKind, RunOutcome, Store,
    auth::{
        CodexOAuthClient, active_account_profile, default_auth_status, list_account_profiles,
        load_default_auth, logout_default, openai_oauth_config, remove_account_profile, save_account_profile,
        set_account_profile_enabled, set_active_account_profile,
    },
    deepseek::{DEEPSEEK_PRICING_SOURCE, DEEPSEEK_PRICING_VERSION, DeepSeekClient, estimate_cost_usd},
    memory::{MemoryRecord, MemoryScope},
    mimo::{
        MIMO_PRICING_SOURCE, MIMO_PRICING_VERSION, MiMoClient, XIAOMI_MIMO_BASE_URL,
        estimate_cost_usd as estimate_mimo_cost_usd,
    },
    provider::DEEPSEEK_BASE_URL,
    provider_credentials::{
        default_path as provider_credentials_path, load_deepseek_key, load_xiaomi_mimo, remove_deepseek,
        remove_xiaomi_mimo, save_deepseek_key, save_xiaomi_mimo,
    },
    store::state_name,
    worktree::GitRepo,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
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
    /// Add, list, test, or remove direct model providers.
    Provider(ProviderArgs),
    /// Search, inspect, correct, pin, or delete durable memory.
    Memory(MemoryArgs),
    /// Show or change project memory generation and retrieval controls.
    Memories(MemoriesArgs),
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
    /// Stage a supplied, bounded maintenance patch for human review; never applies or publishes it.
    Maintain(MaintainArgs),
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
    #[arg(value_name = "TEXT", help = "Answer to the pending single question")]
    text: Option<String>,
    #[arg(
        long = "answer",
        value_name = "ID=VALUE",
        action = clap::ArgAction::Append,
        help = "Answer one issue-clarification question; repeat for a batch"
    )]
    answers: Vec<String>,
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
struct MaintainArgs {
    /// A supplied unified patch. Minha only stages it for review; it never generates or applies a patch here.
    #[arg(long, value_name = "PATCH")]
    patch: PathBuf,
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

#[derive(Args, Debug)]
struct ProviderArgs {
    #[command(subcommand)]
    command: ProviderCommand,
}

#[derive(Subcommand, Debug)]
enum ProviderCommand {
    /// Add a provider credential using a no-echo prompt.
    Add {
        name: String,
        /// HTTPS base URL for a Xiaomi MiMo Token Plan or custom console endpoint.
        #[arg(long)]
        base_url: Option<String>,
    },
    /// List configured providers without exposing credentials.
    List,
    /// Test provider authentication without making a model generation request.
    Test { name: String },
    /// Remove a provider credential.
    Remove { name: String },
}

#[derive(Args, Debug)]
struct MemoryArgs {
    #[command(subcommand)]
    command: MemoryCommand,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MemoryScopeArg {
    User,
    Project,
    Run,
}

#[derive(Subcommand, Debug)]
enum MemoryCommand {
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        run: Option<String>,
    },
    Inspect {
        id: String,
    },
    Add {
        #[arg(long, value_enum, default_value_t = MemoryScopeArg::Project)]
        scope: MemoryScopeArg,
        subject: String,
        body: String,
        #[arg(long)]
        run: Option<String>,
    },
    Pin {
        id: String,
    },
    Correct {
        id: String,
        body: String,
    },
    Supersede {
        id: String,
        subject: String,
        body: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Args, Debug)]
struct MemoriesArgs {
    #[arg(long, value_name = "BOOL")]
    enabled: Option<bool>,
    #[arg(long = "use", value_name = "BOOL")]
    use_memory: Option<bool>,
    #[arg(long, value_name = "BOOL")]
    generate: Option<bool>,
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
    if jsonl {
        // The stream already ended with a typed run_complete event; only the
        // exit code remains.
        return ExitCode::from(result.code);
    }
    emit(json_output, result)
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
                let mut available = models
                    .iter()
                    .map(|model| {
                        json!({
                            "provider":"chatgpt_codex", "slug":model.slug,
                            "capabilities":model.capabilities()
                        })
                    })
                    .collect::<Vec<_>>();
                if let Some(path) = provider_credentials_path()
                    && load_deepseek_key(&path).ok().flatten().is_some()
                {
                    available.extend(["deepseek-v4-flash", "deepseek-v4-pro"].map(|slug| {
                        json!({
                            "provider":"deepseek", "slug":slug, "context_window":1_048_576_u64
                        })
                    }));
                }
                if let Some(path) = provider_credentials_path()
                    && load_xiaomi_mimo(&path).ok().flatten().is_some()
                {
                    available.extend(["mimo-v2.5", "mimo-v2.5-pro"].map(|slug| {
                        json!({
                            "provider":"xiaomi_mimo", "slug":slug, "context_window":1_048_576_u64,
                            "quota":"unavailable_by_api"
                        })
                    }));
                }
                Ok(success(ExitState::Succeeded, json!({"models": available})))
            })
            .await
        }
        CommandLine::Provider(args) => provider(args).await,
        CommandLine::Memory(args) => memory(args).await,
        CommandLine::Memories(args) => memories(args).await,
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
        CommandLine::Maintain(args) => maintain(args),
    }
}

async fn provider(args: ProviderArgs) -> ResultData {
    let Some(path) = provider_credentials_path() else {
        return failure("could not determine the user configuration directory".into());
    };
    match args.command {
        ProviderCommand::Add { name, .. } if name.eq_ignore_ascii_case("deepseek") => {
            let key = match rpassword::prompt_password("DeepSeek API key: ") {
                Ok(key) => key,
                Err(error) => return failure(format!("could not read API key: {error}")),
            };
            match save_deepseek_key(&path, &key) {
                Ok(()) => success(
                    ExitState::Succeeded,
                    json!({"provider":"deepseek","configured":true}),
                ),
                Err(error) => failure(error.to_string()),
            }
        }
        ProviderCommand::Add { name, base_url } if is_xiaomi_mimo_name(&name) => {
            let key = match rpassword::prompt_password("Xiaomi MiMo API key: ") {
                Ok(key) => key,
                Err(error) => return failure(format!("could not read API key: {error}")),
            };
            if key.trim_start().starts_with("tp-") && base_url.is_none() {
                return blocked_data(
                    "MiMo Token Plan keys require --base-url; copy the region-specific HTTPS endpoint from the Xiaomi console",
                );
            }
            match save_xiaomi_mimo(&path, &key, base_url.as_deref()) {
                Ok(()) => success(
                    ExitState::Succeeded,
                    json!({
                        "provider":"xiaomi_mimo",
                        "configured":true,
                        "base_url": base_url.unwrap_or_else(|| XIAOMI_MIMO_BASE_URL.into()),
                        "quota":"unavailable_by_api"
                    }),
                ),
                Err(error) => failure(error.to_string()),
            }
        }
        ProviderCommand::List => match (load_deepseek_key(&path), load_xiaomi_mimo(&path)) {
            (Ok(deepseek), Ok(mimo)) => success(
                ExitState::Succeeded,
                json!({
                    "providers":[
                        {"id":"chatgpt_codex","authentication":"oauth"},
                        {"id":"deepseek","authentication":"api_key","configured":deepseek.is_some()},
                        {
                            "id":"xiaomi_mimo",
                            "authentication":"api_key",
                            "configured":mimo.is_some(),
                            "base_url":mimo.as_ref().map(|credential| credential.base_url.as_str()),
                            "quota":"unavailable_by_api"
                        }
                    ]
                }),
            ),
            (Err(error), _) | (_, Err(error)) => failure(error.to_string()),
        },
        ProviderCommand::Test { name } if name.eq_ignore_ascii_case("deepseek") => {
            let key = match load_deepseek_key(&path) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    return blocked_data("DeepSeek is not configured; run `minha provider add deepseek`");
                }
                Err(error) => return failure(error.to_string()),
            };
            let client = DeepSeekClient::new(DEEPSEEK_BASE_URL, key);
            match client.test_connection().await {
                Ok(()) => match client.fetch_balance().await {
                    Ok(balance) => success(
                        ExitState::Succeeded,
                        json!({"provider":"deepseek","healthy":true,"balance":balance}),
                    ),
                    Err(error) => success(
                        ExitState::Succeeded,
                        json!({"provider":"deepseek","healthy":true,"balance_error":error.to_string()}),
                    ),
                },
                Err(error) => failure(error.to_string()),
            }
        }
        ProviderCommand::Remove { name } if name.eq_ignore_ascii_case("deepseek") => {
            match remove_deepseek(&path) {
                Ok(removed) => success(
                    ExitState::Succeeded,
                    json!({"provider":"deepseek","removed":removed}),
                ),
                Err(error) => failure(error.to_string()),
            }
        }
        ProviderCommand::Test { name } if is_xiaomi_mimo_name(&name) => {
            let credential = match load_xiaomi_mimo(&path) {
                Ok(Some(credential)) => credential,
                Ok(None) => {
                    return blocked_data("Xiaomi MiMo is not configured; run `minha provider add xiaomi`");
                }
                Err(error) => return failure(error.to_string()),
            };
            let client = MiMoClient::new(credential.base_url, credential.api_key);
            match client.test_connection().await {
                Ok(()) => success(
                    ExitState::Succeeded,
                    json!({
                        "provider":"xiaomi_mimo",
                        "healthy":true,
                        "quota":"unavailable_by_api",
                        "quota_detail":"Provider does not expose remaining quota by API"
                    }),
                ),
                Err(error) => failure(error.to_string()),
            }
        }
        ProviderCommand::Remove { name } if is_xiaomi_mimo_name(&name) => match remove_xiaomi_mimo(&path) {
            Ok(removed) => success(
                ExitState::Succeeded,
                json!({"provider":"xiaomi_mimo","removed":removed}),
            ),
            Err(error) => failure(error.to_string()),
        },
        ProviderCommand::Add { name, .. }
        | ProviderCommand::Test { name }
        | ProviderCommand::Remove { name } => blocked_data(&format!("unsupported provider `{name}`")),
    }
}

fn is_xiaomi_mimo_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "xiaomi" | "mimo" | "xiaomi_mimo"
    )
}

async fn memory(args: MemoryArgs) -> ResultData {
    with_harness(|h| async move {
        let data = match args.command {
            MemoryCommand::Search { query, limit, run } => {
                let run = run
                    .as_deref()
                    .map(str::parse::<RunId>)
                    .transpose()
                    .map_err(|error| {
                        HarnessError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
                    })?;
                json!({"query":query,"hits":h.store.search_memories(h.workspace_id(), run, &query, limit)?})
            }
            MemoryCommand::Inspect { id } => {
                json!({"memory":h.store.memory(&id)?})
            }
            MemoryCommand::Add {
                scope,
                subject,
                body,
                run,
            } => {
                let scope = match scope {
                    MemoryScopeArg::User => MemoryScope::User,
                    MemoryScopeArg::Project => MemoryScope::Project,
                    MemoryScopeArg::Run => MemoryScope::Run,
                };
                let run_id = run
                    .as_deref()
                    .map(str::parse::<RunId>)
                    .transpose()
                    .map_err(|error| {
                        HarnessError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
                    })?;
                if scope == MemoryScope::Run && run_id.is_none() {
                    return Ok(blocked_data("run-scoped memory requires --run UUID"));
                }
                let mut record = MemoryRecord::candidate(scope, "user_correction", subject, body);
                record.workspace_id = (scope != MemoryScope::User).then(|| h.workspace_id().to_owned());
                record.run_id = run_id;
                record.pinned = true;
                record.confidence = 100;
                record.provenance = vec!["user:explicit".into()];
                let record = h.store.put_memory(record)?;
                if let Some(run_id) = record.run_id {
                    h.store.record_runtime_event(
                        run_id,
                        minha_core::protocol::RuntimeEvent::MemoryChanged {
                            memory_id: record.id.clone(),
                            action: "added".into(),
                            scope: record.scope.as_str().into(),
                        },
                    )?;
                }
                json!({"memory":record})
            }
            MemoryCommand::Pin { id } => {
                let changed = h.store.set_memory_state(&id, Some(true), None)?;
                json!({"id":id,"pinned":changed})
            }
            MemoryCommand::Correct { id, body } => {
                let Some(previous) = h.store.memory(&id)? else {
                    return Ok(blocked_data("memory not found"));
                };
                let record = corrected_memory(previous, None, body);
                let record = h.store.put_memory(record)?;
                json!({"memory":record})
            }
            MemoryCommand::Supersede { id, subject, body } => {
                let Some(previous) = h.store.memory(&id)? else {
                    return Ok(blocked_data("memory not found"));
                };
                let record = corrected_memory(previous, Some(subject), body);
                let record = h.store.put_memory(record)?;
                json!({"memory":record})
            }
            MemoryCommand::Delete { id } => {
                let changed = h.store.set_memory_state(&id, None, Some(true))?;
                json!({"id":id,"deleted":changed})
            }
        };
        Ok(success(ExitState::Succeeded, data))
    })
    .await
}

fn corrected_memory(previous: MemoryRecord, subject: Option<String>, body: String) -> MemoryRecord {
    let mut record = MemoryRecord::candidate(
        previous.scope,
        "user_correction",
        subject.unwrap_or_else(|| previous.subject.clone()),
        body,
    );
    record.workspace_id = previous.workspace_id;
    record.run_id = previous.run_id;
    record.pinned = true;
    record.confidence = 100;
    record.salience = previous.salience.max(75);
    record.entities = previous.entities;
    record.provenance = vec!["user:explicit_correction".into()];
    record.supersedes_id = Some(previous.id);
    record
}

async fn memories(args: MemoriesArgs) -> ResultData {
    with_harness(|h| async move {
        let mut settings = h.store.memory_settings(h.workspace_id())?;
        let changed = args.enabled.is_some() || args.use_memory.is_some() || args.generate.is_some();
        if let Some(enabled) = args.enabled {
            settings.enabled = enabled;
        }
        if let Some(use_memory) = args.use_memory {
            settings.use_memory = use_memory;
        }
        if let Some(generate) = args.generate {
            settings.generate = generate;
        }
        if changed {
            h.store.set_memory_settings(h.workspace_id(), settings)?;
        }
        Ok(success(
            ExitState::Succeeded,
            json!({
                "settings": settings,
                "configuration": {
                    "enabled": h.config.memory.enabled,
                    "use_memory": h.config.memory.use_memory,
                    "generate": h.config.memory.generate,
                    "retrieval_limit": h.config.memory.retrieval_limit,
                },
                "effective": {
                    "enabled": settings.enabled && h.config.memory.enabled,
                    "use_memory": settings.use_memory && h.config.memory.use_memory,
                    "generate": settings.generate && h.config.memory.generate,
                }
            }),
        ))
    })
    .await
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

const MAX_MAINTENANCE_PATCH_BYTES: usize = 512 * 1024;

/// The maintenance command is intentionally a reviewable staging lane, not a
/// self-modifying agent. It accepts only a patch supplied by the human, rejects
/// high-risk domains, and writes the unchanged patch into a private local
/// review directory. There is no model call, apply, commit, push, or release.
fn maintain(args: MaintainArgs) -> ResultData {
    match stage_maintenance_patch(&current_dir(), &args.patch) {
        Ok(path) => success(
            ExitState::Succeeded,
            json!({
                "schema_version": 1,
                "staged_patch": path,
                "applied": false,
                "next_step": "review and apply the patch manually if it is acceptable"
            }),
        ),
        Err(error) => blocked_data(&error),
    }
}

fn stage_maintenance_patch(root: &Path, supplied: &Path) -> Result<PathBuf, String> {
    let root = fs::canonicalize(root).map_err(|error| format!("could not resolve workspace: {error}"))?;
    let supplied =
        fs::canonicalize(supplied).map_err(|error| format!("could not resolve supplied patch: {error}"))?;
    if !supplied.is_file() {
        return Err("supplied maintenance patch is not a regular file".into());
    }
    let metadata = fs::metadata(&supplied).map_err(|error| format!("could not inspect patch: {error}"))?;
    if metadata.len() == 0 || metadata.len() > MAX_MAINTENANCE_PATCH_BYTES as u64 {
        return Err(format!(
            "supplied maintenance patch must be 1..={MAX_MAINTENANCE_PATCH_BYTES} bytes"
        ));
    }
    let patch = fs::read_to_string(&supplied)
        .map_err(|error| format!("could not read supplied patch as UTF-8 text: {error}"))?;
    validate_maintenance_patch(&patch)?;
    let staging = root.join(".minha").join("maintenance");
    fs::create_dir_all(&staging).map_err(|error| format!("could not create maintenance staging: {error}"))?;
    let staged = staging.join(format!("{}.patch", uuid::Uuid::now_v7()));
    fs::write(&staged, patch).map_err(|error| format!("could not stage maintenance patch: {error}"))?;
    Ok(staged)
}

fn validate_maintenance_patch(patch: &str) -> Result<(), String> {
    let lower = patch.to_ascii_lowercase();
    const FORBIDDEN_CONTENT: &[&str] = &[
        "migrations/",
        "migration/",
        "create table",
        "alter table",
        "drop table",
        "pragma user_version",
        "api_key",
        "access_token",
        "refresh_token",
        "password",
        "private_key",
        "credentials",
        ".env",
        "git commit",
        "git push",
        "git merge",
        "git rebase",
        "git reset",
        "git tag",
        "gh pr",
        "gh release",
        "cargo publish",
        "npm publish",
        "release ",
        "publish ",
        "curl ",
        "wget ",
        "ssh ",
        "scp ",
        "rsync ",
    ];
    if let Some(marker) = FORBIDDEN_CONTENT.iter().find(|marker| lower.contains(**marker)) {
        return Err(format!(
            "maintenance patch touches a prohibited migration, credential, VCS, remote, or release surface ({marker})"
        ));
    }
    let mut saw_diff = false;
    for line in patch.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            saw_diff = true;
            for path in paths.split_whitespace().take(2) {
                validate_maintenance_patch_path(path)?;
            }
        } else if let Some(path) = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ ")) {
            let path = path
                .split('\t')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default();
            validate_maintenance_patch_path(path)?;
        } else if let Some(path) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("rename to "))
            .or_else(|| line.strip_prefix("copy from "))
            .or_else(|| line.strip_prefix("copy to "))
        {
            validate_maintenance_patch_path(path.trim())?;
        }
    }
    if !saw_diff {
        return Err("maintenance patch must contain a unified diff header".into());
    }
    Ok(())
}

fn validate_maintenance_patch_path(raw: &str) -> Result<(), String> {
    if raw == "/dev/null" {
        return Ok(());
    }
    let path = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    let candidate = Path::new(path);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|component| component == Component::ParentDir)
        || candidate
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
    {
        return Err(format!("maintenance patch path escapes the workspace: {raw}"));
    }
    if path.starts_with(".git/") || path.starts_with(".minha/") {
        return Err(format!(
            "maintenance patch may not modify protected local state: {raw}"
        ));
    }
    Ok(())
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
                match result {
                    Ok(event) => {
                        println!("{}", serde_json::to_string(&event).unwrap_or_else(|_| "{\"type\":\"serialization_error\"}".into()));
                    }
                    // The event channel closed while the run kept working;
                    // stop polling it instead of busy-spinning.
                    Err(_) => {
                        let outcome = task.await
                            .map_err(|error| HarnessError::Io(std::io::Error::other(error.to_string())))??;
                        print_run_complete(&outcome);
                        return Ok(outcome);
                    }
                }
            }
            result = &mut task => {
                while let Ok(event) = events.try_recv() {
                    println!("{}", serde_json::to_string(&event).unwrap_or_else(|_| "{\"type\":\"serialization_error\"}".into()));
                }
                let outcome = result
                    .map_err(|error| HarnessError::Io(std::io::Error::other(error.to_string())))??;
                print_run_complete(&outcome);
                return Ok(outcome);
            }
        }
    }
}

/// The JSONL stream is fully typed; the final outcome is emitted as a typed
/// event too so per-line consumers never see a differently-shaped envelope.
fn print_run_complete(outcome: &RunOutcome) {
    println!("{}", run_complete_event(outcome));
}

fn run_complete_event(outcome: &RunOutcome) -> serde_json::Value {
    serde_json::json!({
        "type": "run_complete",
        "run_id": outcome.run_id.to_string(),
        "state": state_name(outcome.state),
        "model": outcome.model,
        "text": outcome.text,
    })
}

async fn answer(args: AnswerArgs) -> ResultData {
    with_harness(|h| async move {
        let run = selected_run(&h.store, args.run.as_deref())?;
        if !args.answers.is_empty() {
            if args.text.is_some() {
                return blocked("use either positional TEXT or repeatable --answer ID=VALUE, not both");
            }
            let mut answers = Vec::with_capacity(args.answers.len());
            for encoded in args.answers {
                let Some((id, value)) = encoded.split_once('=') else {
                    return blocked("clarification answers must use --answer ID=VALUE");
                };
                if id.trim().is_empty() || value.trim().is_empty() {
                    return blocked("clarification answer IDs and values must not be empty");
                }
                answers.push((id.trim().to_owned(), value.trim().to_owned()));
            }
            return Ok(outcome(
                h.resume_with_clarification_answers(run.id, &answers).await,
            ));
        }
        let Some(text) = args.text else {
            return blocked("provide TEXT or at least one --answer ID=VALUE");
        };
        Ok(outcome(h.resume_with_answer(run.id, &text).await))
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
            let clarification = h.store.issue_clarification(run.id)?;
            return Ok(ResultData {
                code: EXIT_BLOCKED,
                state: ExitState::NeedsInput,
                data: json!({
                    "run_id": run.id,
                    "question": run.pending_question,
                    "clarification": clarification,
                }),
                error: Some(
                    "pending input requires `minha answer TEXT` or repeatable `minha answer --answer ID=VALUE`"
                        .into(),
                ),
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
                let events = h.store.events(run.id)?;
                let mut contexts = BTreeMap::new();
                let mut providers = BTreeMap::new();
                let mut last_incident = None;
                let mut catalog_fetched_at = None;
                let mut projected_next_deepseek_usd = 0.0;
                let mut projected_next_mimo_usd = 0.0;
                for event in &events {
                    match &event.event {
                        minha_core::protocol::RuntimeEvent::ContextUsage {
                            agent_id,
                            model,
                            forecast_tokens,
                            output_allowance,
                            ..
                        } => {
                            contexts.insert(agent_id.to_string(), event.payload());
                            projected_next_deepseek_usd +=
                                estimate_cost_usd(model, *forecast_tokens, 0, *output_allowance)
                                    .unwrap_or(0.0);
                            projected_next_mimo_usd += estimate_mimo_cost_usd(
                                model,
                                *forecast_tokens,
                                0,
                                *output_allowance,
                            )
                            .unwrap_or(0.0);
                        }
                        minha_core::protocol::RuntimeEvent::ProviderState { provider, .. } => {
                            providers.insert(provider.clone(), event.payload());
                        }
                        minha_core::protocol::RuntimeEvent::Incident { .. } => {
                            last_incident = Some(event.payload());
                        }
                        minha_core::protocol::RuntimeEvent::ModelCatalog { fetched_at, .. } => {
                            catalog_fetched_at = Some(*fetched_at);
                        }
                        _ => {}
                    }
                }
                let todo_rollup = h.store.todo_rollup(run.id)?;
                let todo_details = h.store.todo_rollup_details(run.id, 3)?;
                let memory = h.store.memory_settings(h.workspace_id())?;
                let deepseek_cost = h.store.deepseek_cost_totals(Some(run.id))?;
                let mimo_cost = h.store.xiaomi_mimo_cost_totals(Some(run.id))?;
                json!({
                    "run": run,
                    "usage": usage,
                    "contexts": contexts,
                    "cache": cache,
                    "cache_hit_ratio": if cache.hits + cache.misses == 0 { 0.0 } else { cache.hits as f64 / (cache.hits + cache.misses) as f64 },
                    "office": {
                        "active_agents": active_agents,
                        "open_tasks": open_tasks,
                        "blocked_tasks": blocked_tasks,
                    },
                    "todos": {
                        "active": todo_rollup.0,
                        "blocked": todo_rollup.1,
                        "completed": todo_rollup.2,
                        "stale_agents": todo_rollup.3,
                        "active_goals": todo_details.active_goals,
                        "blocked_work": todo_details.blocked_work,
                        "recently_completed": todo_details.recently_completed,
                    },
                    "providers": providers,
                    "deepseek_cost": {
                        "estimated_usd": deepseek_cost.estimated_usd,
                        "projected_usd": deepseek_cost.estimated_usd + projected_next_deepseek_usd,
                        "projected_next_turn_assumption": "current per-agent forecast, cache miss, full output allowance",
                        "cache_savings_usd": deepseek_cost.cache_savings_usd,
                        "priced_turns": deepseek_cost.priced_turns,
                        "unpriced_turns": deepseek_cost.unpriced_turns,
                        "pricing_version": DEEPSEEK_PRICING_VERSION,
                        "pricing_source": DEEPSEEK_PRICING_SOURCE,
                    },
                    "xiaomi_mimo_cost": {
                        "estimated_usd": mimo_cost.estimated_usd,
                        "projected_usd": mimo_cost.estimated_usd + projected_next_mimo_usd,
                        "projected_next_turn_assumption": "current per-agent forecast, cache miss, full output allowance",
                        "cache_savings_usd": mimo_cost.cache_savings_usd,
                        "priced_turns": mimo_cost.priced_turns,
                        "unpriced_turns": mimo_cost.unpriced_turns,
                        "pricing_version": MIMO_PRICING_VERSION,
                        "pricing_source": MIMO_PRICING_SOURCE,
                        "quota": "unavailable_by_api",
                    },
                    "memory": memory,
                    "last_incident": last_incident,
                    "model_catalog_fetched_at": catalog_fetched_at,
                    "configuration_sources": {
                        "project": current_dir().join("minha.toml"),
                        "user": dirs::config_dir().map(|path| path.join("minha/config.toml")),
                    },
                    "books": {"indexed": h.store.indexed_book_count()?},
                    "accounts": {"active": active_account, "profiles": accounts},
                    "clarification": h.store.issue_clarification(run.id)?,
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
                json!({
                    "run": run,
                    "messages": h.store.messages(run.id)?,
                    "events": h.store.events(run.id)?,
                    "clarification": h.store.issue_clarification(run.id)?,
                })
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
    Ok(blocked_data(error))
}

fn blocked_data(error: &str) -> ResultData {
    ResultData {
        code: EXIT_BLOCKED,
        state: ExitState::Blocked,
        data: json!({}),
        error: Some(error.into()),
    }
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
        if let Some(clarification) = result.data.get("clarification").filter(|value| !value.is_null()) {
            print_clarification(clarification);
        } else if let Some(question) = result.data.get("question").and_then(Value::as_object) {
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

fn print_clarification(clarification: &Value) {
    let ambiguity = clarification
        .pointer("/meter/overall")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let status = clarification
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("collecting");
    println!("\nIssue Clarifier · ambiguity {ambiguity}/100 · {status}");

    let questions = clarification
        .pointer("/pending_batch/questions")
        .and_then(Value::as_array);
    if let Some(questions) = questions {
        for question in questions {
            let id = question.get("id").and_then(Value::as_str).unwrap_or("question");
            let prompt = question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("What should Minha know?");
            println!("\n[{id}] {prompt}");
            if let Some(options) = question.get("options").and_then(Value::as_array) {
                for option in options {
                    let value = option.get("value").and_then(Value::as_str).unwrap_or_default();
                    let label = option.get("label").and_then(Value::as_str).unwrap_or(value);
                    let recommended = option
                        .get("recommended")
                        .and_then(Value::as_bool)
                        .is_some_and(|recommended| recommended);
                    println!(
                        "  - {value} = {label}{}",
                        if recommended { " (recommended)" } else { "" }
                    );
                }
            }
            println!("  - Not sure, or write your own answer");
        }
        println!("\nReply with `minha answer --answer ID=VALUE` (repeat --answer for the batch).");
    } else if status == "reviewing" {
        println!("Reply with `minha answer confirm`, `edit`, `keep clarifying`, or `cancel`.");
    }
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

    #[test]
    fn answer_parser_accepts_repeatable_clarification_fields() {
        let cli = Cli::try_parse_from([
            "minha",
            "answer",
            "--answer",
            "goal-1=wrong result",
            "--answer",
            "scope-1=tui",
        ])
        .expect("repeatable answer syntax");

        let Some(CommandLine::Answer(args)) = cli.command else {
            panic!("answer subcommand");
        };
        assert_eq!(
            args.answers,
            ["goal-1=wrong result".to_owned(), "scope-1=tui".to_owned()]
        );
        assert!(args.text.is_none());
    }

    #[test]
    fn jsonl_final_event_is_typed_run_complete() {
        let outcome = RunOutcome {
            run_id: RunId::new(),
            state: ExitState::Succeeded,
            kind: RunKind::Implement,
            model: Some("gpt-5.6-spark".into()),
            text: "done".into(),
            question: None,
            clarification: None,
            usage: Default::default(),
            agents_used: 1,
            worktrees: Vec::new(),
        };
        assert_eq!(
            run_complete_event(&outcome),
            json!({
                "type": "run_complete",
                "run_id": outcome.run_id.to_string(),
                "state": "succeeded",
                "model": "gpt-5.6-spark",
                "text": "done",
            })
        );
    }

    #[test]
    fn maintenance_stages_only_a_supplied_safe_patch() {
        let workspace = tempfile::tempdir().expect("workspace");
        let supplied = tempfile::NamedTempFile::new().expect("supplied patch");
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n";
        fs::write(supplied.path(), patch).expect("write patch");
        let staged = stage_maintenance_patch(workspace.path(), supplied.path()).expect("stage patch");
        let staging = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .join(".minha/maintenance");
        assert!(staged.starts_with(staging));
        assert_eq!(fs::read_to_string(staged).expect("read staged patch"), patch);
    }

    #[test]
    fn maintenance_rejects_unsafe_or_non_diff_supplied_patches() {
        for patch in [
            "not a patch\n",
            "diff --git a/.env b/.env\n--- a/.env\n+++ b/.env\n@@ -1 +1 @@\n-x\n+y\n",
            "diff --git a/migrations/1.sql b/migrations/1.sql\n--- a/migrations/1.sql\n+++ b/migrations/1.sql\n@@ -1 +1 @@\n-x\n+y\n",
            "diff --git a/ok.rs b/ok.rs\n--- a/ok.rs\n+++ b/ok.rs\n@@ -1 +1 @@\n-git status\n+git push origin main\n",
        ] {
            assert!(validate_maintenance_patch(patch).is_err(), "accepted: {patch:?}");
        }
    }
}
