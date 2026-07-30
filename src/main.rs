// 程序入口：日志初始化、上下文构建、TUI 启动。
// Program entry point: logging initialization, context construction, TUI launch.
mod agent_loop;
mod cli;
mod context;
mod event;
mod evolution;
mod memory;
mod providers;
mod registry;
mod reviewer;
mod sandbox;
mod skills;
mod tools;
mod tools_ext;
mod ui;

use std::sync::Arc;

use anyhow::Result;
use cli::context::AppContext;
use evolution::prompt_evolve::PromptEvolver;
use registry::{AgentRegistry, AgentRegistryConfig, Orchestrator};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let reg_cfg = AgentRegistryConfig::load("agent.toml")?;
    let registry = AgentRegistry::new(reg_cfg);
    let orchestrator = Orchestrator::new(registry.clone());
    let evolver = PromptEvolver::new(registry.clone(), "AGENTS.md".to_string());
    let memory = memory::MemoryStore::new(&cli::context::load_memory_cfg()?)?;
    let rule_threshold = cli::context::load_escalation_threshold()?;

    let ctx = Arc::new(AppContext {
        registry,
        orchestrator,
        memory,
        evolver,
        rule_threshold,
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
