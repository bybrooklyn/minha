//! Durable facts with explicit replacement and tombstone semantics.

use crate::protocol::{BoardEntryView, EventAgentId, RunId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    Preference,
    Decision,
    Constraint,
    Observation,
    Identity,
    Other,
}

impl FactType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Constraint => "constraint",
            Self::Observation => "observation",
            Self::Identity => "identity",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardScope {
    Session,
    Project,
}

impl BoardScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardKind {
    Decision,
    Constraint,
    Finding,
    Blocker,
    Artifact,
    Progress,
}

impl BoardKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Constraint => "constraint",
            Self::Finding => "finding",
            Self::Blocker => "blocker",
            Self::Artifact => "artifact",
            Self::Progress => "progress",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardStatus {
    Open,
    Resolved,
    Superseded,
}

impl BoardStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardEntry {
    pub id: String,
    pub workspace_id: String,
    pub run_id: Option<RunId>,
    pub scope: BoardScope,
    pub kind: BoardKind,
    pub subject: String,
    pub body: String,
    pub task_id: Option<String>,
    pub author_agent_id: Option<EventAgentId>,
    pub confidence: u8,
    pub status: BoardStatus,
    pub supersedes_id: Option<String>,
    pub evidence: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl BoardEntry {
    pub fn session(
        workspace_id: impl Into<String>,
        run_id: RunId,
        kind: BoardKind,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            workspace_id: workspace_id.into(),
            run_id: Some(run_id),
            scope: BoardScope::Session,
            kind,
            subject: subject.into(),
            body: body.into(),
            task_id: None,
            author_agent_id: None,
            confidence: 100,
            status: BoardStatus::Open,
            supersedes_id: None,
            evidence: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn view(&self) -> BoardEntryView {
        BoardEntryView {
            id: self.id.clone(),
            scope: self.scope.as_str().into(),
            kind: self.kind.as_str().into(),
            subject: self.subject.clone(),
            body: self.body.clone(),
            task_id: self.task_id.clone(),
            author_agent_id: self.author_agent_id,
            confidence: self.confidence,
            status: self.status.as_str().into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub kind: FactType,
    pub subject: String,
    pub value: String,
    pub confidence: u8,
    pub tombstone: bool,
}

impl Fact {
    pub fn new(
        id: impl Into<String>,
        kind: FactType,
        subject: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            subject: subject.into(),
            value: value.into(),
            confidence: 100,
            tombstone: false,
        }
    }
    pub fn tombstone(id: impl Into<String>) -> Self {
        Self::new(id, FactType::Other, "", "").with_tombstone()
    }
    pub fn with_confidence(mut self, confidence: u8) -> Self {
        self.confidence = confidence;
        self
    }
    fn with_tombstone(mut self) -> Self {
        self.tombstone = true;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct FactStore {
    facts: HashMap<String, Fact>,
}

impl FactStore {
    pub fn upsert(&mut self, fact: Fact) {
        self.facts.insert(fact.id.clone(), fact);
    }
    pub fn remove(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.facts.insert(id.clone(), Fact::tombstone(id));
    }
    pub fn get(&self, id: &str) -> Option<&Fact> {
        self.facts.get(id).filter(|f| !f.tombstone)
    }
    pub fn all(&self) -> impl Iterator<Item = &Fact> {
        self.facts.values().filter(|f| !f.tombstone)
    }
    pub fn retrieve(&self, query: &str, limit: usize) -> Vec<&Fact> {
        let terms: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut ranked: Vec<(usize, &Fact)> = self
            .all()
            .filter_map(|f| {
                let hay = format!("{} {}", f.subject, f.value).to_lowercase();
                let score = terms.iter().filter(|t| hay.contains(t.as_str())).count();
                (score > 0).then_some((score, f))
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.confidence.cmp(&a.1.confidence))
                .then_with(|| a.1.id.cmp(&b.1.id))
        });
        ranked.into_iter().take(limit).map(|(_, f)| f).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tombstones_hide_deleted_facts() {
        let mut s = FactStore::default();
        s.upsert(Fact::new("x", FactType::Decision, "db", "sqlite"));
        s.remove("x");
        assert!(s.get("x").is_none());
        assert!(s.facts["x"].tombstone);
    }
    #[test]
    fn retrieval_ranks_matches() {
        let mut s = FactStore::default();
        s.upsert(Fact::new("a", FactType::Preference, "editor", "vim"));
        s.upsert(Fact::new("b", FactType::Observation, "editor", "vim config"));
        assert_eq!(s.retrieve("editor vim", 2)[0].id, "a");
        assert_eq!(s.retrieve("config", 1)[0].id, "b");
    }
}
