//! Durable, reviewable semantic and episodic memory records.

use crate::cache::contains_secret;
use crate::protocol::RunId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    User,
    Project,
    Run,
}

impl MemoryScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Run => "run",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub workspace_id: Option<String>,
    pub run_id: Option<RunId>,
    pub scope: MemoryScope,
    pub kind: String,
    pub subject: String,
    pub body: String,
    pub confidence: u8,
    pub salience: u8,
    pub provenance: Vec<String>,
    pub entities: Vec<String>,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub access_count: u64,
    pub pinned: bool,
    pub supersedes_id: Option<String>,
    pub tombstone: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MemoryRecord {
    pub fn candidate(
        scope: MemoryScope,
        kind: impl Into<String>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            workspace_id: None,
            run_id: None,
            scope,
            kind: kind.into(),
            subject: subject.into(),
            body: body.into(),
            confidence: 80,
            salience: 50,
            provenance: Vec::new(),
            entities: Vec::new(),
            valid_from: now,
            valid_until: None,
            access_count: 0,
            pinned: false,
            supersedes_id: None,
            tombstone: false,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_safe(&self) -> bool {
        !self.subject.trim().is_empty()
            && !self.body.trim().is_empty()
            && !contains_secret("memory", self.subject.as_bytes())
            && !contains_secret("memory", self.body.as_bytes())
            && self
                .provenance
                .iter()
                .all(|value| !contains_secret("memory", value.as_bytes()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    pub memory: MemoryRecord,
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemorySettings {
    pub enabled: bool,
    pub use_memory: bool,
    pub generate: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            use_memory: true,
            generate: true,
        }
    }
}
