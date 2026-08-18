// 程序入口：日志初始化、上下文构建、TUI 启动。
// Program entry point: logging initialization, context construction, TUI launch.
mod agent_loop;
mod cli;
mod config;
mod context;
mod event;
mod events;
mod evolution;
mod http_trace;
mod mcp;
mod memory;
mod model_history;
mod provider;
mod providers;
mod prompts;
mod registry;
mod reviewer;
mod sandbox;
mod seam;
mod session;
mod session_log;
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
    init_logging();

    // 统一解析 agent.toml（仅此一处），各模块共享同一份配置。
    // 若本地 agent.toml 和全局 config.toml 均不存在，启动首次配置向导。
    // Parse agent.toml once here; all modules share this single config.
    // If neither local agent.toml nor global config.toml exists, launch the setup wizard.
    if !crate::config::has_config_file() {
        crate::ui::setup::run_setup().await?;
    }

    // 加载项目根 .env（若存在）：把供应商/API Key/模型等配置写进 .env 一次，
    // 之后无需每次启动前 export。已显式 export 的环境变量优先，不会被覆盖。
    // 必须在 setup 向导之后加载——向导会写入 .env，若在此前加载则进程环境里
    // 拿不到刚配置的 API Key（例如 ARK_API_KEY），随后构建客户端会报"未设置"。
    // Loads the project-root .env (if present): provider/API key/model config can be
    // written to .env once, no need to export before every launch. Explicitly exported
    // environment variables take precedence and are never overridden.
    // Must run after the setup wizard — the wizard writes .env; loading earlier would
    // leave the just-configured API key (e.g. ARK_API_KEY) absent from the process env,
    // and the client would then report it as "not set".
    let env_file = dotenvy::dotenv().ok();

    if let Some(path) = env_file {
        info!(
            "[env] \u{8f7d}\u{5165}\u{4e86}\u{914d}\u{7f6e}\u{6587}\u{4ef6}: {}",
            path.display()
        );
    }

    let config = crate::config::init("agent.toml")?;

    // `--dump-config`：打印 profile 叠加后的组合配置树到 stdout，然后退出。
    // 用于诊断"实际生效的配置是什么"（含 profile patch 的结果）。
    // `--dump-config`: print the combined config tree (after profile overlay) to
    // stdout, then exit. A debug/introspection feature.
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--dump-config") {
        let raw = std::fs::read_to_string("agent.toml")?;
        let dump = crate::cli::context::dump_config_to_string(&raw)?;
        println!("{dump}");
        return Ok(());
    }

    // `--continue`：继续上一次会话（加载最新 session 的对话注入 Orchestrator 历史）。
    // `--continue`: resume the most recent session (inject its conversation into the
    // Orchestrator's history).
    let resume = cli_args.iter().any(|a| a == "--continue");

    let mcp_manager = crate::mcp::McpManager::connect_all(&config.mcp).await;
    // 根据 [sandbox].mode 选择 OS 级沙箱 provider（todo 8）：
    // - "landlock": 用 LandlockSandbox（bwrap 不可用时的 fallback）
    // - "off": 禁用 OS 级沙箱
    // - 其他（"auto"/"bwrap"/未知）: 用 SimpleSandbox（bwrap/seatbelt/path 后端）
    // Select the OS-level sandbox provider based on [sandbox].mode (todo 8).
    let sandbox_provider = crate::cli::context::build_sandbox_provider(&config);
    info!(
        "[sandbox] mode={} backend={}",
        config.sandbox.mode, config.sandbox.backend
    );
    let registry = AgentRegistry::new(config.clone(), Arc::new(mcp_manager), sandbox_provider);
    let orchestrator = Orchestrator::new(registry.clone());
    let evolver = PromptEvolver::new(registry.clone(), "AGENTS.md".to_string());
    let memory = memory::MemoryStore::new(&config.memory)?;
    let rule_threshold = config.evolution.rule_escalation_threshold;

    // 会话：新建或继续。--continue 时加载最新会话并恢复其对话历史。
    // Session: start fresh or resume. With --continue, load the latest session and
    // restore its conversation into the Orchestrator's history.
    let session_store = crate::session::SessionStore::new(&config.memory.dir);
    let session = match resume {
        true => match session_store.latest()? {
            Some(s) => {
                orchestrator.seed_history(s.messages());
                info!("[session] \u{7ee7}\u{7eed}\u{4f1a}\u{8bdd} / resumed session {}", s.meta.id);
                s
            }
            None => {
                info!("[session] \u{65e0}\u{53ef}\u{7ee7}\u{7eed}\u{7684}\u{4f1a}\u{8bdd}\u{ff0c}\u{5f00}\u{59cb}\u{65b0}\u{4f1a}\u{8bdd} / no session to resume, starting a new one");
                session_store.start()?
            }
        },
        false => session_store.start()?,
    };
    let session = Arc::new(Mutex::new(session));

    // 加载跨会话模型历史（~/.config/moye/models.json）；失败时回退空历史，不阻断启动。
    // Load cross-session model history (~/.config/moye/models.json); fall back to empty
    // on failure without blocking startup.
    let model_history = Arc::new(Mutex::new(ModelHistory::load()));

    let ctx = Arc::new(AppContext {
        registry,
        orchestrator,
        memory,
        evolver,
        rule_threshold,
        session,
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
