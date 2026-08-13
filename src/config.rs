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
    /// MCP 服务器配置：`[mcp.<name>]` 小节，每个小节定义一个 MCP 服务器连接。
    /// 通过 `command`+`args`（stdio）或 `url`（HTTP/SSE）指定传输方式。
    /// MCP server configs: `[mcp.<name>]` sections, each defining one MCP server connection.
    /// Transport is selected by `command`+`args` (stdio) or `url` (HTTP/SSE).
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
}

/// 单个 MCP 服务器的配置。通过 `command`（stdio）或 `url`（HTTP/SSE）选择传输方式。
/// Config for a single MCP server. Transport is selected by `command` (stdio) or `url` (HTTP/SSE).
#[derive(Debug, Deserialize, Default)]
pub struct McpServerConfig {
    /// stdio 传输：要执行的命令（如 `codegraph`、`context7-mcp`）。
    /// stdio transport: the command to execute (e.g. `codegraph`, `context7-mcp`).
    pub command: Option<String>,
    /// stdio 传输：传给命令的参数。
    /// stdio transport: arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// HTTP/SSE 传输：MCP 服务器 URL（如 `https://mcp.grep.app`）。
    /// HTTP/SSE transport: the MCP server URL (e.g. `https://mcp.grep.app`).
    pub url: Option<String>,
    /// stdio 传输：传给子进程的环境变量。
    /// stdio transport: environment variables for the child process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// npm 包名：设置后，首次使用时自动安装到 `~/.moye/`，后续直接使用本地二进制。
    /// npm package name: when set, auto-installs to `~/.moye/` on first use, then runs the local binary.
    pub package: Option<String>,
    /// 初始化参数：设置后，若 `init_if_missing` 指定的目录不存在，则在启动 MCP server 之前
    /// 用这些参数运行一次初始化命令（如 `["init"]` → `codegraph init`）。
    /// Init args: when set, if the directory specified by `init_if_missing` doesn't exist,
    /// runs `<command> <init_args>` before starting the MCP server (e.g. `["init"]` → `codegraph init`).
    #[serde(default)]
    pub init: Vec<String>,
    /// 触发初始化的条件：检查此路径（相对当前工作目录）是否存在，不存在则运行 init。
    /// Condition for triggering init: checks if this path (relative to CWD) exists; if not, runs init.
    pub init_if_missing: Option<String>,
}

impl McpServerConfig {
    /// 返回此配置使用的传输类型（`"stdio"` 或 `"http"`）。
    /// Returns the transport type this config uses (`"stdio"` or `"http"`).
    pub fn transport_type(&self) -> &'static str {
        if self.command.is_some() {
            "stdio"
        } else {
            "http"
        }
    }
}

/// `[sandbox]` 小节：沙箱配置（后端选择 + 预授权目录）。
/// The `[sandbox]` section: sandbox config (backend selection + pre-authorized dirs).
///
/// `backend` 控制 OS 级沙箱后端：auto（自动检测）、bwrap、seatbelt、path（仅路径检查）、off。
/// `backend` selects the OS-level sandbox backend: auto, bwrap, seatbelt, path, or off.
/// `authorized_dirs` 预授权一组目录，Agent 访问时不再弹窗确认。
/// `authorized_dirs` pre-authorizes directories so the Agent can access them without prompting.
#[derive(Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,

    #[serde(default)]
    pub authorized_dirs: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: default_sandbox_backend(),
            authorized_dirs: Vec::new(),
        }
    }
}

fn default_sandbox_backend() -> String {
    "auto".to_string()
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
    /// `AGENT_PROVIDER` 环境变量优先于此值。
    pub provider: Option<String>,
    /// 全局 OpenAI 兼容 base URL 覆盖。`AGENT_BASE_URL` 环境变量优先。
    pub base_url: Option<String>,
    /// 自定义 API key 环境变量名；为空时按供应商自动选择。
    pub api_key_env: Option<String>,
    /// API 套餐：standard（按量付费，默认）/ coding / agent。
    /// 仅部分供应商支持套餐端点（volcengine / bailian / moonshot / zhipu）。
    /// `AGENT_PLAN` 环境变量优先于此值。
    #[serde(default)]
    pub plan: Option<String>,
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

/// 启动时调用一次：加载项目 `agent.toml`，再用全局 `~/.config/moye/config.toml`
/// 作为 fallback 填充项目中缺失的 `[provider]` 字段，最后缓存并返回共享 Arc。
/// 合并优先级：环境变量 > 项目 agent.toml > 全局 config.toml > 供应商默认。
/// Call once at startup: loads the project `agent.toml`, then fills in any missing
/// `[provider]` fields from the global `~/.config/moye/config.toml` as a fallback,
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

/// 检查本地 `agent.toml` 或全局 `~/.config/moye/config.toml` 是否存在。
/// 任一存在即跳过 setup 向导。
/// Checks if a local `agent.toml` or global `~/.config/moye/config.toml` exists.
/// Either being present skips the setup wizard.
pub fn has_config_file() -> bool {
    std::path::Path::new("agent.toml").exists()
        || global_config_path()
            .map(|p| p.exists())
            .unwrap_or(false)
}

/// 返回全局配置路径 `~/.config/moye/config.toml`；`HOME` 未设置时返回 `None`。
/// Return the global config path `~/.config/moye/config.toml`; `None` when `HOME` is unset.
fn global_config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("moye")
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
    if project.provider.plan.is_none() {
        project.provider.plan = g.plan;
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
pub fn default_model_for_provider(provider: &str) -> &'static str {
    default_model_for_provider_plan(provider, "standard")
}

/// 根据供应商 slug + 套餐返回推荐默认模型。
/// Return a recommended default model for the given provider slug and plan.
pub fn default_model_for_provider_plan(provider: &str, plan: &str) -> &'static str {
    let p = provider.to_lowercase();
    let plan = crate::providers::ApiPlan::parse(plan);
    match (p.as_str(), plan) {
        ("volcengine" | "volcanoark" | "ark" | "火山", crate::providers::ApiPlan::Agent) => {
            "doubao-seed-evolving"
        }
        ("volcengine" | "volcanoark" | "ark" | "火山", crate::providers::ApiPlan::Coding) => {
            "doubao-seed-2.0-code"
        }
        ("volcengine" | "volcanoark" | "ark" | "火山", _) => "doubao-seed-evolving",
        ("bailian", crate::providers::ApiPlan::Coding) => "qwen3-coder-plus",
        ("bailian", _) => "qwen3.7-plus",
        ("moonshot" | "kimi", crate::providers::ApiPlan::Coding) => "kimi-for-coding",
        ("moonshot" | "kimi", _) => "kimi-k3",
        ("zhipu" | "glm" | "bigmodel", _) => "glm-5.2",
        ("openai", _) => "gpt-5.6-sol",
        ("claude" | "anthropic", _) => "claude-sonnet-5",
        ("mimo" | "xiaomi", _) => "mimo-v2.5-pro",
        ("gemini" | "google", _) => "gemini-3.6-flash",
        _ => "deepseek-v4-pro",
    }
}

/// 当项目根目录没有 `agent.toml` 时，从全局配置 `~/.config/moye/config.toml`
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
    let (provider, base_url, api_key_env, global_model) = if let Some(global_path) = global_config_path() {
        if global_path.exists() {
            match Config::load(global_path.to_string_lossy().as_ref()) {
                Ok(global) => (
                    global.provider.provider.unwrap_or_else(|| "deepseek".to_string()),
                    global.provider.base_url,
                    global.provider.api_key_env,
                    global.agent.default_model,
                ),
                Err(_) => ("deepseek".to_string(), None, None, String::new()),
            }
        } else {
            ("deepseek".to_string(), None, None, String::new())
        }
    } else {
        ("deepseek".to_string(), None, None, String::new())
    };

    let model = if !global_model.is_empty() {
        global_model
    } else {
        default_model_for_provider(&provider).to_string()
    };

    let content = render_agent_toml(
        &provider,
        &model,
        base_url.as_deref(),
        api_key_env.as_deref(),
        None,
    );
    std::fs::write(path, &content)?;
    eprintln!(
        "[config] agent.toml 不存在，已从全局配置自动生成: {path}\n\
         [config] Auto-generated agent.toml from global config.\n\
         [config] 默认模型 = {model}，请确认是否匹配你的供应商，按需修改。"
    );
    Ok(())
}

pub(crate) fn render_agent_toml(
    provider: &str,
    model: &str,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    plan: Option<&str>,
) -> String {
    let mut provider_lines = vec![format!("provider = \"{provider}\"")];
    if let Some(p) = plan {
        provider_lines.push(format!("plan = \"{p}\""));
    }
    if let Some(url) = base_url {
        provider_lines.push(format!("base_url = \"{url}\""));
    }
    if let Some(env) = api_key_env {
        provider_lines.push(format!("api_key_env = \"{env}\""));
    }
    let provider_section = provider_lines.join("\n");

    format!(
        r#"# Agent 运行配置（首次配置向导生成）。
# Generated by first-time setup wizard.
# 可按需修改各参数。agent.toml 已在 .gitignore 中，不会被提交。
# Edit as needed. agent.toml is git-ignored and won't be committed.

[provider]
{provider_section}

[agent]
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
backend = "auto"
authorized_dirs = []

[mcp.codegraph]
command = "codegraph"
args = ["serve", "--mcp"]
package = "@colbymchenry/codegraph"
init = ["init"]
init_if_missing = ".codegraph"

[mcp.context7]
command = "context7-mcp"
args = []
package = "@upstash/context7-mcp"
"#
    )
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
authorized_dirs = ["~/.config", "/tmp/moye"]
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
        assert_eq!(cfg.sandbox.authorized_dirs[1], "/tmp/moye");
    }

    #[test]
    fn load_empty_config_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.max_turns(), 50);
        assert!(cfg.roles.is_empty());
        assert_eq!(cfg.evolution.rule_escalation_threshold, 0);
        assert_eq!(cfg.context.max_output_tokens, 4096);
        assert!(cfg.sandbox.authorized_dirs.is_empty());
        assert_eq!(cfg.sandbox.backend, "auto");
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
        assert_eq!(default_model_for_provider("bailian"), "qwen3.7-plus");
        assert_eq!(default_model_for_provider("moonshot"), "kimi-k3");
        assert_eq!(default_model_for_provider("volcengine"), "doubao-seed-evolving");
        assert_eq!(default_model_for_provider("openai"), "gpt-5.6-sol");
        assert_eq!(default_model_for_provider("claude"), "claude-sonnet-5");
        assert_eq!(default_model_for_provider("mimo"), "mimo-v2.5-pro");
        assert_eq!(default_model_for_provider("gemini"), "gemini-3.6-flash");
        assert_eq!(default_model_for_provider("zhipu"), "glm-5.2");
        assert_eq!(default_model_for_provider("unknown"), "deepseek-v4-pro");
    }

    #[test]
    fn default_model_respects_plan() {
        assert_eq!(
            default_model_for_provider_plan("volcengine", "agent"),
            "doubao-seed-evolving"
        );
        assert_eq!(
            default_model_for_provider_plan("volcengine", "coding"),
            "doubao-seed-2.0-code"
        );
        assert_eq!(
            default_model_for_provider_plan("bailian", "coding"),
            "qwen3-coder-plus"
        );
        assert_eq!(
            default_model_for_provider_plan("moonshot", "coding"),
            "kimi-for-coding"
        );
        assert_eq!(
            default_model_for_provider_plan("moonshot", "standard"),
            "kimi-k3"
        );
    }

    #[test]
    fn generate_agent_toml_produces_valid_config() {
        // 生成的 agent.toml 应能被 Config::load 正确解析。
        // The generated agent.toml should be parseable by Config::load.
        let tmp = std::env::temp_dir().join("moye-test-gen.toml");
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
        // 模型应有值（来自全局配置或供应商默认）。
        // Model should be set (from global config or provider default).
        assert!(!cfg.agent.default_model.is_empty());
        assert_eq!(cfg.roles.get("builder").unwrap().model, cfg.agent.default_model);
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

    #[test]
    fn load_mcp_config() {
        let toml_str = r#"
[mcp.codegraph]
command = "codegraph"
args = ["serve"]

[mcp.context7]
url = "https://context7.com/api/v2/mcp"

[mcp.grep_app]
url = "https://mcp.grep.app"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.mcp.len(), 3);

        let cg = cfg.mcp.get("codegraph").unwrap();
        assert_eq!(cg.command.as_deref(), Some("codegraph"));
        assert_eq!(cg.args, vec!["serve"]);
        assert!(cg.url.is_none());
        assert_eq!(cg.transport_type(), "stdio");

        let c7 = cfg.mcp.get("context7").unwrap();
        assert_eq!(c7.url.as_deref(), Some("https://context7.com/api/v2/mcp"));
        assert!(c7.command.is_none());
        assert_eq!(c7.transport_type(), "http");
    }
}
