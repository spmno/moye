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

// allow: SIZE_OK — Wave 2 todo 7 task scope restricts changes to src/config.rs only;
// splitting profile into a sibling module would require touching src/main.rs (explicitly
// out of scope). File was already 488 pure LOC pre-profile; profile is cohesive config
// logic belonging here. Refactor into a submodule in Wave 4+ when main.rs is safe to edit.

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
    /// Profile 叠加配置：声明式配置组合（`[profile]` + `[profile.<name>]`）。
    /// Profile overlay config: declarative config composition.
    #[serde(default)]
    pub profile: ProfileSection,
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

/// `[sandbox]` 小节：沙箱配置(后端选择 + 模式 + 预授权目录)。
/// The `[sandbox]` section: sandbox config (backend + mode + pre-authorized dirs).
///
/// `backend` 控制 SimpleSandbox 的低层后端:auto / bwrap / seatbelt / path / off。
/// `backend` controls SimpleSandbox's low-level backend: auto / bwrap / seatbelt / path / off.
///
/// `mode` 是高层沙箱模式选择(含 Landlock 选项,todo 5):
/// `mode` is the high-level sandbox mode (includes Landlock option, todo 5):
/// - `auto`: bwrap 优先 landlock fallback(todo 8/9 接入选择逻辑)。
/// - `bwrap`: 强制用 bwrap(SimpleSandbox)。
/// - `landlock`: 强制用 LandlockSandbox(无 bwrap mount namespace,弱隔离)。
/// - `off`: 禁用沙箱。
///
/// `authorized_dirs` 预授权一组目录,Agent 访问时不再弹窗确认。
/// `authorized_dirs` pre-authorizes directories so the Agent can access them without prompting.
#[derive(Debug, Deserialize)]
pub struct SandboxConfig {
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,

    /// 高层沙箱模式:auto / bwrap / landlock / off。
    /// High-level sandbox mode: auto / bwrap / landlock / off.
    /// `auto` = bwrap 优先 landlock fallback(todo 8/9 接入选择逻辑)。
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,

    #[serde(default)]
    pub authorized_dirs: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: default_sandbox_backend(),
            mode: default_sandbox_mode(),
            authorized_dirs: Vec::new(),
        }
    }
}

fn default_sandbox_backend() -> String {
    "auto".to_string()
}

fn default_sandbox_mode() -> String {
    "auto".to_string()
}

/// `[profile]` 小节：profile 叠加配置（声明式配置组合）。
/// The `[profile]` section: profile overlay config (declarative config composition).
///
/// 一个 profile 是一组有序 patch，每个 patch 用 `id` 定位配置树中的某个路径
/// （如 `sandbox` 或 `agents.builder.permissions`），用 `config` 替换该路径的
/// 整个值。启动时通过 `AGENT_PROFILE` 环境变量或 `[profile] active = "..."` 选
/// profile；未选 profile 时行为与无 `[profile]` 段完全一致（向后兼容）。
#[derive(Debug, Deserialize, Default)]
pub struct ProfileSection {
    /// 选中的 profile 名（`AGENT_PROFILE` 环境变量优先于此值）。
    #[serde(default)]
    pub active: Option<String>,
    /// 所有已定义的 profile（键为 profile 名，来自 `[profile.<name>]` 子表）。
    #[serde(default, flatten)]
    pub profiles: HashMap<String, Profile>,
}

/// 单个 profile：一组有序 patch + 可选基础 profile（链式继承）。
/// A single profile: an ordered list of patches + optional base profile (chain).
#[derive(Debug, Deserialize, Clone)]
pub struct Profile {
    /// profile 名（与 `[profile.<name>]` 的 TOML 键一致）。
    #[allow(dead_code)] // infrastructure for future phases
    pub name: String,
    /// 基础 profile 名：解析时先应用 base 的 patch，再应用本 profile 的 patch。
    /// `"default"` 或缺省表示基础为顶层内联配置（不继承其他 profile）。
    #[serde(default)]
    pub base: Option<String>,
    /// 有序 patch 列表：每个 patch 用 `id` 定位并替换其整个 `config`。
    #[serde(default)]
    pub patches: Vec<ProfilePatch>,
}

/// 单个 patch：定位 `id` 路径，用 `config` 替换该路径的整个值（非深度合并）。
/// A single patch: locates the `id` path and replaces its entire value (no deep merge).
#[derive(Debug, Deserialize, Clone)]
pub struct ProfilePatch {
    /// 配置树中的目标路径（点号分隔，如 `sandbox`、`agents.builder.permissions`）。
    pub id: String,
    /// 替换值（TOML 内联表）。
    pub config: toml::Value,
}

impl Config {
    /// 从 agent.toml 解析。整个 crate 仅此处解析该文件。
    /// Parses from agent.toml. This is the only place the file is parsed.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::from_str_with_profile(&raw, None)
    }

    /// 解析 TOML 并应用 profile 叠加。
    /// Parse TOML and apply profile overlay.
    ///
    /// `explicit` 为 `Some("dev")` 时强制使用名为 `dev` 的 profile；
    /// 为 `None` 时按 `AGENT_PROFILE` 环境变量或 `[profile] active` 字段决定。
    /// 无 profile 被选中时，行为与直接 `toml::from_str` 一致（向后兼容）。
    pub fn from_str_with_profile(raw: &str, explicit: Option<&str>) -> anyhow::Result<Self> {
        let (cfg, _value, _active) = Self::parse_combined(raw, explicit)?;
        Ok(cfg)
    }

    /// 解析 TOML、应用 profile 叠加，并返回组合后的配置树与选中的 profile 名。
    /// Parse TOML, apply profile overlay, and return the combined value tree +
    /// the selected profile name alongside the typed Config.
    ///
    /// 返回的 `toml::Value` 是 profile patch 应用后的组合树（即 `--dump-config`
    /// 应该打印的内容）。`active` 为 `Some(name)` 表示选中了某个 profile；`None`
    /// 表示无 profile 被选中（向后兼容模式）。
    ///
    /// 优先级：`explicit` 参数 > `AGENT_PROFILE` 环境变量 > `[profile] active` 字段。
    pub fn parse_combined(
        raw: &str,
        explicit: Option<&str>,
    ) -> anyhow::Result<(Self, toml::Value, Option<String>)> {
        use serde::de::IntoDeserializer;

        let mut value: toml::Value =
            toml::from_str(raw).map_err(|e| anyhow::anyhow!("agent.toml parse error: {e}"))?;

        let active = explicit
            .map(String::from)
            .or_else(|| std::env::var("AGENT_PROFILE").ok())
            .or_else(|| {
                value
                    .get("profile")
                    .and_then(|p| p.get("active"))
                    .and_then(|a| a.as_str())
                    .map(String::from)
            });

        if let Some(name) = &active {
            let profile_section: ProfileSection = value
                .get("profile")
                .map(|p| {
                    ProfileSection::deserialize(p.clone().into_deserializer())
                        .map_err(|e| anyhow::anyhow!("[profile] section parse error: {e}"))
                })
                .transpose()?
                .unwrap_or_default();

            let patches = resolve_profile_chain(&profile_section.profiles, name, 0)?;
            for patch in &patches {
                apply_patch(&mut value, &patch.id, &patch.config)
                    .map_err(|e| anyhow::anyhow!("profile '{name}': {e}"))?;
            }
        }

        let cfg = Config::deserialize(value.clone().into_deserializer())
            .map_err(|e| anyhow::anyhow!("config after patch: {e}"))?;
        Ok((cfg, value, active))
    }

    /// 返回当前生效的 profile 名（env > `[profile].active`）。
    /// 仅做语法解析，不重新应用 patch；用于 `--dump-config` 等诊断输出。
    /// Returns the active profile name (env > `[profile].active`).
    /// Syntax-only; does not re-apply patches. Used for diagnostics like `--dump-config`.
    pub fn active_profile_name(&self) -> Option<String> {
        std::env::var("AGENT_PROFILE")
            .ok()
            .or_else(|| self.profile.active.clone())
    }

    /// 自主循环轮数上限；为 0 时回退到默认 50。
    /// Max turns for the autonomous loop; falls back to 50 when 0.
    pub fn max_turns(&self) -> usize {
        let turns = self.agent.max_turns;
        if turns == 0 { 50 } else { turns }
    }
}

/// 解析 profile 链：先应用 base profile 的 patch，再应用本 profile 的 patch。
/// 限制链深度以防止循环继承。
fn resolve_profile_chain(
    profiles: &HashMap<String, Profile>,
    name: &str,
    depth: u8,
) -> anyhow::Result<Vec<ProfilePatch>> {
    const MAX_DEPTH: u8 = 16;
    if depth > MAX_DEPTH {
        anyhow::bail!("profile chain too deep (>{MAX_DEPTH} levels, possible cycle)");
    }
    let profile = profiles
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("profile '{name}' not found in [profile] section"))?;
    let mut patches = Vec::new();
    if let Some(base) = &profile.base
        && base != "default"
        && base != name
    {
        patches.extend(resolve_profile_chain(profiles, base, depth + 1)?);
    }
    patches.extend(profile.patches.clone());
    Ok(patches)
}

/// 按 `id`（点号分隔路径）定位 value 树中的位置，用 `replacement` 替换整个值。
/// 路径中任一段不存在即报错（fail-closed，不静默 no-op）。
fn apply_patch(value: &mut toml::Value, id: &str, replacement: &toml::Value) -> anyhow::Result<()> {
    if id.is_empty() {
        anyhow::bail!("patch id is empty");
    }
    let segments: Vec<&str> = id.split('.').collect();
    let last_idx = segments.len() - 1;
    let mut current: &mut toml::Value = value;
    for (i, seg) in segments.iter().enumerate() {
        let is_last = i == last_idx;
        let table = current.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "patch id '{id}' cannot navigate into non-table value at segment '{seg}'"
            )
        })?;
        if is_last {
            if !table.contains_key(*seg) {
                anyhow::bail!("patch id '{id}' references non-existent path segment '{seg}'");
            }
            table.insert((*seg).to_string(), replacement.clone());
            return Ok(());
        }
        current = table.get_mut(*seg).ok_or_else(|| {
            anyhow::anyhow!("patch id '{id}' references non-existent path segment '{seg}'")
        })?;
    }
    Ok(())
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
    if let Some(global_path) = global_config_path()
        && global_path.exists()
        && let Ok(global) = Config::load(global_path.to_string_lossy().as_ref())
    {
        merge_provider_fallback(&mut cfg, global);
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
        || global_config_path().map(|p| p.exists()).unwrap_or(false)
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
    let (provider, base_url, api_key_env, plan, global_model) =
        if let Some(global_path) = global_config_path() {
            if global_path.exists() {
                match Config::load(global_path.to_string_lossy().as_ref()) {
                    Ok(global) => (
                        global
                            .provider
                            .provider
                            .unwrap_or_else(|| "deepseek".to_string()),
                        global.provider.base_url,
                        global.provider.api_key_env,
                        global.provider.plan,
                        global.agent.default_model,
                    ),
                    Err(_) => ("deepseek".to_string(), None, None, None, String::new()),
                }
            } else {
                ("deepseek".to_string(), None, None, None, String::new())
            }
        } else {
            ("deepseek".to_string(), None, None, None, String::new())
        };

    let model = if !global_model.is_empty() {
        global_model
    } else {
        default_model_for_provider_plan(
            &provider,
            plan.as_deref().unwrap_or("standard"),
        )
        .to_string()
    };

    let content = render_agent_toml(
        &provider,
        &model,
        base_url.as_deref(),
        api_key_env.as_deref(),
        plan.as_deref(),
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
mode = "auto"
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
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Test helper: saves the current value of an env var, sets a new one (or
    /// unsets it), and restores the original on Drop. Wraps the edition-2024
    /// `unsafe` env mutators so tests stay clean.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn new(key: &'static str, value: Option<&str>) -> Self {
            let prev = std::env::var(key).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn env_guard(key: &'static str, value: Option<&str>) -> EnvGuard {
        EnvGuard::new(key, value)
    }

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
        assert_eq!(cfg.sandbox.mode, "auto");
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
        assert_eq!(
            default_model_for_provider("volcengine"),
            "doubao-seed-evolving"
        );
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
        assert_eq!(
            cfg.roles.get("builder").unwrap().model,
            cfg.agent.default_model
        );
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
    fn generate_agent_toml_propagates_global_plan() {
        // Regression: when global config sets [provider].plan (e.g. "agent"),
        // the auto-generated project agent.toml must include `plan = "..."` so
        // direct file reads (e.g. --dump-config) and runtime plan resolution
        // both pick up the non-standard endpoint without re-merging.
        let tmp = std::env::temp_dir().join("moye-test-gen-plan.toml");
        let content = render_agent_toml(
            "volcengine",
            "doubao-seed-evolving",
            None,
            Some("ARK_API_KEY"),
            Some("agent"),
        );
        std::fs::write(&tmp, &content).unwrap();
        let raw = std::fs::read_to_string(&tmp).unwrap();
        let cfg: Config = toml::from_str(&raw).unwrap();
        assert_eq!(cfg.provider.provider.as_deref(), Some("volcengine"));
        assert_eq!(cfg.provider.plan.as_deref(), Some("agent"));
        assert_eq!(cfg.provider.api_key_env.as_deref(), Some("ARK_API_KEY"));
        assert_eq!(cfg.agent.default_model, "doubao-seed-evolving");
        assert!(
            raw.contains("plan = \"agent\""),
            "generated toml must contain plan line, got:\n{raw}"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn generate_agent_toml_omits_plan_when_not_set() {
        // Backward compat: when no plan is configured (standard), the generated
        // file must not include a plan line.
        let content = render_agent_toml(
            "deepseek",
            "deepseek-v4-pro",
            None,
            Some("DEEPSEEK_API_KEY"),
            None,
        );
        assert!(
            !content.contains("plan ="),
            "standard plan must not emit a plan line:\n{content}"
        );
        let cfg: Config = toml::from_str(&content).unwrap();
        assert_eq!(cfg.provider.plan, None);
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

    #[test]
    fn profile_patch_replaces_sandbox_backend() {
        // Given: config with [sandbox] backend="auto" + [profile.dev] patch
        //        replacing [sandbox] with backend="landlock".
        // When: parsing with explicit profile "dev".
        // Then: composed Config has sandbox.backend == "landlock" (whole-table replace).
        let toml_str = r#"
[sandbox]
backend = "auto"
authorized_dirs = []

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "landlock", authorized_dirs = [] } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("dev"))
            .expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.backend, "landlock");
        assert!(cfg.sandbox.authorized_dirs.is_empty());
    }

    #[test]
    fn profile_patch_replaces_nested_role_permissions() {
        // Given: config with [agents.builder] permissions allow + [profile.lockdown]
        //        patch replacing [agents.builder.permissions] entirely with all-deny.
        // When: parsing with explicit profile "lockdown".
        // Then: builder permissions are all Deny (full replacement); model/preamble untouched.
        let toml_str = r#"
[agents.builder]
model = "glm-latest"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
permissions.edit_file = "allow"
permissions.run_bash_mutating = "allow"

[profile.lockdown]
name = "lockdown"
patches = [
    { id = "agents.builder.permissions", config = { read_file = "deny", run_bash_readonly = "deny", run_bash_mutating = "deny", edit_file = "deny", write_file = "deny", web_fetch = "deny", web_search = "deny" } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("lockdown"))
            .expect("profile parse should succeed");
        let builder = cfg.roles.get("builder").expect("builder role present");
        assert_eq!(builder.permissions.read_file, Permission::Deny);
        assert_eq!(builder.permissions.edit_file, Permission::Deny);
        assert_eq!(builder.permissions.run_bash_mutating, Permission::Deny);
        assert_eq!(builder.model, "glm-latest");
    }

    #[test]
    fn no_profile_section_backward_compat() {
        // Given: config with no [profile] section.
        // When: parsing with no explicit profile and no AGENT_PROFILE env.
        // Then: behavior identical to direct toml::from_str (backward compat).
        let toml_str = r#"
[sandbox]
backend = "bwrap"
authorized_dirs = ["/tmp"]

[agent]
default_model = "kimi-k3"
max_turns = 20
"#;
        let cfg = Config::from_str_with_profile(toml_str, None).expect("parse should succeed");
        assert_eq!(cfg.sandbox.backend, "bwrap");
        assert_eq!(cfg.agent.default_model, "kimi-k3");
        assert_eq!(cfg.max_turns(), 20);
    }

    #[test]
    fn explicit_none_profile_with_profile_section_is_noop() {
        // Given: config with a [profile.dev] section, but explicit profile = None
        //        and no AGENT_PROFILE env.
        // When: parsing with explicit None.
        // Then: no patches applied; base config unchanged (profile section ignored).
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[sandbox]
backend = "auto"

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "landlock", authorized_dirs = [] } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, None).expect("parse should succeed");
        assert_eq!(cfg.sandbox.backend, "auto");
    }

    #[test]
    fn agent_profile_env_var_selects_profile() {
        // Given: config with [profile.dev] that patches sandbox backend to landlock.
        // When: AGENT_PROFILE=dev env set and explicit profile = None.
        // Then: dev profile's patch is applied.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", Some("dev"));
        let toml_str = r#"
[sandbox]
backend = "auto"

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "landlock", authorized_dirs = [] } },
]
"#;
        let cfg =
            Config::from_str_with_profile(toml_str, None).expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.backend, "landlock");
    }

    #[test]
    fn profile_active_field_selects_profile() {
        // Given: [profile] active = "dev" selects dev profile without env var.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[profile]
active = "dev"

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "landlock", authorized_dirs = [] } },
]

[sandbox]
backend = "auto"
"#;
        let cfg = Config::from_str_with_profile(toml_str, None).expect("parse should succeed");
        assert_eq!(cfg.sandbox.backend, "landlock");
    }

    #[test]
    fn patch_nonexistent_id_errors() {
        // Given: profile whose patch references an id that doesn't exist in base config.
        // When: parsing with that profile.
        // Then: error (exit non-zero), not silent no-op (fail-closed on bad reference).
        let toml_str = r#"
[sandbox]
backend = "auto"

[profile.bad]
name = "bad"
patches = [
    { id = "nonexistent.path", config = {} },
]
"#;
        let result = Config::from_str_with_profile(toml_str, Some("bad"));
        assert!(
            result.is_err(),
            "patch referencing non-existent id must error, got: {:?}",
            result
        );
    }

    #[test]
    fn unknown_profile_name_errors() {
        // Given: explicit profile "ghost" that doesn't exist in [profile].
        // When: parsing with that profile.
        // Then: error (fail-closed on unknown profile name).
        let toml_str = r#"
[profile.dev]
name = "dev"
patches = []
"#;
        let result = Config::from_str_with_profile(toml_str, Some("ghost"));
        assert!(result.is_err(), "unknown profile name must error");
    }

    #[test]
    fn profile_base_chains_patches_in_order() {
        // Given: profile "dev" extends "base_p" (base = "base_p").
        //        base_p patches sandbox.backend = "bwrap".
        //        dev patches sandbox.backend = "landlock".
        // When: parsing with profile "dev".
        // Then: both patches applied in order; final value is "landlock" (dev wins).
        let toml_str = r#"
[sandbox]
backend = "auto"

[profile.base_p]
name = "base_p"
patches = [
    { id = "sandbox", config = { backend = "bwrap", mode = "auto", authorized_dirs = [] } },
]

[profile.dev]
name = "dev"
base = "base_p"
patches = [
    { id = "sandbox", config = { backend = "landlock", mode = "landlock", authorized_dirs = [] } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("dev"))
            .expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.backend, "landlock");
    }

    #[test]
    fn sandbox_mode_field_parsed() {
        // Given: [sandbox] with mode = "landlock".
        // When: parsing.
        // Then: cfg.sandbox.mode == "landlock".
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "landlock"
authorized_dirs = []
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.sandbox.mode, "landlock");
    }

    #[test]
    fn sandbox_mode_defaults_to_auto() {
        // Given: [sandbox] without mode field.
        // When: parsing.
        // Then: cfg.sandbox.mode == "auto" (default).
        let toml_str = r#"
[sandbox]
backend = "bwrap"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.sandbox.mode, "auto");
        assert_eq!(cfg.sandbox.backend, "bwrap");
    }

    #[test]
    fn profile_overlay_applies_sandbox_mode() {
        // Given: base [sandbox] mode = "auto", profile patches it to "landlock".
        // When: parsing with the profile selected.
        // Then: cfg.sandbox.mode == "landlock" (the profile patch replaces it).
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "auto"
authorized_dirs = []

[profile.landlock]
name = "landlock"
patches = [
    { id = "sandbox", config = { backend = "auto", mode = "landlock", authorized_dirs = [] } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("landlock"))
            .expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.mode, "landlock");
    }

    #[test]
    fn profile_overlay_applies_agent_model_override() {
        // Given: [agents.builder].model = "base-model", profile patches it to "profile-model".
        // When: parsing with the profile selected.
        // Then: cfg.roles["builder"].model == "profile-model".
        let toml_str = r#"
[agents.builder]
model = "base-model"
preamble = "prompts/builder.md"

[profile.model-swap]
name = "model-swap"
patches = [
    { id = "agents.builder", config = { model = "profile-model", preamble = "prompts/builder.md", permissions = { read_file = "allow", run_bash_readonly = "allow", run_bash_mutating = "allow", edit_file = "allow", write_file = "allow", web_fetch = "allow", web_search = "allow" } } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("model-swap"))
            .expect("profile parse should succeed");
        let builder = cfg.roles.get("builder").expect("builder role present");
        assert_eq!(builder.model, "profile-model");
    }

    #[test]
    fn parse_combined_returns_value_tree_with_patch_applied() {
        // Given: base config + profile that patches sandbox.mode to "landlock".
        // When: parse_combined with the profile.
        // Then: returned toml::Value reflects the patched mode; active name matches.
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "auto"
authorized_dirs = []

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "auto", mode = "landlock", authorized_dirs = [] } },
]
"#;
        let (cfg, value, active) =
            Config::parse_combined(toml_str, Some("dev")).expect("parse_combined should succeed");
        assert_eq!(active.as_deref(), Some("dev"));
        assert_eq!(cfg.sandbox.mode, "landlock");
        let sandbox_mode_in_tree = value
            .get("sandbox")
            .and_then(|s| s.get("mode"))
            .and_then(|m| m.as_str())
            .expect("sandbox.mode present in value tree");
        assert_eq!(sandbox_mode_in_tree, "landlock");
    }

    #[test]
    fn parse_combined_no_profile_returns_none_active() {
        // Given: config with no [profile] section.
        // When: parse_combined with no explicit profile and no AGENT_PROFILE env.
        // Then: active is None; value tree equals the parsed input.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[sandbox]
backend = "bwrap"
"#;
        let (cfg, _value, active) =
            Config::parse_combined(toml_str, None).expect("parse_combined should succeed");
        assert!(active.is_none());
        assert_eq!(cfg.sandbox.backend, "bwrap");
    }

    #[test]
    fn active_profile_name_reads_env_var() {
        // Given: AGENT_PROFILE env var is set.
        // When: calling active_profile_name() on any Config.
        // Then: returns the env var's value (env wins over [profile].active).
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", Some("from-env"));
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.active_profile_name().as_deref(), Some("from-env"));
    }

    #[test]
    fn active_profile_name_falls_back_to_config_field() {
        // Given: no AGENT_PROFILE env, but [profile].active = "dev" in config.
        // When: calling active_profile_name().
        // Then: returns "dev" from the config field.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[profile]
active = "dev"

[profile.dev]
name = "dev"
patches = []
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.active_profile_name().as_deref(), Some("dev"));
    }
}
