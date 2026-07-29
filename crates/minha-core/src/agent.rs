//! Scheduling and the explicit messages exchanged by cooperating agents.

use crate::graph::{AgentId, GraphVersion, Lease, LeaseTable, NodeId};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Busy,
    Waiting,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub status: AgentStatus,
    pub capacity: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterAgentMessage {
    TaskOffer {
        message_id: String,
        from: AgentId,
        to: AgentId,
        node: NodeId,
        graph_version: u64,
    },
    TaskAccepted {
        message_id: String,
        agent: AgentId,
        node: NodeId,
        lease: Lease,
    },
    TaskProgress {
        message_id: String,
        agent: AgentId,
        node: NodeId,
        detail: String,
    },
    TaskCompleted {
        message_id: String,
        agent: AgentId,
        node: NodeId,
        success: bool,
        evidence_ids: Vec<String>,
    },
    Question(crate::graph::Question),
    Answer {
        message_id: String,
        question_id: String,
        from: AgentId,
        text: String,
    },
    Cancel {
        message_id: String,
        from: AgentId,
        node: NodeId,
        reason: String,
    },
}

impl InterAgentMessage {
    pub fn id(&self) -> &str {
        match self {
            Self::TaskOffer { message_id, .. }
            | Self::TaskAccepted { message_id, .. }
            | Self::TaskProgress { message_id, .. }
            | Self::TaskCompleted { message_id, .. }
            | Self::Answer { message_id, .. }
            | Self::Cancel { message_id, .. } => message_id,
            Self::Question(q) => &q.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedulerMessage {
    Tick { at: DateTime<Utc> },
    Register(Agent),
    Unregister { agent: AgentId },
    Deliver(InterAgentMessage),
    LeaseExpired { node: NodeId, generation: u64 },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("agent is not registered: {0}")]
    UnknownAgent(AgentId),
    #[error("agent has no capacity")]
    AtCapacity,
    #[error("node is not ready: {0}")]
    NotReady(NodeId),
    #[error("lease operation failed: {0}")]
    Lease(String),
}

#[derive(Debug, Default)]
pub struct Scheduler {
    pub agents: BTreeMap<AgentId, Agent>,
    pub leases: LeaseTable,
    pub outbox: Vec<InterAgentMessage>,
}

impl Scheduler {
    pub fn register(&mut self, agent: Agent) {
        self.agents.insert(agent.id.clone(), agent);
    }
    pub fn handle(
        &mut self,
        message: SchedulerMessage,
        graph: &GraphVersion,
    ) -> Result<Vec<InterAgentMessage>, SchedulerError> {
        match message {
            SchedulerMessage::Register(agent) => {
                self.register(agent);
                Ok(Vec::new())
            }
            SchedulerMessage::Unregister { agent } => {
                self.agents.remove(&agent);
                Ok(Vec::new())
            }
            SchedulerMessage::Tick { at } => Ok(self.schedule(graph, at)),
            SchedulerMessage::Deliver(message) => {
                self.outbox.push(message);
                Ok(Vec::new())
            }
            SchedulerMessage::LeaseExpired { node, .. } => {
                self.leases.leases.remove(&node);
                Ok(Vec::new())
            }
        }
    }
    pub fn schedule(&mut self, graph: &GraphVersion, now: DateTime<Utc>) -> Vec<InterAgentMessage> {
        let mut offers = Vec::new();
        for node in graph.ready_nodes() {
            let Some(agent) = self
                .agents
                .values_mut()
                .find(|a| a.status == AgentStatus::Idle && a.capacity > 0)
            else {
                break;
            };
            agent.status = AgentStatus::Busy;
            agent.capacity -= 1;
            let offer = InterAgentMessage::TaskOffer {
                message_id: Uuid::now_v7().to_string(),
                from: "scheduler".into(),
                to: agent.id.clone(),
                node: node.id.clone(),
                graph_version: graph.version,
            };
            let _ = now;
            offers.push(offer);
        }
        offers
    }
    pub fn accept(
        &mut self,
        agent: &str,
        node: &str,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<Lease, SchedulerError> {
        let a = self
            .agents
            .get(agent)
            .ok_or_else(|| SchedulerError::UnknownAgent(agent.into()))?;
        if a.status != AgentStatus::Busy {
            return Err(SchedulerError::AtCapacity);
        }
        let lease = Lease::new(node, agent, generation, Duration::minutes(10));
        self.leases
            .acquire(lease.clone(), now)
            .map_err(|e| SchedulerError::Lease(e.to_string()))?;
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphNode, GraphVersion, NodeState};
    #[test]
    fn scheduler_offers_ready_work_to_idle_agents() {
        let g = GraphVersion::new()
            .add_node(GraphNode {
                id: "n".into(),
                label: "work".into(),
                state: NodeState::Ready,
                assigned_to: None,
            })
            .expect("test operation should succeed");
        let mut s = Scheduler::default();
        s.register(Agent {
            id: "a".into(),
            status: AgentStatus::Idle,
            capacity: 1,
            generation: 1,
        });
        let out = s.schedule(&g, Utc::now());
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], InterAgentMessage::TaskOffer { to, .. } if to == "a"));
    }
}
