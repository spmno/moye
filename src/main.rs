mod agent_loop;
mod cli;
mod evolution;
mod memory;
mod providers;
mod registry;
mod reviewer;
mod skills;
mod tools;
mod tools_ext;

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

    let ctx = AppContext {
        registry,
        orchestrator,
        memory,
        evolver,
        rule_threshold,
    };

    let provider_name = format!("{:?}", providers::current_provider());
    info!(
        "my-agent ready ({provider_name}). model: {}\n\
         命令: /model <slug> | /evolve | /evolve-code <file> <old> <new> | /add-tool <name> <desc> | /add-skill <name> <desc> | /skills | /history [n] | /lessons | /help | /quit\n\
         非 `/` 开头的输入会作为任务目标交给 Orchestrator（SDD 管线）执行。",
        ctx.current_model()
    );

    cli::repl::run_repl(&ctx).await
}

fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::fmt::time::LocalTime;

    std::fs::create_dir_all("logs").ok();
    let log_name = format!(
        "logs/{}.log",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );
    let log_file = std::fs::File::create(&log_name).unwrap_or_else(|e| {
        eprintln!("无法创建日志文件 {log_name}: {e}；回退到仅 stdout");
        std::fs::File::create("/dev/null").unwrap()
    });
    let timer = LocalTime::rfc_3339();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_timer(timer.clone())
        .with_writer(log_file);
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_timer(timer)
        .with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info,rig_core=off"))
        .with(file_layer)
        .with(stdout_layer)
        .init();

    info!("[trace] 日志文件: {log_name}");
}
