use crate::evolution::prompt_evolve::PromptEvolver;
use crate::memory::{Lesson, MemoryStore, Turn};
use crate::registry::{self, AgentRegistry, Intent, Orchestrator};
use crate::{evolution, skills};
use tracing::{error, info};

pub struct AppContext {
    pub registry: AgentRegistry,
    pub orchestrator: Orchestrator,
    pub memory: MemoryStore,
    pub evolver: PromptEvolver,
    pub rule_threshold: u32,
}

impl AppContext {
    pub fn current_model(&self) -> String {
        self.registry
            .session_model()
            .or_else(resolve_default_model)
            .unwrap_or_else(|| "deepseek-v4-pro".to_string())
    }

    pub fn cmd_model(&self, slug: Option<String>) {
        match slug {
            Some(s) => {
                self.registry.set_session_model(&s);
                info!("model set to: {s} (applies to all roles this session)");
            }
            None => info!("current model: {}", self.current_model()),
        }
    }

    pub async fn cmd_evolve(&self) {
        match self.evolver.evolve().await {
            Ok(msg) => info!("{msg}"),
            Err(e) => error!("evolve error: {e}"),
        }
    }

    pub fn cmd_evolve_code(&self, file: &str, old: &str, new: &str) {
        match evolution::self_modify::evolve_code(file, old, new) {
            Ok(msg) => info!("{msg}"),
            Err(e) => error!("evolve-code error: {e}"),
        }
    }

    pub fn cmd_add_tool(&self, name: &str, description: &str) {
        match evolution::tool_ext::add_tool(name, description) {
            Ok(msg) => info!("{msg}"),
            Err(e) => error!("add-tool error: {e}"),
        }
    }

    pub fn cmd_add_skill(&self, name: &str, description: &str) {
        let body = format!(
            "# {}\n\n{}\n\n（在此处描述逐步指令。）\n",
            name, description
        );
        match skills::add_skill(name, description, &body) {
            Ok(msg) => info!("{msg}"),
            Err(e) => error!("add-skill error: {e}"),
        }
    }

    pub fn cmd_list_skills(&self) {
        match skills::SkillManifest::load() {
            Ok(m) => {
                let list = m.list();
                if list.is_empty() {
                    info!("no skills registered");
                } else {
                    for n in list {
                        info!("- {n}");
                    }
                }
            }
            Err(e) => error!("skills error: {e}"),
        }
    }

    /// 执行用户目标：走 Orchestrator（SDD 管线），完成后记录记忆与经验。
    pub async fn run_goal(&self, goal: &str) {
        match self.orchestrator.handle(goal).await {
            Ok(out) => {
                info!("{out}");
                let ts = now();
                let _ = self.memory.append_turn(&Turn {
                    role: "user".into(),
                    content: goal.into(),
                    ts,
                });
                let _ = self.memory.append_turn(&Turn {
                    role: "agent".into(),
                    content: out.clone(),
                    ts,
                });

                let summary = format!("任务: {goal} → 产出: {}", truncate(&out, 200));
                let _ = self.memory.record_lesson(&Lesson { summary, ts });

                let pattern = intent_pattern(goal);
                match self.memory.observe_rule(&pattern, self.rule_threshold) {
                    Ok(true) => {
                        if let Err(e) =
                            self.memory.promote_rule_to_agents_md(&pattern, "AGENTS.md")
                        {
                            error!("promote_rule error: {e}");
                        } else {
                            info!("[进化] 规则升级到 AGENTS.md：{pattern}");
                        }
                    }
                    Ok(false) => {}
                    Err(e) => error!("observe_rule error: {e}"),
                }
            }
            Err(e) => {
                error!("orchestrator error: {e}");
                error!("  detail: {e:?}");
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn intent_pattern(goal: &str) -> String {
    match registry::classify(goal) {
        Intent::Implement => "implement_task".to_string(),
        Intent::Investigate => "investigate_task".to_string(),
        Intent::Chat => "chat_task".to_string(),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn resolve_default_model() -> Option<String> {
    std::env::var("MY_AGENT_MODEL").ok().or_else(|| {
        let raw = std::fs::read_to_string("agent.toml").ok()?;
        let parsed: toml::Value = toml::from_str(&raw).ok()?;
        parsed
            .get("agent")?
            .get("default_model")?
            .as_str()
            .map(|s| s.to_string())
    })
}

pub fn load_memory_cfg() -> anyhow::Result<crate::memory::MemoryConfig> {
    let raw = std::fs::read_to_string("agent.toml")?;
    let parsed: toml::Value = toml::from_str(&raw)?;
    let m = parsed
        .get("memory")
        .ok_or_else(|| anyhow::anyhow!("missing [memory] in agent.toml"))?;
    let cfg = m.clone().try_into()?;
    Ok(cfg)
}

pub fn load_escalation_threshold() -> anyhow::Result<u32> {
    let raw = std::fs::read_to_string("agent.toml")?;
    let parsed: toml::Value = toml::from_str(&raw)?;
    let t = parsed
        .get("evolution")
        .and_then(|e| e.get("rule_escalation_threshold"))
        .and_then(|v| v.as_integer())
        .unwrap_or(3);
    Ok(t as u32)
}
