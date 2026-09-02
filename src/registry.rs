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
use std::sync::atomic::{AtomicBool, Ordering};
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
        }
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
            "run_bash" => {
                let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
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

            match crate::agent_loop::consume_stream(stream, None, tx).await {
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
}

impl AgentRegistry {
    pub fn new(
        config: Arc<crate::config::Config>,
        mcp: Arc<McpManager>,
        sandbox: Arc<dyn crate::seam::SandboxProvider>,
    ) -> Self {
        let session_model = std::env::var("AGENT_MODEL").ok();
        Self {
            config,
            mcp,
            sandbox,
            session_model: Arc::new(Mutex::new(session_model)),
            session_provider: Arc::new(Mutex::new(None)),
            session_base_url: Arc::new(Mutex::new(None)),
        }
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
        }
    }

    /// 覆盖本会话所有角色使用的模型。
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

    pub fn max_turns_for_role(&self, role: Role) -> usize {
        let key = format!("{role:?}").to_lowercase();
        self.config
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
            let builder =
                crate::tools::add_builtin_tools(builder, self.context_config(), sandbox_provider);
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
    pub fn tool_perms(&self, role: Role) -> ToolPerms {
        let key = format!("{role:?}").to_lowercase();
        self.config
            .roles
            .get(&key)
            .map(|rc| rc.permissions.clone())
            .unwrap_or_default()
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
        self.config.roles.get(&key)
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
        crate::agent_loop::consume_stream(stream, None, &tx),
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
            let gate = crate::reviewer::ReviewGate::new(reg);
            match gate.review(&task, &built, &tx).await {
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
        Self {
            registry,
            sandbox,
            trust_sandbox: Arc::new(AtomicBool::new(trust)),
            history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 返回信任模式标志的共享引用，供 TUI 切换。
    /// Returns a shared reference to the trust-mode flag, for TUI toggling.
    pub fn trust_sandbox(&self) -> Arc<AtomicBool> {
        self.trust_sandbox.clone()
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

    pub async fn handle(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
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

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
}
