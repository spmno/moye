// 程序入口：日志初始化、上下文构建、TUI 启动。
// Program entry point: logging initialization, context construction, TUI launch.

// 跨测试模块共享的环境变量互斥锁：所有修改 env 的测试必须先持有此锁，
// 避免并行执行时 env 操作互相干扰。
// Cross-test-module env mutex: every env-mutating test must hold this lock to
// avoid races across parallel tests in different modules.
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

mod agent_loop;
mod checkpoint;
mod cli;
mod config;
mod context;
mod event;
mod events;
mod evolution;
mod http_trace;
mod input_history;
mod mcp;
mod memory;
mod model_history;
mod provider;
mod providers;
mod prompts;
mod registry;
mod reviewer;
mod sandbox;
mod scheduler;
mod seam;
mod session;
mod session_log;
mod shell;
mod skills;
mod subagent;
mod tools;
mod tools_ext;
mod ui;
mod verify;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use cli::context::AppContext;
use evolution::prompt_evolve::PromptEvolver;
use model_history::ModelHistory;
use registry::{AgentRegistry, Orchestrator};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // --scheduler-tick / --scheduler-install / --scheduler-uninstall：
    // OS 心跳入口（crontab / Windows 任务计划每分钟触发）。必须最先拦截——
    // 心跳环境没有 TTY，不能启动配置向导；也不走会话日志（每分钟一个文件会撑爆磁盘）。
    // --scheduler-tick / --scheduler-install / --scheduler-uninstall: OS
    // heartbeat entry (fired every minute by crontab / Windows Task Scheduler).
    // Must be intercepted first — no TTY for the setup wizard in a heartbeat
    // environment, and per-session log files would flood the disk.
    {
        let raw_args: Vec<String> = std::env::args().skip(1).collect();
        if raw_args.iter().any(|a| a.starts_with("--scheduler-")) {
            return scheduler_cli_entry(&raw_args).await;
        }
    }

    init_logging();

    // --version / -V：打印版本号并退出。必须在配置向导与 .env 加载之前检查——
    // 没有配置/API Key 时也要能正常打印。
    // --version / -V: print version and exit. Must be checked before the setup
    // wizard and .env loading so it works with no config/API key present.
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("{}", version_string());
        return Ok(());
    }

    // 统一解析 agent.toml（仅此一处），各模块共享同一份配置。
    // 若项目配置（.moye/agent.toml 或旧版根 agent.toml）和全局 config.toml 均不存在，
    // 启动首次配置向导。
    // Parse agent.toml once here; all modules share this single config.
    // If neither a project config (.moye/agent.toml or the legacy root agent.toml) nor
    // the global config.toml exists, launch the setup wizard.
    if !crate::config::has_config_file() {
        crate::ui::setup::run_setup().await?;
    }

    // 旧版布局迁移：把仓库根的 agent.toml / .env 移入 .moye/（新路径已存在则跳过）。
    // 必须在 .env 加载与 config::init 之前执行。
    // Legacy-layout migration: move repo-root agent.toml / .env into .moye/ (skipped
    // when the new path already exists). Must run before .env loading and config::init.
    crate::config::migrate_legacy_config_files();

    // 加载项目 .env（.moye/.env，旧布局时回退根 .env）：把供应商/API Key/模型等配置
    // 写进 .env 一次，之后无需每次启动前 export。已显式 export 的环境变量优先，
    // 不会被覆盖。
    // 必须在 setup 向导之后加载——向导会写入 .env，若在此前加载则进程环境里
    // 拿不到刚配置的 API Key（例如 ARK_API_KEY），随后构建客户端会报“未设置”。
    // Loads the project .env (.moye/.env, falling back to the legacy root .env):
    // provider/API key/model config can be written to .env once, no need to export
    // before every launch. Explicitly exported environment variables take precedence
    // and are never overridden.
    // Must run after the setup wizard — the wizard writes .env; loading earlier would
    // leave the just-configured API key (e.g. ARK_API_KEY) absent from the process env,
    // and the client would then report it as "not set".
    let env_file = crate::config::load_dotenv();

    if let Some(path) = env_file {
        info!(
            "[env] \u{8f7d}\u{5165}\u{4e86}\u{914d}\u{7f6e}\u{6587}\u{4ef6}: {}",
            path.display()
        );
    }

    let config = crate::config::init(crate::config::PROJECT_CONFIG_PATH)?;

    // `--dump-config`：打印 profile 叠加后的组合配置树到 stdout，然后退出。
    // 用于诊断"实际生效的配置是什么"（含 profile patch 的结果）。
    // `--dump-config`: print the combined config tree (after profile overlay) to
    // stdout, then exit. A debug/introspection feature.
    let cli_args: Vec<String> = std::env::args().collect();

    // 无头模式参数解析（-p/--print）。无 -p 时返回 Ok(None)，走 TUI 路径。
    // 解析错误（缺值/未知格式）→ stderr + 退出码 2，在昂贵初始化前尽早失败。
    // Headless arg parsing (-p/--print). Returns Ok(None) when -p is absent → TUI path.
    // Parse errors (missing value / unknown format) → stderr + exit 2, failing fast
    // before the expensive init.
    let headless = match cli::headless::parse_headless_args(&cli_args) {
        Ok(None) => None,
        Ok(Some(h)) => Some(h),
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    if cli_args.iter().any(|a| a == "--dump-config") {
        let raw = std::fs::read_to_string(crate::config::PROJECT_CONFIG_PATH)?;
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

    // 启动定时任务调度器（仅当 [scheduler].enabled = true 时）。
    // Start the scheduler (only when [scheduler].enabled = true).
    let _scheduler_handle: Option<tokio::task::JoinHandle<()>> = if config.scheduler.enabled {
        // 创建 TaskManager 并注入 registry，供 schedule_task 工具使用。
        let mgr = crate::scheduler::TaskManager::from_config(&config.scheduler);
        ctx.registry.set_scheduler_mgr(mgr.clone());

        // 进程内循环（mode = "process"，或 os 模式注册失败时的回退）。
        // In-process loop (mode = "process", or the fallback when OS
        // registration fails).
        fn spawn_process_loop(
            config: &crate::scheduler::SchedulerConfig,
        ) -> Option<tokio::task::JoinHandle<()>> {
            match crate::scheduler::Scheduler::new(config.clone()) {
                Ok(sched) => {
                    info!("[scheduler] in-process loop started (tick={}s)", config.tick_secs);
                    Some(Arc::new(sched).spawn())
                }
                Err(e) => {
                    tracing::warn!("[scheduler] failed to initialize: {e}");
                    None
                }
            }
        }

        match config.scheduler.mode.as_str() {
            // os 模式：向 OS 调度器注册每分钟心跳，moye 退出后任务照常触发。
            // 幂等——心跳内容未变化时不改写 crontab/任务计划。
            // os mode: register a per-minute heartbeat with the OS scheduler so
            // tasks fire even after moye exits. Idempotent — the OS entry is
            // not rewritten when its content is unchanged.
            "os" => {
                let hb = crate::scheduler::os_cron::Heartbeat {
                    workdir: std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from(".")),
                    binary: std::env::current_exe()
                        .unwrap_or_else(|_| std::path::PathBuf::from("moye")),
                    log_path: crate::scheduler::SchedulerPaths::from_config(&config.scheduler)
                        .heartbeat_log(),
                };
                match hb.install() {
                    Ok(msg) => {
                        info!("[scheduler] os mode: {msg}");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[scheduler] OS heartbeat registration failed: {e}; \
                             falling back to in-process loop (mode = \"process\")"
                        );
                        spawn_process_loop(&config.scheduler)
                    }
                }
            }
            // process 模式：进程内循环（历史行为）。
            // process mode: in-process loop (legacy behavior).
            _ => spawn_process_loop(&config.scheduler),
        }
    } else {
        info!("[scheduler] disabled (set [scheduler].enabled = true in agent.toml to enable)");
        None
    };

    // 无头模式派发：-p 存在时运行无头路径并退出，不进入 TUI。
    // --continue + -p 允许组合：--continue 恢复上一次会话的上下文（seed_history），
    // -p 在该上下文中无头执行任务。对脚本化"继续上次工作"的场景有用。
    // Headless dispatch: when -p is present, run the headless path and exit (no TUI).
    // --continue + -p is allowed: --continue resumes the previous session's context
    // (seed_history), and -p runs the task headlessly in that context. Useful for
    // scripted "continue prior work" scenarios.
    if let Some(hargs) = headless {
        let code = match cli::headless::run_headless(
            ctx,
            &hargs.prompt,
            hargs.format,
            hargs.auto_yes,
        )
        .await
        {
            Ok(code) => code,
            Err(e) => {
                eprintln!("headless error: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    ui::tui::run_tui(ctx).await
}

/// OS 心跳 CLI 入口：--scheduler-tick / --scheduler-install / --scheduler-uninstall。
/// 刻意保持轻量：不初始化日志文件/MCP/沙箱/TUI，只做调度相关动作。
/// stdout/stderr 由 OS 调度器重定向到 heartbeat 日志（见 os_cron）。
///
/// OS heartbeat CLI entry: --scheduler-tick / --scheduler-install /
/// --scheduler-uninstall. Deliberately lightweight: no session log file, no
/// MCP/sandbox/TUI init — just scheduler actions. stdout/stderr are redirected
/// to the heartbeat log by the OS scheduler entry (see os_cron).
async fn scheduler_cli_entry(args: &[String]) -> Result<()> {
    // 心跳环境跑交互式配置向导没有意义，直接报错（错误进 heartbeat 日志）。
    // Running the interactive setup wizard from a heartbeat makes no sense;
    // fail instead (the error lands in the heartbeat log).
    // 旧版布局迁移（与主入口一致）：心跳可能在旧布局目录中运行。
    // Legacy-layout migration (same as the main entry): a heartbeat may run in a
    // directory still using the old layout.
    crate::config::migrate_legacy_config_files();
    if !crate::config::has_config_file() {
        anyhow::bail!(
            "no .moye/agent.toml or global config found; run moye interactively once to set up"
        );
    }
    // 加载 .env：tick 派生的子进程（moye -p ...）通过环境继承 API Key。
    // Load .env: children spawned by the tick (moye -p ...) inherit API keys
    // from this process environment.
    crate::config::load_dotenv();
    let config = crate::config::init(crate::config::PROJECT_CONFIG_PATH)?;

    let paths = crate::scheduler::SchedulerPaths::from_config(&config.scheduler);
    let make_heartbeat = || crate::scheduler::os_cron::Heartbeat {
        workdir: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        binary: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("moye")),
        log_path: paths.heartbeat_log(),
    };

    if args.iter().any(|a| a == "--scheduler-install") {
        let hb = make_heartbeat();
        println!("{}", hb.install()?);
        return Ok(());
    }
    if args.iter().any(|a| a == "--scheduler-uninstall") {
        let hb = make_heartbeat();
        println!("{}", hb.uninstall()?);
        return Ok(());
    }

    // 默认动作：--scheduler-tick（由 crontab / schtasks 每分钟调用）。
    // Default action: --scheduler-tick (invoked every minute by crontab / schtasks).
    let lock_path = paths.tick_lock();
    let Some(_guard) = crate::scheduler::TickLock::acquire(
        &lock_path,
        crate::scheduler::TICK_LOCK_STALE,
    )?
    else {
        // 上一轮 tick 还在跑（任务执行超过一分钟），本轮安静跳过。
        // The previous tick is still running (task longer than a minute);
        // skip this round quietly.
        println!("[scheduler-tick] previous tick still running, skipped");
        return Ok(());
    };
    let sched = crate::scheduler::Scheduler::new(config.scheduler.clone())?;
    sched.tick_once(&lock_path).await
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

/// 版本字符串：`moye <semver>`，版本号编译期从 Cargo.toml 注入，避免两处维护。
/// Version string: `moye <semver>` — injected from Cargo.toml at compile time
/// so the version lives in exactly one place.
fn version_string() -> String {
    format!("moye {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_string_matches_cargo_toml() {
        let v = super::version_string();
        assert_eq!(v, format!("moye {}", env!("CARGO_PKG_VERSION")));
        let semver = v.trim_start_matches("moye ");
        assert_eq!(semver.split('.').count(), 3, "expected semver x.y.z");
    }
}
