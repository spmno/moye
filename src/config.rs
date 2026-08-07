// 统一配置模块：agent.toml 只解析一次，全 crate 共享。
// Unified config module: agent.toml is parsed once and shared across the crate.
//
// 此前 agent.toml 被四处独立解析（AgentRegistryConfig::load、load_memory_cfg、
// load_escalation_threshold、resolve_default_model），且 [provider] 段是装饰性配置。
// 现在由 main 调用 `config::init()` 一次性解析并缓存，所有模块通过持有的
// Arc<Config> 或 `config::config()` 读取。
// Previously agent.toml was parsed independently in four places (AgentRegistryConfig::load,
// load_memory_cfg, load_escalation_threshold, resolve_default_model), and the [provider]
// section was decorative. Now main calls `config::init()` to parse once and cache it; all
// modules read via the Arc<Config> they hold or `config::config()`.

use crate::context::ContextConfig;
use crate::memory::MemoryConfig;
use crate::registry::RoleConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// 顶层配置：对应 agent.toml 的全部小节。
/// Top-level config: all sections of agent.toml.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderSection,
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default, rename = "agents")]
    pub roles: HashMap<String, RoleConfig>,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub evolution: EvolutionSection,
    /// 沙箱配置：预授权目录列表等。
    /// Sandbox config: pre-authorized directory list, etc.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// 全局 API key 存储：`[keys]` section，键为环境变量名（如 `DEEPSEEK_API_KEY`），
    /// 值为 key 本身。当前目录 `.env`/export 优先，缺失时回退此处。
    /// Global API key store: `[keys]` section, keyed by env var name (e.g. `DEEPSEEK_API_KEY`),
    /// valued as the key itself. The project `.env`/export takes priority; this is the fallback.
    #[serde(default)]
    pub keys: HashMap<String, String>,
}

/// `[sandbox]` 小节：沙箱预授权目录配置。
/// The `[sandbox]` section: sandbox pre-authorized directory config.
///
/// 通过 `authorized_dirs` 可在配置文件中预先授权一组目录，
/// Agent 访问这些目录及其子目录时不再弹窗确认。
/// You can pre-authorize a set of directories via `authorized_dirs` in the config file;
/// the Agent can access these directories and their subdirectories without prompting.
#[derive(Debug, Deserialize, Default)]
pub struct SandboxConfig {
    /// 预授权目录列表（支持 `~` 展开）。这些目录及其子目录可直接访问，无需确认。
    /// Pre-authorized directory list (supports `~` expansion). These directories and
    /// their subdirectories can be accessed without confirmation.
    #[serde(default)]
    pub authorized_dirs: Vec<String>,
}

impl Config {
    /// 从 agent.toml 解析。整个 crate 仅此处解析该文件。
    /// Parses from agent.toml. This is the only place the file is parsed.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&raw)?)
    }

    /// 自主循环轮数上限；为 0 时回退到默认 30。
    /// Max turns for the autonomous loop; falls back to 30 when 0.
    pub fn max_turns(&self) -> usize {
        let turns = self.agent.max_turns;
        if turns == 0 {
            30
        } else {
            turns
        }
    }
}

/// `[provider]` 小节：默认供应商与可选覆盖（env 优先于文件）。
/// The `[provider]` section: default provider and optional overrides (env wins over file).
#[derive(Debug, Deserialize, Default)]
pub struct ProviderSection {
    /// 默认供应商 slug（deepseek / bailian / moonshot / custom）。
    /// `MY_AGENT_PROVIDER` 环境变量优先于此值。
    pub provider: Option<String>,
    /// 全局 OpenAI 兼容 base URL 覆盖。`MY_AGENT_BASE_URL` 环境变量优先。
    pub base_url: Option<String>,
    /// 自定义 API key 环境变量名；为空时按供应商自动选择。
    pub api_key_env: Option<String>,
}

/// `[agent]` 小节：默认模型与循环上限。
/// The `[agent]` section: default model and loop limit.
#[derive(Debug, Deserialize, Default)]
pub struct AgentSection {
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub max_turns: usize,
}

/// `[evolution]` 小节：规则提升阈值。
/// The `[evolution]` section: rule escalation threshold.
#[derive(Debug, Deserialize, Default)]
pub struct EvolutionSection {
    pub rule_escalation_threshold: usize,
}

static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

/// 启动时调用一次：加载项目 `agent.toml`，再用全局 `~/.config/my-agent/config.toml`
/// 作为 fallback 填充项目中缺失的 `[provider]` 字段，最后缓存并返回共享 Arc。
/// 合并优先级：环境变量 > 项目 agent.toml > 全局 config.toml > 供应商默认。
/// Call once at startup: loads the project `agent.toml`, then fills in any missing
/// `[provider]` fields from the global `~/.config/my-agent/config.toml` as a fallback,
/// before caching and returning the shared Arc.
/// Precedence: env vars > project agent.toml > global config.toml > provider default.
pub fn init(path: &str) -> anyhow::Result<Arc<Config>> {
    let mut cfg = Config::load(path)?;
    // 全局配置仅作为 provider 小节的 fallback（项目优先）。
    // Global config only backfills the provider section (project wins).
    if let Some(global_path) = global_config_path() {
        if global_path.exists() {
            if let Ok(global) = Config::load(global_path.to_string_lossy().as_ref()) {
                merge_provider_fallback(&mut cfg, global);
            }
        }
    }
    let cfg = Arc::new(cfg);
    Ok(CONFIG.get_or_init(|| cfg).clone())
}

/// 返回全局配置路径 `~/.config/my-agent/config.toml`；`HOME` 未设置时返回 `None`。
/// Return the global config path `~/.config/my-agent/config.toml`; `None` when `HOME` is unset.
fn global_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("my-agent")
            .join("config.toml"),
    )
}

/// 用全局配置填充项目中为 `None` 的 `[provider]` 字段，并把全局 `[keys]` 中
/// 项目缺失的条目补全进来。项目已设的值不被覆盖。
/// Fill `None` `[provider]` fields of the project config from the global config, and
/// backfill any `[keys]` entries the project is missing. Values already set in the
/// project config are not overwritten.
fn merge_provider_fallback(project: &mut Config, global: Config) {
    let g = global.provider;
    if project.provider.provider.is_none() {
        project.provider.provider = g.provider;
    }
    if project.provider.base_url.is_none() {
        project.provider.base_url = g.base_url;
    }
    if project.provider.api_key_env.is_none() {
        project.provider.api_key_env = g.api_key_env;
    }
    // 全局 key 回退：项目 [keys] 没有的条目用全局补全（项目优先，不覆盖已设的）。
    // Global key fallback: entries missing from the project [keys] are backfilled from
    // the global config (project wins; existing entries are not overwritten).
    for (k, v) in global.keys {
        project.keys.entry(k).or_insert(v);
    }
}

/// 返回缓存的配置；未初始化（如纯单元测试）时返回 None。
/// Returns the cached config, or None when not yet initialized (e.g. unit tests).
pub fn config() -> Option<&'static Config> {
    CONFIG.get().map(|c| c.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_full_config() {
        let toml_str = r#"
[provider]
provider = "moonshot"
base_url = "https://custom.example.com/v1"
api_key_env = "MY_CUSTOM_KEY"

[agent]
default_model = "kimi-k3"
max_turns = 10

[context]
max_output_tokens = 2048

[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.edit_file = "allow"

[memory]
dir = "memory"
conversation_file = "conv.jsonl"
lessons_file = "less.jsonl"
rules_file = "rules.json"

[evolution]
rule_escalation_threshold = 5

[sandbox]
authorized_dirs = ["~/.config", "/tmp/my-agent"]
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.provider.provider.as_deref(), Some("moonshot"));
        assert_eq!(
            cfg.provider.base_url.as_deref(),
            Some("https://custom.example.com/v1")
        );
        assert_eq!(cfg.provider.api_key_env.as_deref(), Some("MY_CUSTOM_KEY"));
        assert_eq!(cfg.agent.default_model, "kimi-k3");
        assert_eq!(cfg.max_turns(), 10);
        assert_eq!(cfg.context.max_output_tokens, 2048);
        assert!(cfg.roles.contains_key("builder"));
        assert_eq!(cfg.memory.dir, PathBuf::from("memory"));
        assert_eq!(cfg.memory.rules_file, "rules.json");
        assert_eq!(cfg.evolution.rule_escalation_threshold, 5);
        assert_eq!(cfg.sandbox.authorized_dirs.len(), 2);
        assert_eq!(cfg.sandbox.authorized_dirs[0], "~/.config");
        assert_eq!(cfg.sandbox.authorized_dirs[1], "/tmp/my-agent");
    }

    #[test]
    fn load_empty_config_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.max_turns(), 30);
        assert!(cfg.roles.is_empty());
        assert_eq!(cfg.evolution.rule_escalation_threshold, 0);
        assert_eq!(cfg.context.max_output_tokens, 4096);
        assert!(cfg.sandbox.authorized_dirs.is_empty());
        assert_eq!(cfg.memory.rules_file, "rules.json");
    }

    #[test]
    fn max_turns_zero_falls_back() {
        let cfg: Config = toml::from_str("[agent]\nmax_turns = 0\n").unwrap();
        assert_eq!(cfg.max_turns(), 30);
    }

    #[test]
    fn merge_provider_fallback_keeps_project_and_fills_none() {
        // 项目已设的 provider 不被全局覆盖；为 None 的 base_url / api_key_env 用全局回填。
        // A provider already set in the project is not overwritten by the global; None
        // base_url / api_key_env are backfilled from the global config.
        let mut project: Config = toml::from_str(
            r#"
[provider]
provider = "custom"
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[provider]
provider = "deepseek"
base_url = "https://gw.example.com/v1"
api_key_env = "GLOBAL_KEY"
"#,
        )
        .unwrap();
        merge_provider_fallback(&mut project, global);
        assert_eq!(project.provider.provider.as_deref(), Some("custom"));
        assert_eq!(
            project.provider.base_url.as_deref(),
            Some("https://gw.example.com/v1")
        );
        assert_eq!(project.provider.api_key_env.as_deref(), Some("GLOBAL_KEY"));
    }

    #[test]
    fn merge_keys_project_wins_and_global_backfills() {
        // 项目已设的 key 不被全局覆盖；项目缺失的由全局补全（当前目录优先）。
        // A key set in the project is not overwritten by the global; missing keys are
        // backfilled from the global (current dir wins).
        let mut project: Config = toml::from_str(
            r#"
[keys]
DEEPSEEK_API_KEY = "project-key"
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[keys]
DEEPSEEK_API_KEY = "global-key"
MOONSHOT_API_KEY = "global-moon"
"#,
        )
        .unwrap();
        merge_provider_fallback(&mut project, global);
        assert_eq!(project.keys.get("DEEPSEEK_API_KEY").unwrap(), "project-key");
        assert_eq!(project.keys.get("MOONSHOT_API_KEY").unwrap(), "global-moon");
    }
}
