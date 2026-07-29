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

pub const PROTOCOL_VERSION: u16 = 4;
pub const MIN_TYPED_PROTOCOL_VERSION: u16 = 2;

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
pub enum RunPhase {
    Preflight,
    Routing,
    Planning,
    Scheduling,
    Working,
    Integrating,
    Judging,
    Waiting,
    Recovering,
    Complete,
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
    pub slug: String,
    pub context_window: Option<u64>,
    pub supports_parallel_tool_calls: bool,
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
        context_limit: u64,
        compact_at_tokens: u64,
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
            Self::ModelCatalog { .. } => "model.catalog",
            Self::UserMessage { .. } => "message.user",
            Self::AgentStarted { .. } => "agent.started",
            Self::AgentState { .. } => "agent.state",
            Self::TextDelta { .. } => "message.delta",
            Self::AssistantMessage { .. } => "message.assistant",
            Self::PlanCreated { .. } => "plan.created",
            Self::PlanTaskChanged { .. } => "plan.task_changed",
            Self::AgentRetry { .. } => "agent.retry",
            Self::LeaseChanged { .. } => "lease.changed",
            Self::BoardChanged { .. } => "board.changed",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolOutput { .. } => "tool.output",
            Self::FileChange { .. } => "file.change",
            Self::Question { .. } => "request.question",
            Self::Approval { .. } => "request.approval",
            Self::RequestResolved { .. } => "request.resolved",
            Self::SteeringQueued { .. } => "steering.queued",
            Self::SteeringApplied { .. } => "steering.applied",
            Self::Usage { .. } => "usage.turn",
            Self::ContextUsage { .. } => "usage.context",
            Self::AccountUsage { .. } => "usage.account",
            Self::Cache { .. } => "cache.lookup",
            Self::OfficeHealth { .. } => "office.health",
            Self::BookCatalog { .. } => "books.catalog",
            Self::Incident { .. } => "incident",
            Self::Compacted { .. } => "context.compacted",
            Self::SequentialFallback { .. } => "swarm.sequential_fallback",
            Self::TurnInterrupted { .. } => "turn.interrupted",
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
