//! Versioned execution graph and the coordination records attached to it.
//!
//! The graph is deliberately an in-memory, serializable model.  Persistence and
//! transport belong to the callers; keeping this layer pure makes stale writers
//! and read-only observers straightforward to test.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;
use uuid::Uuid;

pub type NodeId = String;
pub type AgentId = String;
pub type LeaseId = String;
pub type QuestionId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: NodeId,
    pub label: String,
    pub state: NodeState,
    pub assigned_to: Option<AgentId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub prerequisite: NodeId,
    pub dependent: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphVersion {
    pub version: u64,
    pub nodes: BTreeMap<NodeId, GraphNode>,
    pub dependencies: BTreeSet<(NodeId, NodeId)>,
}

impl Default for GraphVersion {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphVersion {
    pub fn new() -> Self {
        Self {
            version: 0,
            nodes: BTreeMap::new(),
            dependencies: BTreeSet::new(),
        }
    }
    pub fn add_node(&self, node: GraphNode) -> Result<Self, GraphError> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::DuplicateNode(node.id));
        }
        let mut next = self.clone();
        next.nodes.insert(node.id.clone(), node);
        next.version += 1;
        Ok(next)
    }
    pub fn add_dependency(
        &self,
        prerequisite: impl Into<NodeId>,
        dependent: impl Into<NodeId>,
    ) -> Result<Self, GraphError> {
        let edge = (prerequisite.into(), dependent.into());
        if !self.nodes.contains_key(&edge.0) {
            return Err(GraphError::UnknownNode(edge.0));
        }
        if !self.nodes.contains_key(&edge.1) {
            return Err(GraphError::UnknownNode(edge.1));
        }
        if edge.0 == edge.1 || self.reachable(&edge.1, &edge.0) {
            return Err(GraphError::Cycle);
        }
        let mut next = self.clone();
        next.dependencies.insert(edge);
        next.version += 1;
        Ok(next)
    }
    pub fn update_state(&self, id: &str, state: NodeState) -> Result<Self, GraphError> {
        let mut next = self.clone();
        let node = next
            .nodes
            .get_mut(id)
            .ok_or_else(|| GraphError::UnknownNode(id.into()))?;
        node.state = state;
        next.version += 1;
        Ok(next)
    }
    pub fn ready_nodes(&self) -> Vec<&GraphNode> {
        self.nodes
            .values()
            .filter(|node| {
                matches!(node.state, NodeState::Pending | NodeState::Ready)
                    && self
                        .dependencies
                        .iter()
                        .filter(|(_, d)| d == &node.id)
                        .all(|(p, _)| self.nodes.get(p).is_some_and(|n| n.state == NodeState::Succeeded))
            })
            .collect()
    }
    pub fn dependencies_of(&self, id: &str) -> Vec<NodeId> {
        self.dependencies
            .iter()
            .filter(|(_, d)| d == id)
            .map(|(p, _)| p.clone())
            .collect()
    }
    fn reachable(&self, from: &str, target: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([from.to_owned()]);
        while let Some(current) = queue.pop_front() {
            if current == target {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            queue.extend(
                self.dependencies
                    .iter()
                    .filter(|(p, _)| p == &current)
                    .map(|(_, d)| d.clone()),
            );
        }
        false
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GraphError {
    #[error("node already exists: {0}")]
    DuplicateNode(NodeId),
    #[error("unknown node: {0}")]
    UnknownNode(NodeId),
    #[error("dependency would create a cycle")]
    Cycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub id: LeaseId,
    pub node: NodeId,
    pub holder: AgentId,
    pub generation: u64,
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    pub fn new(node: impl Into<NodeId>, holder: impl Into<AgentId>, generation: u64, ttl: Duration) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            node: node.into(),
            holder: holder.into(),
            generation,
            expires_at: Utc::now() + ttl,
        }
    }
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LeaseError {
    #[error("lease not found")]
    NotFound,
    #[error("lease is expired")]
    Expired,
    #[error("lease holder or generation is stale")]
    Fenced,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeaseTable {
    pub leases: BTreeMap<NodeId, Lease>,
}
impl LeaseTable {
    pub fn acquire(&mut self, lease: Lease, now: DateTime<Utc>) -> Result<(), LeaseError> {
        if self
            .leases
            .get(&lease.node)
            .is_some_and(|old| !old.is_expired(now))
        {
            return Err(LeaseError::Fenced);
        }
        self.leases.insert(lease.node.clone(), lease);
        Ok(())
    }
    pub fn validate(
        &self,
        node: &str,
        holder: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<&Lease, LeaseError> {
        let lease = self.leases.get(node).ok_or(LeaseError::NotFound)?;
        if lease.is_expired(now) {
            return Err(LeaseError::Expired);
        }
        if lease.holder != holder || lease.generation != generation {
            return Err(LeaseError::Fenced);
        }
        Ok(lease)
    }
    pub fn release(
        &mut self,
        node: &str,
        holder: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<Lease, LeaseError> {
        let lease = self.validate(node, holder, generation, now)?.clone();
        self.leases.remove(node);
        Ok(lease)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: QuestionId,
    pub from: AgentId,
    pub to: Option<AgentId>,
    pub text: String,
    pub blocking: bool,
    pub asked_at: DateTime<Utc>,
}
impl Question {
    pub fn new(
        from: impl Into<AgentId>,
        to: Option<AgentId>,
        text: impl Into<String>,
        blocking: bool,
    ) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            from: from.into(),
            to,
            text: text.into(),
            blocking,
            asked_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(id: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: id.into(),
            state: NodeState::Pending,
            assigned_to: None,
        }
    }
    #[test]
    fn versions_are_immutable_and_cycles_are_rejected() {
        let g = GraphVersion::new()
            .add_node(node("a"))
            .expect("test operation should succeed")
            .add_node(node("b"))
            .expect("test operation should succeed");
        let g = g.add_dependency("a", "b").expect("test operation should succeed");
        assert_eq!(g.version, 3);
        assert!(matches!(g.add_dependency("b", "a"), Err(GraphError::Cycle)));
        assert_eq!(g.ready_nodes()[0].id, "a");
    }
    #[test]
    fn leases_are_generation_fenced() {
        let now = Utc::now();
        let mut t = LeaseTable::default();
        t.acquire(
            Lease {
                id: "l".into(),
                node: "n".into(),
                holder: "a".into(),
                generation: 4,
                expires_at: now + Duration::minutes(1),
            },
            now,
        )
        .expect("test operation should succeed");
        assert!(matches!(t.validate("n", "a", 3, now), Err(LeaseError::Fenced)));
    }
}
