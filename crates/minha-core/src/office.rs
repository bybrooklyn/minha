//! Private, typed coordination primitives for Minha's virtual office.
//!
//! This module deliberately contains only data and local validation.  A
//! transport, persistence layer, or scheduler can build on these types
//! without making the model-facing protocol dynamic.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use thiserror::Error;

pub const HIVE_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_MESSAGE_LIMIT_BYTES: usize = 64 * 1024;

/// The implicit room every agent in a run belongs to.
///
/// Membership in this room is not stored: being part of the run *is* the
/// membership.  Every other room id names a private room whose membership is
/// recorded in `office_room_members` and enforced on both the send and the
/// inbox path (see `Store::insert_hive_message` / `Store::hive_inbox`).
pub const RUN_ROOM_ID: &str = "run";

/// The wire recipient that addresses every member of the room a message is
/// posted into, rather than one named agent.
pub const BROADCAST_RECIPIENT: &str = "group:all";

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

impl CoordinationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Decision => "decision",
            Self::Blocker => "blocker",
            Self::Request => "request",
            Self::Progress => "progress",
            Self::Handoff => "handoff",
            Self::ArtifactReference => "artifact_reference",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "finding" => Some(Self::Finding),
            "decision" => Some(Self::Decision),
            "blocker" => Some(Self::Blocker),
            "request" => Some(Self::Request),
            "progress" => Some(Self::Progress),
            "handoff" => Some(Self::Handoff),
            "artifact_reference" => Some(Self::ArtifactReference),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum Recipient {
    Agent(AgentId),
    Group(GroupId),
    Leader,
    Manager,
}

impl Recipient {
    /// Parse the wire form used by `hive_messages.recipient` and
    /// `office_room_members.member_id`.
    ///
    /// This is deliberately strict: it is the single definition of the address
    /// format, so the executor's `to` normalization and the store's room
    /// scoping cannot drift apart.  Anything that is not a recognized address
    /// returns `None` rather than being coerced into a plausible-looking key
    /// that matches no inbox.
    pub fn parse(wire: &str) -> Option<Self> {
        let wire = wire.trim();
        if let Some(agent) = wire.strip_prefix("agent:") {
            return (!agent.is_empty()).then(|| Self::Agent(agent.to_owned()));
        }
        if let Some(group) = wire.strip_prefix("group:") {
            return (!group.is_empty()).then(|| Self::Group(group.to_owned()));
        }
        match wire {
            "leader" => Some(Self::Leader),
            "manager" => Some(Self::Manager),
            _ => None,
        }
    }

    /// Render the wire form. `Recipient::parse(&r.to_wire()) == Some(r)`.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Agent(agent) => format!("agent:{agent}"),
            Self::Group(group) => format!("group:{group}"),
            Self::Leader => "leader".to_owned(),
            Self::Manager => "manager".to_owned(),
        }
    }

    /// The bare agent id, for the `Agent` variant only.
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Agent(agent) => Some(agent.as_str()),
            _ => None,
        }
    }
}

impl std::fmt::Display for Recipient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_wire())
    }
}

/// Keeps the canonical envelope compact on the wire while retaining the
/// already-validated `Recipient` enum in Rust.
mod recipient_wire {
    use super::Recipient;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub fn serialize<S>(recipient: &Recipient, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&recipient.to_wire())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Recipient, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = String::deserialize(deserializer)?;
        Recipient::parse(&wire).ok_or_else(|| D::Error::custom("invalid office recipient"))
    }
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

    /// Build a room from the membership rows the store holds, which are stored
    /// in [`Recipient`] wire form. Non-agent members (`leader`, `manager`,
    /// groups) are always permitted, so only agent addresses are retained.
    pub fn from_wire_members<'a>(
        id: impl Into<RoomId>,
        name: impl Into<String>,
        created_at: DateTime<Utc>,
        members: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut room = Self::new(id, name, created_at);
        for member in members {
            if let Some(Recipient::Agent(agent)) = Recipient::parse(member) {
                room.add_member(agent);
            }
        }
        room
    }

    pub fn add_member(&mut self, agent: impl Into<AgentId>) -> bool {
        self.members.insert(agent.into())
    }

    /// Whether `recipient` may participate in this room.
    ///
    /// Named agents must be members. Groups, the leader, and the manager are
    /// always permitted: a group address is scoped by the room it is posted
    /// into (the inbox path filters group traffic by room membership), and
    /// leader/manager are run-level addresses rather than room members.
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

/// A compact durable replacement for raw tool output in a worker packet or
/// coordinator delta. Raw evidence stays in an artifact; this is only the
/// smallest useful conclusion, locator, digest, and excerpt.
pub const EVIDENCE_RECEIPT_SCHEMA_VERSION: u16 = 1;
pub const MAX_EVIDENCE_RECEIPT_EXCERPT_BYTES: usize = 480;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvidenceReceiptV1 {
    pub schema_version: u16,
    pub conclusion: String,
    /// Exact path/symbol or command/result location.
    pub locator: String,
    pub evidence_digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
}

impl EvidenceReceiptV1 {
    pub fn validate(&self) -> Result<(), EvidenceReceiptError> {
        if self.schema_version != EVIDENCE_RECEIPT_SCHEMA_VERSION {
            return Err(EvidenceReceiptError::UnsupportedVersion(self.schema_version));
        }
        if self.conclusion.trim().is_empty() {
            return Err(EvidenceReceiptError::MissingConclusion);
        }
        if self.locator.trim().is_empty() {
            return Err(EvidenceReceiptError::MissingLocator);
        }
        if self.evidence_digest.trim().is_empty() {
            return Err(EvidenceReceiptError::MissingDigest);
        }
        if self.excerpt.len() > MAX_EVIDENCE_RECEIPT_EXCERPT_BYTES {
            return Err(EvidenceReceiptError::ExcerptTooLarge {
                actual: self.excerpt.len(),
                limit: MAX_EVIDENCE_RECEIPT_EXCERPT_BYTES,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EvidenceReceiptError {
    #[error("unsupported evidence receipt schema version: {0}")]
    UnsupportedVersion(u16),
    #[error("evidence receipt requires a conclusion")]
    MissingConclusion,
    #[error("evidence receipt requires an exact locator")]
    MissingLocator,
    #[error("evidence receipt requires an evidence digest")]
    MissingDigest,
    #[error("evidence receipt excerpt is {actual} bytes, exceeding the {limit}-byte limit")]
    ExcerptTooLarge { actual: usize, limit: usize },
}

/// A compact canonical office delta. New code should persist this typed form;
/// [`OfficeEnvelopeV1::from_legacy_raw`] exists only to read the historic
/// `hive_messages` JSON rows and deliberately has no matching raw writer.
pub const OFFICE_ENVELOPE_SCHEMA_VERSION: u16 = 1;
pub const MAX_OFFICE_SUMMARY_BYTES: usize = 1_200;
pub const MAX_OFFICE_EVIDENCE_RECEIPTS: usize = 8;
pub const MAX_OFFICE_ARTIFACT_REFS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OfficeEnvelopeV1 {
    pub schema_version: u16,
    pub id: String,
    pub room_id: RoomId,
    /// Canonical bare agent id. Legacy `agent:<id>` strings are normalized on
    /// decode so store and coordinator keys do not drift.
    pub sender: AgentId,
    #[serde(with = "recipient_wire")]
    pub recipient: Recipient,
    pub kind: CoordinationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub summary: String,
    /// Artifact IDs only; their content stays outside coordinator context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceReceiptV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_action: Option<String>,
    pub sent_at: DateTime<Utc>,
}

impl OfficeEnvelopeV1 {
    pub fn validate(&self) -> Result<(), OfficeEnvelopeError> {
        if self.schema_version != OFFICE_ENVELOPE_SCHEMA_VERSION {
            return Err(OfficeEnvelopeError::UnsupportedVersion(self.schema_version));
        }
        if self.id.trim().is_empty() || self.room_id.trim().is_empty() || self.sender.trim().is_empty() {
            return Err(OfficeEnvelopeError::MissingAddressing);
        }
        if self.summary.trim().is_empty() {
            return Err(OfficeEnvelopeError::MissingSummary);
        }
        if self.summary.len() > MAX_OFFICE_SUMMARY_BYTES {
            return Err(OfficeEnvelopeError::SummaryTooLarge {
                actual: self.summary.len(),
                limit: MAX_OFFICE_SUMMARY_BYTES,
            });
        }
        if self.artifact_refs.len() > MAX_OFFICE_ARTIFACT_REFS
            || self
                .artifact_refs
                .iter()
                .any(|reference| reference.trim().is_empty())
        {
            return Err(OfficeEnvelopeError::InvalidArtifactReferences);
        }
        if self.evidence.len() > MAX_OFFICE_EVIDENCE_RECEIPTS {
            return Err(OfficeEnvelopeError::TooManyEvidenceReceipts {
                actual: self.evidence.len(),
                limit: MAX_OFFICE_EVIDENCE_RECEIPTS,
            });
        }
        for receipt in &self.evidence {
            receipt.validate().map_err(OfficeEnvelopeError::InvalidEvidence)?;
        }
        Ok(())
    }

    /// Decode a historic store/inbox object such as
    /// `{id, room, sender, recipient, kind, payload, occurred_at}`. This is a
    /// read compatibility boundary only; callers must use the typed envelope
    /// for every new write.
    pub fn from_legacy_raw(raw: &Value) -> Result<Self, OfficeEnvelopeDecodeError> {
        let object = raw.as_object().ok_or(OfficeEnvelopeDecodeError::NotAnObject)?;

        // Let callers use the same decoder while replaying a newly typed row.
        // This is still decode-only and does not make a second writer surface.
        if object.contains_key("schema_version") && object.contains_key("room_id") {
            return serde_json::from_value(raw.clone()).map_err(|error| {
                OfficeEnvelopeDecodeError::InvalidField {
                    field: "office envelope",
                    detail: error.to_string(),
                }
            });
        }

        let payload = match object.get("payload") {
            Some(payload) => payload
                .as_object()
                .ok_or_else(|| OfficeEnvelopeDecodeError::InvalidField {
                    field: "payload",
                    detail: "must be an object".into(),
                })?,
            None => object,
        };
        let id = legacy_required_string(object, &["id", "message_id"])?;
        let room_id = legacy_required_string(object, &["room", "room_id"])?;
        let sender = legacy_sender(legacy_required_string(object, &["sender", "sender_id"])?)?;
        let recipient = legacy_recipient(legacy_required_value(object, &["recipient"])?)?;
        let kind = CoordinationKind::parse(&legacy_required_string(object, &["kind"])?).ok_or_else(|| {
            OfficeEnvelopeDecodeError::InvalidField {
                field: "kind",
                detail: "must be a supported coordination kind".into(),
            }
        })?;
        let summary = legacy_required_string(payload, &["body", "summary"])?;
        let task_id = legacy_optional_string(payload, &["task_id"])?;
        let requested_action = legacy_optional_string(payload, &["requested_action"])?;
        let mut artifact_refs = legacy_string_array(payload, &["refs", "artifact_refs"])?;
        let evidence = legacy_evidence(payload, &mut artifact_refs)?;
        artifact_refs.sort();
        artifact_refs.dedup();

        Ok(Self {
            schema_version: OFFICE_ENVELOPE_SCHEMA_VERSION,
            id,
            room_id,
            sender,
            recipient,
            kind,
            task_id,
            summary,
            artifact_refs,
            evidence,
            requested_action,
            sent_at: legacy_datetime(object, &["occurred_at", "sent_at"])?,
        })
    }

    /// Alias retained for callers that name this operation as a decode rather
    /// than a conversion.
    pub fn decode_legacy_raw(raw: &Value) -> Result<Self, OfficeEnvelopeDecodeError> {
        Self::from_legacy_raw(raw)
    }

    pub fn from_legacy_json(raw: &str) -> Result<Self, OfficeEnvelopeDecodeError> {
        let value = serde_json::from_str(raw)
            .map_err(|error| OfficeEnvelopeDecodeError::InvalidJson(error.to_string()))?;
        Self::from_legacy_raw(&value)
    }
}

fn legacy_required_value<'a>(
    object: &'a serde_json::Map<String, Value>,
    names: &[&'static str],
) -> Result<&'a Value, OfficeEnvelopeDecodeError> {
    names
        .iter()
        .find_map(|name| object.get(*name))
        .ok_or(OfficeEnvelopeDecodeError::MissingField(names[0]))
}

fn legacy_required_string(
    object: &serde_json::Map<String, Value>,
    names: &[&'static str],
) -> Result<String, OfficeEnvelopeDecodeError> {
    let value = legacy_required_value(object, names)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| OfficeEnvelopeDecodeError::InvalidField {
            field: names[0],
            detail: "must be a string".into(),
        })
}

fn legacy_optional_string(
    object: &serde_json::Map<String, Value>,
    names: &[&'static str],
) -> Result<Option<String>, OfficeEnvelopeDecodeError> {
    let Some(value) = names.iter().find_map(|name| object.get(*name)) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(OfficeEnvelopeDecodeError::InvalidField {
            field: names[0],
            detail: "must be a string or null".into(),
        }),
    }
}

fn legacy_string_array(
    object: &serde_json::Map<String, Value>,
    names: &[&'static str],
) -> Result<Vec<String>, OfficeEnvelopeDecodeError> {
    let Some(value) = names.iter().find_map(|name| object.get(*name)) else {
        return Ok(Vec::new());
    };
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| OfficeEnvelopeDecodeError::InvalidField {
                        field: names[0],
                        detail: "must contain only strings".into(),
                    })
            })
            .collect(),
        _ => Err(OfficeEnvelopeDecodeError::InvalidField {
            field: names[0],
            detail: "must be an array".into(),
        }),
    }
}

fn legacy_sender(sender: String) -> Result<AgentId, OfficeEnvelopeDecodeError> {
    let sender = sender.trim();
    if sender.is_empty() {
        return Err(OfficeEnvelopeDecodeError::InvalidField {
            field: "sender",
            detail: "must not be empty".into(),
        });
    }
    match Recipient::parse(sender) {
        Some(Recipient::Agent(agent)) => Ok(agent),
        Some(_) => Err(OfficeEnvelopeDecodeError::InvalidField {
            field: "sender",
            detail: "must address an agent".into(),
        }),
        None if sender.starts_with("agent:") || sender.starts_with("group:") => {
            Err(OfficeEnvelopeDecodeError::InvalidField {
                field: "sender",
                detail: "has an invalid address".into(),
            })
        }
        None => Ok(sender.to_owned()),
    }
}

fn legacy_recipient(value: &Value) -> Result<Recipient, OfficeEnvelopeDecodeError> {
    if let Some(wire) = value.as_str() {
        return Recipient::parse(wire).ok_or_else(|| OfficeEnvelopeDecodeError::InvalidField {
            field: "recipient",
            detail: "has an invalid address".into(),
        });
    }
    serde_json::from_value(value.clone()).map_err(|error| OfficeEnvelopeDecodeError::InvalidField {
        field: "recipient",
        detail: error.to_string(),
    })
}

fn legacy_datetime(
    object: &serde_json::Map<String, Value>,
    names: &[&'static str],
) -> Result<DateTime<Utc>, OfficeEnvelopeDecodeError> {
    serde_json::from_value(legacy_required_value(object, names)?.clone()).map_err(|error| {
        OfficeEnvelopeDecodeError::InvalidField {
            field: names[0],
            detail: error.to_string(),
        }
    })
}

fn legacy_evidence(
    payload: &serde_json::Map<String, Value>,
    artifact_refs: &mut Vec<String>,
) -> Result<Vec<EvidenceReceiptV1>, OfficeEnvelopeDecodeError> {
    let Some(value) = payload
        .get("evidence_receipts")
        .or_else(|| payload.get("evidence"))
    else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    // Older hive callers could place string evidence IDs here. Preserve those
    // as artifact references rather than inventing a receipt without a digest.
    if let Some(values) = value.as_array()
        && values.iter().all(Value::is_string)
    {
        artifact_refs.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        return Ok(Vec::new());
    }
    serde_json::from_value(value.clone()).map_err(|error| OfficeEnvelopeDecodeError::InvalidField {
        field: "evidence",
        detail: error.to_string(),
    })
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OfficeEnvelopeError {
    #[error("unsupported office envelope schema version: {0}")]
    UnsupportedVersion(u16),
    #[error("office envelope requires id, room, and sender")]
    MissingAddressing,
    #[error("office envelope requires a compact summary")]
    MissingSummary,
    #[error("office summary is {actual} bytes, exceeding the {limit}-byte limit")]
    SummaryTooLarge { actual: usize, limit: usize },
    #[error("office envelope has invalid artifact references")]
    InvalidArtifactReferences,
    #[error("office envelope has {actual} evidence receipts, exceeding the {limit}-receipt limit")]
    TooManyEvidenceReceipts { actual: usize, limit: usize },
    #[error("office envelope has invalid evidence receipt: {0}")]
    InvalidEvidence(EvidenceReceiptError),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OfficeEnvelopeDecodeError {
    #[error("legacy office envelope is not an object")]
    NotAnObject,
    #[error("legacy office envelope is missing `{0}`")]
    MissingField(&'static str),
    #[error("legacy office envelope has invalid `{field}`: {detail}")]
    InvalidField { field: &'static str, detail: String },
    #[error("legacy office envelope JSON is invalid: {0}")]
    InvalidJson(String),
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

    /// Full validation of a typed message against the room it claims.
    ///
    /// The live `hive` tool carries an untyped JSON payload rather than a
    /// [`HivePayload`], so it cannot use this entry point directly; the store
    /// enforces the same room rule on the wire form via
    /// [`PrivateRoom::permits`] (see `Store::insert_hive_message`). This
    /// remains the checked path for any caller that does hold a typed message.
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
    fn recipient_wire_form_round_trips_and_rejects_unaddressable_strings() {
        for recipient in [
            Recipient::Agent("018f3a2e-1111-7000-8000-000000000000".into()),
            Recipient::Group("all".into()),
            Recipient::Leader,
            Recipient::Manager,
        ] {
            let wire = recipient.to_wire();
            assert_eq!(Recipient::parse(&wire), Some(recipient.clone()), "{wire}");
        }
        assert_eq!(
            Recipient::parse("agent:abc")
                .as_ref()
                .and_then(Recipient::agent_id),
            Some("abc")
        );
        assert_eq!(
            Recipient::parse(BROADCAST_RECIPIENT),
            Some(Recipient::Group("all".into()))
        );
        // The historical bogus default and other bare words are not addresses.
        for bogus in ["", "  ", "agent:", "group:", "Manager", "worker", "018f3a2e"] {
            assert_eq!(Recipient::parse(bogus), None, "{bogus:?} must not parse");
        }
    }

    #[test]
    fn rooms_scope_named_agents_but_not_run_level_addresses() {
        let now = Utc::now();
        let room =
            PrivateRoom::from_wire_members("room-1", "backend", now, ["agent:a", "manager", "group:all"]);
        assert_eq!(
            room.members.iter().cloned().collect::<Vec<_>>(),
            vec!["a".to_owned()]
        );
        assert!(room.permits(&Recipient::Agent("a".into())));
        assert!(!room.permits(&Recipient::Agent("b".into())));
        assert!(room.permits(&Recipient::Manager));
        assert!(room.permits(&Recipient::Group("all".into())));
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

    #[test]
    fn legacy_raw_messages_decode_to_compact_typed_envelopes() {
        let now = Utc::now();
        let raw = serde_json::json!({
            "id": "message-1",
            "room": "run",
            "sender": "agent:worker-1",
            "recipient": "agent:lead-1",
            "kind": "finding",
            "payload": {
                "body": "focused tests pass",
                "task_id": "parser",
                "refs": ["artifact-b", "artifact-a", "artifact-a"],
                // Historic callers sometimes used `evidence` for artifact
                // IDs. The decoder preserves them without inventing a digest.
                "evidence": ["artifact-e"],
                "requested_action": null,
            },
            "occurred_at": now,
        });

        let envelope = OfficeEnvelopeV1::from_legacy_raw(&raw).expect("legacy row decodes");
        assert_eq!(envelope.schema_version, OFFICE_ENVELOPE_SCHEMA_VERSION);
        assert_eq!(envelope.sender, "worker-1");
        assert_eq!(envelope.recipient, Recipient::Agent("lead-1".into()));
        assert_eq!(envelope.kind, CoordinationKind::Finding);
        assert_eq!(envelope.task_id.as_deref(), Some("parser"));
        assert_eq!(
            envelope.artifact_refs,
            vec!["artifact-a", "artifact-b", "artifact-e"]
        );
        assert!(envelope.evidence.is_empty());
        envelope.validate().expect("normalized envelope is valid");

        // The compatibility reader also accepts the canonical typed row during
        // replay, but exposes no legacy raw writer.
        let canonical = serde_json::to_value(&envelope).expect("serialize typed envelope");
        assert_eq!(OfficeEnvelopeV1::decode_legacy_raw(&canonical), Ok(envelope));
    }

    #[test]
    fn evidence_receipts_and_envelopes_reject_unbounded_or_undigested_context() {
        let receipt = EvidenceReceiptV1 {
            schema_version: EVIDENCE_RECEIPT_SCHEMA_VERSION,
            conclusion: "the focused test suite passed".into(),
            locator: "cargo test -p minha-core books::tests".into(),
            evidence_digest: "sha256:receipt".into(),
            excerpt: "17 passed".into(),
        };
        receipt.validate().expect("complete receipt is valid");

        let envelope = OfficeEnvelopeV1 {
            schema_version: OFFICE_ENVELOPE_SCHEMA_VERSION,
            id: "message-2".into(),
            room_id: RUN_ROOM_ID.into(),
            sender: "worker-1".into(),
            recipient: Recipient::Manager,
            kind: CoordinationKind::Progress,
            task_id: Some("parser".into()),
            summary: "focused parser work is ready for integration".into(),
            artifact_refs: vec!["artifact-1".into()],
            evidence: vec![receipt.clone()],
            requested_action: Some("review patch".into()),
            sent_at: Utc::now(),
        };
        envelope.validate().expect("compact typed envelope is valid");

        let mut missing_digest = receipt;
        missing_digest.evidence_digest.clear();
        assert_eq!(
            missing_digest.validate(),
            Err(EvidenceReceiptError::MissingDigest)
        );
        assert!(matches!(
            OfficeEnvelopeV1::from_legacy_raw(&serde_json::json!({
                "id": "bad",
                "room": "run",
                "sender": "agent:worker",
                "recipient": "not-an-address",
                "kind": "finding",
                "payload": {"body": "x"},
                "occurred_at": Utc::now(),
            })),
            Err(OfficeEnvelopeDecodeError::InvalidField {
                field: "recipient",
                ..
            })
        ));
    }
}
