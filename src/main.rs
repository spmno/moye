// 程序入口（REPL）：加载配置、构建各角色 Agent，并在交互循环中处理元命令与
// 自然语言目标（目标会被交给自主循环 agent_loop 执行）。
mod memory;
mod registry;
mod providers;
mod reviewer;
mod tools;
mod evolution;
mod agent_loop;
mod skills;

use anyhow::Result;
use evolution::{
    prompt_evolve::PromptEvolver,
    self_modify,
    tool_ext,
};
use registry::{AgentRegistryConfig, AgentRegistry, Orchestrator};
use reviewer::ReviewGate;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // 日志同时输出到终端和按时间命名的文件（logs/YYYY-MM-DD_HH-MM-SS.log）。
    // 使用 tracing-subscriber 的 layer 组合：stdout layer + file layer。
    {
        use tracing_subscriber::fmt::time::LocalTime;
        std::fs::create_dir_all("logs")?;
        let log_name = format!(
            "logs/{}.log",
            chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
        );
        let log_file = std::fs::File::create(&log_name)?;
        let timer = LocalTime::rfc_3339();
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_timer(timer.clone())
            .with_writer(log_file);
        let stdout_layer = tracing_subscriber::fmt::layer()
            .with_timer(timer)
            .with_writer(std::io::stdout);
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("info,rig_core=debug"))
            .with(file_layer)
            .with(stdout_layer)
            .init();
        info!("[trace] 日志文件: {log_name}");
    }

    let reg_cfg = AgentRegistryConfig::load("agent.toml")?;
    #[allow(unused)]
    let threshold = load_escalation_threshold()?; // reserved for future review-gated mode
    let registry = AgentRegistry::new(reg_cfg);
    #[allow(unused)]
    let orchestrator = Orchestrator::new(registry.clone()); // reserved for one-shot `chat` mode
    #[allow(unused)]
    let reviewer = ReviewGate::new(registry.clone()); // reserved for review-gated mode
    let evolver = PromptEvolver::new(registry.clone(), "AGENTS.md".to_string());

    let mem = memory::MemoryStore::new(&load_memory_cfg()?)?;

    let provider_name = format!("{:?}", providers::current_provider());
    info!(
        "my-agent ready ({provider_name}). model: {}\nCommands: model <slug> | evolve | evolve-code | add-tool | add-skill | skills | quit",
        current_model(&registry)
    );

    // rustyline 提供正确的 UTF-8 / IME 行编辑：退格删除一个字符（而非一个字节），
    // ↑/↓ 历史记录可用，中文等输入能正确组合。
    let mut rl = rustyline::DefaultEditor::new()?;
    loop {
        let read = rl.readline("> ");
        let line = match read {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        rl.add_history_entry(line.as_str())?;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "quit" {
            break;
        }

        // REPL 元命令。顺序很关键："evolve-code"/"add-tool" 必须在 "evolve" 前缀之前
        // 检查，因为 "evolve-code" 以 "evolve" 开头。"model" 是独立命令。
        if let Some(rest) = input.strip_prefix("model") {
            let slug = rest.trim();
            if slug.is_empty() {
                info!("current model: {}", current_model(&registry));
                continue;
            }
            registry.set_session_model(slug);
            info!("model set to: {slug} (applies to all roles this session)");
            continue;
        }
        if let Some(rest) = input.strip_prefix("evolve-code") {
            let rest = rest.trim();
            let parts: Vec<String> = rest
                .splitn(3, ' ')
                .map(|p| p.trim_matches('"').to_string())
                .collect();
            if parts.len() < 3 {
                warn!("usage: evolve-code <file> <old_text> <new_text>");
                continue;
            }
            match self_modify::evolve_code(&parts[0], &parts[1], &parts[2]) {
                Ok(msg) => info!("{msg}"),
                Err(e) => error!("evolve-code error: {e}"),
            }
            continue;
        }
        if let Some(rest) = input.strip_prefix("add-tool") {
            let rest = rest.trim();
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() < 2 {
                warn!("usage: add-tool <name> <description>");
                continue;
            }
            match tool_ext::add_tool(parts[0], parts[1]) {
                Ok(msg) => info!("{msg}"),
                Err(e) => error!("add-tool error: {e}"),
            }
            continue;
        }
        // 新增技能：写 skills/<name>.md 并登记进清单（无需重新编译）。
        if let Some(rest) = input.strip_prefix("add-skill") {
            let rest = rest.trim();
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() < 2 {
                warn!("usage: add-skill <name> <description>");
                continue;
            }
            let body = format!("# {}\n\n{}\n\n(Describe the step-by-step instructions here.)\n", parts[0], parts[1]);
            match skills::add_skill(parts[0], parts[1], &body) {
                Ok(msg) => info!("{msg}"),
                Err(e) => error!("add-skill error: {e}"),
            }
            continue;
        }
        // 列出所有已注册技能。
        if input == "skills" {
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
            continue;
        }
        if input.strip_prefix("evolve").is_some() {
            match evolver.evolve().await {
                Ok(msg) => info!("{msg}"),
                Err(e) => error!("evolve error: {e}"),
            }
            continue;
        }

        // 任何非元命令的输入都是自主循环的目标。循环运行 Builder 角色，由其自行规划
        // 并调用工具；仅当工具处于"需询问"(ask) 分级时，人才会被暂停确认。
        match agent_loop::run_autonomous(&registry, input).await {
            Ok(out) => {
                info!("{out}");
                mem.append_turn(&memory::Turn {
                    role: "user".into(),
                    content: input.into(),
                    ts: now(),
                })?;
                mem.append_turn(&memory::Turn {
                    role: "agent".into(),
                    content: out,
                    ts: now(),
                })?;
            }
            Err(e) => {
                error!("autonomous loop error: {e}");
                error!("  detail: {e:?}");
            },
        }
    }
    Ok(())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_memory_cfg() -> Result<memory::MemoryConfig> {
    let raw = std::fs::read_to_string("agent.toml")?;
    let parsed: toml::Value = toml::from_str(&raw)?;
    let m = parsed
        .get("memory")
        .ok_or_else(|| anyhow::anyhow!("missing [memory] in agent.toml"))?;
    let cfg: memory::MemoryConfig = m.clone().try_into()?;
    Ok(cfg)
}

fn load_escalation_threshold() -> Result<u32> {
    let raw = std::fs::read_to_string("agent.toml")?;
    let parsed: toml::Value = toml::from_str(&raw)?;
    let t = parsed
        .get("evolution")
        .and_then(|e| e.get("rule_escalation_threshold"))
        .and_then(|v| v.as_integer())
        .unwrap_or(3);
    Ok(t as u32)
}

/// 解析默认模型名称。优先级：
/// 1. 环境变量 MY_AGENT_MODEL（最高）
/// 2. agent.toml 的 [agent].default_model
/// 3. 硬编码回退值
fn resolve_default_model() -> String {
    std::env::var("MY_AGENT_MODEL")
        .ok()
        .or_else(|| load_default_model())
        .unwrap_or_else(|| "deepseek-v4-pro".to_string())
}

fn current_model(registry: &registry::AgentRegistry) -> String {
    match registry.session_model() {
        Some(m) => m,
        None => resolve_default_model(),
    }
}

fn load_default_model() -> Option<String> {
    let raw = std::fs::read_to_string("agent.toml").ok()?;
    let parsed: toml::Value = toml::from_str(&raw).ok()?;
    parsed
        .get("agent")
        .and_then(|a| a.get("default_model"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

