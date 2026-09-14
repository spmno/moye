/// CLI 应用上下文模块：聚合 [`AgentRegistry`]、[`Orchestrator`]、[`MemoryStore`]、
/// [`PromptEvolver`]，作为 TUI 命令分发与任务执行的统一入口。
/// CLI application context module: aggregates [`AgentRegistry`], [`Orchestrator`],
/// [`MemoryStore`], [`PromptEvolver`], serving as the single entry point for
/// TUI command dispatch and task execution.
use crate::event::{AgentEvent, EventSender};
use crate::evolution::prompt_evolve::PromptEvolver;
use crate::memory::{Lesson, MemoryStore};
use crate::model_history::ModelHistory;
use crate::registry::{AgentRegistry, Orchestrator};
use crate::seam::SandboxProvider;
use crate::session::Session;
use crate::{evolution, skills};
use std::sync::{Arc, Mutex};
use tracing::warn;

/// 启动时 OS 级沙箱 provider 的选择结果，由 `[sandbox].mode` 决定。
/// Selection of the OS-level sandbox provider at startup, driven by `[sandbox].mode`.
///
/// 测试友好：纯枚举，可在不构造 trait 对象的情况下断言选择逻辑。
/// Test-friendly: a plain enum, assertable without constructing trait objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// `SimpleSandbox`（bwrap / seatbelt / path 后端）——mode 为 `auto` / `bwrap` / 未知时。
    /// `SimpleSandbox` (bwrap / seatbelt / path backend) — mode is `auto` / `bwrap` / unknown.
    Simple,
    /// `LandlockSandbox`（Landlock LSM，bwrap 不可用时的 fallback）——mode 为 `landlock` 时。
    /// `LandlockSandbox` (Landlock LSM, fallback when bwrap is unavailable) — mode is `landlock`.
    Landlock,
    /// 禁用 OS 级沙箱——mode 为 `off` 时。
    /// OS-level sandbox disabled — mode is `off`.
    Off,
}

/// 根据 `[sandbox].mode` 选择启动时应使用的 OS 级沙箱 provider 类型。
/// Selects which OS-level sandbox provider type to use at startup, based on `[sandbox].mode`.
///
/// - `"landlock"` → [`SandboxKind::Landlock`]
/// - `"off"` / `"false"` / `"0"` → [`SandboxKind::Off`]
/// - 其他（`"auto"` / `"bwrap"` / 未知）→ [`SandboxKind::Simple`]
pub fn select_sandbox_kind(cfg: &crate::config::Config) -> SandboxKind {
    match cfg.sandbox.mode.as_str() {
        "landlock" => SandboxKind::Landlock,
        "off" | "false" | "0" => SandboxKind::Off,
        _ => SandboxKind::Simple,
    }
}

/// 根据组合配置构造 OS 级沙箱 trait 对象（todo 8 启动时选择）。
/// Builds the OS-level sandbox trait object based on the combined config (todo 8).
///
/// 由 `select_sandbox_kind` 决定具体 provider：
/// - [`SandboxKind::Landlock`] → `LandlockSandbox::with_authorized_dirs`
/// - [`SandboxKind::Off`] → `SimpleSandbox`（backend = `Off`）
/// - [`SandboxKind::Simple`] → `SimpleSandbox`（backend 来自 `[sandbox].backend`）
pub fn build_sandbox_provider(cfg: &crate::config::Config) -> Arc<dyn SandboxProvider> {
    match select_sandbox_kind(cfg) {
        SandboxKind::Landlock => Arc::new(crate::provider::LandlockSandbox::with_authorized_dirs(
            &cfg.sandbox.authorized_dirs,
        )),
        SandboxKind::Off => Arc::new(crate::sandbox::SimpleSandbox::with_backend(
            &cfg.sandbox.authorized_dirs,
            crate::sandbox::SandboxBackend::Off,
        )),
        SandboxKind::Simple => {
            let backend = crate::sandbox::SandboxBackend::parse(&cfg.sandbox.backend);
            Arc::new(crate::sandbox::SimpleSandbox::with_backend(
                &cfg.sandbox.authorized_dirs,
                backend,
            ))
        }
    }
}

/// 把 `agent.toml` 原始内容解析 + 应用 profile 叠加后，序列化为人类可读的
/// TOML 字符串。开头以注释形式标注选中的 profile 名（如有）。
///
/// Parses the raw `agent.toml`, applies profile overlay, then serializes the
/// combined tree to a human-readable TOML string. The selected profile name
/// (if any) is noted in a leading comment.
pub fn dump_config_to_string(raw: &str) -> anyhow::Result<String> {
    let (cfg, value, _active_from_parse) = crate::config::Config::parse_combined(raw, None)?;
    // 用 Config::active_profile_name() 而非 parse_combined 返回的 active，既验证
    // 两者一致，也让 active_profile_name() 在生产路径中被实际调用（避免 dead_code）。
    // Use Config::active_profile_name() rather than the active returned by parse_combined,
    // both to cross-check they agree and to exercise the method on the production path.
    let active = cfg.active_profile_name();
    let mut body = toml::to_string_pretty(&value)?;
    if let Some(name) = active {
        let header = format!("# Active profile: {name}\n");
        body = format!("{header}{body}");
    }
    Ok(body)
}

/// 应用上下文：运行期共享的状态集合，承载所有 `/` 命令分发与任务执行所需依赖。
/// Application context: the shared runtime state holding all dependencies needed for
/// `/` command dispatch and task execution.
pub struct AppContext {
    pub registry: AgentRegistry,
    pub orchestrator: Orchestrator,
    pub memory: MemoryStore,
    pub evolver: PromptEvolver,
    pub rule_threshold: usize,
    /// 当前用户级会话（每次打开工具对应一次）。记录本次任务的完整对话，
    /// `--continue` 时会复用上一次会话。
    /// The current user-level session (one per tool invocation). Records the full
    /// conversation of this invocation; `--continue` reuses the previous session.
    pub session: Arc<Mutex<Session>>,
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
            drop(hist);
            // 持久化到 `.moye/.env` 的 AGENT_MODEL，并同步本进程环境：
            // 重启后 `resolve_default_model()` / `AgentRegistry::new()` 优先读 AGENT_MODEL，
            // 否则会回落到 `agent.toml` 的 `[agent].default_model`，导致模型被重置。
            // Persist AGENT_MODEL to `.moye/.env` and sync this process's env: after a
            // restart `resolve_default_model()` / `AgentRegistry::new()` read AGENT_MODEL
            // first, otherwise the model resets to `[agent].default_model` in agent.toml.
            persist_model_env(&s);
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
                "套餐已切换为 {}（{}）。请重启 moye 生效。",
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
            out.push_str(&format!(
                "\n  /plan {}  →  {}{}",
                sp.slug(),
                sp.label(),
                marker
            ));
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
            "moye ({provider}) | model: {}\n\
             \u{2500}\u{2500}\u{2500} \u{547d}\u{4ee4} \u{2500}\u{2500}\u{2500}\n\
             /model [slug]       \u{67e5}\u{770b}\u{6216}\u{5207}\u{6362}\u{5f53}\u{524d}\u{4f1a}\u{8bdd}\u{6a21}\u{578b}\n\
             /models             \u{6253}\u{5f00}\u{4f9b}\u{5e94}\u{5546}\u{9009}\u{62e9}\u{ff08}\u{4f9b}\u{5e94}\u{5546} \u{2192} \u{5957}\u{9910} \u{2192} \u{6a21}\u{578b}\u{ff0c}\u{5373}\u{65f6}\u{751f}\u{6548}\u{ff09}\n\
             /plan [plan]        \u{67e5}\u{770b}\u{6216}\u{5207}\u{6362} API \u{5957}\u{9910}\u{ff08}standard/coding/agent\u{ff0c}\u{9700}\u{91cd}\u{542f}\u{751f}\u{6548}\u{ff09}\n\
             /evolve             \u{89e6}\u{53d1}\u{63d0}\u{793a}\u{8bcd}\u{8fdb}\u{5316}\u{ff08}\u{8bc4}\u{4f30}\u{540e}\u{62e9}\u{4f18}\u{91c7}\u{7eb3}\u{ff09}\n\
             /evolve-code <f> <old> <new>  \u{4ee3}\u{7801}\u{81ea}\u{4fee}\u{6539}\u{ff08}\u{7f16}\u{8bd1}\u{9a8c}\u{8bc1} + \u{56de}\u{9000}\u{ff09}\n\
             /add-tool <name> <desc>  \u{751f}\u{6210}\u{65b0}\u{5de5}\u{5177}\u{811a}\u{624b}\u{67b6}\u{ff08}\u{9700}\u{91cd}\u{65b0}\u{7f16}\u{8bd1}\u{751f}\u{6548}\u{ff09}\n\
             /add-skill <name> <desc>  \u{6dfb}\u{52a0}\u{8fd0}\u{884c}\u{65f6}\u{6280}\u{80fd}\u{ff08}\u{65e0}\u{9700}\u{91cd}\u{7f16}\u{8bd1}\u{ff09}\n\
             /skills             \u{5217}\u{51fa}\u{5df2}\u{6ce8}\u{518c}\u{6280}\u{80fd}\n\
             /history [n]        \u{67e5}\u{770b}\u{6700}\u{8fd1} n \u{8f6e}\u{5bf9}\u{8bdd}\u{8bb0}\u{5f55}\u{ff08}\u{9ed8}\u{8ba4} 10\u{ff09}\n\
             /lessons            \u{67e5}\u{770b}\u{5df2}\u{79ef}\u{7d2f}\u{7684}\u{7ecf}\u{9a8c}\u{6559}\u{8bad}\n\
             /trust              \u{5207}\u{6362}\u{6c99}\u{7bb1}\u{4fe1}\u{4efb}\u{6a21}\u{5f0f}\u{ff08}\u{5f00}\u{542f}\u{540e}\u{6c99}\u{7bb1}\u{5916}\u{8bbf}\u{95ee}\u{81ea}\u{52a8}\u{6388}\u{6743}\u{ff0c}\u{4e0d}\u{518d}\u{5f39}\u{7a97}\u{786e}\u{8ba4}\u{ff09}\n\
             /rewind            \u{56de}\u{6eda}\u{67d0}\u{6b21}\u{4efb}\u{52a1}\u{6539}\u{52a8}\u{7684}\u{6587}\u{4ef6} / rewind a task's file changes\n\
             /context            \u{67e5}\u{770b}\u{5f53}\u{524d}\u{4e0a}\u{4e0b}\u{6587}\u{ff08}\u{6a21}\u{578b}\u{3001}token \u{7528}\u{91cf}\u{3001}\u{6d88}\u{606f}\u{5386}\u{53f2}\u{7b49}\u{ff09}\n\
             /help               \u{663e}\u{793a}\u{672c}\u{5e2e}\u{52a9}\n\
             /quit               \u{9000}\u{51fa}\u{7a0b}\u{5e8f}\n\
             \u{2500}\u{2500}\u{2500} \u{7528}\u{6cd5} \u{2500}\u{2500}\u{2500}\n\
             \u{975e} `/` \u{5f00}\u{5934}\u{7684}\u{8f93}\u{5165} \u{2192} \u{4f5c}\u{4e3a}\u{4efb}\u{52a1}\u{76ee}\u{6807}\u{4ea4}\u{7ed9} Orchestrator\u{ff08}SDD \u{7ba1}\u{7ebf}\u{ff09}\u{6267}\u{884c}\n\
             Esc \u{2192} \u{4e2d}\u{65ad}\u{6b63}\u{5728}\u{8fd0}\u{884c}\u{7684}\u{4efb}\u{52a1}\n\
             Ctrl+E \u{2192} 展开/折叠被截断的工具结果与 diff\n\
             Ctrl+F \u{2192} 搜索会话历史（Enter/↓ 下一个，↑ 上一个，Esc 关闭）\n\
             Ctrl+P \u{2192} 命令面板（模糊搜索所有斜杠命令）",
            self.current_model()
        )
    }

    /// `/history [n]`：加载并格式化最近 n 轮对话记录（默认 10）。
    /// `/history [n]`: load and format the last n turns of conversation (default 10).
    pub fn cmd_history(&self, limit: Option<usize>) -> String {
        let limit = limit.unwrap_or(10);
        let session = self.session.lock().unwrap();
        let turns = session.turns();
        let turns: Vec<_> = if turns.len() > limit {
            turns[turns.len() - limit..].to_vec()
        } else {
            turns.to_vec()
        };
        if turns.is_empty() {
            return "\u{ff08}\u{6682}\u{65e0}\u{5bf9}\u{8bdd}\u{8bb0}\u{5f55}\u{ff09}".to_string();
        }
        let mut out = format!(
            "\u{5f53}\u{524d}\u{4f1a}\u{8bdd} {} \u{ff08}\u{6700}\u{8fd1} {} \u{8f6e}\u{ff09}",
            session.meta.id,
            turns.len()
        );
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

    /// `/lessons`：加载并格式化已积累的经验教训清单。
    /// `/lessons`: load and format the list of accumulated lessons.
    pub fn cmd_list_lessons(&self) -> String {
        match self.memory.load_lessons() {
            Ok(lessons) if lessons.is_empty() => {
                "\u{ff08}\u{6682}\u{65e0}\u{7ecf}\u{9a8c}\u{8bb0}\u{5f55}\u{ff09}".to_string()
            }
            Ok(lessons) => {
                let mut out = format!(
                    "\u{2500}\u{2500}\u{2500} \u{7ecf}\u{9a8c}\u{6559}\u{8bad}\u{ff08}\u{5171} {} \u{6761}\u{ff09}\u{2500}\u{2500}\u{2500}",
                    lessons.len()
                );
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
                // consume_stream already emitted AgentEvent::Agent — record the session
                // conversation here (and extract a lesson from it).
                // consume_stream 已经发送过 AgentEvent::Agent —— 这里负责记录会话对话并提取教训。
                let ts = now();
                {
                    let mut session = self.session.lock().unwrap();
                    let _ = session.append_turn("user", goal);
                    let _ = session.append_turn("agent", &out);
                    // 持久化完整历史（含工具调用/结果），供 --continue 恢复。
                    // Persist full history (incl. tool calls/results) for --continue restore.
                    let full = self.orchestrator.history_snapshot();
                    if let Err(e) = session.set_full_history(full) {
                        warn!("failed to persist full history: {e}");
                    }
                }

                let summary = format!(
                    "\u{4efb}\u{52a1}: {goal} \u{2192} \u{4ea7}\u{51fa}: {}",
                    truncate(&out, 200)
                );
                let lesson = Lesson { summary, ts };
                let _ = self.memory.record_lesson(&lesson);
                if let Ok(Some(rule)) = self
                    .memory
                    .check_and_escalate_rule(&lesson, self.rule_threshold)
                {
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
    let path = crate::config::PROJECT_CONFIG_PATH;
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

    // ── SandboxKind / build_sandbox_provider / dump_config tests (todo 8) ──

    fn sandbox_cfg(mode: &str) -> crate::config::Config {
        let toml_str = format!(
            r#"
[sandbox]
backend = "auto"
mode = "{mode}"
authorized_dirs = []
"#
        );
        toml::from_str(&toml_str).expect("sandbox cfg should parse")
    }

    #[test]
    fn select_sandbox_kind_landlock_mode() {
        // Given: [sandbox] mode = "landlock".
        // When: select_sandbox_kind.
        // Then: returns Landlock.
        let cfg = sandbox_cfg("landlock");
        assert_eq!(select_sandbox_kind(&cfg), SandboxKind::Landlock);
    }

    #[test]
    fn select_sandbox_kind_off_mode() {
        // Given: [sandbox] mode = "off".
        // When: select_sandbox_kind.
        // Then: returns Off.
        let cfg = sandbox_cfg("off");
        assert_eq!(select_sandbox_kind(&cfg), SandboxKind::Off);
    }

    #[test]
    fn select_sandbox_kind_auto_falls_back_to_simple() {
        // Given: [sandbox] mode = "auto" (and "bwrap" / unknown).
        // When: select_sandbox_kind.
        // Then: returns Simple for all non-landlock / non-off modes.
        assert_eq!(
            select_sandbox_kind(&sandbox_cfg("auto")),
            SandboxKind::Simple
        );
        assert_eq!(
            select_sandbox_kind(&sandbox_cfg("bwrap")),
            SandboxKind::Simple
        );
        assert_eq!(
            select_sandbox_kind(&sandbox_cfg("unknown")),
            SandboxKind::Simple
        );
    }

    #[test]
    fn build_sandbox_provider_landlock_probes_without_panic() {
        // Given: config with mode = "landlock".
        // When: build_sandbox_provider.
        // Then: returns an Arc<dyn SandboxProvider> whose probe() returns a valid
        // ProbeLevel without panicking (LandlockSandbox instantiated at startup).
        let cfg = sandbox_cfg("landlock");
        let provider = build_sandbox_provider(&cfg);
        let level = provider.probe();
        use crate::seam::ProbeLevel;
        assert!(
            matches!(
                level,
                ProbeLevel::Full | ProbeLevel::Partial | ProbeLevel::Unusable
            ),
            "probe() must return a valid ProbeLevel, got {level:?}"
        );
    }

    #[test]
    fn build_sandbox_provider_off_mode_yields_disabled_sandbox() {
        // Given: config with mode = "off".
        // When: build_sandbox_provider.
        // Then: probe() returns Unusable (OS-level sandbox disabled).
        // (Path-checking is a separate HITL concern handled by Orchestrator's
        // SimpleSandbox, which gets mode="off" propagated via Orchestrator::new.)
        let cfg = sandbox_cfg("off");
        let provider = build_sandbox_provider(&cfg);
        use crate::seam::ProbeLevel;
        assert_eq!(
            provider.probe(),
            ProbeLevel::Unusable,
            "off mode → probe() must be Unusable"
        );
    }

    #[test]
    fn dump_config_outputs_valid_toml_with_profile_applied() {
        // Given: agent.toml content with a profile patching sandbox.mode to "landlock".
        // When: dump_config_to_string with explicit profile via AGENT_PROFILE env.
        // Then: output is valid TOML (re-parseable) and contains mode = "landlock";
        //       the active profile name appears in the leading comment.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", Some("dev"));
        let raw = r#"
[sandbox]
backend = "auto"
mode = "auto"
authorized_dirs = []

[profile.dev]
name = "dev"
patches = [
    { id = "sandbox", config = { backend = "auto", mode = "landlock", authorized_dirs = [] } },
]
"#;
        let dump = dump_config_to_string(raw).expect("dump should succeed");
        assert!(
            dump.contains("# Active profile: dev"),
            "dump must include the active profile name in a comment, got:\n{dump}"
        );
        assert!(
            dump.contains("mode = \"landlock\""),
            "dump must contain the patched mode value, got:\n{dump}"
        );
        let body = dump
            .strip_prefix("# Active profile: dev\n")
            .unwrap_or(&dump);
        let reparsed: toml::Value = toml::from_str(body).expect("dump body must be valid TOML");
        let mode = reparsed
            .get("sandbox")
            .and_then(|s| s.get("mode"))
            .and_then(|m| m.as_str())
            .expect("sandbox.mode present in reparsed dump");
        assert_eq!(mode, "landlock");
    }

    #[test]
    fn dump_config_no_profile_has_no_active_header() {
        // Given: agent.toml with no [profile] section, no AGENT_PROFILE env.
        // When: dump_config_to_string.
        // Then: output is valid TOML and has no "Active profile" header.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", None);
        let raw = r#"
[sandbox]
backend = "bwrap"
mode = "auto"
"#;
        let dump = dump_config_to_string(raw).expect("dump should succeed");
        assert!(
            !dump.contains("Active profile"),
            "no profile selected → no active-profile header, got:\n{dump}"
        );
        let _reparsed: toml::Value = toml::from_str(&dump).expect("dump must be valid TOML");
    }

    #[test]
    fn dump_config_propagates_profile_format_error() {
        // Given: profile whose patch references a non-existent id.
        // When: dump_config_to_string with that profile active.
        // Then: returns Err (non-zero exit at the call site), not a silent dump.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_PROFILE", Some("bad"));
        let raw = r#"
[sandbox]
backend = "auto"

[profile.bad]
name = "bad"
patches = [
    { id = "nonexistent.path", config = {} },
]
"#;
        let result = dump_config_to_string(raw);
        assert!(
            result.is_err(),
            "profile format error must propagate as Err, got: {result:?}"
        );
    }

    #[test]
    fn upsert_env_line_replaces_existing_model_line() {
        // 已存在 AGENT_MODEL 行 → 替换为新值，其他行（含 key/注释）不变。
        // An existing AGENT_MODEL line is replaced; every other line stays intact.
        let before = "# comment\nAGENT_PROVIDER=deepseek\nAGENT_MODEL=doubao-seed-evolving\nDEEPSEEK_API_KEY=sk-x\n";
        let after = upsert_env_line(before, "AGENT_MODEL", "deepseek-flash");
        assert!(after.contains("AGENT_MODEL=deepseek-flash\n"));
        assert!(!after.contains("doubao-seed-evolving"));
        assert!(after.contains("AGENT_PROVIDER=deepseek\n"));
        assert!(after.contains("DEEPSEEK_API_KEY=sk-x\n"));
        assert!(after.contains("# comment\n"));
        // 只应出现一次 AGENT_MODEL 行。
        assert_eq!(after.matches("AGENT_MODEL=").count(), 1);
    }

    #[test]
    fn upsert_env_line_appends_when_absent_and_ignores_comments() {
        // 无 AGENT_MODEL 行（注释里的同名不算）→ 追加到末尾，注释行保留。
        // No AGENT_MODEL line (a commented one does not count) → appended at the end;
        // the commented line is preserved.
        let before = "# AGENT_MODEL=old\nAGENT_PROVIDER=volcengine\n";
        let after = upsert_env_line(before, "AGENT_MODEL", "deepseek-flash");
        assert!(after.contains("# AGENT_MODEL=old\n"));
        assert!(after.ends_with("AGENT_MODEL=deepseek-flash\n"));
        assert_eq!(after.matches("AGENT_MODEL=deepseek-flash").count(), 1);
    }

    #[test]
    fn upsert_env_line_on_empty_input() {
        // 空文件 → 直接得到一行。
        // Empty input yields a single line.
        assert_eq!(
            upsert_env_line("", "AGENT_MODEL", "deepseek-flash"),
            "AGENT_MODEL=deepseek-flash\n"
        );
    }

    #[test]
    fn persist_model_env_writes_file_and_process_env_without_touching_keys() {
        // 行为级验证：写入 env 文件 + 进程环境；不碰 API key 行；重复切换只保留一行；空值 no-op。
        // Behavior-level: writes the env file + process env; leaves API-key lines intact;
        // repeated switches keep a single line; an empty slug is a no-op.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _guard = env_guard("AGENT_MODEL", None);
        // 用工作树内的 target/ 而非 /tmp（沙箱可能拒绝对 /tmp 的写入）。
        // Use target/ inside the work tree instead of /tmp (the sandbox may deny /tmp writes).
        let dir = std::path::Path::new("target")
            .join(format!("persist-model-env-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "AGENT_PROVIDER=deepseek\nDEEPSEEK_API_KEY=sk-x\n").unwrap();

        persist_model_env_at("deepseek-flash", &path);

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("AGENT_MODEL=deepseek-flash\n"),
            "got:\n{after}"
        );
        assert!(after.contains("AGENT_PROVIDER=deepseek\n"), "got:\n{after}");
        assert!(after.contains("DEEPSEEK_API_KEY=sk-x\n"), "got:\n{after}");
        assert_eq!(
            std::env::var("AGENT_MODEL").ok().as_deref(),
            Some("deepseek-flash"),
            "process env must reflect the switch"
        );

        // 再次切换：替换而非累加。
        persist_model_env_at("deepseek-v4-pro", &path);
        let after2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after2.matches("AGENT_MODEL=").count(), 1, "got:\n{after2}");
        assert!(
            after2.contains("AGENT_MODEL=deepseek-v4-pro\n"),
            "got:\n{after2}"
        );

        // 空 / 空白 slug：no-op（文件与进程环境保持上次值）。
        persist_model_env_at("   ", &path);
        let after3 = std::fs::read_to_string(&path).unwrap();
        assert!(
            after3.contains("AGENT_MODEL=deepseek-v4-pro\n"),
            "got:\n{after3}"
        );
        assert_eq!(
            std::env::var("AGENT_MODEL").ok().as_deref(),
            Some("deepseek-v4-pro"),
            "blank slug must not clear the effective model"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

/// 解析默认模型：优先 `AGENT_MODEL` 环境变量，其次统一配置中
/// `[agent].default_model`；均缺失时返回 `None`。
/// Resolve the default model: prefer the `AGENT_MODEL` env var, then the
/// `[agent].default_model` from the unified config; returns `None` when both are absent.
fn resolve_default_model() -> Option<String> {
    std::env::var("AGENT_MODEL").ok().or_else(|| {
        let dm = &crate::config::config()?.agent.default_model;
        if dm.is_empty() {
            None
        } else {
            Some(dm.clone())
        }
    })
}

/// 把选中的模型 slug 持久化：同步写入本进程的 `AGENT_MODEL` 环境变量（会话内立即一致），
/// 并更新 `.moye/.env` 的 `AGENT_MODEL=` 行（重启后 `resolve_default_model()` /
/// `AgentRegistry::new()` 优先读取）。空 slug 为 no-op；文件写入失败时静默忽略——
/// 环境变量仍已生效，会话不受影响。
/// Persist the selected model slug: sync the in-process `AGENT_MODEL` env var (immediately
/// consistent within the session) and update the `AGENT_MODEL=` line in `.moye/.env`
/// (preferred by `resolve_default_model()` / `AgentRegistry::new()` after a restart).
/// An empty slug is a no-op; env-file write failures are ignored silently — the env var
/// is already applied and the session is unaffected.
fn persist_model_env(slug: &str) {
    persist_model_env_at(slug, std::path::Path::new(crate::config::PROJECT_ENV_PATH));
}

/// `persist_model_env` 的路径可注入版本（便于测试）：同步进程 `AGENT_MODEL` 环境变量，
/// 并把 `AGENT_MODEL=slug` 写入给定 env 文件（更新或追加，父目录自动创建）。
/// Path-injectable core of `persist_model_env` (for tests): syncs the in-process
/// `AGENT_MODEL` env var and writes `AGENT_MODEL=slug` into the given env file
/// (updates or appends; parent dirs are created).
fn persist_model_env_at(slug: &str, path: &std::path::Path) {
    let slug = slug.trim();
    if slug.is_empty() {
        return;
    }
    // 进程环境：读 env 的路径（resolve_default_model / registry 初始化）本会话一致。
    // Process env: env-reading paths (resolve_default_model / registry init) stay consistent.
    unsafe {
        std::env::set_var("AGENT_MODEL", slug);
    }
    // 文件：更新或新增 AGENT_MODEL 行，不触碰其他行（含 API key）。
    // File: update or append the AGENT_MODEL line, leaving other lines (incl. API keys) untouched.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let out = upsert_env_line(&existing, "AGENT_MODEL", slug);
    let _ = std::fs::write(path, out);
}

/// 纯函数：把 `KEY=value` 写回 env 文本——已存在同名（未注释）行则替换，
/// 否则追加；其他行（含注释、API key）原样保留。
/// Pure helper: write `KEY=value` into env text — replace an existing (uncommented)
/// line with the same key, otherwise append; all other lines (incl. comments and API
/// keys) are preserved verbatim.
fn upsert_env_line(existing: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(existing.len() + key.len() + value.len() + 2);
    let mut written = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#')
            && trimmed.contains('=')
            && trimmed.split('=').next().unwrap_or("").trim() == key
        {
            out.push_str(&format!("{key}={value}\n"));
            written = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !written {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}
