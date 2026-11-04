//! `config.toml`: model provider, server, bots and limits.
//!
//! Secrets never live in this file. It only names the environment variables
//! that hold them (`api_key_env`), and direnv loads those from `.env`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub llm: LlmConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    /// `openai` for any OpenAI-compatible endpoint, or `anthropic`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Base URL of the API. `base_url_env` overrides it when that variable is set.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub base_url_env: Option<String>,
    /// Name of the environment variable that holds the API key. Leave empty
    /// for endpoints that need no key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Cheap and fast: chat replies and simple replans.
    pub fast_model: String,
    /// Strong: planning, skill writing, dream consolidation.
    pub strong_model: String,
    #[serde(default)]
    pub fast_model_env: Option<String>,
    #[serde(default)]
    pub strong_model_env: Option<String>,
    /// Extra fields sent with every request to each tier, for example
    /// `{ reasoning_effort = "low" }` to make replies faster.
    #[serde(default)]
    pub fast_params: std::collections::HashMap<String, toml::Value>,
    #[serde(default)]
    pub strong_params: std::collections::HashMap<String, toml::Value>,
}

fn default_kind() -> String {
    "openai".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_address")]
    pub address: String,
    /// Real minutes per Minecraft day. Vanilla is 20; shorter days make the
    /// demo reach dusk sooner. Needs MC_CONSOLE.
    #[serde(default = "default_day_minutes")]
    pub day_minutes: f64,
    /// Console commands run once when the bots start. Needs MC_CONSOLE.
    #[serde(default = "default_gamerules")]
    pub on_start: Vec<String>,
}

fn default_day_minutes() -> f64 {
    20.0
}

fn default_gamerules() -> Vec<String> {
    vec![
        "gamerule players_sleeping_percentage 0".into(),
        "gamerule keep_inventory false".into(),
    ]
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Worker,
    God,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: default_address(),
            day_minutes: default_day_minutes(),
            on_start: default_gamerules(),
        }
    }
}

fn default_address() -> String {
    "localhost:25565".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub name: String,
    /// `worker` (default) lives in the world; `god` runs the world through
    /// the server console: weather, time, lightning, teleports, gifts.
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub personality: String,
    /// 0.0 = races home at the first sign of dusk, 1.0 = pushes on into the night.
    #[serde(default = "default_risk")]
    pub risk_appetite: f32,
    #[serde(default)]
    pub fast_model: Option<String>,
    #[serde(default)]
    pub strong_model: Option<String>,
}

fn default_risk() -> f32 {
    0.5
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    /// Model calls a bot may make per in-game day before it stops thinking.
    #[serde(default = "default_calls_per_day")]
    pub llm_calls_per_day: u32,
    /// Model calls inside one planning turn (tool round-trips included).
    #[serde(default = "default_inferences_per_turn")]
    pub inferences_per_turn: u32,
    #[serde(default = "default_skill_retries")]
    pub skill_write_retries: u32,
    #[serde(default)]
    pub hardcore: bool,
    /// Gives up on a model turn that takes longer than this.
    #[serde(default = "default_turn_timeout")]
    pub turn_timeout_secs: u64,
    /// A bot with no goal for this many seconds picks its own. 0 = never.
    #[serde(default = "default_idle_goal")]
    pub idle_goal_secs: u64,
    /// A god bot acts on a whim this often, unasked. 0 = only when called.
    #[serde(default = "default_god_whim")]
    pub god_whim_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            llm_calls_per_day: default_calls_per_day(),
            inferences_per_turn: default_inferences_per_turn(),
            skill_write_retries: default_skill_retries(),
            hardcore: false,
            turn_timeout_secs: default_turn_timeout(),
            idle_goal_secs: default_idle_goal(),
            god_whim_secs: default_god_whim(),
        }
    }
}

fn default_calls_per_day() -> u32 {
    60
}
fn default_inferences_per_turn() -> u32 {
    6
}
fn default_god_whim() -> u64 {
    0
}
fn default_idle_goal() -> u64 {
    0
}
fn default_turn_timeout() -> u64 {
    180
}
fn default_skill_retries() -> u32 {
    3
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.llm.apply_env();
        Ok(config)
    }
}

impl LlmConfig {
    fn apply_env(&mut self) {
        let from_env = |name: &Option<String>| {
            name.as_deref()
                .and_then(|n| std::env::var(n).ok())
                .filter(|v| !v.is_empty())
        };
        if let Some(url) = from_env(&self.base_url_env) {
            self.base_url = url;
        }
        if let Some(model) = from_env(&self.fast_model_env) {
            self.fast_model = model;
        }
        if let Some(model) = from_env(&self.strong_model_env) {
            self.strong_model = model;
        }
    }

    pub fn api_key(&self) -> Result<Option<String>> {
        match self.api_key_env.as_deref().filter(|n| !n.is_empty()) {
            None => Ok(None),
            Some(name) => std::env::var(name)
                .map(Some)
                .with_context(|| format!("environment variable {name} is not set")),
        }
    }
}
