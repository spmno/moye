// 注册表模块：定义角色（Role）、按工具的权限分级（ToolPerms / Permission）、
// Registry module: defines roles (Role), per-tool permission tiers (ToolPerms / Permission),
// 以及构建和管理各角色 Agent 的 AgentRegistry。权限分级驱动自主循环的 HITL（人在环）控制。
// and AgentRegistry for building and managing role Agents. Permission tiers drive autonomous loop HITL (Human-in-the-Loop) control.
use crate::event::{AgentEvent, EventSender};
use crate::mcp::McpManager;
use crate::providers::ChatAgent;
use crate::sandbox::Sandbox;
use rig_agent::client::AgentClientExt;
use rig_core::completion::Message;
use serde::Deserialize;
use std::sync::atomic::AtomicBool;
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

            info!("[{:?}] \u{6267}\u{884c}\u{4efb}\u{52a1}\u{ff08}\u{5c1d}\u{8bd5} {}/{}\u{ff09}", self.role, attempt + 1, MAX_RETRIES + 1);
            let stream = self.agent.runner(&prompt).stream().await;

            match crate::agent_loop::consume_stream(stream, None, tx).await {
                Ok(output) => return Ok(output),
                Err(e) if crate::agent_loop::is_stream_error(&e) && attempt < MAX_RETRIES => {
                    let remaining = MAX_RETRIES - attempt;
                    let err_snippet: String = e.to_string().chars().take(200).collect();
                    let _ = tx.send(AgentEvent::Info(format!(
                        "[重试 / Retry] {:?} 第 {}/{} 次：SSE 连接中断，剩余 {} 次。错误摘要: {}",
                        self.role, attempt + 1, MAX_RETRIES, remaining, err_snippet
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
            self.role, self.role
        ))
    }
}

/// Agent 注册表：持有共享配置，并为各角色构建 Agent；同时保存会话级的模型覆盖。
/// Agent registry: holds shared config, builds Agents per role; also stores session-level model override.
pub struct AgentRegistry {
    config: Arc<crate::config::Config>,
    mcp: Arc<McpManager>,
    sandbox: crate::sandbox::Sandbox,
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
        sandbox: crate::sandbox::Sandbox,
    ) -> Self {
        let session_model = std::env::var("MY_AGENT_MODEL").ok();
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

    pub fn sandbox(&self) -> &crate::sandbox::Sandbox {
        &self.sandbox
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
        let preamble = std::fs::read_to_string(&rc.preamble)
            .unwrap_or_else(|_| format!("你是 {key} agent。"));
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
        let agent = if with_tools {
            let builder = client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .default_max_turns(max_turns);
            let builder = crate::tools::add_builtin_tools(builder, self.context_config(), &self.sandbox);
            let builder = if !self.mcp.is_empty() {
                let mut b = builder;
                for (tools, sink) in self.mcp.all_tools_and_sinks() {
                    b = b.rmcp_tools(tools, sink);
                }
                b
            } else {
                builder
            };
            builder.max_tokens(max_output).build()
        } else {
            client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .default_max_turns(max_turns)
                .max_tokens(max_output)
                .build()
        };
        Ok(RoleAgent {
            role,
            agent,
        })
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
        "吗", "么", "呢", "吧", "怎么", "如何", "是否", "会不会",
        "能不能", "为什么", "是什么", "哪个", "哪些", "哪里", "多少",
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
        "实现", "添加", "创建", "修复", "编写", "构建", "修改", "重构", "删除", "升级", "更新",
        "implement", "refactor", "upgrade", "update",
    ];
    let investigate_kws = [
        "看一下", "调查", "检查", "查找", "怎么", "分析", "对比", "差距",
        "look into", "investigate", "check", "find", "how does", "explain", "compare",
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
async fn classify_with_llm(message: &str, registry: &AgentRegistry) -> Option<Intent> {
    let client = registry.create_client().ok()?;
    let model = registry
        .session_model()
        .or_else(|| registry.role_config(Role::Orchestrator).map(|rc| rc.model.clone()))
        .unwrap_or_else(|| registry.config.agent.default_model.clone());

    let preamble = "你是一个意图分类器。判断用户消息的意图，只回复一个英文词：\n\
                    - implement: 要求修改、创建、删除代码或文件\n\
                    - investigate: 要求查看、分析、理解代码\n\
                    - chat: 聊天、问答、闲聊\n\
                    只回复一个词，不要任何解释。";

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
    let stream = agent.runner(message).stream().await;
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

/// 意图分类入口：快速路径 → LLM → 关键词降级。
/// Intent classification entry point: fast path → LLM → keyword fallback.
pub async fn classify_intent(message: &str, registry: &AgentRegistry) -> Intent {
    if is_obvious_question(message) {
        return Intent::Chat;
    }
    if let Some(intent) = classify_with_llm(message, registry).await {
        return intent;
    }
    classify_keyword_fallback(message)
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
        // 环境变量 MY_AGENT_TRUST_SANDBOX=on 可在启动时开启信任模式。
        // The MY_AGENT_TRUST_SANDBOX=on env var enables trust mode at startup.
        let trust = std::env::var("MY_AGENT_TRUST_SANDBOX")
            .map(|v| matches!(v.as_str(), "on" | "true" | "1"))
            .unwrap_or(false);
        // 从配置文件 [sandbox].authorized_dirs 预授权目录。
        // Pre-authorize directories from the config file's [sandbox].authorized_dirs.
        let authorized_dirs: Vec<String> = registry
            .config
            .sandbox
            .authorized_dirs
            .iter()
            .cloned()
            .collect();
        let sandbox = Sandbox::with_authorized_dirs(&authorized_dirs);
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

    pub async fn handle(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        let intent = classify_intent(message, &self.registry).await;
        match intent {
            Intent::Implement => self.run_sdd_pipeline(message, tx).await,
            Intent::Investigate => {
                let prior = self.history.lock().unwrap().clone();
                let (out, new_hist) = crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Investigator,
                    message,
                    tx,
                    prior,
                )
                .await?;
                *self.history.lock().unwrap() = new_hist;
                Ok(out)
            }
            Intent::Chat => {
                let agent = self.registry.build(Role::Builder)?;
                agent.run(message, tx).await
            }
        }
    }

    async fn run_sdd_pipeline(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        // 调查步骤：先让调查者判断是否需要调查并探索代码库。
        // Investigation step: let the Investigator decide whether to investigate and explore the codebase.
        let prior = self.history.lock().unwrap().clone();
        let (investigation, inv_hist) = crate::agent_loop::run_autonomous(
            &self.registry,
            &self.sandbox,
            self.trust_sandbox.clone(),
            Role::Investigator,
            &format!(
                "{message}\n\n\
                 请先判断此任务是否需要调查代码背景。如果任务简单明了无需调查，\
                 直接回复\"无需调查\"并简述原因，不调用任何工具。\
                 否则，探索相关代码，理解架构与依赖，产出结构化调查报告。"
            ),
            tx,
            prior,
        )
        .await?;

        // 问答逃生口：如果调查者判断用户消息是问答/咨询，直接返回答案，跳过规划/构建/审计。
        if investigation.contains("无需实现") {
            *self.history.lock().unwrap() = inv_hist;
            return Ok(investigation);
        }

        // 规划步骤：将调查发现注入 Planner 的 prompt。
        // Planning step: inject investigation findings into Planner's prompt.
        let plan_prompt = if investigation.contains("无需调查") {
            format!("{message}\n\n请拆解为相互独立、可执行的步骤。")
        } else {
            format!(
                "{message}\n\n\
                 调查发现：\n{investigation}\n\n\
                 请基于以上调查发现，拆解为相互独立、可执行的步骤。"
            )
        };
        let planner = self.registry.build(Role::Planner)?;
        let plan = planner.run(&plan_prompt, tx).await?;

        let (built, builder_hist) = crate::agent_loop::run_autonomous(
            &self.registry,
            &self.sandbox,
            self.trust_sandbox.clone(),
            Role::Builder,
            &format!("{message}\n\n\u{53c2}\u{8003}\u{8ba1}\u{5212}\u{ff1a}\n{plan}"),
            tx,
            inv_hist,
        )
        .await?;

        let gate = crate::reviewer::ReviewGate::new(self.registry.clone());
        match gate.review(message, &built, tx).await? {
            crate::reviewer::Verdict::Approve => {
                *self.history.lock().unwrap() = builder_hist;
                Ok(built)
            }
            crate::reviewer::Verdict::Reject(reason) => {
                let _ = tx.send(AgentEvent::Info(format!(
                    "[SDD] 审计驳回，带反馈重试一次 / Audit rejected, retrying with feedback:\n  · 驳回原因: {reason}"
                )));
                let retry = format!(
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
                );
                let (retry_out, retry_hist) = crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Builder,
                    &retry,
                    tx,
                    builder_hist,
                )
                .await?;
                *self.history.lock().unwrap() = retry_hist;
                Ok(retry_out)
            }
            crate::reviewer::Verdict::Clarify(q) => {
                *self.history.lock().unwrap() = builder_hist;
                Ok(format!("\u{9700}\u{9700}\u{8981}\u{6f84}\u{6e05}\u{ff1a}{q}\n\n\u{5df2}\u{4ea7}\u{51fa}\u{7684}\u{5de5}\u{4f5c}\u{ff1a}\n{built}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_fallback_upgrade_keywords() {
        assert_eq!(classify_keyword_fallback("在cargo 升级各个package最新版本"), Intent::Implement);
        assert_eq!(classify_keyword_fallback("更新依赖到最新版本"), Intent::Implement);
        assert_eq!(classify_keyword_fallback("upgrade all packages"), Intent::Implement);
        assert_eq!(classify_keyword_fallback("update Cargo.toml"), Intent::Implement);
    }

    #[test]
    fn keyword_fallback_remaining_keywords() {
        assert_eq!(classify_keyword_fallback("实现一个新功能"), Intent::Implement);
        assert_eq!(classify_keyword_fallback("重构这段代码"), Intent::Implement);
        assert_eq!(classify_keyword_fallback("看一下这个模块怎么工作"), Intent::Investigate);
        assert_eq!(classify_keyword_fallback("how does auth work"), Intent::Investigate);
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
}
