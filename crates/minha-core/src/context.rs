//! Context accounting and predictive compaction.
//!
//! This module deliberately uses a cheap, deterministic token estimate.  The
//! estimator is useful for deciding when to compact; it is not intended to be
//! a replacement for a model tokenizer.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.content) + 4
    }
}

/// Conservative, model-independent estimate (four characters per token, with
/// whitespace and punctuation accounted for as separate token boundaries).
pub fn estimate_tokens(text: &str) -> usize {
    if text.trim().is_empty() {
        return 0;
    }
    let words = text.split_whitespace().count();
    let punctuation = text.chars().filter(|c| c.is_ascii_punctuation()).count();
    text.chars().count().div_ceil(4).max(words + punctuation / 2)
}

pub const EFFECTIVE_CONTEXT_PERCENT: u64 = 95;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilitySource {
    ProviderCatalog,
    FallbackTable,
    LegacyCeiling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextPolicy {
    pub advertised_limit: u64,
    pub effective_limit: u64,
    pub protected_reserve: u64,
    pub output_allowance: u64,
    pub source: CapabilitySource,
}

impl ContextPolicy {
    pub fn resolve(model: &str, catalog_limit: Option<u64>, legacy_ceiling: Option<usize>) -> Self {
        let (advertised_limit, mut source) = catalog_limit
            .map(|limit| (limit, CapabilitySource::ProviderCatalog))
            .unwrap_or_else(|| (fallback_context_window(model), CapabilitySource::FallbackTable));
        let mut limit = advertised_limit;
        if let Some(ceiling) = legacy_ceiling.map(|value| value as u64)
            && ceiling < limit
        {
            limit = ceiling;
            source = CapabilitySource::LegacyCeiling;
        }
        let effective_limit = limit.saturating_mul(EFFECTIVE_CONTEXT_PERCENT) / 100;
        Self {
            advertised_limit,
            effective_limit,
            protected_reserve: limit.saturating_sub(effective_limit),
            output_allowance: default_output_allowance(model).min(effective_limit / 4),
            source,
        }
    }

    pub const fn forecast_fits(self, current: u64, pending: u64) -> bool {
        current
            .saturating_add(pending)
            .saturating_add(self.output_allowance)
            <= self.effective_limit
    }
}

pub fn fallback_context_window(model: &str) -> u64 {
    let model = model.to_ascii_lowercase();
    if model.contains("deepseek-v4") {
        1_048_576
    } else if model.contains("spark") {
        128_000
    } else {
        272_000
    }
}

pub fn default_output_allowance(model: &str) -> u64 {
    if model.to_ascii_lowercase().contains("spark") {
        8_192
    } else {
        16_384
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionConfig {
    pub context_limit: usize,
    pub reserve_tokens: usize,
    pub compact_at: f32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_limit: 272_000,
            reserve_tokens: 13_600,
            compact_at: 0.95,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionPlan {
    pub should_compact: bool,
    pub estimated_tokens: usize,
    pub target_tokens: usize,
    pub retain_from: usize,
}

/// Predict whether the next turn can fit, and identify the oldest messages
/// which may be summarized.  No compaction is performed here: callers can
/// choose the summary mechanism appropriate to their model.
pub fn predictive_compaction(
    messages: &[Message],
    next_turn_tokens: usize,
    config: CompactionConfig,
) -> CompactionPlan {
    let used: usize = messages.iter().map(Message::estimated_tokens).sum();
    let total = used.saturating_add(next_turn_tokens);
    let trigger = ((config.context_limit as f32) * config.compact_at) as usize;
    let target = config.context_limit.saturating_sub(config.reserve_tokens);
    let should = total >= trigger || total > config.context_limit;
    if !should {
        return CompactionPlan {
            should_compact: false,
            estimated_tokens: total,
            target_tokens: target,
            retain_from: 0,
        };
    }
    let mut retained = total;
    let mut from = 0;
    while from < messages.len() && retained > target {
        retained = retained.saturating_sub(messages[from].estimated_tokens());
        from += 1;
    }
    CompactionPlan {
        should_compact: true,
        estimated_tokens: total,
        target_tokens: target,
        retain_from: from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn predicts_before_hard_limit() {
        let m = vec![Message::new(Role::User, "a".repeat(80))];
        let p = predictive_compaction(
            &m,
            0,
            CompactionConfig {
                context_limit: 30,
                reserve_tokens: 5,
                compact_at: 0.8,
            },
        );
        assert!(p.should_compact);
        assert!(p.retain_from <= m.len());
    }
    #[test]
    fn empty_context_is_not_compacted() {
        assert!(!predictive_compaction(&[], 1, CompactionConfig::default()).should_compact);
    }

    #[test]
    fn fallback_limits_keep_five_percent_protected() {
        let luna = ContextPolicy::resolve("gpt-5.6-luna", None, None);
        assert_eq!(luna.advertised_limit, 272_000);
        assert_eq!(luna.effective_limit, 258_400);
        assert_eq!(luna.protected_reserve, 13_600);
        let spark = ContextPolicy::resolve("gpt-5.3-codex-spark", None, None);
        assert_eq!(spark.advertised_limit, 128_000);
        assert_eq!(spark.effective_limit, 121_600);
        let deepseek = ContextPolicy::resolve("deepseek-v4-pro", None, None);
        assert_eq!(deepseek.advertised_limit, 1_048_576);
        assert_eq!(deepseek.effective_limit, 996_147);
        assert_eq!(deepseek.protected_reserve, 52_429);
    }

    #[test]
    fn provider_metadata_precedes_fallback_and_legacy_is_only_a_ceiling() {
        let catalog = ContextPolicy::resolve("gpt-5.6-luna", Some(300_000), None);
        assert_eq!(catalog.advertised_limit, 300_000);
        assert_eq!(catalog.source, CapabilitySource::ProviderCatalog);
        let capped = ContextPolicy::resolve("gpt-5.6-luna", Some(300_000), Some(200_000));
        assert_eq!(capped.advertised_limit, 300_000);
        assert_eq!(capped.effective_limit, 190_000);
        assert_eq!(capped.source, CapabilitySource::LegacyCeiling);
    }
}
