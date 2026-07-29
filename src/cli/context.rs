use crate::event::{AgentEvent, EventSender};
use crate::evolution::prompt_evolve::PromptEvolver;
use crate::memory::{Lesson, MemoryStore, Turn};
use crate::registry::{self, AgentRegistry, Intent, Orchestrator};
use crate::{evolution, skills};

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
            }
            None => {}
        }
    }

    pub async fn cmd_evolve(&self, tx: &EventSender) -> String {
        match self.evolver.evolve(tx).await {
            Ok(msg) => msg,
            Err(e) => format!("evolve error: {e}"),
        }
    }

    pub fn cmd_evolve_code(&self, file: &str, old: &str, new: &str) -> String {
        match evolution::self_modify::evolve_code(file, old, new) {
            Ok(msg) => msg,
            Err(e) => format!("evolve-code error: {e}"),
        }
    }

    pub fn cmd_add_tool(&self, name: &str, description: &str) -> String {
        match evolution::tool_ext::add_tool(name, description) {
            Ok(msg) => msg,
            Err(e) => format!("add-tool error: {e}"),
        }
    }

    pub fn cmd_add_skill(&self, name: &str, description: &str) -> String {
        let body = format!(
            "# {}\n\n{}\n\n\u{ff08}\u{5728}\u{6b64}\u{5904}\u{63cf}\u{8ff0}\u{9010}\u{6b65}\u{6307}\u{4ee4}\u{3002}\u{ff09}\n",
            name, description
        );
        match skills::add_skill(name, description, &body) {
            Ok(msg) => msg,
            Err(e) => format!("add-skill error: {e}"),
        }
    }

    pub fn cmd_list_skills(&self) -> String {
        match skills::SkillManifest::load() {
            Ok(m) => {
                let list = m.list();
                if list.is_empty() {
                    "no skills registered".to_string()
                } else {
                    list.iter()
                        .map(|n| format!("- {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Err(e) => format!("skills error: {e}"),
        }
    }

    pub fn cmd_help(&self) -> String {
        let provider = format!("{:?}", crate::providers::current_provider());
        format!(
            "my-agent ({provider}) | model: {}\n\
             \u{2500}\u{2500}\u{2500} \u{547d}\u{4ee4} \u{2500}\u{2500}\u{2500}\n\
             /model [slug]       \u{67e5}\u{770b}\u{6216}\u{5207}\u{6362}\u{5f53}\u{524d}\u{4f1a}\u{8bdd}\u{6a21}\u{578b}\n\
             /evolve             \u{89e6}\u{53d1}\u{63d0}\u{793a}\u{8bcd}\u{8fdb}\u{5316}\u{ff08}\u{8bc4}\u{4f30}\u{540e}\u{62e9}\u{4f18}\u{91c7}\u{7eb3}\u{ff09}\n\
             /evolve-code <f> <old> <new>  \u{4ee3}\u{7801}\u{81ea}\u{4fee}\u{6539}\u{ff08}\u{7f16}\u{8bd1}\u{9a8c}\u{8bc1} + \u{56de}\u{9000}\u{ff09}\n\
             /add-tool <name> <desc>  \u{751f}\u{6210}\u{65b0}\u{5de5}\u{5177}\u{811a}\u{624b}\u{67b6}\u{ff08}\u{9700}\u{91cd}\u{65b0}\u{7f16}\u{8bd1}\u{751f}\u{6548}\u{ff09}\n\
             /add-skill <name> <desc>  \u{6dfb}\u{52a0}\u{8fd0}\u{884c}\u{65f6}\u{6280}\u{80fd}\u{ff08}\u{65e0}\u{9700}\u{91cd}\u{7f16}\u{8bd1}\u{ff09}\n\
             /skills             \u{5217}\u{51fa}\u{5df2}\u{6ce8}\u{518c}\u{6280}\u{80fd}\n\
             /history [n]        \u{67e5}\u{770b}\u{6700}\u{8fd1} n \u{8f6e}\u{5bf9}\u{8bdd}\u{8bb0}\u{5f55}\u{ff08}\u{9ed8}\u{8ba4} 10\u{ff09}\n\
             /lessons            \u{67e5}\u{770b}\u{5df2}\u{79ef}\u{7d2f}\u{7684}\u{7ecf}\u{9a8c}\u{6559}\u{8bad}\n\
             /help               \u{663e}\u{793a}\u{672c}\u{5e2e}\u{52a9}\n\
             /quit               \u{9000}\u{51fa}\u{7a0b}\u{5e8f}\n\
             \u{2500}\u{2500}\u{2500} \u{7528}\u{6cd5} \u{2500}\u{2500}\u{2500}\n\
             \u{975e} `/` \u{5f00}\u{5934}\u{7684}\u{8f93}\u{5165} \u{2192} \u{4f5c}\u{4e3a}\u{4efb}\u{52a1}\u{76ee}\u{6807}\u{4ea4}\u{7ed9} Orchestrator\u{ff08}SDD \u{7ba1}\u{7ebf}\u{ff09}\u{6267}\u{884c}",
            self.current_model()
        )
    }

    pub fn cmd_history(&self, limit: Option<usize>) -> String {
        let limit = limit.unwrap_or(10);
        match self.memory.load_turns(Some(limit)) {
            Ok(turns) if turns.is_empty() => {
                "\u{ff08}\u{6682}\u{65e0}\u{5bf9}\u{8bdd}\u{8bb0}\u{5f55}\u{ff09}".to_string()
            }
            Ok(turns) => {
                let mut out = format!("\u{2500}\u{2500}\u{2500} \u{6700}\u{8fd1} {} \u{8f6e}\u{5bf9}\u{8bdd} \u{2500}\u{2500}\u{2500}", turns.len());
                for t in &turns {
                    let role = match t.role.as_str() {
                        "user" => "\u{7528}\u{6237}",
                        "agent" => "Agent",
                        other => other,
                    };
                    let preview = truncate(&t.content, 200);
                    out.push_str(&format!("\n  [{role}] {preview}"));
                }
                out
            }
            Err(e) => format!("history error: {e}"),
        }
    }

    pub fn cmd_list_lessons(&self) -> String {
        match self.memory.load_lessons() {
            Ok(lessons) if lessons.is_empty() => {
                "\u{ff08}\u{6682}\u{65e0}\u{7ecf}\u{9a8c}\u{8bb0}\u{5f55}\u{ff09}".to_string()
            }
            Ok(lessons) => {
                let mut out = format!("\u{2500}\u{2500}\u{2500} \u{7ecf}\u{9a8c}\u{6559}\u{8bad}\u{ff08}\u{5171} {} \u{6761}\u{ff09}\u{2500}\u{2500}\u{2500}", lessons.len());
                for (i, l) in lessons.iter().enumerate() {
                    out.push_str(&format!("\n  {}. {}", i + 1, l.summary));
                }
                out
            }
            Err(e) => format!("lessons error: {e}"),
        }
    }

    pub async fn run_goal_tui(&self, goal: &str, tx: &EventSender) {
        match self.orchestrator.handle(goal, tx).await {
            Ok(out) => {
                // consume_stream already emitted AgentEvent::Agent — only record memory here.
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

                let summary = format!("\u{4efb}\u{52a1}: {goal} \u{2192} \u{4ea7}\u{51fa}: {}", truncate(&out, 200));
                let _ = self.memory.record_lesson(&Lesson { summary, ts });

                let pattern = intent_pattern(goal);
                match self.memory.observe_rule(&pattern, self.rule_threshold) {
                    Ok(true) => {
                        if let Err(e) =
                            self.memory.promote_rule_to_agents_md(&pattern, "AGENTS.md")
                        {
                            let _ = tx.send(AgentEvent::Error(format!("promote_rule error: {e}")));
                        } else {
                            let _ = tx.send(AgentEvent::Info(format!(
                                "[\u{8fdb}\u{5316}] \u{89c4}\u{5219}\u{5347}\u{7ea7}\u{5230} AGENTS.md\u{ff1a}{pattern}"
                            )));
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        let _ = tx.send(AgentEvent::Error(format!("observe_rule error: {e}")));
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(format!("orchestrator error: {e}")));
                let _ = tx.send(AgentEvent::Error(format!("  detail: {e:?}")));
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\u{2026}", &s[..s.floor_char_boundary(max)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        assert_eq!(truncate("hello", 500), "hello");
    }

    #[test]
    fn truncate_chinese_text_no_panic() {
        let s = "\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(100);
        let result = truncate(&s, 500);
        assert!(result.ends_with('\u{2026}'));
        assert!(result.len() < 500 + 10);
    }

    #[test]
    fn truncate_at_exact_boundary() {
        let s = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
        let result = truncate(s, 6);
        assert_eq!(result, "\u{4f60}\u{597d}\u{2026}");
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
