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

    /// `/help` —— 打印所有可用 REPL 命令及简要说明。
    pub fn cmd_help(&self) {
        let provider = format!("{:?}", crate::providers::current_provider());
        info!("my-agent ({provider}) | model: {}", self.current_model());
        info!("─── 命令 ───");
        info!("  /model [slug]       查看或切换当前会话模型");
        info!("  /evolve             触发提示词进化（评估后择优采纳）");
        info!("  /evolve-code <f> <old> <new>  代码自修改（编译验证 + 回退）");
        info!("  /add-tool <name> <desc>  生成新工具脚手架（需重新编译生效）");
        info!("  /add-skill <name> <desc>  添加运行时技能（无需重编译）");
        info!("  /skills             列出已注册技能");
        info!("  /history [n]        查看最近 n 轮对话记录（默认 10）");
        info!("  /lessons            查看已积累的经验教训");
        info!("  /help               显示本帮助");
        info!("  /quit               退出程序");
        info!("─── 用法 ───");
        info!("  非 `/` 开头的输入 → 作为任务目标交给 Orchestrator（SDD 管线）执行");
    }

    /// `/history [n]` —— 从记忆文件加载并打印最近的对话轮次。
    pub fn cmd_history(&self, limit: Option<usize>) {
        let limit = limit.unwrap_or(10);
        match self.memory.load_turns(Some(limit)) {
            Ok(turns) if turns.is_empty() => {
                info!("（暂无对话记录）");
            }
            Ok(turns) => {
                info!("─── 最近 {} 轮对话 ───", turns.len());
                for t in &turns {
                    let role = match t.role.as_str() {
                        "user" => "用户",
                        "agent" => "Agent",
                        other => other,
                    };
                    let preview = truncate(&t.content, 200);
                    info!("  [{role}] {preview}");
                }
            }
            Err(e) => error!("history error: {e}"),
        }
    }

    /// `/lessons` —— 从记忆文件加载并打印所有积累的经验教训。
    pub fn cmd_list_lessons(&self) {
        match self.memory.load_lessons() {
            Ok(lessons) if lessons.is_empty() => {
                info!("（暂无经验记录）");
            }
            Ok(lessons) => {
                info!("─── 经验教训（共 {} 条）───", lessons.len());
                for (i, l) in lessons.iter().enumerate() {
                    info!("  {}. {}", i + 1, l.summary);
                }
            }
            Err(e) => error!("lessons error: {e}"),
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
        // 找到 max 字节之前的最后一个合法字符边界，避免切到多字节字符中间
        let end = s[..max].char_indices().last().map_or(0, |(i, c)| i + c.len_utf8());
        format!("{}…", &s[..end])
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
