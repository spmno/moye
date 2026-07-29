// 注册表模块：定义角色（Role）、按工具的权限分级（ToolPerms / Permission）、
// Registry module: defines roles (Role), per-tool permission tiers (ToolPerms / Permission),
// 以及构建和管理各角色 Agent 的 AgentRegistry。权限分级驱动自主循环的 HITL（人在环）控制。
// and AgentRegistry for building and managing role Agents. Permission tiers drive autonomous loop HITL (Human-in-the-Loop) control.
use crate::event::{AgentEvent, EventSender};
use crate::providers::{create_client, ChatAgent};
use rig_core::client::CompletionClient;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Agent 角色：编排者 / 规划者 / 构建者 / 审计者。
/// Agent roles: Orchestrator / Planner / Builder / Auditor.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Orchestrator,
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

/// `[agent]` 子节：默认模型与循环上限。
/// `[agent]` subsection: default model and loop limit.
#[derive(Debug, Deserialize, Default)]
pub struct AgentSection {
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub max_turns: usize,
}

/// 注册表顶层配置：来自 agent.toml，包含循环上限与各角色配置。
/// Registry top-level config: from agent.toml, includes loop limit and per-role configs.
#[derive(Debug, Deserialize)]
pub struct AgentRegistryConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(rename = "agents")]
    pub roles: std::collections::HashMap<String, RoleConfig>,
}

impl AgentRegistryConfig {
    /// 从 agent.toml 加载配置。
    /// Loads config from agent.toml.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: AgentRegistryConfig = toml::from_str(&raw)?;
        Ok(cfg)
    }

    /// 返回循环上限；为 0 时回退到默认 20 轮。
    /// Returns the loop limit; falls back to default 20 turns when 0.
    pub fn max_turns(&self) -> usize {
        let turns = self.agent.max_turns;
        if turns == 0 {
            20
        } else {
            turns
        }
    }
}

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
    config: Arc<AgentRegistryConfig>,
    // 整个会话的运行时模型覆盖。一旦设置，所有角色都使用该 slug 而非各自配置的模型，
    // 让用户无需改文件即可从 REPL 切换到免费模型（如 tencent/hy3:free）。
    session_model: Arc<Mutex<Option<String>>>,
}

impl AgentRegistry {
    pub fn new(config: AgentRegistryConfig) -> Self {
        // MY_AGENT_MODEL 环境变量作为会话级模型覆盖的初始值。
        // 优先级：/model REPL 命令 > MY_AGENT_MODEL env > agent.toml 各角色配置。
        let session_model = std::env::var("MY_AGENT_MODEL").ok();
        Self {
            config: Arc::new(config),
            session_model: Arc::new(Mutex::new(session_model)),
        }
    }

    /// clone 时共享同一份 Arc（配置与模型覆盖都会同步）。
    pub fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            session_model: self.session_model.clone(),
        }
    }

    /// 覆盖本会话所有角色使用的模型。
    pub fn set_session_model(&self, slug: &str) {
        *self.session_model.lock().unwrap() = Some(slug.to_string());
    }

    pub fn session_model(&self) -> Option<String> {
        self.session_model.lock().unwrap().clone()
    }

    /// 自主循环的上限轮数，从配置透传。
    pub fn max_turns(&self) -> usize {
        self.config.max_turns()
    }

    /// 为指定角色构建 Agent（带工具或纯对话，取决于权限）。
    pub fn build(&self, role: Role) -> anyhow::Result<RoleAgent> {
        let key = format!("{role:?}").to_lowercase();
        let rc = self
            .config
            .roles
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("no config for role {key}"))?;
        let client = create_client()?;
        let preamble = std::fs::read_to_string(&rc.preamble)
            .unwrap_or_else(|_| format!("你是 {key} agent。"));
        // 把与角色领域相关的技能指令注入提示词，使模型遵循技能中的步骤。
        let preamble = inject_skills_public(&preamble);
        // 会话级模型覆盖优先于角色各自配置的模型。
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
        let agent = if with_tools {
            let tools = crate::tools::builtin_tools()?;
            client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .tools(tools)
                .additional_params(params)
                .default_max_turns(max_turns)
                .build()
        } else {
            client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .default_max_turns(max_turns)
                .build()
        };
        Ok(RoleAgent {
            role,
            agent,
        })
    }

    /// 取某角色的按工具权限分级，供自主循环的 HITL（人在环）门控逐次调用决策
    /// （allow / ask / deny）。
    pub fn tool_perms(&self, role: Role) -> ToolPerms {
        let key = format!("{role:?}").to_lowercase();
        self.config
            .roles
            .get(&key)
            .map(|rc| rc.permissions.clone())
            .unwrap_or_default()
    }

    /// 取某角色的配置，供自主循环重建"可运行"的 Agent
    /// （循环需要原始 `Agent`，而非 `RoleAgent` 包装）。
    pub fn role_config(&self, role: Role) -> Option<&RoleConfig> {
        let key = format!("{role:?}").to_lowercase();
        self.config.roles.get(&key)
    }

    /// 解析当前生效的模型：会话覆盖 → [agent].default_model → 各角色配置中的首选模型。
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
pub fn inject_skills_public(preamble: &str) -> String {
    let skill_text = crate::skills::relevant_skills(preamble);
    if skill_text.is_empty() {
        preamble.to_string()
    } else {
        format!("{preamble}\n\n# Loaded Skills\n{skill_text}")
    }
}

/// 意图分类：驱动 SDD 管线路由。
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    Implement,
    Investigate,
    Chat,
}

/// 将用户消息分类为意图，驱动 SDD 管线路由。
/// 关键词同时覆盖中文和英文，适配中文模型（DeepSeek/GLM/Kimi）为主的使用场景。
pub fn classify(message: &str) -> Intent {
    let m = message.to_lowercase();
    let implement_kws = [
        "实现", "添加", "创建", "修复", "编写", "构建", "修改", "重构", "删除",
        "implement", "add", "create", "fix", "write", "build", "refactor",
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
/// Implement → 规划者拆解 → 构建者执行（工具循环 + HITL）→ 审计者两轮评审。
/// Investigate → 规划者只读探索（工具循环，无编辑权限）。
/// Chat → 构建者直接对话（无工具循环）。
pub struct Orchestrator {
    registry: AgentRegistry,
}

impl Orchestrator {
    pub fn new(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    pub async fn handle(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        let intent = classify(message);
        match intent {
            Intent::Implement => self.run_sdd_pipeline(message, tx).await,
            Intent::Investigate => {
                crate::agent_loop::run_autonomous(&self.registry, Role::Planner, message, tx).await
            }
            Intent::Chat => {
                let agent = self.registry.build(Role::Builder)?;
                agent.run(message, tx).await
            }
        }
    }

    async fn run_sdd_pipeline(&self, message: &str, tx: &EventSender) -> anyhow::Result<String> {
        let planner = self.registry.build(Role::Planner)?;
        let plan = planner
            .run(&format!("{message}\n\n\u{8bf7}\u{62c6}\u{89e3}\u{4e3a}\u{76f8}\u{4e92}\u{72ec}\u{7acb}\u{3001}\u{53ef}\u{6267}\u{884c}\u{7684}\u{6b65}\u{9aa4}\u{3002}"), tx)
            .await?;

        let built = crate::agent_loop::run_autonomous(
            &self.registry,
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
                crate::agent_loop::run_autonomous(&self.registry, Role::Builder, &retry, tx).await
            }
            crate::reviewer::Verdict::Clarify(q) => {
                Ok(format!("\u{9700}\u{8981}\u{6f84}\u{6e05}\u{ff1a}{q}\n\n\u{5df2}\u{4ea7}\u{51fa}\u{7684}\u{5de5}\u{4f5c}\u{ff1a}\n{built}"))
            }
        }
    }
}
