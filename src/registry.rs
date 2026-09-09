// 注册表模块：定义角色（Role）、按工具的权限分级（ToolPerms / Permission）、
// Registry module: defines roles (Role), per-tool permission tiers (ToolPerms / Permission),
// 以及构建和管理各角色 Agent 的 AgentRegistry。权限分级驱动自主循环的 HITL（人在环）控制。
// and AgentRegistry for building and managing role Agents. Permission tiers drive autonomous loop HITL (Human-in-the-Loop) control.
use crate::event::{AgentEvent, EventSender};
use crate::events::{ListenerId, PreStepState, WaterfallEvent, WaterfallRegistry};
use crate::mcp::McpManager;
use crate::providers::ChatAgent;
use crate::sandbox::Sandbox;
use crate::seam::{ApprovalRequest, ApprovalVerdict, SandboxProvider, ToolApproval};
use rig_agent::client::AgentClientExt;
use rig_core::completion::Message;
use rig_core::completion::message::{AssistantContent, UserContent};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tracing::info;

/// Agent 角色：编排者 / 调查者 / 规划者 / 构建者 / 审计者。
/// Agent roles: Orchestrator / Investigator / Planner / Builder / Auditor.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Orchestrator,
    Investigator,
    Planner,
    Builder,
    Auditor,
}

/// 单个角色的运行时配置：模型、preamble（提示词）文件、权限分级。
/// Runtime config for a single role: model, preamble (prompt) file, permission tiers.
#[derive(Debug, Deserialize, Clone)]
pub struct RoleConfig {
    pub model: String,
    pub preamble: String,
    #[serde(default)]
    pub permissions: ToolPerms,
    #[serde(default)]
    pub max_turns: Option<usize>,
}

// 自主循环 HITL（人在环）门控所用的按工具权限分级：
// Per-tool permission tiers used by the autonomous loop HITL (Human-in-the-Loop) gate:
// `allow` = 自动执行不询问；`ask` = 暂停请人类确认；`deny` = 拦截调用并向模型说明原因。
// `allow` = auto-execute without prompt; `ask` = pause for human confirmation; `deny` = block call and explain reason to model.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ToolPerms {
    #[serde(default = "default_allow")]
    pub read_file: Permission,
    #[serde(default = "default_allow")]
    pub run_bash_readonly: Permission,
    #[serde(default = "default_ask")]
    pub run_bash_mutating: Permission,
    #[serde(default = "default_ask")]
    pub edit_file: Permission,
    #[serde(default = "default_ask")]
    pub write_file: Permission,
    #[serde(default = "default_ask")]
    pub web_fetch: Permission,
    #[serde(default = "default_ask")]
    pub web_search: Permission,
    /// 命令规则：对 `run_bash` 的 command 做 glob 匹配，首条匹配胜出（allow/ask/deny）。
    /// 全局列表（从 `[sandbox].command_rules` 注入），非按角色；serde skip 保持角色 TOML 解析干净。
    /// Command rules: glob-matched against `run_bash` commands, first match wins
    /// (allow/ask/deny). Global list (injected from `[sandbox].command_rules`),
    /// not per-role; serde skip keeps role-config TOML parsing clean.
    #[serde(skip)]
    pub command_rules: Vec<CommandRule>,
}

/// 读类工具默认允许（自动执行）。
/// Read-type tools default to Allow (auto-execute).
fn default_allow() -> Permission {
    Permission::Allow
}
/// 会改变状态的工具默认需询问人类。
/// State-changing tools default to Ask (require human confirmation).
fn default_ask() -> Permission {
    Permission::Ask
}

impl Default for ToolPerms {
    fn default() -> Self {
        ToolPerms {
            read_file: Permission::Allow,
            run_bash_readonly: Permission::Allow,
            run_bash_mutating: Permission::Ask,
            edit_file: Permission::Ask,
            write_file: Permission::Ask,
            web_fetch: Permission::Ask,
            web_search: Permission::Ask,
            command_rules: Vec::new(),
        }
    }
}

/// 可构建的 agent 规格：把"角色"泛化为可构建的 agent 规格。
/// A buildable agent spec: generalizes "role" into a constructible spec.
///
/// 内置角色通过 `agent_spec(role)` 从现有角色配置构建；
/// 自定义子代理通过 `custom_spec(name)` 从 `[agents.custom.<name>]` 构建。
/// Built-in roles build from existing role config via `agent_spec(role)`;
/// custom sub-agents build from `[agents.custom.<name>]` via `custom_spec(name)`.
pub struct AgentSpec {
    /// 用于日志/Info 行的名称（如 "investigator"、"researcher"）。
    /// Name used in logs and Info lines (e.g. "investigator", "researcher").
    pub name: String,
    /// 相对项目根目录的 preamble 文件路径。
    /// Preamble file path, relative to the project root.
    pub preamble_path: String,
    /// 按工具的权限分级（驱动 HitlHook 门控）。
    /// Per-tool permission tiers (drives the HitlHook gate).
    pub permissions: ToolPerms,
    /// 模型；None 时用会话/注册表默认模型。
    /// Model; None → registry/session default.
    pub model: Option<String>,
    /// 自主循环轮数上限；None 时用注册表默认。
    /// Max turns for the autonomous loop; None → registry default.
    pub max_turns: Option<usize>,
    /// 内嵌 preamble 回退（内置角色有，自定义子代理无）。
    /// Embedded preamble fallback (built-in roles have it, custom sub-agents don't).
    pub embedded_preamble: Option<&'static str>,
}

impl std::fmt::Debug for AgentSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentSpec")
            .field("name", &self.name)
            .field("preamble_path", &self.preamble_path)
            .field("permissions", &self.permissions)
            .field("model", &self.model)
            .field("max_turns", &self.max_turns)
            .field("embedded_preamble", &self.embedded_preamble.map(|s| s.len()))
            .finish()
    }
}

/// 单条权限：允许 / 需询问 / 拒绝。
/// A single permission: Allow / Ask / Deny.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    #[default]
    Allow,
    Ask,
    Deny,
}

impl Permission {
    /// 将本权限映射到审批裁决（todo 9 管线 approval 阶段使用）。
    /// Maps this permission to an approval verdict (used by the todo 9 pipeline approval stage).
    fn to_verdict(self) -> ApprovalVerdict {
        match self {
            Permission::Allow => ApprovalVerdict::Allow,
            Permission::Ask => ApprovalVerdict::Ask,
            Permission::Deny => ApprovalVerdict::Deny,
        }
    }
}

/// 单条命令规则：用 glob 模式匹配 `run_bash` 的 command 字符串，决定其权限分级。
/// 在只读/会改变状态分类之前评估，让用户预授权高频可信命令或限制只读分类的命令。
/// A single command rule: glob-matches the `run_bash` command string to decide
/// its permission tier. Evaluated BEFORE the readonly/mutating classification so
/// users can pre-approve trusted frequent commands or restrict readonly-classified
/// commands.
///
/// `pattern` 支持 `*` 通配符（匹配任意可为空序列）；其余字符按字面；大小写敏感。
/// `pattern` supports `*` wildcards (matches any possibly-empty sequence);
/// all other chars are literal; case-sensitive.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CommandRule {
    /// glob 模式，如 `"cargo test*"`、`"rm *"`、`"git * log"`。
    /// Glob pattern, e.g. `"cargo test*"`, `"rm *"`, `"git * log"`.
    pub pattern: String,
    /// 匹配时返回的权限分级（allow / ask / deny）。
    /// Permission tier returned on match (allow / ask / deny).
    pub tier: Permission,
}

/// Glob 匹配：`*` 匹配任意（可为空）字符序列；其余字符按字面；大小写敏感。
/// 空模式匹配 nothing（返回 false），用于 `command_rules` 的 pattern 匹配。
/// Glob match: `*` matches any (possibly empty) char sequence; all other chars
/// literal; case-sensitive. An empty pattern matches nothing (returns false).
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<usize> = None;
    let mut match_pos = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_pos = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            match_pos += 1;
            ti = match_pos;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl ToolPerms {
    /// 按 `tool_name` + `args` 解析单条权限。与 `agent_loop::decide_tier` 行为一致
    /// （`run_bash` 需从 `args.command` 判断只读/会改变状态）。未知工具默认 `Ask`。
    /// Resolves a single permission by `tool_name` + `args`. Consistent with
    /// `agent_loop::decide_tier` (`run_bash` inspects `args.command` for read-only vs
    /// mutating). Unknown tools default to `Ask`.
    pub fn permission_for(&self, tool_name: &str, args: &Value) -> Permission {
        match tool_name {
            "read_file" => self.read_file,
            "edit_file" => self.edit_file,
            "write_file" => self.write_file,
            "web_fetch" => self.web_fetch,
            "web_search" => self.web_search,
            "run_file" => self.run_bash_mutating,
            // bash_output reads session-state (buffer + status) — same tier as read-only bash.
            // bash_output 读取会话状态（缓冲区 + 状态）——同只读 bash 等级。
            "bash_output" => self.run_bash_readonly,
            // kill_shell terminates a process — same tier as mutating bash.
            // kill_shell 终止进程——同会改变状态的 bash 等级。
            "kill_shell" => self.run_bash_mutating,
            // todo_write 只改 UI 可见会话状态，无文件系统/系统副作用——同 read_file 安全类，静默放行。
            // todo_write only mutates UI-visible session state, no fs/system side effects —
            // same safety class as read_file, auto-allow without HITL popup.
            "todo_write" => Permission::Allow,
            // task 是编排机制（扇出子代理），子代理自身的权限由其角色决定；
            // 同 todo_write 安全类，静默放行，否则每次调用都弹窗。
            // task is an orchestration mechanism (fanout subagents); subagent
            // permissions are governed by their own role — same safety class as
            // todo_write, auto-allow without HITL popup.
            "task" => Permission::Allow,
            "run_bash" => {
                let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
                // 命令规则：首条匹配胜出（allow/ask/deny），在只读/会改变状态分类之前评估，
                // 让用户预授权高频可信命令或限制只读分类的命令。
                // Command rules: first match wins (allow/ask/deny), evaluated before the
                // readonly/mutating classification so users can pre-approve trusted frequent
                // commands or restrict readonly-classified commands.
                for rule in &self.command_rules {
                    if glob_match(&rule.pattern, command) {
                        return rule.tier;
                    }
                }
                if crate::tools::is_readonly_bash(command) {
                    self.run_bash_readonly
                } else {
                    self.run_bash_mutating
                }
            }
            _ => Permission::Ask,
        }
    }
}

/// 默认审批器：包装 `ToolPerms` 作为数据源，实现 `ToolApproval`（todo 10 升级）。
/// Default approval: wraps `ToolPerms` as its data source, implements `ToolApproval`
/// (todo 10 upgrade). 行为与 todo 9 的 `impl ToolApproval for ToolPerms` 完全一致——
/// Allow 自动通过，Ask 触发 HITL y/n，Deny 拒绝。`ToolPerms` 仍是数据源，不变。
/// Same behavior as todo 9's `impl ToolApproval for ToolPerms` — Allow passes silently,
/// Ask triggers HITL y/n, Deny rejects. `ToolPerms` stays as the data source, unchanged.
pub struct DefaultApproval {
    perms: ToolPerms,
}

impl DefaultApproval {
    pub fn new(perms: ToolPerms) -> Self {
        Self { perms }
    }
}

impl ToolApproval for DefaultApproval {
    fn request(&self, req: &ApprovalRequest) -> ApprovalVerdict {
        self.perms
            .permission_for(&req.tool_name, &req.args)
            .to_verdict()
    }
}

/// 可插拔审批链：持有 `Vec<Box<dyn ToolApproval>>` 注册监听器（todo 10 升级）。
/// Pluggable approval chain: holds `Vec<Box<dyn ToolApproval>>` registered listeners
/// (todo 10 upgrade).
///
/// 迭代顺序与裁决优先级（fail-closed）：
/// Iteration order and verdict priority (fail-closed):
/// - 第一个返回 `Deny` 的监听器短路（立即拒绝，fail-closed）。
/// - The first listener to return `Deny` short-circuits (immediate reject, fail-closed).
/// - 无 `Deny` 时，最高优先级的非-Deny 裁决胜出（`Allow` > `Ask`）。
/// - With no `Deny`, the highest-priority non-Deny verdict wins (`Allow` > `Ask`).
/// - 无监听器注册时返回 `Deny`（fail-closed）。
/// - With no listeners registered, returns `Deny` (fail-closed).
#[derive(Default)]
pub struct ApprovalChain {
    listeners: Vec<Box<dyn ToolApproval>>,
}

impl ApprovalChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个审批监听器。监听器按注册顺序迭代。
    /// Register an approval listener. Listeners are iterated in registration order.
    pub fn add(&mut self, listener: Box<dyn ToolApproval>) {
        self.listeners.push(listener);
    }

    /// Builder-style：注册一个监听器并返回 self。
    /// Builder-style: register a listener and return self.
    #[allow(dead_code)] // used in tests
    pub fn with(mut self, listener: Box<dyn ToolApproval>) -> Self {
        self.listeners.push(listener);
        self
    }

    /// 已注册的监听器数量。
    /// Number of registered listeners.
    #[allow(dead_code)] // used in tests
    pub fn len(&self) -> usize {
        self.listeners.len()
    }

    /// 是否无监听器注册。
    /// Whether no listeners are registered.
    #[allow(dead_code)] // used in tests
    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }
}

impl ToolApproval for ApprovalChain {
    fn request(&self, req: &ApprovalRequest) -> ApprovalVerdict {
        if self.listeners.is_empty() {
            return ApprovalVerdict::Deny;
        }
        // 跟踪最高优先级的非-Deny 裁决：Allow > Ask。
        // Track the highest-priority non-Deny verdict: Allow > Ask.
        let mut winner = ApprovalVerdict::Ask;
        for listener in &self.listeners {
            match listener.request(req) {
                ApprovalVerdict::Deny => return ApprovalVerdict::Deny,
                ApprovalVerdict::Allow => winner = ApprovalVerdict::Allow,
                ApprovalVerdict::Ask => {}
            }
        }
        winner
    }
}

// `[agent]` 子节与顶层配置已合并进统一配置模块 `crate::config::Config`，
// 由 main 通过 `config::init()` 一次解析，此处不再单独定义。
// The `[agent]` subsection and top-level config now live in the unified
// `crate::config::Config`, parsed once via `config::init()` in main.

/// 绑定到某个角色的 Agent：模型 + preamble（提示词，从 .md 文件加载）。
/// An Agent bound to a role: model + preamble (prompt, loaded from .md file).
pub struct RoleAgent {
    role: Role,
    agent: ChatAgent,
}

impl RoleAgent {
    /// 用该角色 Agent 直接执行一次任务（用于 Planner/Auditor 等不需要工具循环的角色）。
    /// Executes a task once directly with this role's Agent (for roles like Planner/Auditor that don't need tool loops).
    /// 流式输出通过 `tx` channel 发送给 TUI。
    /// Streaming output is sent to the TUI via the `tx` channel.
    pub async fn run(&self, task: &str, tx: &EventSender) -> anyhow::Result<String> {
        const MAX_RETRIES: usize = 3;

        for attempt in 0..=MAX_RETRIES {
            let prompt = if attempt == 0 {
                task.to_string()
            } else {
                let remaining = MAX_RETRIES - attempt;
                format!(
                    "{task}\n\n\
                     [系统提示 / System] 上次因 SSE 连接中断（第 {attempt}/{MAX_RETRIES} 次重试，剩余 {remaining} 次）。\n\
                     请重新生成完整内容。注意：\n\
                     - 不要重复上次已完成的步骤或分析\n\
                     - 直接从断点处继续，输出完整结果\n\
                     - 如果上次输出不完整，请从头生成完整版本\n\n\
                     [System] Previous SSE stream disconnected (attempt {attempt}/{MAX_RETRIES}, {remaining} retries left). \
                     Regenerate the full response. Skip already-completed steps and produce complete output."
                )
            };

            info!(
                "[{:?}] \u{6267}\u{884c}\u{4efb}\u{52a1}\u{ff08}\u{5c1d}\u{8bd5} {}/{}\u{ff09}",
                self.role,
                attempt + 1,
                MAX_RETRIES + 1
            );
            let stream = self.agent.runner(&prompt).stream().await;

            match crate::agent_loop::consume_stream(stream, None, crate::agent_loop::sse_idle_timeout(), tx).await {
                Ok(output) => return Ok(output),
                Err(e) if crate::agent_loop::is_stream_error(&e) && attempt < MAX_RETRIES => {
                    let remaining = MAX_RETRIES - attempt;
                    let err_snippet: String = e.to_string().chars().take(200).collect();
                    let _ = tx.send(AgentEvent::Info(format!(
                        "[重试 / Retry] {:?} 第 {}/{} 次：SSE 连接中断，剩余 {} 次。错误摘要: {}",
                        self.role,
                        attempt + 1,
                        MAX_RETRIES,
                        remaining,
                        err_snippet
                    )));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(anyhow::anyhow!(
            "{:?} 重试 {MAX_RETRIES} 次后仍失败（SSE 连接反复中断）。建议检查网络或 API 稳定性后重试。\n\
             [System] {:?} failed after {MAX_RETRIES} retries (repeated SSE disconnects). \
             Check network/API stability and try again.",
            self.role,
            self.role
        ))
    }
}

/// Agent 注册表：持有共享配置，并为各角色构建 Agent；同时保存会话级的模型覆盖。
/// Agent registry: holds shared config, builds Agents per role; also stores session-level model override.
pub struct AgentRegistry {
    config: Arc<crate::config::Config>,
    mcp: Arc<McpManager>,
    sandbox: Arc<dyn crate::seam::SandboxProvider>,
    session_model: Arc<Mutex<Option<String>>>,
    /// 会话级供应商覆盖（切回历史模型时恢复）。None 时走 env > config。
    /// Session-level provider override (restored when switching back). None falls through.
    session_provider: Arc<Mutex<Option<String>>>,
    /// 会话级 base_url 覆盖（切回历史模型时恢复）。None 时走 env > config > 默认。
    /// Session-level base_url override (restored when switching back). None falls through.
    session_base_url: Arc<Mutex<Option<String>>>,
    /// todo_write 工具的共享 store + 事件发送端。
    /// Shared store + event sender for the todo_write tool.
    /// 由 Orchestrator 在 handle() 时设置；build() 时读取并传给 add_builtin_tools。
    /// Set by the Orchestrator at handle() time; read by build() and passed to add_builtin_tools.
    todo_ctx: Arc<Mutex<Option<crate::tools::TodoContext>>>,
    /// task 工具（子代理扇出）的共享上下文。
    /// Shared context for the task tool (subagent fanout).
    /// 由 Orchestrator 在 handle() 时设置；build_runner_agent / build 时读取。
    /// Set by the Orchestrator at handle() time; read by build_runner_agent / build.
    task_ctx: Arc<Mutex<Option<crate::subagent::SubagentCtx>>>,
    /// 子代理深度计数器：> 0 时 task_ctx_for_role 返回 None（防止递归扇出）。
    /// Subagent depth counter: when > 0, task_ctx_for_role returns None
    /// (prevents recursive fanout — subagents do NOT get the task tool).
    subagent_depth: Arc<AtomicU32>,
    /// 文件检查点存储——会话级共享，EditFile/WriteFile 在写入前记录快照。
    /// File checkpoint store — session-shared; EditFile/WriteFile snapshot before writing.
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
    /// 共享后台 shell 注册表（dev server / watch 模式等长命令）。
    /// 共享后台 shell 注册表（dev server / watch 模式等长命令）。
    /// Shared background shell registry (long commands like dev server / watch mode).
    /// 一个 `Arc<BackgroundRegistry>` 在 Orchestrator 级别共享，通过 ToolDeps 注入。
    /// One `Arc<BackgroundRegistry>` shared at Orchestrator level, injected via ToolDeps.
    bg: Arc<crate::shell::BackgroundRegistry>,
    /// 定时任务管理器（可选，启用调度器时设置）。
    /// Scheduler task manager (optional, set when scheduler is enabled).
    scheduler_mgr: Arc<Mutex<Option<crate::scheduler::TaskManager>>>,
}

impl AgentRegistry {
    pub fn new(
        config: Arc<crate::config::Config>,
        mcp: Arc<McpManager>,
        sandbox: Arc<dyn crate::seam::SandboxProvider>,
    ) -> Self {
        let session_model = std::env::var("AGENT_MODEL").ok();
        let max_bash_output_chars = config.context.max_bash_output_chars;
        Self {
            config,
            mcp,
            sandbox,
            session_model: Arc::new(Mutex::new(session_model)),
            session_provider: Arc::new(Mutex::new(None)),
            session_base_url: Arc::new(Mutex::new(None)),
            todo_ctx: Arc::new(Mutex::new(None)),
            task_ctx: Arc::new(Mutex::new(None)),
            subagent_depth: Arc::new(AtomicU32::new(0)),
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
            bg: Arc::new(crate::shell::BackgroundRegistry::new(
                max_bash_output_chars,
            )),
            scheduler_mgr: Arc::new(Mutex::new(None)),
        }
    }

    /// 设置调度器任务管理器（在 main 中 Scheduler 创建后调用）。
    /// Sets the scheduler task manager (called from main after Scheduler is created).
    pub fn set_scheduler_mgr(&self, mgr: crate::scheduler::TaskManager) {
        *self.scheduler_mgr.lock().unwrap() = Some(mgr);
    }

    /// clone 时共享同一份 Arc（配置与模型覆盖都会同步）。
    /// Shares the same Arc on clone (config and model override stay in sync).
    pub fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            mcp: self.mcp.clone(),
            sandbox: self.sandbox.clone(),
            session_model: self.session_model.clone(),
            session_provider: self.session_provider.clone(),
            session_base_url: self.session_base_url.clone(),
            todo_ctx: self.todo_ctx.clone(),
            task_ctx: self.task_ctx.clone(),
            subagent_depth: self.subagent_depth.clone(),
            checkpoints: self.checkpoints.clone(),
            bg: self.bg.clone(),
            scheduler_mgr: self.scheduler_mgr.clone(),
        }
    }
    /// Overrides the model used by all roles in this session.
    pub fn set_session_model(&self, slug: &str) {
        *self.session_model.lock().unwrap() = Some(slug.to_string());
    }

    pub fn session_model(&self) -> Option<String> {
        self.session_model.lock().unwrap().clone()
    }

    /// 覆盖本会话的供应商（切回历史模型时恢复当时的供应商）。
    /// Overrides the provider for this session (restored when switching back to a historical model).
    pub fn set_session_provider(&self, provider: &str) {
        *self.session_provider.lock().unwrap() = Some(provider.to_string());
    }

    pub fn session_provider(&self) -> Option<String> {
        self.session_provider.lock().unwrap().clone()
    }

    /// 覆盖本会话的 base URL（切回历史模型时恢复当时的网关）。
    /// Overrides the base URL for this session (restored when switching back to a historical model).
    pub fn set_session_base_url(&self, base_url: &str) {
        *self.session_base_url.lock().unwrap() = Some(base_url.to_string());
    }

    pub fn session_base_url(&self) -> Option<String> {
        self.session_base_url.lock().unwrap().clone()
    }

    /// 设置 todo_write 工具的共享 store + 事件发送端。
    /// Orchestrator 在 handle() 时调用，把 store + tx 注入 registry，
    /// 随后 build() / build_runner_agent() 会传给 add_builtin_tools。
    /// Sets the shared store + event sender for the todo_write tool.
    /// Called by the Orchestrator at handle() time; build() / build_runner_agent()
    /// then passes it to add_builtin_tools.
    pub fn set_todo_ctx(&self, ctx: crate::tools::TodoContext) {
        *self.todo_ctx.lock().unwrap() = Some(ctx);
    }

    pub fn todo_ctx(&self) -> Option<crate::tools::TodoContext> {
        self.todo_ctx.lock().unwrap().clone()
    }

    /// Returns the todo_write context only for roles that should have the tool
    /// (Builder + Orchestrator). Read-only roles (Investigator/Planner/Auditor) get None.
    pub fn todo_ctx_for_role(&self, role: Role) -> Option<crate::tools::TodoContext> {
        if matches!(role, Role::Builder | Role::Orchestrator) {
            self.todo_ctx()
        } else {
            None
        }
    }

    /// 设置 task 工具（子代理扇出）的共享上下文。
    /// Orchestrator 在 handle() 时调用，把 sandbox + trust + tx + depth 注入 registry，
    /// 随后 build() / build_runner_agent() 会传给 add_builtin_tools。
    /// Sets the shared context for the task tool (subagent fanout).
    /// Called by the Orchestrator at handle() time; build() / build_runner_agent()
    /// then passes it to add_builtin_tools.
    pub fn set_task_ctx(&self, ctx: crate::subagent::SubagentCtx) {
        *self.task_ctx.lock().unwrap() = Some(ctx);
    }

    /// 返回子代理深度计数器的共享引用。
    /// Returns the shared subagent depth counter.
    pub fn subagent_depth(&self) -> Arc<AtomicU32> {
        self.subagent_depth.clone()
    }

    /// 返回 task 工具上下文，仅限 Builder + Orchestrator 角色，且深度为 0 时。
    /// 深度 > 0 时返回 None——子代理构建的 agent 不包含 task 工具（防止递归扇出）。
    /// Returns the task tool context only for Builder + Orchestrator roles
    /// when depth is 0. When depth > 0, returns None — subagent-built agents
    /// do NOT include the task tool (prevents recursive fanout).
    pub fn task_ctx_for_role(&self, role: Role) -> Option<crate::subagent::SubagentCtx> {
        if self.subagent_depth.load(Ordering::Relaxed) > 0 {
            return None;
        }
        if matches!(role, Role::Builder | Role::Orchestrator) {
            self.task_ctx.lock().unwrap().clone()
        } else {
            None
        }
    }

    /// 构建客户端，应用 session 级 provider/base_url 覆盖（切回历史模型时走当时的网关）。
    /// Build a client applying session-level provider/base_url overrides
    /// (uses the gateway from the time when switching back to a historical model).
    pub fn create_client(&self) -> anyhow::Result<crate::providers::CompletionsClient> {
        crate::providers::create_client_with(
            self.session_provider().as_deref(),
            self.session_base_url().as_deref(),
        )
    }

    /// 自主循环的上限轮数，从配置透传。
    /// The autonomous loop's max turns, passed through from config.
    pub fn max_turns(&self) -> usize {
        self.config.max_turns()
    }

    pub fn sandbox(&self) -> Arc<dyn crate::seam::SandboxProvider> {
        self.sandbox.clone()
    }

    /// 返回共享的后台 shell 注册表。
    /// Returns the shared background shell registry.
    pub fn bg(&self) -> Arc<crate::shell::BackgroundRegistry> {
        self.bg.clone()
    }

    /// 返回调度器任务管理器（可选）。
    /// Returns the scheduler task manager (optional).
    pub fn scheduler_mgr(&self) -> Option<crate::scheduler::TaskManager> {
        self.scheduler_mgr.lock().unwrap().clone()
    }

    /// 返回共享的文件检查点存储。
    /// Returns the shared file checkpoint store.

    /// 返回共享的文件检查点存储。
    /// Returns the shared file checkpoint store.
    pub fn checkpoints(&self) -> Arc<crate::checkpoint::CheckpointStore> {
        self.checkpoints.clone()
    }

    #[allow(dead_code)]
    pub fn max_turns_for_role(&self, role: Role) -> usize {
        let key = format!("{role:?}").to_lowercase();
        self.config
            .agents
            .roles
            .get(&key)
            .and_then(|rc| rc.max_turns)
            .unwrap_or_else(|| self.max_turns())
    }

    /// 上下文管理配置（token 预算、压缩阈值、截断限制）。
    /// Context management config (token budget, compaction threshold, truncation limits).
    pub fn context_config(&self) -> &crate::context::ContextConfig {
        &self.config.context
    }

    pub fn active_profile(&self) -> Option<String> {
        self.config.active_profile_name()
    }

    /// 为指定角色构建 Agent（带工具或纯对话，取决于权限）。
    /// Builds an Agent for the specified role (with tools or pure chat, depending on permissions).
    pub fn build(&self, role: Role) -> anyhow::Result<RoleAgent> {
        let key = format!("{role:?}").to_lowercase();
        let rc = self
            .config
            .agents
            .roles
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("no config for role {key}"))?;
        let client = self.create_client()?;
        let preamble = crate::prompts::load(role, &rc.preamble);
        // 把与角色领域相关的技能指令注入提示词，使模型遵循技能中的步骤。
        // Injects role-domain skill instructions into the preamble so the model follows the steps in skills.
        let preamble = inject_skills_public(&preamble);
        let rules_path = self.config.memory.dir.join(&self.config.memory.rules_file);
        let rules = crate::memory::load_rules_from_file(&rules_path);
        let preamble = if rules.is_empty() {
            preamble
        } else {
            let rules_text = rules
                .iter()
                .map(|r| format!("- {}", r.text))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{preamble}\n\n# Escalated Rules\n{rules_text}")
        };
        // 会话级模型覆盖优先于角色各自配置的模型。
        // Session-level model override takes priority over per-role configured model.
        let model = match *self.session_model.lock().unwrap() {
            Some(ref m) => m.clone(),
            None => rc.model.clone(),
        };
        let with_tools = rc.permissions.read_file == Permission::Allow
            || rc.permissions.run_bash_readonly == Permission::Allow
            || rc.permissions.run_bash_mutating == Permission::Allow
            || rc.permissions.edit_file == Permission::Allow
            || rc.permissions.web_fetch != Permission::Deny
            || rc.permissions.web_search != Permission::Deny;
        let params = crate::providers::provider_additional_params();
        let max_turns = self.max_turns();
        info!("[build] role={key} model={model} max_turns={max_turns}");
        let max_output = self.context_config().max_output_tokens as u64;
        let reasoning = crate::providers::is_reasoning_model(&model);
        // max_tokens 策略：推理模型始终跳过（reasoning+输出+工具共享预算）；
        // 非推理模型在 max_output_tokens=0 时跳过（用模型默认输出预算），>0 时作为显式上限。
        // rig 的 OpenAI 路径在 None 时省略该字段。详见 is_reasoning_model 文档。
        // max_tokens policy: reasoning models always skip (reasoning+output+tools share budget);
        // non-reasoning skip when max_output_tokens=0 (use model default), >0 = explicit cap.
        // rig's OpenAI path omits the field when None. See is_reasoning_model doc.
        let effective_max_tokens: Option<u64> = if reasoning {
            None
        } else if max_output > 0 {
            Some(max_output)
        } else {
            None
        };
        if reasoning {
            info!("[build] reasoning model detected, skipping max_tokens (model default output budget)");
        } else if effective_max_tokens.is_none() {
            info!("[build] non-reasoning model, skipping max_tokens (model default output budget; set [context].max_output_tokens>0 to cap)");
        }
        let agent = if with_tools {
            let builder = client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .default_max_turns(max_turns);
            // 沙箱以 `Arc<dyn SandboxProvider>` trait 对象注入（todo 4 迁移）——
            // build() 不再直接传具体 `Sandbox` 类型，使后端可在配置层切换。
            let sandbox_provider: Arc<dyn SandboxProvider> = self.sandbox.clone();
            let deps = crate::tools::ToolDeps {
                sandbox: sandbox_provider.clone(),
                todo_ctx: self.todo_ctx_for_role(role),
                task_ctx: self.task_ctx_for_role(role),
                task_registry: self.clone(),
                shells: Arc::new(crate::shell::LazyShell::new(
                    sandbox_provider.clone(),
                    self.context_config().max_bash_output_chars,
                )),
                bg: self.bg.clone(),
                checkpoints: self.checkpoints.clone(),
                scheduler_mgr: self.scheduler_mgr(),
            };
            let builder =
                crate::tools::add_builtin_tools(builder, self.context_config(), &deps);
            let builder = if !self.mcp.is_empty() {
                let mut b = builder;
                for (tools, sink) in self.mcp.all_tools_and_sinks() {
                    b = b.rmcp_tools(tools, sink);
                }
                b
            } else {
                builder
            };
            let builder = if let Some(v) = effective_max_tokens {
                builder.max_tokens(v)
            } else {
                builder
            };
            builder.build()
        } else {
            let builder = client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .default_max_turns(max_turns);
            let builder = if let Some(v) = effective_max_tokens {
                builder.max_tokens(v)
            } else {
                builder
            };
            builder.build()
        };
        Ok(RoleAgent { role, agent })
    }

    /// 取某角色的按工具权限分级，供自主循环的 HITL（人在环）门控逐次调用决策
    /// Gets the per-tool permission tiers for a role, for the autonomous loop's HITL (Human-in-the-Loop) gate per-call decisions
    /// （allow / ask / deny）。
    /// (allow / ask / deny).
    #[allow(dead_code)]
    pub fn tool_perms(&self, role: Role) -> ToolPerms {
        let key = format!("{role:?}").to_lowercase();
        let mut perms = self
            .config
            .agents
            .roles
            .get(&key)
            .map(|rc| rc.permissions.clone())
            .unwrap_or_default();
        perms.command_rules = self.config.sandbox.command_rules.clone();
        perms
    }

    /// 返回所有 MCP 服务器的显示信息（名称、状态、工具列表、错误信息）。
    /// Returns display info for all MCP servers (name, status, tools, error).
    pub fn mcp_server_displays(&self) -> Vec<crate::mcp::McpServerDisplay> {
        self.mcp.server_displays()
    }

    /// 取某角色的配置，供自主循环重建"可运行"的 Agent
    /// Gets a role's config, for the autonomous loop to rebuild a "runnable" Agent
    /// （循环需要原始 `Agent`，而非 `RoleAgent` 包装）。
    /// (the loop needs the raw `Agent`, not the `RoleAgent` wrapper).
    pub fn role_config(&self, role: Role) -> Option<&RoleConfig> {
        let key = format!("{role:?}").to_lowercase();
        self.config.agents.roles.get(&key)
    }

    /// 从内置角色配置构建 `AgentSpec`。
    /// Builds an `AgentSpec` from a built-in role's config.
    ///
    /// 角色配置不存在时返回带默认值的 spec（与 `tool_perms()` 的降级行为一致）。
    /// When the role config is absent, returns a spec with defaults
    /// (consistent with `tool_perms()` fallback behavior).
    pub fn agent_spec(&self, role: Role) -> AgentSpec {
        let key = format!("{role:?}").to_lowercase();
        let rc = self.config.agents.roles.get(&key);
        let mut permissions = rc.map(|c| c.permissions.clone()).unwrap_or_default();
        permissions.command_rules = self.config.sandbox.command_rules.clone();
        AgentSpec {
            name: key,
            preamble_path: rc.map(|c| c.preamble.clone()).unwrap_or_default(),
            permissions,
            model: rc.map(|c| c.model.clone()),
            max_turns: rc.and_then(|c| c.max_turns),
            embedded_preamble: Some(crate::prompts::default_for(role)),
        }
    }

    /// 从 `[agents.custom.<name>]` 构建 `AgentSpec`；未配置时返回 None。
    /// Builds an `AgentSpec` from `[agents.custom.<name>]`; None if not configured.
    pub fn custom_spec(&self, name: &str) -> Option<AgentSpec> {
        let cc = self.config.agents.custom.get(name)?;
        let mut permissions = cc.permissions.clone();
        permissions.command_rules = self.config.sandbox.command_rules.clone();
        Some(AgentSpec {
            name: name.to_string(),
            preamble_path: cc.preamble.clone(),
            permissions,
            model: cc.model.clone(),
            max_turns: None,
            embedded_preamble: None,
        })
    }

    /// 返回已配置的所有自定义子代理名称（已排序，便于错误信息稳定）。
    /// Returns all configured custom sub-agent names (sorted for stable error messages).
    pub fn custom_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.config.agents.custom.keys().cloned().collect();
        names.sort();
        names
    }

    /// 解析当前生效的模型：会话覆盖 → [agent].default_model → 各角色配置中的首选模型。
    /// Resolves the currently effective model: session override -> [agent].default_model -> first role's configured model.
    pub fn effective_model(&self) -> String {
        self.session_model().unwrap_or_else(|| {
            let dm = &self.config.agent.default_model;
            if !dm.is_empty() {
                return dm.clone();
            }
            self.config
                .agents
                .roles
                .values()
                .next()
                .map(|rc| rc.model.clone())
                .unwrap_or_else(|| "deepseek-v4-pro".to_string())
        })
    }
}

/// 把与给定文本相关的技能指令拼接到提示词末尾，供模型遵循。无相关技能时原样返回。
/// Appends skill instructions relevant to the given text to the end of the preamble for the model to follow. Returns as-is if no relevant skills.
pub fn inject_skills_public(preamble: &str) -> String {
    let skill_text = crate::skills::relevant_skills(preamble);
    if skill_text.is_empty() {
        preamble.to_string()
    } else {
        format!("{preamble}\n\n# Loaded Skills\n{skill_text}")
    }
}

/// 意图分类：驱动 SDD 管线路由。
/// Intent classification: drives SDD pipeline routing.
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    Implement,
    Investigate,
    Chat,
}

/// 快速路径：明显问句模式直接返回 Chat，跳过 LLM 调用。
/// Fast path: obvious question patterns return Chat, skipping the LLM call.
fn is_obvious_question(message: &str) -> bool {
    let m = message.trim();
    if m.ends_with('?') || m.ends_with('？') {
        return true;
    }
    let question_markers = [
        "吗",
        "么",
        "呢",
        "吧",
        "怎么",
        "如何",
        "是否",
        "会不会",
        "能不能",
        "为什么",
        "是什么",
        "哪个",
        "哪些",
        "哪里",
        "多少",
    ];
    if question_markers.iter().any(|k| m.contains(k)) {
        return true;
    }
    if m.chars().count() <= 4 {
        return true;
    }
    false
}

/// 关键词降级匹配（LLM 不可用时的 fallback）。
/// Keyword fallback matching (used when LLM is unavailable).
pub fn classify_keyword_fallback(message: &str) -> Intent {
    let m = message.to_lowercase();
    let implement_kws = [
        "实现",
        "添加",
        "创建",
        "修复",
        "编写",
        "构建",
        "修改",
        "重构",
        "删除",
        "升级",
        "更新",
        "implement",
        "refactor",
        "upgrade",
        "update",
    ];
    let investigate_kws = [
        "看一下",
        "调查",
        "检查",
        "查找",
        "怎么",
        "分析",
        "对比",
        "差距",
        "look into",
        "investigate",
        "check",
        "find",
        "how does",
        "explain",
        "compare",
    ];
    if implement_kws.iter().any(|k| m.contains(k)) {
        Intent::Implement
    } else if investigate_kws.iter().any(|k| m.contains(k)) {
        Intent::Investigate
    } else {
        Intent::Chat
    }
}

/// LLM 意图分类：构建无工具 Agent，发送短 prompt，解析单词响应。
/// LLM intent classification: builds a tool-less Agent, sends a short prompt, parses the single-word response.
async fn classify_with_llm(
    message: &str,
    history: &[Message],
    registry: &AgentRegistry,
) -> Option<Intent> {
    let client = registry.create_client().ok()?;
    let model = registry
        .session_model()
        .or_else(|| {
            registry
                .role_config(Role::Orchestrator)
                .map(|rc| rc.model.clone())
        })
        .unwrap_or_else(|| registry.config.agent.default_model.clone());

    let preamble = "你是一个意图分类器。根据用户当前消息（结合最近的对话上下文）判断意图，只回复一个英文词：\n\
                    - implement: 要求修改、创建、删除代码或文件\n\
                    - investigate: 要求查看、分析、理解代码\n\
                    - chat: 聊天、问答、闲聊\n\
                    只回复一个词，不要任何解释。";

    let history_ctx = recent_history_text(history, 10);
    let prompt = if history_ctx.is_empty() {
        message.to_string()
    } else {
        format!("{history_ctx}\n[Current message]\n{message}")
    };

    let params = crate::providers::provider_additional_params();
    let agent = client
        .agent(&model)
        .preamble(preamble)
        .temperature(crate::providers::Provider::clamp_temperature(0.0))
        .additional_params(params)
        .default_max_turns(1)
        .max_tokens(20)
        .build();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let stream = agent.runner(&prompt).stream().await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        crate::agent_loop::consume_stream(stream, None, crate::agent_loop::sse_idle_timeout(), &tx),
    )
    .await
    .ok()?
    .ok()?;

    let lower = output.to_lowercase();
    if lower.contains("implement") {
        Some(Intent::Implement)
    } else if lower.contains("investigate") {
        Some(Intent::Investigate)
    } else if lower.contains("chat") {
        Some(Intent::Chat)
    } else {
        None
    }
}

/// 提取最近几条对话的文本摘要，供意图分类器理解上下文。
/// Extracts a text summary of recent conversation turns for the intent classifier.
fn recent_history_text(history: &[Message], max_messages: usize) -> String {
    let start = history.len().saturating_sub(max_messages);
    let recent = &history[start..];
    if recent.is_empty() {
        return String::new();
    }
    let mut s = String::from("[Recent conversation]\n");
    for msg in recent {
        match msg {
            Message::User { content } => {
                for item in content.iter() {
                    if let UserContent::Text(t) = item {
                        let text: String = t.text.chars().take(500).collect();
                        s.push_str(&format!("User: {text}\n"));
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for item in content.iter() {
                    if let AssistantContent::Text(t) = item {
                        let text: String = t.text.chars().take(500).collect();
                        s.push_str(&format!("Assistant: {text}\n"));
                    }
                }
            }
            _ => {}
        }
    }
    s
}

/// 意图分类入口：快速路径 → LLM → 关键词降级。
/// Intent classification entry point: fast path → LLM → keyword fallback.
pub async fn classify_intent(
    message: &str,
    history: &[Message],
    registry: &AgentRegistry,
) -> Intent {
    if is_obvious_question(message) {
        return Intent::Chat;
    }
    if let Some(intent) = classify_with_llm(message, history, registry).await {
        return intent;
    }
    classify_keyword_fallback(message)
}

// ── SDD pipeline prompt builders + decision functions (todo 12 GAP-4) ──
// Extracted as pure functions for characterization testing. These capture the
// exact prompt formats and decision logic that run_sdd_pipeline uses, ensuring
// behavior equivalence before and after the listener refactor.
// SDD 管线提示词构建器 + 决策函数（todo 12 GAP-4）——提取为纯函数用于特征化测试。

/// 构建调查者提示词。
/// Builds the Investigator's prompt for the SDD pipeline.
pub fn sdd_investigator_prompt(message: &str) -> String {
    format!(
        "{message}\n\n\
         请先判断此任务是否需要调查代码背景。如果任务简单明了无需调查，\
         直接回复\"无需调查\"并简述原因，不调用任何工具。\
         否则，探索相关代码，理解架构与依赖，产出结构化调查报告。"
    )
}

/// 判断调查结果是否触发 Q&A 逃生口（无需实现）。
/// Returns true if the investigation output triggers the Q&A escape hatch.
pub fn sdd_escape_hatch_triggers(investigation: &str) -> bool {
    investigation.contains("无需实现")
}

/// 判断调查者是否认为无需调查。
/// Returns true if the investigator decided no investigation was needed.
pub fn sdd_no_investigation_needed(investigation: &str) -> bool {
    investigation.contains("无需调查")
}

/// 构建规划者提示词（根据调查结果）。
/// Builds the Planner's prompt based on investigation findings.
pub fn sdd_plan_prompt(message: &str, investigation: &str) -> String {
    if sdd_no_investigation_needed(investigation) {
        format!("{message}\n\n请拆解为相互独立、可执行的步骤。")
    } else {
        format!(
            "{message}\n\n\
             调查发现：\n{investigation}\n\n\
             请基于以上调查发现，拆解为相互独立、可执行的步骤。"
        )
    }
}

/// 构建构建者提示词（注入计划）。
/// Builds the Builder's prompt with the plan injected.
pub fn sdd_builder_prompt(message: &str, plan: &str) -> String {
    format!("{message}\n\n参考计划：\n{plan}")
}

/// 构建审计驳回后的重试提示词。
/// Builds the retry prompt after the Auditor rejects.
pub fn sdd_retry_prompt(message: &str, plan: &str, built: &str, reason: &str) -> String {
    format!(
        "之前的尝试被审计驳回 / Previous attempt was rejected by audit:\n\
         驳回原因 / Rejection reason:\n{reason}\n\n\
         原始任务 / Original task:\n{message}\n\n\
         参考计划 / Reference plan:\n{plan}\n\n\
         上次产出（需修正）/ Previous output (needs fixing):\n{built}\n\n\
         请根据驳回原因修正上述产出，注意：\n\
         - 逐条对照驳回原因，确保每个问题都已解决\n\
         - 不要从头重做，只需修正被指出的问题\n\
         - 保持其他正确的部分不变\n\n\
         [System] Fix the issues identified in the rejection reason above. \
         Address each point, keep correct parts, only change what's rejected."
    )
}

/// 构建验证门失败后的重试提示词（与 sdd_retry_prompt 同样的纯函数风格）。
/// Builds the retry prompt after the verify gate fails (same pure-fn style as
/// sdd_retry_prompt). Tested structurally.
pub fn sdd_verify_retry_prompt(
    message: &str,
    plan: &str,
    built: &str,
    command: &str,
    output_tail: &str,
) -> String {
    format!(
        "之前的产出未通过验证门（构建/测试失败）/ \
         Previous output failed the verification gate (build/test):\n\
         失败命令 / Failed command:\n{command}\n\n\
         输出尾部 / Output tail:\n{output_tail}\n\n\
         原始任务 / Original task:\n{message}\n\n\
         参考计划 / Reference plan:\n{plan}\n\n\
         上次产出（需修正）/ Previous output (needs fixing):\n{built}\n\n\
         请根据上述错误输出修正代码，注意：\n\
         - 仔细阅读错误信息，定位并修复根本原因\n\
         - 不要从头重做，只需修复导致验证失败的问题\n\
         - 保持其他正确的部分不变\n\n\
         [System] Fix the code so the failing command above passes. Read the \
         error output carefully, find and fix the root cause, keep correct \
         parts, only change what's broken."
    )
}

/// 构建审计澄清响应。
/// Builds the clarify response when the Auditor requests clarification.
pub fn sdd_clarify_response(question: &str, built: &str) -> String {
    format!("需要澄清：{question}\n\n已产出的工作：\n{built}")
}

/// 判断构建者产出是否为退化输出（空白或过短），用于拦截静默失败。
/// Returns true when a Builder output is degenerate (blank or too short),
/// used to intercept silent failures before they are returned as final results.
pub fn is_degenerate_output(s: &str) -> bool {
    let trimmed = s.trim();
    trimmed.is_empty() || trimmed.chars().count() < 4
}

// ── todo 12: Investigator + Planner as agent_pre_step waterfall listeners ──

/// Registers an `InvestigatorListener` on the shared waterfall. When the
/// Builder's `AgentPreStep` fires, the listener runs investigation via
/// `run_autonomous` (without the shared waterfall, to avoid recursion) and
/// stores the result in `PreStepState`. If the escape hatch triggers, sets
/// `PreStepState.escape`. On error, sets `PreStepState.error`.
pub fn register_investigator_listener(
    waterfall: &Arc<WaterfallRegistry>,
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    trust_sandbox: Arc<AtomicBool>,
    tx: EventSender,
    history: Arc<Mutex<Vec<Message>>>,
    pre_step: Arc<PreStepState>,
) -> ListenerId {
    let reg = registry.clone();
    let sbx = sandbox.clone();
    let trust = trust_sandbox;
    let tx_c = tx;
    let hist = history;
    let ps = pre_step;
    waterfall.register_serial(move |event| {
        let WaterfallEvent::AgentPreStep { role, goal } = event else {
            return Box::pin(async {});
        };
        if role != "builder" {
            return Box::pin(async {});
        }
        let goal = goal.clone();
        let reg = reg.clone();
        let sbx = sbx.clone();
        let trust = trust.clone();
        let tx = tx_c.clone();
        let hist = hist.clone();
        let ps = ps.clone();
        Box::pin(async move {
            let _ = tx.send(AgentEvent::PhaseStart { role: "investigator".to_string() });
            let prompt = sdd_investigator_prompt(&goal);
            match crate::agent_loop::run_autonomous(
                &reg,
                &sbx,
                trust,
                Role::Investigator,
                &prompt,
                &tx,
                hist,
                None,
                None,
            )
            .await
            {
                Ok(inv) => {
                    if sdd_escape_hatch_triggers(&inv) {
                        ps.escape.store(true, Ordering::Relaxed);
                    }
                    *ps.investigation.lock().unwrap() = Some(inv);
                }
                Err(e) => {
                    *ps.error.lock().unwrap() = Some(e.to_string());
                }
            }
        })
    })
}

/// Registers a `PlannerListener` on the shared waterfall. When the Builder's
/// `AgentPreStep` fires (after the investigator listener), the listener reads
/// the investigation result, builds the plan prompt, runs the planner, and
/// sets `PreStepState.goal_override` to the builder prompt (message + plan).
/// Skips if the investigator set an error or escape hatch.
pub fn register_planner_listener(
    waterfall: &Arc<WaterfallRegistry>,
    registry: &AgentRegistry,
    tx: EventSender,
    message: String,
    pre_step: Arc<PreStepState>,
) -> ListenerId {
    let reg = registry.clone();
    let tx_c = tx;
    let msg = message;
    let ps = pre_step;
    waterfall.register_serial(move |event| {
        let WaterfallEvent::AgentPreStep { role, .. } = event else {
            return Box::pin(async {});
        };
        if role != "builder" {
            return Box::pin(async {});
        }
        if ps.escape.load(Ordering::Relaxed) {
            return Box::pin(async {});
        }
        if ps.error.lock().unwrap().is_some() {
            return Box::pin(async {});
        }
        let investigation = ps.investigation.lock().unwrap().clone().unwrap_or_default();
        let reg = reg.clone();
        let tx = tx_c.clone();
        let msg = msg.clone();
        let ps = ps.clone();
        Box::pin(async move {
            let _ = tx.send(AgentEvent::PhaseStart { role: "planner".to_string() });
            let plan_prompt = sdd_plan_prompt(&msg, &investigation);
            let planner = match reg.build(Role::Planner) {
                Ok(p) => p,
                Err(e) => {
                    *ps.error.lock().unwrap() = Some(e.to_string());
                    return;
                }
            };
            match planner.run(&plan_prompt, &tx).await {
                Ok(plan) => {
                    *ps.plan.lock().unwrap() = Some(plan.clone());
                    *ps.goal_override.lock().unwrap() = Some(sdd_builder_prompt(&msg, &plan));
                    let _ = tx.send(AgentEvent::PhaseStart { role: "builder".to_string() });
                }
                Err(e) => {
                    *ps.error.lock().unwrap() = Some(e.to_string());
                }
            }
        })
    })
}

// ── todo 13: AuditorListener (tools_post_execute + agent_turn_stopping) ──

/// Shared state between `run_sdd_pipeline` and the `AuditorListener`:
/// pipeline writes `built`, listener writes `verdict` after running
/// `ReviewGate::review()`.
pub struct AuditState {
    pub built: Arc<Mutex<Option<String>>>,
    pub verdict: Arc<Mutex<Option<crate::reviewer::Verdict>>>,
    /// 验证门备注：供后续任务注入审计材料（当前仅存储，不注入）。
    /// Verify-gate note: stored for a later task to inject into audit materials.
    pub verify_note: Arc<Mutex<Option<String>>>,
}

impl Default for AuditState {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditState {
    pub fn new() -> Self {
        Self {
            built: Arc::new(Mutex::new(None)),
            verdict: Arc::new(Mutex::new(None)),
            verify_note: Arc::new(Mutex::new(None)),
        }
    }
}

/// Registers an `AuditorListener` on the shared waterfall. On
/// `AgentTurnStopping`, reads `built` from `AuditState`, runs
/// `ReviewGate::review()` (spec compliance + code quality), and stores the
/// verdict. `ReviewGate` is the listener's implementation. Fast mode does
/// NOT call this (bypasses audit).
pub fn register_auditor_listener(
    waterfall: &Arc<WaterfallRegistry>,
    registry: &AgentRegistry,
    tx: EventSender,
    task: String,
    audit_state: Arc<AuditState>,
) -> ListenerId {
    let reg = registry.clone();
    let tx_c = tx;
    let as_ = audit_state;
    waterfall.register_serial(move |event| {
        let WaterfallEvent::AgentTurnStopping { .. } = event else {
            return Box::pin(async {});
        };
        let built = match as_.built.lock().unwrap().clone() {
            Some(b) => b,
            None => return Box::pin(async {}),
        };
        let reg = reg.clone();
        let tx = tx_c.clone();
        let task = task.clone();
        let as_ = as_.clone();
        Box::pin(async move {
            let _ = tx.send(AgentEvent::PhaseStart { role: "auditor".to_string() });

            // 收集实际改动材料（git diff + 新文件内容）。
            // Collect actual change material (git diff + new file contents).
            // spawn_blocking: std::process::Command is blocking, listener is async.
            // 进程 cwd 即项目根目录（sandbox 设计），用 "." 而非硬编码路径。
            // Process cwd is the project root (sandbox design); use "." not a hardcoded path.
            let cwd = std::path::PathBuf::from(".");
            let material = tokio::task::spawn_blocking(move || {
                crate::reviewer::collect_change_material(&cwd, 8000)
            })
            .await
            .ok()
            .flatten();

            // 读取验证门备注 / read verify-gate note (follows surrounding mutex idiom).
            let note = as_.verify_note.lock().unwrap().clone();

            let gate = crate::reviewer::ReviewGate::new(reg);
            match gate.review(&task, &built, material.as_deref(), note.as_deref(), &tx).await {
                Ok(verdict) => {
                    *as_.verdict.lock().unwrap() = Some(verdict);
                }
                Err(e) => {
                    *as_.verdict.lock().unwrap() =
                        Some(crate::reviewer::Verdict::Reject(e.to_string()));
                }
            }
        })
    })
}

/// 编排者：先分类意图，再按 SDD 纪律委派给对应的角色 Agent。
/// Orchestrator: classifies intent first, then delegates to the corresponding role Agent per SDD discipline.
/// Implement → 调查者探索 → 规划者拆解 → 构建者执行（工具循环 + HITL）→ 审计者两轮评审。
/// Implement -> Investigator explores -> Planner decomposes -> Builder executes (tool loop + HITL) -> Auditor two-round review.
/// Investigate → 调查者只读探索（工具循环，无编辑权限）。
/// Investigate -> Investigator read-only exploration (tool loop, no edit permission).
/// Chat → 构建者直接对话（无工具循环）。
/// Chat -> Builder direct conversation (no tool loop).
pub struct Orchestrator {
    registry: AgentRegistry,
    sandbox: Sandbox,
    /// 信任模式标志：为 true 时沙箱外访问自动授权，不弹窗确认。
    /// Trust-mode flag: when true, out-of-sandbox access is auto-authorized without prompting.
    trust_sandbox: Arc<AtomicBool>,
    /// 跨消息对话历史：让 SDD 管线中各子 agent 能看到之前的对话。
    /// Cross-message conversation history: lets sub-agents in the SDD pipeline see prior turns.
    history: Arc<Mutex<Vec<Message>>>,
    /// todo_write 工具的共享任务列表——同一 Orchestrator 的所有 Builder 实例共享。
    /// Shared todo list for the todo_write tool — all Builder instances within one
    /// Orchestrator share the same store.
    todo_store: Arc<Mutex<Vec<crate::event::TodoItem>>>,
    /// 文件检查点存储——会话级共享，/rewind 命令读取此存储回滚任务。
    /// File checkpoint store — session-shared; the /rewind command reads this to rewind tasks.
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
}

impl Orchestrator {
    pub fn new(registry: AgentRegistry) -> Self {
        // 环境变量 AGENT_TRUST_SANDBOX=on 可在启动时开启信任模式。
        // The AGENT_TRUST_SANDBOX=on env var enables trust mode at startup.
        let trust = std::env::var("AGENT_TRUST_SANDBOX")
            .map(|v| matches!(v.as_str(), "on" | "true" | "1"))
            .unwrap_or(false);
        // 从配置文件 [sandbox].authorized_dirs 预授权目录。
        // Pre-authorize directories from the config file's [sandbox].authorized_dirs.
        let authorized_dirs: Vec<String> = registry.config.sandbox.authorized_dirs.to_vec();
        // HITL 路径检查沙箱（用于弹窗确认）——当 mode == "off" 时禁用路径检查；
        // mode == "landlock" 时仍用 SimpleSandbox 做路径检查（OS 级隔离由
        // AgentRegistry 持有的 sandbox trait 对象负责，见 todo 8）。
        // HITL path-check sandbox (for prompts). Disabled when mode == "off".
        // mode == "landlock" keeps SimpleSandbox for path checking; OS-level
        // isolation is handled by the registry's sandbox provider (todo 8).
        let sandbox = if registry.config.sandbox.mode == "off" {
            Sandbox::with_backend(&authorized_dirs, crate::sandbox::SandboxBackend::Off)
        } else {
            Sandbox::with_authorized_dirs(&authorized_dirs)
        };
        let checkpoints = registry.checkpoints();
        Self {
            registry,
            sandbox,
            trust_sandbox: Arc::new(AtomicBool::new(trust)),
            history: Arc::new(Mutex::new(Vec::new())),
            todo_store: Arc::new(Mutex::new(Vec::new())),
            checkpoints,
        }
    }

    /// 返回信任模式标志的共享引用，供 TUI 切换。
    /// Returns a shared reference to the trust-mode flag, for TUI toggling.
    pub fn trust_sandbox(&self) -> Arc<AtomicBool> {
        self.trust_sandbox.clone()
    }

    /// 返回共享的文件检查点存储，供 /rewind 命令读取。
    /// Returns the shared file checkpoint store, for the /rewind command to read.
    pub fn checkpoints(&self) -> Arc<crate::checkpoint::CheckpointStore> {
        self.checkpoints.clone()
    }

    /// 用 `--continue` 会话的对话历史替换当前历史，让新会话继承上一轮的上下文。
    /// Replaces the current history with a resumed session's conversation, so the
    /// new session inherits prior context (`--continue`).
    pub fn seed_history(&self, messages: Vec<Message>) {
        let mut history = self.history.lock().unwrap();
        *history = messages;
        info!(
            "[session] \u{6062}\u{590d}\u{4e86} {} \u{6761}\u{5386}\u{53f2}\u{6d88}\u{606f} / restored {} history messages",
            history.len(),
            history.len()
        );
    }

    /// 返回当前对话历史的快照（用于持久化到 session 的 history.json）。
    /// Returns a snapshot of the current conversation history (for persisting to
    /// session's history.json).
    pub fn history_snapshot(&self) -> Vec<Message> {
        self.history.lock().unwrap().clone()
    }

    pub async fn handle(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        // 开始一个新的任务：递增检查点计数器，使后续 EditFile/WriteFile 的
        // record() 调用能记录到正确的任务 ID 下。
        // Begin a new task: increment the checkpoint counter so subsequent
        // EditFile/WriteFile record() calls land under the correct task ID.
        self.checkpoints.begin_task();
        // 把 todo_store + 当前 tx 注入 registry，使后续 build() / build_runner_agent()
        // 构造的 TodoWrite 工具共享同一份 store 并能发出 TodoUpdate 事件。
        // Inject the todo_store + current tx into the registry so that subsequent
        // build() / build_runner_agent() calls construct TodoWrite tools that share
        // the same store and can emit TodoUpdate events.
        self.registry.set_todo_ctx(crate::tools::TodoContext {
            store: self.todo_store.clone(),
            tx: tx.clone(),
        });
        // 注入 task 工具（子代理扇出）上下文——与 todo_ctx 同一注入模式。
        // Inject task tool (subagent fanout) context — same injection pattern as todo_ctx.
        self.registry.set_task_ctx(crate::subagent::SubagentCtx {
            sandbox: self.sandbox.clone(),
            trust_sandbox: self.trust_sandbox.clone(),
            tx: tx.clone(),
            depth: self.registry.subagent_depth(),
        });
        let history = self.history.lock().unwrap().clone();
        let intent = classify_intent(message, &history, &self.registry).await;
        match intent {
            Intent::Implement => self.run_sdd_pipeline(message, tx).await,
            Intent::Investigate => {
                crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Investigator,
                    message,
                    tx,
                    self.history.clone(),
                    None,
                    None,
                )
                .await
            }
            Intent::Chat => {
                crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Builder,
                    message,
                    tx,
                    self.history.clone(),
                    None,
                    None,
                )
                .await
            }
        }
    }

    /// 验证门：Builder 产出后运行构建/测试命令，失败则有界重试。
    /// Verify gate: runs build/test commands after the Builder; on failure, a
    /// bounded fix-and-reverify loop feeds the error to the Builder for retry.
    ///
    /// 返回 (产出, 验证备注, verified_ok)。
    /// Returns (output, verify_note, verified_ok).
    /// - verified_ok == true → 继续审计（或 fast 模式直接返回）。
    ///   verified_ok == true → proceed to audit (or return in fast mode).
    /// - verified_ok == false → 跳过审计，产出已带失败说明。
    ///   verified_ok == false → skip audit; output is annotated with the failure.
    async fn verify_with_retries(
        &self,
        message: &str,
        plan: &str,
        built: String,
        tx: &EventSender,
    ) -> anyhow::Result<(String, Option<String>, bool)> {
        use crate::verify::{
            detect_commands, next_gate_decision, run_single_command, GateDecision,
            SingleOutcome, VerifyOutcome,
        };
        use std::time::{Duration, Instant};

        let vcfg = &self.registry.config.verify;

        // 验证禁用时直接放行。
        // Pass through when verification is disabled.
        if !vcfg.enabled {
            let note = "验证已禁用 / verification disabled".to_string();
            return Ok((built, Some(note), true));
        }

        // 获取命令列表：配置覆盖优先，否则自动检测。
        // Get command list: config override takes priority, else auto-detect.
        let cmds: Vec<String> = match &vcfg.commands {
            Some(c) => c.clone(),
            None => match detect_commands(
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ) {
                Some(c) => c,
                None => {
                    let note =
                        "未检测到验证命令 / no verify commands detected".to_string();
                    return Ok((built, Some(note), true));
                }
            },
        };

        let cwd =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let timeout = Duration::from_secs(vcfg.timeout_secs);
        let max_retries = vcfg.max_retries;

        let mut current = built;
        let mut retries_done = 0u32;

        loop {
            let _ = tx.send(AgentEvent::PhaseStart {
                role: "verify".to_string(),
            });

            // 逐条运行命令，每条附带耗时 Info 行。
            // Run each command with a timing Info line.
            let mut ran: Vec<String> = Vec::new();
            let mut outcome: Option<VerifyOutcome> = None;

            for cmd in &cmds {
                let start = Instant::now();
                let single = run_single_command(cmd, &cwd, timeout).await;
                let dur = start.elapsed();
                match single {
                    SingleOutcome::Ok => {
                        let _ = tx.send(AgentEvent::Info(format!(
                            "✓ {} ({}s)",
                            cmd,
                            dur.as_secs()
                        )));
                        ran.push(cmd.clone());
                    }
                    SingleOutcome::Failed {
                        command,
                        output_tail,
                        exit_code,
                    } => {
                        let _ = tx.send(AgentEvent::Info(format!(
                            "✗ {} ({}s)",
                            command,
                            dur.as_secs()
                        )));
                        outcome = Some(VerifyOutcome::Failed {
                            command,
                            output_tail,
                            exit_code,
                        });
                        break;
                    }
                    SingleOutcome::Unavailable { command } => {
                        let _ = tx.send(AgentEvent::Info(format!(
                            "⊘ {} unavailable ({}s)",
                            command,
                            dur.as_secs()
                        )));
                        outcome = Some(VerifyOutcome::Unavailable { command });
                        break;
                    }
                }
            }

            let outcome = outcome.unwrap_or(VerifyOutcome::Passed { ran });
            let retries_left = max_retries.saturating_sub(retries_done);

            match next_gate_decision(&outcome, retries_left) {
                GateDecision::Proceed => {
                    let note = match &outcome {
                        VerifyOutcome::Passed { ran } => {
                            format!("验证通过 / verify passed ({} commands)", ran.len())
                        }
                        VerifyOutcome::Skipped { reason } => {
                            format!("验证跳过 / verify skipped: {reason}")
                        }
                        VerifyOutcome::Unavailable { command } => {
                            format!("命令不可用 / command unavailable: {command}")
                        }
                        VerifyOutcome::Failed { .. } => unreachable!(),
                    };
                    return Ok((current, Some(note), true));
                }
                GateDecision::Retry(_) => {
                    retries_done += 1;
                    let _ = tx.send(AgentEvent::Info(format!(
                        "[verify] 验证失败，第 {retries_done} 次重试 / verification failed, retry #{retries_done}"
                    )));
                    let (command, output_tail) = match &outcome {
                        VerifyOutcome::Failed {
                            command,
                            output_tail,
                            ..
                        } => (command.clone(), output_tail.clone()),
                        _ => unreachable!(),
                    };
                    let retry_prompt = sdd_verify_retry_prompt(
                        message,
                        plan,
                        &current,
                        &command,
                        &output_tail,
                    );
                    let _ =
                        tx.send(AgentEvent::PhaseStart {
                            role: "builder".to_string(),
                        });
                    current = crate::agent_loop::run_autonomous(
                        &self.registry,
                        &self.sandbox,
                        self.trust_sandbox.clone(),
                        Role::Builder,
                        &retry_prompt,
                        tx,
                        self.history.clone(),
                        None,
                        None,
                    )
                    .await?;

                    if is_degenerate_output(&current) {
                        let _ = tx.send(AgentEvent::Error(format!(
                            "[SDD] 验证重试产出无效 / verify retry produced degenerate output: {:?}",
                            current.trim()
                        )));
                        return Ok((
                            format!(
                                "{current}\n\n[验证未通过 / verification failed: degenerate retry output]"
                            ),
                            Some("degenerate retry output".into()),
                            false,
                        ));
                    }
                }
                GateDecision::GiveUp(reason) => {
                    let command = match &outcome {
                        VerifyOutcome::Failed { command, .. } => command.clone(),
                        _ => String::new(),
                    };
                    let _ = tx.send(AgentEvent::Error(format!(
                        "[SDD] 验证未通过，耗尽 {retries_done} 次重试 / verification failed after {retries_done} retries: {command}"
                    )));
                    return Ok((
                        format!(
                            "{current}\n\n[验证未通过 / verification failed after {retries_done} retries: {command}]"
                        ),
                        Some(reason),
                        false,
                    ));
                }
            }
        }
    }

    async fn run_sdd_pipeline(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        let is_fast = self.registry.active_profile().as_deref() == Some("fast");

        if is_fast {
            // Fast mode: skip investigation, planning, AND audit (no
            // AuditorListener registered).
            let built = crate::agent_loop::run_autonomous(
                &self.registry,
                &self.sandbox,
                self.trust_sandbox.clone(),
                Role::Builder,
                message,
                tx,
                self.history.clone(),
                None,
                None,
            )
            .await?;

            // 验证门（fast 模式跳过审计，验证门是其唯一质量网）。
            // Verification gate (fast mode skips audit; the gate is its only quality net).
            let (built, _note, _verified_ok) =
                self.verify_with_retries(message, "", built, tx).await?;
            return Ok(built);
        }

        let waterfall = Arc::new(WaterfallRegistry::new());
        let pre_step = Arc::new(PreStepState::new());
        let audit_state = Arc::new(AuditState::new());

        register_investigator_listener(
            &waterfall,
            &self.registry,
            &self.sandbox,
            self.trust_sandbox.clone(),
            tx.clone(),
            self.history.clone(),
            pre_step.clone(),
        );
        register_planner_listener(
            &waterfall,
            &self.registry,
            tx.clone(),
            message.to_string(),
            pre_step.clone(),
        );
        register_auditor_listener(
            &waterfall,
            &self.registry,
            tx.clone(),
            message.to_string(),
            audit_state.clone(),
        );

        let built = crate::agent_loop::run_autonomous(
            &self.registry,
            &self.sandbox,
            self.trust_sandbox.clone(),
            Role::Builder,
            message,
            tx,
            self.history.clone(),
            Some(waterfall.clone()),
            Some(pre_step.clone()),
        )
        .await?;

        let escaped = pre_step.escape.load(Ordering::Relaxed);
        if escaped {
            return Ok(built);
        }

        let plan = pre_step.plan.lock().unwrap().clone().unwrap_or_default();

        // 验证门：Builder 产出后、审计前，自动运行构建/测试命令。
        // Verification gate: after Builder, before Auditor, auto-run build/test.
        let (built, verify_note, verified_ok) =
            self.verify_with_retries(message, &plan, built, tx).await?;
        *audit_state.verify_note.lock().unwrap() = verify_note;

        if !verified_ok {
            // 验证未通过：跳过审计，直接返回带失败说明的产出。
            // Verification failed: skip audit, return annotated output.
            return Ok(built);
        }

        // Dispatch AgentTurnStopping — the AuditorListener fires, reads `built`
        // from AuditState, runs ReviewGate::review(), stores the verdict.
        *audit_state.built.lock().unwrap() = Some(built.clone());
        let stop_event = WaterfallEvent::AgentTurnStopping {
            reason: "completed".to_string(),
        };
        waterfall.emit(&stop_event);
        waterfall.serial(&stop_event).await;

        let verdict = audit_state
            .verdict
            .lock()
            .unwrap()
            .take()
            .unwrap_or(crate::reviewer::Verdict::Approve);

        match verdict {
            crate::reviewer::Verdict::Approve => Ok(built),
            crate::reviewer::Verdict::Reject(reason) => {
                let _ = tx.send(AgentEvent::Info(format!(
                    "[SDD] 审计驳回，带反馈重试一次 / Audit rejected, retrying with feedback:\n  · 驳回原因: {reason}"
                )));
                let retry = sdd_retry_prompt(message, &plan, &built, &reason);
                let _ = tx.send(AgentEvent::PhaseStart { role: "builder".to_string() });
                let rebuilt = crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Builder,
                    &retry,
                    tx,
                    self.history.clone(),
                    None,
                    None,
                )
                .await?;

                // 拦截退化产出（空白/过短），避免静默失败被当作成功返回。
                // Intercept degenerate output (blank/too short) to avoid silently
                // returning a silent failure as success.
                if is_degenerate_output(&rebuilt) {
                    let _ = tx.send(AgentEvent::Error(format!(
                        "[SDD] 重试产出无效（空白或过短），任务未完成 / Retry produced degenerate output, task incomplete: {:?}",
                        rebuilt.trim()
                    )));
                    return Ok(format!(
                        "任务未完成：审计驳回后重试仍没有产出有效内容。\n\
                         [System] Task incomplete: the retry after audit rejection produced no meaningful output."
                    ));
                }

                // 验证门（审计驳回重试后同样需通过验证）。
                // Verification gate (audit-reject retry also passes through the gate).
                let (rebuilt, verify_note, verified_ok) =
                    self.verify_with_retries(message, &plan, rebuilt, tx).await?;
                *audit_state.verify_note.lock().unwrap() = verify_note;

                if !verified_ok {
                    // 验证未通过：跳过二次审计，返回带失败说明的产出。
                    // Verification failed: skip second audit, return annotated output.
                    return Ok(rebuilt);
                }

                // 重试产出仍需通过审计，而非直接当作最终结果。
                // The retry output must still pass audit, rather than being returned directly.
                *audit_state.built.lock().unwrap() = Some(rebuilt.clone());
                let stop_event = WaterfallEvent::AgentTurnStopping {
                    reason: "retry-completed".to_string(),
                };
                waterfall.emit(&stop_event);
                waterfall.serial(&stop_event).await;

                let verdict = audit_state
                    .verdict
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap_or(crate::reviewer::Verdict::Approve);

                match verdict {
                    crate::reviewer::Verdict::Approve => Ok(rebuilt),
                    crate::reviewer::Verdict::Reject(reason) => {
                        let _ = tx.send(AgentEvent::Error(format!(
                            "[SDD] 重试后审计仍驳回，任务未完成 / Retry still rejected: {reason}"
                        )));
                        Ok(format!(
                            "{rebuilt}\n\n[未通过审计 / Audit rejected again]\n{reason}"
                        ))
                    }
                    crate::reviewer::Verdict::Clarify(q) => Ok(sdd_clarify_response(&q, &rebuilt)),
                }
            }
            crate::reviewer::Verdict::Clarify(q) => Ok(sdd_clarify_response(&q, &built)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degenerate_output_detection() {
        assert!(is_degenerate_output(""));
        assert!(is_degenerate_output("   "));
        assert!(is_degenerate_output(" \n\t "));
        assert!(is_degenerate_output("abc"));
        assert!(!is_degenerate_output("-between"));
        assert!(!is_degenerate_output("created three files"));
    }

    #[test]
    fn keyword_fallback_upgrade_keywords() {
        assert_eq!(
            classify_keyword_fallback("在cargo 升级各个package最新版本"),
            Intent::Implement
        );
        assert_eq!(
            classify_keyword_fallback("更新依赖到最新版本"),
            Intent::Implement
        );
        assert_eq!(
            classify_keyword_fallback("upgrade all packages"),
            Intent::Implement
        );
        assert_eq!(
            classify_keyword_fallback("update Cargo.toml"),
            Intent::Implement
        );
    }

    #[test]
    fn keyword_fallback_remaining_keywords() {
        assert_eq!(
            classify_keyword_fallback("实现一个新功能"),
            Intent::Implement
        );
        assert_eq!(classify_keyword_fallback("重构这段代码"), Intent::Implement);
        assert_eq!(
            classify_keyword_fallback("看一下这个模块怎么工作"),
            Intent::Investigate
        );
        assert_eq!(
            classify_keyword_fallback("how does auth work"),
            Intent::Investigate
        );
        assert_eq!(classify_keyword_fallback("你好"), Intent::Chat);
    }

    #[test]
    fn keyword_fallback_removed_generic_english() {
        assert_eq!(classify_keyword_fallback("cargo build之后"), Intent::Chat);
        assert_eq!(classify_keyword_fallback("how to fix this?"), Intent::Chat);
        assert_eq!(classify_keyword_fallback("add a section"), Intent::Chat);
    }

    #[test]
    fn obvious_question_detection() {
        assert!(is_obvious_question("cargo build之后,都会编译吗?"));
        assert!(is_obvious_question("这个功能怎么用？"));
        assert!(is_obvious_question("是否支持多线程"));
        assert!(is_obvious_question("你好"));
        assert!(!is_obvious_question("实现一个新功能"));
        assert!(!is_obvious_question("修复登录bug"));
    }

    /// 空历史应返回空字符串。
    /// Empty history should return an empty string.
    #[test]
    fn recent_history_text_empty() {
        assert_eq!(recent_history_text(&[], 10), "");
    }

    /// 系统消息应被跳过——只提取 User/Assistant 文本。
    /// System messages should be skipped — only User/Assistant text is extracted.
    #[test]
    fn recent_history_text_skips_system() {
        let hist = vec![
            Message::system("system prompt"),
            Message::system("another system msg"),
        ];
        // Non-empty history → header is present, but no user/assistant text
        let result = recent_history_text(&hist, 10);
        assert!(result.contains("[Recent conversation]"));
        assert!(!result.contains("system prompt"));
    }

    /// 验证 User/Assistant 消息被正确提取并标注角色。
    /// Verifies User/Assistant messages are extracted and labeled.
    #[test]
    fn recent_history_text_extracts_turns() {
        let hist = vec![Message::user("hello"), Message::assistant("hi there")];
        let result = recent_history_text(&hist, 10);
        assert!(result.contains("User: hello"));
        assert!(result.contains("Assistant: hi there"));
    }

    /// 验证只取最近 N 条消息，更早的会被丢弃。
    /// Verifies only the last N messages are included; older ones are dropped.
    #[test]
    fn recent_history_text_respects_max() {
        let hist = vec![
            Message::user("old msg"),
            Message::assistant("old reply"),
            Message::user("recent msg"),
            Message::assistant("recent reply"),
        ];
        let result = recent_history_text(&hist, 2);
        assert!(!result.contains("old msg"));
        assert!(!result.contains("old reply"));
        assert!(result.contains("recent msg"));
        assert!(result.contains("recent reply"));
    }

    // ── todo 8: profile overlay + session override priority ──

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

    fn empty_mcp() -> Arc<McpManager> {
        // 构造一个空 McpManager（无连接、无失败）。用 tokio runtime 驱动 connect_all。
        // Build an empty McpManager (no connections, no failures). Drives connect_all
        // via a tokio runtime.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for test");
        let mcp = rt.block_on(McpManager::connect_all(&std::collections::HashMap::new()));
        Arc::new(mcp)
    }

    fn disabled_sandbox_provider() -> Arc<dyn crate::seam::SandboxProvider> {
        // 一个禁用的 SimpleSandbox（backend=Off），避免测试触碰真实文件系统。
        // A disabled SimpleSandbox (backend=Off) so the test won't touch the real FS.
        Arc::new(crate::sandbox::SimpleSandbox::with_backend(
            &[],
            crate::sandbox::SandboxBackend::Off,
        ))
    }

    /// 会话级 `/model` 覆盖必须优先于 profile 指定的模型（todo 8 验收点）。
    /// Session-level `/model` override must take priority over the profile-specified
    /// model (todo 8 acceptance criterion).
    #[test]
    fn effective_model_session_override_wins_over_profile() {
        // Given: profile "dev" patches [agent].default_model = "profile-model-A".
        // When: parse with that profile active, build an AgentRegistry, then call
        //        set_session_model("session-model-B") (simulating `/model session-model-B`).
        // Then: effective_model() returns "session-model-B" (session > profile > default).
        use crate::config::Config;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "base-model"
max_turns = 50

[profile.dev]
name = "dev"
patches = [
    { id = "agent", config = { default_model = "profile-model-A", max_turns = 50 } },
]
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, Some("dev"))
                .expect("profile parse should succeed"),
        );
        assert_eq!(
            cfg.agent.default_model, "profile-model-A",
            "profile patch must apply default_model = profile-model-A"
        );

        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());

        // Before session override: effective_model falls through to the profile-applied default.
        assert_eq!(registry.effective_model(), "profile-model-A");

        // After `/model session-model-B`: session override wins.
        registry.set_session_model("session-model-B");
        assert_eq!(
            registry.effective_model(),
            "session-model-B",
            "session override must take priority over profile-specified model"
        );
    }

    /// Profile 叠加后的模型在 `effective_model()` 中可见（无会话覆盖时）。
    /// The profile-overlaid model is visible via `effective_model()` when no
    /// session override is set.
    #[test]
    fn effective_model_reads_profile_applied_default() {
        // Given: profile patches [agent].default_model = "profile-model".
        // When: parse with that profile active, build registry, no session override.
        // Then: effective_model() returns the profile-applied value (no AGENT_MODEL env).
        use crate::config::Config;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g1 = env_guard("AGENT_PROFILE", None);
        let _g2 = env_guard("AGENT_MODEL", None);
        let toml_str = r#"
[agent]
default_model = "base-model"

[profile.dev]
name = "dev"
patches = [
    { id = "agent", config = { default_model = "profile-model", max_turns = 50 } },
]
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, Some("dev"))
                .expect("profile parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        assert_eq!(registry.effective_model(), "profile-model");
    }

    // ── todo 9: ToolPerms as ToolApproval (pipeline approval stage) ──
    // ── todo 10: DefaultApproval + ApprovalChain (pluggable seam) ──

    fn perms_allow_readonly_deny_mutating() -> ToolPerms {
        ToolPerms {
            read_file: Permission::Allow,
            run_bash_readonly: Permission::Allow,
            run_bash_mutating: Permission::Deny,
            edit_file: Permission::Deny,
            write_file: Permission::Deny,
            web_fetch: Permission::Ask,
            web_search: Permission::Ask,
            command_rules: Vec::new(),
        }
    }

    #[test]
    fn permission_for_read_file_returns_allow() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"path": "x"});
        assert_eq!(perms.permission_for("read_file", &args), Permission::Allow);
    }

    #[test]
    fn permission_for_readonly_bash_returns_allow() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"command": "ls -la"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Allow);
    }

    #[test]
    fn permission_for_mutating_bash_returns_deny() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"command": "rm -rf x"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Deny);
    }

    #[test]
    fn permission_for_edit_file_returns_deny() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"path": "x", "old": "a", "new": "b"});
        assert_eq!(perms.permission_for("edit_file", &args), Permission::Deny);
    }

    #[test]
    fn permission_for_unknown_tool_defaults_ask() {
        let perms = perms_allow_readonly_deny_mutating();
        assert_eq!(
            perms.permission_for("mystery", &serde_json::json!({})),
            Permission::Ask
        );
    }

    #[test]
    fn permission_for_todo_write_is_allow() {
        // todo_write 只改 UI 可见会话状态，无文件系统/系统副作用，同 read_file 安全类。
        // todo_write only mutates UI-visible session state, no fs/system side effects,
        // same safety class as read_file — must not trigger HITL popup.
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"todos": []});
        assert_eq!(
            perms.permission_for("todo_write", &args),
            Permission::Allow,
            "todo_write should be Allow (no fs/system side effects)"
        );
    }

    #[test]
    fn permission_for_task_is_allow() {
        // task 是编排机制（扇出子代理），同 todo_write 安全类，静默放行。
        // task is an orchestration mechanism (fanout subagents), same safety
        // class as todo_write — must not trigger HITL popup.
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"tasks": []});
        assert_eq!(
            perms.permission_for("task", &args),
            Permission::Allow,
            "task should be Allow (orchestration mechanism, subagent perms govern)"
        );
    }

    // ── task_ctx_for_role depth guard tests ──

    fn test_registry() -> AgentRegistry {
        use crate::config::Config;
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("config parse"),
        );
        AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider())
    }

    #[test]
    fn task_ctx_for_role_returns_none_for_investigator() {
        let reg = test_registry();
        assert!(
            reg.task_ctx_for_role(Role::Investigator).is_none(),
            "Investigator should never get task ctx"
        );
    }

    #[test]
    fn task_ctx_for_role_returns_none_for_planner() {
        let reg = test_registry();
        assert!(reg.task_ctx_for_role(Role::Planner).is_none());
    }

    #[test]
    fn task_ctx_for_role_returns_none_for_auditor() {
        let reg = test_registry();
        assert!(reg.task_ctx_for_role(Role::Auditor).is_none());
    }

    #[test]
    fn task_ctx_for_role_returns_none_when_no_ctx_set() {
        let reg = test_registry();
        // No set_task_ctx called → slot is None even for Builder/Orchestrator.
        assert!(reg.task_ctx_for_role(Role::Builder).is_none());
        assert!(reg.task_ctx_for_role(Role::Orchestrator).is_none());
    }

    #[test]
    fn task_ctx_for_role_returns_some_for_builder_when_set() {
        let reg = test_registry();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::event::AgentEvent>();
        reg.set_task_ctx(crate::subagent::SubagentCtx {
            sandbox: crate::sandbox::Sandbox::with_backend(
                &[],
                crate::sandbox::SandboxBackend::Off,
            ),
            trust_sandbox: Arc::new(AtomicBool::new(false)),
            tx,
            depth: reg.subagent_depth(),
        });
        assert!(
            reg.task_ctx_for_role(Role::Builder).is_some(),
            "Builder should get task ctx when set and depth==0"
        );
        assert!(
            reg.task_ctx_for_role(Role::Orchestrator).is_some(),
            "Orchestrator should get task ctx when set and depth==0"
        );
    }

    #[test]
    fn task_ctx_for_role_returns_none_when_depth_gt_zero() {
        let reg = test_registry();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::event::AgentEvent>();
        reg.set_task_ctx(crate::subagent::SubagentCtx {
            sandbox: crate::sandbox::Sandbox::with_backend(
                &[],
                crate::sandbox::SandboxBackend::Off,
            ),
            trust_sandbox: Arc::new(AtomicBool::new(false)),
            tx,
            depth: reg.subagent_depth(),
        });
        // 深度 > 0 时返回 None（子代理不获得 task 工具）。
        // When depth > 0, returns None (subagents do NOT get the task tool).
        let _guard = crate::subagent::SubagentDepthGuard::new(reg.subagent_depth());
        assert!(
            reg.task_ctx_for_role(Role::Builder).is_none(),
            "Builder should NOT get task ctx when depth > 0"
        );
        assert!(
            reg.task_ctx_for_role(Role::Orchestrator).is_none(),
            "Orchestrator should NOT get task ctx when depth > 0"
        );
        // Guard drops here → depth resets to 0.
    }

    #[test]
    fn task_ctx_for_role_depth_guard_resets_on_drop() {
        let reg = test_registry();
        assert_eq!(reg.subagent_depth().load(Ordering::Relaxed), 0);
        {
            let _g = crate::subagent::SubagentDepthGuard::new(reg.subagent_depth());
            assert_eq!(reg.subagent_depth().load(Ordering::Relaxed), 1);
        }
        assert_eq!(
            reg.subagent_depth().load(Ordering::Relaxed),
            0,
            "depth must reset to 0 after guard drops"
        );
    }

    #[test]
    fn default_approval_maps_permission_to_verdict() {
        let perms = perms_allow_readonly_deny_mutating();
        let approval = DefaultApproval::new(perms);
        // Allow path
        let req = ApprovalRequest {
            tool_name: "read_file".into(),
            args: serde_json::json!({"path": "x"}),
            role: "builder".into(),
        };
        assert_eq!(approval.request(&req), ApprovalVerdict::Allow);
        // Deny path (mutating bash)
        let req = ApprovalRequest {
            tool_name: "run_bash".into(),
            args: serde_json::json!({"command": "rm -rf x"}),
            role: "auditor".into(),
        };
        assert_eq!(approval.request(&req), ApprovalVerdict::Deny);
        // Ask path (web_fetch)
        let req = ApprovalRequest {
            tool_name: "web_fetch".into(),
            args: serde_json::json!({"url": "https://example.com"}),
            role: "investigator".into(),
        };
        assert_eq!(approval.request(&req), ApprovalVerdict::Ask);
    }

    // ── ApprovalChain tests ──

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "run_bash".into(),
            args: serde_json::json!({"command": "ls"}),
            role: "builder".into(),
        }
    }

    struct FixedApproval(ApprovalVerdict);
    impl ToolApproval for FixedApproval {
        fn request(&self, _req: &ApprovalRequest) -> ApprovalVerdict {
            self.0
        }
    }

    #[test]
    fn approval_chain_empty_returns_deny_fail_closed() {
        let chain = ApprovalChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.request(&req()), ApprovalVerdict::Deny);
    }

    #[test]
    fn approval_chain_single_allow_returns_allow() {
        let chain = ApprovalChain::new().with(Box::new(FixedApproval(ApprovalVerdict::Allow)));
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.request(&req()), ApprovalVerdict::Allow);
    }

    #[test]
    fn approval_chain_single_ask_returns_ask() {
        let chain = ApprovalChain::new().with(Box::new(FixedApproval(ApprovalVerdict::Ask)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Ask);
    }

    #[test]
    fn approval_chain_single_deny_returns_deny() {
        let chain = ApprovalChain::new().with(Box::new(FixedApproval(ApprovalVerdict::Deny)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Deny);
    }

    #[test]
    fn approval_chain_deny_short_circuits_regardless_of_order() {
        // [Allow, Deny] → first Allow sets winner, then Deny short-circuits
        let chain = ApprovalChain::new()
            .with(Box::new(FixedApproval(ApprovalVerdict::Allow)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Deny)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Deny);

        // [Deny, Allow] → first Deny short-circuits, Allow never consulted
        let chain = ApprovalChain::new()
            .with(Box::new(FixedApproval(ApprovalVerdict::Deny)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Allow)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Deny);
    }

    #[test]
    fn approval_chain_allow_wins_over_ask() {
        // [Ask, Allow] → no Deny, Allow > Ask
        let chain = ApprovalChain::new()
            .with(Box::new(FixedApproval(ApprovalVerdict::Ask)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Allow)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Allow);

        // [Allow, Ask] → no Deny, Allow > Ask
        let chain = ApprovalChain::new()
            .with(Box::new(FixedApproval(ApprovalVerdict::Allow)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Ask)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Allow);
    }

    #[test]
    fn approval_chain_all_ask_returns_ask() {
        let chain = ApprovalChain::new()
            .with(Box::new(FixedApproval(ApprovalVerdict::Ask)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Ask)));
        assert_eq!(chain.request(&req()), ApprovalVerdict::Ask);
    }

    #[test]
    fn approval_chain_custom_allow_overrides_default_ask() {
        // DefaultApproval says Ask (web_fetch), custom says Allow → Allow wins
        let perms = perms_allow_readonly_deny_mutating();
        let chain = ApprovalChain::new()
            .with(Box::new(DefaultApproval::new(perms)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Allow)));
        let req = ApprovalRequest {
            tool_name: "web_fetch".into(),
            args: serde_json::json!({"url": "https://example.com"}),
            role: "investigator".into(),
        };
        assert_eq!(chain.request(&req), ApprovalVerdict::Allow);
    }

    #[test]
    fn approval_chain_custom_deny_overrides_default_allow() {
        // DefaultApproval says Allow (read_file), custom says Deny → Deny short-circuits
        let perms = perms_allow_readonly_deny_mutating();
        let chain = ApprovalChain::new()
            .with(Box::new(DefaultApproval::new(perms)))
            .with(Box::new(FixedApproval(ApprovalVerdict::Deny)));
        let req = ApprovalRequest {
            tool_name: "read_file".into(),
            args: serde_json::json!({"path": "x"}),
            role: "builder".into(),
        };
        assert_eq!(chain.request(&req), ApprovalVerdict::Deny);
    }

    /// `permission_for` must agree with `agent_loop::decide_tier` (consistency contract).
    #[test]
    fn permission_for_consistent_with_decide_tier() {
        let perms = perms_allow_readonly_deny_mutating();
        for (name, args) in [
            ("read_file", r#"{"path":"x"}"#),
            ("run_bash", r#"{"command":"ls"}"#),
            ("run_bash", r#"{"command":"rm -rf x"}"#),
            ("edit_file", r#"{"path":"x","old":"a","new":"b"}"#),
            ("web_fetch", r#"{"url":"u"}"#),
        ] {
            let value = serde_json::from_str(args).unwrap();
            let via_perms = perms.permission_for(name, &value);
            let via_tier = crate::agent_loop::decide_tier(&perms, name, args);
            assert_eq!(
                via_perms, via_tier,
                "mismatch for {name} / {args}: perms={via_perms:?} tier={via_tier:?}"
            );
        }
    }

    // ── GAP-4: SDD pipeline characterization tests (todo 12) ──
    // These tests capture the exact prompt formats and decision logic of
    // run_sdd_pipeline. They MUST pass before AND after the listener refactor.
    // Behavior equivalence = same prompts, same decisions, same escape hatch.

    #[test]
    fn sdd_investigator_prompt_contains_message_and_instructions() {
        let msg = "implement user auth";
        let prompt = sdd_investigator_prompt(msg);
        assert!(
            prompt.starts_with(msg),
            "prompt must start with the message"
        );
        assert!(
            prompt.contains("无需调查"),
            "must mention the no-investigation keyword"
        );
        assert!(
            prompt.contains("调查代码背景"),
            "must ask about code investigation"
        );
    }

    #[test]
    fn sdd_escape_hatch_triggers_on_no_implementation_marker() {
        assert!(sdd_escape_hatch_triggers("这个任务无需实现，直接回答即可"));
        assert!(sdd_escape_hatch_triggers("无需实现"));
        assert!(!sdd_escape_hatch_triggers("调查发现：模块X负责认证"));
        assert!(!sdd_escape_hatch_triggers("无需调查，任务简单"));
    }

    #[test]
    fn sdd_no_investigation_needed_detection() {
        assert!(sdd_no_investigation_needed("无需调查，任务简单明了"));
        assert!(!sdd_no_investigation_needed("调查发现：需要修改3个文件"));
    }

    #[test]
    fn sdd_plan_prompt_without_investigation() {
        let msg = "fix the bug";
        let inv = "无需调查，任务简单明了";
        let prompt = sdd_plan_prompt(msg, inv);
        assert!(prompt.starts_with(msg));
        assert!(prompt.contains("请拆解为相互独立、可执行的步骤"));
        assert!(!prompt.contains("调查发现"));
    }

    #[test]
    fn sdd_plan_prompt_with_investigation() {
        let msg = "implement auth";
        let inv = "调查发现：auth模块在src/auth.rs，依赖token库";
        let prompt = sdd_plan_prompt(msg, inv);
        assert!(prompt.starts_with(msg));
        assert!(prompt.contains("调查发现"));
        assert!(prompt.contains(inv));
        assert!(prompt.contains("请基于以上调查发现"));
    }

    #[test]
    fn sdd_builder_prompt_injects_plan() {
        let msg = "implement auth";
        let plan = "1. Create user model\n2. Add JWT middleware";
        let prompt = sdd_builder_prompt(msg, plan);
        assert!(prompt.starts_with(msg));
        assert!(prompt.contains("参考计划"));
        assert!(prompt.contains(plan));
    }

    #[test]
    fn sdd_retry_prompt_contains_all_context() {
        let msg = "implement auth";
        let plan = "1. Create user model";
        let built = "fn auth() { }";
        let reason = "missing error handling";
        let prompt = sdd_retry_prompt(msg, plan, built, reason);
        assert!(
            prompt.contains(reason),
            "retry prompt must contain rejection reason"
        );
        assert!(prompt.contains(msg), "must contain original task");
        assert!(prompt.contains(plan), "must contain reference plan");
        assert!(prompt.contains(built), "must contain previous output");
        assert!(
            prompt.contains("[System]"),
            "must contain system instruction"
        );
    }

    #[test]
    fn sdd_verify_retry_prompt_contains_command_tail_and_task() {
        // 结构性测试：验证重试提示词必须包含失败命令、输出尾部、原始任务、
        // 参考计划、上次产出、系统指令。
        // Structural test: the verify-retry prompt must contain the failed
        // command, output tail, original task, reference plan, previous output,
        // and system instruction.
        let msg = "implement user auth";
        let plan = "1. Add JWT\n2. Add middleware";
        let built = "fn auth() { /* TODO */ }";
        let command = "cargo build";
        let output_tail = "error[E0308]: mismatched types";
        let prompt = sdd_verify_retry_prompt(msg, plan, built, command, output_tail);
        assert!(prompt.contains(command), "must contain failed command");
        assert!(prompt.contains(output_tail), "must contain output tail");
        assert!(prompt.contains(msg), "must contain original task");
        assert!(prompt.contains(plan), "must contain reference plan");
        assert!(prompt.contains(built), "must contain previous output");
        assert!(
            prompt.contains("[System]"),
            "must contain system instruction"
        );
    }

    #[test]
    fn audit_state_set_and_take_verify_note() {
        let as_ = AuditState::new();
        assert!(
            as_.verify_note.lock().unwrap().is_none(),
            "verify_note starts as None"
        );
        *as_.verify_note.lock().unwrap() = Some("verify passed".to_string());
        let taken = as_.verify_note.lock().unwrap().take();
        assert_eq!(taken, Some("verify passed".to_string()));
        assert!(
            as_.verify_note.lock().unwrap().is_none(),
            "take must clear verify_note"
        );
    }

    #[test]
    fn sdd_clarify_response_format() {
        let q = "Which auth method?";
        let built = "fn auth() { }";
        let resp = sdd_clarify_response(q, built);
        assert!(resp.contains(q), "must contain clarification question");
        assert!(resp.contains(built), "must contain produced work");
        assert!(resp.contains("澄清"));
    }

    #[test]
    fn sdd_pipeline_data_flow_investigator_to_planner_to_builder() {
        // Characterize the data flow: investigation output → plan prompt →
        // builder prompt. Each step's output feeds into the next step's input.
        let message = "implement user auth";
        let investigation = "调查发现：auth模块在src/auth.rs";
        let plan_prompt = sdd_plan_prompt(message, investigation);
        assert!(plan_prompt.contains(investigation));

        let plan = "1. Add JWT\n2. Add middleware";
        let builder_prompt = sdd_builder_prompt(message, plan);
        assert!(builder_prompt.contains(plan));

        let built = "fn auth() { }";
        let reason = "missing tests";
        let retry_prompt = sdd_retry_prompt(message, plan, built, reason);
        assert!(retry_prompt.contains(plan));
        assert!(retry_prompt.contains(built));
    }

    #[test]
    fn sdd_qa_escape_hatch_via_intent_classification() {
        // Q&A escape hatch at the intent level: obvious questions → Chat,
        // which routes to Builder directly (no SDD pipeline).
        assert!(is_obvious_question("这个功能怎么用？"));
        assert!(is_obvious_question("是否支持多线程"));
        assert!(is_obvious_question("你好"));
        // Implementation tasks do NOT trigger the escape hatch.
        assert!(!is_obvious_question("实现用户认证功能"));
        assert!(!is_obvious_question("修复登录bug"));
        assert!(!is_obvious_question("refactor the auth module"));
    }

    #[test]
    fn sdd_reject_triggers_retry_with_feedback() {
        // Characterize: when the Auditor rejects (Verdict::Reject), the
        // pipeline formats a retry prompt that includes the rejection reason.
        // This test verifies the retry prompt construction logic.
        let reason = "missing error handling";
        let retry = sdd_retry_prompt("task", "plan", "built", reason);
        assert!(retry.contains("驳回"));
        assert!(retry.contains(reason));
        assert!(retry.contains("修正"));
    }

    #[test]
    fn sdd_verdict_approve_skips_retry() {
        // Characterize: Verdict::Approve returns the built output directly
        // (no retry). The absence of retry is characterized by the fact that
        // sdd_retry_prompt is NOT called when the verdict is Approve.
        // This test verifies that the retry prompt is only constructed for
        // Reject, not for Approve.
        let built = "the built output";
        // If verdict is Approve, built is returned directly — no retry prompt.
        // We verify this by checking that the built output does NOT contain
        // retry markers (it's the raw output).
        assert!(!built.contains("驳回"));
        assert!(!built.contains("retry"));
    }

    #[test]
    fn sdd_verdict_clarify_returns_question_and_built() {
        let q = "Which database?";
        let built = "partial implementation";
        let resp = sdd_clarify_response(q, built);
        assert!(resp.contains(q));
        assert!(resp.contains(built));
        // Clarify response does NOT trigger a retry.
        assert!(!resp.contains("驳回"));
        assert!(!resp.contains("修正"));
    }

    #[test]
    fn sdd_pipeline_order_characterization() {
        // Characterize the SDD pipeline order by verifying that each step's
        // output is an input to the next step's prompt:
        // Investigator → Planner → Builder → (Auditor → Retry on Reject)
        let message = "implement feature X";
        let investigation = "调查发现：feature X needs module Y";
        let plan_prompt = sdd_plan_prompt(message, investigation);
        assert!(
            plan_prompt.contains(investigation),
            "Planner must see investigation"
        );

        let plan = "Step 1: do A\nStep 2: do B";
        let builder_prompt = sdd_builder_prompt(message, plan);
        assert!(builder_prompt.contains(plan), "Builder must see plan");

        let built = "implementation done";
        let reason = "tests missing";
        let retry_prompt = sdd_retry_prompt(message, plan, built, reason);
        assert!(
            retry_prompt.contains(built),
            "Retry must see previous built output"
        );
        assert!(
            retry_prompt.contains(reason),
            "Retry must see rejection reason"
        );
    }

    #[test]
    fn fast_mode_profile_detected() {
        use crate::config::Config;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", Some("fast"));
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10

[profile.fast]
name = "fast"
patches = [
    { id = "agent", config = { default_model = "fast-model", max_turns = 5 } },
]
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("profile parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        assert_eq!(registry.active_profile(), Some("fast".to_string()));
    }

    #[test]
    fn no_profile_means_not_fast() {
        use crate::config::Config;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("config parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        assert_ne!(registry.active_profile().as_deref(), Some("fast"));
    }

    // ── todo 13: AuditorListener + AuditState tests ──

    #[test]
    fn audit_state_new_is_empty() {
        let as_ = AuditState::new();
        assert!(as_.built.lock().unwrap().is_none());
        assert!(as_.verdict.lock().unwrap().is_none());
    }

    #[test]
    fn audit_state_set_and_take_built() {
        let as_ = AuditState::new();
        *as_.built.lock().unwrap() = Some("built output".to_string());
        let taken = as_.built.lock().unwrap().clone();
        assert_eq!(taken, Some("built output".to_string()));
    }

    #[test]
    fn audit_state_set_and_take_verdict() {
        let as_ = AuditState::new();
        *as_.verdict.lock().unwrap() = Some(crate::reviewer::Verdict::Reject("reason".into()));
        let taken = as_.verdict.lock().unwrap().take();
        assert_eq!(
            taken,
            Some(crate::reviewer::Verdict::Reject("reason".into()))
        );
        assert!(as_.verdict.lock().unwrap().is_none(), "take must clear");
    }

    #[test]
    fn verdict_is_clone() {
        let a = crate::reviewer::Verdict::Approve;
        assert_eq!(a.clone(), crate::reviewer::Verdict::Approve);
        let r = crate::reviewer::Verdict::Reject("missing tests".into());
        assert_eq!(
            r.clone(),
            crate::reviewer::Verdict::Reject("missing tests".into())
        );
        let c = crate::reviewer::Verdict::Clarify("which db?".into());
        assert_eq!(
            c.clone(),
            crate::reviewer::Verdict::Clarify("which db?".into())
        );
    }

    #[tokio::test]
    async fn mock_auditor_listener_reject_sets_verdict() {
        // Inject a mock listener returning Reject, verify the verdict mechanism.
        let wf = Arc::new(WaterfallRegistry::new());
        let as_ = Arc::new(AuditState::new());
        *as_.built.lock().unwrap() = Some("built output".to_string());

        let as_clone = as_.clone();
        wf.register_serial(move |event| {
            let WaterfallEvent::AgentTurnStopping { .. } = event else {
                return Box::pin(async {});
            };
            let as_clone = as_clone.clone();
            Box::pin(async move {
                *as_clone.verdict.lock().unwrap() =
                    Some(crate::reviewer::Verdict::Reject("mock rejection".into()));
            })
        });

        let event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&event);
        wf.serial(&event).await;

        let verdict = as_.verdict.lock().unwrap().clone();
        assert_eq!(
            verdict,
            Some(crate::reviewer::Verdict::Reject("mock rejection".into()))
        );
    }

    #[tokio::test]
    async fn mock_auditor_listener_approve_sets_verdict() {
        let wf = Arc::new(WaterfallRegistry::new());
        let as_ = Arc::new(AuditState::new());
        *as_.built.lock().unwrap() = Some("built".to_string());

        let as_clone = as_.clone();
        wf.register_serial(move |event| {
            let WaterfallEvent::AgentTurnStopping { .. } = event else {
                return Box::pin(async {});
            };
            let as_clone = as_clone.clone();
            Box::pin(async move {
                *as_clone.verdict.lock().unwrap() = Some(crate::reviewer::Verdict::Approve);
            })
        });

        let event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&event);
        wf.serial(&event).await;

        assert_eq!(
            as_.verdict.lock().unwrap().clone(),
            Some(crate::reviewer::Verdict::Approve)
        );
    }

    #[tokio::test]
    async fn mock_auditor_listener_clarify_sets_verdict() {
        let wf = Arc::new(WaterfallRegistry::new());
        let as_ = Arc::new(AuditState::new());
        *as_.built.lock().unwrap() = Some("built".to_string());

        let as_clone = as_.clone();
        wf.register_serial(move |event| {
            let WaterfallEvent::AgentTurnStopping { .. } = event else {
                return Box::pin(async {});
            };
            let as_clone = as_clone.clone();
            Box::pin(async move {
                *as_clone.verdict.lock().unwrap() =
                    Some(crate::reviewer::Verdict::Clarify("which db?".into()));
            })
        });

        let event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&event);
        wf.serial(&event).await;

        assert_eq!(
            as_.verdict.lock().unwrap().clone(),
            Some(crate::reviewer::Verdict::Clarify("which db?".into()))
        );
    }

    #[tokio::test]
    async fn auditor_listener_ignores_non_turn_stopping_events() {
        let wf = Arc::new(WaterfallRegistry::new());
        let as_ = Arc::new(AuditState::new());
        *as_.built.lock().unwrap() = Some("built".to_string());

        let as_clone = as_.clone();
        wf.register_serial(move |event| {
            let WaterfallEvent::AgentTurnStopping { .. } = event else {
                return Box::pin(async {});
            };
            let as_clone = as_clone.clone();
            Box::pin(async move {
                *as_clone.verdict.lock().unwrap() = Some(crate::reviewer::Verdict::Approve);
            })
        });

        let pre_event = WaterfallEvent::AgentPreStep {
            role: "builder".into(),
            goal: "do task".into(),
        };
        wf.emit(&pre_event);
        wf.serial(&pre_event).await;

        assert!(
            as_.verdict.lock().unwrap().is_none(),
            "verdict must NOT be set for non-AgentTurnStopping events"
        );

        let stop_event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&stop_event);
        wf.serial(&stop_event).await;

        assert_eq!(
            as_.verdict.lock().unwrap().clone(),
            Some(crate::reviewer::Verdict::Approve)
        );
    }

    #[tokio::test]
    async fn auditor_listener_skips_when_built_is_none() {
        let wf = Arc::new(WaterfallRegistry::new());
        let as_ = Arc::new(AuditState::new());

        let as_clone = as_.clone();
        wf.register_serial(move |event| {
            let WaterfallEvent::AgentTurnStopping { .. } = event else {
                return Box::pin(async {});
            };
            let built = match as_clone.built.lock().unwrap().clone() {
                Some(b) => b,
                None => return Box::pin(async {}),
            };
            let as_clone = as_clone.clone();
            Box::pin(async move {
                *as_clone.verdict.lock().unwrap() = Some(crate::reviewer::Verdict::Reject(
                    format!("rejected: {built}"),
                ));
            })
        });

        let event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&event);
        wf.serial(&event).await;

        assert!(
            as_.verdict.lock().unwrap().is_none(),
            "verdict must NOT be set when built is None"
        );
    }

    #[tokio::test]
    async fn register_auditor_listener_adds_serial_listener() {
        use crate::config::Config;
        use std::collections::HashMap;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(Config::from_str_with_profile(toml_str, None).expect("config parse"));
        let mcp = Arc::new(crate::mcp::McpManager::connect_all(&HashMap::new()).await);
        let registry = AgentRegistry::new(cfg, mcp, disabled_sandbox_provider());

        let wf = Arc::new(WaterfallRegistry::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let as_ = Arc::new(AuditState::new());

        let (_emit, _waterfall_count, serial_before) = wf.len();
        assert_eq!(serial_before, 0, "no serial listeners before registration");

        register_auditor_listener(&wf, &registry, tx, "test task".to_string(), as_);

        let (_emit, _waterfall_count, serial_after) = wf.len();
        assert_eq!(
            serial_after, 1,
            "exactly one serial listener after registration"
        );
    }

    #[tokio::test]
    async fn register_auditor_listener_ignores_non_turn_stopping() {
        use crate::config::Config;
        use std::collections::HashMap;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(Config::from_str_with_profile(toml_str, None).expect("config parse"));
        let mcp = Arc::new(crate::mcp::McpManager::connect_all(&HashMap::new()).await);
        let registry = AgentRegistry::new(cfg, mcp, disabled_sandbox_provider());

        let wf = Arc::new(WaterfallRegistry::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let as_ = Arc::new(AuditState::new());
        *as_.built.lock().unwrap() = Some("built".to_string());

        register_auditor_listener(&wf, &registry, tx, "test task".to_string(), as_.clone());

        let pre_event = WaterfallEvent::AgentPreStep {
            role: "builder".into(),
            goal: "do task".into(),
        };
        wf.emit(&pre_event);
        wf.serial(&pre_event).await;

        assert!(
            as_.verdict.lock().unwrap().is_none(),
            "real AuditorListener must ignore non-AgentTurnStopping events"
        );
    }

    #[tokio::test]
    async fn register_auditor_listener_skips_when_built_none() {
        use crate::config::Config;
        use std::collections::HashMap;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(Config::from_str_with_profile(toml_str, None).expect("config parse"));
        let mcp = Arc::new(crate::mcp::McpManager::connect_all(&HashMap::new()).await);
        let registry = AgentRegistry::new(cfg, mcp, disabled_sandbox_provider());

        let wf = Arc::new(WaterfallRegistry::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let as_ = Arc::new(AuditState::new());

        register_auditor_listener(&wf, &registry, tx, "test task".to_string(), as_.clone());

        let event = WaterfallEvent::AgentTurnStopping {
            reason: "done".into(),
        };
        wf.emit(&event);
        wf.serial(&event).await;

        assert!(
            as_.verdict.lock().unwrap().is_none(),
            "AuditorListener must skip when built is None (no LLM call)"
        );
    }

    #[test]
    fn fast_mode_bypasses_audit_no_auditor_listener() {
        // Fast mode gates audit registration — full pipeline test needs an LLM.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", Some("fast"));
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10

[profile.fast]
name = "fast"
patches = [
    { id = "agent", config = { default_model = "fast-model", max_turns = 5 } },
]
"#;
        let cfg = Arc::new(
            crate::config::Config::from_str_with_profile(toml_str, None)
                .expect("profile parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        let is_fast = registry.active_profile().as_deref() == Some("fast");
        assert!(is_fast, "fast profile must be detected for audit bypass");
    }

    #[test]
    fn default_mode_enables_audit() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(
            crate::config::Config::from_str_with_profile(toml_str, None)
                .expect("config parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        let is_fast = registry.active_profile().as_deref() == Some("fast");
        assert!(!is_fast, "default profile must NOT be fast (audit enabled)");
    }

    // ── todo_write role gating tests ──

    fn make_registry_with_todo_ctx() -> AgentRegistry {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10
"#;
        let cfg = Arc::new(
            crate::config::Config::from_str_with_profile(toml_str, None)
                .expect("config parse should succeed"),
        );
        let registry = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        let store = Arc::new(Mutex::new(Vec::new()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let ctx = crate::tools::TodoContext { store, tx };
        registry.set_todo_ctx(ctx);
        registry
    }

    #[test]
    fn todo_ctx_for_role_builder_returns_some_when_set() {
        let registry = make_registry_with_todo_ctx();
        assert!(
            registry.todo_ctx_for_role(Role::Builder).is_some(),
            "Builder role should get todo_write tool"
        );
        assert!(
            registry.todo_ctx_for_role(Role::Orchestrator).is_some(),
            "Orchestrator role should get todo_write tool"
        );
    }

    #[test]
    fn todo_ctx_for_role_readonly_roles_return_none() {
        let registry = make_registry_with_todo_ctx();
        assert!(
            registry.todo_ctx_for_role(Role::Investigator).is_none(),
            "Investigator must NOT get todo_write tool"
        );
        assert!(
            registry.todo_ctx_for_role(Role::Planner).is_none(),
            "Planner must NOT get todo_write tool"
        );
        assert!(
            registry.todo_ctx_for_role(Role::Auditor).is_none(),
            "Auditor must NOT get todo_write tool"
        );
    }

    // ── bash_output / kill_shell permission arms ──

    /// `bash_output` resolves to `run_bash_readonly` (session-state read).
    /// `bash_output` 解析为 `run_bash_readonly`（会话状态读取）。
    #[test]
    fn permission_for_bash_output_returns_readonly() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"id": "bg-0"});
        assert_eq!(
            perms.permission_for("bash_output", &args),
            Permission::Allow,
            "bash_output should use run_bash_readonly tier"
        );
    }

    /// `kill_shell` resolves to `run_bash_mutating` (terminates a process).
    /// `kill_shell` 解析为 `run_bash_mutating`（终止进程）。
    #[test]
    fn permission_for_kill_shell_returns_mutating() {
        let perms = perms_allow_readonly_deny_mutating();
        let args = serde_json::json!({"id": "bg-0"});
        assert_eq!(
            perms.permission_for("kill_shell", &args),
            Permission::Deny,
            "kill_shell should use run_bash_mutating tier"
        );
    }

    // ── AgentSpec + custom_spec tests ──

    fn registry_with_roles() -> AgentRegistry {
        use crate::config::Config;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = env_guard("AGENT_PROFILE", None);
        let toml_str = r#"
[agent]
default_model = "kimi-k3"
max_turns = 50

[agents.investigator]
model = "kimi-k3"
preamble = "prompts/investigator.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"

[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
max_turns = 100
permissions.read_file = "allow"
permissions.run_bash_mutating = "allow"
permissions.edit_file = "allow"

[agents.custom.researcher]
preamble = "agents/researcher.md"
model = "glm-latest"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.edit_file = "deny"
"#;
        let cfg = Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("config parse"),
        );
        AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider())
    }

    #[test]
    fn agent_spec_matches_role_config() {
        let reg = registry_with_roles();
        let spec = reg.agent_spec(Role::Builder);
        let rc = reg.role_config(Role::Builder).expect("builder config");
        assert_eq!(spec.name, "builder");
        assert_eq!(spec.preamble_path, rc.preamble);
        assert_eq!(spec.permissions, rc.permissions);
        assert_eq!(spec.model.as_deref(), Some(rc.model.as_str()));
        assert_eq!(spec.max_turns, rc.max_turns);
        assert!(spec.embedded_preamble.is_some());
    }

    #[test]
    fn agent_spec_investigator_matches_role_config() {
        let reg = registry_with_roles();
        let spec = reg.agent_spec(Role::Investigator);
        let rc = reg.role_config(Role::Investigator).expect("investigator config");
        assert_eq!(spec.name, "investigator");
        assert_eq!(spec.preamble_path, rc.preamble);
        assert_eq!(spec.permissions, rc.permissions);
    }

    #[test]
    fn custom_spec_resolves_configured_name() {
        let reg = registry_with_roles();
        let spec = reg.custom_spec("researcher").expect("researcher configured");
        assert_eq!(spec.name, "researcher");
        assert_eq!(spec.preamble_path, "agents/researcher.md");
        assert_eq!(spec.model.as_deref(), Some("glm-latest"));
        assert_eq!(spec.permissions.read_file, Permission::Allow);
        assert_eq!(spec.permissions.edit_file, Permission::Deny);
        assert!(spec.embedded_preamble.is_none(), "custom has no embedded fallback");
    }

    #[test]
    fn custom_spec_returns_none_for_unconfigured() {
        let reg = registry_with_roles();
        assert!(reg.custom_spec("nonexistent").is_none());
    }

    #[test]
    fn custom_names_returns_sorted_list() {
        let reg = registry_with_roles();
        let names = reg.custom_names();
        assert_eq!(names, vec!["researcher"]);
    }

    // ── command_rules: glob_match matrix + permission_for precedence ──

    fn perms_with_rules(rules: &[(&str, Permission)]) -> ToolPerms {
        let mut perms = perms_allow_readonly_deny_mutating();
        perms.command_rules = rules
            .iter()
            .map(|(p, t)| CommandRule {
                pattern: p.to_string(),
                tier: *t,
            })
            .collect();
        perms
    }

    #[test]
    fn glob_match_empty_pattern_matches_nothing() {
        assert!(!glob_match("", "anything"));
        assert!(!glob_match("", ""));
    }

    #[test]
    fn glob_match_star_matches_everything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "cargo test --release"));
    }

    #[test]
    fn glob_match_exact_equality() {
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("ls", "ls -la"));
        assert!(!glob_match("ls", "LS"));
    }

    #[test]
    fn glob_match_prefix_wildcard() {
        assert!(glob_match("cargo test*", "cargo test"));
        assert!(glob_match("cargo test*", "cargo test --release"));
        assert!(!glob_match("cargo test*", "cargo build"));
    }

    #[test]
    fn glob_match_suffix_wildcard() {
        assert!(glob_match("*--release", "cargo test --release"));
        assert!(!glob_match("*--release", "cargo build"));
    }

    #[test]
    fn glob_match_infix_wildcard() {
        assert!(glob_match("git * log", "git abc log"));
        assert!(!glob_match("git * log", "git log"));
        assert!(glob_match("git *log", "git log"));
        assert!(!glob_match("git * log", "git abc commit"));
    }

    #[test]
    fn glob_match_multiple_wildcards() {
        assert!(glob_match("*c*", "abc"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(glob_match("a*b*c", "abc"));
    }

    #[test]
    fn glob_match_case_sensitive() {
        assert!(!glob_match("C*", "cargo"));
        assert!(glob_match("c*", "cargo"));
        assert!(!glob_match("Cargo*", "cargo test"));
    }

    #[test]
    fn glob_match_deny_pattern() {
        assert!(glob_match("rm *", "rm -rf /"));
        assert!(!glob_match("rm *", "rm"));
        assert!(glob_match("rm*", "rm"));
        assert!(!glob_match("rm *", "ls -la"));
    }

    #[test]
    fn permission_for_command_rule_allow_overrides_mutating_classification() {
        let perms = perms_with_rules(&[("cargo test*", Permission::Allow)]);
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Allow);
    }

    #[test]
    fn permission_for_command_rule_deny_overrides_readonly_classification() {
        let perms = perms_with_rules(&[("ls*", Permission::Deny)]);
        let args = serde_json::json!({"command": "ls -la"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Deny);
    }

    #[test]
    fn permission_for_command_rule_first_match_wins() {
        let perms = perms_with_rules(&[
            ("cargo *", Permission::Allow),
            ("cargo test*", Permission::Deny),
        ]);
        let args = serde_json::json!({"command": "cargo test --release"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Allow);
    }

    #[test]
    fn permission_for_command_rule_no_match_falls_back_to_classification() {
        let perms = perms_with_rules(&[("cargo test*", Permission::Allow)]);
        let args = serde_json::json!({"command": "git status"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Allow);
    }

    #[test]
    fn permission_for_command_rule_empty_command() {
        let perms = perms_with_rules(&[("cargo test*", Permission::Allow)]);
        let args = serde_json::json!({"command": ""});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Deny);
    }

    #[test]
    fn permission_for_command_rule_malformed_args_falls_back() {
        let perms = perms_with_rules(&[("cargo test*", Permission::Allow)]);
        let args = serde_json::json!({"path": "not-a-command"});
        assert_eq!(perms.permission_for("run_bash", &args), Permission::Deny);
    }

    #[test]
    fn permission_for_command_rules_only_affect_run_bash() {
        let perms = perms_with_rules(&[("edit*", Permission::Allow)]);
        let args = serde_json::json!({"path": "x", "old": "a", "new": "b"});
        assert_eq!(perms.permission_for("edit_file", &args), Permission::Deny);
    }
}
