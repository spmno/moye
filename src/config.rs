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
use crate::registry::{CommandRule, RoleConfig, ToolPerms};
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
    pub agents: AgentsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub evolution: EvolutionSection,
    /// 验证门配置：Builder 产出后、Auditor 评审前自动运行构建/测试命令。
    /// Verify gate config: auto-run build/test after the Builder, before the Auditor.
    #[serde(default)]
    pub verify: VerifyConfig,
    /// 沙箱配置：预授权目录列表等。
    /// Sandbox config: pre-authorized directory list, etc.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Profile 叠加配置：声明式配置组合（`[profile]` + `[profile.<name>]`）。
    /// Profile overlay config: declarative config composition.
    #[serde(default)]
    pub profile: ProfileSection,
    /// 全局 API key 存储：`[keys]` section，键为环境变量名（如 `DEEPSEEK_API_KEY`），
    /// 值为 key 本身。当前目录 `.moye/.env`/export 优先，缺失时回退此处。
    /// Global API key store: `[keys]` section, keyed by env var name (e.g. `DEEPSEEK_API_KEY`),
    /// valued as the key itself. The project `.moye/.env`/export takes priority; this is the fallback.
    #[serde(default)]
    pub keys: HashMap<String, String>,
    /// MCP 服务器配置：`[mcp.<name>]` 小节，每个小节定义一个 MCP 服务器连接。
    /// 通过 `command`+`args`（stdio）或 `url`（HTTP/SSE）指定传输方式。
    /// MCP server configs: `[mcp.<name>]` sections, each defining one MCP server connection.
    /// Transport is selected by `command`+`args` (stdio) or `url` (HTTP/SSE).
    #[serde(default)]
    pub mcp: HashMap<String, McpServerConfig>,
    /// 定时任务调度器配置：`[scheduler]` 小节。
    /// Scheduler config: `[scheduler]` section.
    #[serde(default)]
    pub scheduler: crate::scheduler::SchedulerConfig,
}

/// `[agents]` 小节：内置角色配置 + 自定义子代理配置。
/// The `[agents]` section: built-in role configs + custom sub-agent configs.
///
/// `roles` 通过 `#[serde(flatten)]` 捕获所有非保留键（如 `orchestrator`、
/// `builder`），而 `custom` 显式捕获 `[agents.custom.<name>]` 子表。
/// 向后兼容：无 `[agents.custom]` 段时 `custom` 为空 map，行为与旧配置一致。
///
/// `roles` captures all non-reserved keys (e.g. `orchestrator`, `builder`)
/// via `#[serde(flatten)]`, while `custom` explicitly captures the
/// `[agents.custom.<name>]` sub-table. Backward-compatible: when no
/// `[agents.custom]` section is present, `custom` is an empty map and
/// behavior matches the old config exactly.
#[derive(Debug, Deserialize, Default)]
pub struct AgentsConfig {
    /// 内置角色配置（键为角色名，如 "orchestrator"/"builder"）。
    /// Built-in role configs (keyed by role name, e.g. "orchestrator"/"builder").
    #[serde(flatten)]
    pub roles: HashMap<String, RoleConfig>,
    /// 自定义子代理配置（`[agents.custom.<name>]` → 键为 name）。
    /// Custom sub-agent configs (`[agents.custom.<name>]` → key is name).
    #[serde(default)]
    pub custom: HashMap<String, CustomAgentConfig>,
}

/// 自定义子代理配置：`[agents.custom.<name>]` 小节。
/// Custom sub-agent config: the `[agents.custom.<name>]` section.
///
/// 镜像 RoleConfig 的字段，但 model 为可选（None 时用会话/注册表默认模型）。
/// permissions 字段的 serde 默认值与 RoleConfig 一致——省略的权限字段继承
/// ToolPerms 的默认值（read_file/run_bash_readonly 默认 Allow，
/// 其余默认 Ask）。
///
/// Mirrors RoleConfig fields, but model is optional (None → session/registry
/// default model). The permissions field's serde defaults match RoleConfig —
/// omitted permission fields inherit ToolPerms defaults (read_file /
/// run_bash_readonly default to Allow, the rest to Ask).
#[derive(Debug, Deserialize, Clone)]
pub struct CustomAgentConfig {
    /// preamble 提示词文件路径（相对项目根目录）。
    /// Preamble (prompt) file path, relative to the project root.
    pub preamble: String,
    /// 按工具的权限分级，serde 默认值与 RoleConfig 相同。
    /// Per-tool permission tiers, serde defaults match RoleConfig.
    #[serde(default)]
    pub permissions: ToolPerms,
    /// 模型；None 时用会话/注册表默认模型。
    /// Model; None → session/registry default.
    #[serde(default)]
    pub model: Option<String>,
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

    /// 是否允许网络访问（仅 bwrap 后端生效，默认 true）。
    /// false 时 bwrap 命令行附加 --unshare-net 隔离网络命名空间。
    /// Landlock 后端无法控制网络，此选项被忽略（启动时 warn）。
    /// Whether to allow network access (bwrap backend only, default true).
    /// When false, bwrap argv gets --unshare-net to isolate the net namespace.
    /// Landlock can't do network namespaces; this option is ignored (warn at startup).
    #[serde(default = "default_allow_network")]
    pub allow_network: bool,

    /// 命令规则：对 `run_bash` 的 command 做 glob 匹配，首条匹配胜出（allow/ask/deny）。
    /// 省略时为空（完全向后兼容，行为与现有只读/会改变状态分类一致）。
    /// Command rules: glob-matched against `run_bash` commands, first match wins
    /// (allow/ask/deny). Absent → empty (fully backward compatible, behavior
    /// matches existing readonly/mutating classification).
    #[serde(default)]
    pub command_rules: Vec<CommandRule>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: default_sandbox_backend(),
            mode: default_sandbox_mode(),
            authorized_dirs: Vec::new(),
            allow_network: true,
            command_rules: Vec::new(),
        }
    }
}

fn default_sandbox_backend() -> String {
    "auto".to_string()
}

fn default_sandbox_mode() -> String {
    "auto".to_string()
}

fn default_allow_network() -> bool {
    true
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

/// `[verify]` 小节：验证门配置（Builder 产出后、Auditor 评审前自动运行构建/测试）。
/// The `[verify]` section: verify gate config (auto-run build/test after the
/// Builder, before the Auditor).
///
/// `enabled` 控制是否启用验证门（默认 true）。
/// `enabled` controls whether the gate is active (default true).
///
/// `commands` 为可选覆盖列表；省略时按项目根的标记文件自动检测
/// （Cargo.toml → cargo build/test, package.json → npm test, 等）。
/// `commands` is an optional override; when omitted, auto-detected from the
/// project root's marker files (Cargo.toml → cargo build/test, etc.).
///
/// `max_retries` 为失败后的最大重试次数（默认 2）。
/// `max_retries` is the max retry count after failure (default 2).
///
/// `timeout_secs` 为每条命令的超时秒数（默认 600）。
/// `timeout_secs` is the per-command timeout in seconds (default 600).
#[derive(Debug, Deserialize)]
pub struct VerifyConfig {
    #[serde(default = "default_verify_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub commands: Option<Vec<String>>,
    #[serde(default = "default_verify_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_verify_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: default_verify_enabled(),
            commands: None,
            max_retries: default_verify_max_retries(),
            timeout_secs: default_verify_timeout_secs(),
        }
    }
}

fn default_verify_enabled() -> bool {
    true
}

fn default_verify_max_retries() -> u32 {
    2
}

fn default_verify_timeout_secs() -> u64 {
    600
}

static CONFIG: OnceLock<Arc<Config>> = OnceLock::new();

// ── 项目配置文件路径 ─────────────────────────────────────────────────────
// 项目级配置文件统一放在当前目录的 `.moye/` 下，与全局配置目录
// `~/.config/moye/` 对称，便于统一管理；`.moye/` 整体在 .gitignore 中，
// 一条规则即可覆盖所有本地配置与密钥。
// Project-level config files live under `.moye/` in the current directory,
// mirroring the global `~/.config/moye/` directory. `.moye/` is git-ignored
// as a whole, so one rule covers every local config file and secret.

/// 项目级配置目录：本地配置文件（agent.toml / .env）统一放在此目录下。
/// Project config directory: local config files (agent.toml / .env) live here.
pub const PROJECT_CONFIG_DIR: &str = ".moye";
/// 项目级配置文件路径（`.moye/agent.toml`）。
/// Project config file path (`.moye/agent.toml`).
pub const PROJECT_CONFIG_PATH: &str = ".moye/agent.toml";
/// 项目级环境变量文件路径（`.moye/.env`）。
/// Project env-file path (`.moye/.env`).
pub const PROJECT_ENV_PATH: &str = ".moye/.env";
/// 旧版布局：仓库根的 `agent.toml`。仅用于自动迁移与存在性检测。
/// Legacy layout: repo-root `agent.toml`. Only used for auto-migration and checks.
pub const LEGACY_CONFIG_PATH: &str = "agent.toml";
/// 旧版布局：仓库根的 `.env`。仅用于自动迁移与存在性检测。
/// Legacy layout: repo-root `.env`. Only used for auto-migration and checks.
pub const LEGACY_ENV_PATH: &str = ".env";

/// 把旧版布局（仓库根的 `agent.toml` / `.env`）迁移到 `.moye/` 下。
/// 新路径已存在时不动旧文件（新布局优先）。启动早期调用一次。
/// Migrate the legacy layout (repo-root `agent.toml` / `.env`) into `.moye/`.
/// When the new path already exists the legacy file is left untouched (the new
/// layout wins). Call once early at startup.
pub fn migrate_legacy_config_files() {
    migrate_legacy_file(LEGACY_CONFIG_PATH, PROJECT_CONFIG_PATH);
    migrate_legacy_file(LEGACY_ENV_PATH, PROJECT_ENV_PATH);
}

fn migrate_legacy_file(legacy: &str, new: &str) {
    let legacy_p = std::path::Path::new(legacy);
    let new_p = std::path::Path::new(new);
    if !legacy_p.exists() || new_p.exists() {
        return;
    }
    if let Some(parent) = new_p.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("[config] 创建 {PROJECT_CONFIG_DIR}/ 失败: {e}；跳过迁移 {legacy}");
        return;
    }
    match std::fs::rename(legacy_p, new_p) {
        Ok(()) => eprintln!("[config] 已迁移 {legacy} → {new}"),
        Err(_) => {
            // rename 失败（如跨设备）时退回复制 + 删除，保留文件权限。
            // Fall back to copy + remove when rename fails (e.g. cross-device);
            // fs::copy preserves file permissions.
            match std::fs::copy(legacy_p, new_p) {
                Ok(_) => {
                    let _ = std::fs::remove_file(legacy_p);
                    eprintln!("[config] 已迁移 {legacy} → {new}");
                }
                Err(e) => {
                    eprintln!("[config] 迁移 {legacy} → {new} 失败: {e}（保留原文件）")
                }
            }
        }
    }
}

/// 加载项目 `.env` 到进程环境：优先 `.moye/.env`，不存在时回退仓库根 `.env`
/// （旧布局，未被迁移时的兜底）。已显式 export 的环境变量优先，不会被覆盖
/// （dotenvy 语义）。返回实际加载的文件路径；两者都不存在时返回 None。
/// Load the project `.env` into the process environment: prefers `.moye/.env`,
/// falls back to the repo-root `.env` (legacy layout). Explicitly exported
/// variables take precedence and are never overridden (dotenvy semantics).
/// Returns the loaded path, or None when neither file exists.
pub fn load_dotenv() -> Option<PathBuf> {
    let new = std::path::Path::new(PROJECT_ENV_PATH);
    if new.exists() {
        // from_path 返回 Result<()>，成功时手动返回路径。
        // from_path returns Result<()>; return the path manually on success.
        dotenvy::from_path(new).ok().map(|_| new.to_path_buf())
    } else {
        dotenvy::dotenv().ok()
    }
}

/// 启动时调用一次：加载项目 `.moye/agent.toml`，再用全局 `~/.config/moye/config.toml`
/// 作为 fallback 填充项目中缺失的 `[provider]` 字段，最后缓存并返回共享 Arc。
/// 合并优先级：环境变量 > 项目 .moye/agent.toml > 全局 config.toml > 供应商默认。
/// Call once at startup: loads the project `.moye/agent.toml`, then fills in any missing
/// `[provider]` fields from the global `~/.config/moye/config.toml` as a fallback,
/// before caching and returning the shared Arc.
/// Precedence: env vars > project .moye/agent.toml > global config.toml > provider default.
///
/// 如果 `.moye/agent.toml` 不存在，先从全局配置 + 默认模板自动生成一个，再继续加载。
/// If `.moye/agent.toml` doesn't exist, auto-generate one from the global config + default
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

/// 检查项目配置（`.moye/agent.toml`，或旧版根目录 `agent.toml`）或全局
/// `~/.config/moye/config.toml` 是否存在。任一存在即跳过 setup 向导。
/// Checks if a project config (`.moye/agent.toml`, or the legacy repo-root
/// `agent.toml`) or the global `~/.config/moye/config.toml` exists.
/// Either being present skips the setup wizard.
pub fn has_config_file() -> bool {
    std::path::Path::new(PROJECT_CONFIG_PATH).exists()
        || std::path::Path::new(LEGACY_CONFIG_PATH).exists()
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
///
/// 仅测试使用；生产代码直接调用 `default_model_for_provider_plan`。
/// Test-only; production code calls `default_model_for_provider_plan` directly.
#[cfg(test)]
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

/// 当项目 `.moye/agent.toml` 不存在时，从全局配置 `~/.config/moye/config.toml`
/// 的 `[provider]` 信息 + 合理默认值自动生成一个完整的 `agent.toml`。
/// When the project `.moye/agent.toml` doesn't exist, auto-generate a complete one from
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
    // 目标路径带子目录（如 `.moye/agent.toml`）时先创建目录；
    // 裸文件名（父目录为空，测试用例常见）跳过。
    // Create the parent dir when the target path has one (e.g. `.moye/agent.toml`);
    // skip for bare filenames (empty parent, common in tests).
    if let Some(parent) = std::path::Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
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
# 可按需修改各参数。.moye/ 目录已在 .gitignore 中，不会被提交。
# Edit as needed. The whole .moye/ directory is git-ignored and won't be committed.

[provider]
{provider_section}

[agent]
default_model = "{model}"
max_turns = 50

[context]
max_output_tokens = 0
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
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"
permissions.write_file = "allow"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.investigator]
model = "{model}"
preamble = "prompts/investigator.md"
max_turns = 50
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"
permissions.write_file = "allow"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.planner]
model = "{model}"
preamble = "prompts/planner.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"
permissions.write_file = "allow"
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
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"
permissions.write_file = "allow"
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

// ── persist_authorized_dir ─────────────────────────────────────────────────

/// 将一个授权目录持久化到 `.moye/agent.toml` 的 `[sandbox].authorized_dirs` 数组。
/// 持久化采用字符串级编辑——不通过 toml crate 重新序列化，保留文件中的注释和格式。
///
/// 此函数仅影响**未来会话**的配置加载；当前会话的授权已在 agent_loop 中通过
/// `sandbox.authorize_tool` 完成。不热加载——运行中的配置不变。
/// Persist an authorized directory to `.moye/agent.toml`'s `[sandbox].authorized_dirs`.
/// Uses string-level editing — no toml re-serialization, preserving comments and
/// formatting elsewhere in the file.
///
/// This only affects **future sessions**' config loading; the current session's
/// authorization was already applied via `sandbox.authorize_tool` in agent_loop.
/// No hot-reload — the running config is unchanged.
pub fn persist_authorized_dir(dir: &str) -> anyhow::Result<()> {
    persist_authorized_dir_to(dir, PROJECT_CONFIG_PATH)
}

/// 可测试变体：写入显式路径的配置文件。
/// Testable variant: writes to an explicit config file path.
///
/// 策略（按情况）：
/// 1. 文件不存在 → 创建仅含 `[sandbox]\nauthorized_dirs = ["dir"]\n` 的最小文件
///    （其余配置回退到全局 config/默认值）。
/// 2. 有文件但无 `[sandbox]` 小节 → 在文件末尾追加 `\n[sandbox]\nauthorized_dirs = ["dir"]\n`。
/// 3. `[sandbox]` 存在但无 `authorized_dirs` 键 → 在小节头部后插入 `authorized_dirs = ["dir"]`。
/// 4. `authorized_dirs` 已存在 → 解析数组条目，按规范化路径去重；已存在则 no-op；
///    否则将新目录追加到数组（单行形式）。多行数组会被折叠为合并后的单行数组。
///
/// Strategy (per case):
/// 1. File missing → create minimal `[sandbox]\nauthorized_dirs = ["dir"]\n` (other
///    config falls back to global/defaults).
/// 2. File exists, no `[sandbox]` section → append `\n[sandbox]\nauthorized_dirs = ["dir"]\n`.
/// 3. `[sandbox]` exists, no `authorized_dirs` key → insert after the section header.
/// 4. `authorized_dirs` exists → parse entries, dedup by canonical path; no-op if
///    already present; otherwise append. Multi-line arrays are collapsed to single-line.
fn persist_authorized_dir_to(dir: &str, config_path: &str) -> anyhow::Result<()> {
    let quoted = format!("\"{}\"", escape_toml_string(dir));

    if !std::path::Path::new(config_path).exists() {
        let content = format!("[sandbox]\nauthorized_dirs = [{quoted}]\n");
        std::fs::write(config_path, content)?;
        return Ok(());
    }

    let content = std::fs::read_to_string(config_path)?;
    let lines: Vec<&str> = content.lines().collect();

    let sandbox_start = find_section_start(&lines, "sandbox");
    let sandbox_end = sandbox_start.map(|s| section_end(&lines, s)).unwrap_or(0);

    match sandbox_start {
        Some(start) => {
            let end = sandbox_end;
            let auth_line_rel = (start + 1..end).find(|&i| {
                let t = lines[i].trim();
                t.starts_with("authorized_dirs") && t.contains('=')
            });

            match auth_line_rel {
                Some(auth_idx) => {
                    let existing = parse_array_entries_on_line(&lines, auth_idx, end);
                    let canon_input = canonicalize_dir(dir);
                    let already_present = existing.iter().any(|e| {
                        canonicalize_dir(e) == canon_input
                    });
                    if already_present {
                        std::fs::write(config_path, content)?;
                        return Ok(());
                    }

                    let mut merged: Vec<String> = existing.iter().map(|s| s.to_string()).collect();
                    merged.push(dir.to_string());
                    let new_line = build_authorized_dirs_line(&merged);

                    let mut out = String::with_capacity(content.len() + 64);
                    let multi_end = array_end_line(&lines, auth_idx, end);
                    for (i, l) in lines.iter().enumerate() {
                        if i == auth_idx {
                            out.push_str(&new_line);
                        } else if i > auth_idx && i <= multi_end {
                            // Skip multi-line array continuation lines (collapsed to single line).
                        } else {
                            out.push_str(l);
                            out.push('\n');
                        }
                    }
                    std::fs::write(config_path, out)?;
                }
                None => {
                    let mut out = String::with_capacity(content.len() + 64);
                    for (i, l) in lines.iter().enumerate() {
                        out.push_str(l);
                        out.push('\n');
                        if i == start {
                            out.push_str(&format!("authorized_dirs = [{quoted}]\n"));
                        }
                    }
                    std::fs::write(config_path, out)?;
                }
            }
        }
        None => {
            let mut out = String::with_capacity(content.len() + 64);
            out.push_str(&content);
            if !content.is_empty() && !content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("\n[sandbox]\nauthorized_dirs = [{quoted}]\n"));
            std::fs::write(config_path, out)?;
        }
    }

    Ok(())
}

/// 在行数组中查找指定小节头（如 `"sandbox"` → 匹配 `[sandbox]`）的行号。
/// 不匹配 `[agents.sandbox]` 等带点前缀的小节。
/// Find the line index of an exact `[section_name]` header (e.g. `"sandbox"` →
/// `[sandbox]`). Does NOT match dotted prefixes like `[agents.sandbox]`.
fn find_section_start(lines: &[&str], section: &str) -> Option<usize> {
    let target = format!("[{section}]");
    lines.iter().position(|l| l.trim() == target)
}

/// 返回小节的结束行号（exclusive）：下一个以 `[` 开头的行或文件末尾。
/// Return the exclusive end line index of a section: the next `[` header or EOF.
fn section_end(lines: &[&str], start: usize) -> usize {
    let start = start + 1;
    let mut i = start;
    while i < lines.len() {
        if lines[i].trim().starts_with('[') {
            return i;
        }
        i += 1;
    }
    lines.len()
}

/// 解析 `authorized_dirs = [...]` 行中的数组条目。
/// 如果数组跨越多行，从 `auth_idx` 扫描到 `]` 或 `section_end`，收集所有引号字符串。
/// Parse the array entries from an `authorized_dirs = [...]` assignment.
/// If the array spans multiple lines, scan from `auth_idx` until `]` or
/// `section_end`, collecting all quoted strings.
fn parse_array_entries_on_line(lines: &[&str], auth_idx: usize, section_end: usize) -> Vec<String> {
    let mut raw = String::new();
    for i in auth_idx..section_end {
        raw.push_str(lines[i]);
        if lines[i].contains(']') {
            break;
        }
        raw.push('\n');
    }
    parse_toml_string_array(&raw)
}

/// 从包含 `authorized_dirs = [...]` 的原始文本中提取引号内的字符串条目。
/// Extract quoted string entries from raw text containing `authorized_dirs = [...]`.
fn parse_toml_string_array(raw: &str) -> Vec<String> {
    let bracket_start = match raw.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let bracket_end = match raw[bracket_start..].find(']') {
        Some(i) => bracket_start + i,
        None => return Vec::new(),
    };
    let inside = &raw[bracket_start + 1..bracket_end];
    inside
        .split(',')
        .filter_map(|token| {
            let token = token.trim();
            if token.is_empty() {
                return None;
            }
            let unquoted = token
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .unwrap_or(token);
            Some(unescape_toml_string(unquoted))
        })
        .collect()
}

/// 构建单行 `authorized_dirs = [...]` 赋值行。
/// Build a single-line `authorized_dirs = [...]` assignment line.
fn build_authorized_dirs_line(dirs: &[String]) -> String {
    let entries: Vec<String> = dirs
        .iter()
        .map(|d| format!("\"{}\"", escape_toml_string(d)))
        .collect();
    format!("authorized_dirs = [{}]", entries.join(", "))
}

/// 如果 `authorized_dirs` 是多行数组，返回最后一行的索引（含 `]` 的行）；
/// 单行数组返回 `auth_idx` 本身。
/// If `authorized_dirs` is a multi-line array, return the index of the line
/// containing `]`; single-line arrays return `auth_idx` itself.
fn array_end_line(lines: &[&str], auth_idx: usize, section_end: usize) -> usize {
    if lines[auth_idx].contains(']') {
        return auth_idx;
    }
    for i in auth_idx + 1..section_end {
        if lines[i].contains(']') {
            return i;
        }
    }
    auth_idx
}

/// 规范化目录路径用于去重比较（与 sandbox.rs 的 canonicalize 逻辑一致）。
/// Canonicalize a directory path for dedup comparison (matches sandbox.rs logic).
fn canonicalize_dir(dir: &str) -> String {
    let expanded = crate::sandbox::expand_tilde(dir);
    let path = std::path::Path::new(&expanded);
    if path.is_absolute() {
        match path.canonicalize() {
            Ok(canon) => canon.to_string_lossy().to_string(),
            Err(_) => expanded,
        }
    } else {
        match std::env::current_dir() {
            Ok(cwd) => {
                let abs = cwd.join(path);
                match abs.canonicalize() {
                    Ok(canon) => canon.to_string_lossy().to_string(),
                    Err(_) => abs.to_string_lossy().to_string(),
                }
            }
            Err(_) => expanded,
        }
    }
}

/// 转义 TOML 基本字符串中的特殊字符（`"` 和 `\`）。
/// Escape special chars in a TOML basic string (`"` and `\`).
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 反转义 TOML 基本字符串中的转义序列。
/// Unescape TOML basic string escape sequences.
fn unescape_toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Permission;
    use std::path::PathBuf;

    // 使用 crate 根的共享 env 互斥锁（避免跨模块 env 竞争）。
    // Use the crate-root shared env mutex to avoid cross-module env races.
    use crate::TEST_ENV_MUTEX as ENV_MUTEX;

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
        assert!(cfg.agents.roles.contains_key("builder"));
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
        assert!(cfg.agents.roles.is_empty());
        assert_eq!(cfg.evolution.rule_escalation_threshold, 0);
        assert_eq!(cfg.context.max_output_tokens, 0);
        assert!(cfg.sandbox.authorized_dirs.is_empty());
        assert_eq!(cfg.sandbox.backend, "auto");
        assert_eq!(cfg.sandbox.mode, "auto");
        assert!(cfg.sandbox.allow_network);
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
        assert!(cfg.agents.roles.contains_key("orchestrator"));
        assert!(cfg.agents.roles.contains_key("investigator"));
        assert!(cfg.agents.roles.contains_key("planner"));
        assert!(cfg.agents.roles.contains_key("builder"));
        assert!(cfg.agents.roles.contains_key("auditor"));
        // 模型应有值（来自全局配置或供应商默认）。
        // Model should be set (from global config or provider default).
        assert!(!cfg.agent.default_model.is_empty());
        assert_eq!(
            cfg.agents.roles.get("builder").unwrap().model,
            cfg.agent.default_model
        );
        // builder 应有写权限。
        assert_eq!(
            cfg.agents.roles.get("builder").unwrap().permissions.write_file,
            Permission::Allow
        );
        // auditor 应拒绝 web 访问。
        assert_eq!(
            cfg.agents.roles.get("auditor").unwrap().permissions.web_fetch,
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
        let builder = cfg.agents.roles.get("builder").expect("builder role present");
        assert_eq!(builder.permissions.read_file, Permission::Deny);
        assert_eq!(builder.permissions.edit_file, Permission::Deny);
        assert_eq!(builder.permissions.run_bash_mutating, Permission::Deny);
        assert_eq!(builder.model, "glm-latest");
    }

    #[test]
    fn no_profile_section_backward_compat() {
        let _env_lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _g = env_guard("AGENT_PROFILE", None);
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
    fn sandbox_allow_network_defaults_true() {
        // Given: [sandbox] without allow_network field.
        // When: parsing.
        // Then: cfg.sandbox.allow_network == true (default).
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "auto"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.sandbox.allow_network);
    }

    #[test]
    fn sandbox_allow_network_parsed_false() {
        // Given: [sandbox] with allow_network = false.
        // When: parsing.
        // Then: cfg.sandbox.allow_network == false.
        let toml_str = r#"
[sandbox]
backend = "bwrap"
mode = "bwrap"
allow_network = false
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.sandbox.allow_network);
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
        // Then: cfg.agents.roles["builder"].model == "profile-model".
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
        let builder = cfg.agents.roles.get("builder").expect("builder role present");
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

    // ── [verify] 小节测试 / [verify] section tests ──

    #[test]
    fn verify_defaults_when_section_absent() {
        // Given: config with no [verify] section.
        // When: parsing.
        // Then: all defaults apply (enabled=true, commands=None, max_retries=2, timeout_secs=600).
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.verify.enabled);
        assert!(cfg.verify.commands.is_none());
        assert_eq!(cfg.verify.max_retries, 2);
        assert_eq!(cfg.verify.timeout_secs, 600);
    }

    #[test]
    fn verify_explicit_override() {
        // Given: explicit [verify] section with all fields.
        // When: parsing.
        // Then: values match the config, not defaults.
        let toml_str = r#"
[verify]
enabled = false
commands = ["make check"]
max_retries = 5
timeout_secs = 120
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.verify.enabled);
        assert_eq!(
            cfg.verify.commands.as_deref(),
            Some(["make check".to_string()].as_slice())
        );
        assert_eq!(cfg.verify.max_retries, 5);
        assert_eq!(cfg.verify.timeout_secs, 120);
    }

    #[test]
    fn verify_partial_override_keeps_defaults() {
        // Given: [verify] with only enabled=false.
        // When: parsing.
        // Then: enabled is overridden, other fields keep defaults.
        let cfg: Config = toml::from_str("[verify]\nenabled = false\n").unwrap();
        assert!(!cfg.verify.enabled);
        assert!(cfg.verify.commands.is_none());
        assert_eq!(cfg.verify.max_retries, 2);
        assert_eq!(cfg.verify.timeout_secs, 600);
    }

    #[test]
    fn verify_section_does_not_break_profile_overlay() {
        // Given: config with [verify] + a profile patching [sandbox].
        // When: parsing with the profile.
        // Then: profile patch applies; [verify] is untouched by the profile.
        let toml_str = r#"
[verify]
enabled = true

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
        let cfg = Config::from_str_with_profile(toml_str, Some("dev"))
            .expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.mode, "landlock");
        assert!(cfg.verify.enabled);
    }

    // ── [agents.custom.*] 测试 / [agents.custom.*] tests ──

    #[test]
    fn custom_agent_parses_with_full_fields() {
        let toml_str = r#"
[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.read_file = "allow"

[agents.custom.researcher]
preamble = "agents/researcher.md"
model = "kimi-k3"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.write_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        // built-in role still present.
        assert!(cfg.agents.roles.contains_key("builder"));
        // custom agent parsed.
        let r = cfg.agents.custom.get("researcher").expect("researcher present");
        assert_eq!(r.preamble, "agents/researcher.md");
        assert_eq!(r.model.as_deref(), Some("kimi-k3"));
        assert_eq!(r.permissions.read_file, Permission::Allow);
        assert_eq!(r.permissions.run_bash_mutating, Permission::Deny);
        assert_eq!(r.permissions.web_fetch, Permission::Allow);
    }

    #[test]
    fn custom_agent_omitted_permissions_get_defaults() {
        let toml_str = r#"
[agents.custom.minimal]
preamble = "agents/minimal.md"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let r = cfg.agents.custom.get("minimal").expect("minimal present");
        assert_eq!(r.preamble, "agents/minimal.md");
        assert!(r.model.is_none());
        // ToolPerms defaults: read_file/run_bash_readonly = Allow, rest = Ask.
        assert_eq!(r.permissions.read_file, Permission::Allow);
        assert_eq!(r.permissions.run_bash_readonly, Permission::Allow);
        assert_eq!(r.permissions.run_bash_mutating, Permission::Ask);
        assert_eq!(r.permissions.edit_file, Permission::Ask);
        assert_eq!(r.permissions.write_file, Permission::Ask);
        assert_eq!(r.permissions.web_fetch, Permission::Ask);
        assert_eq!(r.permissions.web_search, Permission::Ask);
    }

    #[test]
    fn custom_agent_absent_map_backward_compat() {
        let toml_str = r#"
[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.agents.custom.is_empty(), "no custom → empty map");
        assert!(cfg.agents.roles.contains_key("builder"));
    }

    #[test]
    fn custom_agent_empty_config_no_custom_map() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.agents.custom.is_empty());
        assert!(cfg.agents.roles.is_empty());
    }

    #[test]
    fn two_custom_agents_coexist() {
        let toml_str = r#"
[agents.custom.researcher]
preamble = "agents/researcher.md"
permissions.read_file = "allow"

[agents.custom.reviewer]
preamble = "agents/reviewer.md"
model = "glm-latest"
permissions.edit_file = "deny"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.agents.custom.len(), 2);
        assert!(cfg.agents.custom.contains_key("researcher"));
        assert!(cfg.agents.custom.contains_key("reviewer"));
        assert_eq!(
            cfg.agents.custom.get("reviewer").unwrap().model.as_deref(),
            Some("glm-latest")
        );
    }

    #[test]
    fn custom_agent_does_not_leak_into_roles() {
        let toml_str = r#"
[agents.custom.researcher]
preamble = "agents/researcher.md"

[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(!cfg.agents.roles.contains_key("custom"));
        assert!(!cfg.agents.roles.contains_key("researcher"));
        assert!(cfg.agents.roles.contains_key("builder"));
        assert!(cfg.agents.custom.contains_key("researcher"));
    }

    // ── [sandbox].command_rules 测试 / command_rules tests ──

    #[test]
    fn sandbox_command_rules_absent_defaults_empty() {
        let cfg: Config = toml::from_str("[sandbox]\nbackend = \"auto\"\n").unwrap();
        assert!(cfg.sandbox.command_rules.is_empty());
    }

    #[test]
    fn sandbox_command_rules_parsed_in_order() {
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "auto"
command_rules = [
  { pattern = "cargo test*", tier = "allow" },
  { pattern = "rm *", tier = "deny" },
  { pattern = "git * log", tier = "allow" },
]
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.sandbox.command_rules.len(), 3);
        assert_eq!(cfg.sandbox.command_rules[0].pattern, "cargo test*");
        assert_eq!(cfg.sandbox.command_rules[0].tier, Permission::Allow);
        assert_eq!(cfg.sandbox.command_rules[1].pattern, "rm *");
        assert_eq!(cfg.sandbox.command_rules[1].tier, Permission::Deny);
        assert_eq!(cfg.sandbox.command_rules[2].pattern, "git * log");
        assert_eq!(cfg.sandbox.command_rules[2].tier, Permission::Allow);
    }

    #[test]
    fn sandbox_command_rules_profile_overlay_preserves_rules() {
        let toml_str = r#"
[sandbox]
backend = "auto"
mode = "auto"
command_rules = [{ pattern = "cargo test*", tier = "allow" }]

[profile.strict]
name = "strict"
patches = [
  { id = "sandbox", config = { backend = "landlock", mode = "landlock", authorized_dirs = [], command_rules = [{ pattern = "rm *", tier = "deny" }] } },
]
"#;
        let cfg = Config::from_str_with_profile(toml_str, Some("strict"))
            .expect("profile parse should succeed");
        assert_eq!(cfg.sandbox.backend, "landlock");
        assert_eq!(cfg.sandbox.command_rules.len(), 1);
        assert_eq!(cfg.sandbox.command_rules[0].pattern, "rm *");
        assert_eq!(cfg.sandbox.command_rules[0].tier, Permission::Deny);
    }

    #[test]
    fn tool_perms_command_rules_serde_skipped_in_role_config() {
        let toml_str = r#"
[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        let builder = cfg.agents.roles.get("builder").unwrap();
        assert!(
            builder.permissions.command_rules.is_empty(),
            "serde(skip) → command_rules always empty when deserialized from role TOML"
        );
    }

    // ── persist_authorized_dir 测试 / persist_authorized_dir tests ──

    fn temp_config_path(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("moye-test-persist-{name}.toml"));
        let _ = std::fs::remove_file(&path);
        path.to_string_lossy().to_string()
    }

    fn make_temp_dir(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("moye-test-authdir-{name}"));
        let _ = std::fs::create_dir_all(&path);
        path.to_string_lossy().to_string()
    }

    #[test]
    fn persist_missing_file_creates_minimal() {
        let path = temp_config_path("missing");
        let dir = make_temp_dir("missing-file");
        persist_authorized_dir_to(&dir, &path).expect("persist should succeed");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[sandbox]"), "must contain [sandbox] section");
        assert!(
            raw.contains(&dir),
            "must contain the dir: {raw}"
        );
        let cfg: Config = toml::from_str(&raw).expect("written file must parse");
        assert!(
            cfg.sandbox.authorized_dirs.iter().any(|d| d == &dir),
            "sandbox.authorized_dirs must contain the dir"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_file_with_other_sections_no_sandbox() {
        let path = temp_config_path("no-sandbox");
        let dir = make_temp_dir("no-sandbox");
        std::fs::write(
            &path,
            "[provider]\nprovider = \"deepseek\"\n\n[agent]\ndefault_model = \"kimi-k3\"\n",
        )
        .unwrap();
        persist_authorized_dir_to(&dir, &path).expect("persist should succeed");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[provider]"), "existing sections preserved");
        assert!(raw.contains("[sandbox]"), "[sandbox] appended");
        assert!(raw.contains(&dir), "dir present");
        let cfg: Config = toml::from_str(&raw).expect("file must still parse");
        assert!(cfg.sandbox.authorized_dirs.iter().any(|d| d == &dir));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_sandbox_exists_no_authorized_dirs_key() {
        let path = temp_config_path("no-key");
        let dir = make_temp_dir("no-key");
        std::fs::write(
            &path,
            "[sandbox]\nbackend = \"auto\"\nmode = \"landlock\"\n",
        )
        .unwrap();
        persist_authorized_dir_to(&dir, &path).expect("persist should succeed");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("backend = \"auto\""), "existing keys preserved");
        assert!(
            raw.contains("authorized_dirs"),
            "authorized_dirs key added"
        );
        assert!(raw.contains(&dir), "dir present");
        let cfg: Config = toml::from_str(&raw).expect("file must parse");
        assert!(cfg.sandbox.authorized_dirs.iter().any(|d| d == &dir));
        assert_eq!(cfg.sandbox.backend, "auto");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_single_line_array_appends_and_dedupes() {
        let path = temp_config_path("single-line");
        let dir1 = make_temp_dir("single-1");
        let dir2 = make_temp_dir("single-2");
        let initial = format!(
            "[sandbox]\nbackend = \"auto\"\nauthorized_dirs = [\"{dir1}\"]\n"
        );
        std::fs::write(&path, &initial).unwrap();

        persist_authorized_dir_to(&dir2, &path).expect("first persist");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(&dir1), "existing dir preserved");
        assert!(raw.contains(&dir2), "new dir appended");
        let cfg: Config = toml::from_str(&raw).expect("file must parse");
        assert_eq!(cfg.sandbox.authorized_dirs.len(), 2);

        persist_authorized_dir_to(&dir2, &path).expect("second persist (dedup)");
        let raw2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, raw2, "second call with same dir must be no-op");
        let cfg2: Config = toml::from_str(&raw2).expect("file must parse");
        assert_eq!(
            cfg2.sandbox.authorized_dirs.len(),
            2,
            "no duplicate entry added"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn persist_roundtrip_config_loader_contains_dir() {
        let path = temp_config_path("roundtrip");
        let dir = make_temp_dir("roundtrip");
        std::fs::write(
            &path,
            "[sandbox]\nbackend = \"bwrap\"\nmode = \"auto\"\n",
        )
        .unwrap();
        persist_authorized_dir_to(&dir, &path).expect("persist");
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&raw).expect("must parse via config loader");
        assert!(
            cfg.sandbox.authorized_dirs.iter().any(|d| d == &dir),
            "sandbox.authorized_dirs must contain the persisted dir"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_preserves_comments_elsewhere() {
        let path = temp_config_path("comments");
        let dir = make_temp_dir("comments");
        let initial = "[provider]\nprovider = \"deepseek\"\n# important comment\n\n[sandbox]\nbackend = \"auto\"\n";
        std::fs::write(&path, initial).unwrap();
        persist_authorized_dir_to(&dir, &path).expect("persist");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("# important comment"),
            "comment elsewhere must be byte-present after persist"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
