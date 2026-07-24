// 注册表模块：定义角色（Role）、按工具的权限分级（ToolPerms / Permission）、
// 以及构建和管理各角色 Agent 的 AgentRegistry。权限分级驱动自主循环的 HITL（人在环）控制。
use crate::providers::{create_client, ChatAgent};
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Agent 角色：编排者 / 规划者 / 构建者 / 审计者。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Orchestrator,
    Planner,
    Builder,
    Auditor,
}

/// 单个角色的运行时配置：模型、preamble（提示词）文件、权限分级。
#[derive(Debug, Deserialize, Clone)]
pub struct RoleConfig {
    pub model: String,
    pub preamble: String,
    #[serde(default)]
    pub permissions: ToolPerms,
}

// 自主循环 HITL（人在环）门控所用的按工具权限分级：
// `allow` = 自动执行不询问；`ask` = 暂停请人类确认；`deny` = 拦截调用并向模型说明原因。
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
}

/// 读类工具默认允许（自动执行）。
fn default_allow() -> Permission {
    Permission::Allow
}
/// 会改变状态的工具默认需询问人类。
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
        }
    }
}

/// 单条权限：允许 / 需询问 / 拒绝。
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    #[default]
    Allow,
    Ask,
    Deny,
}

/// 注册表顶层配置：来自 agent.toml，包含循环上限与各角色配置。
#[derive(Debug, Deserialize)]
pub struct AgentRegistryConfig {
    #[serde(default)]
    pub max_turns: usize,
    #[serde(rename = "agents")]
    pub roles: std::collections::HashMap<String, RoleConfig>,
}

impl AgentRegistryConfig {
    /// 从 agent.toml 加载配置。
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: AgentRegistryConfig = toml::from_str(&raw)?;
        Ok(cfg)
    }

    /// 返回循环上限；为 0 时回退到默认 20 轮。
    pub fn max_turns(&self) -> usize {
        if self.max_turns == 0 {
            20
        } else {
            self.max_turns
        }
    }
}

/// 绑定到某个角色的 Agent：模型 + preamble（提示词，从 .md 文件加载）+ 权限。
pub struct RoleAgent {
    #[allow(dead_code)]
    pub role: Role,
    agent: ChatAgent,
    // 权限在构建时即已生效（决定 Agent 能拿到哪些工具）。此处保留字段用于
    // 运行时检视 / Phase-3 的策略检查。
    #[allow(dead_code)]
    pub permissions: ToolPerms,
}

impl RoleAgent {
    /// 用该角色 Agent 直接执行一次任务（用于一次性 chat 模式）。
    pub async fn run(&self, task: &str) -> anyhow::Result<String> {
        Ok(self.agent.prompt(task).await?)
    }
}

/// Agent 注册表：持有共享配置，并为各角色构建 Agent；同时保存会话级的模型覆盖。
pub struct AgentRegistry {
    config: Arc<AgentRegistryConfig>,
    // 整个会话的运行时模型覆盖。一旦设置，所有角色都使用该 slug 而非各自配置的模型，
    // 让用户无需改文件即可从 REPL 切换到免费模型（如 tencent/hy3:free）。
    session_model: Arc<Mutex<Option<String>>>,
}

impl AgentRegistry {
    pub fn new(config: AgentRegistryConfig) -> Self {
        Self {
            config: Arc::new(config),
            session_model: Arc::new(Mutex::new(None)),
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
        info!("[build] role={key} model={model}");
        // 只要该角色任一内置工具被允许，就为其装配工具。
        let with_tools = rc.permissions.read_file == Permission::Allow
            || rc.permissions.run_bash_readonly == Permission::Allow
            || rc.permissions.run_bash_mutating == Permission::Allow
            || rc.permissions.edit_file == Permission::Allow;
        let params = crate::providers::provider_additional_params();
        let agent = if with_tools {
            let tools = crate::tools::builtin_tools()?;
            client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .tools(tools)
                .additional_params(params)
                .build()
        } else {
            client
                .agent(&model)
                .preamble(&preamble)
                .temperature(crate::providers::Provider::clamp_temperature(0.7))
                .additional_params(params)
                .build()
        };
        Ok(RoleAgent {
            role,
            agent,
            permissions: rc.permissions.clone(),
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

    /// 解析当前生效的模型：会话覆盖 → agent.toml 各角色配置中的首选模型。
    /// 用于在 registry.build() 之外（如基准评估）获取模型名。
    pub fn effective_model(&self) -> String {
        self.session_model().unwrap_or_else(|| {
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

/// 将用户消息分类为意图，对应 OMO 的意图门（Intent Gate）。
/// 为一次性 chat 模式预留；当前自主循环直接处理所有非元命令输入。
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    Implement,
    Investigate,
    Chat,
}

#[allow(dead_code)]
pub fn classify(message: &str) -> Intent {
    let m = message.to_lowercase();
    if ["implement", "add", "create", "fix", "write", "build"]
        .iter()
        .any(|k| m.contains(k))
    {
        Intent::Implement
    } else if ["look into", "investigate", "check", "find", "how does"]
        .iter()
        .any(|k| m.contains(k))
    {
        Intent::Investigate
    } else {
        Intent::Chat
    }
}

/// 编排者：先分类意图，再委派给对应的角色 Agent。相当于 Sisyphus 的编排层。
/// 为一次性 chat 模式预留；当前自主循环直接处理非元命令输入。
#[allow(dead_code)]
pub struct Orchestrator {
    registry: AgentRegistry,
}

#[allow(dead_code)]
impl Orchestrator {
    pub fn new(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    pub async fn handle(&self, message: &str) -> anyhow::Result<String> {
        let intent = classify(message);
        let role = match intent {
            Intent::Implement => Role::Builder,
            Intent::Investigate => Role::Planner,
            Intent::Chat => Role::Orchestrator,
        };
        let agent = self.registry.build(role)?;
        agent.run(message).await
    }
}
