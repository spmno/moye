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

    /// 自主循环轮数上限；为 0 时回退到默认 50。
    /// Max turns for the autonomous loop; falls back to 50 when 0.
    pub fn max_turns(&self) -> usize {
        let turns = self.agent.max_turns;
        if turns == 0 {
            50
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
///
/// 如果 `agent.toml` 不存在，先从全局配置 + 默认模板自动生成一个，再继续加载。
/// If `agent.toml` doesn't exist, auto-generate one from the global config + default
/// template first, then proceed to load it.
pub fn init(path: &str) -> anyhow::Result<Arc<Config>> {
    // agent.toml 不存在时，从全局配置自动生成一个（含全部小节 + 合理默认值）。
    // When agent.toml is missing, auto-generate one from the global config + defaults.
    if !std::path::Path::new(path).exists() {
        generate_agent_toml(path)?;
    }

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

/// 根据供应商 slug 返回推荐默认模型。
/// Return a recommended default model for the given provider slug.
fn default_model_for_provider(provider: &str) -> &'static str {
    match provider.to_lowercase().as_str() {
        "bailian" => "kimi/kimi-k3",
        "moonshot" | "kimi" => "kimi-k3",
        "volcengine" | "volcanoark" | "ark" | "火山" => "doubao-1-5-pro-256k",
        "custom" | "openai" | "glm" => "gpt-4o",
        _ => "deepseek-v4-pro",
    }
}

/// 当项目根目录没有 `agent.toml` 时，从全局配置 `~/.config/my-agent/config.toml`
/// 的 `[provider]` 信息 + 合理默认值自动生成一个完整的 `agent.toml`。
/// When the project root has no `agent.toml`, auto-generate a complete one from
/// the global config's `[provider]` info + sensible defaults.
///
/// 生成的文件包含全部小节（provider / agent / context / agents.* / memory /
/// evolution / sandbox），API Key 不写入项目文件（仍在全局 config.toml 的 [keys]
/// 中，运行时通过 merge_provider_fallback 回退读取）。
/// The generated file includes all sections. API keys are NOT written to the project
/// file (they stay in the global config.toml's [keys], backfilled at runtime via
/// merge_provider_fallback).
fn generate_agent_toml(path: &str) -> anyhow::Result<()> {
    // 尝试读取全局配置获取 provider 信息；全局配置不存在时用 deepseek 默认。
    // Try to read the global config for provider settings; fall back to deepseek.
    let (provider, base_url, api_key_env) = if let Some(global_path) = global_config_path() {
        if global_path.exists() {
            match Config::load(global_path.to_string_lossy().as_ref()) {
                Ok(global) => (
                    global
                        .provider
                        .provider
                        .unwrap_or_else(|| "deepseek".to_string()),
                    global.provider.base_url,
                    global.provider.api_key_env,
                ),
                Err(_) => ("deepseek".to_string(), None, None),
            }
        } else {
            ("deepseek".to_string(), None, None)
        }
    } else {
        ("deepseek".to_string(), None, None)
    };

    let model = default_model_for_provider(&provider);

    // 构建 [provider] 小节：只包含全局配置中实际存在的字段。
    // Build the [provider] section: only include fields actually present in the global config.
    let mut provider_lines = vec![format!("provider = \"{provider}\"")];
    if let Some(ref url) = base_url {
        provider_lines.push(format!("base_url = \"{url}\""));
    }
    if let Some(ref env) = api_key_env {
        provider_lines.push(format!("api_key_env = \"{env}\""));
    }
    let provider_section = provider_lines.join("\n");

    let content = format!(
        r#"# Agent 运行配置（自动生成）。
# Auto-generated from global config (~/.config/my-agent/config.toml) + defaults.
# 可按需修改各参数。agent.toml 已在 .gitignore 中，不会被提交。
# Edit as needed. agent.toml is git-ignored and won't be committed.

[provider]
{provider_section}

[agent]
# 默认模型：需与供应商匹配。请根据你的供应商修改。
# Default model: must match your provider. Please adjust for your provider.
default_model = "{model}"
max_turns = 50

[context]
max_output_tokens = 4096
compaction_threshold = 0.5
keep_recent_turns = 2
max_bash_output_chars = 20000
max_read_lines = 500
microcompact_threshold = 20000
microcompact_protected_results = 3

[agents.orchestrator]
model = "{model}"
preamble = "AGENTS.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.write_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.investigator]
model = "{model}"
preamble = "prompts/investigator.md"
max_turns = 50
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.write_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.planner]
model = "{model}"
preamble = "prompts/planner.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.write_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.builder]
model = "{model}"
preamble = "prompts/builder.md"
max_turns = 100
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"
permissions.write_file = "allow"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.auditor]
model = "{model}"
preamble = "prompts/auditor.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.write_file = "deny"
permissions.web_fetch = "deny"
permissions.web_search = "deny"

[memory]
dir = "memory"
conversation_file = "conversations.jsonl"
lessons_file = "lessons.jsonl"
rules_file = "rules.json"

[evolution]
rule_escalation_threshold = 3

[sandbox]
authorized_dirs = []
"#
    );

    std::fs::write(path, &content)?;
    eprintln!(
        "[config] agent.toml 不存在，已从全局配置自动生成: {path}\n\
         [config] Auto-generated agent.toml from global config.\n\
         [config] 默认模型 = {model}，请确认是否匹配你的供应商，按需修改。"
    );
    Ok(())
}

/// 返回缓存的配置；未初始化（如纯单元测试）时返回 None。
/// Returns the cached config, or None when not yet initialized (e.g. unit tests).
pub fn config() -> Option<&'static Config> {
    CONFIG.get().map(|c| c.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Permission;
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
        assert_eq!(cfg.max_turns(), 50);
        assert!(cfg.roles.is_empty());
        assert_eq!(cfg.evolution.rule_escalation_threshold, 0);
        assert_eq!(cfg.context.max_output_tokens, 4096);
        assert!(cfg.sandbox.authorized_dirs.is_empty());
        assert_eq!(cfg.memory.rules_file, "rules.json");
    }

    #[test]
    fn max_turns_zero_falls_back() {
        let cfg: Config = toml::from_str("[agent]\nmax_turns = 0\n").unwrap();
        assert_eq!(cfg.max_turns(), 50);
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

    #[test]
    fn default_model_for_each_provider() {
        assert_eq!(default_model_for_provider("deepseek"), "deepseek-v4-pro");
        assert_eq!(default_model_for_provider("bailian"), "kimi/kimi-k3");
        assert_eq!(default_model_for_provider("moonshot"), "kimi-k3");
        assert_eq!(default_model_for_provider("volcengine"), "doubao-1-5-pro-256k");
        assert_eq!(default_model_for_provider("custom"), "gpt-4o");
        assert_eq!(default_model_for_provider("unknown"), "deepseek-v4-pro");
    }

    #[test]
    fn generate_agent_toml_produces_valid_config() {
        // 生成的 agent.toml 应能被 Config::load 正确解析。
        // The generated agent.toml should be parseable by Config::load.
        let tmp = std::env::temp_dir().join("my-agent-test-gen.toml");
        generate_agent_toml(tmp.to_string_lossy().as_ref()).unwrap();
        let raw = std::fs::read_to_string(&tmp).unwrap();
        let cfg: Config = toml::from_str(&raw).unwrap();
        // provider 应有值（deepseek 或全局配置的值）。
        assert!(cfg.provider.provider.is_some());
        // 5 个角色都应存在。
        assert!(cfg.roles.contains_key("orchestrator"));
        assert!(cfg.roles.contains_key("investigator"));
        assert!(cfg.roles.contains_key("planner"));
        assert!(cfg.roles.contains_key("builder"));
        assert!(cfg.roles.contains_key("auditor"));
        // 默认模型应与 provider 匹配。
        let provider = cfg.provider.provider.as_deref().unwrap_or("deepseek");
        let expected_model = default_model_for_provider(provider);
        assert_eq!(cfg.agent.default_model, expected_model);
        assert_eq!(cfg.roles.get("builder").unwrap().model, expected_model);
        // builder 应有写权限。
        assert_eq!(
            cfg.roles.get("builder").unwrap().permissions.write_file,
            Permission::Allow
        );
        // auditor 应拒绝 web 访问。
        assert_eq!(
            cfg.roles.get("auditor").unwrap().permissions.web_fetch,
            Permission::Deny
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
