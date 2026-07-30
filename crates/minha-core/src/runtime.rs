//! Token-conscious orchestration over direct ChatGPT Codex model turns.

use crate::{
    Config, Store,
    auth::{
        AuthError, AuthRecord, CodexOAuthClient, active_account_profile, enabled_account_records,
        load_default_auth, openai_oauth_config, save_account_profile, save_default_auth,
    },
    cache::{
        CacheClass, CacheEntry, CachePolicy, HotCache, LookupMode, ObservedInputManifest, cache_key,
        contains_secret,
    },
    clarify::{
        analyze as analyze_issue, apply_answers as apply_clarification_answers, confirm as confirm_issue,
        make_fallback_batch, needs_clarification, prepare_brief, render_brief, reopen as reopen_issue,
        sanitize_model_batch, should_consult_terra,
    },
    context::{ContextPolicy, estimate_tokens},
    deepseek::DeepSeekClient,
    executor::{
        CoordinationContext, ExecutorPolicy, InputRequest, ToolError, ToolExecutor, ToolOutcome,
        tool_definitions,
    },
    facts::{BoardEntry, BoardKind},
    instructions::{
        AgentDefinition, Skill, discover_agents, discover_instructions, discover_skills, load_skill,
    },
    memory::{MemoryRecord, MemoryScope},
    protocol::{
        AgentState, BoardEntryView, CatalogModel, ClarificationStatus, EventAgentId, ExitState,
        IncidentSeverity, IncidentView, IssueClarificationView, ItemId, Mode, PlanTask, PlanTaskState,
        RequestId, RunId, RunPhase, RuntimeEvent, TerminationReason, TodoItem, TodoState,
    },
    provider::{
        ChatGptClient, DEEPSEEK_BASE_URL, ModelCatalog, ModelDescriptor, ProviderBalanceV1, ProviderError,
        ProviderId, ProviderStreamEvent, ToolCall, TurnRequest, TurnResult,
    },
    provider_credentials::{default_path as provider_credentials_path, load_deepseek_key},
    store::{AgentRecord, TaskRecord},
    usage::{TokenUsage, reserve_reached},
    worktree::{GitError, GitRepo, copy_workspace, diff_snapshots},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;

pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const MAX_PLAN_TASKS: usize = 16;
const HOT_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
enum RuntimeProviderClient {
    ChatGpt(ChatGptClient),
    DeepSeek(DeepSeekClient),
}

impl RuntimeProviderClient {
    const fn provider_id(&self) -> ProviderId {
        match self {
            Self::ChatGpt(_) => ProviderId::ChatGptCodex,
            Self::DeepSeek(_) => ProviderId::DeepSeek,
        }
    }

    async fn fetch_models(&self, etag: Option<&str>) -> Result<ModelCatalog, ProviderError> {
        match self {
            Self::ChatGpt(client) => client.fetch_models(etag).await,
            Self::DeepSeek(client) => client.fetch_models().await,
        }
    }

    async fn fetch_balance(&self) -> Option<Result<ProviderBalanceV1, ProviderError>> {
        match self {
            Self::ChatGpt(_) => None,
            Self::DeepSeek(client) => Some(client.fetch_balance().await),
        }
    }

    fn install_model_catalog(&self, models: &[ModelDescriptor]) {
        if let Self::ChatGpt(client) = self {
            client.install_model_catalog(models);
        }
    }

    async fn turn(&self, mut request: TurnRequest) -> Result<TurnResult, ProviderError> {
        request.model = provider_model_slug(&request.model).to_owned();
        match self {
            Self::ChatGpt(client) => client.turn(request).await,
            Self::DeepSeek(client) => client.turn(request).await,
        }
    }

    async fn turn_stream<F>(&self, mut request: TurnRequest, on_event: F) -> Result<TurnResult, ProviderError>
    where
        F: FnMut(ProviderStreamEvent),
    {
        request.model = provider_model_slug(&request.model).to_owned();
        match self {
            Self::ChatGpt(client) => client.turn_stream(request, on_event).await,
            Self::DeepSeek(client) => client.turn_stream(request, on_event).await,
        }
    }
}

impl From<ChatGptClient> for RuntimeProviderClient {
    fn from(client: ChatGptClient) -> Self {
        Self::ChatGpt(client)
    }
}

fn provider_model_slug(model: &str) -> &str {
    model
        .strip_prefix("deepseek/")
        .or_else(|| model.strip_prefix("chatgpt/"))
        .unwrap_or(model)
}

fn provider_for_model(model: &str) -> ProviderId {
    if model.starts_with("deepseek/") || model.starts_with("deepseek-") {
        ProviderId::DeepSeek
    } else {
        ProviderId::ChatGptCodex
    }
}

fn reasoning_for_turn(model: &str, role: &str, turn: usize, configured: &str) -> String {
    if provider_for_model(model) != ProviderId::DeepSeek {
        return configured.to_owned();
    }
    let _ = (role, turn);
    // Minha deliberately uses both V4 tiers as reasoning models. Flash is
    // Max-only; Pro prefers Max and never uses non-thinking mode.
    "max".into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Auto,
    Implement,
    Plan,
    Audit,
    Review,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JudgeVerdictV1 {
    Verified,
    Incomplete,
    Blocked,
    Inconclusive,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JudgeReportV1 {
    schema_version: u16,
    verdict: JudgeVerdictV1,
    summary: String,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    findings: Vec<String>,
}

fn parse_judge_report(text: &str) -> Option<JudgeReportV1> {
    let opening = "<minha-judge>";
    let closing = "</minha-judge>";
    let start = text.find(opening)?.saturating_add(opening.len());
    let end = text[start..].find(closing)?.saturating_add(start);
    let report = serde_json::from_str::<JudgeReportV1>(text[start..end].trim()).ok()?;
    (report.schema_version == 1 && !report.summary.trim().is_empty()).then_some(report)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub state: ExitState,
    pub kind: RunKind,
    pub model: Option<String>,
    pub text: String,
    pub question: Option<InputRequestView>,
    #[serde(default)]
    pub clarification: Option<IssueClarificationView>,
    pub usage: TokenUsage,
    pub agents_used: usize,
    pub worktrees: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InputRequestView {
    pub question: String,
    pub options: Vec<String>,
}

impl From<InputRequest> for InputRequestView {
    fn from(value: InputRequest) -> Self {
        Self {
            question: value.question,
            options: value.options,
        }
    }
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("state error: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("ChatGPT login required; run `minha login`")]
    LoginRequired,
    #[error("authenticated token has no ChatGPT account id; log in again")]
    MissingAccountId,
    #[error("authentication error: {0}")]
    Auth(#[from] AuthError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("provider credential error: {0}")]
    ProviderCredential(#[from] crate::provider_credentials::CredentialError),
    #[error("required model is not available to this account: {0}")]
    ModelUnavailable(String),
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("turn interrupted")]
    Interrupted,
}

#[derive(Default)]
struct RunControl {
    steering: VecDeque<String>,
    interrupted: bool,
    cooperative_pause: bool,
    approved_exec_once: Option<Vec<String>>,
    force_compaction: bool,
    bypass_cache: bool,
    budget_tokens: u64,
}

#[derive(Clone)]
pub struct Harness {
    root: PathBuf,
    workspace_id: String,
    pub config: Config,
    pub store: Store,
    controls: Arc<Mutex<HashMap<RunId, RunControl>>>,
    account_clients: Arc<Mutex<Vec<RuntimeProviderClient>>>,
    hot_cache: Arc<Mutex<HotCache>>,
    model_context_limits: Arc<Mutex<HashMap<String, u64>>>,
    deepseek_balance_percent: Arc<Mutex<Option<f64>>>,
}

impl Harness {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, HarnessError> {
        let root = root.as_ref().canonicalize()?;
        let config = Config::discover(&root)?;
        let store = Store::open(&config.database_path)?;
        let workspace = store.ensure_workspace(&root)?;
        let _ = store.reclaim_expired_leases()?;
        if config.books.enabled {
            let _ = store.sync_bundled_books()?;
        }
        if config.cache.enabled {
            let _ = store.prune_cache(&workspace.id, config.cache.max_bytes)?;
        }
        for run in store.list_runs(1_000)? {
            store.attach_run_workspace(run.id, &workspace.id)?;
        }
        let hot_cache = HotCache::with_limits(
            config.cache.hot_entries,
            config.cache.max_bytes.min(HOT_CACHE_MAX_BYTES),
        );
        Ok(Self {
            root,
            workspace_id: workspace.id,
            config,
            store,
            controls: Arc::new(Mutex::new(HashMap::new())),
            account_clients: Arc::new(Mutex::new(Vec::new())),
            hot_cache: Arc::new(Mutex::new(hot_cache)),
            model_context_limits: Arc::new(Mutex::new(HashMap::new())),
            deepseek_balance_percent: Arc::new(Mutex::new(None)),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    fn cache_policy(&self) -> CachePolicy {
        CachePolicy::new(
            self.config.cache.max_bytes,
            Duration::from_secs(u64::from(self.config.cache.max_age_days) * 24 * 60 * 60),
        )
    }

    fn hot_cached_result(&self, key: &str) -> Result<Option<Vec<u8>>, HarnessError> {
        let value = self
            .hot_cache
            .lock()
            .get(key, SystemTime::now(), self.cache_policy(), LookupMode::FreshOnly)
            .map(<[u8]>::to_vec);
        let Some(value) = value else {
            return Ok(None);
        };
        if self
            .store
            .touch_cached_result(&self.workspace_id, key, value.len() as u64)?
        {
            return Ok(Some(value));
        }
        self.hot_cache.lock().remove(key);
        Ok(None)
    }

    fn remember_hot_result(&self, key: &str, class: CacheClass, value: &[u8]) {
        let now = SystemTime::now();
        self.hot_cache.lock().insert(CacheEntry {
            key: key.to_owned(),
            class,
            bytes: value.to_vec(),
            stored_at: now,
            last_used_at: now,
            hits: 0,
            pinned: false,
        });
    }

    pub async fn models(&self) -> Result<Vec<ModelDescriptor>, HarnessError> {
        let client = self.client().await?;
        self.fetch_model_catalog(&client, None).await
    }

    pub fn queue_steering(&self, run_id: RunId, text: &str) -> Result<(), HarnessError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        self.controls
            .lock()
            .entry(run_id)
            .or_default()
            .steering
            .push_back(text.to_owned());
        self.store
            .append_message(run_id, "user", &json!({"text": text, "steering": true}), false)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SteeringQueued {
                text: text.to_owned(),
            },
        )?;
        Ok(())
    }

    pub fn interrupt(&self, run_id: RunId) -> Result<(), HarnessError> {
        self.controls.lock().entry(run_id).or_default().interrupted = true;
        self.store
            .update_run_state(run_id, ExitState::Cancelled, None, None, None)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::TurnInterrupted {
                reason: "interrupted by user".into(),
            },
        )?;
        Ok(())
    }

    /// Requests a cooperative stop at the next safe model/tool boundary.
    /// Unlike interruption, this keeps the run resumable and does not abort an
    /// in-flight provider request or mutation.
    pub fn pause(&self, run_id: RunId) -> Result<(), HarnessError> {
        self.controls.lock().entry(run_id).or_default().cooperative_pause = true;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Waiting,
                detail: "pausing at the next safe boundary".into(),
            },
        )?;
        Ok(())
    }

    pub fn request_compaction(&self, run_id: RunId) -> Result<(), HarnessError> {
        self.controls.lock().entry(run_id).or_default().force_compaction = true;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::Warning {
                message: "context compaction queued for the next model boundary".into(),
            },
        )?;
        Ok(())
    }

    pub fn set_cache_bypass(&self, run_id: RunId, bypass: bool) -> Result<(), HarnessError> {
        self.controls.lock().entry(run_id).or_default().bypass_cache = bypass;
        if bypass {
            self.store.record_cache_bypass(&self.workspace_id)?;
        }
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::Warning {
                message: if bypass {
                    "fresh mode enabled for reusable model-result cache"
                } else {
                    "reusable model-result cache enabled"
                }
                .into(),
            },
        )?;
        Ok(())
    }

    pub async fn continue_session(&self, run_id: RunId, text: &str) -> Result<RunOutcome, HarnessError> {
        let run = self.store.run(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            ))
        })?;
        self.controls.lock().insert(run_id, RunControl::default());
        self.store
            .append_message(run_id, "user", &json!({"text": text, "steering": false}), false)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::UserMessage {
                text: text.into(),
                steering: false,
            },
        )?;
        self.store
            .update_run_state(run_id, ExitState::Running, None, None, None)?;
        self.store
            .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SessionState {
                state: ExitState::Running,
            },
        )?;
        let prompt = format!(
            "Original session goal: {}\nPrevious durable summary: {}\nNew user request: {}\nContinue the same session. Inspect current repository state before acting.",
            run.goal,
            run.summary.as_deref().unwrap_or("none"),
            text
        );
        let operation = self.run_single(
            run_id,
            RunKind::Implement,
            &prompt,
            &self.config.models.lead,
            false,
            "Luna session lead",
        );
        self.capture_failure(run_id, operation).await
    }

    pub async fn run(&self, kind: RunKind, goal: &str) -> Result<RunOutcome, HarnessError> {
        self.run_with_cache_mode(kind, goal, false).await
    }

    pub async fn run_fresh(&self, kind: RunKind, goal: &str) -> Result<RunOutcome, HarnessError> {
        self.run_with_cache_mode(kind, goal, true).await
    }

    async fn run_with_cache_mode(
        &self,
        kind: RunKind,
        goal: &str,
        bypass_cache: bool,
    ) -> Result<RunOutcome, HarnessError> {
        let run = self.store.create_run(goal, mode_for(kind))?;
        self.store.attach_run_workspace(run.id, &self.workspace_id)?;
        self.controls.lock().insert(
            run.id,
            RunControl {
                bypass_cache,
                ..RunControl::default()
            },
        );
        if bypass_cache {
            self.store.record_cache_bypass(&self.workspace_id)?;
        }
        self.store
            .update_run_state(run.id, ExitState::Running, None, None, None)?;
        self.store.record_runtime_event(
            run.id,
            RuntimeEvent::SessionStarted {
                kind: format!("{kind:?}").to_ascii_lowercase(),
                goal: goal.to_owned(),
            },
        )?;
        self.store.record_runtime_event(
            run.id,
            RuntimeEvent::UserMessage {
                text: goal.to_owned(),
                steering: false,
            },
        )?;
        self.store
            .append_message(run.id, "user", &json!({"text": goal}), false)?;
        let clarification = analyze_issue(goal, run_kind_name(kind));
        if kind == RunKind::Implement && needs_clarification(&clarification) {
            self.store.save_issue_clarification(run.id, &clarification)?;
            self.store.record_runtime_event(
                run.id,
                RuntimeEvent::ClarificationStarted {
                    clarification: clarification.clone(),
                },
            )?;
        }
        if bypass_cache {
            self.store.record_runtime_event(
                run.id,
                RuntimeEvent::Warning {
                    message: "fresh run: reusable local model-result cache is bypassed".into(),
                },
            )?;
        }

        self.capture_failure(run.id, self.run_inner(run.id, kind, goal))
            .await
    }

    async fn capture_failure<T, F>(&self, run_id: RunId, operation: F) -> Result<T, HarnessError>
    where
        F: std::future::Future<Output = Result<T, HarnessError>>,
    {
        match operation.await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.record_run_failure(run_id, &error)?;
                Err(error)
            }
        }
    }

    fn record_run_failure(&self, run_id: RunId, error: &HarnessError) -> Result<(), HarnessError> {
        let state = match error {
            HarnessError::LoginRequired | HarnessError::MissingAccountId | HarnessError::Auth(_) => {
                ExitState::AuthUnavailable
            }
            HarnessError::ModelUnavailable(_) => ExitState::ModelUnavailable,
            HarnessError::Interrupted => ExitState::Cancelled,
            _ => ExitState::Failed,
        };
        self.store
            .update_run_state(run_id, state, None, Some(&error.to_string()), None)?;
        let incident = incident_for(run_id, error);
        self.store
            .record_incident(Some(run_id), &incident, Some(&error.to_string()))?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::Incident {
                incident: incident.clone(),
            },
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::Error {
                state,
                message: error.to_string(),
            },
        )?;
        Ok(())
    }

    pub async fn resume_with_answer(&self, run_id: RunId, answer: &str) -> Result<RunOutcome, HarnessError> {
        let run = self.store.run(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "run not found"))
        })?;
        if let Some(clarification) = self.store.issue_clarification(run_id)?
            && matches!(
                clarification.status,
                ClarificationStatus::Collecting | ClarificationStatus::Reviewing
            )
        {
            let id = if clarification.status == ClarificationStatus::Reviewing {
                "$action".to_owned()
            } else {
                clarification
                    .pending_batch
                    .as_ref()
                    .and_then(|batch| batch.questions.first())
                    .map(|question| question.id.clone())
                    .unwrap_or_else(|| "$action".into())
            };
            return self
                .resume_with_clarification_answers(run_id, &[(id, answer.to_owned())])
                .await;
        }
        if run.state == ExitState::ApprovalRequired {
            let approval = self.store.events(run_id)?.into_iter().rev().find_map(|envelope| {
                if let RuntimeEvent::Approval {
                    request_id, command, ..
                } = envelope.event
                {
                    Some((request_id, command))
                } else {
                    None
                }
            });
            let approved = is_affirmative(answer);
            let mut controls = self.controls.lock();
            let control = controls.entry(run_id).or_default();
            control.approved_exec_once = if approved {
                approval.as_ref().and_then(|(_, command)| command.clone())
            } else {
                None
            };
            drop(controls);
            if let Some((request_id, _)) = approval {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::RequestResolved {
                        request_id,
                        answer: answer.to_owned(),
                        approved: Some(approved),
                    },
                )?;
            }
        }
        let question = run.pending_question.clone().unwrap_or_default();
        self.store
            .append_message(run_id, "user", &json!({"answer": answer}), false)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::UserMessage {
                text: answer.to_owned(),
                steering: false,
            },
        )?;
        self.store
            .update_run_state(run_id, ExitState::Running, None, None, None)?;
        self.store
            .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SessionState {
                state: ExitState::Running,
            },
        )?;
        if run.state == ExitState::NeedsInput
            && let Some(task) = self
                .store
                .tasks(run_id)?
                .into_iter()
                .find(|task| task.state == PlanTaskState::Blocked)
        {
            let resume_context = format!("User answer to task blocker: {answer}");
            self.store.update_task(
                run_id,
                &task.task_id,
                PlanTaskState::Pending,
                None,
                task.attempt,
                task.generation.saturating_add(1),
                Some(&resume_context),
            )?;
            self.record_board_note(
                run_id,
                BoardKind::Decision,
                &format!("User answered task {}", task.task_id),
                answer,
                Some(&task.task_id),
                None,
            )?;
            let kind = stored_run_kind(&self.store.events(run_id)?).unwrap_or(RunKind::Implement);
            return self
                .capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
                .await;
        }
        let goal = format!(
            "{}\n\nA worker had to ask: {}\nUser answer: {}\nResume and finish the original goal.",
            run.goal, question, answer
        );
        let operation = self.run_single(
            run_id,
            RunKind::Implement,
            &goal,
            &self.config.models.lead,
            false,
            "resumed Luna lead",
        );
        self.capture_failure(run_id, operation).await
    }

    pub async fn resume_with_clarification_answers(
        &self,
        run_id: RunId,
        answers: &[(String, String)],
    ) -> Result<RunOutcome, HarnessError> {
        let run = self.store.run(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "run not found"))
        })?;
        let mut clarification = self.store.issue_clarification(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "run has no active issue clarification",
            ))
        })?;
        let answer_map = answers
            .iter()
            .map(|(id, value)| (id.clone(), Value::String(value.clone())))
            .collect::<serde_json::Map<String, Value>>();
        self.store.append_message(
            run_id,
            "user",
            &json!({"clarification_answers": answer_map}),
            false,
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::UserMessage {
                text: clarification_answer_display(&clarification, answers),
                steering: false,
            },
        )?;

        if clarification.status == ClarificationStatus::Reviewing {
            let action = answers
                .iter()
                .find(|(id, _)| id == "$action")
                .map(|(_, value)| value.trim().to_ascii_lowercase())
                .unwrap_or_default();
            match action.as_str() {
                "confirm" | "confirmed" | "yes" => confirm_issue(&mut clarification),
                "edit" => {
                    let note = answers
                        .iter()
                        .find(|(id, _)| id == "$edit")
                        .map(|(_, value)| value.as_str());
                    reopen_issue(&mut clarification, note);
                }
                "keep clarifying" | "keep_clarifying" => reopen_issue(&mut clarification, None),
                "cancel" => clarification.status = ClarificationStatus::Cancelled,
                _ => {
                    return Err(HarnessError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "reviewing an issue brief requires confirm, edit, keep_clarifying, or cancel",
                    )));
                }
            }
        } else {
            apply_clarification_answers(&mut clarification, answers);
            if clarification.status == ClarificationStatus::Reviewing && clarification.brief.is_none() {
                prepare_brief(&mut clarification, &run.goal);
            }
        }

        self.store.save_issue_clarification(run_id, &clarification)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::ClarificationUpdated {
                clarification: clarification.clone(),
            },
        )?;
        let kind = stored_run_kind(&self.store.events(run_id)?).unwrap_or(RunKind::Implement);
        match clarification.status {
            ClarificationStatus::Reviewing => {
                self.clarification_outcome(run_id, kind, clarification, TokenUsage::default(), None, 0)
            }
            ClarificationStatus::Confirmed => {
                if let Some(brief) = clarification.brief.as_ref() {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::ClarificationConfirmed { brief: brief.clone() },
                    )?;
                    let mut entry = BoardEntry::session(
                        &self.workspace_id,
                        run_id,
                        BoardKind::Decision,
                        "Confirmed issue brief",
                        bound(&render_brief(brief), 4_000),
                    );
                    entry.scope = crate::facts::BoardScope::Project;
                    self.store.insert_board_entry(&entry)?;
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::BoardChanged {
                            entry: board_entry_view(&entry),
                        },
                    )?;
                }
                self.store
                    .update_run_state(run_id, ExitState::Running, None, None, None)?;
                self.store
                    .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
                self.capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
                    .await
            }
            ClarificationStatus::Collecting => {
                self.store
                    .update_run_state(run_id, ExitState::Running, None, None, None)?;
                self.store
                    .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
                self.capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
                    .await
            }
            ClarificationStatus::Cancelled => {
                self.store.update_run_state(
                    run_id,
                    ExitState::Cancelled,
                    None,
                    Some("Issue clarification cancelled; no work was started."),
                    None,
                )?;
                Ok(RunOutcome {
                    run_id,
                    state: ExitState::Cancelled,
                    kind,
                    model: None,
                    text: "Issue clarification cancelled; no work was started.".into(),
                    question: None,
                    clarification: Some(clarification),
                    usage: TokenUsage::default(),
                    agents_used: 0,
                    worktrees: Vec::new(),
                })
            }
        }
    }

    /// Resume a run after its configured account-usage reserve has protected
    /// the remaining quota. The original run kind is recovered from the event
    /// log so audits and reviews stay read-only.
    pub async fn resume_paused(&self, run_id: RunId) -> Result<RunOutcome, HarnessError> {
        let run = self.store.run(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "run not found"))
        })?;
        if run.state != ExitState::UsagePaused {
            return Err(HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "run is not paused by the usage reserve",
            )));
        }
        let kind = stored_run_kind(&self.store.events(run_id)?).unwrap_or(RunKind::Implement);
        self.store
            .append_message(run_id, "user", &json!({"resume_after_usage_reset": true}), false)?;
        self.store
            .update_run_state(run_id, ExitState::Running, None, None, None)?;
        self.store
            .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SessionState {
                state: ExitState::Running,
            },
        )?;
        self.capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
            .await
    }

    /// Retry a failed or inconclusive run in place so its transcript, board,
    /// recovery patches, and completed task graph remain available.
    pub async fn retry_session(&self, run_id: RunId) -> Result<RunOutcome, HarnessError> {
        let run = self.store.run(run_id)?.ok_or_else(|| {
            HarnessError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "run not found"))
        })?;
        if run.state == ExitState::Running {
            return Err(HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "run is already active",
            )));
        }
        for task in self.store.tasks(run_id)? {
            if matches!(task.state, PlanTaskState::Failed | PlanTaskState::Blocked) {
                self.store.update_task(
                    run_id,
                    &task.task_id,
                    PlanTaskState::Pending,
                    None,
                    0,
                    task.generation.saturating_add(1),
                    Some("explicit user retry"),
                )?;
            }
        }
        self.controls.lock().insert(run_id, RunControl::default());
        self.store
            .append_message(run_id, "user", &json!({"retry": true}), false)?;
        self.store
            .update_run_state(run_id, ExitState::Running, None, None, None)?;
        self.store
            .record_runtime_event(run_id, RuntimeEvent::SessionResumed)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Recovering,
                detail: "explicit retry in the same durable session".into(),
            },
        )?;
        let kind = stored_run_kind(&self.store.events(run_id)?).unwrap_or(RunKind::Implement);
        self.capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
            .await
    }

    async fn run_inner(&self, run_id: RunId, kind: RunKind, goal: &str) -> Result<RunOutcome, HarnessError> {
        let used = self.store.usage_totals(Some(run_id))?;
        self.controls.lock().entry(run_id).or_default().budget_tokens =
            used.session_input.saturating_add(used.session_output);
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Preflight,
                detail: "checking authentication and exact model availability".into(),
            },
        )?;
        self.process_pending_memory_extractions(run_id)?;
        let clients = self.clients().await?;
        let client = clients.first().cloned().ok_or(HarnessError::LoginRequired)?;
        *self.account_clients.lock() = clients.clone();
        for provider_client in &clients {
            if let Some(balance) = provider_client.fetch_balance().await {
                match balance {
                    Ok(balance) => self.record_provider_balance(run_id, &balance)?,
                    Err(error) => {
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::Warning {
                                message: format!("DeepSeek balance is temporarily unavailable: {error}"),
                            },
                        )?;
                    }
                }
            }
        }
        self.model_context_limits.lock().clear();
        let mut models = Vec::new();
        let mut last_catalog_error = None;
        for provider_client in &clients {
            let provider_name = match provider_client.provider_id() {
                ProviderId::ChatGptCodex => "chatgpt_codex",
                ProviderId::DeepSeek => "deepseek",
            };
            match self.fetch_model_catalog(provider_client, Some(run_id)).await {
                Ok(provider_models) => {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::ProviderState {
                            provider: provider_name.into(),
                            enabled: true,
                            healthy: Some(true),
                            detail: format!("{} model(s) available", provider_models.len()),
                        },
                    )?;
                    models.extend(provider_models);
                }
                Err(error) => {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::ProviderState {
                            provider: provider_name.into(),
                            enabled: true,
                            healthy: Some(false),
                            detail: error.to_string(),
                        },
                    )?;
                    last_catalog_error = Some(error);
                }
            }
        }
        if models.is_empty() {
            return Err(last_catalog_error.unwrap_or(HarnessError::LoginRequired));
        }
        let available = models
            .into_iter()
            .flat_map(|model| {
                let slug = model.slug;
                let qualified = if slug.starts_with("deepseek-") {
                    format!("deepseek/{slug}")
                } else {
                    format!("chatgpt/{slug}")
                };
                [slug, qualified]
            })
            .collect::<HashSet<_>>();
        let balance_percent = *self.deepseek_balance_percent.lock();
        let available = if balance_percent
            .is_some_and(|percent| percent <= f64::from(self.config.budgets.deepseek_hard_reserve_percent))
        {
            available
                .into_iter()
                .filter(|model| provider_for_model(model) != ProviderId::DeepSeek)
                .collect()
        } else if balance_percent
            .is_some_and(|percent| percent <= f64::from(self.config.budgets.deepseek_soft_reserve_percent))
        {
            let openai = available
                .iter()
                .filter(|model| provider_for_model(model) == ProviderId::ChatGptCodex)
                .cloned()
                .collect::<HashSet<_>>();
            if openai.is_empty() { available } else { openai }
        } else {
            available
        };
        let clarification = self.store.issue_clarification(run_id)?;
        if let Some(clarification) = clarification.as_ref() {
            match clarification.status {
                ClarificationStatus::Collecting => {
                    return self
                        .run_clarification_round(
                            run_id,
                            kind,
                            goal,
                            client,
                            &available,
                            clarification.clone(),
                        )
                        .await;
                }
                ClarificationStatus::Reviewing => {
                    return self.clarification_outcome(
                        run_id,
                        kind,
                        clarification.clone(),
                        TokenUsage::default(),
                        None,
                        0,
                    );
                }
                ClarificationStatus::Cancelled => {
                    self.store.update_run_state(
                        run_id,
                        ExitState::Cancelled,
                        None,
                        Some("Issue clarification cancelled; no work was started."),
                        None,
                    )?;
                    return Ok(RunOutcome {
                        run_id,
                        state: ExitState::Cancelled,
                        kind,
                        model: None,
                        text: "Issue clarification cancelled; no work was started.".into(),
                        question: None,
                        clarification: Some(clarification.clone()),
                        usage: TokenUsage::default(),
                        agents_used: 0,
                        worktrees: Vec::new(),
                    });
                }
                ClarificationStatus::Confirmed => {}
            }
        }
        let confirmed_goal = clarification
            .as_ref()
            .filter(|clarification| clarification.status == ClarificationStatus::Confirmed)
            .and_then(|clarification| clarification.brief.as_ref())
            .map(render_brief);
        let goal = confirmed_goal.as_deref().unwrap_or(goal);
        let lead_model = routed_lead_model(goal, &available, &self.config)?;
        let planner_model = first_available(
            &available,
            &[
                &self.config.models.planner,
                "deepseek/deepseek-v4-pro",
                &self.config.models.lead,
                &self.config.models.complex_lead,
            ],
        )?;

        match kind {
            RunKind::Auto => {
                self.run_auto(run_id, goal, client, &available, lead_model, planner_model)
                    .await
            }
            RunKind::Plan => {
                self.run_single_with_client(run_id, kind, goal, planner_model, true, "planner lead", client)
                    .await
            }
            RunKind::Review => {
                let review_model = first_available(
                    &available,
                    &[
                        &self.config.models.worker_fast,
                        "deepseek/deepseek-v4-flash",
                        lead_model,
                    ],
                )?;
                self.run_single_with_client(run_id, kind, goal, review_model, true, "reviewer", client)
                    .await
            }
            RunKind::Audit => self.run_audit(run_id, goal, client, &available, lead_model).await,
            RunKind::Implement => {
                self.run_implementation(run_id, goal, client, &available, lead_model, planner_model)
                    .await
            }
        }
    }

    async fn run_clarification_round(
        &self,
        run_id: RunId,
        kind: RunKind,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        mut clarification: IssueClarificationView,
    ) -> Result<RunOutcome, HarnessError> {
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Clarifying,
                detail: format!(
                    "helping describe the issue · ambiguity {}/100",
                    clarification.meter.overall
                ),
            },
        )?;
        let mut usage = TokenUsage::default();
        let mut agents_used = 0;
        let mut model_used = None;
        let mut batch = None;
        let mut consultation = String::new();
        if should_consult_terra(&clarification, goal)
            && available.contains(&self.config.models.consult_ambiguous)
        {
            let executor = ToolExecutor::new(&self.root, true)?;
            let system = self.system_prompt(goal, "ambiguity consultant Terra", true)?;
            let state = serde_json::to_string(&clarification).unwrap_or_else(|_| "{}".into());
            let prompt = format!(
                "Review this unresolved high-impact issue clarification. Identify at most three decisions that materially affect safety or scope. Do not ask the user directly and do not call tools. Return terse advice for the Luna clarifier.\n\nIssue: {}\nState: {}",
                bound(goal, 4_000),
                bound(&state, 10_000),
            );
            match self
                .run_agent(
                    run_id,
                    &client,
                    &self.config.models.consult_ambiguous,
                    &system,
                    &prompt,
                    executor,
                    "ambiguity consultant Terra",
                )
                .await
            {
                Ok(result) => {
                    usage = add_usage(usage, result.usage);
                    consultation = bound(&result.text, 3_000);
                    agents_used += 1;
                    model_used = Some(self.config.models.consult_ambiguous.clone());
                }
                Err(error) => {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::Warning {
                            message: format!("optional Terra ambiguity consultation was skipped: {error}"),
                        },
                    )?;
                }
            }
        }
        if available.contains(&self.config.models.lead) {
            let executor = ToolExecutor::new(&self.root, true)?;
            let mut system = self.system_prompt(goal, "issue clarifier Luna lead", true)?;
            system.push_str(
                "\nHelp a non-expert explain one issue without interrogating them. Ask only questions whose answers could change scope, safety, diagnosis, or acceptance. Never repeat a resolved dimension. Use bounded read-only evidence only when it can replace a question. Do not read credential, token, key, or secret-like paths. A screenshot path is evidence that an image exists, not permission to claim you inspected its contents. Return exactly one <minha-clarification> JSON object with a questions array. Each question needs dimension, header, question, 2-3 options with value/label/description/recommended, plus allow_free_text and allow_not_sure. Dimensions must be unresolved IDs supplied by the caller. Be warm, plain, and concise.",
            );
            let state = serde_json::to_string(&clarification).unwrap_or_else(|_| "{}".into());
            let prompt = format!(
                "Original report:\n{}\n\nCurrent explainable clarification state:\n{}\n\nOptional high-impact consultation:\n{}\n\nAsk the next one to three smallest useful questions. Do not solve the issue yet.",
                bound(goal, 4_000),
                bound(&state, 12_000),
                if consultation.is_empty() {
                    "none"
                } else {
                    &consultation
                },
            );
            match self
                .run_agent(
                    run_id,
                    &client,
                    &self.config.models.lead,
                    &system,
                    &prompt,
                    executor,
                    "issue clarifier Luna lead",
                )
                .await
            {
                Ok(result) => {
                    usage = add_usage(usage, result.usage);
                    batch = sanitize_model_batch(&result.text, &clarification);
                    agents_used += 1;
                    model_used = Some(self.config.models.lead.clone());
                }
                Err(error) => {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::Warning {
                            message: format!(
                                "Luna clarification was unavailable; using local scoped questions: {error}"
                            ),
                        },
                    )?;
                }
            }
        }
        let batch = batch.unwrap_or_else(|| make_fallback_batch(&clarification));
        clarification.round = batch.round;
        clarification.pending_batch = Some(batch);
        self.store.save_issue_clarification(run_id, &clarification)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::ClarificationUpdated {
                clarification: clarification.clone(),
            },
        )?;
        self.clarification_outcome(run_id, kind, clarification, usage, model_used, agents_used)
    }

    fn clarification_outcome(
        &self,
        run_id: RunId,
        kind: RunKind,
        clarification: IssueClarificationView,
        usage: TokenUsage,
        model: Option<String>,
        agents_used: usize,
    ) -> Result<RunOutcome, HarnessError> {
        let (text, question) = if clarification.status == ClarificationStatus::Reviewing {
            let text = clarification
                .brief
                .as_ref()
                .map(render_brief)
                .unwrap_or_else(|| "Issue brief is ready for review.".into());
            (
                format!("{text}\n\nConfirm, edit, or keep clarifying before work begins."),
                Some(InputRequestView {
                    question: "Is this issue brief accurate?".into(),
                    options: vec!["confirm".into(), "edit".into(), "keep clarifying".into()],
                }),
            )
        } else {
            let question = clarification.pending_batch.as_ref().and_then(|batch| {
                batch.questions.first().map(|question| InputRequestView {
                    question: question.question.clone(),
                    options: question
                        .options
                        .iter()
                        .map(|option| option.label.clone())
                        .collect(),
                })
            });
            (
                "I need one detail before I can start safely. Choose the closest answer, or add a note."
                    .into(),
                question,
            )
        };
        self.store.update_run_state(
            run_id,
            ExitState::NeedsInput,
            model.as_deref(),
            Some(&text),
            question.as_ref().map(|question| question.question.as_str()),
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SessionState {
                state: ExitState::NeedsInput,
            },
        )?;
        Ok(RunOutcome {
            run_id,
            state: ExitState::NeedsInput,
            kind,
            model,
            text,
            question,
            clarification: Some(clarification),
            usage,
            agents_used,
            worktrees: Vec::new(),
        })
    }

    async fn fetch_model_catalog(
        &self,
        client: &RuntimeProviderClient,
        run_id: Option<RunId>,
    ) -> Result<Vec<ModelDescriptor>, HarnessError> {
        const FRESH_MINUTES: i64 = 15;
        const STALE_FALLBACK_HOURS: i64 = 24;
        if client.provider_id() == ProviderId::DeepSeek {
            let catalog = client.fetch_models(None).await?;
            self.emit_model_catalog(
                run_id,
                ProviderId::DeepSeek,
                &catalog.models,
                chrono::Utc::now(),
                false,
            )?;
            return Ok(catalog.models);
        }
        let cached = self.store.model_catalog(&self.workspace_id)?;
        let now = chrono::Utc::now();
        if let Some(cached) = &cached
            && now.signed_duration_since(cached.fetched_at) <= chrono::Duration::minutes(FRESH_MINUTES)
        {
            client.install_model_catalog(&cached.models);
            self.emit_model_catalog(
                run_id,
                ProviderId::ChatGptCodex,
                &cached.models,
                cached.fetched_at,
                true,
            )?;
            return Ok(cached.models.clone());
        }

        match client
            .fetch_models(cached.as_ref().and_then(|catalog| catalog.etag.as_deref()))
            .await
        {
            Ok(catalog) if catalog.not_modified => {
                let cached = cached.ok_or(HarnessError::Provider(ProviderError::InvalidResponse(
                    "provider returned not-modified without a local catalog",
                )))?;
                self.store.touch_model_catalog(&self.workspace_id)?;
                client.install_model_catalog(&cached.models);
                self.emit_model_catalog(run_id, ProviderId::ChatGptCodex, &cached.models, now, true)?;
                Ok(cached.models)
            }
            Ok(catalog) => {
                let saved = self.store.save_model_catalog(
                    &self.workspace_id,
                    &catalog.models,
                    catalog.etag.as_deref(),
                )?;
                self.emit_model_catalog(
                    run_id,
                    ProviderId::ChatGptCodex,
                    &saved.models,
                    saved.fetched_at,
                    false,
                )?;
                Ok(saved.models)
            }
            Err(error) => {
                if let Some(cached) = cached
                    && now.signed_duration_since(cached.fetched_at)
                        <= chrono::Duration::hours(STALE_FALLBACK_HOURS)
                {
                    client.install_model_catalog(&cached.models);
                    if let Some(run_id) = run_id {
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::Warning {
                                message: format!(
                                    "model catalog refresh failed; using a cached catalog from {}: {error}",
                                    cached.fetched_at
                                ),
                            },
                        )?;
                    }
                    self.emit_model_catalog(
                        run_id,
                        ProviderId::ChatGptCodex,
                        &cached.models,
                        cached.fetched_at,
                        true,
                    )?;
                    Ok(cached.models)
                } else {
                    Err(error.into())
                }
            }
        }
    }

    fn record_provider_balance(
        &self,
        run_id: RunId,
        balance: &ProviderBalanceV1,
    ) -> Result<(), HarnessError> {
        let usd = balance
            .balances
            .iter()
            .find(|entry| entry.currency.eq_ignore_ascii_case("USD"))
            .or_else(|| balance.balances.first());
        let Some(usd) = usd else {
            return Ok(());
        };
        let current = usd.total.parse::<f64>().ok();
        let reserve_percent = current
            .map(|current| {
                self.store
                    .update_provider_balance_high_water(
                        &self.workspace_id,
                        "deepseek",
                        &usd.currency,
                        current,
                    )
                    .map(|high_water| {
                        if high_water > 0.0 {
                            (current / high_water * 100.0).clamp(0.0, 100.0)
                        } else {
                            0.0
                        }
                    })
            })
            .transpose()?;
        *self.deepseek_balance_percent.lock() = reserve_percent;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::ProviderBalance {
                provider: "deepseek".into(),
                available: balance.is_available,
                currency: usd.currency.clone(),
                total: usd.total.clone(),
                granted: usd.granted.clone(),
                topped_up: usd.topped_up.clone(),
                reserve_percent,
            },
        )?;
        Ok(())
    }

    fn emit_model_catalog(
        &self,
        run_id: Option<RunId>,
        provider: ProviderId,
        models: &[ModelDescriptor],
        fetched_at: chrono::DateTime<chrono::Utc>,
        cached: bool,
    ) -> Result<(), HarnessError> {
        {
            let mut limits = self.model_context_limits.lock();
            for model in models {
                if let Some(limit) = model.capabilities().context_window {
                    limits.insert(model.slug.clone(), limit);
                    let prefix = match provider {
                        ProviderId::ChatGptCodex => "chatgpt",
                        ProviderId::DeepSeek => "deepseek",
                    };
                    limits.insert(format!("{prefix}/{}", model.slug), limit);
                }
            }
        }
        if let Some(run_id) = run_id {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ModelCatalog {
                    models: models
                        .iter()
                        .map(|model| {
                            let capabilities = model.capabilities();
                            CatalogModel {
                                provider: match provider {
                                    ProviderId::ChatGptCodex => "chatgpt_codex",
                                    ProviderId::DeepSeek => "deepseek",
                                }
                                .into(),
                                slug: model.slug.clone(),
                                context_window: capabilities.context_window,
                                maximum_output: capabilities.maximum_output,
                                reasoning_levels: capabilities.reasoning_efforts,
                                supports_tools: capabilities.supports_tools,
                                supports_parallel_tool_calls: capabilities.supports_parallel_tool_calls,
                                capability_source: model
                                    .metadata
                                    .get("capability_source")
                                    .and_then(Value::as_str)
                                    .unwrap_or_else(|| {
                                        if capabilities.context_window.is_some() {
                                            "provider_catalog"
                                        } else {
                                            "fallback_table_v1"
                                        }
                                    })
                                    .into(),
                                pricing: model.metadata.get("pricing").cloned(),
                                capability_fetched_at: Some(fetched_at),
                            }
                        })
                        .collect(),
                    fetched_at,
                    cached,
                },
            )?;
        }
        Ok(())
    }

    async fn run_auto(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        lead_model: &str,
        planner_model: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let (selected, route_usage, routing_model, reason) = if let Some(mode) = local_auto_mode(goal) {
            (
                mode,
                TokenUsage::default(),
                None,
                "deterministic local intent routing",
            )
        } else {
            let classifier = first_available(
                available,
                &[&self.config.models.worker_fast, planner_model, lead_model],
            )?;
            let executor = ToolExecutor::new(&self.root, true)?;
            let system = "Classify intent without tools. Return exactly one tag: <minha-mode>chat</minha-mode>, <minha-mode>implement</minha-mode>, <minha-mode>plan</minha-mode>, <minha-mode>audit</minha-mode>, or <minha-mode>review</minha-mode>.";
            let route = self
                .run_agent(
                    run_id,
                    &client,
                    classifier,
                    system,
                    goal,
                    executor,
                    "intent classifier",
                )
                .await?;
            (
                parse_auto_mode(&route.text),
                route.usage,
                Some(classifier.to_owned()),
                "bounded no-tool classifier",
            )
        };
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RoutingDecision {
                mode: auto_mode_name(selected).into(),
                reason: reason.into(),
                provider: match provider_for_model(routing_model.as_deref().unwrap_or(lead_model)) {
                    ProviderId::ChatGptCodex => "chatgpt_codex",
                    ProviderId::DeepSeek => "deepseek",
                }
                .into(),
                model: routing_model,
            },
        )?;
        if selected == AutoMode::Implement {
            let clarification = analyze_issue(goal, "implement");
            if needs_clarification(&clarification) {
                self.store.save_issue_clarification(run_id, &clarification)?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::ClarificationStarted {
                        clarification: clarification.clone(),
                    },
                )?;
                return self
                    .run_clarification_round(run_id, RunKind::Auto, goal, client, available, clarification)
                    .await;
            }
        }
        if selected == AutoMode::Chat {
            return self
                .run_single_with_client(
                    run_id,
                    RunKind::Auto,
                    goal,
                    lead_model,
                    true,
                    "conversation lead",
                    client,
                )
                .await;
        }
        let mut outcome = match selected {
            AutoMode::Implement => {
                self.run_implementation(run_id, goal, client, available, lead_model, planner_model)
                    .await?
            }
            AutoMode::Plan => {
                self.run_single_with_client(
                    run_id,
                    RunKind::Plan,
                    goal,
                    planner_model,
                    true,
                    "planner lead",
                    client,
                )
                .await?
            }
            AutoMode::Audit => {
                self.run_audit(run_id, goal, client, available, lead_model)
                    .await?
            }
            AutoMode::Review => {
                let review_model = first_available(
                    available,
                    &[
                        &self.config.models.worker_fast,
                        "deepseek/deepseek-v4-flash",
                        lead_model,
                    ],
                )?;
                self.run_single_with_client(
                    run_id,
                    RunKind::Review,
                    goal,
                    review_model,
                    true,
                    "reviewer",
                    client,
                )
                .await?
            }
            AutoMode::Chat => {
                return Err(HarnessError::Provider(ProviderError::InvalidResponse(
                    "chat route escaped its terminal branch",
                )));
            }
        };
        outcome.usage = add_usage(outcome.usage, route_usage);
        outcome.agents_used += usize::from(route_usage.total() > 0);
        Ok(outcome)
    }

    async fn run_single(
        &self,
        run_id: RunId,
        kind: RunKind,
        goal: &str,
        model: &str,
        read_only: bool,
        role: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let client = self.client().await?;
        let models = client.fetch_models(None).await?.models;
        ensure_model(
            &models
                .iter()
                .map(|item| item.slug.clone())
                .collect::<HashSet<_>>(),
            model,
        )?;
        self.run_single_with_client(run_id, kind, goal, model, read_only, role, client)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_single_with_client(
        &self,
        run_id: RunId,
        kind: RunKind,
        goal: &str,
        model: &str,
        read_only: bool,
        role: &str,
        client: RuntimeProviderClient,
    ) -> Result<RunOutcome, HarnessError> {
        let executor = ToolExecutor::new(&self.root, read_only)?;
        let system = self.system_prompt(goal, role, read_only)?;
        let result = self
            .run_agent(run_id, &client, model, &system, goal, executor, role)
            .await?;
        self.finish_agent_outcome(run_id, kind, model, result, 1, Vec::new())
    }

    async fn run_audit(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        lead_model: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let lenses = [
            (
                "correctness",
                "Find concrete correctness bugs, races, and error-path failures.",
            ),
            (
                "tests",
                "Find missing, weak, or misleading tests and unverified behavior.",
            ),
            (
                "performance",
                "Find measurable performance and token-efficiency problems.",
            ),
            (
                "security",
                "Find concrete trust-boundary, secret, injection, and unsafe-operation risks.",
            ),
            (
                "maintainability",
                "Find architecture drift, duplication, and documentation mismatches.",
            ),
        ];
        let count = lenses.len().min(
            self.config
                .scheduler
                .max_agents
                .min(self.config.scheduler.hard_max_agents)
                .max(1),
        );
        let worker_models = [
            self.config.models.worker_fast.as_str(),
            "deepseek/deepseek-v4-flash",
        ]
        .into_iter()
        .filter(|model| available.contains(*model))
        .collect::<Vec<_>>();
        if worker_models.is_empty() {
            return Err(HarnessError::ModelUnavailable(
                "audit requires Spark or DeepSeek V4 Flash".into(),
            ));
        }
        let futures = FuturesUnordered::new();
        for (slot, (lens, directive)) in lenses.into_iter().take(count).enumerate() {
            let harness = self.clone();
            let goal = goal.to_owned();
            let model = worker_models[slot % worker_models.len()].to_owned();
            let client = self.pooled_client(slot, &model, &client);
            futures.push(async move {
                let role = format!("{} {lens} auditor", model_label(&model));
                let executor = ToolExecutor::new(&harness.root, true)?;
                let mut system = harness.system_prompt(&goal, &role, true)?;
                system.push_str("\nAudit lens: ");
                system.push_str(directive);
                system.push_str(
                    "\nReport only evidence-backed findings with path and line. If none, say none. Never edit.",
                );
                harness
                    .run_agent(run_id, &client, &model, &system, &goal, executor, &role)
                    .await
            });
        }
        let mut reports = Vec::new();
        let mut usage = TokenUsage::default();
        let mut first_error = None;
        let mut failed_agents = 0_usize;
        let mut pending_question = None;
        let mut paused = false;
        let mut futures = futures;
        while let Some(result) = futures.next().await {
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    failed_agents += 1;
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            usage = add_usage(usage, result.usage);
            paused |= result.pause_before_next_call();
            if let Some(question) = result.question {
                pending_question.get_or_insert(question);
            }
            reports.push(result.text);
        }
        if reports.is_empty()
            && let Some(error) = first_error
        {
            return Err(error);
        }
        if failed_agents > 0 {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: format!(
                        "{failed_agents} audit agent(s) failed; synthesizing the independent reports that completed"
                    ),
                },
            )?;
        }
        if let Some(question) = pending_question {
            return self.finish_agent_outcome(
                run_id,
                RunKind::Audit,
                worker_models[0],
                AgentResult {
                    text: reports.join("\n\n"),
                    question: Some(question),
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::Blocked),
                },
                count,
                Vec::new(),
            );
        }
        if paused {
            return self.finish_agent_outcome(
                run_id,
                RunKind::Audit,
                worker_models[0],
                AgentResult::usage_pause(
                    format!(
                        "Account usage reserve reached after audit workers. Partial reports:\n\n{}",
                        reports.join("\n\n")
                    ),
                    usage,
                ),
                count,
                Vec::new(),
            );
        }

        let synthesis = format!(
            "Audit goal:\n{goal}\n\nIndependent audit reports:\n{}\n\nDeduplicate and rank findings by severity. Preserve path/line evidence. State uncertainty. Do not invent findings.",
            reports
                .iter()
                .enumerate()
                .map(|(index, report)| format!("## Report {}\n{}", index + 1, bound(report, 12_000)))
                .collect::<Vec<_>>()
                .join("\n\n")
        );
        let executor = ToolExecutor::new(&self.root, true)?;
        let system = self.system_prompt(goal, "audit synthesizer lead", true)?;
        let mut final_result = self
            .run_agent(
                run_id,
                &client,
                lead_model,
                &system,
                &synthesis,
                executor,
                "audit synthesizer lead",
            )
            .await?;
        final_result.usage = add_usage(final_result.usage, usage);
        self.finish_agent_outcome(
            run_id,
            RunKind::Audit,
            lead_model,
            final_result,
            count + 1,
            Vec::new(),
        )
    }

    async fn run_implementation(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        lead_model: &str,
        planner_model: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let existing_tasks = self.store.tasks(run_id)?;
        if existing_tasks.is_empty() && !should_delegate(goal, self.config.budgets.default) {
            return self
                .run_focused_implementation(run_id, goal, client, lead_model)
                .await;
        }
        let (plan, plan_result, resumed_graph) = if existing_tasks.is_empty() {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::RunPhase {
                    phase: RunPhase::Planning,
                    detail: format!("{planner_model} is producing a bounded dependency graph"),
                },
            )?;
            let planner = ToolExecutor::new(&self.root, true)?;
            let mut planner_system = self.system_prompt(goal, "branch planner lead", true)?;
            planner_system.push_str(
                "\nInspect first. End with one <minha-plan> JSON object: {\"summary\":string,\"consult\":null|\"terra\"|\"sol\",\"tasks\":[{\"id\":string,\"objective\":string,\"paths\":[string],\"dependencies\":[string]}]}. Use consult=null normally, terra for important cross-cutting work, and sol only for critical work. Tasks must be testable slices; declare dependencies only when one slice needs another. Prefer one focused lane unless evidence proves meaningful independent work. Use up to 8 tasks normally and up to 16 only in Turbo for truly disjoint work. Do not edit.",
            );
            let plan_result = self
                .run_agent(
                    run_id,
                    &client,
                    planner_model,
                    &planner_system,
                    goal,
                    planner,
                    "branch planner lead",
                )
                .await?;
            if plan_result.question.is_some() || plan_result.pause_before_next_call() {
                return self.finish_agent_outcome(
                    run_id,
                    RunKind::Implement,
                    planner_model,
                    plan_result,
                    1,
                    Vec::new(),
                );
            }
            let parsed = parse_plan(&plan_result.text).unwrap_or_else(|| single_task_plan(goal));
            let plan = match validate_branch_plan(parsed) {
                Ok(plan) => plan,
                Err(error) => {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::Warning {
                            message: format!("planner graph was invalid ({error}); using one safe lane"),
                        },
                    )?;
                    single_task_plan(goal)
                }
            };
            let now = chrono::Utc::now();
            let records = plan
                .tasks
                .iter()
                .map(|task| TaskRecord {
                    run_id,
                    task_id: task.id.clone(),
                    objective: task.objective.clone(),
                    state: PlanTaskState::Pending,
                    paths: task.paths.clone(),
                    dependencies: task.dependencies.clone(),
                    assigned_agent_id: None,
                    attempt: 0,
                    max_attempts: 2,
                    generation: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                })
                .collect::<Vec<_>>();
            self.store.replace_tasks(run_id, &records)?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::PlanCreated {
                    summary: plan.summary.clone(),
                    tasks: plan
                        .tasks
                        .iter()
                        .map(|task| PlanTask {
                            id: task.id.clone(),
                            objective: task.objective.clone(),
                            paths: task.paths.clone(),
                            dependencies: task.dependencies.clone(),
                            state: PlanTaskState::Pending,
                            agent_id: None,
                        })
                        .collect(),
                },
            )?;
            (plan, plan_result, false)
        } else {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::RunPhase {
                    phase: RunPhase::Recovering,
                    detail: "reloading the persisted task graph; running tasks require explicit rescheduling"
                        .into(),
                },
            )?;
            for task in &existing_tasks {
                if task.state == PlanTaskState::Running {
                    self.store.update_task(
                        run_id,
                        &task.task_id,
                        PlanTaskState::Pending,
                        None,
                        task.attempt,
                        task.generation.saturating_add(1),
                        Some("recovered after an interrupted process"),
                    )?;
                }
            }
            (
                BranchPlan {
                    summary: "Recovered persistent task graph".into(),
                    consult: None,
                    tasks: existing_tasks
                        .into_iter()
                        .map(|task| BranchTask {
                            id: task.task_id,
                            objective: task.objective,
                            paths: task.paths,
                            dependencies: task.dependencies,
                        })
                        .collect(),
                },
                AgentResult {
                    text: "Recovered persistent task graph without another planning call.".into(),
                    question: None,
                    usage: TokenUsage::default(),
                    paused: false,
                    reserve_reached: false,
                    termination: None,
                },
                true,
            )
        };

        let mut orchestration_usage = plan_result.usage;
        let mut orchestration_agents = usize::from(!resumed_graph);
        let mut consultation = String::new();
        if let Some((consult_model, consult_role)) = consultation_route(&plan, &self.config) {
            ensure_model(available, consult_model)?;
            let executor = ToolExecutor::new(&self.root, true)?;
            let system = self.system_prompt(goal, consult_role, true)?;
            let consult_prompt = format!(
                "Goal: {goal}\n\nLuna plan:\n{}\n\nInspect only the uncertainty or risk that justifies this consultation. Return concise, evidence-backed constraints and recommendations for workers and integrator. Do not restate the plan and never edit.",
                bound(&plan_result.text, 10_000)
            );
            let mut result = self
                .run_agent(
                    run_id,
                    &client,
                    consult_model,
                    &system,
                    &consult_prompt,
                    executor,
                    consult_role,
                )
                .await?;
            result.usage = add_usage(result.usage, orchestration_usage);
            if result.question.is_some() || result.pause_before_next_call() {
                return self.finish_agent_outcome(
                    run_id,
                    RunKind::Implement,
                    consult_model,
                    result,
                    2,
                    Vec::new(),
                );
            }
            orchestration_usage = result.usage;
            orchestration_agents += 1;
            consultation = bound(&result.text, 8_000);
        }

        let (active_agents, open_tasks, blocked_tasks) = self.store.office_health(run_id)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::OfficeHealth {
                active_agents,
                open_tasks,
                blocked_tasks,
                manager_consultations: 0,
            },
        )?;

        let graph = self
            .run_worker_graph(
                run_id,
                goal,
                &client,
                available,
                &consultation,
                orchestration_usage,
            )
            .await?;
        let reports = graph.reports;
        let usage = graph.usage;
        let worktrees = graph.lanes;
        orchestration_agents += graph.agents_used;
        if graph.paused {
            return self.finish_agent_outcome(
                run_id,
                RunKind::Implement,
                &self.config.models.worker_fast,
                AgentResult::usage_pause(
                    format!(
                        "Account usage reserve reached after branch work. Recovery state is preserved.\n\n{}",
                        reports.join("\n\n")
                    ),
                    usage,
                ),
                orchestration_agents,
                worktrees,
            );
        }
        if let Some(question) = graph.question {
            return self.finish_agent_outcome(
                run_id,
                RunKind::Implement,
                &self.config.models.worker_fast,
                AgentResult {
                    text: format!(
                        "Independent ready tasks finished; one task is waiting for the user.\n\n{}",
                        reports.join("\n\n")
                    ),
                    question: Some(question),
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::Blocked),
                },
                orchestration_agents,
                worktrees,
            );
        }

        let integrator_prompt = format!(
            "Original goal: {goal}\n\nRead-only consultation:\n{}\n\nBranch results:\n{}\n\nInspect the primary checkout and recovery patches. Resolve any conflicts, finish missing integration, and run sufficient checks. Do not commit, merge, push, or discard user changes.",
            if consultation.is_empty() {
                "none"
            } else {
                &consultation
            },
            if reports.is_empty() {
                "No new worker report was needed; inspect persisted task and board state.".into()
            } else {
                reports.join("\n\n")
            }
        );
        let integrator = ToolExecutor::new(&self.root, false)?.with_policy(ExecutorPolicy {
            allow_destructive: false,
        });
        let system = self.system_prompt(goal, "integrator lead", false)?;
        let mut result = self
            .run_agent(
                run_id,
                &client,
                lead_model,
                &system,
                &integrator_prompt,
                integrator,
                "integrator lead",
            )
            .await?;
        result.usage = add_usage(result.usage, usage);
        self.judge_and_finish(
            run_id,
            goal,
            lead_model,
            result,
            JudgeContext {
                client,
                agents_used: orchestration_agents + 1,
                worktrees,
            },
        )
        .await
    }

    async fn run_focused_implementation(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        lead_model: &str,
    ) -> Result<RunOutcome, HarnessError> {
        let now = chrono::Utc::now();
        let task = TaskRecord {
            run_id,
            task_id: "focused".into(),
            objective: goal.into(),
            state: PlanTaskState::Pending,
            paths: Vec::new(),
            dependencies: Vec::new(),
            assigned_agent_id: None,
            attempt: 0,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        self.store.replace_tasks(run_id, std::slice::from_ref(&task))?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::PlanCreated {
                summary: "One focused lead lane; delegation was not justified by task evidence.".into(),
                tasks: vec![PlanTask {
                    id: task.task_id.clone(),
                    objective: task.objective.clone(),
                    paths: Vec::new(),
                    dependencies: Vec::new(),
                    state: PlanTaskState::Pending,
                    agent_id: None,
                }],
            },
        )?;
        let agent_id = EventAgentId::new();
        self.set_scheduler_todo(run_id, agent_id, &task, TodoState::InProgress, None, Vec::new())?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Working,
                detail: format!("{lead_model} is inspecting and implementing in one focused lane"),
            },
        )?;
        let executor = ToolExecutor::new(&self.root, false)?.with_policy(ExecutorPolicy {
            allow_destructive: false,
        });
        let system = self.system_prompt(goal, "Lead task focused", false)?;
        let prompt = format!(
            "Implement this task end to end: {goal}\n\nInspect before editing, preserve unrelated work, run sufficient checks, and finish with a concise evidence-backed result. Delegate nothing unless new evidence proves an independent lane is necessary."
        );
        let result = self
            .run_agent_as(
                run_id,
                &client,
                lead_model,
                &system,
                &prompt,
                executor,
                "Lead task focused",
                agent_id,
            )
            .await?;
        if result.question.is_none() && !result.pause_before_next_call() {
            self.store
                .update_task(run_id, "focused", PlanTaskState::Completed, None, 1, 0, None)?;
            self.set_scheduler_todo(
                run_id,
                agent_id,
                &task,
                TodoState::Completed,
                None,
                vec!["completion judge pending".into()],
            )?;
        }
        self.judge_and_finish(
            run_id,
            goal,
            lead_model,
            result,
            JudgeContext {
                client,
                agents_used: 1,
                worktrees: Vec::new(),
            },
        )
        .await
    }

    async fn run_worker_graph(
        &self,
        run_id: RunId,
        goal: &str,
        client: &RuntimeProviderClient,
        available: &HashSet<String>,
        consultation: &str,
        initial_usage: TokenUsage,
    ) -> Result<WorkerGraphResult, HarnessError> {
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Scheduling,
                detail: "dispatching persisted ready tasks with fenced path leases".into(),
            },
        )?;
        let repo = GitRepo::new(&self.root);
        let use_git_worktrees =
            repo.head().is_ok() && repo.status_porcelain().is_ok_and(|status| status.is_empty());
        if !use_git_worktrees {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: "repository is dirty or has no HEAD; using isolated snapshot lanes so Spark workers can still run concurrently".into(),
                },
            )?;
        }
        let lane_base = self
            .root
            .join(if use_git_worktrees {
                ".minha/worktrees"
            } else {
                ".minha/snapshots"
            })
            .join(run_id.to_string());
        let recovery_base = self.root.join(".minha/recovery").join(run_id.to_string());
        std::fs::create_dir_all(&lane_base)?;
        std::fs::create_dir_all(&recovery_base)?;
        let primary_executor = ToolExecutor::new(&self.root, false)?;
        let mut output = WorkerGraphResult {
            usage: initial_usage,
            ..WorkerGraphResult::default()
        };

        loop {
            if output.paused {
                break;
            }
            let tasks = self.store.tasks(run_id)?;
            let completed = tasks
                .iter()
                .filter(|task| task.state == PlanTaskState::Completed)
                .map(|task| task.task_id.as_str())
                .collect::<HashSet<_>>();
            let ready = tasks
                .iter()
                .filter(|task| {
                    task.state == PlanTaskState::Pending
                        && task
                            .dependencies
                            .iter()
                            .all(|dependency| completed.contains(dependency.as_str()))
                })
                .cloned()
                .collect::<Vec<_>>();
            let policy = self.config.budgets.default.policy();
            let configured_max = if self.config.budgets.default == crate::config::ExecutionProfile::Turbo {
                self.config.scheduler.hard_max_agents
            } else {
                self.config.scheduler.max_agents
            };
            let ready = disjoint_ready_tasks(
                ready,
                policy
                    .max_agents
                    .min(configured_max)
                    .min(self.config.scheduler.hard_max_agents)
                    .max(1),
            );
            if ready.is_empty() {
                break;
            }

            let futures = FuturesUnordered::new();
            for (slot, task) in ready.into_iter().enumerate() {
                let attempt = task.attempt.saturating_add(1);
                let generation = task.generation;
                let agent_id = EventAgentId::new();
                let lane =
                    prepare_worker_lane(&self.root, &lane_base, run_id, &task, attempt, use_git_worktrees)?;
                if !output.lanes.iter().any(|path| path == lane.path()) {
                    output.lanes.push(lane.path().to_owned());
                }
                let resources = lease_resources(&task);
                self.store.acquire_task_leases(
                    run_id,
                    &task.task_id,
                    agent_id,
                    generation,
                    &resources,
                    chrono::Utc::now() + chrono::Duration::hours(2),
                )?;
                self.store.update_task(
                    run_id,
                    &task.task_id,
                    PlanTaskState::Running,
                    Some(agent_id),
                    attempt,
                    generation,
                    None,
                )?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::PlanTaskChanged {
                        task_id: task.task_id.clone(),
                        state: PlanTaskState::Running,
                        agent_id: Some(agent_id),
                    },
                )?;
                self.set_scheduler_todo(run_id, agent_id, &task, TodoState::InProgress, None, Vec::new())?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::LeaseChanged {
                        task_id: task.task_id.clone(),
                        agent_id,
                        generation,
                        resources,
                        acquired: true,
                    },
                )?;

                let harness = self.clone();
                let model = routed_worker_model(&task, available, &self.config).to_owned();
                let client = self.pooled_client(slot.saturating_add(attempt as usize), &model, client);
                let goal = goal.to_owned();
                let consultation = consultation.to_owned();
                futures.push(async move {
                    let role = format!("{} worker {}", model_label(&model), task.task_id);
                    let result = async {
                        let executor = ToolExecutor::new(lane.path(), false)?;
                        let mut system = harness.system_prompt(&goal, &role, false)?;
                        system.push_str(include_str!("../../../bundled/agents/spark-worker.md"));
                        let prompt = format!(
                            "Shared goal: {goal}\n\nRead-only consultation: {}\n\nTask: {}\nDependencies already integrated: {}\nPrior scheduler context: {}\nOwned paths: {}\nStay within this slice. Read the shared board only when it saves duplicate work; post only durable findings, blockers, artifacts, or decisions. Inspect, edit, and run the smallest sufficient checks. Do not commit, push, or claim global completion.",
                            if consultation.is_empty() { "none" } else { &consultation },
                            task.objective,
                            if task.dependencies.is_empty() { "none".into() } else { task.dependencies.join(", ") },
                            task.last_error.as_deref().unwrap_or("none"),
                            if task.paths.is_empty() { "planner did not constrain paths".into() } else { task.paths.join(", ") },
                        );
                        harness
                            .run_agent_as(
                                run_id,
                                &client,
                                &model,
                                &system,
                                &prompt,
                                executor,
                                &role,
                                agent_id,
                            )
                            .await
                    }
                    .await;
                    (task, lane, agent_id, attempt, generation, result)
                });
            }

            let mut futures = futures;
            while let Some((task, lane, agent_id, attempt, generation, result)) = futures.next().await {
                output.agents_used += 1;
                let _ = self
                    .store
                    .release_task_leases(run_id, &task.task_id, agent_id, generation)?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::LeaseChanged {
                        task_id: task.task_id.clone(),
                        agent_id,
                        generation,
                        resources: lease_resources(&task),
                        acquired: false,
                    },
                )?;

                match result {
                    Ok(result) => {
                        output.usage = add_usage(output.usage, result.usage);
                        if let Some(question) = result.question {
                            self.store.update_task(
                                run_id,
                                &task.task_id,
                                PlanTaskState::Blocked,
                                Some(agent_id),
                                attempt,
                                generation,
                                Some(&question.question),
                            )?;
                            self.set_scheduler_todo(
                                run_id,
                                agent_id,
                                &task,
                                TodoState::Blocked,
                                Some(&question.question),
                                Vec::new(),
                            )?;
                            output.question.get_or_insert(question);
                            output.reports.push(format!(
                                "Task {} is waiting for user input; independent tasks continued.",
                                task.task_id
                            ));
                            continue;
                        }
                        if result.pause_before_next_call() {
                            self.store.update_task(
                                run_id,
                                &task.task_id,
                                PlanTaskState::Pending,
                                None,
                                attempt,
                                generation.saturating_add(1),
                                Some("paused by account usage reserve"),
                            )?;
                            output.paused = true;
                            output.reports.push(format!(
                                "Task {} paused at the configured account-usage reserve.",
                                task.task_id
                            ));
                            continue;
                        }

                        let patch = lane.patch()?;
                        let patch_path = recovery_base.join(format!(
                            "{}-g{}-a{}.patch",
                            safe_component(&task.task_id),
                            generation,
                            attempt
                        ));
                        std::fs::write(&patch_path, &patch)?;
                        let applied = if patch.trim().is_empty() {
                            Ok("no source changes".to_owned())
                        } else {
                            primary_executor
                                .execute("apply_patch", &json!({"patch": patch}))
                                .map(|_| "patch applied to primary checkout".to_owned())
                        };
                        match applied {
                            Ok(applied) => {
                                self.store.update_task(
                                    run_id,
                                    &task.task_id,
                                    PlanTaskState::Completed,
                                    Some(agent_id),
                                    attempt,
                                    generation,
                                    None,
                                )?;
                                self.store.record_runtime_event(
                                    run_id,
                                    RuntimeEvent::PlanTaskChanged {
                                        task_id: task.task_id.clone(),
                                        state: PlanTaskState::Completed,
                                        agent_id: Some(agent_id),
                                    },
                                )?;
                                self.set_scheduler_todo(
                                    run_id,
                                    agent_id,
                                    &task,
                                    TodoState::Completed,
                                    None,
                                    vec![patch_path.display().to_string()],
                                )?;
                                self.record_board_note(
                                    run_id,
                                    BoardKind::Progress,
                                    &format!("Task {} completed", task.task_id),
                                    &format!("{applied}; recovery patch: {}", patch_path.display()),
                                    Some(&task.task_id),
                                    Some(agent_id),
                                )?;
                                if !patch.trim().is_empty() {
                                    std::fs::write(
                                        recovery_base.join(format!(
                                            "{:020}-{}.applied.patch",
                                            chrono::Utc::now().timestamp_micros(),
                                            safe_component(&task.task_id)
                                        )),
                                        &patch,
                                    )?;
                                }
                                output.reports.push(format!(
                                    "Task {}: {}\nRecovery patch: {}\nWorker report: {}",
                                    task.task_id,
                                    applied,
                                    patch_path.display(),
                                    bound(&result.text, 8_000)
                                ));
                            }
                            Err(error) => {
                                self.retry_or_fail_task(
                                    run_id,
                                    &task,
                                    agent_id,
                                    attempt,
                                    generation,
                                    &format!("patch integration failed: {error}"),
                                )?;
                                output.reports.push(format!(
                                    "Task {} patch was preserved at {} but could not be applied: {error}",
                                    task.task_id,
                                    patch_path.display()
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        self.retry_or_fail_task(
                            run_id,
                            &task,
                            agent_id,
                            attempt,
                            generation,
                            &error.to_string(),
                        )?;
                        output.reports.push(format!(
                            "Task {} attempt {} failed: {}",
                            task.task_id, attempt, error
                        ));
                    }
                }
            }
        }

        let unresolved = self
            .store
            .tasks(run_id)?
            .into_iter()
            .filter(|task| matches!(task.state, PlanTaskState::Failed | PlanTaskState::Pending))
            .collect::<Vec<_>>();
        if !unresolved.is_empty() {
            output.reports.push(format!(
                "Luna must replan or finish these unresolved slices during integration: {}",
                unresolved
                    .iter()
                    .map(|task| format!(
                        "{} ({})",
                        task.task_id,
                        task.last_error.as_deref().unwrap_or("dependency not ready")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(output)
    }

    fn retry_or_fail_task(
        &self,
        run_id: RunId,
        task: &TaskRecord,
        agent_id: EventAgentId,
        attempt: u32,
        generation: u64,
        reason: &str,
    ) -> Result<(), HarnessError> {
        let retry = attempt < task.max_attempts;
        let state = if retry {
            PlanTaskState::Pending
        } else {
            PlanTaskState::Failed
        };
        self.store.update_task(
            run_id,
            &task.task_id,
            state,
            (!retry).then_some(agent_id),
            attempt,
            generation.saturating_add(1),
            Some(reason),
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::PlanTaskChanged {
                task_id: task.task_id.clone(),
                state,
                agent_id: (!retry).then_some(agent_id),
            },
        )?;
        if retry {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::AgentRetry {
                    task_id: task.task_id.clone(),
                    attempt: attempt.saturating_add(1),
                    reason: bound(reason, 500),
                },
            )?;
        } else {
            self.record_board_note(
                run_id,
                BoardKind::Blocker,
                &format!("Task {} exhausted retries", task.task_id),
                &bound(reason, 2_000),
                Some(&task.task_id),
                Some(agent_id),
            )?;
        }
        Ok(())
    }

    fn set_scheduler_todo(
        &self,
        run_id: RunId,
        agent_id: EventAgentId,
        task: &TaskRecord,
        state: TodoState,
        blocker: Option<&str>,
        evidence: Vec<String>,
    ) -> Result<(), HarnessError> {
        self.store.upsert_todo(
            run_id,
            agent_id,
            TodoItem {
                id: task.task_id.clone(),
                objective: task.objective.clone(),
                state,
                order: 0,
                blocker: blocker.map(str::to_owned),
                evidence,
                revision: 0,
            },
        )?;
        Ok(())
    }

    fn record_board_note(
        &self,
        run_id: RunId,
        kind: BoardKind,
        subject: &str,
        body: &str,
        task_id: Option<&str>,
        author_agent_id: Option<EventAgentId>,
    ) -> Result<(), HarnessError> {
        let mut entry = BoardEntry::session(&self.workspace_id, run_id, kind, subject, body);
        entry.task_id = task_id.map(str::to_owned);
        entry.author_agent_id = author_agent_id;
        self.store.insert_board_entry(&entry)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::BoardChanged {
                entry: board_entry_view(&entry),
            },
        )?;
        Ok(())
    }

    async fn judge_and_finish(
        &self,
        run_id: RunId,
        goal: &str,
        lead_model: &str,
        integration: AgentResult,
        context: JudgeContext,
    ) -> Result<RunOutcome, HarnessError> {
        let JudgeContext {
            client,
            agents_used,
            worktrees,
        } = context;
        if integration.question.is_some() || integration.pause_before_next_call() {
            return self.finish_agent_outcome(
                run_id,
                RunKind::Implement,
                lead_model,
                integration,
                agents_used,
                worktrees,
            );
        }
        let mut system = self.system_prompt(goal, "Spark completion judge", true)?;
        system.push_str(include_str!("../../../bundled/agents/completion-judge.md"));
        let mut integrated_text = integration.text;
        let mut cumulative_usage = integration.usage;
        let mut additional_agents = 0;
        let mut repair_cycle = 0_u8;
        let (mut judged, report) = loop {
            let judge_prompt = format!(
                "Goal: {goal}\n\nIntegrator claim:\n{}\n\nIndependently inspect current source, diff, and relevant checks. Never edit. Return exactly one versioned report as <minha-judge>{{\"schema_version\":1,\"verdict\":\"verified|incomplete|blocked|inconclusive\",\"summary\":\"...\",\"evidence\":[\"...\"],\"findings\":[\"...\"]}}</minha-judge>. Use verified only when current source and sufficient checks prove the goal.",
                bound(&integrated_text, 10_000)
            );
            let executor = ToolExecutor::new(&self.root, true)?;
            let mut candidate = self
                .run_agent(
                    run_id,
                    &client,
                    &self.config.models.worker_fast,
                    &system,
                    &judge_prompt,
                    executor,
                    "Spark completion judge",
                )
                .await?;
            additional_agents += 1;
            candidate.usage = add_usage(candidate.usage, cumulative_usage);
            cumulative_usage = candidate.usage;
            if candidate.paused {
                return self.finish_agent_outcome(
                    run_id,
                    RunKind::Implement,
                    &self.config.models.worker_fast,
                    candidate,
                    agents_used + additional_agents,
                    worktrees,
                );
            }
            let report = parse_judge_report(&candidate.text);
            let repairable = report
                .as_ref()
                .is_some_and(|report| report.verdict == JudgeVerdictV1::Incomplete)
                && repair_cycle < 2;
            if !repairable {
                break (candidate, report);
            }

            repair_cycle += 1;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::RunPhase {
                    phase: RunPhase::Retrying,
                    detail: format!("repair cycle {repair_cycle} of 2 after independent review"),
                },
            )?;
            let report_json = report
                .as_ref()
                .and_then(|report| serde_json::to_string(report).ok())
                .unwrap_or_else(|| "typed judge report unavailable".into());
            let repair_prompt = format!(
                "Original goal: {goal}\n\nIndependent judge report: {report_json}\n\nInspect current source, repair only verified deficiencies, and run the smallest sufficient checks. Preserve unrelated changes. Do not claim completion without evidence."
            );
            let executor = ToolExecutor::new(&self.root, false)?;
            let repair_system = self.system_prompt(goal, "Lead repair cycle", false)?;
            let mut repair = self
                .run_agent(
                    run_id,
                    &client,
                    lead_model,
                    &repair_system,
                    &repair_prompt,
                    executor,
                    "Lead repair cycle",
                )
                .await?;
            additional_agents += 1;
            repair.usage = add_usage(repair.usage, cumulative_usage);
            cumulative_usage = repair.usage;
            if repair.question.is_some() || repair.pause_before_next_call() {
                return self.finish_agent_outcome(
                    run_id,
                    RunKind::Implement,
                    lead_model,
                    repair,
                    agents_used + additional_agents,
                    worktrees,
                );
            }
            integrated_text.push_str(&format!(
                "\n\n--- repair cycle {repair_cycle} ---\n{}",
                repair.text
            ));
        };
        let unresolved_tasks = self
            .store
            .tasks(run_id)?
            .into_iter()
            .filter(|task| task.state != PlanTaskState::Completed)
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let judge_verified = report
            .as_ref()
            .is_some_and(|report| report.verdict == JudgeVerdictV1::Verified);
        let state = if judge_verified && unresolved_tasks.is_empty() {
            ExitState::Succeeded
        } else if report
            .as_ref()
            .is_some_and(|report| report.verdict == JudgeVerdictV1::Blocked)
        {
            ExitState::Blocked
        } else {
            ExitState::Inconclusive
        };
        if report.is_none() {
            judged.text.push_str(
                "\n\nRuntime invariant: malformed or missing typed judge report; completion was not promoted.",
            );
            judged.termination = Some(TerminationReason::InvalidEmptyResponse);
        }
        if judge_verified && !unresolved_tasks.is_empty() {
            judged.text.push_str(&format!(
                "\n\nRuntime invariant: verified verdict was not promoted because task state is unresolved: {}.",
                unresolved_tasks.join(", ")
            ));
        }
        let text = format!(
            "{}\n\n--- independent judge ---\n{}",
            integrated_text, judged.text
        );
        judged.text = text;
        self.finish_agent_outcome_with_state(
            run_id,
            RunKind::Implement,
            lead_model,
            judged,
            state,
            agents_used + additional_agents,
            worktrees,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_agent(
        &self,
        run_id: RunId,
        client: &RuntimeProviderClient,
        model: &str,
        system: &str,
        prompt: &str,
        executor: ToolExecutor,
        role: &str,
    ) -> Result<AgentResult, HarnessError> {
        self.run_agent_as(
            run_id,
            client,
            model,
            system,
            prompt,
            executor,
            role,
            EventAgentId::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_agent_as(
        &self,
        run_id: RunId,
        client: &RuntimeProviderClient,
        model: &str,
        system: &str,
        prompt: &str,
        executor: ToolExecutor,
        role: &str,
        agent_id: EventAgentId,
    ) -> Result<AgentResult, HarnessError> {
        let selected_client = self.pooled_client(0, model, client);
        let client = &selected_client;
        let task_id = worker_task_id(role).map(str::to_owned);
        let read_only = executor.is_read_only();
        let executor = executor.with_coordination(CoordinationContext {
            store: self.store.clone(),
            workspace_id: self.workspace_id.clone(),
            run_id,
            agent_id,
            task_id: task_id.clone(),
            can_write: !read_only,
        });
        let now = chrono::Utc::now();
        let task_snapshot = self
            .store
            .tasks(run_id)?
            .into_iter()
            .find(|task| Some(&task.task_id) == task_id.as_ref());
        let mut agent_record = AgentRecord {
            run_id,
            agent_id,
            parent_agent_id: None,
            role: role.to_owned(),
            model: model.to_owned(),
            state: initial_agent_state(role),
            task_id: task_id.clone(),
            attempt: task_snapshot.as_ref().map_or(1, |task| task.attempt.max(1)),
            generation: task_snapshot.as_ref().map_or(0, |task| task.generation),
            started_at: now,
            updated_at: now,
            finished_at: None,
        };
        self.store.upsert_agent(&agent_record)?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::AgentStarted {
                agent_id,
                role: role.to_owned(),
                model: model.to_owned(),
                parent: None,
            },
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::AgentState {
                agent_id,
                state: initial_agent_state(role),
                detail: "starting".into(),
            },
        )?;
        let mut system = system.to_owned();
        let memory_settings = self.store.memory_settings(&self.workspace_id)?;
        if self.config.memory.enabled
            && self.config.memory.use_memory
            && memory_settings.enabled
            && memory_settings.use_memory
            && !role.contains("classifier")
            && !role.contains("issue clarifier")
        {
            let memories = self.store.search_memories(
                &self.workspace_id,
                Some(run_id),
                prompt,
                self.config.memory.retrieval_limit,
            )?;
            if !memories.is_empty() {
                system.push_str(
                    "\nRetrieved generated memory (advisory only; repository instructions and current evidence remain authoritative):\n<minha-memory version=\"1\">\n",
                );
                for hit in &memories {
                    system.push_str(&format!(
                        "- id={} scope={} kind={} subject={} body={}\n",
                        hit.memory.id,
                        hit.memory.scope.as_str(),
                        bound(&hit.memory.kind, 48),
                        bound(&hit.memory.subject, 160),
                        bound(&hit.memory.body, 480),
                    ));
                }
                system.push_str("</minha-memory>\n");
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::MemoryRetrieved {
                        agent_id,
                        memory_ids: memories.iter().map(|hit| hit.memory.id.clone()).collect(),
                        estimated_tokens: estimate_tokens(&system) as u64,
                    },
                )?;
            }
        }
        if let Some(task_id) = task_id.as_deref() {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::PlanTaskChanged {
                    task_id: task_id.to_owned(),
                    state: PlanTaskState::Running,
                    agent_id: Some(agent_id),
                },
            )?;
        }
        let mut input = vec![message("user", prompt)];
        let mut tools = tool_definitions(
            role,
            read_only,
            role_can_ask_user(role)
                && !matches!(
                    self.config.scheduler.question_policy,
                    crate::config::QuestionPolicy::Never
                ),
            true,
        );
        if !self.config.books.enabled {
            tools.retain(|tool| tool.get("name").and_then(Value::as_str) != Some("books"));
        }
        if role.contains("issue clarifier") {
            tools.retain(|tool| {
                matches!(
                    tool.get("name").and_then(Value::as_str),
                    Some("read_files" | "search" | "github")
                )
            });
        }
        if role.contains("intent classifier") {
            tools.clear();
        }
        let mut usage = TokenUsage::default();
        let turn_limit = agent_turn_limit(role);
        let input_budget = agent_input_budget(role);
        let tool_budget = agent_tool_budget(role);
        let mut tool_calls_used = 0_usize;
        let mut last_text = String::new();
        let mut last_todo_snapshot = String::new();

        for turn in 0..turn_limit {
            if self.is_interrupted(run_id) {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::AgentState {
                        agent_id,
                        state: AgentState::Cancelled,
                        detail: "interrupted".into(),
                    },
                )?;
                return Err(HarnessError::Interrupted);
            }
            if self.take_cooperative_pause(run_id) {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::AgentState {
                        agent_id,
                        state: AgentState::Waiting,
                        detail: "paused by user at a safe boundary".into(),
                    },
                )?;
                return Ok(AgentResult {
                    text: "Work paused safely. Active tool/model work reached a boundary; completed evidence and TODO state were preserved.".into(),
                    question: Some(InputRequest {
                        question: "What should Minha do next?".into(),
                        options: vec!["Resume the current plan".into(), "Change direction".into()],
                    }),
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::UserPaused),
                });
            }
            let todo_snapshot = serde_json::to_string(&self.store.todos(run_id, agent_id)?)
                .map_err(crate::store::StoreError::Json)?;
            if todo_snapshot != last_todo_snapshot {
                input.push(message(
                    "user",
                    &format!(
                        "<minha-todos version=\"1\">{todo_snapshot}</minha-todos>\nUpdate only changed items with the todo tool. TODOs are advisory and never override verified work."
                    ),
                ));
                last_todo_snapshot = todo_snapshot;
            }
            let estimated_next_input = estimate_tokens(&system)
                + input
                    .iter()
                    .map(|item| estimate_tokens(&item.to_string()))
                    .sum::<usize>();
            if turn > 0 && estimated_next_input as u64 > input_budget {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::Warning {
                        message: format!(
                            "{role} stopped at its {} input-token budget before another model call",
                            input_budget
                        ),
                    },
                )?;
                return Ok(AgentResult {
                    text: if last_text.is_empty() {
                        "Agent reached its token budget before a terminal answer; inspect its tool evidence."
                            .into()
                    } else {
                        format!("{last_text}\n\nAgent reached its token budget before another turn.")
                    },
                    question: None,
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::ContextBoundary),
                });
            }
            for steering in self.take_steering(run_id) {
                input.push(message("user", &format!("User steering: {steering}")));
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::SteeringApplied {
                        agent_id,
                        text: steering,
                    },
                )?;
            }
            if self
                .compact_if_needed(run_id, client, model, &system, &mut input, &mut usage)
                .await?
            {
                return Ok(AgentResult::usage_pause(
                    "Account usage reserve reached during context compaction; no further model call was made."
                        .into(),
                    usage,
                ));
            }
            let item_id = ItemId::new();
            let stream_store = self.store.clone();
            let reservation = (estimated_next_input as u64).saturating_add(1_024);
            if !self.try_reserve_budget(run_id, reservation) {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::Warning {
                        message: format!(
                            "{role} stopped before a model call because the {:?} session token budget was reached",
                            self.config.budgets.default
                        ),
                    },
                )?;
                return Ok(AgentResult {
                    text: format!(
                        "Session token budget reached before another {role} turn; durable progress is preserved."
                    ),
                    question: None,
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::ContextBoundary),
                });
            }
            let turn_result = client
                .turn_stream(
                    TurnRequest {
                        model: model.to_owned(),
                        instructions: system.clone(),
                        input: input.clone(),
                        tools: if tool_calls_used >= tool_budget {
                            Vec::new()
                        } else {
                            tools.clone()
                        },
                        parallel_tool_calls: true,
                        reasoning_effort: Some(reasoning_for_turn(
                            model,
                            role,
                            turn,
                            &self.config.models.reasoning_effort,
                        )),
                        prompt_cache_key: Some(prompt_cache_key(&format!(
                            "{:?}|{model}|{role}|{}|tools-v2|prompt-v3",
                            client.provider_id(),
                            self.root.to_string_lossy()
                        ))),
                        subagent_label: Some(role.to_ascii_lowercase().replace(' ', "_")),
                        response_format: None,
                    },
                    |event| {
                        if let ProviderStreamEvent::TextDelta(delta) = event {
                            stream_store.publish_runtime_event(
                                run_id,
                                RuntimeEvent::TextDelta {
                                    agent_id,
                                    item_id,
                                    delta,
                                },
                            );
                        }
                    },
                )
                .await;
            let result = match turn_result {
                Ok(result) => {
                    self.settle_budget(run_id, reservation, result.usage.total());
                    result
                }
                Err(error) => {
                    self.settle_budget(run_id, reservation, 0);
                    return Err(error.into());
                }
            };
            usage = add_usage(usage, result.usage);
            if !result.output_text.is_empty() {
                last_text = result.output_text.clone();
            }
            self.store
                .add_usage(run_id, result.usage.input, result.usage.output)?;
            let estimated_context = estimate_tokens(&system)
                + input
                    .iter()
                    .map(|item| estimate_tokens(&item.to_string()))
                    .sum::<usize>();
            self.store.record_usage_turn(
                run_id,
                Some(agent_id),
                model,
                result.usage,
                Some(estimated_context as u64),
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Usage {
                    agent_id: Some(agent_id),
                    model: model.to_owned(),
                    input_tokens: result.usage.input,
                    output_tokens: result.usage.output,
                    cached_input_tokens: result.usage.cached_input,
                    cache_write_tokens: result.usage.cache_write,
                    reasoning_output_tokens: result.usage.reasoning_output,
                },
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ContextUsage {
                    agent_id,
                    model: model.to_owned(),
                    estimated_tokens: estimated_context as u64,
                    advertised_limit: self.context_policy(model).advertised_limit,
                    effective_limit: self.context_policy(model).effective_limit,
                    forecast_tokens: estimated_next_input as u64
                        + self.context_policy(model).output_allowance,
                    output_allowance: self.context_policy(model).output_allowance,
                    protected_reserve: self.context_policy(model).protected_reserve,
                    capability_source: format!("{:?}", self.context_policy(model).source)
                        .to_ascii_lowercase(),
                },
            )?;
            if !result.rate_limits.is_empty() {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::AccountUsage {
                        snapshot: serde_json::to_value(&result.rate_limits).unwrap_or_default(),
                    },
                )?;
            }
            let account_reserve_reached =
                reserve_reached(&result.rate_limits, self.config.scheduler.usage_reserve_percent);
            self.store.append_message(
                run_id,
                "assistant",
                &json!({
                    "role": role,
                    "model": model,
                    "turn": turn,
                    "text": result.output_text,
                    "items": result.output_items
                }),
                false,
            )?;
            if !result.output_text.is_empty() {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::AssistantMessage {
                        agent_id,
                        item_id,
                        role: role.to_owned(),
                        model: model.to_owned(),
                        text: result.output_text.clone(),
                    },
                )?;
            }
            condense_consumed_tool_outputs(&mut input);
            input.extend(result.output_items.clone());
            if account_reserve_reached {
                self.record_usage_pause(run_id, role, model, &result.rate_limits)?;
            }

            if result.tool_calls.is_empty() {
                let empty_terminal = result.output_text.trim().is_empty();
                if empty_terminal && turn + 1 < turn_limit {
                    input.push(message(
                        "user",
                        "Return a concise terminal answer from the evidence already gathered. Do not return an empty response and do not call tools.",
                    ));
                    tools.clear();
                    continue;
                }
                agent_record.state = AgentState::Completed;
                agent_record.updated_at = chrono::Utc::now();
                agent_record.finished_at = Some(agent_record.updated_at);
                self.store.upsert_agent(&agent_record)?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::AgentState {
                        agent_id,
                        state: AgentState::Completed,
                        detail: format!("completed in {} turns", turn + 1),
                    },
                )?;
                if let Some(task_id) = task_id.as_deref() {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::PlanTaskChanged {
                            task_id: task_id.to_owned(),
                            state: PlanTaskState::Completed,
                            agent_id: Some(agent_id),
                        },
                    )?;
                }
                return Ok(AgentResult {
                    text: if empty_terminal {
                        "Provider returned an empty terminal response after one bounded retry.".into()
                    } else {
                        result.output_text
                    },
                    question: None,
                    usage,
                    paused: false,
                    reserve_reached: account_reserve_reached,
                    termination: if empty_terminal {
                        Some(TerminationReason::InvalidEmptyResponse)
                    } else {
                        account_reserve_reached.then_some(TerminationReason::ProviderReserve)
                    },
                });
            }

            if account_reserve_reached {
                return Ok(AgentResult::usage_pause(
                    format!(
                        "{}\n\nAccount usage reserve reached; pending tool calls were not executed.",
                        result.output_text
                    )
                    .trim()
                    .to_owned(),
                    usage,
                ));
            }

            if result.tool_calls.len() > 1
                && result
                    .tool_calls
                    .iter()
                    .all(|call| matches!(call.name.as_str(), "read_files" | "search"))
                && tool_calls_used.saturating_add(result.tool_calls.len()) <= tool_budget
            {
                tool_calls_used += result.tool_calls.len();
                input.extend(
                    self.execute_parallel_reads(run_id, agent_id, &executor, &result.tool_calls)
                        .await?,
                );
                continue;
            }

            for call in result.tool_calls {
                if tool_calls_used >= tool_budget {
                    let message = format!(
                        "tool budget exhausted after {tool_budget} calls; return a terminal answer from existing evidence"
                    );
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::ToolOutput {
                            agent_id,
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                            stdout: String::new(),
                            stderr: message.clone(),
                            exit_code: None,
                            truncated: false,
                        },
                    )?;
                    input.push(function_output(
                        &call,
                        json!({"error": message, "recoverable": false}).to_string(),
                    ));
                    continue;
                }
                tool_calls_used += 1;
                let arguments: Value = serde_json::from_str(&call.arguments)
                    .map_err(|error| HarnessError::Tool(ToolError::InvalidArguments(error.to_string())))?;
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::ToolStarted {
                        agent_id,
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: arguments.clone(),
                    },
                )?;
                let activity_started = Instant::now();
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::ActivityStarted {
                        activity_id: call.call_id.clone(),
                        agent_id: Some(agent_id),
                        kind: activity_kind_for_tool(&call.name).into(),
                        summary: activity_summary_for_tool(&call.name, &arguments),
                    },
                )?;
                let mut call_executor = executor.clone();
                if let Some(reason) = call_executor.approval_reason(&call.name, &arguments)? {
                    let permission = permission_for_call(&self.config, &call.name, &arguments);
                    if matches!(permission, crate::config::PermissionLevel::Deny) {
                        let message = format!("operation denied by configured permission policy: {reason}");
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ToolOutput {
                                agent_id,
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                                stdout: String::new(),
                                stderr: message.clone(),
                                exit_code: None,
                                truncated: false,
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ActivityFinished {
                                activity_id: call.call_id.clone(),
                                summary: "denied by permission policy".into(),
                                succeeded: false,
                                duration_ms: activity_started.elapsed().as_millis() as u64,
                            },
                        )?;
                        input.push(function_output(
                            &call,
                            json!({"error": message, "recoverable": false}).to_string(),
                        ));
                        continue;
                    }
                    if matches!(permission, crate::config::PermissionLevel::Allow)
                        || self.take_exec_approval(run_id, &arguments)
                    {
                        call_executor = call_executor.with_policy(ExecutorPolicy {
                            allow_destructive: true,
                        });
                    } else {
                        let request_id = RequestId::new();
                        let command = arguments.get("argv").and_then(Value::as_array).map(|values| {
                            values
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        });
                        self.store.update_run_state(
                            run_id,
                            ExitState::ApprovalRequired,
                            Some(model),
                            None,
                            Some(&reason),
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::Approval {
                                request_id,
                                agent_id,
                                reason: reason.clone(),
                                command,
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ActivityUpdated {
                                activity_id: call.call_id.clone(),
                                detail: "waiting for approval".into(),
                            },
                        )?;
                        if let Some(task_id) = task_id.as_deref() {
                            self.store.record_runtime_event(
                                run_id,
                                RuntimeEvent::PlanTaskChanged {
                                    task_id: task_id.to_owned(),
                                    state: PlanTaskState::Blocked,
                                    agent_id: Some(agent_id),
                                },
                            )?;
                        }
                        return Ok(AgentResult {
                            text: result.output_text,
                            question: Some(InputRequest {
                                question: reason,
                                options: vec!["yes".into(), "no".into()],
                            }),
                            usage,
                            paused: false,
                            reserve_reached: false,
                            termination: Some(TerminationReason::Blocked),
                        });
                    }
                }
                let name = call.name.clone();
                let outcome = tokio::task::spawn_blocking(move || call_executor.execute(&name, &arguments))
                    .await
                    .map_err(|error| HarnessError::Io(std::io::Error::other(error.to_string())))?;
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let message = bound(&error.to_string(), 2_000);
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ToolOutput {
                                agent_id,
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                                stdout: String::new(),
                                stderr: message.clone(),
                                exit_code: None,
                                truncated: false,
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ActivityFinished {
                                activity_id: call.call_id.clone(),
                                summary: message.clone(),
                                succeeded: false,
                                duration_ms: activity_started.elapsed().as_millis() as u64,
                            },
                        )?;
                        input.push(function_output(
                            &call,
                            json!({"error": message, "recoverable": true}).to_string(),
                        ));
                        continue;
                    }
                };
                match outcome {
                    ToolOutcome::Output(output) => {
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ToolOutput {
                                agent_id,
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                                stdout: output.stdout.clone(),
                                stderr: output.stderr.clone(),
                                exit_code: output.exit_code,
                                truncated: output.truncated,
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ActivityFinished {
                                activity_id: call.call_id.clone(),
                                summary: if output.exit_code.is_some_and(|code| code != 0) {
                                    "tool failed"
                                } else {
                                    "tool completed"
                                }
                                .into(),
                                succeeded: output.exit_code.is_none_or(|code| code == 0),
                                duration_ms: activity_started.elapsed().as_millis() as u64,
                            },
                        )?;
                        let value = json!({
                            "stdout": output.stdout,
                            "stderr": output.stderr,
                            "exit_code": output.exit_code,
                            "truncated": output.truncated
                        });
                        input.push(function_output(&call, value.to_string()));
                    }
                    ToolOutcome::NeedsInput(question) => {
                        agent_record.state = AgentState::Waiting;
                        agent_record.updated_at = chrono::Utc::now();
                        self.store.upsert_agent(&agent_record)?;
                        self.store.update_run_state(
                            run_id,
                            ExitState::NeedsInput,
                            Some(model),
                            None,
                            Some(&question.question),
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::Question {
                                request_id: RequestId::new(),
                                agent_id,
                                question: question.question.clone(),
                                options: question.options.clone(),
                                blocking: true,
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::ActivityUpdated {
                                activity_id: call.call_id.clone(),
                                detail: "waiting for user input".into(),
                            },
                        )?;
                        self.store.record_runtime_event(
                            run_id,
                            RuntimeEvent::AgentState {
                                agent_id,
                                state: AgentState::Waiting,
                                detail: "waiting for user input".into(),
                            },
                        )?;
                        if let Some(task_id) = task_id.as_deref() {
                            self.store.record_runtime_event(
                                run_id,
                                RuntimeEvent::PlanTaskChanged {
                                    task_id: task_id.to_owned(),
                                    state: PlanTaskState::Blocked,
                                    agent_id: Some(agent_id),
                                },
                            )?;
                        }
                        return Ok(AgentResult {
                            text: result.output_text,
                            question: Some(question),
                            usage,
                            paused: false,
                            reserve_reached: false,
                            termination: Some(TerminationReason::Blocked),
                        });
                    }
                }
            }
        }
        Ok(AgentResult {
            text: if last_text.is_empty() {
                "Agent reached the turn limit before a terminal answer.".into()
            } else {
                format!("{last_text}\n\nAgent reached the turn limit before a terminal answer.")
            },
            question: None,
            usage,
            paused: false,
            reserve_reached: false,
            termination: Some(TerminationReason::TurnLimit),
        })
    }

    async fn execute_parallel_reads(
        &self,
        run_id: RunId,
        agent_id: EventAgentId,
        executor: &ToolExecutor,
        calls: &[ToolCall],
    ) -> Result<Vec<Value>, HarnessError> {
        let futures = FuturesUnordered::new();
        for call in calls.iter().cloned() {
            let arguments: Value = serde_json::from_str(&call.arguments)
                .map_err(|error| HarnessError::Tool(ToolError::InvalidArguments(error.to_string())))?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ToolStarted {
                    agent_id,
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    arguments: arguments.clone(),
                },
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ActivityStarted {
                    activity_id: call.call_id.clone(),
                    agent_id: Some(agent_id),
                    kind: activity_kind_for_tool(&call.name).into(),
                    summary: activity_summary_for_tool(&call.name, &arguments),
                },
            )?;
            let call_executor = executor.clone();
            futures.push(async move {
                let started = Instant::now();
                let name = call.name.clone();
                let outcome = tokio::task::spawn_blocking(move || call_executor.execute(&name, &arguments))
                    .await
                    .map_err(|error| ToolError::Io(std::io::Error::other(error.to_string())))
                    .and_then(|outcome| outcome);
                (call, outcome, started.elapsed().as_millis() as u64)
            });
        }
        let mut futures = futures;
        let mut outputs = HashMap::new();
        while let Some((call, outcome, duration_ms)) = futures.next().await {
            let (stdout, stderr, exit_code, truncated, succeeded) = match outcome {
                Ok(ToolOutcome::Output(output)) => (
                    output.stdout,
                    output.stderr,
                    output.exit_code,
                    output.truncated,
                    output.exit_code.is_none_or(|code| code == 0),
                ),
                Ok(ToolOutcome::NeedsInput(_)) => (
                    String::new(),
                    "read-only parallel tool unexpectedly requested input".into(),
                    None,
                    false,
                    false,
                ),
                Err(error) => (
                    String::new(),
                    bound(&error.to_string(), 2_000),
                    None,
                    false,
                    false,
                ),
            };
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ToolOutput {
                    agent_id,
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    exit_code,
                    truncated,
                },
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ActivityFinished {
                    activity_id: call.call_id.clone(),
                    summary: if succeeded {
                        "read completed"
                    } else {
                        "read failed"
                    }
                    .into(),
                    succeeded,
                    duration_ms,
                },
            )?;
            outputs.insert(
                call.call_id.clone(),
                json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": exit_code,
                    "truncated": truncated
                })
                .to_string(),
            );
        }
        Ok(calls
            .iter()
            .filter_map(|call| {
                outputs
                    .remove(&call.call_id)
                    .map(|output| function_output(call, output))
            })
            .collect())
    }

    async fn compact_if_needed(
        &self,
        run_id: RunId,
        client: &RuntimeProviderClient,
        model: &str,
        system: &str,
        input: &mut Vec<Value>,
        usage: &mut TokenUsage,
    ) -> Result<bool, HarnessError> {
        let estimated = estimate_tokens(system)
            + input
                .iter()
                .map(|item| estimate_tokens(&item.to_string()))
                .sum::<usize>();
        let policy = self.context_policy(model);
        let trigger = policy.effective_limit.saturating_sub(policy.output_allowance) as usize;
        let forced = self.take_force_compaction(run_id);
        if !forced && estimated < trigger {
            return Ok(false);
        }
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Compacting,
                detail: format!(
                    "forecast exceeds {} effective tokens for {model}; preserving the protected reserve",
                    policy.effective_limit
                ),
            },
        )?;
        let compaction_instructions = format!(
            "Compress the supplied agent transcript. Preserve requirements, decisions, exact paths/symbols, edits, test evidence, failures, open questions, and next actions. Keep at most {} explicit durable facts, ranked by relevance to unfinished work. Drop narration and raw logs. Return plain compact text.",
            self.config.context.fact_limit
        );
        let observed = std::iter::once(("system".to_owned(), system.as_bytes().to_vec()))
            .chain(
                input
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (format!("input-{index}"), item.to_string().into_bytes())),
            )
            .collect::<Vec<_>>();
        let manifest = ObservedInputManifest::observe(observed).ok();
        let request_key = manifest.as_ref().map(|manifest| {
            cache_key(
                &format!("compaction/v2/{}", self.config.models.lead),
                compaction_instructions.as_bytes(),
                manifest,
            )
        });
        let cache_enabled = self.config.cache.enabled && !self.cache_bypassed(run_id);
        let cached = if cache_enabled {
            if let Some(key) = request_key.as_deref() {
                if let Some(value) = self.hot_cached_result(key)? {
                    Some((value, "exact".to_owned()))
                } else if let Some(cached) = self.store.cached_result(&self.workspace_id, key)? {
                    self.remember_hot_result(key, cache_class_from_name(&cached.class), &cached.value);
                    Some((cached.value, cached.class))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let (Some(manifest), Some(key), Some((cached, class))) =
            (manifest.as_ref(), request_key.as_deref(), cached)
            && let Ok(summary) = String::from_utf8(cached)
        {
            self.store
                .record_cache_savings(&self.workspace_id, estimated as u64)?;
            self.store
                .record_compaction_checkpoint(run_id, &summary, manifest, estimated as u64)?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Cache {
                    hit: true,
                    class,
                    key_prefix: key.chars().take(12).collect(),
                    bytes: summary.len() as u64,
                    saved_input_tokens: estimated as u64,
                },
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Compacted {
                    summary: summary.clone(),
                    estimated_tokens_before: estimated,
                },
            )?;
            let recent = paired_recent_items(input, self.config.context.recent_turns);
            *input = vec![message("user", &format!("Prior compacted context:\n{summary}"))];
            input.extend(recent);
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::RunPhase {
                    phase: RunPhase::Working,
                    detail: "resumed after cached context checkpoint".into(),
                },
            )?;
            return Ok(false);
        }
        if cache_enabled && let Some(key) = request_key.as_deref() {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Cache {
                    hit: false,
                    class: "exact".into(),
                    key_prefix: key.chars().take(12).collect(),
                    bytes: 0,
                    saved_input_tokens: 0,
                },
            )?;
        }
        let reservation = (estimated as u64).saturating_add(1_024);
        if !self.try_reserve_budget(run_id, reservation) {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: "context compaction skipped because the session token budget is exhausted"
                        .into(),
                },
            )?;
            return Ok(false);
        }
        let compaction_model = self.config.models.lead.clone();
        let compaction_client = self.pooled_client(0, &compaction_model, client);
        let compaction_provider = compaction_client.provider_id();
        let compacted = compaction_client
            .turn(TurnRequest {
                model: provider_model_slug(&compaction_model).to_owned(),
                instructions: compaction_instructions,
                input: input.clone(),
                tools: Vec::new(),
                parallel_tool_calls: false,
                reasoning_effort: Some(reasoning_for_turn(&compaction_model, "compaction", 0, "low")),
                prompt_cache_key: Some(prompt_cache_key(&format!(
                    "{:?}|{}|compaction|{}|tools-v2|prompt-v3",
                    compaction_provider,
                    compaction_model,
                    self.root.to_string_lossy()
                ))),
                subagent_label: Some("compaction".into()),
                response_format: None,
            })
            .await;
        let compacted = match compacted {
            Ok(result) => {
                self.settle_budget(run_id, reservation, result.usage.total());
                result
            }
            Err(error) => {
                self.settle_budget(run_id, reservation, 0);
                return Err(error.into());
            }
        };
        if compacted.output_text.trim().is_empty() {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: "context compactor returned an empty checkpoint; original context was retained"
                        .into(),
                },
            )?;
            return Err(HarnessError::Provider(ProviderError::InvalidResponse(
                "context compactor returned an empty checkpoint",
            )));
        }
        if self.config.cache.enabled
            && !self.cache_bypassed(run_id)
            && !contains_secret("summary", compacted.output_text.as_bytes())
            && let (Some(key), Some(manifest)) = (request_key.as_deref(), manifest.as_ref())
        {
            let inserted = self.store.put_cached_result(
                &self.workspace_id,
                key,
                CacheClass::Exact,
                compacted.output_text.as_bytes(),
                manifest,
                None,
            )?;
            if inserted {
                self.remember_hot_result(key, CacheClass::Exact, compacted.output_text.as_bytes());
            }
            let _ = self
                .store
                .prune_cache(&self.workspace_id, self.config.cache.max_bytes)?;
        }
        if let Some(manifest) = manifest.as_ref() {
            self.store.record_compaction_checkpoint(
                run_id,
                &compacted.output_text,
                manifest,
                estimated as u64,
            )?;
        }
        *usage = add_usage(*usage, compacted.usage);
        self.store
            .add_usage(run_id, compacted.usage.input, compacted.usage.output)?;
        if !compacted.rate_limits.is_empty() {
            self.store.record_event(
                run_id,
                "usage.account_snapshot",
                serde_json::to_value(&compacted.rate_limits).unwrap_or_default(),
            )?;
        }
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::Compacted {
                summary: compacted.output_text.clone(),
                estimated_tokens_before: estimated,
            },
        )?;
        let recent = paired_recent_items(input, self.config.context.recent_turns);
        *input = vec![message(
            "user",
            &format!("Prior compacted context:\n{}", compacted.output_text),
        )];
        input.extend(recent);
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunPhase {
                phase: RunPhase::Working,
                detail: "resumed after context checkpoint".into(),
            },
        )?;
        let paused = reserve_reached(
            &compacted.rate_limits,
            self.config.scheduler.usage_reserve_percent,
        );
        if paused {
            self.record_usage_pause(
                run_id,
                "context compactor",
                &self.config.models.lead,
                &compacted.rate_limits,
            )?;
        }
        Ok(paused)
    }

    fn process_pending_memory_extractions(&self, event_run_id: RunId) -> Result<(), HarnessError> {
        let settings = self.store.memory_settings(&self.workspace_id)?;
        if !self.config.memory.enabled
            || !self.config.memory.generate
            || !settings.enabled
            || !settings.generate
        {
            return Ok(());
        }
        for source_run_id in self.store.pending_memory_extractions(4)? {
            let Some(run) = self.store.run(source_run_id)? else {
                self.store.finish_memory_extraction(source_run_id, "missing")?;
                continue;
            };
            let Some(summary) = run
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            else {
                self.store.finish_memory_extraction(source_run_id, "empty")?;
                continue;
            };
            let mut memory = MemoryRecord::candidate(
                MemoryScope::Run,
                "run_outcome",
                bound(&run.title, 160),
                bound(summary, 1_200),
            );
            memory.run_id = Some(source_run_id);
            memory.workspace_id = Some(self.workspace_id.clone());
            memory.confidence = if run.state == ExitState::Succeeded { 75 } else { 55 };
            memory.salience = 45;
            memory.provenance = vec![format!("run:{source_run_id}")];
            match self.store.put_memory(memory) {
                Ok(memory) => {
                    self.store.finish_memory_extraction(source_run_id, "complete")?;
                    self.store.record_runtime_event(
                        event_run_id,
                        RuntimeEvent::MemoryChanged {
                            memory_id: memory.id,
                            action: "extracted".into(),
                            scope: memory.scope.as_str().into(),
                        },
                    )?;
                }
                Err(crate::store::StoreError::Memory(_)) => {
                    self.store.finish_memory_extraction(source_run_id, "rejected")?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn record_usage_pause(
        &self,
        run_id: RunId,
        role: &str,
        model: &str,
        snapshots: &[crate::usage::RateLimitSnapshot],
    ) -> Result<(), HarnessError> {
        self.store.record_event(
            run_id,
            "usage.reserve_reached",
            json!({
                "role": role,
                "model": model,
                "reserve_percent": self.config.scheduler.usage_reserve_percent,
                "account": snapshots,
            }),
        )?;
        Ok(())
    }

    fn system_prompt(&self, goal: &str, role: &str, read_only: bool) -> Result<String, HarnessError> {
        let instructions = discover_instructions(&self.root, &self.root)?;
        let clarifier = role.contains("issue clarifier") || role.contains("ambiguity consultant");
        let skills = if clarifier {
            Vec::new()
        } else {
            discover_skills(&self.root)?
        };
        let agents = if clarifier {
            Vec::new()
        } else {
            discover_agents(&self.root)?
        };
        let question_rule = match self.config.scheduler.question_policy {
            crate::config::QuestionPolicy::AgentDiscretion => {
                "Ask a concise question only when it materially improves the result."
            }
            crate::config::QuestionPolicy::OnlyBlocking => {
                "Ask the user only when a missing decision makes safe progress impossible."
            }
            crate::config::QuestionPolicy::Never => {
                "Never ask the user; state the safest bounded assumption instead."
            }
        };
        let mut prompt = format!(
            "You are an agent in Minha, a fast token-conscious coding hivemind. Workspace: {}. Use only supplied fixed tools; no MCP. Work from evidence. {question_rule} Never redeem credits. Never push or perform remote writes unless the runtime presents the exact operation and the user explicitly approves it. Keep tool output narrow: search before reading, request line ranges, and avoid repeating evidence already in context. Prefer one `quality` call over separate linter/test calls. Use structured read-only `github` queries before raw `gh`; remote GitHub mutations go through permission-gated `exec`. Use `hive` only for durable coordination, blockers, and content-addressed artifacts; use `books` lazily when a curated technical reference will save inspection or reasoning tokens.\n",
            self.root.display()
        );
        if clarifier && !instructions.is_empty() {
            prompt.push_str("\nRepository instruction files are available for targeted read-only lookup: ");
            prompt.push_str(
                &instructions
                    .iter()
                    .map(|instruction| instruction.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            prompt.push_str(
                ". Read only a relevant range when a repository rule can replace a user question.\n",
            );
        } else if !instructions.is_empty() {
            prompt.push_str("\nRepository instructions, low to high precedence:\n");
            let mut remaining = 48 * 1024usize;
            for instruction in instructions {
                if remaining == 0 {
                    break;
                }
                let content = bound(&instruction.content, remaining.min(24 * 1024));
                remaining = remaining.saturating_sub(content.len());
                prompt.push_str(&format!(
                    "\n<{} path=\"{}\">\n{}\n</{}>\n",
                    instruction.name,
                    instruction.path.display(),
                    content,
                    instruction.name
                ));
            }
        }
        if !skills.is_empty() {
            prompt.push_str("\nAvailable skills (metadata only):\n");
            for skill in &skills {
                prompt.push_str(&format!(
                    "- ${}: {}\n",
                    skill.name,
                    bound(&skill.description, 240)
                ));
            }
            for body in selected_skill_bodies(goal, &skills)? {
                prompt.push_str("\nSelected skill instructions:\n");
                prompt.push_str(&bound(&body, 24 * 1024));
            }
        }
        if !agents.is_empty() {
            prompt.push_str("\nCompatible project agents: ");
            prompt.push_str(
                &agents
                    .iter()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            prompt.push('\n');
            for agent in selected_agent_bodies(goal, &agents) {
                prompt.push_str(&format!(
                    "\nSelected agent {}:\n{}\n",
                    agent.name,
                    bound(&agent.content, 24 * 1024)
                ));
            }
        }
        prompt.push_str(&format!(
            "\nCurrent role: {role}. {}\n",
            if read_only {
                "You are read-only: inspect and verify, never edit source."
            } else {
                "You may make scoped local source edits. Preserve unrelated user changes."
            }
        ));
        if role.contains("Spark") {
            prompt.push_str("\nInternal communication compression:\n");
            prompt.push_str(include_str!("../../../bundled/skills/caveman/SKILL.md"));
        }
        Ok(prompt)
    }

    fn finish_agent_outcome(
        &self,
        run_id: RunId,
        kind: RunKind,
        model: &str,
        result: AgentResult,
        agents_used: usize,
        worktrees: Vec<PathBuf>,
    ) -> Result<RunOutcome, HarnessError> {
        let state = if result.paused {
            ExitState::UsagePaused
        } else if result.question.is_some() {
            self.store
                .run(run_id)?
                .filter(|run| run.state == ExitState::ApprovalRequired)
                .map_or(ExitState::NeedsInput, |run| run.state)
        } else if result
            .termination
            .is_some_and(|reason| reason != TerminationReason::Completed)
        {
            ExitState::Inconclusive
        } else {
            ExitState::Succeeded
        };
        self.finish_agent_outcome_with_state(run_id, kind, model, result, state, agents_used, worktrees)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_agent_outcome_with_state(
        &self,
        run_id: RunId,
        kind: RunKind,
        model: &str,
        result: AgentResult,
        state: ExitState,
        agents_used: usize,
        worktrees: Vec<PathBuf>,
    ) -> Result<RunOutcome, HarnessError> {
        let termination = result.termination.unwrap_or_else(|| {
            if state == ExitState::Succeeded {
                TerminationReason::Completed
            } else if state == ExitState::Cancelled {
                TerminationReason::Interrupted
            } else {
                TerminationReason::Blocked
            }
        });
        let question = result.question.map(InputRequestView::from);
        self.store.update_run_state(
            run_id,
            state,
            Some(model),
            Some(&result.text),
            question.as_ref().map(|value| value.question.as_str()),
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::RunStopped {
                reason: termination,
                detail: result.text.clone(),
            },
        )?;
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::SessionFinished {
                state,
                model: Some(model.to_owned()),
                text: result.text.clone(),
                agents_used,
            },
        )?;
        if !matches!(
            state,
            ExitState::Running | ExitState::NeedsInput | ExitState::ApprovalRequired | ExitState::UsagePaused
        ) {
            let summary = if result.text.trim().is_empty() {
                format!("run ended: {termination:?}")
            } else {
                bound(&result.text, 600)
            };
            self.store.close_office_rooms(run_id, &summary)?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::OfficeRoomChanged {
                    room_id: "*".into(),
                    kind: "run".into(),
                    state: "closed".into(),
                    purpose: summary,
                },
            )?;
        }
        let memory_settings = self.store.memory_settings(&self.workspace_id)?;
        if self.config.memory.enabled
            && self.config.memory.generate
            && memory_settings.enabled
            && memory_settings.generate
            && matches!(
                termination,
                TerminationReason::Completed | TerminationReason::Blocked
            )
            && matches!(state, ExitState::Succeeded | ExitState::Inconclusive)
        {
            self.store.queue_memory_extraction(run_id)?;
        }
        Ok(RunOutcome {
            run_id,
            state,
            kind,
            model: Some(model.to_owned()),
            text: result.text,
            question,
            clarification: self.store.issue_clarification(run_id)?,
            usage: result.usage,
            agents_used,
            worktrees,
        })
    }

    async fn client(&self) -> Result<RuntimeProviderClient, HarnessError> {
        self.clients()
            .await?
            .into_iter()
            .next()
            .ok_or(HarnessError::LoginRequired)
    }

    async fn clients(&self) -> Result<Vec<RuntimeProviderClient>, HarnessError> {
        #[cfg(test)]
        {
            let injected = self.account_clients.lock().clone();
            if !injected.is_empty() {
                return Ok(injected);
            }
        }
        let profiles = enabled_account_records().await?;
        let active = active_account_profile().await?.map(|profile| profile.name);
        let mut profiles = profiles;
        profiles.sort_by_key(|(profile, _)| usize::from(active.as_deref() != Some(&profile.name)));
        let mut clients = Vec::with_capacity(profiles.len().saturating_add(1));
        if profiles.is_empty()
            && let Ok(client) = self.legacy_default_client().await
        {
            clients.push(client);
        }
        for (profile, mut auth) in profiles {
            let now = chrono::Utc::now().timestamp();
            if auth.expires_at_unix.is_some_and(|expiry| expiry <= now + 120)
                && let Some(refresh) = auth.refresh_token.clone()
            {
                match CodexOAuthClient::new(openai_oauth_config())?
                    .refresh(&refresh)
                    .await
                {
                    Ok(refreshed) => {
                        auth = merge_refreshed_auth(auth, refreshed);
                        save_account_profile(&profile.name, &profile.label, &auth, false).await?;
                    }
                    Err(error) if clients.is_empty() => return Err(error.into()),
                    Err(_) => continue,
                }
            }
            let Some(account) = auth.account_id.clone() else {
                if clients.is_empty() {
                    return Err(HarnessError::MissingAccountId);
                }
                continue;
            };
            clients.push(RuntimeProviderClient::ChatGpt(ChatGptClient::new(
                CHATGPT_CODEX_BASE_URL,
                auth.access_token,
                account,
            )));
        }
        if let Some(path) = provider_credentials_path()
            && let Some(key) = load_deepseek_key(&path)?
        {
            clients.push(RuntimeProviderClient::DeepSeek(DeepSeekClient::new(
                DEEPSEEK_BASE_URL,
                key,
            )));
        }
        if clients.is_empty() {
            return Err(HarnessError::LoginRequired);
        }
        Ok(clients)
    }

    async fn legacy_default_client(&self) -> Result<RuntimeProviderClient, HarnessError> {
        let mut auth = load_default_auth().await?.ok_or(HarnessError::LoginRequired)?;
        let now = chrono::Utc::now().timestamp();
        if auth.expires_at_unix.is_some_and(|expiry| expiry <= now + 120)
            && let Some(refresh) = auth.refresh_token.clone()
        {
            let refreshed = CodexOAuthClient::new(openai_oauth_config())?
                .refresh(&refresh)
                .await?;
            auth = merge_refreshed_auth(auth, refreshed);
            save_default_auth(&auth).await?;
        }
        let account = auth.account_id.clone().ok_or(HarnessError::MissingAccountId)?;
        Ok(RuntimeProviderClient::ChatGpt(ChatGptClient::new(
            CHATGPT_CODEX_BASE_URL,
            auth.access_token,
            account,
        )))
    }

    fn pooled_client(
        &self,
        slot: usize,
        model: &str,
        fallback: &RuntimeProviderClient,
    ) -> RuntimeProviderClient {
        let clients = self.account_clients.lock();
        let provider = provider_for_model(model);
        let matching = clients
            .iter()
            .filter(|client| client.provider_id() == provider)
            .collect::<Vec<_>>();
        matching
            .get(slot % matching.len().max(1))
            .map_or_else(|| fallback.clone(), |client| (*client).clone())
    }

    fn is_interrupted(&self, run_id: RunId) -> bool {
        self.controls
            .lock()
            .get(&run_id)
            .is_some_and(|control| control.interrupted)
    }

    fn take_steering(&self, run_id: RunId) -> Vec<String> {
        self.controls
            .lock()
            .entry(run_id)
            .or_default()
            .steering
            .drain(..)
            .collect()
    }

    fn take_cooperative_pause(&self, run_id: RunId) -> bool {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        std::mem::take(&mut control.cooperative_pause)
    }

    fn take_exec_approval(&self, run_id: RunId, arguments: &Value) -> bool {
        let requested = arguments.get("argv").and_then(Value::as_array).map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        let approved = control.approved_exec_once.take();
        approved.is_some() && approved == requested
    }

    fn take_force_compaction(&self, run_id: RunId) -> bool {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        std::mem::take(&mut control.force_compaction)
    }

    fn cache_bypassed(&self, run_id: RunId) -> bool {
        self.controls
            .lock()
            .get(&run_id)
            .is_some_and(|control| control.bypass_cache)
    }

    fn try_reserve_budget(&self, run_id: RunId, tokens: u64) -> bool {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        control.budget_tokens = control.budget_tokens.saturating_add(tokens);
        true
    }

    fn settle_budget(&self, run_id: RunId, reserved: u64, actual: u64) {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        control.budget_tokens = control
            .budget_tokens
            .saturating_sub(reserved)
            .saturating_add(actual);
    }

    fn context_policy(&self, model: &str) -> ContextPolicy {
        ContextPolicy::resolve(
            model,
            self.model_context_limits.lock().get(model).copied(),
            self.config.context.context_limit,
        )
    }
}

fn initial_agent_state(role: &str) -> AgentState {
    let role = role.to_ascii_lowercase();
    if role.contains("planner") {
        AgentState::Planning
    } else if role.contains("judge") || role.contains("review") || role.contains("auditor") {
        AgentState::Verifying
    } else if role.contains("integrator") || role.contains("synthesizer") {
        AgentState::Integrating
    } else {
        AgentState::Working
    }
}

fn agent_turn_limit(role: &str) -> usize {
    let role = role.to_ascii_lowercase();
    if role.contains("ambiguity consultant") {
        2
    } else if role.contains("issue clarifier") {
        4
    } else if role.contains("router") || role.contains("manager") {
        3
    } else if role.contains("auditor") {
        5
    } else if role.contains("judge") || role.contains("review") {
        6
    } else if role.contains("planner") {
        8
    } else if role.contains("worker") {
        10
    } else {
        12
    }
}

fn agent_input_budget(role: &str) -> u64 {
    let role = role.to_ascii_lowercase();
    if role.contains("ambiguity consultant") {
        12_000
    } else if role.contains("issue clarifier") {
        16_000
    } else if role.contains("router") {
        24_000
    } else if role.contains("manager") {
        32_000
    } else if role.contains("auditor") || role.contains("judge") || role.contains("review") {
        80_000
    } else if role.contains("worker") || role.contains("planner") {
        160_000
    } else {
        200_000
    }
}

fn agent_tool_budget(role: &str) -> usize {
    let role = role.to_ascii_lowercase();
    if role.contains("ambiguity consultant") {
        0
    } else if role.contains("issue clarifier") {
        4
    } else if role.contains("manager") {
        0
    } else if role.contains("router") {
        4
    } else if role.contains("auditor") || role.contains("judge") || role.contains("review") {
        12
    } else if role.contains("planner") {
        16
    } else {
        32
    }
}

fn paired_recent_items(input: &[Value], keep: usize) -> Vec<Value> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut start = input.len().saturating_sub(keep.max(1));
    loop {
        let mut expanded = start;
        for output in &input[start..] {
            if output.get("type").and_then(Value::as_str) != Some("function_call_output") {
                continue;
            }
            let Some(call_id) = output.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(index) = input[..start].iter().rposition(|candidate| {
                candidate.get("type").and_then(Value::as_str) == Some("function_call")
                    && candidate.get("call_id").and_then(Value::as_str) == Some(call_id)
            }) {
                expanded = expanded.min(index);
            }
        }
        if expanded == start {
            break;
        }
        start = expanded;
    }
    input[start..].to_vec()
}

fn worker_task_id(role: &str) -> Option<&str> {
    role.strip_prefix("Spark worker ")
        .or_else(|| role.strip_prefix("Lead task "))
}

fn is_affirmative(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "approve" | "approved" | "allow"
    )
}

#[derive(Clone, Debug)]
struct AgentResult {
    text: String,
    question: Option<InputRequest>,
    usage: TokenUsage,
    paused: bool,
    reserve_reached: bool,
    termination: Option<TerminationReason>,
}

impl AgentResult {
    fn usage_pause(text: String, usage: TokenUsage) -> Self {
        Self {
            text,
            question: None,
            usage,
            paused: true,
            reserve_reached: true,
            termination: Some(TerminationReason::ProviderReserve),
        }
    }

    fn pause_before_next_call(&self) -> bool {
        self.paused || self.reserve_reached
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BranchPlan {
    summary: String,
    #[serde(default)]
    consult: Option<String>,
    tasks: Vec<BranchTask>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct BranchTask {
    id: String,
    objective: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Default)]
struct WorkerGraphResult {
    reports: Vec<String>,
    usage: TokenUsage,
    question: Option<InputRequest>,
    paused: bool,
    lanes: Vec<PathBuf>,
    agents_used: usize,
}

struct JudgeContext {
    client: RuntimeProviderClient,
    agents_used: usize,
    worktrees: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
enum WorkerLane {
    Git { baseline: PathBuf, path: PathBuf },
    Snapshot { baseline: PathBuf, path: PathBuf },
}

impl WorkerLane {
    fn path(&self) -> &Path {
        match self {
            Self::Git { path, .. } | Self::Snapshot { path, .. } => path,
        }
    }

    fn patch(&self) -> Result<String, GitError> {
        match self {
            Self::Git { baseline, path } => {
                let filtered = baseline.with_file_name(format!(
                    "{}-changed-{}",
                    baseline
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map_or("lane", |name| name),
                    uuid::Uuid::now_v7()
                ));
                copy_workspace(path, &filtered).map_err(GitError::from)?;
                let result = diff_snapshots(baseline, &filtered);
                let _ = std::fs::remove_dir_all(&filtered);
                result
            }
            Self::Snapshot { baseline, path } => diff_snapshots(baseline, path),
        }
    }
}

fn prepare_worker_lane(
    root: &Path,
    lane_base: &Path,
    run_id: RunId,
    task: &TaskRecord,
    attempt: u32,
    use_git_worktrees: bool,
) -> Result<WorkerLane, HarnessError> {
    let id = safe_component(&task.task_id);
    if use_git_worktrees {
        let path = lane_base.join(format!("{id}-g{}", task.generation));
        if !path.exists() {
            let branch = format!("minha/{}/{id}-g{}", short_id(run_id), task.generation);
            GitRepo::new(root).add_worktree(&path, &branch, Some("HEAD"))?;
        }
        let recovery = root.join(".minha/recovery").join(run_id.to_string());
        if recovery.is_dir() {
            let executor = ToolExecutor::new(&path, false)?;
            let mut patches = std::fs::read_dir(recovery)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".applied.patch"))
                })
                .collect::<Vec<_>>();
            patches.sort();
            for patch in patches {
                let patch = std::fs::read_to_string(patch)?;
                if !patch.trim().is_empty() {
                    executor.execute("apply_patch", &json!({"patch": patch}))?;
                }
            }
        }
        let baseline = lane_base.join(format!("{id}-g{}-base", task.generation));
        copy_workspace(&path, &baseline)?;
        Ok(WorkerLane::Git { baseline, path })
    } else {
        let prefix = format!("{id}-g{}-a{attempt}", task.generation);
        let baseline = lane_base.join(format!("{prefix}-base"));
        let path = lane_base.join(format!("{prefix}-lane"));
        copy_workspace(root, &baseline)?;
        copy_workspace(root, &path)?;
        Ok(WorkerLane::Snapshot { baseline, path })
    }
}

fn lease_resources(task: &TaskRecord) -> Vec<String> {
    if task.paths.is_empty() {
        vec![format!("task:{}", task.task_id)]
    } else {
        task.paths
            .iter()
            .map(|path| format!("path:{}", path.trim_start_matches("./").trim_end_matches('/')))
            .collect()
    }
}

fn disjoint_ready_tasks(tasks: Vec<TaskRecord>, limit: usize) -> Vec<TaskRecord> {
    let mut selected = Vec::<TaskRecord>::new();
    for task in tasks {
        if selected.len() >= limit {
            break;
        }
        if selected.iter().all(|other| !tasks_overlap(&task, other)) {
            selected.push(task);
        }
    }
    selected
}

fn tasks_overlap(left: &TaskRecord, right: &TaskRecord) -> bool {
    left.paths.iter().any(|left| {
        right.paths.iter().any(|right| {
            let left = left.trim_matches('/');
            let right = right.trim_matches('/');
            left == right
                || left
                    .strip_prefix(right)
                    .is_some_and(|suffix| suffix.starts_with('/'))
                || right
                    .strip_prefix(left)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    })
}

fn single_task_plan(goal: &str) -> BranchPlan {
    BranchPlan {
        summary: "Single safe implementation lane".into(),
        consult: None,
        tasks: vec![BranchTask {
            id: "main".into(),
            objective: goal.into(),
            paths: Vec::new(),
            dependencies: Vec::new(),
        }],
    }
}

fn validate_branch_plan(mut plan: BranchPlan) -> Result<BranchPlan, String> {
    plan.tasks.truncate(MAX_PLAN_TASKS);
    if plan.tasks.is_empty() {
        return Err("plan has no tasks".into());
    }
    let mut ids = HashSet::new();
    let mut branch_ids = HashSet::new();
    for task in &plan.tasks {
        if task.id.trim().is_empty() || task.objective.trim().is_empty() {
            return Err("task id and objective must be non-empty".into());
        }
        if !ids.insert(task.id.clone()) {
            return Err(format!("duplicate task id {}", task.id));
        }
        if !branch_ids.insert(safe_component(&task.id)) {
            return Err(format!("task id {} collides after branch normalization", task.id));
        }
        for path in &task.paths {
            if Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
            {
                return Err(format!("task {} has unsafe path {path}", task.id));
            }
        }
    }
    for task in &plan.tasks {
        for dependency in &task.dependencies {
            if dependency == &task.id || !ids.contains(dependency) {
                return Err(format!("task {} has invalid dependency {dependency}", task.id));
            }
        }
    }
    let mut resolved = HashSet::new();
    loop {
        let before = resolved.len();
        for task in &plan.tasks {
            if task
                .dependencies
                .iter()
                .all(|dependency| resolved.contains(dependency))
            {
                resolved.insert(task.id.clone());
            }
        }
        if resolved.len() == plan.tasks.len() {
            return Ok(plan);
        }
        if resolved.len() == before {
            return Err("task dependencies contain a cycle".into());
        }
    }
}

fn board_entry_view(entry: &BoardEntry) -> BoardEntryView {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutoMode {
    Chat,
    Implement,
    Plan,
    Audit,
    Review,
}

fn parse_auto_mode(text: &str) -> AutoMode {
    let lower = text.to_ascii_lowercase();
    for (tag, mode) in [
        ("<minha-mode>implement</minha-mode>", AutoMode::Implement),
        ("<minha-mode>plan</minha-mode>", AutoMode::Plan),
        ("<minha-mode>audit</minha-mode>", AutoMode::Audit),
        ("<minha-mode>review</minha-mode>", AutoMode::Review),
        ("<minha-mode>chat</minha-mode>", AutoMode::Chat),
    ] {
        if lower.contains(tag) {
            return mode;
        }
    }
    AutoMode::Chat
}

const fn auto_mode_name(mode: AutoMode) -> &'static str {
    match mode {
        AutoMode::Chat => "chat",
        AutoMode::Implement => "implement",
        AutoMode::Plan => "plan",
        AutoMode::Audit => "audit",
        AutoMode::Review => "review",
    }
}

fn local_auto_mode(text: &str) -> Option<AutoMode> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Some(AutoMode::Chat);
    }
    let conversational = text.trim_matches(|character: char| !character.is_alphanumeric());
    if matches!(
        conversational,
        "hello"
            | "hi"
            | "hey"
            | "hiya"
            | "thanks"
            | "thank you"
            | "good morning"
            | "good afternoon"
            | "good evening"
    ) {
        return Some(AutoMode::Chat);
    }
    let starts = |words: &[&str]| words.iter().any(|word| text.starts_with(word));
    if starts(&["/plan", "plan ", "make a plan", "design a plan"]) {
        Some(AutoMode::Plan)
    } else if starts(&["/audit", "audit ", "inspect for", "find bugs", "assess "]) {
        Some(AutoMode::Audit)
    } else if starts(&["/review", "review ", "code review", "review the diff"]) {
        Some(AutoMode::Review)
    } else if starts(&[
        "/implement",
        "implement ",
        "fix ",
        "add ",
        "change ",
        "update ",
        "remove ",
        "refactor ",
        "build ",
        "create ",
        "continue working",
        "keep working",
    ]) {
        Some(AutoMode::Implement)
    } else if text.ends_with('?')
        || starts(&[
            "what ",
            "why ",
            "how ",
            "where ",
            "when ",
            "who ",
            "tell me ",
            "explain ",
            "summarize ",
            "is ",
            "are ",
        ])
    {
        Some(AutoMode::Chat)
    } else {
        None
    }
}

fn message(role: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{"type": "input_text", "text": text}]
    })
}

fn function_output(call: &ToolCall, output: String) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": call.call_id,
        "output": output
    })
}

fn condense_consumed_tool_outputs(input: &mut [Value]) {
    const RETAINED_BYTES: usize = 2_048;
    let call_names = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|item| {
            Some((
                item.get("call_id")?.as_str()?.to_owned(),
                item.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect::<HashMap<_, _>>();
    for item in input {
        if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
            continue;
        }
        let Some(output) = item.get("output").and_then(Value::as_str) else {
            continue;
        };
        if output.len() <= RETAINED_BYTES {
            continue;
        }
        let digest = Sha256::digest(output.as_bytes());
        let excerpt = bound(output, RETAINED_BYTES);
        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or_default();
        let tool = call_names.get(call_id).map(String::as_str).unwrap_or("tool");
        let parsed = serde_json::from_str::<Value>(output).ok();
        let exit_code = parsed
            .as_ref()
            .and_then(|value| value.get("exit_code"))
            .cloned()
            .unwrap_or(Value::Null);
        let truncated = parsed
            .as_ref()
            .and_then(|value| value.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        item["output"] = json!({
            "evidence_summary_version": 1,
            "tool": tool,
            "evidence_kind": match tool {
                "read_files" => "read",
                "search" => "search",
                "exec" | "quality" => "command_or_test",
                "apply_patch" => "patch",
                _ => "tool",
            },
            "bytes": output.len(),
            "sha256": format!("{digest:x}"),
            "exit_code": exit_code,
            "truncated": truncated,
            "retained_excerpt": excerpt,
            "raw_output": "stored in the durable runtime event stream"
        })
        .to_string()
        .into();
    }
}

fn ensure_model(available: &HashSet<String>, model: &str) -> Result<(), HarnessError> {
    if available.contains(model) {
        Ok(())
    } else {
        Err(HarnessError::ModelUnavailable(model.to_owned()))
    }
}

fn first_available<'a>(available: &HashSet<String>, candidates: &[&'a str]) -> Result<&'a str, HarnessError> {
    candidates
        .iter()
        .copied()
        .find(|model| available.contains(*model))
        .ok_or_else(|| {
            HarnessError::ModelUnavailable(
                candidates
                    .first()
                    .copied()
                    .unwrap_or("supported Codex model")
                    .to_owned(),
            )
        })
}

fn routed_lead_model<'a>(
    goal: &str,
    available: &HashSet<String>,
    config: &'a Config,
) -> Result<&'a str, HarnessError> {
    if complexity_score(goal) >= 5 && available.contains("deepseek/deepseek-v4-pro") {
        return Ok("deepseek/deepseek-v4-pro");
    }
    let preferred = if complexity_score(goal) >= 5 {
        &config.models.complex_lead
    } else {
        &config.models.lead
    };
    first_available(
        available,
        &[
            preferred,
            &config.models.lead,
            &config.models.complex_lead,
            &config.models.planner,
            &config.models.worker_deep,
            &config.models.worker_fast,
            "deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-flash",
        ],
    )
}

fn routed_worker_model<'a>(task: &TaskRecord, available: &HashSet<String>, config: &'a Config) -> &'a str {
    let score = complexity_score(&format!(
        "{}\n{}\n{}",
        task.objective,
        task.paths.join(" "),
        task.dependencies.join(" ")
    ));
    if (task.attempt > 1 || task.last_error.is_some()) && available.contains("deepseek/deepseek-v4-pro") {
        return "deepseek/deepseek-v4-pro";
    }
    if score < 7 && available.contains("deepseek/deepseek-v4-flash") {
        return "deepseek/deepseek-v4-flash";
    }
    let candidates = if score >= 7 {
        [
            config.models.worker_deep.as_str(),
            config.models.worker_medium.as_str(),
            config.models.worker_fast.as_str(),
        ]
    } else if score >= 4 {
        [
            config.models.worker_medium.as_str(),
            config.models.worker_fast.as_str(),
            config.models.worker_deep.as_str(),
        ]
    } else {
        [
            config.models.worker_fast.as_str(),
            config.models.worker_medium.as_str(),
            config.models.worker_deep.as_str(),
        ]
    };
    candidates
        .into_iter()
        .find(|model| available.contains(*model))
        .or_else(|| {
            ["deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash"]
                .into_iter()
                .find(|model| available.contains(*model))
        })
        .unwrap_or(config.models.worker_fast.as_str())
}

fn complexity_score(text: &str) -> u8 {
    let lower = text.to_ascii_lowercase();
    let mut score = 0_u8;
    score += u8::from(text.len() > 320);
    score += u8::from(text.len() > 1_200);
    score += u8::from(text.lines().count() > 8);
    score += u8::from(text.matches('/').count() > 4);
    for terms in [
        ["architecture", "migration", "cross-cutting", "redesign"],
        ["security", "authentication", "permission", "secret"],
        ["concurrent", "race", "distributed", "hivemind"],
        ["database", "schema", "protocol", "compatibility"],
        ["release", "production", "destructive", "high-risk"],
    ] {
        score = score.saturating_add(u8::from(terms.iter().any(|term| lower.contains(term))));
    }
    score.min(10)
}

fn should_delegate(goal: &str, profile: crate::config::ExecutionProfile) -> bool {
    let score = complexity_score(goal);
    let lower = goal.to_ascii_lowercase();
    let explicit_parallelism = [
        "independent tasks",
        "in parallel",
        "parallel work",
        "multiple agents",
        "audit every",
        "across the workspace",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    match profile {
        crate::config::ExecutionProfile::Economy => false,
        crate::config::ExecutionProfile::Balanced => explicit_parallelism || score >= 5,
        crate::config::ExecutionProfile::Turbo => explicit_parallelism || score >= 2,
    }
}

fn model_label(model: &str) -> &'static str {
    if model.contains("spark") {
        "Spark"
    } else if model.contains("terra") {
        "Terra"
    } else if model.contains("sol") {
        "Sol"
    } else if model.contains("luna") {
        "Luna"
    } else {
        "Codex"
    }
}

fn activity_kind_for_tool(name: &str) -> &'static str {
    match name {
        "read_files" => "explored",
        "search" => "searched",
        "apply_patch" => "edited",
        "quality" | "exec" => "ran_checks",
        "hive" => "delegated",
        _ => "tool",
    }
}

fn activity_summary_for_tool(name: &str, arguments: &Value) -> String {
    let summary = match name {
        "read_files" => arguments
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| file.get("path").and_then(Value::as_str))
            .take(4)
            .collect::<Vec<_>>()
            .join(", "),
        "search" => arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("workspace search")
            .to_owned(),
        "exec" => arguments
            .get("argv")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(8)
            .collect::<Vec<_>>()
            .join(" "),
        "quality" => arguments
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("quality")
            .to_owned(),
        "hive" => format!(
            "{} to {}",
            arguments.get("kind").and_then(Value::as_str).unwrap_or("message"),
            arguments.get("to").and_then(Value::as_str).unwrap_or("manager")
        ),
        _ => name.to_owned(),
    };
    bound(&summary, 240)
}

fn consultation_route<'a>(plan: &BranchPlan, config: &'a Config) -> Option<(&'a str, &'static str)> {
    match plan.consult.as_deref()?.trim().to_ascii_lowercase().as_str() {
        "terra" => Some((&config.models.consult_ambiguous, "Terra ambiguity consultant")),
        "sol" => Some((&config.models.consult_high_risk, "Sol high-risk consultant")),
        _ => None,
    }
}

fn incident_for(run_id: RunId, error: &HarnessError) -> IncidentView {
    let (code, severity, category, retryable, actions) = match error {
        HarnessError::LoginRequired => (
            "auth.login_required",
            IncidentSeverity::Warning,
            "authentication",
            false,
            vec!["Run `minha login` and retry the session.".into()],
        ),
        HarnessError::MissingAccountId => (
            "auth.account_id_missing",
            IncidentSeverity::Error,
            "authentication",
            false,
            vec!["Sign out and log in again to refresh the account record.".into()],
        ),
        HarnessError::ModelUnavailable(_) => (
            "model.unavailable",
            IncidentSeverity::Error,
            "model",
            true,
            vec!["Check `/models`, enable a supported model, then retry.".into()],
        ),
        HarnessError::Provider(ProviderError::Http { status, .. })
            if *status == reqwest::StatusCode::TOO_MANY_REQUESTS =>
        {
            (
                "provider.rate_limited",
                IncidentSeverity::Warning,
                "provider",
                true,
                vec!["Wait for the account window to recover or enable another account profile.".into()],
            )
        }
        HarnessError::Provider(ProviderError::Http { status, .. }) if status.is_server_error() => (
            "provider.unavailable",
            IncidentSeverity::Error,
            "provider",
            true,
            vec!["Retry in the same session; durable task state is preserved.".into()],
        ),
        HarnessError::Provider(ProviderError::Request(_))
        | HarnessError::Provider(ProviderError::IncompleteStream) => (
            "provider.transport",
            IncidentSeverity::Error,
            "network",
            true,
            vec!["Check connectivity and retry in the same session.".into()],
        ),
        HarnessError::Interrupted => (
            "run.interrupted",
            IncidentSeverity::Info,
            "runtime",
            true,
            vec!["Resume or retry when ready.".into()],
        ),
        HarnessError::Tool(ToolError::CommandDenied(_)) | HarnessError::Tool(ToolError::ReadOnlyDenied) => (
            "tool.permission_denied",
            IncidentSeverity::Warning,
            "permission",
            false,
            vec!["Review the requested operation and the current permission policy.".into()],
        ),
        HarnessError::Store(_) => (
            "state.persistence",
            IncidentSeverity::Critical,
            "storage",
            false,
            vec!["Run `/doctor` and preserve `.minha/minha.sqlite3` for diagnosis.".into()],
        ),
        HarnessError::Config(_) => (
            "config.invalid",
            IncidentSeverity::Error,
            "configuration",
            false,
            vec!["Fix `minha.toml` using `minha.toml.example` as a reference.".into()],
        ),
        _ => (
            "run.failed",
            IncidentSeverity::Error,
            "runtime",
            false,
            vec!["Inspect the Problems view and retry only after addressing the cause.".into()],
        ),
    };
    IncidentView {
        code: code.into(),
        severity,
        category: category.into(),
        summary: bound(&error.to_string(), 1_000),
        retryable,
        correlation_id: format!("{}-{}", short_id(run_id), uuid::Uuid::now_v7()),
        actions,
    }
}

fn mode_for(kind: RunKind) -> Mode {
    match kind {
        RunKind::Review | RunKind::Audit => Mode::Review,
        RunKind::Implement | RunKind::Plan => Mode::Batch,
        RunKind::Auto => Mode::Interactive,
    }
}

const fn run_kind_name(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Auto => "auto",
        RunKind::Implement => "implement",
        RunKind::Plan => "plan",
        RunKind::Audit => "audit",
        RunKind::Review => "review",
    }
}

fn cache_class_from_name(name: &str) -> CacheClass {
    match name {
        "ttl" => CacheClass::Ttl,
        "never" => CacheClass::Never,
        _ => CacheClass::Exact,
    }
}

fn stored_run_kind(events: &[crate::protocol::EventEnvelope]) -> Option<RunKind> {
    if let Some(kind) = events.iter().rev().find_map(|event| match &event.event {
        RuntimeEvent::RoutingDecision { mode, .. } => match mode.as_str() {
            "implement" => Some(RunKind::Implement),
            "plan" => Some(RunKind::Plan),
            "audit" => Some(RunKind::Audit),
            "review" => Some(RunKind::Review),
            "chat" => Some(RunKind::Auto),
            _ => None,
        },
        _ => None,
    }) {
        return Some(kind);
    }
    events.iter().find_map(|event| match &event.event {
        RuntimeEvent::SessionStarted { kind, .. } => match kind.as_str() {
            "auto" => Some(RunKind::Auto),
            "implement" => Some(RunKind::Implement),
            "plan" => Some(RunKind::Plan),
            "audit" => Some(RunKind::Audit),
            "review" => Some(RunKind::Review),
            _ => None,
        },
        RuntimeEvent::Legacy { kind, payload } if kind == "run.started" => {
            serde_json::from_value(payload.get("kind")?.clone()).ok()
        }
        _ => None,
    })
}

fn clarification_answer_display(
    clarification: &IssueClarificationView,
    answers: &[(String, String)],
) -> String {
    let mut display = Vec::new();
    for (id, value) in answers {
        if id.starts_with("$note:") || id == "$note" || id == "$edit" {
            continue;
        }
        if id == "$action" {
            display.push(match value.as_str() {
                "confirm" => "Confirm".into(),
                "edit" => "Edit details".into(),
                "keep_clarifying" | "keep clarifying" => "Keep clarifying".into(),
                "cancel" => "Cancel".into(),
                other => other.to_owned(),
            });
            continue;
        }
        let question = clarification
            .pending_batch
            .as_ref()
            .and_then(|batch| batch.questions.iter().find(|question| question.id == *id));
        let label = question
            .and_then(|question| {
                question
                    .options
                    .iter()
                    .find(|option| option.value == *value)
                    .map(|option| option.label.as_str())
            })
            .unwrap_or(value);
        display.push(question.map_or_else(
            || label.to_owned(),
            |question| format!("{}: {label}", question.header),
        ));
    }
    let notes = answers
        .iter()
        .filter(|(id, note)| {
            (id.starts_with("$note:") || id == "$note" || id == "$edit") && !note.trim().is_empty()
        })
        .map(|(_, note)| format!("Note: {}", note.trim()))
        .collect::<Vec<_>>();
    display.extend(notes);
    display.join("\n")
}

fn parse_plan(text: &str) -> Option<BranchPlan> {
    let start = text.find("<minha-plan>")? + "<minha-plan>".len();
    let tail = &text[start..];
    let end = tail.find("</minha-plan>").unwrap_or(tail.len());
    let mut plan: BranchPlan = serde_json::from_str(tail[..end].trim()).ok()?;
    plan.tasks.truncate(MAX_PLAN_TASKS);
    (!plan.tasks.is_empty()).then_some(plan)
}

fn prompt_cache_key(system: &str) -> String {
    let digest = Sha256::digest(system.as_bytes());
    format!("minha-{}", &format!("{digest:x}")[..24])
}

fn role_can_ask_user(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    [
        "lead",
        "planner",
        "integrator",
        "synthesizer",
        "intent router",
        "worker",
    ]
    .iter()
    .any(|marker| role.contains(marker))
}

fn permission_for_call(config: &Config, name: &str, arguments: &Value) -> crate::config::PermissionLevel {
    if name == "exec"
        && arguments
            .get("argv")
            .and_then(Value::as_array)
            .is_some_and(|argv| {
                let argv = argv.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                is_remote_write_argv(&argv)
            })
    {
        config.permissions.remote_writes
    } else {
        config.permissions.destructive
    }
}

fn is_remote_write_argv(argv: &[&str]) -> bool {
    let Some(program) = argv
        .first()
        .and_then(|value| Path::new(value).file_name())
        .and_then(|value| value.to_str())
    else {
        return false;
    };
    let subcommand = argv.get(1).copied().unwrap_or_default();
    match program {
        "git" => subcommand == "push",
        "gh" | "ssh" | "scp" | "sftp" | "rsync" => true,
        "npm" | "pnpm" | "yarn" | "cargo" => subcommand == "publish",
        "docker" | "podman" => subcommand == "push",
        "kubectl" => matches!(
            subcommand,
            "apply" | "create" | "delete" | "edit" | "patch" | "replace" | "scale" | "set"
        ),
        "curl" => argv.iter().skip(1).any(|argument| {
            matches!(
                *argument,
                "-d" | "--data" | "--data-raw" | "--data-binary" | "-F" | "--form" | "-T" | "--upload-file"
            ) || argument.eq_ignore_ascii_case("POST")
                || argument.eq_ignore_ascii_case("PUT")
                || argument.eq_ignore_ascii_case("PATCH")
                || argument.eq_ignore_ascii_case("DELETE")
        }),
        _ => false,
    }
}

fn add_usage(a: TokenUsage, b: TokenUsage) -> TokenUsage {
    TokenUsage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cached_input: a.cached_input.saturating_add(b.cached_input),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        reasoning_output: a.reasoning_output.saturating_add(b.reasoning_output),
    }
}

fn bound(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

fn safe_component(input: &str) -> String {
    let value = input
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let value = value.trim_matches('-');
    if value.is_empty() {
        "task".into()
    } else {
        value.chars().take(48).collect()
    }
}

fn short_id(run_id: RunId) -> String {
    run_id.to_string().chars().take(8).collect()
}

fn selected_skill_bodies(goal: &str, skills: &[Skill]) -> Result<Vec<String>, HarnessError> {
    let lower = goal.to_ascii_lowercase();
    skills
        .iter()
        .filter(|skill| {
            lower.contains(&format!("${}", skill.name.to_ascii_lowercase()))
                || (skill.name == "caveman" && lower.contains("caveman mode"))
        })
        .map(load_skill)
        .collect::<Result<Vec<_>, _>>()
        .map_err(HarnessError::Io)
}

fn selected_agent_bodies<'a>(goal: &str, agents: &'a [AgentDefinition]) -> Vec<&'a AgentDefinition> {
    let lower = goal.to_ascii_lowercase();
    agents
        .iter()
        .filter(|agent| lower.contains(&format!("${}", agent.name.to_ascii_lowercase())))
        .collect()
}

fn merge_refreshed_auth(old: AuthRecord, mut refreshed: AuthRecord) -> AuthRecord {
    if refreshed.refresh_token.as_deref().is_none_or(str::is_empty) {
        refreshed.refresh_token = old.refresh_token;
    }
    if refreshed.id_token.as_deref().is_none_or(str::is_empty) {
        refreshed.id_token = old.id_token;
    }
    if refreshed.account_id.is_none() {
        refreshed.account_id = old.account_id;
    }
    if refreshed.email.is_none() {
        refreshed.email = old.email;
    }
    refreshed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_judgment_requires_a_versioned_typed_report() {
        let report = parse_judge_report(
            "<minha-judge>{\"schema_version\":1,\"verdict\":\"verified\",\"summary\":\"checks pass\",\"evidence\":[\"cargo test\"],\"findings\":[]}</minha-judge>",
        )
        .expect("typed report");
        assert_eq!(report.verdict, JudgeVerdictV1::Verified);
        assert!(parse_judge_report("VERDICT: verified").is_none());
        assert!(parse_judge_report(
            "<minha-judge>{\"schema_version\":2,\"verdict\":\"verified\",\"summary\":\"old\"}</minha-judge>"
        )
        .is_none());
    }
    use std::{
        io::{self, Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering as AtomicOrdering},
        },
        thread,
        time::Duration,
    };

    struct HiveFixtureServer {
        base_url: String,
        wake_address: SocketAddr,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<String>>>,
        join: Option<thread::JoinHandle<io::Result<()>>>,
    }

    impl HiveFixtureServer {
        fn start() -> io::Result<Self> {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            let address = listener.local_addr()?;
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_requests = Arc::clone(&requests);
            let join = thread::spawn(move || {
                while !thread_stop.load(AtomicOrdering::Relaxed) {
                    let (mut stream, _) = listener.accept()?;
                    if thread_stop.load(AtomicOrdering::Relaxed) {
                        break;
                    }
                    let request = read_fixture_request(&mut stream)?;
                    thread_requests.lock().push(request.clone());
                    write_fixture_response(&mut stream, &fixture_response(&request))?;
                }
                Ok(())
            });
            Ok(Self {
                base_url: format!("http://{address}"),
                wake_address: address,
                stop,
                requests,
                join: Some(join),
            })
        }

        fn request_labels(&self) -> Vec<String> {
            self.requests
                .lock()
                .iter()
                .filter_map(|request| {
                    request.lines().find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("x-openai-subagent: ")
                            .map(str::to_owned)
                    })
                })
                .collect()
        }
    }

    impl Drop for HiveFixtureServer {
        fn drop(&mut self) {
            self.stop.store(true, AtomicOrdering::Relaxed);
            let _ = TcpStream::connect(self.wake_address);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn read_fixture_request(stream: &mut TcpStream) -> io::Result<String> {
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut expected = None;
        loop {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if expected.is_none()
                && let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let body_start = header_end + 4;
                let headers = String::from_utf8_lossy(&bytes[..body_start]);
                let body_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .map_or(0, |length| length);
                expected = Some(body_start + body_length);
            }
            if expected.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn write_fixture_response(stream: &mut TcpStream, response: &str) -> io::Result<()> {
        let content_type = if response.starts_with('{') {
            "application/json"
        } else {
            "text/event-stream"
        };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )?;
        stream.flush()
    }

    fn fixture_response(request: &str) -> String {
        if request.starts_with("GET /models?") {
            return json!({"models": [
                {"slug":"gpt-5.6-luna"},
                {"slug":"gpt-5.6-terra"},
                {"slug":"gpt-5.6-sol"},
                {"slug":"gpt-5.4-mini"},
                {"slug":"gpt-5.3-codex-spark"}
            ]})
            .to_string();
        }
        let lower = request.to_ascii_lowercase();
        let has_tool_output = request.contains("function_call_output");
        if lower.contains("x-openai-subagent: branch_planner_lead") {
            return fixture_text(
                r#"<minha-plan>{"summary":"two independent fixes","consult":null,"tasks":[{"id":"slug","objective":"Implement slugify and run its test","paths":["src/slug.rs"],"dependencies":[]},{"id":"stats","objective":"Implement word_counts and run its test","paths":["src/stats.rs"],"dependencies":[]}]}</minha-plan>"#,
            );
        }
        if lower.contains("x-openai-subagent: spark_worker_slug") {
            return if has_tool_output {
                fixture_text("slug task complete")
            } else {
                fixture_tool("slug-patch", "apply_patch", &json!({"patch": slug_patch()}))
            };
        }
        if lower.contains("x-openai-subagent: spark_worker_stats") {
            return if has_tool_output {
                fixture_text("stats task complete")
            } else {
                fixture_tool("stats-patch", "apply_patch", &json!({"patch": stats_patch()}))
            };
        }
        if lower.contains("x-openai-subagent: spark_completion_judge") {
            return fixture_text(
                "<minha-judge>{\"schema_version\":1,\"verdict\":\"verified\",\"summary\":\"Both independent implementations are present.\",\"evidence\":[\"fixture checks passed\"],\"findings\":[]}</minha-judge>",
            );
        }
        if lower.contains("x-openai-subagent: integrator_lead") {
            return fixture_text("Integrated the two disjoint worker patches; tests are ready to run.");
        }
        fixture_text("fixture completed")
    }

    fn fixture_text(text: &str) -> String {
        let delta = json!({"type":"response.output_text.delta", "delta":text});
        let completed = json!({"type":"response.completed", "response":{
            "id":"fixture-response", "model":"fixture-model",
            "usage":{"input_tokens":20,"output_tokens":10}, "output":[]
        }});
        format!("data: {delta}\n\ndata: {completed}\n\n")
    }

    fn fixture_tool(call_id: &str, name: &str, arguments: &Value) -> String {
        let item = json!({
            "type":"function_call", "call_id":call_id, "name":name,
            "arguments":arguments.to_string()
        });
        let output = json!({"type":"response.output_item.done", "item":item});
        let completed = json!({"type":"response.completed", "response":{
            "id":"fixture-response", "model":"fixture-model",
            "usage":{"input_tokens":20,"output_tokens":10}, "output":[item]
        }});
        format!("data: {output}\n\ndata: {completed}\n\n")
    }

    fn slug_patch() -> &'static str {
        "diff --git a/src/slug.rs b/src/slug.rs\n--- a/src/slug.rs\n+++ b/src/slug.rs\n@@ -1,4 +1,17 @@\n /// Convert a title to a compact ASCII slug.\n-pub fn slugify(_input: &str) -> String {\n-    String::new()\n+pub fn slugify(input: &str) -> String {\n+    let mut output = String::new();\n+    let mut separator = false;\n+    for character in input.chars() {\n+        if character.is_ascii_alphanumeric() {\n+            if separator && !output.is_empty() {\n+                output.push('-');\n+            }\n+            output.push(character.to_ascii_lowercase());\n+            separator = false;\n+        } else {\n+            separator = true;\n+        }\n+    }\n+    output\n }\n"
    }

    fn stats_patch() -> &'static str {
        "diff --git a/src/stats.rs b/src/stats.rs\n--- a/src/stats.rs\n+++ b/src/stats.rs\n@@ -1,6 +1,13 @@\n use std::collections::BTreeMap;\n \n /// Count normalized non-empty words in deterministic key order.\n-pub fn word_counts(_input: &str) -> BTreeMap<String, usize> {\n-    BTreeMap::new()\n+pub fn word_counts(input: &str) -> BTreeMap<String, usize> {\n+    let mut counts = BTreeMap::new();\n+    for word in input\n+        .split(|character: char| !character.is_ascii_alphanumeric())\n+        .filter(|word| !word.is_empty())\n+    {\n+        *counts.entry(word.to_ascii_lowercase()).or_insert(0) += 1;\n+    }\n+    counts\n }\n"
    }

    fn fixture_git(root: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(root)
            .status()
            .expect("fixture git command should start");
        assert!(status.success(), "fixture git command failed: {arguments:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn implementation_hive_runs_disjoint_workers_integrates_and_judges() {
        let temp = tempfile::tempdir().expect("temporary fixture repository");
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).expect("fixture source directory");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"hive-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("fixture manifest");
        std::fs::write(root.join(".gitignore"), ".minha/\ntarget/\n").expect("fixture ignore file");
        std::fs::write(
            root.join("AGENTS.md"),
            "Keep slug and stats work independent. Add no dependencies. Run cargo test.\n",
        )
        .expect("fixture instructions");
        std::fs::write(root.join("src/lib.rs"), "pub mod slug;\npub mod stats;\n").expect("fixture library");
        std::fs::write(
            root.join("src/slug.rs"),
            "/// Convert a title to a compact ASCII slug.\npub fn slugify(_input: &str) -> String {\n    String::new()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::slugify;\n    #[test]\n    fn behavior() {\n        assert_eq!(slugify(\"  Fast, small & SAFE  \"), \"fast-small-safe\");\n        assert_eq!(slugify(\"already--slugged\"), \"already-slugged\");\n    }\n}\n",
        )
        .expect("fixture slug source");
        std::fs::write(
            root.join("src/stats.rs"),
            "use std::collections::BTreeMap;\n\n/// Count normalized non-empty words in deterministic key order.\npub fn word_counts(_input: &str) -> BTreeMap<String, usize> {\n    BTreeMap::new()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::word_counts;\n    #[test]\n    fn behavior() {\n        let counts = word_counts(\"Rust, rust! Fast; safe.\");\n        assert_eq!(counts.get(\"rust\"), Some(&2));\n        assert_eq!(counts.get(\"fast\"), Some(&1));\n        assert_eq!(counts.get(\"safe\"), Some(&1));\n        assert_eq!(counts.len(), 3);\n    }\n}\n",
        )
        .expect("fixture stats source");
        fixture_git(root, &["init", "--quiet"]);
        fixture_git(root, &["config", "user.name", "Minha Test"]);
        fixture_git(root, &["config", "user.email", "minha-test@invalid.example"]);
        fixture_git(root, &["config", "core.autocrlf", "false"]);
        fixture_git(root, &["add", "."]);
        fixture_git(root, &["commit", "--quiet", "-m", "fixture baseline"]);

        let server = HiveFixtureServer::start().expect("local provider fixture");
        let store = Store::open(root.join(".minha/test.sqlite3")).expect("fixture store");
        let workspace = store.ensure_workspace(root).expect("fixture workspace");
        let mut config = Config::default();
        config.books.enabled = false;
        config.cache.enabled = false;
        let client = ChatGptClient::new(&server.base_url, "fixture-token", "fixture-account");
        let harness = Harness {
            root: root.canonicalize().expect("canonical fixture root"),
            workspace_id: workspace.id,
            config,
            store,
            controls: Arc::new(Mutex::new(HashMap::new())),
            account_clients: Arc::new(Mutex::new(vec![client.into()])),
            hot_cache: Arc::new(Mutex::new(HotCache::with_limits(16, HOT_CACHE_MAX_BYTES))),
            model_context_limits: Arc::new(Mutex::new(HashMap::new())),
            deepseek_balance_percent: Arc::new(Mutex::new(None)),
        };

        let outcome = harness
            .run(
                RunKind::Implement,
                "Implement slugify and word_counts as two independent tasks, then verify both.",
            )
            .await
            .expect("fixture implementation run");
        let tasks = harness.store.tasks(outcome.run_id).expect("fixture tasks");
        assert_eq!(
            outcome.state,
            ExitState::Succeeded,
            "tasks: {tasks:?}; outcome: {outcome:?}"
        );
        assert!(outcome.agents_used >= 5, "outcome: {outcome:?}");
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks.iter().all(|task| task.state == PlanTaskState::Completed),
            "tasks: {tasks:?}; outcome: {outcome:?}"
        );

        let status = Command::new("cargo")
            .arg("test")
            .current_dir(root)
            .status()
            .expect("fixture cargo test should start");
        assert!(status.success());
        let labels = server.request_labels();
        assert!(labels.iter().any(|label| label == "spark_worker_slug"));
        assert!(labels.iter().any(|label| label == "spark_worker_stats"));
        assert!(labels.iter().any(|label| label == "integrator_lead"));
        assert!(labels.iter().any(|label| label == "spark_completion_judge"));
    }

    #[test]
    fn parses_tagged_branch_plan() {
        let plan = parse_plan(
            "notes\n<minha-plan>{\"summary\":\"x\",\"consult\":\"terra\",\"tasks\":[{\"id\":\"a\",\"objective\":\"b\",\"paths\":[\"src\"]}]}</minha-plan>",
        )
        .expect("test operation should succeed");
        assert_eq!(plan.tasks[0].id, "a");
        assert_eq!(
            consultation_route(&plan, &Config::default()).map(|route| route.0),
            Some("gpt-5.6-terra")
        );
    }

    #[test]
    fn parses_auto_mode_without_guessing_unknown_output() {
        assert_eq!(
            parse_auto_mode("route\n<minha-mode>implement</minha-mode>"),
            AutoMode::Implement
        );
        assert_eq!(parse_auto_mode("unstructured"), AutoMode::Chat);
    }

    #[test]
    fn local_routing_covers_clear_intents_without_a_model_turn() {
        for greeting in ["hello", "Hello!", "hi", "good morning", "thanks"] {
            assert_eq!(
                local_auto_mode(greeting),
                Some(AutoMode::Chat),
                "greeting {greeting:?} must never enter issue clarification"
            );
        }
        assert_eq!(
            local_auto_mode("Tell me about this codebase"),
            Some(AutoMode::Chat)
        );
        assert_eq!(
            local_auto_mode("continue working on the parser"),
            Some(AutoMode::Implement)
        );
        assert_eq!(local_auto_mode("plan a safe migration"), Some(AutoMode::Plan));
        assert_eq!(local_auto_mode("audit the cache"), Some(AutoMode::Audit));
        assert_eq!(local_auto_mode("review the diff"), Some(AutoMode::Review));
        assert_eq!(local_auto_mode("parser migration"), None);
    }

    #[test]
    fn components_are_branch_safe() {
        assert_eq!(safe_component("api/parser #1"), "api-parser--1");
        assert_eq!(safe_component("///"), "task");
    }

    #[test]
    fn validates_acyclic_plans_and_rejects_cycles() {
        let valid = BranchPlan {
            summary: "ordered".into(),
            consult: None,
            tasks: vec![
                BranchTask {
                    id: "inspect".into(),
                    objective: "inspect".into(),
                    paths: vec!["src/parser.rs".into()],
                    dependencies: Vec::new(),
                },
                BranchTask {
                    id: "change".into(),
                    objective: "change".into(),
                    paths: vec!["tests/parser.rs".into()],
                    dependencies: vec!["inspect".into()],
                },
            ],
        };
        assert!(validate_branch_plan(valid.clone()).is_ok());
        let mut cyclic = valid;
        cyclic.tasks[0].dependencies = vec!["change".into()];
        assert_eq!(
            validate_branch_plan(cyclic).unwrap_err(),
            "task dependencies contain a cycle"
        );
    }

    #[test]
    fn ready_batch_serializes_overlapping_paths() {
        let run_id = RunId::new();
        let now = chrono::Utc::now();
        let task = |id: &str, path: &str| TaskRecord {
            run_id,
            task_id: id.into(),
            objective: id.into(),
            state: PlanTaskState::Pending,
            paths: vec![path.into()],
            dependencies: Vec::new(),
            assigned_agent_id: None,
            attempt: 0,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let selected = disjoint_ready_tasks(
            vec![
                task("parent", "src"),
                task("child", "src/parser.rs"),
                task("docs", "docs"),
            ],
            8,
        );
        assert_eq!(
            selected
                .iter()
                .map(|task| task.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "docs"]
        );
    }

    #[test]
    fn compaction_tail_keeps_function_calls_with_outputs() {
        let call_a = json!({"type":"function_call","call_id":"a","name":"search","arguments":"{}"});
        let call_b = json!({"type":"function_call","call_id":"b","name":"read_files","arguments":"{}"});
        let input = vec![
            message("user", "old"),
            call_a.clone(),
            json!({"type":"function_call_output","call_id":"a","output":"old"}),
            message("assistant", "middle"),
            call_b.clone(),
            json!({"type":"function_call_output","call_id":"b","output":"new"}),
        ];
        let recent = paired_recent_items(&input, 1);
        assert_eq!(recent, vec![call_b, input[5].clone()]);
    }

    #[test]
    fn consumed_tool_outputs_become_bounded_typed_evidence() {
        let mut input = vec![json!({
            "type":"function_call_output", "call_id":"c1", "output":"x".repeat(10_000)
        })];
        condense_consumed_tool_outputs(&mut input);
        let output = input[0]["output"].as_str().expect("condensed output");
        assert!(output.len() < 3_000);
        assert!(output.contains("evidence_summary_version"));
        assert!(output.contains("sha256"));
    }

    #[test]
    fn spark_audits_have_hard_token_and_turn_caps() {
        assert_eq!(agent_turn_limit("Spark correctness auditor"), 5);
        assert_eq!(agent_input_budget("Spark correctness auditor"), 80_000);
        assert_eq!(agent_tool_budget("Spark correctness auditor"), 12);
        assert_eq!(agent_tool_budget("coordination manager"), 0);
        assert!(agent_turn_limit("Spark worker parser") > 5);
        assert!(role_can_ask_user("Spark worker parser"));
        assert!(!role_can_ask_user("Spark correctness auditor"));
    }

    #[test]
    fn deterministic_complexity_routes_only_within_available_models() {
        let config = Config::default();
        let available = HashSet::from([config.models.lead.clone(), config.models.complex_lead.clone()]);
        assert_eq!(
            routed_lead_model("fix one parser typo", &available, &config).expect("simple lead route"),
            config.models.lead
        );
        assert_eq!(
            routed_lead_model(
                "Redesign the distributed database schema, authentication security, concurrent protocol migration, and production release architecture",
                &available,
                &config,
            )
            .expect("complex lead route"),
            config.models.complex_lead
        );
    }

    #[test]
    fn deepseek_only_and_failure_escalation_routes_are_supported() {
        let config = Config::default();
        let available = HashSet::from([
            "deepseek/deepseek-v4-flash".to_owned(),
            "deepseek/deepseek-v4-pro".to_owned(),
        ]);
        assert_eq!(
            routed_lead_model("explain this parser", &available, &config).expect("DeepSeek lead"),
            "deepseek/deepseek-v4-pro"
        );
        let now = chrono::Utc::now();
        let mut task = TaskRecord {
            run_id: RunId::new(),
            task_id: "parser".into(),
            objective: "inspect the parser".into(),
            paths: vec!["src/parser.rs".into()],
            dependencies: Vec::new(),
            state: PlanTaskState::Pending,
            assigned_agent_id: None,
            attempt: 1,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(
            routed_worker_model(&task, &available, &config),
            "deepseek/deepseek-v4-flash"
        );
        task.attempt = 2;
        task.last_error = Some("first attempt failed".into());
        assert_eq!(
            routed_worker_model(&task, &available, &config),
            "deepseek/deepseek-v4-pro"
        );
        assert_eq!(
            reasoning_for_turn("deepseek/deepseek-v4-flash", "intent classifier", 0, "medium"),
            "max"
        );
        assert_eq!(
            reasoning_for_turn("deepseek/deepseek-v4-pro", "integrator", 1, "medium"),
            "max"
        );
    }

    #[test]
    fn remote_write_permissions_are_separate_from_local_destructive_work() {
        assert!(is_remote_write_argv(&["git", "push", "origin", "main"]));
        assert!(is_remote_write_argv(&[
            "curl",
            "--data",
            "{}",
            "https://example.test"
        ]));
        assert!(!is_remote_write_argv(&["git", "reset", "--hard", "HEAD"]));
        assert!(!is_remote_write_argv(&["curl", "https://example.test/status"]));
    }

    #[test]
    fn one_use_approval_is_bound_to_the_exact_exec_argv() {
        let temp = tempfile::tempdir().expect("approval workspace");
        let store = Store::in_memory().expect("approval store");
        let workspace = store
            .ensure_workspace(temp.path())
            .expect("approval workspace record");
        let run_id = RunId::new();
        let harness = Harness {
            root: temp.path().canonicalize().expect("approval root"),
            workspace_id: workspace.id,
            config: Config::default(),
            store,
            controls: Arc::new(Mutex::new(HashMap::from([(
                run_id,
                RunControl {
                    approved_exec_once: Some(vec![
                        "git".into(),
                        "push".into(),
                        "origin".into(),
                        "main".into(),
                    ]),
                    ..RunControl::default()
                },
            )]))),
            account_clients: Arc::new(Mutex::new(Vec::new())),
            hot_cache: Arc::new(Mutex::new(HotCache::with_limits(8, HOT_CACHE_MAX_BYTES))),
            model_context_limits: Arc::new(Mutex::new(HashMap::new())),
            deepseek_balance_percent: Arc::new(Mutex::new(None)),
        };
        assert!(!harness.take_exec_approval(run_id, &json!({"argv":["gh","release","create","v1.0.0"]})));
        assert!(!harness.take_exec_approval(run_id, &json!({"argv":["git","push","origin","main"]})));
        harness
            .controls
            .lock()
            .entry(run_id)
            .or_default()
            .approved_exec_once = Some(vec!["git".into(), "push".into(), "origin".into(), "main".into()]);
        assert!(harness.take_exec_approval(run_id, &json!({"argv":["git","push","origin","main"]})));
        assert!(!harness.take_exec_approval(run_id, &json!({"argv":["git","push","origin","main"]})));
    }

    #[test]
    fn refreshed_auth_preserves_rotating_fields() {
        let old = AuthRecord {
            access_token: "old".into(),
            refresh_token: Some("refresh".into()),
            id_token: Some("id".into()),
            account_id: Some("account".into()),
            email: None,
            expires_at_unix: None,
        };
        let refreshed = AuthRecord {
            access_token: "new".into(),
            refresh_token: Some(String::new()),
            id_token: Some(String::new()),
            account_id: None,
            email: None,
            expires_at_unix: None,
        };
        let merged = merge_refreshed_auth(old, refreshed);
        assert_eq!(merged.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(merged.account_id.as_deref(), Some("account"));
    }

    #[test]
    fn stable_prompt_has_a_bounded_instruction_budget() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        std::fs::write(temp.path().join("AGENTS.md"), "rule\n".repeat(30_000))
            .expect("test operation should succeed");
        let store = Store::in_memory().expect("test operation should succeed");
        let workspace = store
            .ensure_workspace(temp.path())
            .expect("test operation should succeed");
        let harness = Harness {
            root: temp.path().canonicalize().expect("test operation should succeed"),
            workspace_id: workspace.id,
            config: Config::default(),
            store,
            controls: Arc::new(Mutex::new(HashMap::new())),
            account_clients: Arc::new(Mutex::new(Vec::new())),
            hot_cache: Arc::new(Mutex::new(HotCache::with_limits(128, HOT_CACHE_MAX_BYTES))),
            model_context_limits: Arc::new(Mutex::new(HashMap::new())),
            deepseek_balance_percent: Arc::new(Mutex::new(None)),
        };
        let prompt = harness
            .system_prompt("fix the parser", "Spark worker parser", false)
            .expect("test operation should succeed");
        assert!(
            estimate_tokens(&prompt) < 18_000,
            "prompt estimate was {}",
            estimate_tokens(&prompt)
        );

        let clarification_prompt = harness
            .system_prompt("it doesn't work", "issue clarifier Luna lead", true)
            .expect("clarification prompt");
        assert!(
            estimate_tokens(&clarification_prompt) < 2_000,
            "clarification prompt estimate was {}",
            estimate_tokens(&clarification_prompt)
        );
        assert!(clarification_prompt.contains("Repository instruction files are available"));
        assert!(!clarification_prompt.contains("rule\nrule\nrule"));
    }
}
