//! Local, provider-independent token reservation decisions.

use crate::models::Model;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Durable accounting schema. The SQLite store is the source of truth for
/// these entries; events and run-level counters are compatibility projections.
pub const USAGE_LEDGER_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKindV1 {
    ModelTurn,
    Compaction,
    LegacyUnverified,
}

impl UsageKindV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelTurn => "model_turn",
            Self::Compaction => "compaction",
            Self::LegacyUnverified => "legacy_unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStateV1 {
    Settled,
    LegacyUnverified,
}

impl UsageStateV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::LegacyUnverified => "legacy_unverified",
        }
    }
}

/// Compact canonical record for a billable provider boundary. `entry_key` is
/// stable for an observed provider response when the provider exposes one;
/// duplicate settlement must never change totals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageLedgerEntryV1 {
    pub schema_version: u16,
    pub entry_key: String,
    pub run_id: String,
    pub kind: UsageKindV1,
    pub state: UsageStateV1,
    pub provider: String,
    pub model: String,
    pub agent_id: Option<String>,
    pub provider_response_id: Option<String>,
    pub usage: TokenUsage,
    pub context_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cached_input: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub reasoning_output: u64,
}

impl TokenUsage {
    pub const fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    pub input: u64,
    pub output: u64,
    pub total: u64,
}

impl UsageLimit {
    pub const fn unlimited() -> Self {
        Self {
            input: u64::MAX,
            output: u64::MAX,
            total: u64::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub used: TokenUsage,
    pub reserved: TokenUsage,
    pub limit: UsageLimit,
}

impl UsageSnapshot {
    pub const fn remaining(self) -> TokenUsage {
        TokenUsage {
            input: self
                .limit
                .input
                .saturating_sub(self.used.input)
                .saturating_sub(self.reserved.input),
            output: self
                .limit
                .output
                .saturating_sub(self.used.output)
                .saturating_sub(self.reserved.output),
            cached_input: 0,
            cache_write: 0,
            reasoning_output: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReserveDecision {
    Allowed { remaining: TokenUsage },
    Denied { remaining: TokenUsage },
}

#[derive(Debug)]
pub struct UsageLedger {
    snapshot: UsageSnapshot,
}

impl UsageLedger {
    pub const fn new(limit: UsageLimit) -> Self {
        Self {
            snapshot: UsageSnapshot {
                used: TokenUsage {
                    input: 0,
                    output: 0,
                    cached_input: 0,
                    cache_write: 0,
                    reasoning_output: 0,
                },
                reserved: TokenUsage {
                    input: 0,
                    output: 0,
                    cached_input: 0,
                    cache_write: 0,
                    reasoning_output: 0,
                },
                limit,
            },
        }
    }
    pub const fn snapshot(&self) -> UsageSnapshot {
        self.snapshot
    }

    pub fn reserve(&mut self, estimate: TokenUsage) -> ReserveDecision {
        let remaining = self.snapshot.remaining();
        if estimate.input <= remaining.input
            && estimate.output <= remaining.output
            && estimate.total()
                <= self
                    .snapshot
                    .limit
                    .total
                    .saturating_sub(self.snapshot.used.total())
                    .saturating_sub(self.snapshot.reserved.total())
        {
            self.snapshot.reserved.input += estimate.input;
            self.snapshot.reserved.output += estimate.output;
            ReserveDecision::Allowed {
                remaining: self.snapshot.remaining(),
            }
        } else {
            ReserveDecision::Denied { remaining }
        }
    }

    pub fn commit(&mut self, reserved: TokenUsage, actual: TokenUsage) {
        self.snapshot.reserved.input = self.snapshot.reserved.input.saturating_sub(reserved.input);
        self.snapshot.reserved.output = self.snapshot.reserved.output.saturating_sub(reserved.output);
        self.snapshot.used.input = self.snapshot.used.input.saturating_add(actual.input);
        self.snapshot.used.output = self.snapshot.used.output.saturating_add(actual.output);
        self.snapshot.used.cached_input = self
            .snapshot
            .used
            .cached_input
            .saturating_add(actual.cached_input);
        self.snapshot.used.cache_write = self.snapshot.used.cache_write.saturating_add(actual.cache_write);
        self.snapshot.used.reasoning_output = self
            .snapshot
            .used
            .reasoning_output
            .saturating_add(actual.reasoning_output);
    }

    pub fn release(&mut self, reserved: TokenUsage) {
        self.snapshot.reserved.input = self.snapshot.reserved.input.saturating_sub(reserved.input);
        self.snapshot.reserved.output = self.snapshot.reserved.output.saturating_sub(reserved.output);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: Model,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

/// Read-only account quota data returned with a Codex response. Minha exposes
/// it for scheduling and UX; no runtime path can redeem or purchase credits.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub limit_id: String,
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
}

/// Returns true when another model turn would consume the configured account
/// reserve. The provider reports percentages in the inclusive 0-100 range.
pub fn reserve_reached(snapshots: &[RateLimitSnapshot], reserve_percent: f32) -> bool {
    let stop_at = 100.0 - f64::from(reserve_percent.clamp(0.0, 100.0));
    snapshots.iter().any(|snapshot| {
        snapshot
            .primary
            .as_ref()
            .into_iter()
            .chain(snapshot.secondary.as_ref())
            .any(|window| window.used_percent >= stop_at)
    })
}

impl fmt::Display for ReserveDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed { .. } => f.write_str("allowed"),
            Self::Denied { .. } => f.write_str("denied"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserves_and_releases_atomically() {
        let mut l = UsageLedger::new(UsageLimit {
            input: 10,
            output: 5,
            total: 12,
        });
        assert!(matches!(
            l.reserve(TokenUsage {
                input: 4,
                output: 3,
                ..Default::default()
            }),
            ReserveDecision::Allowed { .. }
        ));
        assert!(matches!(
            l.reserve(TokenUsage {
                input: 7,
                output: 0,
                ..Default::default()
            }),
            ReserveDecision::Denied { .. }
        ));
        l.release(TokenUsage {
            input: 4,
            output: 3,
            ..Default::default()
        });
        assert!(matches!(
            l.reserve(TokenUsage {
                input: 7,
                output: 0,
                ..Default::default()
            }),
            ReserveDecision::Allowed { .. }
        ));
    }
    #[test]
    fn commit_moves_reserved_to_used() {
        let mut l = UsageLedger::new(UsageLimit::unlimited());
        l.reserve(TokenUsage {
            input: 3,
            output: 2,
            ..Default::default()
        });
        l.commit(
            TokenUsage {
                input: 3,
                output: 2,
                ..Default::default()
            },
            TokenUsage {
                input: 2,
                output: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            l.snapshot().used,
            TokenUsage {
                input: 2,
                output: 1,
                ..Default::default()
            }
        );
        assert_eq!(l.snapshot().reserved, TokenUsage::default());
    }

    #[test]
    fn account_reserve_stops_before_the_next_turn() {
        let snapshots = vec![RateLimitSnapshot {
            limit_id: "codex".into(),
            primary: Some(RateLimitWindow {
                used_percent: 88.0,
                ..Default::default()
            }),
            ..Default::default()
        }];
        assert!(reserve_reached(&snapshots, 12.0));
        assert!(!reserve_reached(&snapshots, 10.0));
    }
}
