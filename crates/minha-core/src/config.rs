//! Layered runtime configuration.

use crate::protocol::Mode;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid merged config: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("config database path must not be empty")]
    EmptyDatabasePath,
    #[error("scheduler.max_agents must be between 1 and 16")]
    AgentCount,
    #[error("scheduler.min_agents must be between 1 and scheduler.max_agents")]
    MinAgentCount,
    #[error("context.compact_at_percent must be between 25 and 95")]
    CompactThreshold,
    #[error("scheduler.usage_reserve_percent must be between 0 and 50")]
    UsageReserve,
    #[error("context.context_limit must be positive and context.reserve_tokens must be smaller")]
    ContextWindow,
    #[error("context.fact_limit and context.recent_turns must be positive")]
    ContextRetention,
    #[error("cache.max_bytes, cache.max_age_days, and cache.hot_entries must be positive")]
    CachePolicy,
    #[error("configured model slugs must not be empty")]
    EmptyModel,
    #[error("tui.tool_detail must be `compact` or `expanded`")]
    ToolDetail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub database_path: PathBuf,
    pub mode: Mode,
    pub models: ModelConfig,
    pub scheduler: SchedulerConfig,
    pub context: ContextConfig,
    pub permissions: PermissionConfig,
    pub cache: CacheConfig,
    pub budgets: BudgetConfig,
    pub books: BooksConfig,
    pub tui: TuiConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub planner: String,
    pub worker_fast: String,
    pub lead: String,
    pub complex_lead: String,
    pub manager: String,
    pub worker_medium: String,
    pub worker_deep: String,
    pub consult_ambiguous: String,
    pub consult_high_risk: String,
    pub reasoning_effort: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    pub min_agents: usize,
    pub max_agents: usize,
    pub hard_max_agents: usize,
    pub usage_reserve_percent: f32,
    pub question_policy: QuestionPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionPolicy {
    AgentDiscretion,
    OnlyBlocking,
    Never,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub compact_at_percent: f32,
    pub context_limit: usize,
    pub reserve_tokens: usize,
    pub fact_limit: usize,
    pub recent_turns: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionConfig {
    pub remote_writes: PermissionLevel,
    pub destructive: PermissionLevel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_bytes: u64,
    pub max_age_days: u32,
    pub hot_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPreset {
    Economy,
    Balanced,
    Thorough,
    Exhaustive,
}

impl BudgetPreset {
    pub const fn token_limit(self) -> u64 {
        match self {
            Self::Economy => 25_000,
            Self::Balanced => 100_000,
            Self::Thorough => 300_000,
            Self::Exhaustive => 1_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub default: BudgetPreset,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BooksConfig {
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    Deny,
    Ask,
    Allow,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub mouse: bool,
    pub tool_detail: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from(".minha/minha.sqlite3"),
            mode: Mode::Interactive,
            models: ModelConfig::default(),
            scheduler: SchedulerConfig::default(),
            context: ContextConfig::default(),
            permissions: PermissionConfig::default(),
            cache: CacheConfig::default(),
            budgets: BudgetConfig::default(),
            books: BooksConfig::default(),
            tui: TuiConfig::default(),
        }
    }
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            planner: "gpt-5.6-luna".into(),
            worker_fast: "gpt-5.3-codex-spark".into(),
            lead: "gpt-5.6-luna".into(),
            complex_lead: "gpt-5.6-terra".into(),
            manager: "gpt-5.4-mini".into(),
            worker_medium: "gpt-5.4-mini".into(),
            worker_deep: "gpt-5.6-luna".into(),
            consult_ambiguous: "gpt-5.6-terra".into(),
            consult_high_risk: "gpt-5.6-sol".into(),
            reasoning_effort: "medium".into(),
        }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            min_agents: 2,
            max_agents: 8,
            hard_max_agents: 16,
            usage_reserve_percent: 12.0,
            question_policy: QuestionPolicy::OnlyBlocking,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compact_at_percent: 72.0,
            context_limit: 128_000,
            reserve_tokens: 16_384,
            fact_limit: 24,
            recent_turns: 8,
        }
    }
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            remote_writes: PermissionLevel::Ask,
            destructive: PermissionLevel::Ask,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 512 * 1024 * 1024,
            max_age_days: 30,
            hot_entries: 128,
        }
    }
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            default: BudgetPreset::Balanced,
        }
    }
}

impl Default for BooksConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            tool_detail: "compact".into(),
        }
    }
}

impl Config {
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref().to_owned();
        Self::from_toml(&fs::read_to_string(&path).map_err(|source| ConfigError::Read { path, source })?)
    }

    /// Load defaults, then an optional user config, then `minha.toml` in the
    /// project. Maps merge recursively so a small project file does not erase
    /// unrelated defaults.
    pub fn discover(project_root: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let project_root = project_root.as_ref();
        let mut merged = toml::Value::try_from(Self::default())?;
        if let Some(user) = dirs::config_dir().map(|dir| dir.join("minha/config.toml"))
            && user.is_file()
        {
            merge_toml(
                &mut merged,
                toml::from_str(&fs::read_to_string(&user).map_err(|source| ConfigError::Read {
                    path: user.clone(),
                    source,
                })?)?,
            );
        }
        let project = project_root.join("minha.toml");
        if project.is_file() {
            merge_toml(
                &mut merged,
                toml::from_str(&fs::read_to_string(&project).map_err(|source| ConfigError::Read {
                    path: project.clone(),
                    source,
                })?)?,
            );
        }
        let mut config: Self = merged.try_into().map_err(ConfigError::Parse)?;
        if config.database_path.is_relative() {
            config.database_path = project_root.join(&config.database_path);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.database_path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyDatabasePath);
        }
        if !(1..=16).contains(&self.scheduler.max_agents) {
            return Err(ConfigError::AgentCount);
        }
        if self.scheduler.min_agents == 0
            || self.scheduler.min_agents > self.scheduler.max_agents
            || self.scheduler.hard_max_agents < self.scheduler.max_agents
            || self.scheduler.hard_max_agents > 16
        {
            return Err(ConfigError::MinAgentCount);
        }
        if !(25.0..=95.0).contains(&self.context.compact_at_percent) {
            return Err(ConfigError::CompactThreshold);
        }
        if !(0.0..=50.0).contains(&self.scheduler.usage_reserve_percent) {
            return Err(ConfigError::UsageReserve);
        }
        if self.context.context_limit == 0 || self.context.reserve_tokens >= self.context.context_limit {
            return Err(ConfigError::ContextWindow);
        }
        if self.context.fact_limit == 0 || self.context.recent_turns == 0 {
            return Err(ConfigError::ContextRetention);
        }
        if self.cache.max_bytes == 0
            || self.cache.max_bytes > i64::MAX as u64
            || self.cache.max_age_days == 0
            || self.cache.hot_entries == 0
        {
            return Err(ConfigError::CachePolicy);
        }
        let models = &self.models;
        if [
            &models.planner,
            &models.worker_fast,
            &models.lead,
            &models.complex_lead,
            &models.manager,
            &models.worker_medium,
            &models.worker_deep,
            &models.consult_ambiguous,
            &models.consult_high_risk,
        ]
        .iter()
        .any(|model| model.trim().is_empty())
        {
            return Err(ConfigError::EmptyModel);
        }
        if !matches!(self.tui.tool_detail.as_str(), "compact" | "expanded") {
            return Err(ConfigError::ToolDetail);
        }
        Ok(())
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Interactive => "interactive",
            Mode::Batch => "batch",
            Mode::Review => "review",
        }
    }
}

fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(current) = base.get_mut(&key) {
                    merge_toml(current, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

impl FromStr for Config {
    type Err = ConfigError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_toml(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_typed_values_are_applied() {
        let config = Config::from_toml("mode = 'batch'\ndatabase_path = 'state.db'")
            .expect("test operation should succeed");
        assert_eq!(config.mode, Mode::Batch);
        assert_eq!(config.models.lead, "gpt-5.6-luna");
        assert_eq!(config.scheduler.max_agents, 8);
    }

    #[test]
    fn context_reserve_must_fit_inside_the_window() {
        let error = Config::from_toml("[context]\ncontext_limit = 100\nreserve_tokens = 100\n").unwrap_err();
        assert!(matches!(error, ConfigError::ContextWindow));
    }

    #[test]
    fn project_overlay_is_recursive() {
        let dir = tempfile::tempdir().expect("test operation should succeed");
        fs::write(
            dir.path().join("minha.toml"),
            "[scheduler]\nmax_agents = 3\n[models]\nlead = 'gpt-5.6-luna'\n",
        )
        .expect("test operation should succeed");
        let config = Config::discover(dir.path()).expect("test operation should succeed");
        assert_eq!(config.scheduler.max_agents, 3);
        assert_eq!(config.models.worker_fast, "gpt-5.3-codex-spark");
        assert!(config.database_path.starts_with(dir.path()));
    }

    #[test]
    fn empty_database_path_is_rejected() {
        let error = Config::from_toml("database_path = ''").unwrap_err();
        assert!(matches!(error, ConfigError::EmptyDatabasePath));
    }

    #[test]
    fn empty_models_and_unknown_tool_density_are_rejected() {
        let model = Config::from_toml("[models]\nworker_fast = '  '")
            .expect_err("empty model slug must fail validation");
        assert!(matches!(model, ConfigError::EmptyModel));

        let detail = Config::from_toml("[tui]\ntool_detail = 'verbose'")
            .expect_err("unknown tool detail must fail validation");
        assert!(matches!(detail, ConfigError::ToolDetail));
    }
}
