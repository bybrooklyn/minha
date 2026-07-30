//! Private, typed coordination primitives for Minha's virtual office.
//!
//! This module deliberately contains only data and local validation.  A
//! transport, persistence layer, or scheduler can build on these types
//! without making the model-facing protocol dynamic.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

pub const HIVE_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_MESSAGE_LIMIT_BYTES: usize = 64 * 1024;

pub type AgentId = String;
pub type GroupId = String;
pub type RoomId = String;
pub type TaskId = String;
pub type LeaseId = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeRoomKind {
    Run,
    Direct,
    Temporary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficeRoomState {
    Open,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeRoom {
    pub schema_version: u16,
    pub id: RoomId,
    pub kind: OfficeRoomKind,
    pub purpose: String,
    pub owner: Option<AgentId>,
    pub state: OfficeRoomState,
    pub members: BTreeSet<AgentId>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closure_summary: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationKind {
    Finding,
    Decision,
    Blocker,
    Request,
    Progress,
    Handoff,
    ArtifactReference,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Recipient {
    Agent(AgentId),
    Group(GroupId),
    Leader,
    Manager,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Task,
    Evidence,
    Integration,
    Incident,
    Progress,
    Health,
    Handoff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Task,
    Evidence,
    Integration,
    Incident,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PrivateRoom {
    pub id: RoomId,
    pub name: String,
    pub members: BTreeSet<AgentId>,
    pub created_at: DateTime<Utc>,
}

impl PrivateRoom {
    pub fn new(id: impl Into<RoomId>, name: impl Into<String>, created_at: DateTime<Utc>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            members: BTreeSet::new(),
            created_at,
        }
    }

    pub fn add_member(&mut self, agent: impl Into<AgentId>) -> bool {
        self.members.insert(agent.into())
    }

    pub fn permits(&self, recipient: &Recipient) -> bool {
        match recipient {
            Recipient::Agent(agent) => self.members.contains(agent),
            Recipient::Group(_) | Recipient::Leader | Recipient::Manager => true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub kind: String,
    pub uri: String,
    pub digest: Option<String>,
}

impl ArtifactRef {
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        uri: impl Into<String>,
        digest: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            uri: uri.into(),
            digest,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub artifact: ArtifactRef,
    pub claim: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Ready,
    Working,
    Blocked,
    Degraded,
    Replaced,
    Offline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Proposed,
    Ready,
    Claimed,
    InProgress,
    Blocked,
    Complete,
    Failed,
    Abandoned,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: AgentId,
    pub state: AgentState,
    pub generation: u64,
    pub capabilities: BTreeSet<String>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub title: String,
    pub state: TaskState,
    pub owner: Option<AgentId>,
    pub generation: u64,
    pub evidence: Vec<EvidenceRef>,
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LeaseEvidence {
    pub observation_id: String,
    pub artifact: ArtifactRef,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeLease {
    pub id: LeaseId,
    pub task_id: TaskId,
    pub holder: AgentId,
    pub generation: u64,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub evidence: Vec<LeaseEvidence>,
}

impl OfficeLease {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    pub fn has_evidence(&self) -> bool {
        !self.evidence.is_empty()
    }

    pub fn renew(
        &mut self,
        holder: &str,
        generation: u64,
        evidence: LeaseEvidence,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<(), LeaseError> {
        self.validate(holder, generation, now)?;
        self.evidence.push(evidence);
        self.expires_at = now + duration;
        Ok(())
    }

    pub fn validate(&self, holder: &str, generation: u64, now: DateTime<Utc>) -> Result<(), LeaseError> {
        if self.is_expired(now) {
            return Err(LeaseError::Expired);
        }
        if self.holder != holder || self.generation != generation {
            return Err(LeaseError::Fenced);
        }
        if !self.has_evidence() {
            return Err(LeaseError::MissingEvidence);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LeaseError {
    #[error("lease is expired")]
    Expired,
    #[error("lease holder or generation is stale")]
    Fenced,
    #[error("lease requires at least one evidence reference")]
    MissingEvidence,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProgressObservation {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub sequence: u64,
    pub state: TaskState,
    pub completion_percent: f32,
    pub evidence: Vec<EvidenceRef>,
    pub observed_at: DateTime<Utc>,
}

impl ProgressObservation {
    pub fn is_valid(&self) -> bool {
        self.completion_percent.is_finite() && (0.0..=100.0).contains(&self.completion_percent)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConvergenceObservation {
    pub task_id: TaskId,
    pub revision: u64,
    pub progress: ProgressObservation,
    pub blockers: Vec<String>,
    pub converged: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnonymousHealthMetrics {
    pub sample_count: u64,
    pub healthy_count: u64,
    pub degraded_count: u64,
    pub blocked_count: u64,
    pub mean_latency_ms: Option<f64>,
    pub observed_at: DateTime<Utc>,
}

impl AnonymousHealthMetrics {
    pub fn health_ratio(&self) -> Option<f64> {
        let total = self.sample_count;
        (total > 0).then(|| self.healthy_count as f64 / total as f64)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplacementForensicHandoff {
    pub replaced_agent: AgentId,
    pub replacement_agent: AgentId,
    pub task_id: TaskId,
    pub reason: String,
    pub last_known_state: AgentState,
    pub lease: Option<OfficeLease>,
    pub artifacts: Vec<ArtifactRef>,
    pub observations: Vec<ProgressObservation>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HivePayload {
    Task {
        task: TaskRecord,
        ledger: LedgerKind,
    },
    Evidence {
        evidence: EvidenceRef,
        ledger: LedgerKind,
    },
    Integration {
        artifacts: Vec<ArtifactRef>,
        ledger: LedgerKind,
    },
    Incident {
        summary: String,
        artifacts: Vec<ArtifactRef>,
        ledger: LedgerKind,
    },
    Progress(ProgressObservation),
    Health(AnonymousHealthMetrics),
    Handoff(ReplacementForensicHandoff),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HiveMessage {
    pub schema_version: u16,
    pub id: String,
    pub room_id: RoomId,
    pub sender: AgentId,
    pub recipient: Recipient,
    pub kind: MessageKind,
    pub sent_at: DateTime<Utc>,
    pub payload: HivePayload,
}

impl HiveMessage {
    pub fn validate_size(&self, limit: usize) -> Result<usize, MessageError> {
        let bytes = serde_json::to_vec(self).map_err(|_| MessageError::NotSerializable)?;
        if bytes.len() > limit {
            return Err(MessageError::TooLarge {
                actual: bytes.len(),
                limit,
            });
        }
        Ok(bytes.len())
    }

    pub fn validate(&self, room: &PrivateRoom, limit: usize) -> Result<usize, MessageError> {
        if self.schema_version != HIVE_SCHEMA_VERSION {
            return Err(MessageError::UnsupportedVersion(self.schema_version));
        }
        let payload_kind = match &self.payload {
            HivePayload::Task { .. } => MessageKind::Task,
            HivePayload::Evidence { .. } => MessageKind::Evidence,
            HivePayload::Integration { .. } => MessageKind::Integration,
            HivePayload::Incident { .. } => MessageKind::Incident,
            HivePayload::Progress(_) => MessageKind::Progress,
            HivePayload::Health(_) => MessageKind::Health,
            HivePayload::Handoff(_) => MessageKind::Handoff,
        };
        if self.kind != payload_kind {
            return Err(MessageError::KindMismatch);
        }
        if self.room_id != room.id || !room.permits(&self.recipient) {
            return Err(MessageError::RecipientNotInRoom);
        }
        self.validate_size(limit)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MessageError {
    #[error("unsupported hive schema version: {0}")]
    UnsupportedVersion(u16),
    #[error("recipient is not permitted in the private room")]
    RecipientNotInRoom,
    #[error("message kind does not match its typed payload")]
    KindMismatch,
    #[error("message is {actual} bytes, exceeding the {limit}-byte limit")]
    TooLarge { actual: usize, limit: usize },
    #[error("message could not be serialized")]
    NotSerializable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(now: DateTime<Utc>) -> LeaseEvidence {
        LeaseEvidence {
            observation_id: "obs-1".into(),
            artifact: ArtifactRef::new("a-1", "test-log", "file://test.log", None),
            observed_at: now,
        }
    }

    #[test]
    fn lease_requires_fresh_generation_and_evidence() {
        let now = Utc::now();
        let mut lease = OfficeLease {
            id: "lease-1".into(),
            task_id: "task-1".into(),
            holder: "agent-1".into(),
            generation: 2,
            acquired_at: now,
            expires_at: now + Duration::minutes(5),
            evidence: vec![],
        };
        assert_eq!(
            lease.validate("agent-1", 2, now),
            Err(LeaseError::MissingEvidence)
        );
        lease.evidence.push(evidence(now));
        assert!(lease.validate("agent-1", 2, now).is_ok());
        assert_eq!(lease.validate("agent-1", 1, now), Err(LeaseError::Fenced));
        assert_eq!(
            lease.validate("agent-1", 2, now + Duration::minutes(6)),
            Err(LeaseError::Expired)
        );
    }

    #[test]
    fn messages_are_versioned_private_and_size_bounded() {
        let now = Utc::now();
        let mut room = PrivateRoom::new("room-1", "backend", now);
        room.add_member("agent-1");
        let message = HiveMessage {
            schema_version: HIVE_SCHEMA_VERSION,
            id: "msg-1".into(),
            room_id: room.id.clone(),
            sender: "agent-1".into(),
            recipient: Recipient::Agent("agent-1".into()),
            kind: MessageKind::Evidence,
            sent_at: now,
            payload: HivePayload::Evidence {
                evidence: EvidenceRef {
                    artifact: ArtifactRef::new("a", "log", "file://log", None),
                    claim: "tests pass".into(),
                    observed_at: now,
                },
                ledger: LedgerKind::Evidence,
            },
        };
        assert!(message.validate(&room, DEFAULT_MESSAGE_LIMIT_BYTES).is_ok());
        assert_eq!(
            message.validate(&room, 1),
            Err(MessageError::TooLarge {
                actual: serde_json::to_vec(&message)
                    .expect("test operation should succeed")
                    .len(),
                limit: 1
            })
        );
        assert_eq!(
            message
                .validate_size(DEFAULT_MESSAGE_LIMIT_BYTES)
                .expect("test operation should succeed"),
            serde_json::to_vec(&message)
                .expect("test operation should succeed")
                .len()
        );
    }

    #[test]
    fn observations_reject_nan_and_health_is_anonymous() {
        let observation = ProgressObservation {
            task_id: "task".into(),
            agent_id: "agent".into(),
            sequence: 1,
            state: TaskState::InProgress,
            completion_percent: f32::NAN,
            evidence: vec![],
            observed_at: Utc::now(),
        };
        assert!(!observation.is_valid());
        let health = AnonymousHealthMetrics {
            sample_count: 4,
            healthy_count: 3,
            degraded_count: 1,
            blocked_count: 0,
            mean_latency_ms: Some(12.0),
            observed_at: Utc::now(),
        };
        assert_eq!(health.health_ratio(), Some(0.75));
    }

    #[test]
    fn serde_round_trip_preserves_typed_message() {
        let now = Utc::now();
        let message = HiveMessage {
            schema_version: HIVE_SCHEMA_VERSION,
            id: "msg".into(),
            room_id: "room".into(),
            sender: "agent".into(),
            recipient: Recipient::Manager,
            kind: MessageKind::Incident,
            sent_at: now,
            payload: HivePayload::Incident {
                summary: "blocked".into(),
                artifacts: vec![],
                ledger: LedgerKind::Incident,
            },
        };
        let encoded = serde_json::to_string(&message).expect("test operation should succeed");
        assert_eq!(
            serde_json::from_str::<HiveMessage>(&encoded).expect("test operation should succeed"),
            message
        );
    }
}
