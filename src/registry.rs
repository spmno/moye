// 注册表模块：定义角色（Role）、按工具的权限分级（ToolPerms / Permission）、
// Registry module: defines roles (Role), per-tool permission tiers (ToolPerms / Permission),
// 以及构建和管理各角色 Agent 的 AgentRegistry。权限分级驱动自主循环的 HITL（人在环）控制。
// and AgentRegistry for building and managing role Agents. Permission tiers drive autonomous loop HITL (Human-in-the-Loop) control.
use crate::event::{AgentEvent, EventSender};
use crate::providers::ChatAgent;
use crate::sandbox::Sandbox;
use rig_agent::client::AgentClientExt;
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
                format!("{task}\n\n[\u{7cfb}\u{7edf}\u{63d0}\u{793a}] \u{4e0a}\u{6b21}\u{56e0} SSE \u{8fde}\u{63a5}\u{4e2d}\u{65ad}\u{ff0c}\u{8bf7}\u{91cd}\u{65b0}\u{751f}\u{6210}\u{3002}")
            };

            info!("[{:?}] \u{6267}\u{884c}\u{4efb}\u{52a1}\u{ff08}\u{5c1d}\u{8bd5} {}/{}\u{ff09}", self.role, attempt + 1, MAX_RETRIES + 1);
            let stream = self.agent.runner(&prompt).stream().await;

            match crate::agent_loop::consume_stream(stream, None, tx).await {
                Ok(output) => return Ok(output),
                Err(e) if crate::agent_loop::is_stream_error(&e) && attempt < MAX_RETRIES => {
                    let _ = tx.send(AgentEvent::Info(format!(
                        "[\u{91cd}\u{8bd5}] {:?} \u{7b2c} {}/{} \u{6b21}\u{ff1a}SSE \u{8fde}\u{63a5}\u{4e2d}\u{65ad}",
                        self.role, attempt + 1, MAX_RETRIES
                    )));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(anyhow::anyhow!("{:?} \u{91cd}\u{8bd5} {MAX_RETRIES} \u{6b21}\u{540e}\u{4ecd}\u{5931}\u{8d25}", self.role))
    }
}

/// Agent 注册表：持有共享配置，并为各角色构建 Agent；同时保存会话级的模型覆盖。
/// Agent registry: holds shared config, builds Agents per role; also stores session-level model override.
pub struct AgentRegistry {
    config: Arc<crate::config::Config>,
    // 整个会话的运行时模型覆盖。一旦设置，所有角色都使用该 slug 而非各自配置的模型，
    // Runtime model override for the entire session. Once set, all roles use this slug instead of their configured model,
    // 让用户无需改文件即可从 REPL 切换到免费模型（如 tencent/hy3:free）。
    // letting users switch to a free model (e.g. tencent/hy3:free) from REPL without editing files.
    session_model: Arc<Mutex<Option<String>>>,
    /// 会话级供应商覆盖（切回历史模型时恢复）。None 时走 env > config。
    /// Session-level provider override (restored when switching back). None falls through.
    session_provider: Arc<Mutex<Option<String>>>,
    /// 会话级 base_url 覆盖（切回历史模型时恢复）。None 时走 env > config > 默认。
    /// Session-level base_url override (restored when switching back). None falls through.
    session_base_url: Arc<Mutex<Option<String>>>,
}

impl AgentRegistry {
    pub fn new(config: Arc<crate::config::Config>) -> Self {
        // MY_AGENT_MODEL 环境变量作为会话级模型覆盖的初始值。
        // The MY_AGENT_MODEL env var serves as the initial session-level model override.
        // 优先级：/model REPL 命令 > MY_AGENT_MODEL env > agent.toml 各角色配置。
        // Priority: /model REPL command > MY_AGENT_MODEL env > agent.toml per-role config.
        let session_model = std::env::var("MY_AGENT_MODEL").ok();
        Self {
            config,
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
            crate::tools::add_builtin_tools(builder, self.context_config())
                .max_tokens(max_output)
                .build()
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

/// 将用户消息分类为意图，驱动 SDD 管线路由。
/// Classifies a user message into an intent, driving SDD pipeline routing.
/// 关键词同时覆盖中文和英文，适配中文模型（DeepSeek/GLM/Kimi）为主的使用场景。
/// Keywords cover both Chinese and English, adapting to usage scenarios primarily with Chinese models (DeepSeek/GLM/Kimi).
pub fn classify(message: &str) -> Intent {
    let m = message.to_lowercase();
    let implement_kws = [
        "实现", "添加", "创建", "修复", "编写", "构建", "修改", "重构", "删除", "升级", "更新",
        "implement", "add", "create", "fix", "write", "build", "refactor", "upgrade", "update",
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
        }
    }

    /// 返回信任模式标志的共享引用，供 TUI 切换。
    /// Returns a shared reference to the trust-mode flag, for TUI toggling.
    pub fn trust_sandbox(&self) -> Arc<AtomicBool> {
        self.trust_sandbox.clone()
    }

    pub async fn handle(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        let intent = classify(message);
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
                )
                .await
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
        let investigation = crate::agent_loop::run_autonomous(
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
        )
        .await?;

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

        let built = crate::agent_loop::run_autonomous(
            &self.registry,
            &self.sandbox,
            self.trust_sandbox.clone(),
            Role::Builder,
            &format!("{message}\n\n\u{53c2}\u{8003}\u{8ba1}\u{5212}\u{ff1a}\n{plan}"),
            tx,
        )
        .await?;

        let gate = crate::reviewer::ReviewGate::new(self.registry.clone());
        match gate.review(message, &built, tx).await? {
            crate::reviewer::Verdict::Approve => Ok(built),
            crate::reviewer::Verdict::Reject(reason) => {
                let _ = tx.send(AgentEvent::Info(format!("[SDD] \u{5ba1}\u{8ba1}\u{9a73}\u{56de}\u{ff0c}\u{5e26}\u{53cd}\u{9988}\u{91cd}\u{8bd5}\u{4e00}\u{6b21}\u{ff1a}{reason}")));
                let retry = format!(
                    "\u{4e4b}\u{524d}\u{7684}\u{5c1d}\u{8bd5}\u{88ab}\u{9a73}\u{56de}\u{ff1a}{reason}\n\n\u{539f}\u{59cb}\u{4efb}\u{52a1}\u{ff1a}{message}\n\n\u{53c2}\u{8003}\u{8ba1}\u{5212}\u{ff1a}{plan}"
                );
                crate::agent_loop::run_autonomous(
                    &self.registry,
                    &self.sandbox,
                    self.trust_sandbox.clone(),
                    Role::Builder,
                    &retry,
                    tx,
                )
                .await
            }
            crate::reviewer::Verdict::Clarify(q) => {
                Ok(format!("\u{9700}\u{8981}\u{6f84}\u{6e05}\u{ff1a}{q}\n\n\u{5df2}\u{4ea7}\u{51fa}\u{7684}\u{5de5}\u{4f5c}\u{ff1a}\n{built}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_upgrade_keywords_route_to_implement() {
        assert_eq!(classify("在cargo 升级各个package最新版本"), Intent::Implement);
        assert_eq!(classify("更新依赖到最新版本"), Intent::Implement);
        assert_eq!(classify("upgrade all packages"), Intent::Implement);
        assert_eq!(classify("update Cargo.toml"), Intent::Implement);
    }

    #[test]
    fn classify_existing_keywords_still_route_correctly() {
        assert_eq!(classify("实现一个新功能"), Intent::Implement);
        assert_eq!(classify("fix the bug in main"), Intent::Implement);
        assert_eq!(classify("看一下这个模块怎么工作"), Intent::Investigate);
        assert_eq!(classify("how does auth work"), Intent::Investigate);
        assert_eq!(classify("你好"), Intent::Chat);
    }
}
