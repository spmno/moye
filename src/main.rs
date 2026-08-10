// 程序入口：日志初始化、上下文构建、TUI 启动。
// Program entry point: logging initialization, context construction, TUI launch.
mod agent_loop;
mod cli;
mod config;
mod mcp;
mod context;
mod event;
mod evolution;
mod memory;
mod model_history;
mod providers;
mod registry;
mod reviewer;
mod sandbox;
mod skills;
mod tools;
mod tools_ext;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use cli::context::AppContext;
use evolution::prompt_evolve::PromptEvolver;
use model_history::ModelHistory;
use registry::{AgentRegistry, Orchestrator};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // 加载项目根 .env（若存在）：把供应商/API Key/模型等配置写进 .env 一次，
    // 之后无需每次启动前 export。已显式 export 的环境变量优先，不会被覆盖。
    // Loads the project-root .env (if present): provider/API key/model config can be
    // written to .env once, no need to export before every launch. Explicitly exported
    // environment variables take precedence and are never overridden.
    let env_file = dotenvy::dotenv().ok();

    init_logging();

    if let Some(path) = env_file {
        info!("[env] \u{8f7d}\u{5165}\u{4e86}\u{914d}\u{7f6e}\u{6587}\u{4ef6}: {}", path.display());
    }

    // 统一解析 agent.toml（仅此一处），各模块共享同一份配置。
    // 若 agent.toml 不存在，启动首次配置向导（交互式选择供应商、模型、输入 API key）。
    // Parse agent.toml once here; all modules share this single config.
    // If agent.toml is missing, launch the first-time setup wizard.
    if !std::path::Path::new("agent.toml").exists() {
        crate::ui::setup::run_setup().await?;
    }
    let config = crate::config::init("agent.toml")?;
    let mcp_manager = crate::mcp::McpManager::connect_all(&config.mcp).await;
    let registry = AgentRegistry::new(config.clone(), Arc::new(mcp_manager));
    let orchestrator = Orchestrator::new(registry.clone());
    let evolver = PromptEvolver::new(registry.clone(), "AGENTS.md".to_string());
    let memory = memory::MemoryStore::new(&config.memory)?;
    let rule_threshold = config.evolution.rule_escalation_threshold;

    // 加载跨会话模型历史（~/.config/my-agent/models.json）；失败时回退空历史，不阻断启动。
    // Load cross-session model history (~/.config/my-agent/models.json); fall back to empty
    // on failure without blocking startup.
    let model_history = Arc::new(Mutex::new(ModelHistory::load()));

    let ctx = Arc::new(AppContext {
        registry,
        orchestrator,
        memory,
        evolver,
        rule_threshold,
        model_history,
    });

    ui::tui::run_tui(ctx).await
}

fn init_logging() {
    use tracing_subscriber::fmt::time::LocalTime;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    std::fs::create_dir_all("logs").ok();
    let log_name = format!(
        "logs/{}.log",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let log_file = std::fs::File::create(&log_name).unwrap_or_else(|e| {
        eprintln!(
            "\u{65e0}\u{6cd5}\u{521b}\u{5efa}\u{65e5}\u{5fd7}\u{6587}\u{4ef6} {log_name}: {e}\u{ff1b}\u{56de}\u{9000}\u{5230}\u{4ec5} stderr"
        );
        std::fs::File::create("/dev/null").unwrap()
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info,rig_core=off"))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_timer(LocalTime::rfc_3339())
                .with_writer(log_file),
        )
        .init();

    info!("[trace] \u{65e5}\u{5fd7}\u{6587}\u{4ef6}: {log_name}");
}
