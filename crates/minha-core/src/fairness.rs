//! Provider-neutral, durable routing fairness primitives.
//!
//! The store owns persistence and atomic admission.  This module deliberately
//! contains only small typed values and deterministic arithmetic so routing
//! policy remains testable without a provider client or a database.

use crate::usage::TokenUsage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version for persisted WDRR state and decisions.
pub const FAIRNESS_SCHEMA_VERSION: u16 = 1;
/// Version for persisted provider health observations.
pub const PROVIDER_HEALTH_SCHEMA_VERSION: u16 = 1;
/// Equal-weight WDRR quantum, expressed in normalized token work.
pub const WDRR_QUANTUM: u64 = 100_000;
/// First transient provider cooldown when no Retry-After value is available.
pub const INITIAL_PROVIDER_COOLDOWN_SECONDS: i64 = 15;
/// Upper bound for locally inferred exponential cooldowns.
pub const MAX_PROVIDER_COOLDOWN_SECONDS: i64 = 5 * 60;

/// Normalized useful consumption used by the fair router.
///
/// Cached input is deliberately discounted and output/reasoning output are
/// weighted more heavily. `cache_write` is metadata for cache accounting, not
/// useful model work, so it is intentionally excluded.
pub fn normalized_token_work(usage: TokenUsage) -> u64 {
    usage
        .input
        .saturating_add(usage.cached_input / 4)
        .saturating_add(usage.output.saturating_mul(4))
        .saturating_add(usage.reasoning_output.saturating_mul(4))
}

/// Local fallback for a transient provider failure. The first failure waits
/// 15 seconds, then doubles until it reaches five minutes.
pub fn exponential_cooldown_seconds(consecutive_failures: u32) -> i64 {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    let multiplier = 1_i64 << exponent;
    INITIAL_PROVIDER_COOLDOWN_SECONDS
        .saturating_mul(multiplier)
        .min(MAX_PROVIDER_COOLDOWN_SECONDS)
}

/// Stable identity of one fair route. Workspace and role are intentional
/// dimensions: a provider/model should not borrow fairness credit from a
/// different project or a semantically different role.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FairnessKeyV1 {
    pub workspace_id: String,
    pub role: String,
    pub provider: String,
    pub model: String,
}

impl FairnessKeyV1 {
    pub fn new(
        workspace_id: impl Into<String>,
        role: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            role: role.into(),
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// Durable state for one fair route. `deficit` is signed because a large real
/// response may legitimately spend more than one quantum; later rounds let
/// peers catch up instead of pretending that work did not happen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FairnessStateV1 {
    pub schema_version: u16,
    pub key: FairnessKeyV1,
    pub deficit: i64,
    pub dispatched: u64,
    pub settled_work: u64,
    pub updated_at: DateTime<Utc>,
}

/// An eligible candidate passed from the policy layer to equal-weight WDRR.
/// Any policy exclusion (capability, pin, reserve, cooldown, or health) is
/// decided before this compact candidate reaches the fair scheduler.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FairnessCandidateV1 {
    pub provider: String,
    pub model: String,
}

impl FairnessCandidateV1 {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// The deterministic WDRR result persisted alongside a dispatch receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FairnessSelectionV1 {
    pub schema_version: u16,
    pub key: FairnessKeyV1,
    pub quantum: u64,
    pub estimated_work: u64,
    pub deficit_before: i64,
    pub deficit_after: i64,
}

/// Provider health is neither account-quota telemetry nor a price signal.
/// `Unknown` remains routable; it never implies either unlimited capacity or
/// exhaustion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatusV1 {
    #[default]
    Unknown,
    Healthy,
    CoolingDown,
    Unsupported,
    AuthenticationRequired,
}

impl ProviderHealthStatusV1 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Healthy => "healthy",
            Self::CoolingDown => "cooling_down",
            Self::Unsupported => "unsupported",
            Self::AuthenticationRequired => "authentication_required",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unknown" => Some(Self::Unknown),
            "healthy" => Some(Self::Healthy),
            "cooling_down" => Some(Self::CoolingDown),
            "unsupported" => Some(Self::Unsupported),
            "authentication_required" => Some(Self::AuthenticationRequired),
            _ => None,
        }
    }
}

/// Durable, inspectable provider health for one workspace/provider pair.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealthV1 {
    pub schema_version: u16,
    pub workspace_id: String,
    pub provider: String,
    pub status: ProviderHealthStatusV1,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub detail: String,
    pub updated_at: DateTime<Utc>,
}

impl ProviderHealthV1 {
    pub fn unknown(workspace_id: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            schema_version: PROVIDER_HEALTH_SCHEMA_VERSION,
            workspace_id: workspace_id.into(),
            provider: provider.into(),
            status: ProviderHealthStatusV1::Unknown,
            cooldown_until: None,
            consecutive_failures: 0,
            detail: "no provider telemetry has been observed".into(),
            updated_at: Utc::now(),
        }
    }

    pub fn cooldown_active_at(&self, now: DateTime<Utc>) -> bool {
        self.status == ProviderHealthStatusV1::CoolingDown
            && self.cooldown_until.is_some_and(|until| until > now)
    }
}

/// Compact state for a `/routing` inspector. It contains only local routing
/// evidence; no prompts, headers, credentials, or raw provider bodies.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RoutingInspectorV1 {
    pub fairness: Vec<FairnessStateV1>,
    pub providers: Vec<ProviderHealthV1>,
}

#[derive(Clone, Debug)]
struct RankedCandidate {
    key: FairnessKeyV1,
    deficit_before: i64,
    credited_deficit: i64,
}

/// Pick an equal-weight WDRR candidate from durable deficits. The caller must
/// credit every eligible state and persist the returned selected debit in the
/// same transaction. Ties intentionally use provider then model lexical order
/// rather than hash/map iteration order.
pub fn choose_equal_weight_wdrr(
    workspace_id: &str,
    role: &str,
    candidates: impl IntoIterator<Item = (FairnessCandidateV1, i64)>,
    estimated_work: u64,
) -> Option<FairnessSelectionV1> {
    let mut ranked = candidates
        .into_iter()
        .map(|(candidate, deficit_before)| RankedCandidate {
            key: FairnessKeyV1::new(workspace_id, role, candidate.provider, candidate.model),
            deficit_before,
            credited_deficit: deficit_before.saturating_add(WDRR_QUANTUM.min(i64::MAX as u64) as i64),
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .credited_deficit
            .cmp(&left.credited_deficit)
            .then_with(|| left.key.provider.cmp(&right.key.provider))
            .then_with(|| left.key.model.cmp(&right.key.model))
    });
    let selected = ranked.into_iter().next()?;
    let estimated_work = estimated_work.min(i64::MAX as u64);
    Some(FairnessSelectionV1 {
        schema_version: FAIRNESS_SCHEMA_VERSION,
        key: selected.key,
        quantum: WDRR_QUANTUM,
        estimated_work,
        deficit_before: selected.deficit_before,
        deficit_after: selected.credited_deficit.saturating_sub(estimated_work as i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_work_uses_the_fixed_provider_neutral_formula() {
        let work = normalized_token_work(TokenUsage {
            input: 100,
            cached_input: 20,
            output: 3,
            reasoning_output: 4,
            cache_write: 999,
        });
        assert_eq!(work, 133);
    }

    #[test]
    fn cooldown_backoff_starts_at_fifteen_seconds_and_caps_at_five_minutes() {
        assert_eq!(exponential_cooldown_seconds(0), 15);
        assert_eq!(exponential_cooldown_seconds(1), 15);
        assert_eq!(exponential_cooldown_seconds(2), 30);
        assert_eq!(exponential_cooldown_seconds(5), 240);
        assert_eq!(exponential_cooldown_seconds(6), 300);
        assert_eq!(exponential_cooldown_seconds(u32::MAX), 300);
    }

    #[test]
    fn equal_deficits_use_stable_provider_then_model_tie_breaking() {
        let selection = choose_equal_weight_wdrr(
            "workspace",
            "worker",
            [
                (FairnessCandidateV1::new("xiaomi_mimo", "xiaomi/mimo-v2.5"), 0),
                (
                    FairnessCandidateV1::new("deepseek", "deepseek/deepseek-v4-flash"),
                    0,
                ),
                (
                    FairnessCandidateV1::new("deepseek", "deepseek/deepseek-v4-pro"),
                    0,
                ),
            ],
            40,
        )
        .expect("candidate selected");
        assert_eq!(selection.key.provider, "deepseek");
        assert_eq!(selection.key.model, "deepseek/deepseek-v4-flash");
        assert_eq!(selection.deficit_after, 99_960);
    }

    #[test]
    fn larger_credit_wins_before_stable_tie_breaking() {
        let selection = choose_equal_weight_wdrr(
            "workspace",
            "worker",
            [
                (FairnessCandidateV1::new("chatgpt_codex", "chatgpt/a"), 0),
                (FairnessCandidateV1::new("xiaomi_mimo", "xiaomi/b"), 20),
            ],
            100,
        )
        .expect("candidate selected");
        assert_eq!(selection.key.provider, "xiaomi_mimo");
        assert_eq!(selection.deficit_before, 20);
    }
}
