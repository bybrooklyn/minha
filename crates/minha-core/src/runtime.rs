//! Token-conscious orchestration over direct ChatGPT Codex model turns.

use crate::{
    Config, Store,
    auth::{
        AuthError, AuthRecord, CodexOAuthClient, active_account_profile, enabled_account_records,
        load_account_profile, load_default_auth, openai_oauth_config, save_account_profile,
        save_default_auth,
    },
    cache::{
        CacheClass, CacheEntry, CachePolicy, HotCache, LookupMode, ObservedInputManifest, cache_key,
        contains_secret,
    },
    clarify::{
        analyze as analyze_issue, apply_answers as apply_clarification_answers, confirm as confirm_issue,
        exhaust_rounds, make_fallback_batch, needs_clarification, prepare_brief, render_brief,
        reopen as reopen_issue, sanitize_model_batch, should_consult_terra,
    },
    context::{ContextPolicy, estimate_tokens},
    deepseek::DeepSeekClient,
    executor::{
        CoordinationContext, ExecutorPolicy, InputRequest, ToolError, ToolExecutor, ToolOutcome,
        tool_definitions,
    },
    facts::{BoardEntry, BoardKind},
    fairness::{
        FairnessCandidateV1, FairnessSelectionV1, ProviderHealthStatusV1, ProviderHealthV1,
        normalized_token_work,
    },
    instructions::{
        AgentDefinition, Skill, discover_agents, discover_instructions, discover_skills, load_skill,
    },
    memory::{MemoryRecord, MemoryScope},
    mimo::MiMoClient,
    protocol::{
        AgentState, BoardEntryView, CatalogModel, ClarificationStatus, DISPATCH_RECEIPT_SCHEMA_VERSION,
        DISPATCH_RECEIPT_V2_SCHEMA_VERSION, DispatchReceiptV1, DispatchReceiptV2, DispatchRoutingV1,
        EventAgentId, ExitState, IncidentSeverity, IncidentView, IssueClarificationView, ItemId,
        MICROTASK_CONTRACT_SCHEMA_VERSION, MicrotaskContractV1, Mode, PlanTask, PlanTaskState, RequestId,
        RoutingCandidateV1, RoutingCandidateV2, RunId, RunPhase, RuntimeEvent, TerminationReason, TodoItem,
        TodoState,
    },
    provider::{
        ChatGptClient, DEEPSEEK_BASE_URL, ModelCatalog, ModelDescriptor, ModelRef, ProviderBalanceV1,
        ProviderError, ProviderId, ProviderStreamEvent, ToolCall, TurnRequest, TurnResult,
    },
    provider_credentials::{default_path as provider_credentials_path, load_deepseek_key, load_xiaomi_mimo},
    store::{AgentRecord, TaskRecord},
    usage::{
        TokenUsage, USAGE_LEDGER_SCHEMA_VERSION, UsageKindV1, UsageLedgerEntryV1, UsageStateV1,
        reserve_reached,
    },
    worktree::{GitError, GitRepo, copy_workspace, diff_snapshots},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;

pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const MAX_PLAN_TASKS: usize = 8;
/// A session keeps the final five percent of its configured target untouched
/// for recovery.  The preceding band is a warning/admission pressure signal,
/// not a second hard terminal budget.
const ADAPTIVE_TAPER_PERCENT: u64 = 90;
const ADAPTIVE_PAUSE_PERCENT: u64 = 95;
/// Upper bound the executor clamps `exec` to, stated in the system prompt so a
/// role does not spend tool calls rediscovering it. Mirrors
/// `ToolExecutor::clamp_exec_timeout`.
const EXEC_TIMEOUT_HINT_SECONDS: u64 = 120;
const HOT_CACHE_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
enum RuntimeProviderClient {
    ChatGpt(ChatGptClient),
    DeepSeek(DeepSeekClient),
    XiaomiMiMo(MiMoClient),
}

/// Whether a catalog result proves live provider recovery. Static capability
/// tables and cached catalogs remain useful for model discovery, but they do
/// not erase a recorded cooldown, unsupported model, or auth failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogProvenance {
    Live,
    Cached,
    StaticFallback,
}

impl CatalogProvenance {
    const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Cached => "cached",
            Self::StaticFallback => "static fallback",
        }
    }
}

impl RuntimeProviderClient {
    const fn provider_id(&self) -> ProviderId {
        match self {
            Self::ChatGpt(_) => ProviderId::ChatGptCodex,
            Self::DeepSeek(_) => ProviderId::DeepSeek,
            Self::XiaomiMiMo(_) => ProviderId::XiaomiMiMo,
        }
    }

    async fn fetch_models(&self, etag: Option<&str>) -> Result<ModelCatalog, ProviderError> {
        match self {
            Self::ChatGpt(client) => client.fetch_models(etag).await,
            Self::DeepSeek(client) => client.fetch_models().await,
            Self::XiaomiMiMo(client) => client.fetch_models().await,
        }
    }

    async fn fetch_balance(&self) -> Option<Result<ProviderBalanceV1, ProviderError>> {
        match self {
            Self::ChatGpt(_) => None,
            Self::DeepSeek(client) => Some(client.fetch_balance().await),
            // MiMo documents authentication and pricing but not a
            // machine-readable remaining-quota endpoint.  Unknown is kept
            // distinct from zero so reserve routing never fabricates a limit.
            Self::XiaomiMiMo(_) => None,
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
            Self::XiaomiMiMo(client) => client.turn(request).await,
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
            Self::XiaomiMiMo(client) => client.turn_stream(request, on_event).await,
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
        .or_else(|| model.strip_prefix("xiaomi/"))
        .unwrap_or(model)
}

fn provider_for_model(model: &str) -> ProviderId {
    // ChatGPT catalog slugs are stored unqualified and enumerated dynamically,
    // so there is no prefix to match and no closed list to check; that single
    // legacy assumption lives in `parse_or_legacy_chatgpt`.
    ModelRef::parse_or_legacy_chatgpt(model).provider
}

/// Store a model exactly once in a provider-qualified form for routing. The
/// ChatGPT catalog also exposes legacy bare slugs for compatibility, but those
/// aliases must not receive separate WDRR credit.
fn canonical_routing_model(model: &str) -> String {
    let provider = provider_for_model(model);
    let slug = provider_model_slug(model);
    match provider {
        ProviderId::ChatGptCodex => format!("chatgpt/{slug}"),
        ProviderId::DeepSeek => format!("deepseek/{slug}"),
        ProviderId::XiaomiMiMo => format!("xiaomi/{slug}"),
    }
}

fn usage_entry_key(
    run_id: RunId,
    agent_id: Option<EventAgentId>,
    turn: usize,
    kind: UsageKindV1,
    provider: ProviderId,
    provider_response_id: Option<&str>,
) -> String {
    if let Some(response_id) = provider_response_id.filter(|id| !id.trim().is_empty()) {
        return format!("provider-response:{}:{response_id}", provider.key());
    }
    // Some compatible providers do not return a response id. Model-agent turns
    // still have a durable intent identity: the run, agent, provider, and turn
    // number. Keep that key stable so recovery/replay cannot double-settle
    // usage or fair work. Compaction has no durable per-attempt turn number,
    // so it keeps a distinct local UUID to allow multiple real compactions.
    if agent_id.is_some() || kind == UsageKindV1::ModelTurn {
        return format!(
            "local:{}:{}:{}:{}:{}",
            run_id,
            agent_id.map_or_else(|| "none".into(), |id| id.to_string()),
            kind.as_str(),
            turn,
            provider.key(),
        );
    }
    format!(
        "local:{}:{}:{}:{}:{}:{}",
        run_id,
        agent_id.map_or_else(|| "none".into(), |id| id.to_string()),
        kind.as_str(),
        turn,
        provider.key(),
        uuid::Uuid::now_v7()
    )
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
    /// Exact compact operation identity accepted by one human approval. This
    /// covers `exec`, opaque Just recipes, and persistent-terminal batches.
    approved_operation_once: Option<Vec<String>>,
    force_compaction: bool,
    bypass_cache: bool,
    budget_tokens: u64,
    /// Set while a run is paused at the integration approval gate
    /// (`scheduler.integration_approval`). Holds everything needed to
    /// either proceed straight into the integrator prompt or report the
    /// isolated worktrees on decline, without redoing planning or worker
    /// dispatch on resume.
    integration_pending: Option<IntegrationApprovalContext>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BudgetPressure {
    Normal,
    Tapered,
    Paused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReserveAdmission {
    Unknown,
    Normal,
    Soft,
    Hard,
}

impl ReserveAdmission {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Normal => "normal",
            Self::Soft => "soft_reserved",
            Self::Hard => "hard_reserved",
        }
    }
}

/// Result of policy filtering plus one durable equal-weight WDRR admission.
/// The V2 receipt retains the full candidate evidence; V1 consumers receive a
/// compatibility projection after persistence.
#[derive(Clone, Debug)]
struct RoutedAutomaticSelection {
    model: String,
    candidates: Vec<RoutingCandidateV2>,
    fairness: FairnessSelectionV1,
    health: ProviderHealthStatusV1,
    user_pin: bool,
    reserve_override: Option<bool>,
    cooldown_override: Option<bool>,
}

const fn budget_pressure_name(pressure: BudgetPressure) -> &'static str {
    match pressure {
        BudgetPressure::Normal => "normal",
        BudgetPressure::Tapered => "tapered",
        BudgetPressure::Paused => "paused",
    }
}

/// Saved context for a paused integration approval decision. Captured when
/// `run_implementation` pauses before building the integrator prompt, and
/// consumed by `resume_with_answer` once the human approves or declines.
#[derive(Clone)]
struct IntegrationApprovalContext {
    goal: String,
    lead_model: String,
    consultation: String,
    reports: Vec<String>,
    usage: TokenUsage,
    orchestration_agents: usize,
    worktrees: Vec<PathBuf>,
}

/// One mutex per account profile serializes token refreshes: concurrent
/// refreshes of the same profile must never race, while different profiles
/// stay independent.
type RefreshLocks = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>;

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
    /// Remaining balance as a percentage of each provider's observed high-water
    /// mark. Keyed by provider so reserve protection is not tied to one vendor;
    /// a provider absent from the map has no known balance, which is treated as
    /// `unknown` rather than as either exhausted or unlimited.
    provider_balance_percent: Arc<Mutex<HashMap<ProviderId, f64>>>,
    refresh_locks: RefreshLocks,
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
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
        self.fetch_model_catalog(&client, None)
            .await
            .map(|(models, _)| models)
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
        if run.state == ExitState::ApprovalRequired {
            return Err(HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "session is awaiting an approval answer; use answer rather than continue",
            )));
        }
        // A continuation is part of the same bounded session.  Reset only
        // one-shot controls; replacing the control record here used to reset
        // `budget_tokens`, allowing every Continue press to buy another full
        // Balanced budget and to create another visible Mina lead.
        self.reset_resume_control(run_id)?;
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
        self.recover_running_tasks(run_id)?;
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
            "Mina, session lead",
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
            control.approved_operation_once = if approved {
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
        if run.state == ExitState::NeedsInput {
            // Multiple workers can be blocked on the same question; every
            // blocked task holding the answered blocker is resumed.
            let blocked = self
                .store
                .tasks(run_id)?
                .into_iter()
                .filter(|task| task.state == PlanTaskState::Blocked)
                .collect::<Vec<_>>();
            if !blocked.is_empty() {
                let resume_context = format!("User answer to task blocker: {answer}");
                for task in &blocked {
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
                }
                let kind = stored_run_kind(&self.store.events(run_id)?).unwrap_or(RunKind::Implement);
                return self
                    .capture_failure(run_id, self.run_inner(run_id, kind, &run.goal))
                    .await;
            }
        }
        let integration_pending = self
            .controls
            .lock()
            .entry(run_id)
            .or_default()
            .integration_pending
            .take();
        if let Some(context) = integration_pending {
            if is_affirmative(answer) {
                let client = self.client().await?;
                let operation = self.integrate_and_judge(
                    run_id,
                    &context.goal,
                    client,
                    &context.lead_model,
                    &context.consultation,
                    &context.reports,
                    context.usage,
                    context.orchestration_agents,
                    context.worktrees,
                );
                return self.capture_failure(run_id, operation).await;
            }
            let recovery_note = self.integration_recovery_note(run_id);
            return self.finish_agent_outcome(
                run_id,
                RunKind::Implement,
                &context.lead_model,
                AgentResult {
                    text: format!(
                        "Integration was declined. The integrator agent did not run; branch work was not resolved or judged.\n\n{recovery_note}"
                    ),
                    question: None,
                    usage: context.usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::Blocked),
                },
                context.orchestration_agents,
                context.worktrees,
            );
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
            "Mina, resumed session lead",
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
            // Free-text action verbs from the CLI (`minha answer cancel`)
            // must act as actions instead of being bound to a question.
            match free_action_from_answers(answers) {
                Some("cancel") => clarification.status = ClarificationStatus::Cancelled,
                Some("confirm") => {
                    // `confirm` is a control verb, never an answer. Binding it
                    // would record the literal text "confirm" as the answer to
                    // whichever question happened to be pending and mark that
                    // dimension Confirmed with garbage detail. An explicit
                    // confirm while still collecting means "stop asking and
                    // proceed", so any still-unknown dimension is delegated to
                    // the safest supported assumption, exactly as it is when
                    // the clarification rounds run out.
                    if clarification.status == ClarificationStatus::Collecting {
                        exhaust_rounds(&mut clarification, &run.goal);
                    }
                    if clarification.brief.is_none() {
                        prepare_brief(&mut clarification, &run.goal);
                    }
                    confirm_issue(&mut clarification);
                }
                _ => {
                    apply_clarification_answers(&mut clarification, answers);
                    if clarification.status == ClarificationStatus::Reviewing && clarification.brief.is_none()
                    {
                        prepare_brief(&mut clarification, &run.goal);
                    }
                }
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
        // A retry is still the same billed session. Clear only transient
        // controls and restore its durable budget before re-admitting work.
        self.reset_resume_control(run_id)?;
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
                                message: format!(
                                    "{} balance is temporarily unavailable: {error}",
                                    provider_client.provider_id()
                                ),
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
            let provider_name = provider_client.provider_id().key();
            match self.fetch_model_catalog(provider_client, Some(run_id)).await {
                Ok((provider_models, provenance)) => {
                    let health =
                        self.record_provider_catalog_observation(provider_client.provider_id(), provenance)?;
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::ProviderState {
                            provider: provider_name.into(),
                            enabled: true,
                            healthy: match health.status {
                                ProviderHealthStatusV1::Healthy => Some(true),
                                ProviderHealthStatusV1::Unknown => None,
                                ProviderHealthStatusV1::CoolingDown
                                | ProviderHealthStatusV1::Unsupported
                                | ProviderHealthStatusV1::AuthenticationRequired => Some(false),
                            },
                            detail: format!(
                                "{} model(s) available from {}; provider health is {}",
                                provider_models.len(),
                                provenance.label(),
                                health.status.as_str(),
                            ),
                        },
                    )?;
                    models.extend(
                        provider_models
                            .into_iter()
                            .map(|model| (provider_client.provider_id(), model)),
                    );
                }
                Err(error) => {
                    if let HarnessError::Provider(provider_error) = &error {
                        self.record_provider_failure(provider_client.provider_id(), provider_error)?;
                    }
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
        let catalog_available = models
            .into_iter()
            .flat_map(|(provider, model)| {
                let slug = model.slug;
                let qualified = match provider {
                    ProviderId::ChatGptCodex => format!("chatgpt/{slug}"),
                    ProviderId::DeepSeek => format!("deepseek/{slug}"),
                    ProviderId::XiaomiMiMo => format!("xiaomi/{slug}"),
                };
                if provider == ProviderId::ChatGptCodex {
                    vec![slug, qualified]
                } else {
                    vec![qualified]
                }
            })
            .collect::<HashSet<_>>();
        // Leadership/planning retain the established reserve-filtered pool.
        // Automatic worker/audit routing also receives the observed catalog so
        // an explicit reserve override can be evaluated truthfully before WDRR.
        let available = self.apply_provider_reserves(catalog_available.clone());
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
        let lead_route = routed_lead_model(goal, &available, &self.config)?;
        if let Some(reason) = lead_route.degraded.as_deref() {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: reason.to_owned(),
                },
            )?;
        }
        let lead_model = lead_route.model;
        let planner_model = first_available(
            &available,
            &[
                &self.config.models.planner,
                &self.config.models.lead,
                &self.config.models.complex_lead,
            ],
        )
        .or_else(|error| deterministic_candidates(&available).first().copied().ok_or(error))?;
        self.recover_running_tasks(run_id)?;

        match kind {
            RunKind::Auto => {
                self.run_auto(
                    run_id,
                    goal,
                    client,
                    &available,
                    &catalog_available,
                    lead_model,
                    planner_model,
                )
                .await
            }
            RunKind::Plan => {
                self.run_single_with_client(run_id, kind, goal, planner_model, true, "Mina, planning", client)
                    .await
            }
            RunKind::Review => {
                let review_model =
                    first_available(&available, &[&self.config.models.worker_fast, lead_model]).or_else(
                        |error| deterministic_candidates(&available).first().copied().ok_or(error),
                    )?;
                self.run_single_with_client(run_id, kind, goal, review_model, true, "reviewer", client)
                    .await
            }
            RunKind::Audit => {
                self.run_audit(run_id, goal, client, &catalog_available, lead_model)
                    .await
            }
            RunKind::Implement => {
                self.run_implementation(
                    run_id,
                    goal,
                    client,
                    &available,
                    &catalog_available,
                    lead_model,
                    planner_model,
                )
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
        if clarification.round >= crate::clarify::MAX_CLARIFICATION_ROUNDS {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Warning {
                    message: format!(
                        "clarification rounds exhausted after {} rounds; remaining uncertainty is delegated",
                        clarification.round
                    ),
                },
            )?;
            exhaust_rounds(&mut clarification, goal);
            self.store.save_issue_clarification(run_id, &clarification)?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::ClarificationUpdated {
                    clarification: clarification.clone(),
                },
            )?;
            return self.clarification_outcome(run_id, kind, clarification, TokenUsage::default(), None, 0);
        }
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
                "Review this unresolved high-impact issue clarification. Identify at most three decisions that materially affect safety or scope. Do not ask the user directly and do not call tools. Return terse advice for the Mina clarifier.\n\nIssue: {}\nState: {}",
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
            let mut system = self.system_prompt(goal, "Mina, issue clarifier", true)?;
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
                    "Mina, issue clarifier",
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
                                "Mina clarification was unavailable; using local scoped questions: {error}"
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

    /// Observe model catalog provenance without treating a cached/static
    /// capability list as evidence that a failed provider has recovered.
    fn record_provider_catalog_observation(
        &self,
        provider: ProviderId,
        provenance: CatalogProvenance,
    ) -> Result<ProviderHealthV1, HarnessError> {
        match provenance {
            CatalogProvenance::Live => Ok(self
                .store
                .record_provider_catalog_success(&self.workspace_id, provider.key())?),
            CatalogProvenance::Cached | CatalogProvenance::StaticFallback => {
                Ok(self.store.provider_health(&self.workspace_id, provider.key())?)
            }
        }
    }

    async fn fetch_model_catalog(
        &self,
        client: &RuntimeProviderClient,
        run_id: Option<RunId>,
    ) -> Result<(Vec<ModelDescriptor>, CatalogProvenance), HarnessError> {
        const FRESH_MINUTES: i64 = 15;
        const STALE_FALLBACK_HOURS: i64 = 24;
        if client.provider_id() != ProviderId::ChatGptCodex {
            let catalog = client.fetch_models(None).await?;
            self.emit_model_catalog(
                run_id,
                client.provider_id(),
                &catalog.models,
                chrono::Utc::now(),
                false,
            )?;
            return Ok((catalog.models, CatalogProvenance::StaticFallback));
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
            return Ok((cached.models.clone(), CatalogProvenance::Cached));
        }

        match client
            .fetch_models(cached.as_ref().and_then(|catalog| catalog.etag.as_deref()))
            .await
        {
            Ok(catalog) if catalog.not_modified => {
                let cached = cached.ok_or(HarnessError::Provider(ProviderError::InvalidResponse(
                    "provider returned not-modified without a local catalog".into(),
                )))?;
                self.store.touch_model_catalog(&self.workspace_id)?;
                client.install_model_catalog(&cached.models);
                self.emit_model_catalog(run_id, ProviderId::ChatGptCodex, &cached.models, now, true)?;
                Ok((cached.models, CatalogProvenance::Live))
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
                Ok((saved.models, CatalogProvenance::Live))
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
                    Ok((cached.models, CatalogProvenance::Cached))
                } else {
                    Err(error.into())
                }
            }
        }
    }

    /// Withdraw models from providers that have reached their configured
    /// balance reserve. A provider under its hard reserve is removed outright; a
    /// provider under its soft reserve is used only when no unreserved provider
    /// can serve the request. Providers with no known balance are left alone,
    /// because missing telemetry means `unknown`, not `exhausted`.
    fn apply_provider_reserves(&self, available: HashSet<String>) -> HashSet<String> {
        let balances = self.provider_balance_percent.lock().clone();
        let mut hard_reserved = Vec::new();
        let mut soft_reserved = Vec::new();
        for provider in ProviderId::all() {
            let Some(percent) = balances.get(&provider).copied() else {
                continue;
            };
            let policy = self.config.budgets.reserve_for(provider);
            if percent <= f64::from(policy.hard_percent) {
                hard_reserved.push(provider);
            } else if percent <= f64::from(policy.soft_percent) {
                soft_reserved.push(provider);
            }
        }
        let mut available = available;
        if !hard_reserved.is_empty() {
            available.retain(|model| !hard_reserved.contains(&provider_for_model(model)));
        }
        if !soft_reserved.is_empty() {
            let unreserved = available
                .iter()
                .filter(|model| !soft_reserved.contains(&provider_for_model(model)))
                .cloned()
                .collect::<HashSet<_>>();
            if !unreserved.is_empty() {
                available = unreserved;
            }
        }
        available
    }

    fn provider_reserve_admission(&self, provider: ProviderId) -> ReserveAdmission {
        let override_policy = self.config.routing.provider_override(provider);
        match override_policy.reserve {
            Some(true) => return ReserveAdmission::Hard,
            Some(false) => return ReserveAdmission::Normal,
            None => {}
        }
        let Some(percent) = self.provider_balance_percent.lock().get(&provider).copied() else {
            return ReserveAdmission::Unknown;
        };
        let policy = self.config.budgets.reserve_for(provider);
        if percent <= f64::from(policy.hard_percent) {
            ReserveAdmission::Hard
        } else if percent <= f64::from(policy.soft_percent) {
            ReserveAdmission::Soft
        } else {
            ReserveAdmission::Normal
        }
    }

    /// Route an automatic worker/audit dispatch after capability, explicit
    /// policy, health, and reserve filtering. Equal-weight WDRR is deliberately
    /// the final choice; this code never consults provider price estimates.
    #[allow(clippy::too_many_arguments)]
    fn select_automatic_route(
        &self,
        fairness_role: &str,
        run_id: RunId,
        agent_id: EventAgentId,
        receipt_id: &str,
        candidate_models: impl IntoIterator<Item = String>,
        estimated_input_tokens: u64,
    ) -> Result<RoutedAutomaticSelection, HarnessError> {
        let pinned_model = self
            .config
            .routing
            .pin_for_role(fairness_role)
            .map(canonical_routing_model);
        let mut models = candidate_models
            .into_iter()
            .map(|model| canonical_routing_model(&model))
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        let now = chrono::Utc::now();
        let mut candidates = Vec::with_capacity(models.len());
        let mut soft_candidates = Vec::new();
        for model in models {
            let provider = provider_for_model(&model);
            let provider_name = provider.key().to_owned();
            let provider_override = self.config.routing.provider_override(provider);
            let health = self.store.provider_health(&self.workspace_id, &provider_name)?;
            let reserve = self.provider_reserve_admission(provider);
            let pinned = pinned_model.as_deref() == Some(model.as_str());
            let mut eligible = true;
            let mut reason = if let Some(exclusion) = worker_model_policy_exclusion(&model) {
                eligible = false;
                exclusion.into()
            } else if pinned_model.is_some() && !pinned {
                eligible = false;
                "excluded by explicit user model pin".into()
            } else if matches!(
                health.status,
                ProviderHealthStatusV1::Unsupported | ProviderHealthStatusV1::AuthenticationRequired
            ) {
                eligible = false;
                match health.status {
                    ProviderHealthStatusV1::Unsupported => {
                        "provider unsupported until a successful catalog refresh".into()
                    }
                    ProviderHealthStatusV1::AuthenticationRequired => {
                        "provider authentication needs remediation".into()
                    }
                    _ => unreachable!("matched above"),
                }
            } else if provider_override.cooldown == Some(true) {
                eligible = false;
                "excluded by explicit user cooldown override".into()
            } else if health.cooldown_active_at(now) && provider_override.cooldown != Some(false) {
                eligible = false;
                "provider cooldown is active".into()
            } else if reserve == ReserveAdmission::Hard {
                eligible = false;
                "provider reserve excludes this route".into()
            } else {
                let mut reason = if health.status == ProviderHealthStatusV1::Unknown {
                    "eligible; provider telemetry is unknown".to_owned()
                } else {
                    "eligible by capability and provider health".to_owned()
                };
                if provider_override.cooldown == Some(false) && health.cooldown_active_at(now) {
                    reason.push_str("; explicit user cooldown bypass applied");
                }
                if provider_override.reserve == Some(false) {
                    reason.push_str("; explicit user reserve bypass applied");
                }
                reason
            };
            if eligible && reserve == ReserveAdmission::Soft {
                soft_candidates.push(candidates.len());
                reason.push_str("; soft provider reserve");
            }
            candidates.push(RoutingCandidateV2 {
                provider: provider_name,
                model,
                eligible,
                reason,
                health: health.status,
                cooldown_until: health.cooldown_until,
                reserve: reserve.as_str().into(),
                pinned,
            });
        }
        // Soft-reserved routes remain a truthful fallback only when no
        // non-reserved route can serve this role. Unknown telemetry is not
        // placed in either reserve bucket.
        let has_unreserved = candidates
            .iter()
            .enumerate()
            .any(|(index, candidate)| candidate.eligible && !soft_candidates.contains(&index));
        if has_unreserved {
            for index in soft_candidates {
                let candidate = &mut candidates[index];
                candidate.eligible = false;
                candidate.reason = "provider soft reserve; an unreserved eligible route exists".into();
            }
        }
        let eligible = candidates
            .iter()
            .filter(|candidate| candidate.eligible)
            .map(|candidate| FairnessCandidateV1::new(&candidate.provider, &candidate.model))
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            let pin_note = pinned_model
                .as_deref()
                .map(|pin| format!(" (pinned model `{pin}` is not eligible)"))
                .unwrap_or_default();
            return Err(HarnessError::ModelUnavailable(format!(
                "no eligible automatic {fairness_role} route{pin_note}"
            )));
        }
        let fairness = self.store.admit_equal_weight_route(
            &self.workspace_id,
            fairness_role,
            run_id,
            agent_id,
            receipt_id,
            &eligible,
            normalized_token_work(TokenUsage {
                input: estimated_input_tokens,
                ..TokenUsage::default()
            }),
        )?;
        let (selected_model, selected_health, user_pin) = candidates
            .iter()
            .find(|candidate| {
                candidate.provider == fairness.key.provider && candidate.model == fairness.key.model
            })
            .map(|candidate| (candidate.model.clone(), candidate.health, candidate.pinned))
            .ok_or_else(|| HarnessError::ModelUnavailable("fair route disappeared after admission".into()))?;
        let selected_override = self
            .config
            .routing
            .provider_override(provider_for_model(&selected_model));
        Ok(RoutedAutomaticSelection {
            model: selected_model,
            candidates,
            fairness,
            health: selected_health,
            user_pin,
            reserve_override: selected_override.reserve,
            cooldown_override: selected_override.cooldown,
        })
    }

    fn record_provider_failure(
        &self,
        provider: ProviderId,
        error: &ProviderError,
    ) -> Result<(), HarnessError> {
        let (status, retry_after) = match error {
            ProviderError::Http {
                status, retry_after, ..
            } if *status == reqwest::StatusCode::UNAUTHORIZED
                || *status == reqwest::StatusCode::FORBIDDEN =>
            {
                (Some(ProviderHealthStatusV1::AuthenticationRequired), None)
            }
            ProviderError::Http { status, .. }
                if *status == reqwest::StatusCode::NOT_FOUND
                    || *status == reqwest::StatusCode::METHOD_NOT_ALLOWED
                    || *status == reqwest::StatusCode::NOT_IMPLEMENTED =>
            {
                (Some(ProviderHealthStatusV1::Unsupported), None)
            }
            ProviderError::Http { retry_after, .. } => (None, *retry_after),
            ProviderError::Request(_)
            | ProviderError::IncompleteStream
            | ProviderError::InvalidResponse(_)
            | ProviderError::Json(_)
            | ProviderError::Sse(_)
            | ProviderError::RemoteError(_) => (None, None),
            ProviderError::Header => (Some(ProviderHealthStatusV1::AuthenticationRequired), None),
        };
        if let Some(status) = status {
            self.store.record_provider_remediation_needed(
                &self.workspace_id,
                provider.key(),
                status,
                &error.to_string(),
            )?;
        } else {
            self.store.record_provider_transient_failure(
                &self.workspace_id,
                provider.key(),
                retry_after,
                &error.to_string(),
            )?;
        }
        Ok(())
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
        let current = usd
            .total
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0);
        let provider = balance.provider;
        let reserve_percent = current
            .map(|current| {
                self.store
                    .update_provider_balance_high_water(
                        &self.workspace_id,
                        provider.key(),
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
        // Only this provider's entry moves; an unreadable balance leaves the
        // previous reading for other providers untouched.
        match reserve_percent {
            Some(percent) => {
                self.provider_balance_percent.lock().insert(provider, percent);
            }
            None => {
                self.provider_balance_percent.lock().remove(&provider);
            }
        }
        self.store.record_runtime_event(
            run_id,
            RuntimeEvent::ProviderBalance {
                provider: provider.key().into(),
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
                        ProviderId::XiaomiMiMo => "xiaomi",
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
                                    ProviderId::XiaomiMiMo => "xiaomi_mimo",
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

    #[allow(clippy::too_many_arguments)]
    async fn run_auto(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        catalog_available: &HashSet<String>,
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
                provider: provider_for_model(routing_model.as_deref().unwrap_or(lead_model))
                    .key()
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
                    "Mina, conversation lead",
                    client,
                )
                .await;
        }
        let mut outcome = match selected {
            AutoMode::Implement => {
                self.run_implementation(
                    run_id,
                    goal,
                    client,
                    available,
                    catalog_available,
                    lead_model,
                    planner_model,
                )
                .await?
            }
            AutoMode::Plan => {
                self.run_single_with_client(
                    run_id,
                    RunKind::Plan,
                    goal,
                    planner_model,
                    true,
                    "Mina, planning",
                    client,
                )
                .await?
            }
            AutoMode::Audit => {
                self.run_audit(run_id, goal, client, catalog_available, lead_model)
                    .await?
            }
            AutoMode::Review => {
                let review_model = first_available(available, &[&self.config.models.worker_fast, lead_model])
                    .or_else(|error| deterministic_candidates(available).first().copied().ok_or(error))?;
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
                    "chat route escaped its terminal branch".into(),
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
        if self.session_budget_exhausted(run_id)? {
            let result = AgentResult {
                text: format!(
                    "The session token budget is exhausted; no new {role} turn was dispatched. Start a new session or change the execution profile."
                ),
                question: None,
                usage: TokenUsage::default(),
                paused: true,
                reserve_reached: false,
                termination: Some(TerminationReason::BudgetTarget),
            };
            return self.finish_agent_outcome(run_id, kind, model, result, 0, Vec::new());
        }
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
        catalog_available: &HashSet<String>,
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
        let count = lenses
            .len()
            .min(
                self.config
                    .scheduler
                    .max_agents
                    .min(self.config.scheduler.hard_max_agents)
                    .max(1),
            )
            .min(if self.budget_pressure(run_id)? == BudgetPressure::Tapered {
                1
            } else {
                usize::MAX
            });
        let futures = FuturesUnordered::new();
        let mut audit_models = Vec::new();
        let mut admitted_agents = Vec::new();
        for (slot, (lens, directive)) in lenses.into_iter().take(count).enumerate() {
            let agent_id = EventAgentId::new();
            let task_id = format!("audit-{lens}");
            let receipt_id = dispatch_receipt_id(run_id, &task_id, 0, agent_id);
            let estimated_input_tokens = estimate_tokens(&format!("{goal}\n{directive}")) as u64;
            let routed = match self.select_automatic_route(
                "audit",
                run_id,
                agent_id,
                &receipt_id,
                fair_audit_models(catalog_available, &self.config),
                estimated_input_tokens,
            ) {
                Ok(routed) => routed,
                Err(error) => {
                    for admitted_agent in admitted_agents {
                        let _ = self.store.cancel_fair_route_admission(run_id, admitted_agent);
                    }
                    return Err(error);
                }
            };
            let model = routed.model.clone();
            let role = format!("{} {lens} auditor", model_identity(&model));
            if let Err(error) = self.record_audit_dispatch_receipt(
                run_id,
                &task_id,
                agent_id,
                &role,
                &routed,
                estimated_input_tokens,
            ) {
                let _ = self.store.cancel_fair_route_admission(run_id, agent_id);
                for admitted_agent in admitted_agents {
                    let _ = self.store.cancel_fair_route_admission(run_id, admitted_agent);
                }
                return Err(error);
            }
            audit_models.push(model.clone());
            admitted_agents.push(agent_id);
            let harness = self.clone();
            let goal = goal.to_owned();
            let client = self.pooled_client(slot, &model, &client);
            futures.push(async move {
                let result = async {
                    let executor = ToolExecutor::new(&harness.root, true)?;
                    let mut system = harness.system_prompt(&goal, &role, true)?;
                    system.push_str("\nAudit lens: ");
                    system.push_str(directive);
                    system.push_str(
                        "\nReport only evidence-backed findings with path and line. If none, say none. Never edit.",
                    );
                    harness
                        .run_agent_as(
                            run_id,
                            &client,
                            &model,
                            &system,
                            &goal,
                            executor,
                            &role,
                            agent_id,
                        )
                        .await
                }
                .await;
                if result.is_err() {
                    let _ = harness.store.cancel_fair_route_admission(run_id, agent_id);
                }
                result
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
                audit_models.first().map(String::as_str).unwrap_or(lead_model),
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
                audit_models.first().map(String::as_str).unwrap_or(lead_model),
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
        let system = self.system_prompt(goal, "Mina, audit synthesis", true)?;
        let mut final_result = self
            .run_agent(
                run_id,
                &client,
                lead_model,
                &system,
                &synthesis,
                executor,
                "Mina, audit synthesis",
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

    #[allow(clippy::too_many_arguments)]
    async fn run_implementation(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        available: &HashSet<String>,
        catalog_available: &HashSet<String>,
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
            let mut planner_system = self.system_prompt(goal, "Mina, branch planning", true)?;
            planner_system.push_str(
                "\nInspect first. End with one <minha-plan> JSON object: {\"summary\":string,\"consult\":null|\"terra\"|\"sol\",\"tasks\":[{\"id\":string,\"objective\":string,\"paths\":[string],\"dependencies\":[string],\"check\":string}]}. This is a dispatch contract, not an invitation to summon agents. Return one focused task unless the repository evidence proves at least two genuinely independent, path-disjoint slices that will finish sooner in parallel. Every task needs a concrete acceptance check. Do not create tasks for planning, coordination, restating the goal, or a final review. Use consult=null normally, Terra only for a material cross-cutting uncertainty, and Sol only for independently demonstrated critical/high-risk work. Balanced permits at most four tasks; Turbo permits at most eight only when Turbo was explicitly selected. Extra tasks still require evidence of an independent speedup. Tasks must be testable slices; declare dependencies only when one slice needs another. Do not edit.",
            );
            let plan_result = self
                .run_agent(
                    run_id,
                    &client,
                    planner_model,
                    &planner_system,
                    goal,
                    planner,
                    "Mina, branch planning",
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
            let mut parsed = parse_plan(&plan_result.text).unwrap_or_else(|| single_task_plan(goal));
            parsed.tasks.truncate(
                self.config
                    .budgets
                    .default
                    .policy()
                    .max_agents
                    .min(MAX_PLAN_TASKS),
            );
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
            let contracts = plan
                .tasks
                .iter()
                .map(microtask_contract_for_branch)
                .collect::<Vec<_>>();
            self.store.replace_task_contracts(run_id, &contracts)?;
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
            // Running→Pending recovery moved to `run_inner` so every entry
            // point (fresh runs, continue_session, resumptions) reschedules
            // tasks stranded by an interrupted process.
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
                            check: String::new(),
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
                "Goal: {goal}\n\nMina plan:\n{}\n\nInspect only the uncertainty or risk that justifies this consultation. Return concise, evidence-backed constraints and recommendations for workers and integrator. Do not restate the plan and never edit.",
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
                catalog_available,
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

        if self.config.scheduler.integration_approval {
            let scope = self.integration_approval_scope(run_id, &worktrees)?;
            let request_id = RequestId::new();
            let agent_id = EventAgentId::new();
            self.store.update_run_state(
                run_id,
                ExitState::ApprovalRequired,
                Some(lead_model),
                None,
                Some(&scope),
            )?;
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Approval {
                    request_id,
                    agent_id,
                    reason: scope.clone(),
                    command: None,
                },
            )?;
            self.controls
                .lock()
                .entry(run_id)
                .or_default()
                .integration_pending = Some(IntegrationApprovalContext {
                goal: goal.to_owned(),
                lead_model: lead_model.to_owned(),
                consultation: consultation.clone(),
                reports: reports.clone(),
                usage,
                orchestration_agents,
                worktrees: worktrees.clone(),
            });
            return self.finish_agent_outcome(
                run_id,
                RunKind::Implement,
                lead_model,
                AgentResult {
                    text: format!("Branch work is complete and waiting for integration approval.\n\n{scope}"),
                    question: Some(InputRequest {
                        question: scope,
                        options: vec!["approve".into(), "decline".into()],
                    }),
                    usage,
                    paused: false,
                    reserve_reached: false,
                    termination: Some(TerminationReason::Blocked),
                },
                orchestration_agents,
                worktrees,
            );
        }
        self.integrate_and_judge(
            run_id,
            goal,
            client,
            lead_model,
            &consultation,
            &reports,
            usage,
            orchestration_agents,
            worktrees,
        )
        .await
    }

    /// Gathers a lightweight scope summary for the integration approval
    /// gate: the task/path list from persisted tasks, the worktree count,
    /// and (if cheaply derivable from existing quality-check tool-output
    /// events) a pass/fail line. No new tracking machinery is added for the
    /// check-count line; it is simply omitted when there is nothing to
    /// derive it from.
    fn integration_approval_scope(
        &self,
        run_id: RunId,
        worktrees: &[PathBuf],
    ) -> Result<String, HarnessError> {
        let tasks = self.store.tasks(run_id)?;
        let mut lines = vec![format!(
            "Branch work finished: {} task{} across {} worktree{}.",
            tasks.len(),
            if tasks.len() == 1 { "" } else { "s" },
            worktrees.len(),
            if worktrees.len() == 1 { "" } else { "s" },
        )];
        for task in &tasks {
            let paths = if task.paths.is_empty() {
                "no declared paths".to_owned()
            } else {
                task.paths.join(", ")
            };
            lines.push(format!("- {}: {}", task.task_id, paths));
        }
        if let Some(checks) = self.quality_check_summary(run_id)? {
            lines.push(checks);
        }
        lines.push(self.integration_recovery_note(run_id));
        lines.push(
            "Approve to have Mina integrate now, or decline to stop here without running the integrator."
                .into(),
        );
        Ok(lines.join("\n"))
    }

    /// Describes where branch-work changes actually live by the time this
    /// gate runs. Worker lanes (the `worktrees` paths) are removed by
    /// `cleanup_worker_lane` as soon as each task's patch is captured, win
    /// or lose, so they never survive to this point — the durable record is
    /// the per-task recovery patch plus the working-tree changes already
    /// applied to the primary checkout (uncommitted).
    fn integration_recovery_note(&self, run_id: RunId) -> String {
        let recovery_dir = self.root.join(".minha/recovery").join(run_id.to_string());
        let mut patches = std::fs::read_dir(&recovery_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|extension| extension == "patch"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        patches.sort();
        if patches.is_empty() {
            format!(
                "Worker changes are already applied to {} as uncommitted working-tree changes; no recovery patches were recorded.",
                self.root.display()
            )
        } else {
            let list = patches
                .iter()
                .map(|path| format!("- {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "Worker changes are already applied to {} as uncommitted working-tree changes. Recovery patches:\n{list}",
                self.root.display()
            )
        }
    }

    /// Cheap pass/fail summary derived from already-recorded
    /// `RuntimeEvent::ToolOutput` events for `quality` tool calls. Returns
    /// `None` when no such events exist, rather than adding new tracking.
    fn quality_check_summary(&self, run_id: RunId) -> Result<Option<String>, HarnessError> {
        let mut passed = 0_usize;
        let mut failed = 0_usize;
        for envelope in self.store.events(run_id)? {
            if let RuntimeEvent::ToolOutput { name, exit_code, .. } = envelope.event
                && name == "quality"
            {
                match exit_code {
                    Some(0) => passed += 1,
                    Some(_) => failed += 1,
                    None => {}
                }
            }
        }
        if passed == 0 && failed == 0 {
            return Ok(None);
        }
        Ok(Some(format!("Quality checks: {passed} passed, {failed} failed.")))
    }

    /// Builds the integrator prompt from the branch results and runs the
    /// integrator agent through to judging. Shared by the direct path
    /// (`scheduler.integration_approval` off) and the resumed-approval path
    /// (`resume_with_answer`, after a human approves).
    #[allow(clippy::too_many_arguments)]
    async fn integrate_and_judge(
        &self,
        run_id: RunId,
        goal: &str,
        client: RuntimeProviderClient,
        lead_model: &str,
        consultation: &str,
        reports: &[String],
        usage: TokenUsage,
        orchestration_agents: usize,
        worktrees: Vec<PathBuf>,
    ) -> Result<RunOutcome, HarnessError> {
        let integrator_prompt = format!(
            "Original goal: {goal}\n\nRead-only consultation:\n{}\n\nBranch results:\n{}\n\nInspect the primary checkout and recovery patches. Resolve any conflicts, finish missing integration, and run sufficient checks. Do not commit, merge, push, or discard user changes.",
            if consultation.is_empty() {
                "none"
            } else {
                consultation
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
        let system = self.system_prompt(goal, "Mina, integrating", false)?;
        let mut result = self
            .run_agent(
                run_id,
                &client,
                lead_model,
                &system,
                &integrator_prompt,
                integrator,
                "Mina, integrating",
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
        let contract = microtask_contract_for_task(&task);
        self.store
            .replace_task_contracts(run_id, std::slice::from_ref(&contract))?;
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
        let system = self.system_prompt(goal, "Mina, direct task", false)?;
        let prompt = format!(
            "Microtask contract:\n- Goal: {}\n- Lease: {}\n- Acceptance check: {}\n\nInspect before editing, preserve unrelated work, run sufficient checks, and finish with a concise evidence-backed result. Delegate nothing unless new evidence proves an independent lane is necessary.",
            contract.goal,
            contract.lease_resources.join(", "),
            contract.acceptance_check,
        );
        self.record_dispatch_receipt(
            run_id,
            &task,
            &contract,
            agent_id,
            "Mina, direct task",
            lead_model,
            vec![RoutingCandidateV1 {
                provider: provider_for_model(lead_model).key().into(),
                model: lead_model.into(),
                eligible: true,
                reason: "selected focused lead lane".into(),
            }],
            estimate_tokens(&system).saturating_add(estimate_tokens(&prompt)) as u64,
            "one-agent default; delegation was not justified by task evidence",
            None,
        )?;
        let result = self
            .run_agent_as(
                run_id,
                &client,
                lead_model,
                &system,
                &prompt,
                executor,
                "Mina, direct task",
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
        catalog_available: &HashSet<String>,
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
            // Once a durable session crosses its adaptive target, finish only
            // one already-independent slice at a time. This protects enough
            // allowance for a compact recovery instead of multiplying the
            // remaining cost across a last batch of workers.
            let pressure_cap = if self.budget_pressure(run_id)? == BudgetPressure::Tapered {
                1
            } else {
                usize::MAX
            };
            let ready = disjoint_ready_tasks(
                ready,
                policy
                    .max_agents
                    .min(configured_max)
                    .min(self.config.scheduler.hard_max_agents)
                    .min(pressure_cap)
                    .max(1),
            );
            if ready.is_empty() {
                break;
            }

            let admitted_parallel = ready.len() > 1;
            let futures = FuturesUnordered::new();
            for (slot, task) in ready.into_iter().enumerate() {
                let attempt = task.attempt.saturating_add(1);
                let generation = task.generation;
                let agent_id = EventAgentId::new();
                let contract = self.task_contract_for_dispatch(run_id, &task)?;
                let resources = contract.lease_resources.clone();
                if let Err(crate::store::StoreError::LeaseConflict(_)) = self.store.acquire_task_leases(
                    run_id,
                    &task.task_id,
                    agent_id,
                    generation,
                    &resources,
                    chrono::Utc::now() + chrono::Duration::hours(2),
                ) {
                    // A ready task whose declared resources collide with
                    // another selected task is skipped instead of aborting
                    // the entire run; it stays Pending for the next batch.
                    continue;
                }
                let parallelism_reason = if admitted_parallel {
                    "admitted with path-disjoint ready work; concurrent completion has an independent speedup case"
                } else {
                    "one-agent default; no second independent ready slice was admitted"
                };
                let estimated_input_tokens = estimate_tokens(&format!(
                    "{}\n{}\n{}\n{}",
                    goal, task.objective, contract.acceptance_check, consultation
                )) as u64;
                let receipt_id = dispatch_receipt_id(run_id, &task.task_id, generation, agent_id);
                let routed = match self.select_automatic_route(
                    "worker",
                    run_id,
                    agent_id,
                    &receipt_id,
                    fair_worker_models(&task, catalog_available, &self.config),
                    estimated_input_tokens,
                ) {
                    Ok(routed) => routed,
                    Err(error) => {
                        let _ =
                            self.store
                                .release_task_leases(run_id, &task.task_id, agent_id, generation)?;
                        return Err(error);
                    }
                };
                let model = routed.model.clone();
                let role = format!("{} worker {}", model_identity(&model), task.task_id);
                let lane = match prepare_worker_lane(
                    &self.root,
                    &lane_base,
                    run_id,
                    &task,
                    attempt,
                    use_git_worktrees,
                ) {
                    Ok(lane) => lane,
                    Err(error) => {
                        let _ = self.store.cancel_fair_route_admission(run_id, agent_id)?;
                        let _ =
                            self.store
                                .release_task_leases(run_id, &task.task_id, agent_id, generation)?;
                        return Err(error);
                    }
                };
                if let Err(error) = self.record_dispatch_receipt(
                    run_id,
                    &task,
                    &contract,
                    agent_id,
                    &role,
                    &model,
                    routed.candidates.iter().cloned().map(Into::into).collect(),
                    estimated_input_tokens,
                    parallelism_reason,
                    Some(&routed),
                ) {
                    let _ = self.store.cancel_fair_route_admission(run_id, agent_id);
                    let _ = self
                        .store
                        .release_task_leases(run_id, &task.task_id, agent_id, generation);
                    cleanup_worker_lane(run_id, &task, lane, use_git_worktrees, &self.root);
                    return Err(error);
                }
                if !output.lanes.iter().any(|path| path == lane.path()) {
                    output.lanes.push(lane.path().to_owned());
                }
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
                let client = self.pooled_client(slot.saturating_add(attempt as usize), &model, client);
                let goal = goal.to_owned();
                let consultation = consultation.to_owned();
                futures.push(async move {
                    let result = async {
                        let executor = ToolExecutor::new(lane.path(), false)?;
                        let mut system = harness.system_prompt(&goal, &role, false)?;
                        system.push_str("\nWorker agent definition:\n");
                        system.push_str(include_str!("../../../bundled/agents/spark-worker.md"));
                        let prompt = format!(
                            "Shared goal: {goal}\n\nMicrotask contract:\n- Goal: {}\n- Lease: {}\n- Acceptance check: {}\n\nRead-only consultation: {}\nDependencies already integrated: {}\nPrior scheduler context: {}\nStay within this slice. Read the shared board only when it saves duplicate work; post only durable findings, blockers, artifacts, or decisions. Inspect, edit, and run the smallest sufficient checks. Do not commit, push, or claim global completion.",
                            contract.goal,
                            contract.lease_resources.join(", "),
                            contract.acceptance_check,
                            if consultation.is_empty() { "none" } else { &consultation },
                            if task.dependencies.is_empty() { "none".into() } else { task.dependencies.join(", ") },
                            task.last_error.as_deref().unwrap_or("none"),
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
                    if result.is_err() {
                        let _ = harness.store.cancel_fair_route_admission(run_id, agent_id);
                    }
                    (task, lane, agent_id, attempt, generation, result)
                });
            }

            let mut futures = futures;
            while let Some((task, lane, agent_id, attempt, generation, result)) = futures.next().await {
                // `run_agent_as` persists the agent only after durable budget
                // admission. Count from that authoritative record, not from
                // a scheduled future, so a denied preflight is neither a
                // ghost agent nor a misleading "agent used" total.
                if self
                    .store
                    .agents(run_id)?
                    .iter()
                    .any(|agent| agent.agent_id == agent_id)
                {
                    output.agents_used += 1;
                }
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
                            // A reserve pause is not a failure: reset the
                            // attempt so pauses never consume retry budget.
                            self.store.update_task(
                                run_id,
                                &task.task_id,
                                PlanTaskState::Pending,
                                None,
                                0,
                                generation.saturating_add(1),
                                Some("paused by account usage reserve"),
                            )?;
                            self.store.record_runtime_event(
                                run_id,
                                RuntimeEvent::PlanTaskChanged {
                                    task_id: task.task_id.clone(),
                                    state: PlanTaskState::Pending,
                                    agent_id: None,
                                },
                            )?;
                            // `run_agent_as` deliberately did not create an
                            // agent record after denied budget admission, so
                            // remove the scheduler's provisional TODO too.
                            // Leaving it would make the Operations panel
                            // describe ghost work as in progress.
                            self.store.clear_todos(run_id, agent_id)?;
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
                cleanup_worker_lane(run_id, &task, lane, use_git_worktrees, &self.root);
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
                "Mina must replan or finish these unresolved slices during integration: {}",
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

    fn task_contract_for_dispatch(
        &self,
        run_id: RunId,
        task: &TaskRecord,
    ) -> Result<MicrotaskContractV1, HarnessError> {
        if let Some(contract) = self.store.task_contract(run_id, &task.task_id)? {
            return Ok(contract);
        }
        // Runs created before the contract table remain resumable. Repair the
        // missing contract once, before a new lease is dispatched.
        let contract = microtask_contract_for_task(task);
        self.store.upsert_task_contract(run_id, &contract)?;
        Ok(contract)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_dispatch_receipt(
        &self,
        run_id: RunId,
        task: &TaskRecord,
        contract: &MicrotaskContractV1,
        agent_id: EventAgentId,
        role: &str,
        model: &str,
        candidates: Vec<RoutingCandidateV1>,
        estimated_input_tokens: u64,
        parallelism_reason: &str,
        routing: Option<&RoutedAutomaticSelection>,
    ) -> Result<(), HarnessError> {
        let usage = self.store.usage_totals(Some(run_id))?;
        let pressure = self.budget_pressure(run_id)?;
        let receipt = DispatchReceiptV1 {
            schema_version: DISPATCH_RECEIPT_SCHEMA_VERSION,
            receipt_id: dispatch_receipt_id(run_id, &task.task_id, task.generation, agent_id),
            task_id: task.task_id.clone(),
            generation: task.generation,
            agent_id,
            role: role.to_owned(),
            provider: provider_for_model(model).key().into(),
            model: model.to_owned(),
            candidates,
            lease_resources: contract.lease_resources.clone(),
            acceptance_check: contract.acceptance_check.clone(),
            estimated_input_tokens,
            session_used_tokens: usage.session_input.saturating_add(usage.session_output),
            session_target_tokens: self.config.budgets.default.soft_token_target(),
            budget_pressure: budget_pressure_name(pressure).into(),
            parallelism_reason: parallelism_reason.to_owned(),
            book_sources: Vec::new(),
            issued_at: chrono::Utc::now(),
        };
        let mut receipt = DispatchReceiptV2::from(receipt);
        if let Some(routing) = routing {
            receipt.model = routing.model.clone();
            receipt.provider = provider_for_model(&routing.model).key().into();
            receipt.candidates = routing.candidates.clone();
            let mut detail = DispatchRoutingV1::equal_weight(routing.fairness.clone());
            detail.user_pin = routing.user_pin;
            detail.reserve_override = routing.reserve_override;
            detail.cooldown_override = routing.cooldown_override;
            detail.health = routing.health;
            receipt.routing = detail;
        }
        if self.store.record_dispatch_receipt_v2(run_id, &receipt)? {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::DispatchReceipt {
                    receipt: receipt.to_v1(),
                },
            )?;
        }
        Ok(())
    }

    /// Audits do not have a scheduler task contract, but they are still
    /// automatic provider dispatches and retain the same durable routing
    /// evidence as worker lanes. This avoids manufacturing a mutable task
    /// merely to record a read-only lens.
    fn record_audit_dispatch_receipt(
        &self,
        run_id: RunId,
        task_id: &str,
        agent_id: EventAgentId,
        role: &str,
        routing: &RoutedAutomaticSelection,
        estimated_input_tokens: u64,
    ) -> Result<(), HarnessError> {
        let usage = self.store.usage_totals(Some(run_id))?;
        let mut detail = DispatchRoutingV1::equal_weight(routing.fairness.clone());
        detail.user_pin = routing.user_pin;
        detail.reserve_override = routing.reserve_override;
        detail.cooldown_override = routing.cooldown_override;
        detail.health = routing.health;
        let receipt = DispatchReceiptV2 {
            schema_version: DISPATCH_RECEIPT_V2_SCHEMA_VERSION,
            receipt_id: dispatch_receipt_id(run_id, task_id, 0, agent_id),
            task_id: task_id.to_owned(),
            generation: 0,
            agent_id,
            role: role.to_owned(),
            provider: provider_for_model(&routing.model).key().into(),
            model: routing.model.clone(),
            candidates: routing.candidates.clone(),
            lease_resources: vec!["read_only:workspace".into()],
            acceptance_check: "return evidence-backed audit findings".into(),
            estimated_input_tokens,
            session_used_tokens: usage.session_input.saturating_add(usage.session_output),
            session_target_tokens: self.config.budgets.default.soft_token_target(),
            budget_pressure: budget_pressure_name(self.budget_pressure(run_id)?).into(),
            parallelism_reason: "independent read-only audit lens admitted by equal-weight routing".into(),
            book_sources: Vec::new(),
            issued_at: chrono::Utc::now(),
            routing: detail,
        };
        if self.store.record_dispatch_receipt_v2(run_id, &receipt)? {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::DispatchReceipt {
                    receipt: receipt.to_v1(),
                },
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
        system.push_str("\nJudge agent definition:\n");
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
            let repair_system = self.system_prompt(goal, "Mina, repair cycle", false)?;
            let mut repair = self
                .run_agent(
                    run_id,
                    &client,
                    lead_model,
                    &repair_system,
                    &repair_prompt,
                    executor,
                    "Mina, repair cycle",
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
        let result = self
            .run_agent_as_inner(run_id, client, model, system, prompt, executor, role, agent_id)
            .await;
        // A fair route is admitted before this function starts. If no
        // canonical usage entry was ever settled (budget denial, interruption,
        // or provider failure), return its provisional estimate rather than
        // counting artificial work. After real usage this is a no-op.
        self.store.cancel_fair_route_admission(run_id, agent_id)?;
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_agent_as_inner(
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
        // Callers already select a client by slot; re-pooling with slot 0
        // would defeat account spread. Only re-pool when the caller's client
        // belongs to a different provider than the routed model.
        let selected_client = if client.provider_id() == provider_for_model(model) {
            client.clone()
        } else {
            self.pooled_client(0, model, client)
        };
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
        let mut system = system.to_owned();
        // Retrieval is prepared before admission so it can contribute to the
        // truthful budget estimate, but its event is emitted only once the
        // agent exists.  A denied preflight must not leave a ghost agent with
        // apparently consumed memory.
        let mut pending_memory_retrieval: Option<(Vec<String>, u64)> = None;
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
                pending_memory_retrieval = Some((
                    memories.iter().map(|hit| hit.memory.id.clone()).collect(),
                    estimate_tokens(&system) as u64,
                ));
            }
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
        let mut agent_started = false;

        for turn in 0..turn_limit {
            if self.is_interrupted(run_id) {
                if agent_started {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::AgentState {
                            agent_id,
                            state: AgentState::Cancelled,
                            detail: "interrupted".into(),
                        },
                    )?;
                }
                return Err(HarnessError::Interrupted);
            }
            if self.take_cooperative_pause(run_id) {
                if agent_started {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::AgentState {
                            agent_id,
                            state: AgentState::Waiting,
                            detail: "paused by user at a safe boundary".into(),
                        },
                    )?;
                }
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
            if !self.try_reserve_budget(run_id, reservation)? {
                self.store.record_runtime_event(
                    run_id,
                    RuntimeEvent::Warning {
                        message: format!(
                            "{role} stopped before a model call because the {:?} session token budget was reached",
                            self.config.budgets.default
                        ),
                    },
                )?;
                return Ok(AgentResult::budget_pause(
                    format!(
                        "Session token target reached before another {role} turn; durable progress is preserved."
                    ),
                    usage,
                ));
            }
            if !agent_started {
                // A provider turn has passed durable-budget admission. Only
                // now does the runtime make the agent visible or transition
                // its task, so a denied preflight cannot leave a ghost agent.
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
                        detail: "admitted for provider turn".into(),
                    },
                )?;
                if let Some((memory_ids, estimated_tokens)) = pending_memory_retrieval.take() {
                    self.store.record_runtime_event(
                        run_id,
                        RuntimeEvent::MemoryRetrieved {
                            agent_id,
                            memory_ids,
                            estimated_tokens,
                        },
                    )?;
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
                agent_started = true;
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
                            "{:?}|{model}|{role}|{}|tools-v3|prompt-v3",
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
                    self.store
                        .record_provider_turn_success(&self.workspace_id, client.provider_id().key())?;
                    result
                }
                Err(error) => {
                    self.settle_budget(run_id, reservation, 0);
                    self.record_provider_failure(client.provider_id(), &error)?;
                    return Err(error.into());
                }
            };
            let estimated_context = estimate_tokens(&system)
                + input
                    .iter()
                    .map(|item| estimate_tokens(&item.to_string()))
                    .sum::<usize>();
            let entry_key = usage_entry_key(
                run_id,
                Some(agent_id),
                turn,
                UsageKindV1::ModelTurn,
                client.provider_id(),
                result.response_id.as_deref(),
            );
            let fairness_entry_key = entry_key.clone();
            let recorded = self.store.record_usage_entry(&UsageLedgerEntryV1 {
                schema_version: USAGE_LEDGER_SCHEMA_VERSION,
                entry_key,
                run_id: run_id.to_string(),
                kind: UsageKindV1::ModelTurn,
                state: UsageStateV1::Settled,
                provider: client.provider_id().key().to_owned(),
                model: model.to_owned(),
                agent_id: Some(agent_id.to_string()),
                provider_response_id: result.response_id.clone(),
                usage: result.usage,
                context_tokens: Some(estimated_context as u64),
            })?;
            if recorded {
                self.store
                    .settle_fair_route_usage(run_id, agent_id, &fairness_entry_key, result.usage)?;
                usage = add_usage(usage, result.usage);
            }
            if !result.output_text.is_empty() {
                last_text = result.output_text.clone();
            }
            if recorded {
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
            }
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
                // Unparsable arguments are the model's mistake, or a truncated
                // tool-call stream, and are recoverable: report them back like
                // every other tool failure so the model can correct the call.
                // Propagating here used to abort the whole turn.
                let arguments: Value = match serde_json::from_str(&call.arguments) {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        let message = format!(
                            "tool arguments were not valid JSON: {error}; resend this call with a single well-formed JSON object"
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
                            json!({"error": message, "recoverable": true}).to_string(),
                        ));
                        continue;
                    }
                };
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
                    let approval_command = call_executor.approval_command(&call.name, &arguments)?;
                    if matches!(permission, crate::config::PermissionLevel::Allow)
                        || self.take_operation_approval(run_id, approval_command)
                    {
                        call_executor = call_executor.with_policy(ExecutorPolicy {
                            allow_destructive: true,
                        });
                    } else {
                        let request_id = RequestId::new();
                        let command = call_executor.approval_command(&call.name, &arguments)?;
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
        let mut outputs = HashMap::new();
        for call in calls.iter().cloned() {
            // One unparsable call must not abort the batch or the turn: report
            // it back as a recoverable failure and let its siblings run.
            let arguments: Value = match serde_json::from_str(&call.arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    let message = format!(
                        "tool arguments were not valid JSON: {error}; resend this call with a single well-formed JSON object"
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
                    outputs.insert(
                        call.call_id.clone(),
                        json!({"error": message, "recoverable": true}).to_string(),
                    );
                    continue;
                }
            };
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
        if !self.try_reserve_budget(run_id, reservation)? {
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
                    "{:?}|{}|compaction|{}|tools-v3|prompt-v3",
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
                "context compactor returned an empty checkpoint".into(),
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
        let entry_key = usage_entry_key(
            run_id,
            None,
            0,
            UsageKindV1::Compaction,
            compaction_provider,
            compacted.response_id.as_deref(),
        );
        let recorded = self.store.record_usage_entry(&UsageLedgerEntryV1 {
            schema_version: USAGE_LEDGER_SCHEMA_VERSION,
            entry_key,
            run_id: run_id.to_string(),
            kind: UsageKindV1::Compaction,
            state: UsageStateV1::Settled,
            provider: compaction_provider.key().to_owned(),
            model: compaction_model.clone(),
            agent_id: None,
            provider_response_id: compacted.response_id.clone(),
            usage: compacted.usage,
            context_tokens: Some(estimated as u64),
        })?;
        if recorded {
            *usage = add_usage(*usage, compacted.usage);
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::Usage {
                    agent_id: None,
                    model: compaction_model.clone(),
                    input_tokens: compacted.usage.input,
                    output_tokens: compacted.usage.output,
                    cached_input_tokens: compacted.usage.cached_input,
                    cache_write_tokens: compacted.usage.cache_write,
                    reasoning_output_tokens: compacted.usage.reasoning_output,
                },
            )?;
        }
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
        let policy = RolePolicy::for_role(role);
        // Only the issue clarifier runs on a stripped context. The Terra and Sol
        // risk consultants were caught by the old substring test and lost the
        // skills and agents they need to judge risk against how this repository
        // actually works.
        let clarifier = policy.kind == RoleKind::IssueClarifier;
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
            "You are {identity}, an agent in Minha, a fast token-conscious coding hivemind. Workspace: {}. Use only supplied fixed tools; no MCP. Work from evidence. {question_rule} Never push, merge, or spend account billing credits without the runtime presenting the exact operation and the user explicitly approving it. Keep tool output narrow: search before reading, request line ranges, and avoid repeating evidence already in context. Prefer one `quality` call over separate linter/test calls. Use structured read-only `github` queries before raw `gh`; remote GitHub mutations go through permission-gated `exec`. Use `hive` only for durable coordination, blockers, and content-addressed artifacts; use `books` lazily when a curated technical reference will save inspection or reasoning tokens.\n",
            self.root.display(),
            identity = identity_label(role),
        );
        if policy.tool_budget > 0 {
            prompt.push_str(&format!(
                "\nBudgets for this role: at most {} turns and {} tool calls. `exec` is killed at {} seconds, so prefer a narrower command over a long one.",
                policy.turn_limit, policy.tool_budget, EXEC_TIMEOUT_HINT_SECONDS
            ));
            if read_only {
                prompt.push_str(
                    " You are read-only, so `exec` accepts only inspection commands; a mutating command is refused rather than run, and retrying it wastes a tool call.",
                );
            }
            prompt.push('\n');
        }
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
            // `discover_instructions` returns nearest directories last, so the
            // last entry has the highest precedence. Claim the shared pool in
            // reverse, highest precedence first, so a broad root file can no
            // longer exhaust the budget and truncate the specific, more
            // authoritative file to nothing. Emission stays low to high so the
            // authoritative file is still what the model reads last.
            let mut remaining = 48 * 1024usize;
            let mut rendered = vec![None; instructions.len()];
            for (index, instruction) in instructions.iter().enumerate().rev() {
                if remaining == 0 {
                    break;
                }
                let content = bound(&instruction.content, remaining.min(24 * 1024));
                remaining = remaining.saturating_sub(content.len());
                rendered[index] = Some(content);
            }
            for (instruction, content) in instructions.iter().zip(rendered) {
                let Some(content) = content else {
                    continue;
                };
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
            // Purpose-written for a headless worker. The bundled caveman skill
            // used to be pasted here verbatim, but it is written for interactive
            // human invocation ("select with `$caveman lite`", "stop when the
            // user requests normal mode") and none of that applies to an agent
            // reporting to another agent.
            prompt.push_str(
                "\nInternal communication compression:\nYou report to other agents, not to a person. Keep every fact, path, line number, symbol, and command exactly as observed, and drop narration, hedging, restatement, and pleasantries. Structure each report as claim, then the evidence for it, then the next step.\n",
            );
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

    /// Refresh an expiring record under a per-profile lock. Concurrent runs
    /// that both see an expiring token serialize here, and the second caller
    /// re-reads the record the first one saved instead of racing the same
    /// refresh token and failing with invalid_grant.
    async fn refreshed_or_current(
        &self,
        profile_name: &str,
        auth: &AuthRecord,
    ) -> Result<Option<AuthRecord>, HarnessError> {
        Self::refreshed_or_current_with(
            &self.refresh_locks,
            profile_name,
            auth,
            || async {
                load_account_profile(profile_name)
                    .await
                    .map_err(HarnessError::from)
            },
            |refresh| async move {
                CodexOAuthClient::new(openai_oauth_config())
                    .map_err(HarnessError::from)?
                    .refresh(&refresh)
                    .await
                    .map_err(HarnessError::from)
            },
        )
        .await
    }

    /// The refresh contract, with the profile store and OAuth client
    /// injected so regression tests can exercise serialization and the
    /// re-read of the saved record without touching the real home dir.
    async fn refreshed_or_current_with<Stored, Refreshed, LoadFn, RefreshFn>(
        locks: &RefreshLocks,
        profile_name: &str,
        auth: &AuthRecord,
        load_stored: LoadFn,
        refresh: RefreshFn,
    ) -> Result<Option<AuthRecord>, HarnessError>
    where
        LoadFn: Fn() -> Stored,
        Stored: Future<Output = Result<Option<AuthRecord>, HarnessError>>,
        RefreshFn: FnOnce(String) -> Refreshed,
        Refreshed: Future<Output = Result<AuthRecord, HarnessError>>,
    {
        let now = chrono::Utc::now().timestamp();
        if !auth.expires_at_unix.is_some_and(|expiry| expiry <= now + 120) {
            return Ok(None);
        }
        let Some(refresh_token) = auth.refresh_token.clone() else {
            return Ok(None);
        };
        let lock = locks.lock().entry(profile_name.to_owned()).or_default().clone();
        let _guard = lock.lock().await;
        if let Some(current) = load_stored().await?
            && current.access_token != auth.access_token
        {
            return Ok(Some(current));
        }
        let refreshed = refresh(refresh_token).await?;
        Ok(Some(merge_refreshed_auth(auth.clone(), refreshed)))
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
            match self.refreshed_or_current(&profile.name, &auth).await {
                Ok(Some(updated)) => {
                    auth = updated;
                    save_account_profile(&profile.name, &profile.label, &auth, false).await?;
                }
                Ok(None) => {}
                Err(error) if clients.is_empty() => return Err(error),
                Err(_) => continue,
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
        if let Some(path) = provider_credentials_path()
            && let Some(credential) = load_xiaomi_mimo(&path)?
        {
            clients.push(RuntimeProviderClient::XiaomiMiMo(MiMoClient::new(
                credential.base_url,
                credential.api_key,
            )));
        }
        if clients.is_empty() {
            return Err(HarnessError::LoginRequired);
        }
        Ok(clients)
    }

    async fn legacy_default_client(&self) -> Result<RuntimeProviderClient, HarnessError> {
        let mut auth = load_default_auth().await?.ok_or(HarnessError::LoginRequired)?;
        if let Some(updated) = self.refreshed_or_current("legacy-default", &auth).await? {
            auth = updated;
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

    /// Reset tasks left `Running` by an interrupted process. Runs through any
    /// entry point (including `continue_session` and resumptions), not only
    /// fresh implementations, so a crashed graph cannot strand tasks.
    fn recover_running_tasks(&self, run_id: RunId) -> Result<(), HarnessError> {
        let mut recovered = 0;
        for task in self.store.tasks(run_id)? {
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
                recovered += 1;
            }
        }
        if recovered > 0 {
            self.store.record_runtime_event(
                run_id,
                RuntimeEvent::RunPhase {
                    phase: RunPhase::Recovering,
                    detail: format!(
                        "reloaded the persisted task graph; {recovered} running task(s) require explicit rescheduling"
                    ),
                },
            )?;
        }
        Ok(())
    }

    fn take_operation_approval(&self, run_id: RunId, requested: Option<Vec<String>>) -> bool {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        let approved = control.approved_operation_once.take();
        approved.is_some() && approved == requested
    }

    fn take_force_compaction(&self, run_id: RunId) -> bool {
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        std::mem::take(&mut control.force_compaction)
    }

    /// Clear transient resume state without granting a resumed run another
    /// session budget.  Usage is durable in SQLite; the in-memory counter is
    /// only a fast admission check for the next model call.
    fn reset_resume_control(&self, run_id: RunId) -> Result<(), HarnessError> {
        let used = self.store.usage_totals(Some(run_id))?;
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        control.steering.clear();
        control.interrupted = false;
        control.cooperative_pause = false;
        control.approved_operation_once = None;
        control.force_compaction = false;
        // Integration approval is durable only for the current process, so a
        // generic continuation must never discard it. Callers that resolve
        // the approval consume it explicitly in `resume_with_answer`.
        control.budget_tokens = control
            .budget_tokens
            .max(used.session_input.saturating_add(used.session_output));
        Ok(())
    }

    fn budget_pressure(&self, run_id: RunId) -> Result<BudgetPressure, HarnessError> {
        let target = self.config.budgets.default.soft_token_target();
        if target == 0 {
            return Ok(BudgetPressure::Paused);
        }
        let used = self.store.usage_totals(Some(run_id))?;
        let durable = used.session_input.saturating_add(used.session_output);
        let controls = self.controls.lock();
        let reserved = controls
            .get(&run_id)
            .map_or(durable, |control| control.budget_tokens.max(durable));
        let percent = reserved.saturating_mul(100) / target;
        Ok(if percent >= ADAPTIVE_PAUSE_PERCENT {
            BudgetPressure::Paused
        } else if percent >= ADAPTIVE_TAPER_PERCENT {
            BudgetPressure::Tapered
        } else {
            BudgetPressure::Normal
        })
    }

    fn session_budget_exhausted(&self, run_id: RunId) -> Result<bool, HarnessError> {
        Ok(self.budget_pressure(run_id)? == BudgetPressure::Paused)
    }

    fn cache_bypassed(&self, run_id: RunId) -> bool {
        self.controls
            .lock()
            .get(&run_id)
            .is_some_and(|control| control.bypass_cache)
    }

    fn try_reserve_budget(&self, run_id: RunId, tokens: u64) -> Result<bool, HarnessError> {
        let used = self.store.usage_totals(Some(run_id))?;
        let durable = used.session_input.saturating_add(used.session_output);
        let mut controls = self.controls.lock();
        let control = controls.entry(run_id).or_default();
        control.budget_tokens = control.budget_tokens.max(durable);
        let target = self.config.budgets.default.soft_token_target();
        // Do not spend the protected recovery band on a new request. The
        // final allowance is for deterministic evidence condensation and a
        // truthful paused state, never an extra speculative agent turn.
        let admission_limit = target.saturating_mul(ADAPTIVE_PAUSE_PERCENT) / 100;
        if control.budget_tokens.saturating_add(tokens) > admission_limit {
            return Ok(false);
        }
        control.budget_tokens = control.budget_tokens.saturating_add(tokens);
        Ok(true)
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

/// What an assignment is, independent of which model runs it.
///
/// Role strings are display/persistence text; every capability decision derives
/// from this enum in one place instead of from `role.contains(..)` checks
/// scattered across the runtime. Classification still reads the role string
/// because that is what callers and stored events carry, but it happens exactly
/// once, so renaming a role cannot silently change a budget or a permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RoleKind {
    AmbiguityConsultant,
    IssueClarifier,
    IntentClassifier,
    IntentRouter,
    Manager,
    Auditor,
    Judge,
    Planner,
    Integrator,
    Worker,
    Lead,
}

/// Typed capability policy for a role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RolePolicy {
    pub kind: RoleKind,
    pub turn_limit: usize,
    pub input_budget: u64,
    pub tool_budget: usize,
    pub can_ask_user: bool,
    /// May hold the session-leader assignment. Used when the preferred leader
    /// model is unavailable and a degraded route has to be chosen.
    pub leadership_eligible: bool,
    /// May be trusted with critical or high-risk work.
    pub critical_work_eligible: bool,
    pub initial_state: AgentState,
}

impl RolePolicy {
    pub fn for_role(role: &str) -> Self {
        Self::for_kind(RoleKind::classify(role))
    }

    pub fn for_kind(kind: RoleKind) -> Self {
        // (turn_limit, input_budget, tool_budget, can_ask_user, state)
        let (turn_limit, input_budget, tool_budget, can_ask_user, initial_state) = match kind {
            RoleKind::AmbiguityConsultant => (2, 12_000, 0, false, AgentState::Working),
            RoleKind::IssueClarifier => (4, 16_000, 4, true, AgentState::Working),
            RoleKind::IntentClassifier => (12, 200_000, 32, false, AgentState::Working),
            RoleKind::IntentRouter => (3, 24_000, 4, true, AgentState::Working),
            RoleKind::Manager => (3, 32_000, 0, false, AgentState::Working),
            RoleKind::Auditor => (5, 80_000, 12, false, AgentState::Verifying),
            RoleKind::Judge => (6, 80_000, 12, false, AgentState::Verifying),
            RoleKind::Planner => (8, 160_000, 16, true, AgentState::Planning),
            RoleKind::Integrator => (12, 200_000, 32, true, AgentState::Integrating),
            RoleKind::Worker => (10, 160_000, 32, true, AgentState::Working),
            RoleKind::Lead => (12, 200_000, 32, true, AgentState::Working),
        };
        Self {
            kind,
            turn_limit,
            input_budget,
            tool_budget,
            can_ask_user,
            leadership_eligible: matches!(
                kind,
                RoleKind::Lead | RoleKind::Planner | RoleKind::Integrator | RoleKind::IssueClarifier
            ),
            critical_work_eligible: matches!(
                kind,
                RoleKind::Lead | RoleKind::Integrator | RoleKind::Judge | RoleKind::Auditor
            ),
            initial_state,
        }
    }
}

impl RoleKind {
    /// Classify a role string. Order matters: the more specific markers are
    /// tested first, and stems (`synthes`, `integrat`, `plann`) are used so that
    /// noun and gerund spellings of the same role classify identically.
    fn classify(role: &str) -> Self {
        let role = role.to_ascii_lowercase();
        let has = |marker: &str| role.contains(marker);
        if has("ambiguity consultant") {
            Self::AmbiguityConsultant
        } else if has("clarifier") {
            Self::IssueClarifier
        } else if has("classifier") {
            Self::IntentClassifier
        } else if has("synthes") || has("integrat") {
            Self::Integrator
        } else if has("router") {
            Self::IntentRouter
        } else if has("manager") {
            Self::Manager
        } else if has("auditor") {
            Self::Auditor
        } else if has("judge") || has("review") {
            Self::Judge
        } else if has("plann") {
            Self::Planner
        } else if has("worker") {
            Self::Worker
        } else {
            Self::Lead
        }
    }
}

fn initial_agent_state(role: &str) -> AgentState {
    RolePolicy::for_role(role).initial_state
}

fn agent_turn_limit(role: &str) -> usize {
    RolePolicy::for_role(role).turn_limit
}

fn agent_input_budget(role: &str) -> u64 {
    RolePolicy::for_role(role).input_budget
}

fn agent_tool_budget(role: &str) -> usize {
    RolePolicy::for_role(role).tool_budget
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

    fn budget_pause(text: String, usage: TokenUsage) -> Self {
        Self {
            text,
            question: None,
            usage,
            paused: true,
            reserve_reached: false,
            termination: Some(TerminationReason::BudgetTarget),
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
    #[serde(default, alias = "acceptance_check")]
    check: String,
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
        // Stale lanes from a crashed dispatch share the same names because
        // the task was still Pending with the same generation and attempt.
        // They are regenerable snapshots, so remove them before copying.
        if baseline.exists() {
            std::fs::remove_dir_all(&baseline)?;
        }
        if path.exists() {
            std::fs::remove_dir_all(&path)?;
        }
        copy_workspace(root, &baseline)?;
        copy_workspace(root, &path)?;
        Ok(WorkerLane::Snapshot { baseline, path })
    }
}

/// Normalize a planner-supplied path for lease keys and overlap checks. Both
/// consumers must agree on the shape or the disjointness pre-filter and the
/// lease table can disagree (e.g. `./src/x` vs `src/x`).
fn normalize_lease_path(path: &str) -> &str {
    path.trim_start_matches("./").trim_matches('/')
}

/// Remove a worker lane after its attempt is resolved so lanes, snapshot
/// pairs, and `minha/*` worktree branches cannot accumulate without bound.
/// Best effort: cleanup failures are logged by the caller context, never
/// fatal.
fn cleanup_worker_lane(
    run_id: RunId,
    task: &TaskRecord,
    lane: WorkerLane,
    use_git_worktrees: bool,
    root: &Path,
) {
    match lane {
        WorkerLane::Git { baseline, path } => {
            if use_git_worktrees {
                let id = safe_component(&task.task_id);
                let branch = format!("minha/{}/{id}-g{}", short_id(run_id), task.generation);
                let repo = GitRepo::new(root);
                let _ = repo.remove_worktree(&path, true);
                let _ = repo.delete_branch(&branch);
            }
            let _ = std::fs::remove_dir_all(&baseline);
        }
        WorkerLane::Snapshot { baseline, path } => {
            let _ = std::fs::remove_dir_all(&baseline);
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn lease_resources_for(task_id: &str, paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        vec![format!("task:{task_id}")]
    } else {
        paths
            .iter()
            .map(|path| format!("path:{}", normalize_lease_path(path)))
            .collect()
    }
}

fn lease_resources(task: &TaskRecord) -> Vec<String> {
    lease_resources_for(&task.task_id, &task.paths)
}

fn default_acceptance_check(goal: &str) -> String {
    format!(
        "Run the smallest relevant verification for `{}` and report the exact command or inspected evidence.",
        bound(goal, 160)
    )
}

fn microtask_contract_for_branch(task: &BranchTask) -> MicrotaskContractV1 {
    let acceptance_check = if !task.check.trim().is_empty() {
        task.check.trim().to_owned()
    } else {
        default_acceptance_check(&task.objective)
    };
    MicrotaskContractV1 {
        schema_version: MICROTASK_CONTRACT_SCHEMA_VERSION,
        task_id: task.id.clone(),
        goal: task.objective.clone(),
        lease_resources: lease_resources_for(&task.id, &task.paths),
        acceptance_check,
    }
}

fn microtask_contract_for_task(task: &TaskRecord) -> MicrotaskContractV1 {
    MicrotaskContractV1 {
        schema_version: MICROTASK_CONTRACT_SCHEMA_VERSION,
        task_id: task.task_id.clone(),
        goal: task.objective.clone(),
        lease_resources: lease_resources(task),
        acceptance_check: default_acceptance_check(&task.objective),
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
            let left = normalize_lease_path(left);
            let right = normalize_lease_path(right);
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
            check: "Run the smallest relevant verification for this task and report its exact evidence."
                .into(),
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

/// A chosen leader plus, when the preferred leader could not be used, the reason
/// the route was degraded. The reason is surfaced to the user rather than the
/// substitution happening silently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LeadRoute<'a> {
    pub model: &'a str,
    pub degraded: Option<String>,
}

/// Models the user has explicitly placed in a leadership slot, in the order they
/// should be considered for `complex` and routine work respectively.
fn leadership_slots(config: &Config, complex: bool) -> Vec<&str> {
    let mut slots = if complex {
        vec![
            config.models.complex_lead.as_str(),
            config.models.lead.as_str(),
            config.models.planner.as_str(),
        ]
    } else {
        vec![
            config.models.lead.as_str(),
            config.models.complex_lead.as_str(),
            config.models.planner.as_str(),
        ]
    };
    let mut seen = HashSet::new();
    slots.retain(|model| seen.insert(*model));
    slots
}

/// Every available model that can be addressed unambiguously, in a stable order.
///
/// ChatGPT catalog slugs retain their legacy bare spelling and every provider
/// also has a qualified spelling; external providers are qualified-only.  The
/// latter prevents an otherwise ambiguous bare vendor slug from being routed
/// through the ChatGPT account. Sorting gives a deterministic tie-break
/// between equally eligible candidates instead of depending on hash order.
fn deterministic_candidates(available: &HashSet<String>) -> Vec<&str> {
    let mut candidates = available
        .iter()
        .map(String::as_str)
        .filter(|model| model.contains('/'))
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    candidates
}

/// Select the session leader.
///
/// No vendor is preferred: the configured leadership slots are tried in order,
/// and when none of them is available the choice falls to whatever is left, by a
/// deterministic tie-break. A leader that is not the preferred one always
/// carries an explanation of the degraded route.
fn routed_lead_model<'a>(
    goal: &str,
    available: &'a HashSet<String>,
    config: &'a Config,
) -> Result<LeadRoute<'a>, HarnessError> {
    let complex = complexity_score(goal) >= 5;
    let slots = leadership_slots(config, complex);
    let preferred = slots.first().copied().unwrap_or(config.models.lead.as_str());
    if available.contains(preferred) {
        return Ok(LeadRoute {
            model: preferred,
            degraded: None,
        });
    }
    // Degrade only to another explicitly leadership-capable model first.
    if let Some(model) = slots
        .iter()
        .skip(1)
        .copied()
        .find(|model| available.contains(*model))
    {
        return Ok(LeadRoute {
            model,
            degraded: Some(format!(
                "preferred leader `{preferred}` is unavailable; leading with `{model}`, which is also configured as leadership-capable"
            )),
        });
    }
    // No configured leader is reachable. Rather than stop, take the first
    // remaining candidate deterministically and say plainly that no
    // leadership-capable model was available.
    let model = deterministic_candidates(available)
        .first()
        .copied()
        .ok_or_else(|| HarnessError::ModelUnavailable(preferred.to_owned()))?;
    Ok(LeadRoute {
        model,
        degraded: Some(format!(
            "no configured leadership-capable model is available (preferred `{preferred}`); leading with `{model}` as a degraded route"
        )),
    })
}

/// Ordered worker candidates. Difficulty decides the configured slot order;
/// an unavailable slot is an explicit exclusion, never a hidden vendor bias.
fn worker_model_candidates<'a>(task: &TaskRecord, config: &'a Config) -> Vec<&'a str> {
    let score = complexity_score(&format!(
        "{}\n{}\n{}",
        task.objective,
        task.paths.join(" "),
        task.dependencies.join(" ")
    ));
    let escalate = task.attempt > 1 || task.last_error.is_some();
    let mut candidates = if escalate || score >= 7 {
        vec![
            config.models.worker_deep.as_str(),
            config.models.worker_medium.as_str(),
            config.models.worker_fast.as_str(),
        ]
    } else if score >= 4 {
        vec![
            config.models.worker_medium.as_str(),
            config.models.worker_fast.as_str(),
            config.models.worker_deep.as_str(),
        ]
    } else {
        vec![
            config.models.worker_fast.as_str(),
            config.models.worker_medium.as_str(),
            config.models.worker_deep.as_str(),
        ]
    };
    let mut seen = HashSet::new();
    candidates.retain(|model| seen.insert(*model));
    candidates
}

/// Role-compatible worker pool for equal-weight WDRR. Only live, qualified
/// catalog models enter it; configured slots are retained when observed, but
/// never receive ordering preference. Every observed candidate then receives
/// the same capability and policy filters before fair admission.
fn fair_worker_models(task: &TaskRecord, available: &HashSet<String>, config: &Config) -> Vec<String> {
    let observed = deterministic_candidates(available)
        .into_iter()
        .map(canonical_routing_model)
        .collect::<HashSet<_>>();
    let mut models = worker_model_candidates(task, config)
        .into_iter()
        .map(canonical_routing_model)
        .filter(|model| observed.contains(model))
        .collect::<Vec<_>>();
    models.extend(observed);
    models.sort();
    models.dedup();
    models
}

fn fair_audit_models(available: &HashSet<String>, config: &Config) -> Vec<String> {
    let observed = deterministic_candidates(available)
        .into_iter()
        .map(canonical_routing_model)
        .collect::<HashSet<_>>();
    let mut models = [
        config.models.worker_fast.as_str(),
        config.models.worker_medium.as_str(),
        config.models.worker_deep.as_str(),
    ]
    .into_iter()
    .map(canonical_routing_model)
    .filter(|model| observed.contains(model))
    .collect::<Vec<_>>();
    models.extend(observed);
    models.sort();
    models.dedup();
    models
}

/// DeepSeek Pro is intentionally outside automatic worker routing.  It may be
/// presented in a provider catalog, but the receipt must show that a policy
/// exclusion — not an availability accident — kept it out of the pool.
fn worker_model_policy_exclusion(model: &str) -> Option<&'static str> {
    let reference = ModelRef::parse_or_legacy_chatgpt(model);
    (reference.provider == ProviderId::DeepSeek && reference.slug == "deepseek-v4-pro")
        .then_some("DeepSeek Pro is excluded from automatic worker routing by policy")
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
        // The normal profile remains a single focused lane unless the user
        // asks for parallel work. Turbo is an explicit opt-in to inspect a
        // complex request for independent speedup, but the planner still has
        // to prove disjoint work before more than one task is dispatched.
        crate::config::ExecutionProfile::Balanced => explicit_parallelism,
        crate::config::ExecutionProfile::Turbo => explicit_parallelism || score >= 5,
    }
}

/// The agent identity that owns a role.
///
/// This keys off the role, not the model slug. Keying off the slug was wrong:
/// real slugs are vendor strings like `deepseek/deepseek-v4-flash`, which match
/// no identity, so every such agent was labelled `Codex` and introduced itself
/// with the wrong identity in its own system prompt. There is no catch-all
/// family here — a role that names no identity is led by Mina, which is what an
/// unnamed lead role has always meant.
fn identity_label(role: &str) -> &'static str {
    if role.contains("Spark") {
        "Spark"
    } else if role.contains("Terra") {
        "Terra"
    } else if role.contains("Sol") {
        "Sol"
    } else {
        "Mina"
    }
}

/// Identity to use when naming a role that is being created for a model, before
/// any role string exists.
///
/// A model outside the known identity families is named by what it actually is
/// — its provider or its own slug — rather than being folded into an unrelated
/// family, so an agent never introduces itself as a model it is not.
fn model_identity(model: &str) -> String {
    let reference = ModelRef::parse_or_legacy_chatgpt(model);
    let slug = reference.slug.to_ascii_lowercase();
    for (marker, identity) in [
        ("spark", "Spark"),
        ("terra", "Terra"),
        ("sol", "Sol"),
        ("luna", "Mina"),
    ] {
        if slug.contains(marker) {
            return identity.to_owned();
        }
    }
    match reference.provider {
        ProviderId::DeepSeek => "DeepSeek".to_owned(),
        ProviderId::XiaomiMiMo => "MiMo".to_owned(),
        ProviderId::ChatGptCodex => reference.slug,
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
    RolePolicy::for_role(role).can_ask_user
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

fn dispatch_receipt_id(run_id: RunId, task_id: &str, generation: u64, agent_id: EventAgentId) -> String {
    format!("dispatch:{run_id}:{task_id}:{generation}:{agent_id}")
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

/// Classify free-text answers into action verbs (`minha answer cancel`).
/// Only answers not bound to a question id count, so a free-text "cancel"
/// never gets bound to a question; values like "$action=cancel" are handled
/// by the `$action` branch instead.
fn free_action_from_answers(answers: &[(String, String)]) -> Option<&'static str> {
    let action = |verb: &'static str| {
        answers
            .iter()
            .filter(|(id, _)| !id.starts_with('$'))
            .find(|(_, value)| value.trim().eq_ignore_ascii_case(verb))
            .map(|_| verb)
    };
    action("cancel").or_else(|| action("confirm"))
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
            atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
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
            Self::start_with_reserve(None)
        }

        /// Serve `x-codex-primary-used-percent: <percent>` on worker-labeled
        /// responses so the account-usage reserve path can be exercised.
        fn start_with_reserve(reserve_percent: Option<f64>) -> io::Result<Self> {
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
                    let lower = request.to_ascii_lowercase();
                    // Equal-weight routing may choose any qualified model for
                    // a worker. Keep the reserve fixture tied to the worker
                    // role rather than one historical model identity.
                    let reserve = reserve_percent.filter(|_| lower.contains("_worker_"));
                    write_fixture_response(&mut stream, &fixture_response(&request), reserve)?;
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

    fn write_fixture_response(
        stream: &mut TcpStream,
        response: &str,
        reserve_percent: Option<f64>,
    ) -> io::Result<()> {
        let content_type = if response.starts_with('{') {
            "application/json"
        } else {
            "text/event-stream"
        };
        let reserve = reserve_percent
            .map(|percent| format!("x-codex-primary-used-percent: {percent}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{reserve}Content-Length: {}\r\nConnection: close\r\n\r\n{response}",
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
        if lower.contains("x-openai-subagent: mina,_branch_planning") {
            return fixture_text(
                r#"<minha-plan>{"summary":"two independent fixes","consult":null,"tasks":[{"id":"slug","objective":"Implement slugify and run its test","paths":["src/slug.rs"],"dependencies":[]},{"id":"stats","objective":"Implement word_counts and run its test","paths":["src/stats.rs"],"dependencies":[]}]}</minha-plan>"#,
            );
        }
        if lower.contains("_worker_slug") {
            return if has_tool_output {
                fixture_text("slug task complete")
            } else {
                fixture_tool("slug-patch", "apply_patch", &json!({"patch": slug_patch()}))
            };
        }
        if lower.contains("_worker_stats") {
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
        if lower.contains("x-openai-subagent: mina,_integrating") {
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
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
        let contracts = harness
            .store
            .task_contracts(outcome.run_id)
            .expect("durable task contracts");
        assert_eq!(contracts.len(), 2);
        assert!(contracts.iter().all(|contract| {
            !contract.lease_resources.is_empty() && !contract.acceptance_check.trim().is_empty()
        }));
        let receipts = harness
            .store
            .dispatch_receipts(outcome.run_id)
            .expect("durable dispatch receipts");
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().all(|receipt| {
            receipt.candidates.iter().any(|candidate| candidate.eligible)
                && !receipt.parallelism_reason.trim().is_empty()
                && !receipt.acceptance_check.trim().is_empty()
        }));

        let status = Command::new("cargo")
            .arg("test")
            .current_dir(root)
            .status()
            .expect("fixture cargo test should start");
        assert!(status.success());
        let labels = server.request_labels();
        assert!(labels.iter().any(|label| label.ends_with("_worker_slug")));
        assert!(labels.iter().any(|label| label.ends_with("_worker_stats")));
        assert!(labels.iter().any(|label| label == "mina,_integrating"));
        assert!(labels.iter().any(|label| label == "spark_completion_judge"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn implementation_hive_pauses_for_integration_approval_when_configured() {
        let temp = fixture_repo();
        let root = temp.path();
        let server = HiveFixtureServer::start().expect("local provider fixture");
        let store = Store::open(root.join(".minha/test.sqlite3")).expect("fixture store");
        let workspace = store.ensure_workspace(root).expect("fixture workspace");
        let mut config = Config::default();
        config.books.enabled = false;
        config.cache.enabled = false;
        config.scheduler.integration_approval = true;
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        };

        let outcome = harness
            .run(
                RunKind::Implement,
                "Implement slugify and word_counts as two independent tasks, then verify both.",
            )
            .await
            .expect("fixture implementation run");

        assert_eq!(outcome.state, ExitState::ApprovalRequired, "outcome: {outcome:?}");
        let question = outcome
            .question
            .as_ref()
            .expect("integration approval gate must ask a question")
            .question
            .clone();
        assert!(
            question.contains("src/slug.rs"),
            "approval request missing the slug task's path: {question}"
        );
        assert!(
            question.contains("src/stats.rs"),
            "approval request missing the stats task's path: {question}"
        );
        assert_eq!(outcome.worktrees.len(), 2, "outcome: {outcome:?}");

        // The integrator (and therefore the judge downstream of it) must
        // never have been invoked while paused for approval.
        let labels = server.request_labels();
        assert!(labels.iter().any(|label| label.ends_with("_worker_slug")));
        assert!(labels.iter().any(|label| label.ends_with("_worker_stats")));
        assert!(!labels.iter().any(|label| label == "mina,_integrating"));
        assert!(!labels.iter().any(|label| label == "spark_completion_judge"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn implementation_hive_declining_integration_skips_integrator_and_preserves_recovery_patches() {
        let temp = fixture_repo();
        let root = temp.path();
        let server = HiveFixtureServer::start().expect("local provider fixture");
        let store = Store::open(root.join(".minha/test.sqlite3")).expect("fixture store");
        let workspace = store.ensure_workspace(root).expect("fixture workspace");
        let mut config = Config::default();
        config.books.enabled = false;
        config.cache.enabled = false;
        config.scheduler.integration_approval = true;
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        };

        let paused = harness
            .run(
                RunKind::Implement,
                "Implement slugify and word_counts as two independent tasks, then verify both.",
            )
            .await
            .expect("fixture implementation run");
        assert_eq!(paused.state, ExitState::ApprovalRequired, "outcome: {paused:?}");
        // Both branch tasks got a lane during dispatch. Worker lanes are
        // cleaned up as soon as each task's patch is captured (win or
        // lose), so by the time this gate is reached the lane directories
        // themselves are already gone; the historical count/paths remain
        // useful as a record of what ran.
        assert_eq!(paused.worktrees.len(), 2, "outcome: {paused:?}");

        let declined = harness
            .resume_with_answer(paused.run_id, "decline")
            .await
            .expect("declining integration approval");

        assert_eq!(declined.state, ExitState::Inconclusive, "outcome: {declined:?}");
        assert_eq!(declined.worktrees, paused.worktrees, "outcome: {declined:?}");

        // Declining must leave the branch work unintegrated: each task's
        // patch was already applied directly to the primary checkout as an
        // uncommitted change, and its recovery patch file is preserved on
        // disk so the change can be reviewed or reverted independent of
        // Mina ever running the integrator.
        let recovery_dir = root.join(".minha/recovery").join(paused.run_id.to_string());
        let patch_files = std::fs::read_dir(&recovery_dir)
            .expect("recovery directory should exist")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "patch")
            })
            .count();
        assert!(
            patch_files >= 2,
            "expected a recovery patch per branch task, found {patch_files} in {recovery_dir:?}"
        );
        assert!(
            declined.text.contains("Recovery patches:") || declined.text.contains("recovery patches"),
            "decline report should point at the recovery patches: {}",
            declined.text
        );
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root)
            .output()
            .expect("git status should run");
        assert!(
            !String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "primary checkout should still hold the uncommitted worker changes after decline"
        );

        // Integration and judging must never have run.
        let labels = server.request_labels();
        assert!(!labels.iter().any(|label| label == "mina,_integrating"));
        assert!(!labels.iter().any(|label| label == "spark_completion_judge"));
    }

    /// Minimal git repository with the two-task fixture source tree. The
    /// returned temp dir stays alive for the duration of the test.
    fn fixture_repo() -> tempfile::TempDir {
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
        temp
    }

    fn fixture_harness(root: &Path, clients: Vec<RuntimeProviderClient>) -> Harness {
        let store = Store::open(root.join(".minha/test.sqlite3")).expect("fixture store");
        let workspace = store.ensure_workspace(root).expect("fixture workspace");
        let mut config = Config::default();
        config.books.enabled = false;
        config.cache.enabled = false;
        Harness {
            root: root.canonicalize().expect("canonical fixture root"),
            workspace_id: workspace.id,
            config,
            store,
            controls: Arc::new(Mutex::new(HashMap::new())),
            account_clients: Arc::new(Mutex::new(clients)),
            hot_cache: Arc::new(Mutex::new(HotCache::with_limits(16, HOT_CACHE_MAX_BYTES))),
            model_context_limits: Arc::new(Mutex::new(HashMap::new())),
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn fixture_task(
        run_id: RunId,
        task_id: &str,
        state: PlanTaskState,
        attempt: u32,
        generation: u64,
    ) -> TaskRecord {
        let now = chrono::Utc::now();
        TaskRecord {
            run_id,
            task_id: task_id.into(),
            objective: format!("implement {task_id}"),
            paths: Vec::new(),
            dependencies: Vec::new(),
            state,
            assigned_agent_id: None,
            attempt,
            max_attempts: 2,
            generation,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reserve_pause_resets_attempt_and_survives_resume() {
        let temp = fixture_repo();
        let root = temp.path().to_path_buf();
        let reserve_server = HiveFixtureServer::start_with_reserve(Some(95.0)).expect("reserve fixture");
        let harness = fixture_harness(
            &root,
            vec![ChatGptClient::new(&reserve_server.base_url, "fixture-token", "fixture-account").into()],
        );

        let outcome = harness
            .run(
                RunKind::Implement,
                "Implement slugify and word_counts as two independent tasks, then verify both.",
            )
            .await
            .expect("fixture implementation run");
        assert_eq!(outcome.state, ExitState::UsagePaused, "outcome: {outcome:?}");
        let paused = harness.store.tasks(outcome.run_id).expect("fixture tasks");
        assert_eq!(paused.len(), 2);
        for task in &paused {
            assert_eq!(
                task.state,
                PlanTaskState::Pending,
                "a reserve pause must not fail the task: {task:?}"
            );
            assert_eq!(
                task.attempt, 0,
                "a reserve pause must not consume retry budget: {task:?}"
            );
            assert_eq!(task.generation, 1, "task: {task:?}");
        }

        let healthy_server = HiveFixtureServer::start().expect("healthy fixture");
        *harness.account_clients.lock() =
            vec![ChatGptClient::new(&healthy_server.base_url, "fixture-token", "fixture-account").into()];
        let resumed = harness.resume_paused(outcome.run_id).await.expect("resumed run");
        assert_eq!(resumed.state, ExitState::Succeeded, "resumed: {resumed:?}");
        let tasks = harness.store.tasks(outcome.run_id).expect("fixture tasks");
        assert!(
            tasks.iter().all(|task| task.state == PlanTaskState::Completed),
            "tasks: {tasks:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn per_slot_client_selection_survives_agent_dispatch() {
        let temp = fixture_repo();
        let root = temp.path().to_path_buf();
        let first = HiveFixtureServer::start().expect("first fixture");
        let second = HiveFixtureServer::start().expect("second fixture");
        let harness = fixture_harness(
            &root,
            vec![
                ChatGptClient::new(&first.base_url, "fixture-token", "fixture-account-1").into(),
                ChatGptClient::new(&second.base_url, "fixture-token", "fixture-account-2").into(),
            ],
        );

        let outcome = harness
            .run(
                RunKind::Implement,
                "Implement slugify and word_counts as two independent tasks, then verify both.",
            )
            .await
            .expect("fixture implementation run");
        assert_eq!(outcome.state, ExitState::Succeeded, "outcome: {outcome:?}");
        let labels = second.request_labels();
        assert!(
            labels.iter().any(|label| label.starts_with("spark_worker_")),
            "the second account must receive its per-slot worker; second server saw: {labels:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resume_with_answer_resumes_every_blocked_task() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let harness = fixture_harness(
            temp.path(),
            vec![ChatGptClient::new("http://127.0.0.1:1", "fixture-token", "fixture-account").into()],
        );
        let run = harness
            .store
            .create_run("goal", Mode::Batch)
            .expect("fixture run");
        harness
            .store
            .update_run_state(run.id, ExitState::NeedsInput, None, Some("what next?"), None)
            .expect("fixture state");
        harness
            .store
            .replace_tasks(
                run.id,
                &[
                    fixture_task(run.id, "t1", PlanTaskState::Blocked, 1, 1),
                    fixture_task(run.id, "t2", PlanTaskState::Blocked, 1, 1),
                ],
            )
            .expect("fixture tasks");

        harness
            .resume_with_answer(run.id, "keep going")
            .await
            .expect_err("dispatch must fail with an unreachable provider after resuming tasks");
        let tasks = harness.store.tasks(run.id).expect("fixture tasks");
        assert_eq!(tasks.len(), 2);
        for task in &tasks {
            assert_eq!(
                task.state,
                PlanTaskState::Pending,
                "every blocked task must be resumed, not just the first: {task:?}"
            );
            assert_eq!(task.generation, 2, "task: {task:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn free_text_cancel_aborts_collecting_without_binding_to_a_question() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let harness = fixture_harness(
            temp.path(),
            vec![ChatGptClient::new("http://127.0.0.1:1", "fixture-token", "fixture-account").into()],
        );
        let run = harness
            .store
            .create_run("goal", Mode::Batch)
            .expect("fixture run");
        let mut clarification = crate::clarify::analyze(&run.goal, "auto");
        clarification.pending_batch = Some(crate::clarify::make_fallback_batch(&clarification));
        harness
            .store
            .save_issue_clarification(run.id, &clarification)
            .expect("fixture clarification");
        harness
            .store
            .update_run_state(run.id, ExitState::NeedsInput, None, Some("what next?"), None)
            .expect("fixture state");

        let outcome = harness
            .resume_with_clarification_answers(run.id, &[("".into(), "cancel".into())])
            .await
            .expect("free-text cancel must not require a provider round");
        assert_eq!(outcome.state, ExitState::Cancelled);
        assert_eq!(
            harness
                .store
                .run(run.id)
                .expect("fixture run")
                .expect("fixture run")
                .state,
            ExitState::Cancelled
        );
        let saved = harness
            .store
            .issue_clarification(run.id)
            .expect("fixture clarification")
            .expect("clarification must persist");
        assert_eq!(saved.status, ClarificationStatus::Cancelled);
        assert!(
            saved.meter.dimensions.iter().all(|dimension| {
                !matches!(
                    dimension.status,
                    crate::protocol::DimensionStatus::Confirmed
                        | crate::protocol::DimensionStatus::Delegated
                        | crate::protocol::DimensionStatus::NotApplicable
                )
            }),
            "the free-text cancel must never move the meter: {saved:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn continue_session_recovers_running_tasks() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let harness = fixture_harness(
            temp.path(),
            vec![ChatGptClient::new("http://127.0.0.1:1", "fixture-token", "fixture-account").into()],
        );
        let run = harness
            .store
            .create_run("goal", Mode::Batch)
            .expect("fixture run");
        harness
            .store
            .update_run_state(run.id, ExitState::Running, None, None, None)
            .expect("fixture state");
        harness
            .store
            .replace_tasks(
                run.id,
                &[fixture_task(run.id, "t1", PlanTaskState::Running, 1, 0)],
            )
            .expect("fixture tasks");

        harness
            .continue_session(run.id, "keep going")
            .await
            .expect_err("dispatch must fail with an unreachable provider after recovery");
        let task = harness
            .store
            .tasks(run.id)
            .expect("fixture tasks")
            .into_iter()
            .find(|task| task.task_id == "t1")
            .expect("fixture task");
        assert_eq!(task.state, PlanTaskState::Pending, "task: {task:?}");
        assert_eq!(task.generation, 1, "task: {task:?}");
        assert_eq!(
            task.last_error.as_deref(),
            Some("recovered after an interrupted process")
        );
    }

    #[test]
    fn try_reserve_budget_preserves_the_recovery_band() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let mut harness = fixture_harness(temp.path(), Vec::new());
        harness.config.budgets.default = crate::config::ExecutionProfile::Economy;
        let run_id = RunId::new();
        assert!(harness.try_reserve_budget(run_id, 10_000).expect("reservation"));
        assert!(harness.try_reserve_budget(run_id, 13_000).expect("reservation"));
        assert!(
            harness
                .try_reserve_budget(run_id, 750)
                .expect("reservation at the 95% boundary")
        );
        assert!(
            !harness.try_reserve_budget(run_id, 1).expect("reservation"),
            "the final five percent must stay available for recovery"
        );
        let fresh_run = RunId::new();
        assert!(
            harness
                .try_reserve_budget(fresh_run, 23_750)
                .expect("reservation")
        );
        assert!(!harness.try_reserve_budget(fresh_run, 1).expect("reservation"));
    }

    #[test]
    fn static_or_cached_catalogs_do_not_clear_provider_remediation_state() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let harness = fixture_harness(temp.path(), Vec::new());
        harness
            .store
            .record_provider_remediation_needed(
                &harness.workspace_id,
                ProviderId::DeepSeek.key(),
                ProviderHealthStatusV1::AuthenticationRequired,
                "fixture authentication failure",
            )
            .expect("provider remediation state");
        let static_state = harness
            .record_provider_catalog_observation(ProviderId::DeepSeek, CatalogProvenance::StaticFallback)
            .expect("static catalog observation");
        assert_eq!(
            static_state.status,
            ProviderHealthStatusV1::AuthenticationRequired
        );
        let cached_state = harness
            .record_provider_catalog_observation(ProviderId::DeepSeek, CatalogProvenance::Cached)
            .expect("cached catalog observation");
        assert_eq!(
            cached_state.status,
            ProviderHealthStatusV1::AuthenticationRequired
        );
        let live_state = harness
            .record_provider_catalog_observation(ProviderId::DeepSeek, CatalogProvenance::Live)
            .expect("live catalog observation");
        assert_eq!(live_state.status, ProviderHealthStatusV1::Healthy);
    }

    #[test]
    fn local_agent_usage_keys_are_stable_without_provider_response_ids() {
        let run_id = RunId::new();
        let agent_id = EventAgentId::new();
        let first = usage_entry_key(
            run_id,
            Some(agent_id),
            2,
            UsageKindV1::ModelTurn,
            ProviderId::DeepSeek,
            None,
        );
        assert_eq!(
            first,
            usage_entry_key(
                run_id,
                Some(agent_id),
                2,
                UsageKindV1::ModelTurn,
                ProviderId::DeepSeek,
                None,
            )
        );
        assert_ne!(
            first,
            usage_entry_key(
                run_id,
                Some(agent_id),
                3,
                UsageKindV1::ModelTurn,
                ProviderId::DeepSeek,
                None,
            )
        );
        assert_ne!(
            usage_entry_key(
                run_id,
                None,
                0,
                UsageKindV1::Compaction,
                ProviderId::DeepSeek,
                None,
            ),
            usage_entry_key(
                run_id,
                None,
                0,
                UsageKindV1::Compaction,
                ProviderId::DeepSeek,
                None,
            ),
            "separate legitimate compaction attempts have no stable turn identity"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn denied_budget_admission_does_not_create_a_ghost_agent() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let mut harness = fixture_harness(temp.path(), Vec::new());
        harness.config.budgets.default = crate::config::ExecutionProfile::Economy;
        let run = harness
            .store
            .create_run("budget boundary", Mode::Batch)
            .expect("run");
        assert!(
            harness
                .try_reserve_budget(run.id, 23_750)
                .expect("reserve recovery boundary")
        );
        let client = RuntimeProviderClient::ChatGpt(ChatGptClient::new(
            "http://127.0.0.1:9",
            "fixture-token",
            "fixture-account",
        ));
        let result = harness
            .run_agent_as(
                run.id,
                &client,
                "gpt-5.6-luna",
                "test system",
                "test prompt",
                ToolExecutor::new(temp.path(), false).expect("executor"),
                "Mina, direct task",
                EventAgentId::new(),
            )
            .await
            .expect("budget pause result");
        assert!(result.paused);
        assert_eq!(result.termination, Some(TerminationReason::BudgetTarget));
        assert!(harness.store.agents(run.id).expect("agent records").is_empty());
        assert!(
            !harness
                .store
                .events(run.id)
                .expect("events")
                .iter()
                .any(|event| matches!(event.event, RuntimeEvent::AgentStarted { .. }))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sanitized_95_percent_continuation_cannot_reset_budget_or_duplicate_the_lead() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let mut harness = fixture_harness(temp.path(), Vec::new());
        harness.config.budgets.default = crate::config::ExecutionProfile::Economy;
        let run = harness
            .store
            .create_run("durable budget boundary", Mode::Batch)
            .expect("run");
        assert!(
            harness
                .try_reserve_budget(run.id, 23_750)
                .expect("reserve exactly the protected boundary")
        );
        assert!(harness.session_budget_exhausted(run.id).expect("budget pressure"));

        // This mirrors a sanitized continuation: one-shot controls clear, but
        // the in-memory reservation must remain at least as large as the
        // durable session use. A continuation must not buy a fresh lead turn.
        harness.reset_resume_control(run.id).expect("resume reset");
        assert!(
            harness
                .session_budget_exhausted(run.id)
                .expect("budget remains paused")
        );
        assert!(
            !harness
                .try_reserve_budget(run.id, 1)
                .expect("no room beyond the protected boundary")
        );

        let client = RuntimeProviderClient::ChatGpt(ChatGptClient::new(
            "http://127.0.0.1:9",
            "fixture-token",
            "fixture-account",
        ));
        for _ in 0..2 {
            let result = harness
                .run_agent_as(
                    run.id,
                    &client,
                    "gpt-5.6-luna",
                    "test system",
                    "test prompt",
                    ToolExecutor::new(temp.path(), false).expect("executor"),
                    "Mina, session lead",
                    EventAgentId::new(),
                )
                .await
                .expect("budget pause result");
            assert!(result.paused);
            assert_eq!(result.termination, Some(TerminationReason::BudgetTarget));
        }
        assert!(harness.store.agents(run.id).expect("agent records").is_empty());
        assert!(
            !harness
                .store
                .events(run.id)
                .expect("events")
                .iter()
                .any(|event| matches!(event.event, RuntimeEvent::AgentStarted { .. }))
        );
    }

    #[test]
    fn stale_worker_lanes_are_removed_before_recopy() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let root = temp.path();
        std::fs::write(root.join("file.txt"), "fresh").expect("workspace file");
        let lane_dir = tempfile::tempdir().expect("lane directory outside the workspace");
        let lane_base = lane_dir.path().to_path_buf();
        let run_id = RunId::new();
        let task = fixture_task(run_id, "t1", PlanTaskState::Pending, 0, 0);
        let stale_base = lane_base.join("t1-g0-a0-base");
        let stale_lane = lane_base.join("t1-g0-a0-lane");
        std::fs::create_dir_all(&stale_base).expect("stale baseline");
        std::fs::create_dir_all(&stale_lane).expect("stale lane");
        std::fs::write(stale_base.join("file.txt"), "stale").expect("stale baseline file");
        std::fs::write(stale_lane.join("file.txt"), "stale").expect("stale lane file");

        let lane = prepare_worker_lane(root, &lane_base, run_id, &task, 0, false)
            .expect("lane preparation must recover from stale crash leftovers");
        let (baseline, path) = match lane {
            WorkerLane::Snapshot { baseline, path } => (baseline, path),
            WorkerLane::Git { .. } => panic!("snapshot lane expected"),
        };
        assert_eq!(
            std::fs::read_to_string(baseline.join("file.txt")).expect("fresh baseline"),
            "fresh"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("file.txt")).expect("fresh lane"),
            "fresh"
        );
    }

    #[test]
    fn worker_lane_cleanup_removes_snapshots_worktrees_and_branches() {
        let temp = tempfile::tempdir().expect("temporary fixture directory");
        let root = temp.path();
        let run_id = RunId::new();
        let task = fixture_task(run_id, "t1", PlanTaskState::Pending, 1, 0);

        let snapshot_base = root.join("s-base");
        let snapshot_lane = root.join("s-lane");
        std::fs::create_dir_all(&snapshot_base).expect("baseline");
        std::fs::create_dir_all(&snapshot_lane).expect("lane");
        cleanup_worker_lane(
            run_id,
            &task,
            WorkerLane::Snapshot {
                baseline: snapshot_base.clone(),
                path: snapshot_lane.clone(),
            },
            false,
            root,
        );
        assert!(!snapshot_base.exists(), "snapshot baseline must be removed");
        assert!(!snapshot_lane.exists(), "snapshot lane must be removed");

        fixture_git(root, &["init", "--quiet"]);
        fixture_git(root, &["config", "user.name", "Minha Test"]);
        fixture_git(root, &["config", "user.email", "minha-test@invalid.example"]);
        std::fs::write(root.join("file.txt"), "content").expect("file");
        fixture_git(root, &["add", "."]);
        fixture_git(root, &["commit", "--quiet", "-m", "baseline"]);
        let worktree = root.join("wt");
        let branch = format!("minha/{}/t1-g0", short_id(run_id));
        GitRepo::new(root)
            .add_worktree(&worktree, &branch, Some("HEAD"))
            .expect("worktree add");
        let baseline = root.join("wt-base");
        std::fs::create_dir_all(&baseline).expect("baseline");
        cleanup_worker_lane(
            run_id,
            &task,
            WorkerLane::Git {
                baseline,
                path: worktree.clone(),
            },
            true,
            root,
        );
        assert!(!worktree.exists(), "worktree directory must be removed");
        let branches = Command::new("git")
            .args(["branch", "--list", &branch])
            .current_dir(root)
            .output()
            .expect("branch list");
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "minha/* branch must be deleted after cleanup"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_refresh_reuses_the_record_the_first_caller_saved() {
        let locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let store = Arc::new(Mutex::new(HashMap::new()));
        let refresh_count = Arc::new(AtomicUsize::new(0));
        let old = AuthRecord {
            access_token: "old-token".into(),
            refresh_token: Some("refresh-token".into()),
            id_token: None,
            account_id: None,
            email: None,
            expires_at_unix: Some(chrono::Utc::now().timestamp() + 30),
        };
        let load = |store: Arc<Mutex<HashMap<String, AuthRecord>>>| {
            move || {
                let store = Arc::clone(&store);
                async move { Ok::<_, HarnessError>(store.lock().get("work").cloned()) }
            }
        };
        let refresh = |refresh_count: Arc<AtomicUsize>| {
            move |refresh: String| {
                assert_eq!(refresh, "refresh-token");
                let refresh_count = Arc::clone(&refresh_count);
                async move {
                    refresh_count.fetch_add(1, AtomicOrdering::SeqCst);
                    Ok::<_, HarnessError>(AuthRecord {
                        access_token: "fresh-token".into(),
                        refresh_token: Some("fresh-refresh".into()),
                        id_token: None,
                        account_id: None,
                        email: None,
                        expires_at_unix: Some(chrono::Utc::now().timestamp() + 3600),
                    })
                }
            }
        };

        let first = Harness::refreshed_or_current_with(
            &locks,
            "work",
            &old,
            load(Arc::clone(&store)),
            refresh(Arc::clone(&refresh_count)),
        )
        .await
        .expect("test operation should succeed")
        .expect("an expiring record must be refreshed");
        assert_eq!(first.access_token, "fresh-token");
        store.lock().insert("work".into(), first.clone());
        assert_eq!(refresh_count.load(AtomicOrdering::SeqCst), 1);

        let second = Harness::refreshed_or_current_with(
            &locks,
            "work",
            &old,
            load(Arc::clone(&store)),
            |_| async move { unreachable!("the saved record must be reused, not refreshed") },
        )
        .await
        .expect("test operation should succeed")
        .expect("a stored record must be returned");
        assert_eq!(second.access_token, "fresh-token");
        assert_eq!(
            refresh_count.load(AtomicOrdering::SeqCst),
            1,
            "the second caller must reuse the first caller's saved record"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_refreshes_serialize_even_with_stale_records() {
        let locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let store = Arc::new(Mutex::new(HashMap::new()));
        let refresh_count = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let old = AuthRecord {
            access_token: "old-token".into(),
            refresh_token: Some("refresh-token".into()),
            id_token: None,
            account_id: None,
            email: None,
            expires_at_unix: Some(chrono::Utc::now().timestamp() + 30),
        };
        store.lock().insert("work".into(), old.clone());

        let run = |locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
                   store: Arc<Mutex<HashMap<String, AuthRecord>>>,
                   refresh_count: Arc<AtomicUsize>,
                   active: Arc<AtomicUsize>,
                   old: AuthRecord| {
            async move {
                let result = Harness::refreshed_or_current_with(
                    &locks,
                    "work",
                    &old,
                    move || {
                        let store = Arc::clone(&store);
                        async move { Ok::<_, HarnessError>(store.lock().get("work").cloned()) }
                    },
                    move |refresh: String| {
                        let refresh_count = Arc::clone(&refresh_count);
                        let active = Arc::clone(&active);
                        async move {
                            assert_eq!(
                                active.fetch_add(1, AtomicOrdering::SeqCst),
                                0,
                                "refreshes must never overlap"
                            );
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            active.fetch_sub(1, AtomicOrdering::SeqCst);
                            refresh_count.fetch_add(1, AtomicOrdering::SeqCst);
                            Ok::<_, HarnessError>(AuthRecord {
                                access_token: format!("fresh-{refresh}"),
                                refresh_token: Some("fresh-refresh".into()),
                                id_token: None,
                                account_id: None,
                                email: None,
                                expires_at_unix: Some(chrono::Utc::now().timestamp() + 3600),
                            })
                        }
                    },
                )
                .await
                .expect("test operation should succeed")
                .expect("a stale record must be refreshed");
                result.access_token
            }
        };

        let (first, second) = tokio::join!(
            run(
                Arc::clone(&locks),
                Arc::clone(&store),
                Arc::clone(&refresh_count),
                Arc::clone(&active),
                old.clone(),
            ),
            run(
                Arc::clone(&locks),
                Arc::clone(&store),
                Arc::clone(&refresh_count),
                Arc::clone(&active),
                old.clone(),
            ),
        );
        assert_eq!(refresh_count.load(AtomicOrdering::SeqCst), 2);
        assert!(first.starts_with("fresh-"));
        assert!(second.starts_with("fresh-"));
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
    fn balanced_parallelism_requires_an_explicit_speedup_request() {
        use crate::config::ExecutionProfile;

        assert!(!should_delegate(
            "redesign a cross-cutting database and protocol migration",
            ExecutionProfile::Balanced
        ));
        assert!(should_delegate(
            "implement these independent tasks in parallel",
            ExecutionProfile::Balanced
        ));
        assert!(!should_delegate(
            "implement these independent tasks in parallel",
            ExecutionProfile::Economy
        ));
        assert!(should_delegate(
            "redesign the architecture for a concurrent distributed database migration with authentication security and a production release",
            ExecutionProfile::Turbo
        ));
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
                    check: "Read the parser and report the relevant invariant.".into(),
                },
                BranchTask {
                    id: "change".into(),
                    objective: "change".into(),
                    paths: vec!["tests/parser.rs".into()],
                    dependencies: vec!["inspect".into()],
                    check: "Run the parser test after the change.".into(),
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
    fn lease_normalization_matches_between_planning_and_leases() {
        let now = chrono::Utc::now();
        let task = |paths: Vec<String>| TaskRecord {
            run_id: RunId::new(),
            task_id: "parser".into(),
            objective: "inspect the parser".into(),
            paths,
            dependencies: Vec::new(),
            state: PlanTaskState::Pending,
            assigned_agent_id: None,
            attempt: 0,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let with_prefix = task(vec!["./src/parser.rs".into()]);
        let bare = task(vec!["src/parser.rs".into()]);
        assert!(
            tasks_overlap(&with_prefix, &bare),
            "prefixed and bare paths must overlap"
        );
        assert_eq!(
            lease_resources(&with_prefix),
            lease_resources(&bare),
            "lease keys must agree between spellings"
        );
        assert!(tasks_overlap(
            &task(vec!["src/parser.rs".into()]),
            &task(vec!["src/".into()])
        ));
        assert!(!tasks_overlap(
            &task(vec!["src/parser.rs".into()]),
            &task(vec!["tests/parser.rs".into()])
        ));
    }

    #[test]
    fn deterministic_complexity_routes_only_within_available_models() {
        let config = Config::default();
        let available = HashSet::from([config.models.lead.clone(), config.models.complex_lead.clone()]);
        let simple =
            routed_lead_model("fix one parser typo", &available, &config).expect("simple lead route");
        assert_eq!(simple.model, config.models.lead);
        assert_eq!(
            simple.degraded, None,
            "the preferred leader was available, so the route is not degraded"
        );
        let complex = routed_lead_model(
            "Redesign the distributed database schema, authentication security, concurrent protocol migration, and production release architecture",
            &available,
            &config,
        )
        .expect("complex lead route");
        assert_eq!(complex.model, config.models.complex_lead);
        assert_eq!(complex.degraded, None);
    }

    #[test]
    fn deepseek_only_and_failure_escalation_routes_are_supported() {
        let config = Config::default();
        let available = HashSet::from([
            "deepseek/deepseek-v4-flash".to_owned(),
            "deepseek/deepseek-v4-pro".to_owned(),
        ]);
        // No configured leadership slot is reachable, so leading is degraded to
        // the deterministic first candidate and the reason is reported rather
        // than the substitution happening silently.
        let lead = routed_lead_model("explain this parser", &available, &config)
            .expect("a DeepSeek-only account must still be able to lead");
        assert_eq!(lead.model, "deepseek/deepseek-v4-flash");
        assert!(
            lead.degraded
                .as_deref()
                .is_some_and(|reason| reason.contains("no configured leadership-capable model")),
            "a degraded leader route must explain itself: {lead:?}"
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
        let pool = fair_worker_models(&task, &available, &config);
        assert_eq!(
            pool,
            vec![
                "deepseek/deepseek-v4-flash".to_owned(),
                "deepseek/deepseek-v4-pro".to_owned(),
            ]
        );
        task.attempt = 2;
        task.last_error = Some("first attempt failed".into());
        assert_eq!(
            fair_worker_models(&task, &available, &config),
            pool,
            "difficulty may change worker prompts, but it cannot create vendor preference in WDRR"
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
    fn automatic_routing_honors_pins_cooldowns_and_worker_policy() {
        let temp = tempfile::tempdir().expect("routing workspace");
        let mut harness = fixture_harness(temp.path(), Vec::new());
        let run = harness
            .store
            .create_run("automatic routing", Mode::Batch)
            .expect("run");
        let now = chrono::Utc::now();
        let task = TaskRecord {
            run_id: run.id,
            task_id: "worker-policy".into(),
            objective: "inspect a bounded parser change".into(),
            paths: vec!["src/parser.rs".into()],
            dependencies: Vec::new(),
            state: PlanTaskState::Pending,
            assigned_agent_id: None,
            attempt: 0,
            max_attempts: 2,
            generation: 0,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let available = HashSet::from([
            "chatgpt/gpt-5.3-codex-spark".to_owned(),
            "deepseek/deepseek-v4-flash".to_owned(),
            "deepseek/deepseek-v4-pro".to_owned(),
        ]);
        let first_agent = EventAgentId::new();
        let first = harness
            .select_automatic_route(
                "worker",
                run.id,
                first_agent,
                "dispatch:routing:worker:one",
                fair_worker_models(&task, &available, &harness.config),
                100,
            )
            .expect("unknown provider telemetry remains routable");
        assert_eq!(first.model, "chatgpt/gpt-5.3-codex-spark");
        assert!(
            first
                .candidates
                .iter()
                .any(|candidate| candidate.model == "chatgpt/gpt-5.3-codex-spark"
                    && candidate.eligible
                    && candidate.health == ProviderHealthStatusV1::Unknown)
        );
        let pro = first
            .candidates
            .iter()
            .find(|candidate| candidate.model == "deepseek/deepseek-v4-pro")
            .expect("Pro must be visible in the routing receipt");
        assert!(!pro.eligible);
        assert!(pro.reason.contains("excluded"));

        harness
            .config
            .routing
            .pins
            .insert("worker".into(), "deepseek/deepseek-v4-flash".into());
        let audit = harness
            .select_automatic_route(
                "audit",
                run.id,
                EventAgentId::new(),
                "dispatch:routing:audit:pin",
                fair_audit_models(&available, &harness.config),
                100,
            )
            .expect("worker pin applies to audit lenses");
        assert_eq!(audit.model, "deepseek/deepseek-v4-flash");
        assert!(audit.user_pin);

        harness.config.routing.pins.clear();
        harness
            .store
            .record_provider_transient_failure(
                &harness.workspace_id,
                ProviderId::DeepSeek.key(),
                None,
                "fixture cooldown",
            )
            .expect("provider cooldown");
        let cooled = harness
            .select_automatic_route(
                "worker",
                run.id,
                EventAgentId::new(),
                "dispatch:routing:worker:cooldown",
                fair_worker_models(&task, &available, &harness.config),
                100,
            )
            .expect("other healthy-or-unknown provider remains eligible");
        assert_eq!(cooled.model, "chatgpt/gpt-5.3-codex-spark");
        assert!(cooled.candidates.iter().any(|candidate| {
            candidate.model == "deepseek/deepseek-v4-flash"
                && !candidate.eligible
                && candidate.reason.contains("cooldown")
        }));

        harness
            .config
            .routing
            .pins
            .insert("worker".into(), "deepseek/deepseek-v4-flash".into());
        harness.config.routing.providers.insert(
            ProviderId::DeepSeek,
            crate::config::RoutingProviderOverride {
                reserve: None,
                cooldown: Some(false),
            },
        );
        let bypassed = harness
            .select_automatic_route(
                "worker",
                run.id,
                EventAgentId::new(),
                "dispatch:routing:worker:bypass",
                fair_worker_models(&task, &available, &harness.config),
                100,
            )
            .expect("explicit user cooldown override");
        assert_eq!(bypassed.model, "deepseek/deepseek-v4-flash");
        assert_eq!(bypassed.cooldown_override, Some(false));
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
    fn one_use_approval_is_bound_to_the_exact_operation() {
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
                    approved_operation_once: Some(vec![
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        assert!(!harness.take_operation_approval(
            run_id,
            Some(vec![
                "gh".into(),
                "release".into(),
                "create".into(),
                "v1.0.0".into()
            ])
        ));
        assert!(!harness.take_operation_approval(
            run_id,
            Some(vec!["git".into(), "push".into(), "origin".into(), "main".into()])
        ));
        harness
            .controls
            .lock()
            .entry(run_id)
            .or_default()
            .approved_operation_once =
            Some(vec!["git".into(), "push".into(), "origin".into(), "main".into()]);
        assert!(harness.take_operation_approval(
            run_id,
            Some(vec!["git".into(), "push".into(), "origin".into(), "main".into()])
        ));
        assert!(!harness.take_operation_approval(
            run_id,
            Some(vec!["git".into(), "push".into(), "origin".into(), "main".into()])
        ));
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
            provider_balance_percent: Arc::new(Mutex::new(HashMap::new())),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
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
            .system_prompt("it doesn't work", "Mina, issue clarifier", true)
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
