//! Versioned runtime protocol shared by the harness, persistence, CLI, and TUI.
//!
//! Events are deliberately typed. Rendering code never infers agent state or
//! plans from prose, and persisted sessions can be replayed without calling a
//! model.

use crate::fairness::{FairnessSelectionV1, ProviderHealthStatusV1};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{fmt, str::FromStr};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MIN_TYPED_PROTOCOL_VERSION: u16 = 1;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(value.parse()?))
            }
        }
    };
}

id_type!(RunId);
id_type!(TurnId);
id_type!(EventAgentId);
id_type!(ItemId);
id_type!(RequestId);

/// SessionId is the v2 name. RunId remains the public alias for CLI and store
/// compatibility with the prototype.
pub type SessionId = RunId;

impl From<Uuid> for RunId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}
impl From<RunId> for Uuid {
    fn from(value: RunId) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Interactive,
    Batch,
    Review,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Interactive => "interactive",
            Self::Batch => "batch",
            Self::Review => "review",
        })
    }
}
impl FromStr for Mode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "interactive" => Ok(Self::Interactive),
            "batch" => Ok(Self::Batch),
            "review" => Ok(Self::Review),
            _ => Err(format!("unknown mode: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
    Inconclusive,
    NeedsInput,
    UsagePaused,
    ApprovalRequired,
    AuthUnavailable,
    ModelUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Inspecting,
    Planning,
    Working,
    Waiting,
    Integrating,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskState {
    Pending,
    Running,
    Completed,
    Blocked,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub objective: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub state: PlanTaskState,
    pub agent_id: Option<EventAgentId>,
}

/// Contract schema for a single coordinator-admitted microtask.  This stays
/// deliberately small because it is both durable coordination state and the
/// worker's task packet.
pub const MICROTASK_CONTRACT_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MicrotaskContractV1 {
    pub schema_version: u16,
    pub task_id: String,
    pub goal: String,
    #[serde(default)]
    pub lease_resources: Vec<String>,
    pub acceptance_check: String,
}

/// One candidate considered at an explicit dispatch boundary.  An exclusion
/// is always stated rather than being hidden behind a provider preference.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoutingCandidateV1 {
    pub provider: String,
    pub model: String,
    pub eligible: bool,
    #[serde(default)]
    pub reason: String,
}

/// Persisted explanation for one worker assignment.  It is an audit record,
/// not a learned routing cache or a prompt-visible transcript.
pub const DISPATCH_RECEIPT_SCHEMA_VERSION: u16 = 1;
/// Current durable dispatch receipt schema. `DispatchReceiptV1` remains the
/// event/TUI compatibility projection while SQLite may retain the richer V2
/// record for routing inspection.
pub const DISPATCH_RECEIPT_V2_SCHEMA_VERSION: u16 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchReceiptV1 {
    pub schema_version: u16,
    pub receipt_id: String,
    pub task_id: String,
    pub generation: u64,
    pub agent_id: EventAgentId,
    pub role: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub candidates: Vec<RoutingCandidateV1>,
    #[serde(default)]
    pub lease_resources: Vec<String>,
    pub acceptance_check: String,
    pub estimated_input_tokens: u64,
    pub session_used_tokens: u64,
    pub session_target_tokens: u64,
    pub budget_pressure: String,
    pub parallelism_reason: String,
    #[serde(default)]
    pub book_sources: Vec<String>,
    pub issued_at: DateTime<Utc>,
}

/// V2 adds compact, local routing evidence without enlarging the model-facing
/// worker packet. It is intentionally a separate type: existing event readers
/// and TUI state can continue to decode `DispatchReceiptV1` unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoutingCandidateV2 {
    pub provider: String,
    pub model: String,
    pub eligible: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub health: ProviderHealthStatusV1,
    #[serde(default)]
    pub cooldown_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reserve: String,
    #[serde(default)]
    pub pinned: bool,
}

impl From<RoutingCandidateV1> for RoutingCandidateV2 {
    fn from(candidate: RoutingCandidateV1) -> Self {
        Self {
            provider: candidate.provider,
            model: candidate.model,
            eligible: candidate.eligible,
            reason: candidate.reason,
            health: ProviderHealthStatusV1::Unknown,
            cooldown_until: None,
            reserve: String::new(),
            pinned: false,
        }
    }
}

impl From<RoutingCandidateV2> for RoutingCandidateV1 {
    fn from(candidate: RoutingCandidateV2) -> Self {
        Self {
            provider: candidate.provider,
            model: candidate.model,
            eligible: candidate.eligible,
            reason: candidate.reason,
        }
    }
}

/// Compact explanation of how policy and equal-weight WDRR admitted a route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchRoutingV1 {
    pub schema_version: u16,
    pub policy: String,
    pub quantum: u64,
    pub estimated_work: u64,
    pub deficit_before: i64,
    pub deficit_after: i64,
    #[serde(default)]
    pub user_pin: bool,
    #[serde(default)]
    pub reserve_override: Option<bool>,
    #[serde(default)]
    pub cooldown_override: Option<bool>,
    #[serde(default)]
    pub health: ProviderHealthStatusV1,
}

impl Default for DispatchRoutingV1 {
    fn default() -> Self {
        Self {
            schema_version: 1,
            policy: "legacy_untracked".into(),
            quantum: 0,
            estimated_work: 0,
            deficit_before: 0,
            deficit_after: 0,
            user_pin: false,
            reserve_override: None,
            cooldown_override: None,
            health: ProviderHealthStatusV1::Unknown,
        }
    }
}

impl DispatchRoutingV1 {
    pub fn equal_weight(selection: FairnessSelectionV1) -> Self {
        Self {
            schema_version: 1,
            policy: "equal_weight_wdrr".into(),
            quantum: selection.quantum,
            estimated_work: selection.estimated_work,
            deficit_before: selection.deficit_before,
            deficit_after: selection.deficit_after,
            ..Self::default()
        }
    }
}

/// Current durable dispatch receipt. Its custom decoder upcasts literal V1
/// JSON, so old rows remain readable after V2 is introduced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DispatchReceiptV2 {
    pub schema_version: u16,
    pub receipt_id: String,
    pub task_id: String,
    pub generation: u64,
    pub agent_id: EventAgentId,
    pub role: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub candidates: Vec<RoutingCandidateV2>,
    #[serde(default)]
    pub lease_resources: Vec<String>,
    pub acceptance_check: String,
    pub estimated_input_tokens: u64,
    pub session_used_tokens: u64,
    pub session_target_tokens: u64,
    pub budget_pressure: String,
    pub parallelism_reason: String,
    #[serde(default)]
    pub book_sources: Vec<String>,
    pub issued_at: DateTime<Utc>,
    #[serde(default)]
    pub routing: DispatchRoutingV1,
}

impl DispatchReceiptV2 {
    pub fn from_v1(receipt: DispatchReceiptV1) -> Self {
        Self {
            schema_version: DISPATCH_RECEIPT_V2_SCHEMA_VERSION,
            receipt_id: receipt.receipt_id,
            task_id: receipt.task_id,
            generation: receipt.generation,
            agent_id: receipt.agent_id,
            role: receipt.role,
            provider: receipt.provider,
            model: receipt.model,
            candidates: receipt.candidates.into_iter().map(Into::into).collect(),
            lease_resources: receipt.lease_resources,
            acceptance_check: receipt.acceptance_check,
            estimated_input_tokens: receipt.estimated_input_tokens,
            session_used_tokens: receipt.session_used_tokens,
            session_target_tokens: receipt.session_target_tokens,
            budget_pressure: receipt.budget_pressure,
            parallelism_reason: receipt.parallelism_reason,
            book_sources: receipt.book_sources,
            issued_at: receipt.issued_at,
            routing: DispatchRoutingV1::default(),
        }
    }

    /// The compatibility projection used by the existing typed runtime event
    /// and TUI. V2-only fields remain durable in SQLite and queryable through
    /// the Store routing inspector.
    pub fn to_v1(&self) -> DispatchReceiptV1 {
        DispatchReceiptV1 {
            schema_version: DISPATCH_RECEIPT_SCHEMA_VERSION,
            receipt_id: self.receipt_id.clone(),
            task_id: self.task_id.clone(),
            generation: self.generation,
            agent_id: self.agent_id,
            role: self.role.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            candidates: self.candidates.clone().into_iter().map(Into::into).collect(),
            lease_resources: self.lease_resources.clone(),
            acceptance_check: self.acceptance_check.clone(),
            estimated_input_tokens: self.estimated_input_tokens,
            session_used_tokens: self.session_used_tokens,
            session_target_tokens: self.session_target_tokens,
            budget_pressure: self.budget_pressure.clone(),
            parallelism_reason: self.parallelism_reason.clone(),
            book_sources: self.book_sources.clone(),
            issued_at: self.issued_at,
        }
    }
}

impl From<DispatchReceiptV1> for DispatchReceiptV2 {
    fn from(receipt: DispatchReceiptV1) -> Self {
        Self::from_v1(receipt)
    }
}

#[derive(Deserialize)]
struct DispatchReceiptV2Wire {
    schema_version: u16,
    receipt_id: String,
    task_id: String,
    generation: u64,
    agent_id: EventAgentId,
    role: String,
    provider: String,
    model: String,
    #[serde(default)]
    candidates: Vec<RoutingCandidateV2>,
    #[serde(default)]
    lease_resources: Vec<String>,
    acceptance_check: String,
    estimated_input_tokens: u64,
    session_used_tokens: u64,
    session_target_tokens: u64,
    budget_pressure: String,
    parallelism_reason: String,
    #[serde(default)]
    book_sources: Vec<String>,
    issued_at: DateTime<Utc>,
    #[serde(default)]
    routing: DispatchRoutingV1,
}

impl From<DispatchReceiptV2Wire> for DispatchReceiptV2 {
    fn from(receipt: DispatchReceiptV2Wire) -> Self {
        Self {
            schema_version: receipt.schema_version,
            receipt_id: receipt.receipt_id,
            task_id: receipt.task_id,
            generation: receipt.generation,
            agent_id: receipt.agent_id,
            role: receipt.role,
            provider: receipt.provider,
            model: receipt.model,
            candidates: receipt.candidates,
            lease_resources: receipt.lease_resources,
            acceptance_check: receipt.acceptance_check,
            estimated_input_tokens: receipt.estimated_input_tokens,
            session_used_tokens: receipt.session_used_tokens,
            session_target_tokens: receipt.session_target_tokens,
            budget_pressure: receipt.budget_pressure,
            parallelism_reason: receipt.parallelism_reason,
            book_sources: receipt.book_sources,
            issued_at: receipt.issued_at,
            routing: receipt.routing,
        }
    }
}

impl<'de> Deserialize<'de> for DispatchReceiptV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let version = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| serde::de::Error::custom("dispatch receipt schema_version is required"))?;
        match version {
            1 => serde_json::from_value::<DispatchReceiptV1>(value)
                .map(Self::from_v1)
                .map_err(serde::de::Error::custom),
            2 => serde_json::from_value::<DispatchReceiptV2Wire>(value)
                .map(Into::into)
                .map_err(serde::de::Error::custom),
            version => Err(serde::de::Error::custom(format!(
                "unsupported dispatch receipt schema {version}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Dropped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub objective: String,
    pub state: TodoState,
    pub order: u32,
    pub blocker: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Queued,
    Preflight,
    Clarifying,
    Routing,
    Planning,
    Scheduling,
    Working,
    Compacting,
    Retrying,
    Integrating,
    Judging,
    Waiting,
    Recovering,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    Completed,
    BudgetTarget,
    ContextBoundary,
    ToolLimit,
    TurnLimit,
    ProviderReserve,
    SafetyPolicy,
    Interrupted,
    UserPaused,
    Blocked,
    RetryScheduled,
    Forked,
    RecoveryRequired,
    InvalidEmptyResponse,
    ProviderFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Defect,
    Feature,
    Audit,
    Review,
    Question,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationStatus {
    Collecting,
    Reviewing,
    Confirmed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionStatus {
    Unknown,
    Partial,
    Inferred,
    Delegated,
    Confirmed,
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AmbiguityDimension {
    pub id: String,
    pub label: String,
    pub weight: u8,
    pub status: DimensionStatus,
    #[serde(default)]
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AmbiguityMeter {
    pub overall: u8,
    pub dimensions: Vec<AmbiguityDimension>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClarificationOption {
    pub value: String,
    pub label: String,
    pub description: String,
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClarificationQuestion {
    pub id: String,
    pub dimension: String,
    pub header: String,
    pub question: String,
    pub options: Vec<ClarificationOption>,
    #[serde(default = "default_true")]
    pub allow_free_text: bool,
    #[serde(default = "default_true")]
    pub allow_not_sure: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClarificationBatch {
    pub round: u32,
    pub questions: Vec<ClarificationQuestion>,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IssueBrief {
    pub issue_kind: IssueKind,
    pub observed: String,
    pub expected: String,
    #[serde(default)]
    pub reproduction: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    pub recommended_workflow: String,
    pub meter: AmbiguityMeter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IssueClarificationView {
    pub schema_version: u16,
    pub status: ClarificationStatus,
    pub issue_kind: IssueKind,
    pub round: u32,
    pub meter: AmbiguityMeter,
    pub pending_batch: Option<ClarificationBatch>,
    pub brief: Option<IssueBrief>,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IncidentView {
    pub code: String,
    pub severity: IncidentSeverity,
    pub category: String,
    pub summary: String,
    pub retryable: bool,
    pub correlation_id: String,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    #[serde(default = "default_chatgpt_provider")]
    pub provider: String,
    pub slug: String,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub maximum_output: Option<u64>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    #[serde(default)]
    pub capability_source: String,
    #[serde(default)]
    pub pricing: Option<Value>,
    #[serde(default)]
    pub capability_fetched_at: Option<DateTime<Utc>>,
}

fn default_chatgpt_provider() -> String {
    "chatgpt_codex".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardEntryView {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub task_id: Option<String>,
    pub author_agent_id: Option<EventAgentId>,
    pub confidence: u8,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RuntimeEvent {
    SessionStarted {
        kind: String,
        goal: String,
    },
    SessionResumed,
    SessionForked {
        source: RunId,
    },
    SessionRenamed {
        title: String,
    },
    SessionArchived,
    SessionState {
        state: ExitState,
    },
    RunPhase {
        phase: RunPhase,
        detail: String,
    },
    RoutingDecision {
        mode: String,
        reason: String,
        provider: String,
        model: Option<String>,
    },
    ModelCatalog {
        models: Vec<CatalogModel>,
        fetched_at: DateTime<Utc>,
        cached: bool,
    },
    UserMessage {
        text: String,
        steering: bool,
    },
    AgentStarted {
        agent_id: EventAgentId,
        role: String,
        model: String,
        parent: Option<EventAgentId>,
    },
    AgentState {
        agent_id: EventAgentId,
        state: AgentState,
        detail: String,
    },
    TextDelta {
        agent_id: EventAgentId,
        item_id: ItemId,
        delta: String,
    },
    AssistantMessage {
        agent_id: EventAgentId,
        item_id: ItemId,
        role: String,
        model: String,
        text: String,
    },
    PlanCreated {
        summary: String,
        tasks: Vec<PlanTask>,
    },
    PlanTaskChanged {
        task_id: String,
        state: PlanTaskState,
        agent_id: Option<EventAgentId>,
    },
    TodoChanged {
        agent_id: EventAgentId,
        item: TodoItem,
    },
    TodoRollupChanged {
        active: u64,
        blocked: u64,
        completed: u64,
        stale_agents: u64,
        #[serde(default)]
        active_goals: Vec<String>,
        #[serde(default)]
        blocked_work: Vec<String>,
        #[serde(default)]
        recently_completed: Vec<String>,
    },
    ActivityStarted {
        activity_id: String,
        agent_id: Option<EventAgentId>,
        kind: String,
        summary: String,
    },
    ActivityUpdated {
        activity_id: String,
        detail: String,
    },
    ActivityFinished {
        activity_id: String,
        summary: String,
        succeeded: bool,
        duration_ms: u64,
    },
    MemoryChanged {
        memory_id: String,
        action: String,
        scope: String,
    },
    MemoryRetrieved {
        agent_id: EventAgentId,
        memory_ids: Vec<String>,
        estimated_tokens: u64,
    },
    ProviderState {
        provider: String,
        enabled: bool,
        healthy: Option<bool>,
        detail: String,
    },
    ProviderBalance {
        provider: String,
        available: bool,
        currency: String,
        total: String,
        granted: String,
        topped_up: String,
        reserve_percent: Option<f64>,
    },
    AgentRetry {
        task_id: String,
        attempt: u32,
        reason: String,
    },
    LeaseChanged {
        task_id: String,
        agent_id: EventAgentId,
        generation: u64,
        resources: Vec<String>,
        acquired: bool,
    },
    DispatchReceipt {
        receipt: DispatchReceiptV1,
    },
    BoardChanged {
        entry: BoardEntryView,
    },
    ToolStarted {
        agent_id: EventAgentId,
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolOutput {
        agent_id: EventAgentId,
        call_id: String,
        name: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        truncated: bool,
    },
    FileChange {
        agent_id: Option<EventAgentId>,
        path: Option<String>,
        diff: String,
    },
    Question {
        request_id: RequestId,
        agent_id: EventAgentId,
        question: String,
        options: Vec<String>,
        blocking: bool,
    },
    ClarificationStarted {
        clarification: IssueClarificationView,
    },
    ClarificationUpdated {
        clarification: IssueClarificationView,
    },
    ClarificationConfirmed {
        brief: IssueBrief,
    },
    Approval {
        request_id: RequestId,
        agent_id: EventAgentId,
        reason: String,
        command: Option<Vec<String>>,
    },
    RequestResolved {
        request_id: RequestId,
        answer: String,
        approved: Option<bool>,
    },
    SteeringQueued {
        text: String,
    },
    SteeringApplied {
        agent_id: EventAgentId,
        text: String,
    },
    Usage {
        #[serde(default)]
        agent_id: Option<EventAgentId>,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cached_input_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
        #[serde(default)]
        reasoning_output_tokens: u64,
    },
    ContextUsage {
        agent_id: EventAgentId,
        model: String,
        estimated_tokens: u64,
        advertised_limit: u64,
        effective_limit: u64,
        forecast_tokens: u64,
        output_allowance: u64,
        protected_reserve: u64,
        capability_source: String,
    },
    AccountUsage {
        snapshot: Value,
    },
    Cache {
        hit: bool,
        class: String,
        key_prefix: String,
        bytes: u64,
        saved_input_tokens: u64,
    },
    OfficeHealth {
        active_agents: u64,
        open_tasks: u64,
        blocked_tasks: u64,
        manager_consultations: u64,
    },
    OfficeRoomChanged {
        room_id: String,
        kind: String,
        state: String,
        purpose: String,
    },
    OfficeMessageChanged {
        message_id: String,
        room_id: String,
        sender: String,
        recipient: String,
        kind: String,
        summary: String,
        deduplicated: bool,
    },
    BookCatalog {
        indexed: u64,
        stale: u64,
        updates_pending: u64,
    },
    Incident {
        incident: IncidentView,
    },
    Compacted {
        summary: String,
        estimated_tokens_before: usize,
    },
    SequentialFallback {
        reason: String,
    },
    TurnInterrupted {
        reason: String,
    },
    RunStopped {
        reason: TerminationReason,
        detail: String,
    },
    SessionFinished {
        state: ExitState,
        model: Option<String>,
        text: String,
        agents_used: usize,
    },
    Warning {
        message: String,
    },
    Error {
        state: ExitState,
        message: String,
    },
    /// Compatibility only for old call sites. New behavior should add a typed
    /// variant rather than teaching the UI to inspect this payload.
    Legacy {
        kind: String,
        payload: Value,
    },
}

impl RuntimeEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session.started",
            Self::SessionResumed => "session.resumed",
            Self::SessionForked { .. } => "session.forked",
            Self::SessionRenamed { .. } => "session.renamed",
            Self::SessionArchived => "session.archived",
            Self::SessionState { .. } => "session.state",
            Self::RunPhase { .. } => "run.phase",
            Self::RoutingDecision { .. } => "routing.decision",
            Self::ModelCatalog { .. } => "model.catalog",
            Self::UserMessage { .. } => "message.user",
            Self::AgentStarted { .. } => "agent.started",
            Self::AgentState { .. } => "agent.state",
            Self::TextDelta { .. } => "message.delta",
            Self::AssistantMessage { .. } => "message.assistant",
            Self::PlanCreated { .. } => "plan.created",
            Self::PlanTaskChanged { .. } => "plan.task_changed",
            Self::TodoChanged { .. } => "todo.changed",
            Self::TodoRollupChanged { .. } => "todo.rollup_changed",
            Self::ActivityStarted { .. } => "activity.started",
            Self::ActivityUpdated { .. } => "activity.updated",
            Self::ActivityFinished { .. } => "activity.finished",
            Self::MemoryChanged { .. } => "memory.changed",
            Self::MemoryRetrieved { .. } => "memory.retrieved",
            Self::ProviderState { .. } => "provider.state",
            Self::ProviderBalance { .. } => "provider.balance",
            Self::AgentRetry { .. } => "agent.retry",
            Self::LeaseChanged { .. } => "lease.changed",
            Self::DispatchReceipt { .. } => "dispatch.receipt",
            Self::BoardChanged { .. } => "board.changed",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolOutput { .. } => "tool.output",
            Self::FileChange { .. } => "file.change",
            Self::Question { .. } => "request.question",
            Self::ClarificationStarted { .. } => "clarification.started",
            Self::ClarificationUpdated { .. } => "clarification.updated",
            Self::ClarificationConfirmed { .. } => "clarification.confirmed",
            Self::Approval { .. } => "request.approval",
            Self::RequestResolved { .. } => "request.resolved",
            Self::SteeringQueued { .. } => "steering.queued",
            Self::SteeringApplied { .. } => "steering.applied",
            Self::Usage { .. } => "usage.turn",
            Self::ContextUsage { .. } => "usage.context",
            Self::AccountUsage { .. } => "usage.account",
            Self::Cache { .. } => "cache.lookup",
            Self::OfficeHealth { .. } => "office.health",
            Self::OfficeRoomChanged { .. } => "office.room_changed",
            Self::OfficeMessageChanged { .. } => "office.message_changed",
            Self::BookCatalog { .. } => "books.catalog",
            Self::Incident { .. } => "incident",
            Self::Compacted { .. } => "context.compacted",
            Self::SequentialFallback { .. } => "swarm.sequential_fallback",
            Self::TurnInterrupted { .. } => "turn.interrupted",
            Self::RunStopped { .. } => "run.stopped",
            Self::SessionFinished { .. } => "session.finished",
            Self::Warning { .. } => "warning",
            Self::Error { .. } => "error",
            Self::Legacy { .. } => "legacy",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub protocol_version: u16,
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub sequence: u64,
    pub event: RuntimeEvent,
    pub occurred_at: DateTime<Utc>,
}

impl EventEnvelope {
    pub fn new(run_id: RunId, sequence: u64, event: RuntimeEvent) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            run_id,
            turn_id: None,
            sequence,
            event,
            occurred_at: Utc::now(),
        }
    }

    pub fn kind(&self) -> &str {
        match &self.event {
            RuntimeEvent::Legacy { kind, .. } => kind,
            event => event.kind(),
        }
    }

    pub fn payload(&self) -> Value {
        match &self.event {
            RuntimeEvent::Legacy { payload, .. } => payload.clone(),
            event => serde_json::to_value(event).unwrap_or(Value::Null),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RuntimeCommand {
    StartTurn {
        kind: String,
        text: String,
    },
    QueueSteering {
        run_id: RunId,
        text: String,
    },
    Interrupt {
        run_id: RunId,
    },
    Answer {
        run_id: RunId,
        request_id: Option<RequestId>,
        text: String,
    },
    ResolveApproval {
        run_id: RunId,
        request_id: RequestId,
        approved: bool,
    },
    Resume {
        run_id: RunId,
    },
    Fork {
        run_id: RunId,
    },
    Rename {
        run_id: RunId,
        title: String,
    },
    Archive {
        run_id: RunId,
    },
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_types_round_trip() {
        let id = RunId::new();
        let event = EventEnvelope::new(
            id,
            3,
            RuntimeEvent::SessionStarted {
                kind: "implement".into(),
                goal: "fix it".into(),
            },
        );
        let encoded = serde_json::to_string(&event).expect("test operation should succeed");
        let decoded: EventEnvelope = serde_json::from_str(&encoded).expect("test operation should succeed");
        assert_eq!(decoded, event);
        assert_eq!(decoded.kind(), "session.started");
        assert_eq!(Mode::Batch.to_string().parse(), Ok(Mode::Batch));
    }

    #[test]
    fn literal_v1_dispatch_receipt_upcasts_to_v2() {
        let agent_id = EventAgentId::new();
        let literal_v1 = format!(
            r#"{{"schema_version":1,"receipt_id":"dispatch:synthetic:parser:0:agent","task_id":"parser","generation":0,"agent_id":"{agent_id}","role":"Spark worker parser","provider":"deepseek","model":"deepseek/deepseek-v4-flash","lease_resources":["path:src/parser.rs"],"acceptance_check":"synthetic check","estimated_input_tokens":240,"session_used_tokens":12,"session_target_tokens":100000,"budget_pressure":"normal","parallelism_reason":"synthetic test route","issued_at":"2026-08-01T00:00:00Z"}}"#
        );
        let receipt: DispatchReceiptV2 =
            serde_json::from_str(&literal_v1).expect("V1 receipt remains readable");
        assert_eq!(receipt.schema_version, DISPATCH_RECEIPT_V2_SCHEMA_VERSION);
        assert_eq!(receipt.routing, DispatchRoutingV1::default());
        assert_eq!(receipt.to_v1().schema_version, DISPATCH_RECEIPT_SCHEMA_VERSION);
        assert_eq!(receipt.to_v1().receipt_id, "dispatch:synthetic:parser:0:agent");
    }
}
