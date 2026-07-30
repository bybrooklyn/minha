//! Versioned runtime protocol shared by the harness, persistence, CLI, and TUI.
//!
//! Events are deliberately typed. Rendering code never infers agent state or
//! plans from prose, and persisted sessions can be replayed without calling a
//! model.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
    ContextBoundary,
    ToolLimit,
    TurnLimit,
    ProviderReserve,
    Interrupted,
    UserPaused,
    Blocked,
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
}
