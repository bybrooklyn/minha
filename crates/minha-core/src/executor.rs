//! Minha's deliberately small, fixed model-facing tool executor.
//!
//! This module is intentionally independent of the older orchestration tool
//! catalog in [`crate::tools`]. The model surface stays fixed and compact.

use crate::{
    books::Book,
    facts::{BoardEntry, BoardKind, BoardStatus},
    github::GitHubQuery,
    protocol::{BoardEntryView, EventAgentId, RunId, RuntimeEvent, TodoItem, TodoState},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

pub const DEFAULT_OUTPUT_CAP: usize = 16 * 1024;
pub const DEFAULT_READ_CAP: usize = 24 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const TRUNCATION_MARKER: &str = "\n[output truncated]";
const MAX_BOOK_READ_TOKENS: usize = 32_000;

static BUNDLED_BOOKS: OnceLock<Result<Vec<Book>, String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BookDetail {
    Index,
    Compact,
    Detailed,
}

impl BookDetail {
    fn parse(value: Option<&Value>) -> Result<Self, ToolError> {
        match value.and_then(Value::as_str).unwrap_or("compact") {
            "index" => Ok(Self::Index),
            "compact" => Ok(Self::Compact),
            "detailed" => Ok(Self::Detailed),
            other => Err(invalid(&format!(
                "detail must be one of index, compact, detailed; got {other}"
            ))),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Compact => "compact",
            Self::Detailed => "detailed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(String),
    #[error("read-only policy denied tool invocation")]
    ReadOnlyDenied,
    #[error("command denied by safety policy: {0}")]
    CommandDenied(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("git apply failed: {0}")]
    PatchFailed(String),
    #[error("coordination store error: {0}")]
    Coordination(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputRequest {
    pub question: String,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolOutcome {
    Output(ToolOutput),
    NeedsInput(InputRequest),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutorPolicy {
    pub allow_destructive: bool,
}

#[derive(Clone)]
pub struct ToolExecutor {
    root: PathBuf,
    read_only: bool,
    policy: ExecutorPolicy,
    coordination: Option<CoordinationContext>,
}

#[derive(Clone)]
pub struct CoordinationContext {
    pub store: Store,
    pub workspace_id: String,
    pub run_id: RunId,
    pub agent_id: EventAgentId,
    pub task_id: Option<String>,
    pub can_write: bool,
}

impl ToolExecutor {
    pub fn new(root: impl AsRef<Path>, read_only: bool) -> Result<Self, ToolError> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(ToolError::InvalidArguments("workspace is not a directory".into()));
        }
        Ok(Self {
            root,
            read_only,
            policy: ExecutorPolicy::default(),
            coordination: None,
        })
    }

    pub fn with_policy(mut self, policy: ExecutorPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_coordination(mut self, coordination: CoordinationContext) -> Self {
        self.coordination = Some(coordination);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Return a concise approval reason before a risky invocation executes.
    /// The fixed tool remains responsible for enforcing the decision; this is
    /// only the user-facing preflight used by the runtime protocol.
    pub fn approval_reason(&self, name: &str, args: &Value) -> Result<Option<String>, ToolError> {
        if name != "exec" || self.read_only {
            return Ok(None);
        }
        let argv = string_array(args.get("argv"), "argv")?;
        if argv.is_empty() {
            return Err(invalid("argv must not be empty"));
        }
        Ok(
            (is_dangerous_command(&argv) && !is_never_allowed_command(&argv)).then(|| {
                format!(
                    "command can mutate history, remote state, credentials, or delete data: {}",
                    argv.join(" ")
                )
            }),
        )
    }

    /// Dispatch one of the fixed tools using its compact JSON arguments.
    pub fn execute(&self, name: &str, args: &Value) -> Result<ToolOutcome, ToolError> {
        match name {
            "read_files" => self.read_files(args),
            "search" => self.search(args),
            "apply_patch" => self.apply_patch(args),
            "exec" => self.exec(args),
            "ask_user" => self.ask_user(args),
            "hive" => self.hive(args),
            "todo" => self.todo(args),
            "books" => self.books(args),
            "github" => self.github(args),
            "quality" => self.quality(args),
            "board_read" => self.board_read(args),
            "board_write" => self.board_write(args),
            other => Err(ToolError::InvalidArguments(format!("unknown tool {other}"))),
        }
    }

    fn read_files(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let files = args
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("files must be an array"))?;
        let cap = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_CAP as u64) as usize;
        let cap = cap.clamp(1, 256 * 1024);
        let mut out = String::new();
        for file in files {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("each file needs path"))?;
            let full = self.contained_existing(path)?;
            let bytes = fs::read(full)?;
            let text = String::from_utf8_lossy(&bytes);
            let ranges = file.get("ranges").and_then(Value::as_array);
            let selected: Vec<(usize, usize)> = ranges
                .map(|rs| {
                    rs.iter()
                        .filter_map(|r| {
                            Some((
                                r.get("start")?.as_u64()? as usize,
                                r.get("end")?.as_u64()? as usize,
                            ))
                        })
                        .collect()
                })
                .unwrap_or_else(|| vec![(1, usize::MAX)]);
            out.push_str(&format!("--- {path} ---\n"));
            for (line_no, line) in text.lines().enumerate() {
                let n = line_no + 1;
                if selected.iter().any(|(start, end)| n >= *start && n <= *end) {
                    out.push_str(&format!("{n}: {line}\n"));
                    if out.len() >= cap {
                        break;
                    }
                }
            }
            if out.len() >= cap {
                break;
            }
        }
        let (stdout, truncated) = cap_text(out.into_bytes(), cap);
        Ok(ToolOutcome::Output(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: Some(0),
            truncated,
        }))
    }

    fn search(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("query is required"))?;
        let paths = string_array(args.get("paths"), "paths")?;
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(10_000);
        let max_string = max.to_string();
        let mut command = vec![
            "--line-number".to_owned(),
            "--color".to_owned(),
            "never".to_owned(),
            "--max-count".to_owned(),
            max_string,
            "--".to_owned(),
            query.to_owned(),
        ];
        let glob = args.get("glob").and_then(Value::as_str);
        if let Some(glob) = glob {
            command.splice(0..0, ["--glob".to_owned(), glob.to_owned()]);
        }
        let owned_paths: Vec<String> = if paths.is_empty() { vec![".".into()] } else { paths };
        for path in &owned_paths {
            self.contained_dir_or_file(path)?;
        }
        command.extend(owned_paths);
        let output = self.run_command("rg", &command, None, DEFAULT_TIMEOUT_MS, DEFAULT_OUTPUT_CAP)?;
        Ok(ToolOutcome::Output(output))
    }

    fn apply_patch(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        if self.read_only {
            return Err(ToolError::ReadOnlyDenied);
        }
        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("patch is required"))?;
        validate_patch_paths(patch, &self.root)?;
        self.run_patch(patch, true)?;
        self.run_patch(patch, false)?;
        Ok(success())
    }

    fn run_patch(&self, patch: &str, check: bool) -> Result<(), ToolError> {
        let mut args = vec!["apply"];
        if check {
            args.push("--check");
        }
        args.push("-");
        let mut child = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "git apply did not provide piped stdin",
            )
        })?;
        stdin.write_all(patch.as_bytes())?;
        drop(stdin);
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ToolError::PatchFailed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    fn exec(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let argv = string_array(args.get("argv"), "argv")?;
        if argv.is_empty() {
            return Err(invalid("argv must not be empty"));
        }
        if self.read_only && !is_read_only_command(&argv) {
            return Err(ToolError::ReadOnlyDenied);
        }
        if self.read_only {
            validate_read_only_argv(&self.root, &argv)?;
        }
        if is_never_allowed_command(&argv) {
            return Err(ToolError::CommandDenied(argv.join(" ")));
        }
        if !self.policy.allow_destructive && is_dangerous_command(&argv) {
            return Err(ToolError::CommandDenied(argv.join(" ")));
        }
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|p| self.contained_dir_or_file(p))
            .transpose()?;
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let cap = args
            .get("max_output_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_OUTPUT_CAP as u64) as usize;
        let cap = cap.clamp(1, 256 * 1024);
        self.run_command(&argv[0], &argv[1..], cwd.as_deref(), timeout, cap)
            .map(ToolOutcome::Output)
    }

    fn ask_user(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("question is required"))?;
        let options = string_array(args.get("options"), "options").unwrap_or_default();
        Ok(ToolOutcome::NeedsInput(InputRequest {
            question: question.into(),
            options,
        }))
    }

    fn todo(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let context = self
            .coordination
            .as_ref()
            .ok_or_else(|| invalid("todo requires a coordinated agent context"))?;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        if action == "list" {
            return Ok(text_output(
                serde_json::to_string(&json!({
                    "schema_version": 1,
                    "items": context.store.todos(context.run_id, context.agent_id)
                        .map_err(coordination_error)?
                }))
                .map_err(|error| ToolError::Coordination(error.to_string()))?,
            ));
        }
        if action == "replace" {
            context
                .store
                .clear_todos(context.run_id, context.agent_id)
                .map_err(coordination_error)?;
        }
        let id = args.get("id").and_then(Value::as_str).unwrap_or("todo-1");
        let existing = context
            .store
            .todos(context.run_id, context.agent_id)
            .map_err(coordination_error)?
            .into_iter()
            .find(|item| item.id == id);
        let state = match action {
            "start" => TodoState::InProgress,
            "complete" => TodoState::Completed,
            "block" => TodoState::Blocked,
            "drop" => TodoState::Dropped,
            "add" | "replace" => TodoState::Pending,
            _ => {
                return Err(invalid(
                    "todo action must be list, replace, add, start, complete, block, or drop",
                ));
            }
        };
        let item = TodoItem {
            id: id.to_owned(),
            objective: args
                .get("objective")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| existing.as_ref().map(|item| item.objective.clone()))
                .ok_or_else(|| invalid("new todo items need objective"))?,
            state,
            order: args
                .get("order")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| existing.as_ref().map_or(0, |item| u64::from(item.order)))
                as u32,
            blocker: args.get("blocker").and_then(Value::as_str).map(str::to_owned),
            evidence: args
                .get("evidence")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            revision: 0,
        };
        let item = context
            .store
            .upsert_todo(context.run_id, context.agent_id, item)
            .map_err(coordination_error)?;
        Ok(text_output(
            serde_json::to_string(&json!({"schema_version": 1, "item": item}))
                .map_err(|error| ToolError::Coordination(error.to_string()))?,
        ))
    }

    fn hive(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let coordination = self
            .coordination
            .as_ref()
            .ok_or_else(|| invalid("hive is unavailable outside a coordinated run"))?;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("inbox");
        match action {
            "inbox" => {
                let recipient = format!("agent:{}", coordination.agent_id);
                let messages = coordination
                    .store
                    .hive_inbox(
                        coordination.run_id,
                        &recipient,
                        args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize,
                    )
                    .map_err(coordination_error)?;
                Ok(text_output(
                    serde_json::to_string(&messages)
                        .map_err(|error| ToolError::Coordination(error.to_string()))?,
                ))
            }
            "send" => {
                let body = args.get("body").and_then(Value::as_str).unwrap_or_default();
                if body.len() > 1_200 {
                    return Err(invalid("hive message body exceeds 1200 bytes; store an artifact"));
                }
                let recipient = args.get("to").and_then(Value::as_str).unwrap_or("manager");
                let kind = args.get("kind").and_then(Value::as_str).unwrap_or("progress");
                if !matches!(
                    kind,
                    "finding"
                        | "decision"
                        | "blocker"
                        | "request"
                        | "progress"
                        | "handoff"
                        | "artifact_reference"
                ) {
                    return Err(invalid(
                        "hive kind must be finding, decision, blocker, request, progress, handoff, or artifact_reference",
                    ));
                }
                let id = uuid::Uuid::now_v7().to_string();
                let stored_id = coordination
                    .store
                    .insert_hive_message(
                        coordination.run_id,
                        &id,
                        args.get("room").and_then(Value::as_str).unwrap_or("run"),
                        &format!("agent:{}", coordination.agent_id),
                        recipient,
                        kind,
                        &json!({
                            "body": body,
                            "task_id": coordination.task_id,
                            "refs": args.get("refs").cloned().unwrap_or(Value::Array(Vec::new())),
                            "requested_action": args.get("requested_action"),
                        }),
                        Some(chrono::Utc::now() + chrono::Duration::hours(24)),
                    )
                    .map_err(coordination_error)?;
                let deduplicated = stored_id != id;
                coordination
                    .store
                    .record_runtime_event(
                        coordination.run_id,
                        RuntimeEvent::OfficeMessageChanged {
                            message_id: stored_id.clone(),
                            room_id: args
                                .get("room")
                                .and_then(Value::as_str)
                                .unwrap_or("run")
                                .to_owned(),
                            sender: format!("agent:{}", coordination.agent_id),
                            recipient: recipient.to_owned(),
                            kind: kind.to_owned(),
                            summary: body.chars().take(240).collect(),
                            deduplicated,
                        },
                    )
                    .map_err(coordination_error)?;
                Ok(text_output(
                    json!({"id": stored_id, "delivered": true, "deduplicated": deduplicated}).to_string(),
                ))
            }
            "board_read" => self.board_read(args),
            "board_post" | "board_resolve" => {
                if self.read_only || !coordination.can_write {
                    return Err(ToolError::ReadOnlyDenied);
                }
                let mut mapped = args.clone();
                if let Some(object) = mapped.as_object_mut() {
                    object.insert(
                        "action".into(),
                        Value::String(if action == "board_post" { "post" } else { "resolve" }.into()),
                    );
                }
                self.board_write(&mapped)
            }
            "artifact_put" => {
                let body = args.get("body").and_then(Value::as_str).unwrap_or_default();
                if body.is_empty() || body.len() > 256 * 1024 {
                    return Err(invalid("artifact body must be between 1 and 262144 bytes"));
                }
                let id = args
                    .get("artifact_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                let digest = coordination
                    .store
                    .put_office_artifact(
                        coordination.run_id,
                        &id,
                        args.get("kind").and_then(Value::as_str).unwrap_or("note"),
                        body.as_bytes(),
                        &json!({"agent_id": coordination.agent_id, "task_id": coordination.task_id}),
                    )
                    .map_err(coordination_error)?;
                Ok(text_output(json!({"id": id, "digest": digest}).to_string()))
            }
            _ => Err(invalid("unknown hive action")),
        }
    }

    fn books(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("search");
        match action {
            "search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or_default();
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8).min(20) as usize;
                let books = bundled_book_catalog()?;
                let query_terms = search_terms(query);
                let mut entries = books
                    .iter()
                    .filter_map(|book| {
                        let score = book_search_score(book, query, &query_terms);
                        (query_terms.is_empty() || score > 0).then_some((score, book))
                    })
                    .collect::<Vec<_>>();
                entries.sort_by(|(left_score, left), (right_score, right)| {
                    right_score
                        .cmp(left_score)
                        .then_with(|| left.metadata.id.cmp(&right.metadata.id))
                });
                let entries = entries
                    .into_iter()
                    .take(limit)
                    .map(|(_, book)| book_search_view(book))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(text_output(
                    serde_json::to_string(&entries)
                        .map_err(|error| ToolError::Coordination(error.to_string()))?,
                ))
            }
            "read" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("book id is required"))?;
                let books = bundled_book_catalog()?;
                let book = books
                    .iter()
                    .find(|book| book.metadata.id == id)
                    .ok_or_else(|| invalid("book not found"))?;
                let detail = BookDetail::parse(args.get("detail"))?;
                let max_tokens = book_read_limit(args.get("max_tokens"), detail, book)?;
                let output = book_read_view(book, detail, max_tokens)?;
                Ok(text_output(
                    serde_json::to_string(&output)
                        .map_err(|error| ToolError::Coordination(error.to_string()))?,
                ))
            }
            "draft" => {
                let coordination = self
                    .coordination
                    .as_ref()
                    .ok_or_else(|| invalid("book drafts require a coordinated run"))?;
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("draft body is required"))?;
                if body.len() > 64 * 1024 {
                    return Err(invalid("book draft exceeds 65536 bytes"));
                }
                let id = format!("book-draft-{}", uuid::Uuid::now_v7());
                let digest = coordination
                    .store
                    .put_office_artifact(
                        coordination.run_id,
                        &id,
                        "book_draft",
                        body.as_bytes(),
                        &json!({"agent_id": coordination.agent_id, "state": "draft"}),
                    )
                    .map_err(coordination_error)?;
                Ok(text_output(
                    json!({"id": id, "digest": digest, "trust": "draft"}).to_string(),
                ))
            }
            "feedback" => Ok(text_output("{\"recorded\":true}".into())),
            _ => Err(invalid("unknown books action")),
        }
    }

    fn github(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let query = GitHubQuery::parse(args).map_err(|error| invalid(&error.to_string()))?;
        self.run_command(
            "gh",
            &query.argv,
            None,
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1_000, 120_000),
            args.get("max_output_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_OUTPUT_CAP as u64)
                .clamp(1, 256 * 1024) as usize,
        )
        .map(ToolOutcome::Output)
    }

    fn quality(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("check");
        let suite = args.get("suite").and_then(Value::as_str).unwrap_or("auto");
        if action == "format" && self.read_only {
            return Err(ToolError::ReadOnlyDenied);
        }
        let commands = quality_commands(&self.root, suite, action)?;
        if action == "detect" {
            return Ok(text_output(serde_json::to_string(&commands).map_err(
                |error| invalid(&format!("could not encode quality tools: {error}")),
            )?));
        }
        if commands.is_empty() {
            return Err(invalid("no matching quality commands were detected"));
        }
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120_000)
            .clamp(1_000, 600_000);
        let cap = args
            .get("max_output_bytes")
            .and_then(Value::as_u64)
            .unwrap_or((DEFAULT_OUTPUT_CAP * 2) as u64)
            .clamp(1, 256 * 1024) as usize;
        let per_command_cap = (cap / commands.len()).max(4 * 1024);
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = Some(0);
        let mut truncated = false;
        for command in commands {
            if !command.available {
                stderr.push_str(&format!(
                    "--- {} ---\n[skipped: {} is not installed]\n",
                    command.label, command.program
                ));
                continue;
            }
            let output = self.run_command(command.program, &command.argv, None, timeout, per_command_cap)?;
            stdout.push_str(&format!("--- {} ---\n{}", command.label, output.stdout));
            if !output.stdout.ends_with('\n') {
                stdout.push('\n');
            }
            if !output.stderr.is_empty() {
                stderr.push_str(&format!("--- {} ---\n{}", command.label, output.stderr));
                if !output.stderr.ends_with('\n') {
                    stderr.push('\n');
                }
            }
            if output.exit_code != Some(0) && exit_code == Some(0) {
                exit_code = output.exit_code;
            }
            truncated |= output.truncated;
        }
        let (stdout, stdout_truncated) = cap_text(stdout.into_bytes(), cap);
        let (stderr, stderr_truncated) = cap_text(stderr.into_bytes(), cap);
        Ok(ToolOutcome::Output(ToolOutput {
            stdout,
            stderr,
            exit_code,
            truncated: truncated || stdout_truncated || stderr_truncated,
        }))
    }

    fn board_read(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let coordination = self
            .coordination
            .as_ref()
            .ok_or_else(|| invalid("board is unavailable outside a coordinated run"))?;
        let query = args.get("query").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let entries = coordination
            .store
            .board_entries(
                &coordination.workspace_id,
                Some(coordination.run_id),
                query,
                limit.min(50),
            )
            .map_err(coordination_error)?;
        let rows = entries.iter().map(board_view).collect::<Vec<_>>();
        Ok(text_output(
            serde_json::to_string(&rows).map_err(|error| ToolError::Coordination(error.to_string()))?,
        ))
    }

    fn board_write(&self, args: &Value) -> Result<ToolOutcome, ToolError> {
        let coordination = self
            .coordination
            .as_ref()
            .ok_or_else(|| invalid("board is unavailable outside a coordinated run"))?;
        if self.read_only || !coordination.can_write {
            return Err(ToolError::ReadOnlyDenied);
        }
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("action is required"))?;
        let entry = match action {
            "post" => {
                let kind = parse_board_kind(
                    args.get("kind")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("kind is required for post"))?,
                )?;
                let subject = args
                    .get("subject")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("subject is required for post"))?;
                let body = args
                    .get("body")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("body is required for post"))?;
                let mut entry = BoardEntry::session(
                    coordination.workspace_id.clone(),
                    coordination.run_id,
                    kind,
                    subject,
                    body,
                );
                entry.task_id = args
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| coordination.task_id.clone());
                entry.author_agent_id = Some(coordination.agent_id);
                entry.confidence = args
                    .get("confidence")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .min(100) as u8;
                entry.evidence = string_array(args.get("evidence"), "evidence")?;
                entry.supersedes_id = args
                    .get("supersedes_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                coordination
                    .store
                    .insert_board_entry(&entry)
                    .map_err(coordination_error)?;
                entry
            }
            "resolve" => {
                let id = args
                    .get("entry_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("entry_id is required for resolve"))?;
                coordination
                    .store
                    .revise_board_entry(id, None, Some(BoardStatus::Resolved), Some(coordination.agent_id))
                    .map_err(coordination_error)?
                    .ok_or_else(|| invalid("board entry was not found"))?
            }
            other => return Err(invalid(&format!("unsupported board action {other}"))),
        };
        let view = board_view(&entry);
        coordination
            .store
            .record_runtime_event(
                coordination.run_id,
                RuntimeEvent::BoardChanged { entry: view.clone() },
            )
            .map_err(coordination_error)?;
        Ok(text_output(
            serde_json::to_string(&view).map_err(|error| ToolError::Coordination(error.to_string()))?,
        ))
    }

    fn run_command(
        &self,
        program: &str,
        argv: &[String],
        cwd: Option<&Path>,
        timeout_ms: u64,
        cap: usize,
    ) -> Result<ToolOutput, ToolError> {
        let mut child = Command::new(program)
            .args(argv)
            .current_dir(cwd.unwrap_or(&self.root))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "command did not provide piped stdout",
            )
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "command did not provide piped stderr",
            )
        })?;
        let stdout_reader = thread::spawn(move || read_capped_stream(&mut stdout, cap));
        let stderr_reader = thread::spawn(move || read_capped_stream(&mut stderr, cap));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                timed_out = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let status = child.wait()?;
        let (stdout_bytes, stdout_reader_truncated) = stdout_reader
            .join()
            .map_err(|_| std::io::Error::other("stdout reader thread panicked"))??;
        let (stderr_bytes, stderr_reader_truncated) = stderr_reader
            .join()
            .map_err(|_| std::io::Error::other("stderr reader thread panicked"))??;
        let (stdout, a) = cap_text(stdout_bytes, cap);
        let (mut stderr, b) = cap_text(stderr_bytes, cap);
        if timed_out {
            stderr.push_str("\n[command timed out]");
        }
        Ok(ToolOutput {
            stdout,
            stderr,
            exit_code: status.code(),
            truncated: timed_out || stdout_reader_truncated || stderr_reader_truncated || a || b,
        })
    }

    fn contained_existing(&self, input: &str) -> Result<PathBuf, ToolError> {
        self.contained(input, true)
    }
    fn contained_dir_or_file(&self, input: &str) -> Result<PathBuf, ToolError> {
        self.contained(input, false)
    }
    fn contained(&self, input: &str, must_exist: bool) -> Result<PathBuf, ToolError> {
        let path = Path::new(input);
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(ToolError::OutsideWorkspace(input.into()));
        }
        let joined = if path.is_absolute() {
            path.to_owned()
        } else {
            self.root.join(path)
        };
        let candidate = if must_exist || joined.exists() {
            fs::canonicalize(&joined)?
        } else {
            joined
        };
        if !candidate.starts_with(&self.root) {
            return Err(ToolError::OutsideWorkspace(input.into()));
        }
        Ok(candidate)
    }
}

fn bundled_book_catalog() -> Result<&'static [Book], ToolError> {
    match BUNDLED_BOOKS.get_or_init(|| crate::books::bundled_books().map_err(|error| error.to_string())) {
        Ok(books) => Ok(books.as_slice()),
        Err(error) => Err(ToolError::Coordination(format!(
            "bundled book catalog failed integrity validation: {error}"
        ))),
    }
}

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn book_search_score(book: &Book, query: &str, query_terms: &[String]) -> i32 {
    if query_terms.is_empty() {
        return 0;
    }
    let metadata = &book.metadata;
    let query = query.to_ascii_lowercase();
    let title = metadata.title.to_ascii_lowercase();
    let abstract_text = metadata.abstract_text.to_ascii_lowercase();
    let tags = metadata.tags.join(" ").to_ascii_lowercase();
    let path = metadata.path.to_ascii_lowercase();
    let mut score = i32::from(title.contains(&query)) * 20;
    for term in query_terms {
        score += i32::from(title.contains(term)) * 8;
        score += i32::from(abstract_text.contains(term)) * 3;
        score += i32::from(tags.contains(term)) * 5;
        score += i32::from(path.contains(term));
    }
    score
}

fn pack_id(book: &Book) -> Result<&str, ToolError> {
    book.metadata
        .path
        .strip_prefix("bundled/books/")
        .and_then(|path| path.strip_suffix(".json"))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ToolError::Coordination(format!(
                "bundled book {} has an invalid content path {}",
                book.metadata.id, book.metadata.path
            ))
        })
}

fn book_search_view(book: &Book) -> Result<Value, ToolError> {
    let metadata = &book.metadata;
    Ok(json!({
        "id": metadata.id,
        "title": metadata.title,
        "pack_id": pack_id(book)?,
        "version": metadata.version,
        "language": metadata.language,
        "taxonomy": metadata.taxonomy,
        "tags": metadata.tags,
        "path": metadata.path,
        "abstract_text": metadata.abstract_text,
        "trust": metadata.trust,
        "staleness": metadata.staleness,
        "token_budget": metadata.token_budget,
    }))
}

fn book_index_view(book: &Book) -> Result<Value, ToolError> {
    let metadata = &book.metadata;
    Ok(json!({
        "id": metadata.id,
        "title": metadata.title,
        "authors": metadata.authors,
        "pack_id": pack_id(book)?,
        "version": metadata.version,
        "language": metadata.language,
        "taxonomy": metadata.taxonomy,
        "tags": metadata.tags,
        "path": metadata.path,
        "abstract_text": metadata.abstract_text,
        "trust": metadata.trust,
        "staleness": metadata.staleness,
        "token_budget": metadata.token_budget,
        "source": metadata.source,
    }))
}

fn compact_book_content(book: &Book) -> Value {
    json!({
        "abstract": book.metadata.abstract_text,
        "chapters": book.chapters.iter().map(|chapter| json!({
            "id": chapter.id,
            "title": chapter.title,
            "summary": chapter.summary,
            "sections": chapter.sections.iter().map(|section| json!({
                "id": section.id,
                "title": section.title,
                "summary": section.summary,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "key_facts": book.key_facts.iter().map(|fact| json!({
            "id": fact.id,
            "statement": fact.statement,
        })).collect::<Vec<_>>(),
        "source": {
            "title": book.metadata.source.title,
            "publisher": book.metadata.source.publisher,
        },
    })
}

fn detailed_book_content(book: &Book) -> Result<Value, ToolError> {
    serde_json::to_value(json!({
        "abstract": book.metadata.abstract_text,
        "chapters": book.chapters,
        "key_facts": book.key_facts,
        "citations": book.citations,
        "source": book.metadata.source,
    }))
    .map_err(|error| ToolError::Coordination(format!("could not serialize bundled book: {error}")))
}

fn book_read_limit(value: Option<&Value>, detail: BookDetail, book: &Book) -> Result<usize, ToolError> {
    let default = match detail {
        BookDetail::Index => 1,
        BookDetail::Compact => book.metadata.token_budget.compact_tokens as usize,
        BookDetail::Detailed => book.metadata.token_budget.detailed_tokens as usize,
    }
    .clamp(1, MAX_BOOK_READ_TOKENS);
    let Some(value) = value else {
        return Ok(default);
    };
    let requested = value
        .as_u64()
        .ok_or_else(|| invalid("max_tokens must be an integer"))?;
    if requested == 0 || requested > MAX_BOOK_READ_TOKENS as u64 {
        return Err(invalid("max_tokens must be between 1 and 32000"));
    }
    Ok(requested as usize)
}

fn value_token_count(value: &Value) -> usize {
    match value {
        Value::String(text) => text.split_whitespace().count(),
        Value::Array(values) => values.iter().map(value_token_count).sum(),
        Value::Object(values) => values.values().map(value_token_count).sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn truncate_value(value: &mut Value, remaining: &mut usize) {
    match value {
        Value::String(text) => {
            let token_count = text.split_whitespace().count();
            if token_count > *remaining {
                let mut truncated = String::with_capacity(text.len().min(remaining.saturating_mul(8)));
                for token in text.split_whitespace().take(*remaining) {
                    if !truncated.is_empty() {
                        truncated.push(' ');
                    }
                    truncated.push_str(token);
                }
                *text = truncated;
                *remaining = 0;
            } else {
                *remaining -= token_count;
            }
        }
        Value::Array(values) => {
            let mut kept = 0;
            for value in values.iter_mut() {
                if *remaining == 0 {
                    break;
                }
                truncate_value(value, remaining);
                kept += 1;
            }
            values.truncate(kept);
        }
        Value::Object(values) => {
            let keys = values.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                if *remaining == 0 {
                    values.remove(&key);
                    continue;
                }
                if let Some(value) = values.get_mut(&key) {
                    truncate_value(value, remaining);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn book_read_view(book: &Book, detail: BookDetail, max_tokens: usize) -> Result<Value, ToolError> {
    let mut output = json!({
        "detail": detail.name(),
        "max_tokens": max_tokens,
        "book": book_index_view(book)?,
    });
    if detail == BookDetail::Index {
        return Ok(output);
    }
    let mut content = match detail {
        BookDetail::Index => Value::Null,
        BookDetail::Compact => compact_book_content(book),
        BookDetail::Detailed => detailed_book_content(book)?,
    };
    let original_tokens = value_token_count(&content);
    let truncated = original_tokens > max_tokens;
    if truncated {
        let mut remaining = max_tokens;
        truncate_value(&mut content, &mut remaining);
    }
    output["content"] = content;
    output["content_tokens"] = json!(value_token_count(&output["content"]));
    output["truncated"] = json!(truncated);
    Ok(output)
}

pub fn tool_definitions(role: &str, read_only: bool, allow_questions: bool, coordinated: bool) -> Vec<Value> {
    let mode = if read_only { "read-only" } else { "workspace" };
    let mut tools = vec![
        json!({"type":"function","name":"read_files","description":format!("Read batched files in the {mode} workspace ({role})."),"strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["files"],"properties":{"files":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["path"],"properties":{"path":{"type":"string"},"ranges":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["start","end"],"properties":{"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}}}}}}},"max_bytes":{"type":"integer","minimum":1}}}}),
        json!({"type":"function","name":"search","description":"Search workspace text with rg.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}},"glob":{"type":"string"},"max_results":{"type":"integer","minimum":1}}}}),
        json!({"type":"function","name":"apply_patch","description":if read_only{"Denied in read-only mode."}else{"Apply a workspace-contained unified patch."},"strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["patch"],"properties":{"patch":{"type":"string"}}}}),
        json!({"type":"function","name":"exec","description":"Execute an argv vector without a shell.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["argv"],"properties":{"argv":{"type":"array","minItems":1,"items":{"type":"string"}},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1},"max_output_bytes":{"type":"integer","minimum":1}}}}),
        json!({"type":"function","name":"ask_user","description":"Block for a user decision.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["question"],"properties":{"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}}}}),
        json!({"type":"function","name":"books","description":"Search or read indexed technical books; create a private draft only for durable reusable knowledge.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"type":"string","enum":["search","read","draft","feedback"]},"query":{"type":"string"},"id":{"type":"string"},"body":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20},"detail":{"type":"string","enum":["index","compact","detailed"]},"max_tokens":{"type":"integer","minimum":1,"maximum":32000}}}}),
        json!({"type":"function","name":"github","description":"Read structured repository, issue, pull request, check, workflow, run, and release data through authenticated gh CLI.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"type":"string","enum":["repo","issues","issue","prs","pr","checks","runs","workflows","release","releases"]},"repo":{"type":"string"},"number":{"type":"integer","minimum":1},"tag":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100},"timeout_ms":{"type":"integer","minimum":1000,"maximum":120000},"max_output_bytes":{"type":"integer","minimum":1,"maximum":262144}}}}),
        json!({"type":"function","name":"quality","description":"Detect or run bounded built-in quality suites in one call; supports Rust, Python, JavaScript, and Go.","strict":false,"parameters":{"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"type":"string","enum":["detect","check","lint","test","docs","security","all","format"]},"suite":{"type":"string","enum":["auto","rust","python","javascript","go"]},"timeout_ms":{"type":"integer","minimum":1000,"maximum":600000},"max_output_bytes":{"type":"integer","minimum":1,"maximum":262144}}}}),
    ];
    if read_only {
        tools.retain(|tool| tool["name"] != "apply_patch");
    }
    if !allow_questions {
        tools.retain(|tool| tool["name"] != "ask_user");
    }
    if coordinated {
        tools.push(json!({
            "type": "function",
            "name": "todo",
            "description": "Maintain this agent's compact durable work list using deltas.",
            "strict": false,
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "required": ["action"],
                "properties": {
                    "action": {"type":"string","enum":["list","replace","add","start","complete","block","drop"]},
                    "id": {"type":"string"},
                    "objective": {"type":"string"},
                    "order": {"type":"integer","minimum":0},
                    "blocker": {"type":"string"},
                    "evidence": {"type":"array","items":{"type":"string"}}
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "name": "hive",
            "description": "Private compact coordination: inbox, typed messages, shared board entries, and artifact references.",
            "strict": false,
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "required": ["action"],
                "properties": {
                    "action": {"type":"string","enum":["inbox","send","board_read","board_post","board_resolve","artifact_put"]},
                    "to": {"type":"string"},
                    "room": {"type":"string"},
                    "kind": {"type":"string","enum":["finding","decision","blocker","request","progress","handoff","artifact_reference"]},
                    "body": {"type":"string"},
                    "subject": {"type":"string"},
                    "entry_id": {"type":"string"},
                    "query": {"type":"string"},
                    "limit": {"type":"integer","minimum":1,"maximum":100},
                    "refs": {"type":"array","items":{"type":"string"}},
                    "requested_action": {"type":"string"},
                    "artifact_id": {"type":"string"},
                    "confidence": {"type":"integer","minimum":0,"maximum":100},
                    "evidence": {"type":"array","items":{"type":"string"}}
                }
            }
        }));
    }
    tools
}

fn coordination_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Coordination(error.to_string())
}

fn parse_board_kind(value: &str) -> Result<BoardKind, ToolError> {
    match value {
        "decision" => Ok(BoardKind::Decision),
        "constraint" => Ok(BoardKind::Constraint),
        "finding" => Ok(BoardKind::Finding),
        "blocker" => Ok(BoardKind::Blocker),
        "artifact" => Ok(BoardKind::Artifact),
        "progress" => Ok(BoardKind::Progress),
        other => Err(invalid(&format!("unsupported board kind {other}"))),
    }
}

fn board_view(entry: &BoardEntry) -> BoardEntryView {
    BoardEntryView {
        id: entry.id.clone(),
        scope: entry.scope.as_str().into(),
        kind: entry.kind.as_str().into(),
        subject: entry.subject.clone(),
        body: entry.body.clone(),
        task_id: entry.task_id.clone(),
        author_agent_id: entry.author_agent_id,
        confidence: entry.confidence,
        status: entry.status.as_str().into(),
    }
}

fn text_output(stdout: String) -> ToolOutcome {
    ToolOutcome::Output(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: Some(0),
        truncated: false,
    })
}

fn invalid(message: &str) -> ToolError {
    ToolError::InvalidArguments(message.into())
}
fn success() -> ToolOutcome {
    ToolOutcome::Output(ToolOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: Some(0),
        truncated: false,
    })
}
fn string_array(value: Option<&Value>, name: &str) -> Result<Vec<String>, ToolError> {
    value
        .map(|v| {
            v.as_array()
                .ok_or_else(|| invalid(&format!("{name} must be an array")))
                .and_then(|a| {
                    a.iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| invalid(&format!("{name} must contain strings")))
                        })
                        .collect()
                })
        })
        .unwrap_or(Ok(Vec::new()))
}

#[derive(Clone, Debug, serde::Serialize)]
struct QualityCommand {
    label: &'static str,
    program: &'static str,
    argv: Vec<String>,
    available: bool,
}

impl QualityCommand {
    fn new(label: &'static str, program: &'static str, argv: &[&str]) -> Self {
        Self {
            label,
            program,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            available: command_available(program),
        }
    }

    fn cargo_plugin(label: &'static str, plugin: &'static str, argv: &[&str]) -> Self {
        Self {
            label,
            program: "cargo",
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            available: command_available(plugin),
        }
    }
}

fn quality_commands(root: &Path, suite: &str, action: &str) -> Result<Vec<QualityCommand>, ToolError> {
    if !matches!(
        action,
        "detect" | "check" | "lint" | "test" | "docs" | "security" | "all" | "format"
    ) {
        return Err(invalid(
            "quality action must be detect, check, lint, test, docs, security, all, or format",
        ));
    }
    if !matches!(suite, "auto" | "rust" | "python" | "javascript" | "go") {
        return Err(invalid(
            "quality suite must be auto, rust, python, javascript, or go",
        ));
    }
    let detected = [
        ("rust", root.join("Cargo.toml").is_file()),
        (
            "python",
            root.join("pyproject.toml").is_file()
                || root.join("setup.py").is_file()
                || root.join("pytest.ini").is_file(),
        ),
        ("javascript", root.join("package.json").is_file()),
        ("go", root.join("go.mod").is_file()),
    ];
    let suites = detected
        .into_iter()
        .filter_map(|(name, present)| (present && (suite == "auto" || suite == name)).then_some(name))
        .collect::<Vec<_>>();
    if suites.is_empty() {
        return Err(invalid(
            "requested quality suite was not detected in the workspace root",
        ));
    }
    let action = if action == "detect" { "all" } else { action };
    let mut commands = Vec::new();
    for suite in suites {
        match suite {
            "rust" => rust_quality_commands(root, action, &mut commands),
            "python" => python_quality_commands(action, &mut commands),
            "javascript" => javascript_quality_commands(action, &mut commands),
            "go" => go_quality_commands(action, &mut commands),
            _ => {}
        }
    }
    Ok(commands)
}

fn rust_quality_commands(root: &Path, action: &str, commands: &mut Vec<QualityCommand>) {
    if matches!(action, "check" | "lint" | "all") {
        commands.push(QualityCommand::new(
            "rustfmt check",
            "cargo",
            &["fmt", "--all", "--", "--check"],
        ));
    }
    if matches!(action, "check" | "all") {
        commands.push(QualityCommand::new(
            "cargo check",
            "cargo",
            &["check", "--workspace", "--locked"],
        ));
    }
    if matches!(action, "lint" | "all") {
        commands.push(QualityCommand::new(
            "clippy",
            "cargo",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        ));
        if root.join(".github/workflows").is_dir() {
            commands.push(QualityCommand::new("actionlint", "actionlint", &[]));
        }
    }
    if matches!(action, "test" | "all") {
        commands.push(QualityCommand::new(
            "cargo test",
            "cargo",
            &["test", "--workspace", "--locked"],
        ));
    }
    if matches!(action, "docs" | "all") {
        commands.push(QualityCommand::new(
            "cargo doc",
            "cargo",
            &["doc", "--workspace", "--no-deps", "--locked"],
        ));
    }
    if matches!(action, "security" | "all") {
        commands.push(QualityCommand::cargo_plugin(
            "cargo audit",
            "cargo-audit",
            &["audit"],
        ));
        commands.push(QualityCommand::cargo_plugin(
            "cargo deny",
            "cargo-deny",
            &["deny", "--locked", "check"],
        ));
    }
    if action == "format" {
        commands.push(QualityCommand::new("rustfmt", "cargo", &["fmt", "--all"]));
    }
}

fn python_quality_commands(action: &str, commands: &mut Vec<QualityCommand>) {
    if matches!(action, "check" | "lint" | "all") {
        commands.push(QualityCommand::new("ruff", "ruff", &["check", "."]));
    }
    if matches!(action, "test" | "all") {
        commands.push(QualityCommand::new("pytest", "pytest", &["-q"]));
    }
    if action == "format" {
        commands.push(QualityCommand::new("ruff format", "ruff", &["format", "."]));
    }
    if matches!(action, "security" | "all") {
        commands.push(QualityCommand::new("pip audit", "pip-audit", &[]));
    }
}

fn javascript_quality_commands(action: &str, commands: &mut Vec<QualityCommand>) {
    if matches!(action, "check" | "all") {
        commands.push(QualityCommand::new(
            "npm typecheck",
            "npm",
            &["run", "typecheck", "--if-present"],
        ));
    }
    if matches!(action, "lint" | "all") {
        commands.push(QualityCommand::new(
            "npm lint",
            "npm",
            &["run", "lint", "--if-present"],
        ));
    }
    if matches!(action, "test" | "all") {
        commands.push(QualityCommand::new("npm test", "npm", &["test", "--if-present"]));
    }
    if action == "format" {
        commands.push(QualityCommand::new(
            "npm format",
            "npm",
            &["run", "format", "--if-present"],
        ));
    }
    if matches!(action, "security" | "all") {
        commands.push(QualityCommand::new("npm audit", "npm", &["audit", "--json"]));
    }
}

fn go_quality_commands(action: &str, commands: &mut Vec<QualityCommand>) {
    if matches!(action, "check" | "lint" | "all") {
        commands.push(QualityCommand::new("go vet", "go", &["vet", "./..."]));
    }
    if matches!(action, "test" | "all") {
        commands.push(QualityCommand::new("go test", "go", &["test", "./..."]));
    }
    if action == "format" {
        commands.push(QualityCommand::new("go fmt", "go", &["fmt", "./..."]));
    }
}

fn command_available(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(program);
        candidate.is_file() || (cfg!(windows) && directory.join(format!("{program}.exe")).is_file())
    })
}

fn cap_text(bytes: Vec<u8>, cap: usize) -> (String, bool) {
    if bytes.len() <= cap {
        (String::from_utf8_lossy(&bytes).into_owned(), false)
    } else {
        let mut text = String::from_utf8_lossy(&bytes[..cap]).into_owned();
        text.push_str(TRUNCATION_MARKER);
        (text, true)
    }
}

fn read_capped_stream(reader: &mut impl Read, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(cap.min(64 * 1024));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = cap.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((output, truncated))
}

fn is_read_only_command(argv: &[String]) -> bool {
    let Some(program) = argv
        .first()
        .and_then(|value| Path::new(value).file_name().and_then(OsStr::to_str))
    else {
        return false;
    };
    if program == "git" {
        return argv.get(1).is_some_and(|subcommand| {
            matches!(
                subcommand.as_str(),
                "status" | "diff" | "show" | "log" | "rev-parse" | "ls-files" | "grep"
            )
        });
    }
    if program == "cargo" {
        return argv.get(1).is_some_and(|subcommand| {
            matches!(subcommand.as_str(), "check" | "test" | "clippy" | "metadata")
                || (subcommand == "fmt" && argv.iter().any(|argument| argument == "--check"))
        });
    }
    if matches!(program, "npm" | "pnpm" | "yarn") {
        return argv.get(1).is_some_and(|subcommand| subcommand == "test");
    }
    if program == "go" {
        return argv.get(1).is_some_and(|subcommand| subcommand == "test");
    }
    if matches!(program, "pytest" | "ruff") {
        return true;
    }
    matches!(
        program,
        "cat" | "head" | "tail" | "ls" | "pwd" | "rg" | "printf" | "echo"
    ) && !is_dangerous_command(argv)
}

fn validate_read_only_argv(root: &Path, argv: &[String]) -> Result<(), ToolError> {
    let program = argv
        .first()
        .and_then(|value| Path::new(value).file_name())
        .and_then(OsStr::to_str)
        .ok_or(ToolError::ReadOnlyDenied)?;
    let unsafe_option = match program {
        "git" => argv.iter().skip(1).any(|argument| {
            matches!(
                argument.as_str(),
                "-C" | "--git-dir"
                    | "--work-tree"
                    | "--namespace"
                    | "--config-env"
                    | "--exec-path"
                    | "--output"
                    | "--ext-diff"
                    | "--textconv"
                    | "--open-files-in-pager"
            ) || argument.starts_with("--git-dir=")
                || argument.starts_with("--work-tree=")
                || argument.starts_with("--namespace=")
                || argument.starts_with("--config-env=")
                || argument.starts_with("--exec-path=")
                || argument.starts_with("--output=")
                || argument.starts_with("--open-files-in-pager=")
        }),
        "rg" => argv.iter().skip(1).any(|argument| {
            matches!(
                argument.as_str(),
                "--pre" | "--hostname-bin" | "--search-zip" | "-z"
            ) || argument.starts_with("--pre=")
                || argument.starts_with("--hostname-bin=")
        }),
        _ => false,
    };
    if unsafe_option {
        return Err(ToolError::CommandDenied(argv.join(" ")));
    }

    for argument in argv.iter().skip(1) {
        let value = argument
            .split_once('=')
            .map_or(argument.as_str(), |(_, value)| value);
        if value.is_empty() || (value.starts_with('-') && !Path::new(value).is_absolute()) {
            continue;
        }
        let path = Path::new(value);
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(ToolError::OutsideWorkspace(value.into()));
        }
        let candidate = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        if candidate.exists() {
            let canonical = fs::canonicalize(&candidate)?;
            if !canonical.starts_with(root) {
                return Err(ToolError::OutsideWorkspace(value.into()));
            }
        } else if path.is_absolute() {
            return Err(ToolError::OutsideWorkspace(value.into()));
        }
    }
    Ok(())
}
fn is_dangerous_command(argv: &[String]) -> bool {
    let program = Path::new(argv.first().map(String::as_str).unwrap_or(""))
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    let joined = argv
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    const DENIED_PROGRAMS: &[&str] = &[
        "rm",
        "rmdir",
        "shred",
        "dd",
        "mkfs",
        "fdisk",
        "format",
        "shutdown",
        "reboot",
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "pwsh",
        "powershell",
        "cmd",
        "env",
        "xargs",
        "gh",
        "codex",
        "claude",
        "gemini",
        "minha",
    ];
    const DENIED_GIT_SUBCOMMANDS: &[&str] = &[
        "add",
        "am",
        "apply",
        "bisect",
        "branch",
        "checkout",
        "cherry-pick",
        "clean",
        "commit",
        "config",
        "merge",
        "mv",
        "push",
        "rebase",
        "reset",
        "restore",
        "revert",
        "rm",
        "stash",
        "switch",
        "tag",
        "worktree",
    ];
    const REMOTE_PROGRAMS: &[&str] = &[
        "curl", "wget", "ssh", "scp", "sftp", "rsync", "kubectl", "docker", "podman",
    ];
    let remote_package_operation = matches!(program, "npm" | "pnpm" | "yarn")
        && argv.iter().skip(1).any(|argument| argument == "publish");
    let remote_cargo_operation =
        program == "cargo" && argv.iter().skip(1).any(|argument| argument == "publish");
    let remote_git_operation = program == "git"
        && argv
            .iter()
            .skip(1)
            .any(|argument| matches!(argument.as_str(), "clone" | "fetch" | "pull" | "push"));
    DENIED_PROGRAMS.contains(&program)
        || REMOTE_PROGRAMS.contains(&program)
        || remote_package_operation
        || remote_cargo_operation
        || remote_git_operation
        || (program == "git"
            && DENIED_GIT_SUBCOMMANDS
                .iter()
                .any(|word| argv.iter().skip(1).any(|argument| argument == word)))
        || joined.contains(" > /dev/")
}

fn is_never_allowed_command(argv: &[String]) -> bool {
    let program = Path::new(argv.first().map(String::as_str).unwrap_or(""))
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("");
    matches!(
        program,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "env"
            | "xargs"
            | "codex"
            | "claude"
            | "gemini"
            | "minha"
    )
}

fn validate_patch_paths(patch: &str, root: &Path) -> Result<(), ToolError> {
    if patch.lines().any(|line| {
        line.contains("new file mode 120000")
            || line.contains("old mode 120000")
            || line.contains("new mode 120000")
    }) {
        return Err(ToolError::OutsideWorkspace(
            "symlink patches are not allowed".into(),
        ));
    }
    for line in patch.lines().filter(|line| {
        line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("copy from ")
            || line.starts_with("copy to ")
    }) {
        let raw = line
            .split_once(' ')
            .map(|(_, value)| value)
            .unwrap_or("")
            .split('\t')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        if raw == "/dev/null" {
            continue;
        }
        let path = raw
            .strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .unwrap_or(raw);
        if Path::new(path).is_absolute() || Path::new(path).components().any(|c| c == Component::ParentDir) {
            return Err(ToolError::OutsideWorkspace(path.into()));
        }
        let full = root.join(path);
        let resolved = if full.exists() {
            fs::canonicalize(&full)?
        } else {
            let parent = full.parent().unwrap_or(root);
            fs::canonicalize(parent)?.join(full.file_name().unwrap_or_default())
        };
        if !resolved.starts_with(root) {
            return Err(ToolError::OutsideWorkspace(path.into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn tool_count_is_fixed() {
        assert_eq!(tool_definitions("implementer", false, true, false).len(), 8);
        assert_eq!(tool_definitions("reviewer", true, true, false).len(), 7);
        assert_eq!(tool_definitions("reviewer", true, false, false).len(), 6);
        assert_eq!(tool_definitions("implementer", false, true, true).len(), 10);
        assert_eq!(tool_definitions("reviewer", true, true, true).len(), 9);
        assert!(
            tool_definitions("implementer", false, true, true)
                .iter()
                .all(|tool| tool["strict"] == false),
            "compact optional arguments are validated by the executor; strict Responses schemas would require every optional key"
        );
        let coordinated = tool_definitions("implementer", false, true, true);
        assert!(coordinated.iter().any(|tool| tool["name"] == "hive"));
        assert!(coordinated.iter().any(|tool| tool["name"] == "todo"));
        assert!(coordinated.iter().any(|tool| tool["name"] == "books"));
        assert!(coordinated.iter().any(|tool| tool["name"] == "github"));
        assert!(coordinated.iter().any(|tool| tool["name"] == "quality"));
        assert!(!coordinated.iter().any(|tool| tool["name"] == "board_read"));
        let books = coordinated
            .iter()
            .find(|tool| tool["name"] == "books")
            .expect("test operation should succeed");
        assert_eq!(
            books["parameters"]["properties"]["detail"]["enum"],
            json!(["index", "compact", "detailed"])
        );
        assert_eq!(books["parameters"]["properties"]["max_tokens"]["maximum"], 32000);
    }

    #[test]
    fn quality_detects_bounded_rust_checks_without_running_them() {
        let directory = tempdir().expect("temporary workspace");
        fs::write(directory.path().join("Cargo.toml"), "[workspace]\n").expect("workspace manifest");
        let executor = ToolExecutor::new(directory.path(), true).expect("quality executor");
        let ToolOutcome::Output(output) = executor
            .execute("quality", &json!({"action":"detect", "suite":"auto"}))
            .expect("quality detection")
        else {
            panic!("quality detect must produce output")
        };
        let commands: Vec<Value> = serde_json::from_str(&output.stdout).expect("quality JSON");
        assert!(commands.iter().any(|command| command["label"] == "clippy"));
        assert!(commands.len() <= 7);
    }

    #[test]
    fn quality_format_is_denied_to_read_only_roles() {
        let directory = tempdir().expect("temporary workspace");
        fs::write(directory.path().join("Cargo.toml"), "[workspace]\n").expect("workspace manifest");
        let executor = ToolExecutor::new(directory.path(), true).expect("quality executor");
        assert!(matches!(
            executor.execute("quality", &json!({"action":"format", "suite":"rust"})),
            Err(ToolError::ReadOnlyDenied)
        ));
    }

    #[test]
    fn books_read_returns_structured_verified_content_with_bounded_detail() {
        let book = crate::books::bundled_books()
            .expect("test operation should succeed")
            .remove(0);
        let executor = ToolExecutor::new(tempdir().expect("test operation should succeed").path(), true)
            .expect("test operation should succeed");
        let ToolOutcome::Output(output) = executor
            .execute(
                "books",
                &json!({"action": "read", "id": book.metadata.id, "max_tokens": 8}),
            )
            .expect("test operation should succeed")
        else {
            panic!()
        };
        let compact: Value = serde_json::from_str(&output.stdout).expect("test operation should succeed");
        assert_eq!(compact["detail"], "compact");
        assert_eq!(compact["truncated"], true);
        assert!(compact["content"].is_object());
        assert!(
            compact["content_tokens"]
                .as_u64()
                .expect("test operation should succeed")
                <= 8
        );

        let ToolOutcome::Output(output) = executor
            .execute(
                "books",
                &json!({"action": "read", "id": book.metadata.id, "detail": "detailed", "max_tokens": 32000}),
            )
            .expect("test operation should succeed")
        else {
            panic!()
        };
        let detailed: Value = serde_json::from_str(&output.stdout).expect("test operation should succeed");
        assert_eq!(detailed["detail"], "detailed");
        assert!(detailed["content"]["citations"].is_array());
        assert!(detailed["content"]["chapters"][0]["sections"].is_array());
    }

    #[test]
    fn books_search_is_relevance_sorted_and_read_rejects_invalid_limits() {
        let book = crate::books::bundled_books()
            .expect("test operation should succeed")
            .remove(0);
        let executor = ToolExecutor::new(tempdir().expect("test operation should succeed").path(), true)
            .expect("test operation should succeed");
        let ToolOutcome::Output(output) = executor
            .execute(
                "books",
                &json!({"action": "search", "query": book.metadata.title, "limit": 20}),
            )
            .expect("test operation should succeed")
        else {
            panic!()
        };
        let results: Vec<Value> =
            serde_json::from_str(&output.stdout).expect("test operation should succeed");
        assert_eq!(
            results.first().and_then(|result| result["id"].as_str()),
            Some(book.metadata.id.as_str())
        );

        assert!(matches!(
            executor.execute(
                "books",
                &json!({"action": "read", "id": book.metadata.id, "max_tokens": 32001})
            ),
            Err(ToolError::InvalidArguments(_))
        ));
        assert!(matches!(
            executor.execute(
                "books",
                &json!({"action": "read", "id": book.metadata.id, "detail": "full"})
            ),
            Err(ToolError::InvalidArguments(_))
        ));
    }

    #[test]
    fn coordinated_board_is_shared_but_project_pins_are_not_model_tools() {
        let directory = tempdir().expect("test operation should succeed");
        let store = Store::in_memory().expect("test operation should succeed");
        let workspace = store
            .ensure_workspace(directory.path())
            .expect("test operation should succeed");
        let run = store
            .create_run("coordinate", crate::protocol::Mode::Batch)
            .expect("test operation should succeed");
        store
            .attach_run_workspace(run.id, &workspace.id)
            .expect("test operation should succeed");
        let executor = ToolExecutor::new(directory.path(), false)
            .expect("test operation should succeed")
            .with_coordination(CoordinationContext {
                store: store.clone(),
                workspace_id: workspace.id,
                run_id: run.id,
                agent_id: EventAgentId::new(),
                task_id: Some("task-a".into()),
                can_write: true,
            });
        executor
            .execute(
                "board_write",
                &json!({
                    "action": "post",
                    "kind": "finding",
                    "subject": "Parser location",
                    "body": "The parser lives in src/parser.rs"
                }),
            )
            .expect("test operation should succeed");
        let ToolOutcome::Output(output) = executor
            .execute("board_read", &json!({"query": "parser", "limit": 5}))
            .expect("test operation should succeed")
        else {
            panic!()
        };
        assert!(output.stdout.contains("Parser location"));
        assert_eq!(
            store
                .board_entries(
                    executor
                        .coordination
                        .as_ref()
                        .expect("test operation should succeed")
                        .workspace_id
                        .as_str(),
                    Some(run.id),
                    None,
                    10
                )
                .expect("test operation should succeed")[0]
                .task_id
                .as_deref(),
            Some("task-a")
        );
    }
    #[test]
    fn traversal_is_denied() {
        let d = tempdir().expect("test operation should succeed");
        let e = ToolExecutor::new(d.path(), false).expect("test operation should succeed");
        assert!(matches!(
            e.execute("read_files", &json!({"files":[{"path":"../x"}]})),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }
    #[test]
    fn canonical_absolute_paths_inside_workspace_are_allowed() {
        let directory = tempdir().expect("test operation should succeed");
        fs::write(directory.path().join("inside.txt"), "inside").expect("test operation should succeed");
        let executor = ToolExecutor::new(directory.path(), true).expect("test operation should succeed");
        let ToolOutcome::Output(output) = executor
            .execute(
                "read_files",
                &json!({"files":[{"path": directory.path().join("inside.txt").to_string_lossy()}]}),
            )
            .expect("test operation should succeed")
        else {
            panic!()
        };
        assert!(output.stdout.contains("inside"));
        assert!(matches!(
            executor.execute(
                "read_files",
                &json!({"files":[{"path": std::env::current_exe().expect("test operation should succeed").to_string_lossy()}]})
            ),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }
    #[test]
    fn read_files_batches_ranges() {
        let d = tempdir().expect("test operation should succeed");
        fs::write(d.path().join("a"), "one\ntwo\nthree\n").expect("test operation should succeed");
        let e = ToolExecutor::new(d.path(), true).expect("test operation should succeed");
        let ToolOutcome::Output(o) = e
            .execute(
                "read_files",
                &json!({"files":[{"path":"a","ranges":[{"start":2,"end":3}]}]}),
            )
            .expect("test operation should succeed")
        else {
            panic!()
        };
        assert!(o.stdout.contains("2: two") && o.stdout.contains("3: three") && !o.stdout.contains("1: one"));
    }

    #[test]
    fn patches_apply_in_snapshot_lanes_without_git_metadata() {
        let directory = tempdir().expect("test operation should succeed");
        fs::write(directory.path().join("note.txt"), "old\n").expect("test operation should succeed");
        let executor = ToolExecutor::new(directory.path(), false).expect("test operation should succeed");
        executor
            .execute(
                "apply_patch",
                &json!({"patch": "diff --git a/note.txt b/note.txt\n--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n"}),
            )
            .expect("test operation should succeed");
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).expect("test operation should succeed"),
            "new\n"
        );
    }
    #[test]
    fn destructive_exec_is_denied() {
        let d = tempdir().expect("test operation should succeed");
        let e = ToolExecutor::new(d.path(), false).expect("test operation should succeed");
        for argv in [
            json!(["git", "reset", "--hard"]),
            json!(["git", "commit", "-am", "model change"]),
            json!(["sh", "-c", "echo unsafe"]),
            json!(["codex", "exec", "spend more tokens"]),
        ] {
            assert!(matches!(
                e.execute("exec", &json!({"argv": argv})),
                Err(ToolError::CommandDenied(_))
            ));
        }
    }

    #[test]
    fn risky_commands_are_classified_before_execution() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        let executor = ToolExecutor::new(temp.path(), false).expect("test operation should succeed");
        assert!(
            executor
                .approval_reason("exec", &json!({"argv": ["git", "push", "origin", "main"]}))
                .expect("test operation should succeed")
                .is_some()
        );
        assert!(
            executor
                .approval_reason("exec", &json!({"argv": ["cargo", "test"]}))
                .expect("test operation should succeed")
                .is_none()
        );
        let read_only = ToolExecutor::new(temp.path(), true).expect("test operation should succeed");
        assert!(
            read_only
                .approval_reason("exec", &json!({"argv": ["bash", "-lc", "ls"]}))
                .expect("test operation should succeed")
                .is_none()
        );
        let approved = ToolExecutor::new(temp.path(), false)
            .expect("test operation should succeed")
            .with_policy(ExecutorPolicy {
                allow_destructive: true,
            });
        assert!(matches!(
            approved.execute("exec", &json!({"argv": ["bash", "-lc", "ls"]})),
            Err(ToolError::CommandDenied(_))
        ));
    }

    #[test]
    fn remote_commands_require_one_use_approval_but_local_builds_remain_allowed() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        let executor = ToolExecutor::new(temp.path(), false).expect("test operation should succeed");
        for argv in [
            vec!["curl", "https://example.invalid"],
            vec!["wget", "https://example.invalid"],
            vec!["ssh", "host"],
            vec!["scp", "file", "host:/tmp/file"],
            vec!["sftp", "host"],
            vec!["rsync", "file", "host:/tmp"],
            vec!["kubectl", "apply", "-f", "deployment.yaml"],
            vec!["docker", "push", "image:latest"],
            vec!["npm", "publish"],
            vec!["cargo", "publish"],
            vec!["git", "fetch", "origin"],
            vec!["git", "pull", "--ff-only"],
            vec!["git", "push", "origin", "main"],
        ] {
            assert!(is_dangerous_command(
                &argv.iter().map(ToString::to_string).collect::<Vec<_>>()
            ));
            assert!(
                executor
                    .approval_reason("exec", &json!({"argv": argv}))
                    .expect("test operation should succeed")
                    .is_some()
            );
        }
        for argv in [
            json!(["cargo", "check"]),
            json!(["cargo", "test"]),
            json!(["cargo", "fmt", "--", "--check"]),
            json!(["npm", "test"]),
        ] {
            assert!(
                executor
                    .approval_reason("exec", &json!({"argv": argv}))
                    .expect("test operation should succeed")
                    .is_none()
            );
        }
        assert!(
            !executor
                .approval_reason("exec", &json!({"argv": ["bash", "-lc", "curl example.invalid"]}))
                .expect("test operation should succeed")
                .is_some()
        );
        assert!(matches!(
            executor.execute("exec", &json!({"argv": ["bash", "-lc", "curl example.invalid"]})),
            Err(ToolError::CommandDenied(_))
        ));
    }

    #[test]
    fn read_only_git_cannot_hide_a_mutation_behind_branch() {
        let d = tempdir().expect("test operation should succeed");
        let e = ToolExecutor::new(d.path(), true).expect("test operation should succeed");
        assert!(matches!(
            e.execute("exec", &json!({"argv":["git","branch","-D","main"]})),
            Err(ToolError::ReadOnlyDenied)
        ));
    }

    #[test]
    fn read_only_exec_cannot_escape_workspace_through_arguments() {
        let workspace = tempdir().expect("workspace");
        let outside = tempdir().expect("outside directory");
        let inside_path = workspace.path().join("inside.txt");
        let outside_path = outside.path().join("outside.txt");
        fs::write(&inside_path, "inside").expect("inside fixture");
        fs::write(&outside_path, "outside").expect("outside fixture");
        let executor = ToolExecutor::new(workspace.path(), true).expect("read-only executor");

        assert!(
            executor
                .execute("exec", &json!({"argv":["cat", inside_path]}))
                .is_ok()
        );
        assert!(matches!(
            executor.execute("exec", &json!({"argv":["cat", outside_path.clone()]})),
            Err(ToolError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            executor.execute("exec", &json!({"argv":["cat", "../outside.txt"]})),
            Err(ToolError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            executor.execute("exec", &json!({"argv":["rg", "--pre", "cat", "needle", "."]})),
            Err(ToolError::CommandDenied(_))
        ));
        assert!(matches!(
            executor.execute(
                "exec",
                &json!({"argv":["rg", "--hostname-bin=touch", "needle", "."]})
            ),
            Err(ToolError::CommandDenied(_))
        ));
        assert!(matches!(
            executor.execute("exec", &json!({"argv":["git", "diff", "--ext-diff"]})),
            Err(ToolError::CommandDenied(_))
        ));
        assert!(matches!(
            executor.execute(
                "exec",
                &json!({"argv":["git", "grep", "--open-files-in-pager=touch", "needle"]})
            ),
            Err(ToolError::CommandDenied(_))
        ));
        assert!(matches!(
            executor.execute("exec", &json!({"argv":["find", "."]})),
            Err(ToolError::ReadOnlyDenied)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_path, workspace.path().join("outside-link"))
                .expect("outside symlink fixture");
            assert!(matches!(
                executor.execute("exec", &json!({"argv":["cat", "outside-link"]})),
                Err(ToolError::OutsideWorkspace(_))
            ));
        }
    }
}
