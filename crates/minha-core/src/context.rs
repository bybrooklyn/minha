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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionConfig {
    pub context_limit: usize,
    pub reserve_tokens: usize,
    pub compact_at: f32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_limit: 128_000,
            reserve_tokens: 8_192,
            compact_at: 0.85,
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
}
