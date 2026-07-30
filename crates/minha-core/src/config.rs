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
    #[error("scheduler.usage_reserve_percent must be between 0 and 50")]
    UsageReserve,
    #[error("budgets DeepSeek reserves must satisfy 0 <= hard < soft <= 100")]
    DeepSeekReserve,
    #[error("context.context_limit must be positive when configured")]
    ContextWindow,
    #[error("context.fact_limit and context.recent_turns must be positive")]
    ContextRetention,
    #[error("cache.max_bytes, cache.max_age_days, and cache.hot_entries must be positive")]
    CachePolicy,
    #[error("memory.retrieval_limit must be between 1 and 20")]
    MemoryPolicy,
    #[error("configured model slugs must not be empty")]
    EmptyModel,
    #[error("tui.tool_detail must be `compact` or `expanded`")]
    ToolDetail,
    #[error("tui.theme must be auto, dark, light, ansi16, high_contrast, or no_color")]
    Theme,
    #[error("tui.surface_renderer must be auto, kitty, quadrant, or square")]
    SurfaceRenderer,
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
    pub memory: MemoryConfig,
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
    /// Optional compatibility ceiling. Provider model metadata is authoritative
    /// when this is absent.
    pub context_limit: Option<usize>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub use_memory: bool,
    pub generate: bool,
    pub retrieval_limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfile {
    Economy,
    Balanced,
    Turbo,
}

impl ExecutionProfile {
    /// A routing target, never a terminal budget.
    pub const fn soft_token_target(self) -> u64 {
        match self {
            Self::Economy => 25_000,
            Self::Balanced => 100_000,
            Self::Turbo => 1_000_000,
        }
    }

    pub const fn policy(self) -> RunPolicyV1 {
        match self {
            Self::Economy => RunPolicyV1 {
                schema_version: 1,
                max_agents: 1,
                minimum_speedup_percent: 100,
                maximum_coordination_percent: 0,
                local_yolo: false,
                prefer_strongest_allowed: false,
            },
            Self::Balanced => RunPolicyV1 {
                schema_version: 1,
                max_agents: 8,
                minimum_speedup_percent: 25,
                maximum_coordination_percent: 15,
                local_yolo: false,
                prefer_strongest_allowed: false,
            },
            Self::Turbo => RunPolicyV1 {
                schema_version: 1,
                max_agents: 16,
                minimum_speedup_percent: 1,
                maximum_coordination_percent: 100,
                local_yolo: true,
                prefer_strongest_allowed: true,
            },
        }
    }
}

/// Compact model-facing execution policy. Profiles are presets; callers may
/// override individual fields per run without loading dynamic policy code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunPolicyV1 {
    pub schema_version: u16,
    pub max_agents: usize,
    pub minimum_speedup_percent: u8,
    pub maximum_coordination_percent: u8,
    pub local_yolo: bool,
    pub prefer_strongest_allowed: bool,
}

pub type BudgetPreset = ExecutionProfile;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub default: ExecutionProfile,
    pub deepseek_soft_reserve_percent: f32,
    pub deepseek_hard_reserve_percent: f32,
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
    pub theme: String,
    pub surface_renderer: String,
    pub reduced_motion: bool,
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
            memory: MemoryConfig::default(),
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
            context_limit: None,
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

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            use_memory: true,
            generate: true,
            retrieval_limit: 5,
        }
    }
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            default: ExecutionProfile::Balanced,
            deepseek_soft_reserve_percent: 10.0,
            deepseek_hard_reserve_percent: 2.0,
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
            theme: "dark".into(),
            surface_renderer: "auto".into(),
            reduced_motion: false,
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
        if self.scheduler.hard_max_agents < self.scheduler.max_agents || self.scheduler.hard_max_agents > 16 {
            return Err(ConfigError::AgentCount);
        }
        if !(0.0..=50.0).contains(&self.scheduler.usage_reserve_percent) {
            return Err(ConfigError::UsageReserve);
        }
        if !(0.0..=100.0).contains(&self.budgets.deepseek_soft_reserve_percent)
            || !(0.0..=100.0).contains(&self.budgets.deepseek_hard_reserve_percent)
            || self.budgets.deepseek_hard_reserve_percent >= self.budgets.deepseek_soft_reserve_percent
        {
            return Err(ConfigError::DeepSeekReserve);
        }
        if self.context.context_limit == Some(0) {
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
        if self.memory.retrieval_limit == 0 || self.memory.retrieval_limit > 20 {
            return Err(ConfigError::MemoryPolicy);
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
        if !matches!(
            self.tui.theme.as_str(),
            "auto" | "dark" | "light" | "ansi16" | "high_contrast" | "no_color"
        ) {
            return Err(ConfigError::Theme);
        }
        if !matches!(
            self.tui.surface_renderer.as_str(),
            "auto" | "kitty" | "quadrant" | "square"
        ) {
            return Err(ConfigError::SurfaceRenderer);
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
        assert_eq!(config.memory, MemoryConfig::default());
        assert_eq!(config.tui.theme, "dark");
        assert_eq!(config.tui.surface_renderer, "auto");
        assert!(!config.tui.reduced_motion);
    }

    #[test]
    fn context_reserve_must_fit_inside_the_window() {
        let error = Config::from_toml("[context]\ncontext_limit = 0\n").unwrap_err();
        assert!(matches!(error, ConfigError::ContextWindow));
    }

    #[test]
    fn execution_profiles_are_soft_versioned_policies() {
        let economy = ExecutionProfile::Economy.policy();
        let balanced = ExecutionProfile::Balanced.policy();
        let turbo = ExecutionProfile::Turbo.policy();
        assert_eq!(economy.max_agents, 1);
        assert_eq!(balanced.minimum_speedup_percent, 25);
        assert_eq!(balanced.maximum_coordination_percent, 15);
        assert_eq!(turbo.max_agents, 16);
        assert!(turbo.local_yolo);
        assert_eq!(turbo.schema_version, 1);
        assert_eq!(ExecutionProfile::Balanced.soft_token_target(), 100_000);
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

    #[test]
    fn memory_limit_and_theme_are_validated() {
        let memory = Config::from_toml("[memory]\nretrieval_limit = 21")
            .expect_err("oversized memory retrieval must fail validation");
        assert!(matches!(memory, ConfigError::MemoryPolicy));

        let theme =
            Config::from_toml("[tui]\ntheme = 'solarized'").expect_err("unknown theme must fail validation");
        assert!(matches!(theme, ConfigError::Theme));
        let renderer = Config::from_toml("[tui]\nsurface_renderer = 'bezier'")
            .expect_err("unknown surface renderer must fail validation");
        assert!(matches!(renderer, ConfigError::SurfaceRenderer));

        let configured = Config::from_toml(
            "[memory]\nenabled = false\nuse_memory = false\ngenerate = false\nretrieval_limit = 3\n[tui]\ntheme = 'ansi16'\nreduced_motion = true",
        )
        .expect("supported memory and TUI settings");
        assert!(!configured.memory.enabled);
        assert_eq!(configured.memory.retrieval_limit, 3);
        assert_eq!(configured.tui.theme, "ansi16");
        assert!(configured.tui.reduced_motion);
    }
}
