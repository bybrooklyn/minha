//! A conservative, read-only judge for goal progress.
//!
//! Judging never mutates the graph, assigns work, acquires leases, or executes
//! commands.  It evaluates only the contract and evidence supplied by a caller.

use crate::graph::{GraphVersion, NodeState};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum EvidenceLevel {
    Claim,
    Plan,
    Diff,
    Test,
    Review,
    Runtime,
}

impl EvidenceLevel {
    pub fn rank(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub level: EvidenceLevel,
    pub statement: String,
    pub source: String,
    pub supports: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalContract {
    pub goal: String,
    pub required_nodes: BTreeSet<String>,
    pub minimum_evidence: EvidenceLevel,
    pub read_only: bool,
}

impl GoalContract {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            required_nodes: BTreeSet::new(),
            minimum_evidence: EvidenceLevel::Test,
            read_only: true,
        }
    }
    pub fn requires_node(mut self, node: impl Into<String>) -> Self {
        self.required_nodes.insert(node.into());
        self
    }
    pub fn requiring_at_least(mut self, level: EvidenceLevel) -> Self {
        self.minimum_evidence = level;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Judgement {
    Satisfied,
    Incomplete {
        missing_nodes: Vec<String>,
        insufficient_evidence: Vec<String>,
    },
    Contradicted {
        reasons: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeReport {
    pub judgement: Judgement,
    pub observed_graph_version: u64,
    pub evidence_used: Vec<String>,
    pub mutated: bool,
}

#[derive(Debug, Default)]
pub struct ReadOnlyJudge;

impl ReadOnlyJudge {
    pub fn evaluate(
        &self,
        contract: &GoalContract,
        graph: &GraphVersion,
        evidence: &[Evidence],
    ) -> JudgeReport {
        let missing_nodes = contract
            .required_nodes
            .iter()
            .filter(|id| {
                graph
                    .nodes
                    .get(*id)
                    .is_none_or(|n| n.state != NodeState::Succeeded)
            })
            .cloned()
            .collect::<Vec<_>>();
        let insufficient_evidence = contract
            .required_nodes
            .iter()
            .filter(|id| {
                !evidence.iter().any(|e| {
                    e.supports.iter().any(|supported| supported == *id)
                        && e.level.rank() >= contract.minimum_evidence.rank()
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let judgement = if evidence
            .iter()
            .any(|e| e.statement.to_ascii_lowercase().contains("contradict"))
        {
            Judgement::Contradicted {
                reasons: evidence
                    .iter()
                    .filter(|e| e.statement.to_ascii_lowercase().contains("contradict"))
                    .map(|e| e.statement.clone())
                    .collect(),
            }
        } else if missing_nodes.is_empty() && insufficient_evidence.is_empty() {
            Judgement::Satisfied
        } else {
            Judgement::Incomplete {
                missing_nodes,
                insufficient_evidence,
            }
        };
        JudgeReport {
            judgement,
            observed_graph_version: graph.version,
            evidence_used: evidence.iter().map(|e| e.id.clone()).collect(),
            mutated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphNode, GraphVersion};
    #[test]
    fn judge_is_read_only_and_requires_both_state_and_evidence() {
        let g = GraphVersion::new()
            .add_node(GraphNode {
                id: "n".into(),
                label: "n".into(),
                state: NodeState::Succeeded,
                assigned_to: None,
            })
            .expect("test operation should succeed");
        let before = g.clone();
        let contract = GoalContract::new("ship").requires_node("n");
        let report = ReadOnlyJudge.evaluate(
            &contract,
            &g,
            &[Evidence {
                id: "t".into(),
                level: EvidenceLevel::Test,
                statement: "passed".into(),
                source: "ci".into(),
                supports: vec!["n".into()],
            }],
        );
        assert_eq!(report.judgement, Judgement::Satisfied);
        assert!(!report.mutated);
        assert_eq!(g, before);
    }
    #[test]
    fn weak_evidence_does_not_promote_a_goal() {
        let g = GraphVersion::new()
            .add_node(GraphNode {
                id: "n".into(),
                label: "n".into(),
                state: NodeState::Succeeded,
                assigned_to: None,
            })
            .expect("test operation should succeed");
        let c = GoalContract::new("x")
            .requires_node("n")
            .requiring_at_least(EvidenceLevel::Test);
        let r = ReadOnlyJudge.evaluate(
            &c,
            &g,
            &[Evidence {
                id: "c".into(),
                level: EvidenceLevel::Claim,
                statement: "done".into(),
                source: "agent".into(),
                supports: vec!["n".into()],
            }],
        );
        assert!(matches!(r.judgement, Judgement::Incomplete { .. }));
    }
}
