/// CLI 应用上下文模块：聚合 [`AgentRegistry`]、[`Orchestrator`]、[`MemoryStore`]、
/// [`PromptEvolver`]，作为 TUI 命令分发与任务执行的统一入口。
/// CLI application context module: aggregates [`AgentRegistry`], [`Orchestrator`],
/// [`MemoryStore`], [`PromptEvolver`], serving as the single entry point for
/// TUI command dispatch and task execution.
use crate::event::{AgentEvent, EventSender};
use crate::evolution::prompt_evolve::PromptEvolver;
use crate::memory::{Lesson, MemoryStore, Turn};
use crate::model_history::ModelHistory;
use crate::registry::{AgentRegistry, Orchestrator};
use crate::{evolution, skills};
use std::sync::{Arc, Mutex};

/// 应用上下文：运行期共享的状态集合，承载所有 `/` 命令分发与任务执行所需依赖。
/// Application context: the shared runtime state holding all dependencies needed for
/// `/` command dispatch and task execution.
pub struct AppContext {
    pub registry: AgentRegistry,
    pub orchestrator: Orchestrator,
    pub memory: MemoryStore,
    pub evolver: PromptEvolver,
    pub rule_threshold: usize,
    /// 跨会话持久化的模型历史，供 `/models` 选择器列出"最近使用"分区。
    /// Cross-session persisted model history, listed as a "recently used" section in `/models`.
    pub model_history: Arc<Mutex<ModelHistory>>,
}

impl AppContext {
    /// 返回当前会话生效的模型标识：会话级覆盖 → 默认模型 → `"deepseek-v4-pro"` 兜底。
    /// Return the model id effective for the current session:
    /// session-level override → default model → `"deepseek-v4-pro"` fallback.
    pub fn current_model(&self) -> String {
        self.registry
            .session_model()
            .or_else(resolve_default_model)
            .unwrap_or_else(|| "deepseek-v4-pro".to_string())
    }

    /// `/model [slug]`：切换会话模型；可选 provider/base_url 用于切回历史模型时恢复当时的网关。
    /// 无 slug 则保持不变（调用方负责显示当前模型）。切换后记入历史，跨会话持久化。
    /// `/model [slug]`: switch the session model; optional provider/base_url restore the
    /// gateway used at the time when switching back to a historical model. Without a slug
    /// the model is unchanged (the caller displays it). Recorded into history, persisted across sessions.
    pub fn cmd_model(
        &self,
        slug: Option<String>,
        provider: Option<String>,
        base_url: Option<String>,
    ) {
        if let Some(s) = slug {
            // 切回历史模型时，连同 provider/base_url 一起恢复，否则切了 slug 但网关不对无法调用。
            if let Some(p) = &provider {
                self.registry.set_session_provider(p);
            }
            if let Some(b) = &base_url {
                self.registry.set_session_base_url(b);
            }
            self.registry.set_session_model(&s);
            // 记入历史：provider/base_url 用最终生效值（override 优先，否则当前全局）。
            let p = provider
                .clone()
                .unwrap_or_else(crate::providers::current_provider_slug);
            let b = base_url
                .clone()
                .unwrap_or_else(crate::providers::current_base_url);
            let mut hist = self.model_history.lock().unwrap();
            hist.record(&s, &p, &b);
            let _ = hist.save();
        }
    }

    /// `/plan [standard|coding|agent]`：查看当前套餐或切换套餐（写入 agent.toml，需重启生效）。
    /// `/plan [standard|coding|agent]`: show the current plan or switch it (writes to agent.toml; restart required).
    pub fn cmd_plan(&self, plan: Option<String>) -> String {
        use crate::providers::{ApiPlan, Provider};

        let provider = Provider::from_env();
        let supported = provider.supported_plans();

        if let Some(p) = plan {
            let new_plan = ApiPlan::parse(&p);
            if !supported.contains(&new_plan) {
                return format!(
                    "供应商 {:?} 不支持 {} 套餐。支持的套餐：{}",
                    provider,
                    new_plan.slug(),
                    supported
                        .iter()
                        .map(|p| p.slug())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if let Err(e) = write_plan_to_config(new_plan.slug()) {
                return format!("写入 agent.toml 失败: {e}");
            }
            return format!(
                "套餐已切换为 {}（{}）。请重启 my-agent 生效。",
                new_plan.slug(),
                new_plan.label()
            );
        }

        let current = Provider::plan_from_env();
        let mut out = format!(
            "当前供应商: {:?} | 当前套餐: {} ({})",
            provider,
            current.slug(),
            current.label()
        );
        out.push_str("\n支持的套餐:");
        for sp in supported {
            let marker = if *sp == current { " ← 当前" } else { "" };
            out.push_str(&format!("\n  /plan {}  →  {}{}", sp.slug(), sp.label(), marker));
        }
        out
    }


    /// `/evolve`: trigger prompt evolution (inject lessons → evaluate → adopt best), returning user-facing text.
    pub async fn cmd_evolve(&self, tx: &EventSender) -> String {
        let lessons = self.memory.load_lessons().unwrap_or_default();
        match self.evolver.evolve(&lessons, tx).await {
            Ok(msg) => msg,
            Err(e) => format!("evolve error: {e}"),
        }
    }

    /// `/evolve-code <file> <old> <new>`：执行代码自修改（编译验证 + 失败回退）。
    /// `/evolve-code <file> <old> <new>`: perform code self-modification (compile-verified + rollback on failure).
    pub fn cmd_evolve_code(&self, file: &str, old: &str, new: &str) -> String {
        match evolution::self_modify::evolve_code(file, old, new) {
            Ok(msg) => msg,
            Err(e) => format!("evolve-code error: {e}"),
        }
    }

    /// `/add-tool <name> <desc>`：生成新工具脚手架（需重新编译才生效）。
    /// `/add-tool <name> <desc>`: scaffold a new tool (requires recompile to take effect).
    pub fn cmd_add_tool(&self, name: &str, description: &str) -> String {
        match evolution::tool_ext::add_tool(name, description) {
            Ok(msg) => msg,
            Err(e) => format!("add-tool error: {e}"),
        }
    }

    /// `/add-skill <name> <desc>`：添加运行时技能（写入 skills/ 下的 Markdown，无需重编译）。
    /// `/add-skill <name> <desc>`: add a runtime skill (writes a Markdown file under skills/, no recompile needed).
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

    /// `/skills`：列出已注册技能清单；无技能时返回提示文本。
    /// `/skills`: list registered skills; returns a notice string when none are registered.
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

    /// `/help`：构造面向用户的帮助文本（含当前供应商、模型与所有命令说明）。
    /// `/help`: build the user-facing help text (including current provider, model, and all command descriptions).
    pub fn cmd_help(&self) -> String {
        let provider = format!("{:?}", crate::providers::current_provider());
        format!(
            "my-agent ({provider}) | model: {}\n\
             \u{2500}\u{2500}\u{2500} \u{547d}\u{4ee4} \u{2500}\u{2500}\u{2500}\n\
             /model [slug]       \u{67e5}\u{770b}\u{6216}\u{5207}\u{6362}\u{5f53}\u{524d}\u{4f1a}\u{8bdd}\u{6a21}\u{578b}\n\
             /models             \u{6253}\u{5f00}\u{4ea4}\u{4e92}\u{5f0f}\u{6a21}\u{578b}\u{9009}\u{62e9}\u{5668}\n\
             /plan [plan]        \u{67e5}\u{770b}\u{6216}\u{5207}\u{6362} API \u{5957}\u{9910}\u{ff08}standard/coding/agent\u{ff0c}\u{9700}\u{91cd}\u{542f}\u{751f}\u{6548}\u{ff09}\n\
             /evolve             \u{89e6}\u{53d1}\u{63d0}\u{793a}\u{8bcd}\u{8fdb}\u{5316}\u{ff08}\u{8bc4}\u{4f30}\u{540e}\u{62e9}\u{4f18}\u{91c7}\u{7eb3}\u{ff09}\n\
             /evolve-code <f> <old> <new>  \u{4ee3}\u{7801}\u{81ea}\u{4fee}\u{6539}\u{ff08}\u{7f16}\u{8bd1}\u{9a8c}\u{8bc1} + \u{56de}\u{9000}\u{ff09}\n\
             /add-tool <name> <desc>  \u{751f}\u{6210}\u{65b0}\u{5de5}\u{5177}\u{811a}\u{624b}\u{67b6}\u{ff08}\u{9700}\u{91cd}\u{65b0}\u{7f16}\u{8bd1}\u{751f}\u{6548}\u{ff09}\n\
             /add-skill <name> <desc>  \u{6dfb}\u{52a0}\u{8fd0}\u{884c}\u{65f6}\u{6280}\u{80fd}\u{ff08}\u{65e0}\u{9700}\u{91cd}\u{7f16}\u{8bd1}\u{ff09}\n\
             /skills             \u{5217}\u{51fa}\u{5df2}\u{6ce8}\u{518c}\u{6280}\u{80fd}\n\
             /history [n]        \u{67e5}\u{770b}\u{6700}\u{8fd1} n \u{8f6e}\u{5bf9}\u{8bdd}\u{8bb0}\u{5f55}\u{ff08}\u{9ed8}\u{8ba4} 10\u{ff09}\n\
             /lessons            \u{67e5}\u{770b}\u{5df2}\u{79ef}\u{7d2f}\u{7684}\u{7ecf}\u{9a8c}\u{6559}\u{8bad}\n\
             /trust              \u{5207}\u{6362}\u{6c99}\u{7bb1}\u{4fe1}\u{4efb}\u{6a21}\u{5f0f}\u{ff08}\u{5f00}\u{542f}\u{540e}\u{6c99}\u{7bb1}\u{5916}\u{8bbf}\u{95ee}\u{81ea}\u{52a8}\u{6388}\u{6743}\u{ff0c}\u{4e0d}\u{518d}\u{5f39}\u{7a97}\u{786e}\u{8ba4}\u{ff09}\n\
             /context            \u{67e5}\u{770b}\u{5f53}\u{524d}\u{4e0a}\u{4e0b}\u{6587}\u{ff08}\u{6a21}\u{578b}\u{3001}token \u{7528}\u{91cf}\u{3001}\u{6d88}\u{606f}\u{5386}\u{53f2}\u{7b49}\u{ff09}\n\
             /help               \u{663e}\u{793a}\u{672c}\u{5e2e}\u{52a9}\n\
             /quit               \u{9000}\u{51fa}\u{7a0b}\u{5e8f}\n\
             \u{2500}\u{2500}\u{2500} \u{7528}\u{6cd5} \u{2500}\u{2500}\u{2500}\n\
             \u{975e} `/` \u{5f00}\u{5934}\u{7684}\u{8f93}\u{5165} \u{2192} \u{4f5c}\u{4e3a}\u{4efb}\u{52a1}\u{76ee}\u{6807}\u{4ea4}\u{7ed9} Orchestrator\u{ff08}SDD \u{7ba1}\u{7ebf}\u{ff09}\u{6267}\u{884c}\n\
             Esc \u{2192} \u{4e2d}\u{65ad}\u{6b63}\u{5728}\u{8fd0}\u{884c}\u{7684}\u{4efb}\u{52a1}",
            self.current_model()
        )
    }

    /// `/history [n]`：加载并格式化最近 n 轮对话记录（默认 10）。
    /// `/history [n]`: load and format the last n turns of conversation (default 10).
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

    /// `/lessons`：加载并格式化已积累的经验教训清单。
    /// `/lessons`: load and format the list of accumulated lessons.
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

    /// 在 TUI 中运行一个任务目标：交由 Orchestrator 处理，成功后记录对话轮与
    /// 经验教训。失败时通过事件通道发送错误。
    /// Run a task goal in the TUI: hand it to the Orchestrator; on success, record turns
    /// and extract a lesson. On failure, send errors via the event channel.
    pub async fn run_goal_tui(&self, goal: &str, tx: &EventSender) {
        match self.orchestrator.handle(goal, tx).await {
            Ok(out) => {
                // consume_stream already emitted AgentEvent::Agent — only record memory here.
                // consume_stream 已经发送过 AgentEvent::Agent —— 这里只负责落记忆。
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
                let lesson = Lesson { summary, ts };
                let _ = self.memory.record_lesson(&lesson);
                if let Ok(Some(rule)) = self.memory.check_and_escalate_rule(&lesson, self.rule_threshold) {
                    let _ = tx.send(AgentEvent::Info(format!(
                        "📋 规则提升：教训反复出现 {} 次，已提升为规则：{}",
                        rule.count, rule.text
                    )));
                }
            }
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(format!("orchestrator error: {e}")));
                let _ = tx.send(AgentEvent::Error(format!("  detail: {e:?}")));
            }
        }
    }
}

/// 把字符串截断到最多 `max` 个字节，且不会在 UTF-8 字符中间切断；
/// 超出时追加省略号 `…`。`s.len() <= max` 时原样返回。
/// Truncate `s` to at most `max` bytes without splitting a UTF-8 character;
/// appends an ellipsis `…` when truncated. Returns `s` unchanged if `s.len() <= max`.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\u{2026}", &s[..s.floor_char_boundary(max)])
    }
}

fn write_plan_to_config(plan: &str) -> std::io::Result<()> {
    let path = "agent.toml";
    let content = std::fs::read_to_string(path)?;
    let mut out = String::with_capacity(content.len() + 32);
    let mut in_provider = false;
    let mut written = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_provider && !written {
                out.push_str(&format!("plan = \"{plan}\"\n"));
                written = true;
            }
            in_provider = trimmed == "[provider]";
        }
        if in_provider && trimmed.starts_with("plan") && trimmed.contains('=') {
            out.push_str(&format!("plan = \"{plan}\"\n"));
            written = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if in_provider && !written {
        out.push_str(&format!("plan = \"{plan}\"\n"));
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        // 短文本不应被截断。
        // Short text should not be truncated.
        assert_eq!(truncate("hello", 500), "hello");
    }

    #[test]
    fn truncate_chinese_text_no_panic() {
        // 中文文本截断不应 panic，且应保留省略号。
        // Truncating Chinese text must not panic and should keep the ellipsis.
        let s = "\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(100);
        let result = truncate(&s, 500);
        assert!(result.ends_with('\u{2026}'));
        assert!(result.len() < 500 + 10);
    }

    #[test]
    fn truncate_at_exact_boundary() {
        // 在字符边界附近截断时不应切断多字节字符。
        // Truncation near a character boundary must not split a multi-byte character.
        let s = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
        let result = truncate(s, 6);
        assert_eq!(result, "\u{4f60}\u{597d}\u{2026}");
    }
}

/// 返回当前 Unix 时间戳（秒）。系统时钟异常时退回 0。
/// Return the current Unix timestamp (seconds). Falls back to 0 if the system clock is unavailable.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 解析默认模型：优先 `MY_AGENT_MODEL` 环境变量，其次统一配置中
/// `[agent].default_model`；均缺失时返回 `None`。
/// Resolve the default model: prefer the `MY_AGENT_MODEL` env var, then the
/// `[agent].default_model` from the unified config; returns `None` when both are absent.
fn resolve_default_model() -> Option<String> {
    std::env::var("MY_AGENT_MODEL").ok().or_else(|| {
        let dm = &crate::config::config()?.agent.default_model;
        if dm.is_empty() {
            None
        } else {
            Some(dm.clone())
        }
    })
}
