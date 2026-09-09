// 定时任务调度模块：让 moye agent 能创建、管理和自动执行定时任务。
// Scheduler module: lets the moye agent create, manage, and auto-execute scheduled tasks.

pub mod cron;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::cli::context::AppContext;
use crate::event::{AgentEvent, HitlDecision};

use self::cron::CronExpr;
use self::store::TaskStore;

/// 单个定时任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: DateTime<Local>,
    pub last_run: Option<DateTime<Local>>,
    pub enabled: bool,
    pub max_runs: Option<u64>,
    pub run_count: u64,
}

/// 任务执行结果记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLog {
    pub task_id: String,
    pub task_name: String,
    pub scheduled_at: DateTime<Local>,
    pub started_at: DateTime<Local>,
    pub finished_at: DateTime<Local>,
    pub ok: bool,
    pub output_summary: String,
}

/// 调度器配置。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub store_path: Option<String>,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
}

fn default_tick_secs() -> u64 { 60 }
fn default_max_concurrent() -> usize { 2 }
fn default_log_retention_days() -> u32 { 30 }

/// 调度器：后台 tokio 任务，定期扫描到期任务并执行。
pub struct Scheduler {
    store: Arc<Mutex<TaskStore>>,
    ctx: Arc<AppContext>,
    config: SchedulerConfig,
    running: Arc<Mutex<Vec<String>>>,
    logs: Arc<Mutex<Vec<RunLog>>>,
}

impl Scheduler {
    pub fn new(ctx: Arc<AppContext>, config: SchedulerConfig) -> Result<Self> {
        let store_path = config.store_path.clone().unwrap_or_else(|| {
            let mut p = dirs_or_default();
            p.push("scheduled_tasks.json");
            p.to_string_lossy().into_owned()
        });
        let store = TaskStore::load(&store_path)?;
        let log_p = log_path(&store_path);
        let logs = load_logs(&log_p);
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            ctx,
            config,
            running: Arc::new(Mutex::new(Vec::new())),
            logs: Arc::new(Mutex::new(logs)),
        })
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            this.run_loop().await;
        })
    }

    async fn run_loop(&self) {
        info!(
            "[scheduler] started (tick={}s, max_concurrent={})",
            self.config.tick_secs, self.config.max_concurrent
        );
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(self.config.tick_secs));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("[scheduler] tick error: {e}");
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let now = Local::now();
        let due_tasks = {
            let store = self.store.lock().await;
            let running = self.running.lock().await;
            let mut due = Vec::new();
            for task in store.tasks() {
                if !task.enabled || running.contains(&task.id) {
                    continue;
                }
                if let Some(max) = task.max_runs {
                    if task.run_count >= max {
                        continue;
                    }
                }
                let cron = match CronExpr::parse(&task.cron) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("[scheduler] task {} cron parse error: {e}", task.id);
                        continue;
                    }
                };
                let last = task.last_run.unwrap_or(task.created_at);
                if cron.has_triggered_since(&last, &now) {
                    due.push(task.clone());
                }
            }
            due
        };

        for task in due_tasks {
            {
                let running = self.running.lock().await;
                if running.len() >= self.config.max_concurrent {
                    info!("[scheduler] concurrency full ({}), skip {}", running.len(), task.id);
                    continue;
                }
            }
            self.execute_task(task).await;
        }

        self.cleanup_logs().await;
        Ok(())
    }

    async fn execute_task(&self, task: ScheduledTask) {
        let id = task.id.clone();
        let name = task.name.clone();
        let prompt = task.prompt.clone();
        let scheduled_at = Local::now();

        {
            let mut running = self.running.lock().await;
            running.push(id.clone());
        }

        info!("[scheduler] executing task: {} ({})", name, id);

        let ctx = self.ctx.clone();
        let logs = self.logs.clone();
        let store = self.store.clone();
        let running = self.running.clone();
        let log_p = self.log_path();

        tokio::spawn(async move {
            let started_at = Local::now();
            let result = run_scheduler_task(&ctx.orchestrator, &prompt).await;
            let finished_at = Local::now();
            let (ok, output_summary) = match result {
                Ok(s) => (true, truncate_summary(&s, 200)),
                Err(e) => (false, format!("error: {e}")),
            };
            info!("[scheduler] task done: {} ({}) ok={}", name, id, ok);

            {
                let mut logs_guard = logs.lock().await;
                logs_guard.push(RunLog {
                    task_id: id.clone(), task_name: name.clone(),
                    scheduled_at, started_at, finished_at, ok,
                    output_summary: output_summary.clone(),
                });
                if let Ok(json) = serde_json::to_string_pretty(&*logs_guard) {
                    let _ = std::fs::write(&log_p, json);
                }
            }
            {
                let mut store_guard = store.lock().await;
                if let Some(t) = store_guard.task_mut(&id) {
                    t.last_run = Some(finished_at);
                    t.run_count += 1;
                }
                let _ = store_guard.save();
            }
            {
                let mut running_guard = running.lock().await;
                running_guard.retain(|x| x != &id);
            }
        });
    }

    async fn cleanup_logs(&self) {
        let retention = chrono::Duration::days(self.config.log_retention_days as i64);
        let cutoff = Local::now() - retention;
        let mut logs = self.logs.lock().await;
        let before = logs.len();
        logs.retain(|l| l.finished_at > cutoff);
        if logs.len() != before {
            info!("[scheduler] cleaned {} expired logs", before - logs.len());
        }
    }

    fn log_path(&self) -> PathBuf {
        let store_path = self.config.store_path.clone().unwrap_or_else(|| {
            let mut p = dirs_or_default();
            p.push("scheduled_tasks.json");
            p.to_string_lossy().into_owned()
        });
        log_path(&store_path)
    }

    // ── Public API for tool calls ──

    pub async fn add_task(&self, name: String, cron: String, prompt: String, max_runs: Option<u64>) -> Result<ScheduledTask> {
        CronExpr::parse(&cron)?;
        let task = ScheduledTask {
            id: short_uuid(), name, cron, prompt,
            created_at: Local::now(), last_run: None,
            enabled: true, max_runs, run_count: 0,
        };
        let mut store = self.store.lock().await;
        store.add(task.clone());
        store.save()?;
        info!("[scheduler] added task: {} ({})", task.name, task.id);
        Ok(task)
    }

    pub async fn list_tasks(&self) -> Vec<ScheduledTask> {
        let store = self.store.lock().await;
        store.tasks().to_vec()
    }

    pub async fn remove_task(&self, id: &str) -> Result<bool> {
        let mut store = self.store.lock().await;
        let removed = store.remove(id);
        if removed { store.save()?; }
        Ok(removed)
    }

    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let mut store = self.store.lock().await;
        if let Some(t) = store.task_mut(id) {
            t.enabled = enabled;
            store.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn get_logs(&self, limit: usize) -> Vec<RunLog> {
        let logs = self.logs.lock().await;
        logs.iter().rev().take(limit).cloned().collect()
    }
}

/// 调度器专用任务执行：复用 Orchestrator::handle，但不打印到 stdout，
/// 而是静默收集最终输出。
async fn run_scheduler_task(orchestrator: &crate::registry::Orchestrator, prompt: &str) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    // 创建临时 Orchestrator 执行任务。
    // Create a temporary Orchestrator to execute the task.
    let registry_clone = orchestrator.clone_registry();
    let prompt_owned = prompt.to_string();
    let handle_task = tokio::spawn(async move {
        let tmp_orch = crate::registry::Orchestrator::new(registry_clone);
        tmp_orch.handle(&prompt_owned, &tx).await
    });
    let mut final_text = String::new();
    let mut ok = true;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Agent(text) => final_text = text,
            AgentEvent::Error(_) => ok = false,
            AgentEvent::HitlPrompt { responder, tool, .. } => {
                // 调度器任务无人值守，自动批准所有工具（与 headless --yes 一致）。
                // Scheduler tasks are unattended; auto-approve all tools (same as headless --yes).
                warn!("[scheduler] auto-approving tool: {tool}");
                let _ = responder.send(HitlDecision::Allow);
            }
            _ => {}
        }
    }
    match handle_task.await {
        Ok(Ok(out)) => {
            if !out.is_empty() { final_text = out; }
            if ok { Ok(final_text) } else { anyhow::bail!("orchestrator reported error") }
        }
        Ok(Err(e)) => Err(e),
        Err(e) => anyhow::bail!("scheduler task panicked: {e}"),
    }
}

fn truncate_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max { s.to_string() } else { format!("{}...", &s[..max]) }
}

fn dirs_or_default() -> PathBuf {
    if let Some(d) = dirs_config() { return d; }
    let mut p = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    p.push(".moye");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn dirs_config() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("moye");
        let _ = std::fs::create_dir_all(&p);
        return Some(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".config");
        p.push("moye");
        let _ = std::fs::create_dir_all(&p);
        return Some(p);
    }
    None
}

fn log_path(store_path: &str) -> PathBuf {
    let mut p = PathBuf::from(store_path);
    p.set_file_name("scheduler_logs.json");
    p
}

fn load_logs(path: &PathBuf) -> Vec<RunLog> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn short_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let pid = std::process::id() as u128;
    format!("{:08x}", (t ^ (pid << 48)) & 0xFFFFFFFF)
}
