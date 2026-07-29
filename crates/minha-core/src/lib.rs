//! Core runtime for Minha, a token-efficient multi-agent coding harness.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod auth;
pub mod books;
pub mod cache;
pub mod config;
pub mod context;
pub mod executor;
pub mod facts;
pub mod github;
pub mod graph;
pub mod instructions;
pub mod judge;
pub mod models;
pub mod office;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod store;
pub mod tools;
pub mod update;
pub mod usage;
pub mod worktree;

pub use config::Config;
pub use protocol::{
    AgentState, EventAgentId, EventEnvelope, ExitState, ItemId, Mode, RequestId, RunId, RuntimeCommand,
    RuntimeEvent, SessionId, TurnId,
};
pub use runtime::{Harness, RunKind, RunOutcome};
pub use store::Store;
