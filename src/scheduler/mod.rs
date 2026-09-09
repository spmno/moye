// 定时任务调度模块：后台循环扫描到期任务，通过子进程调用 `moye -p "prompt"` 执行。
// Scheduler module: background loop scans due tasks, executes them via child
// process `moye -p "prompt"`.
//
// 架构要点 / Architecture notes:
//   - 调度器与 agent 运行时完全解耦，不持有 AppContext/Orchestrator 引用。
//   - 任务执行通过 spawn 子进程复用 headless 模式（天然支持 --yes 自动批准）。
//   - 工具层（ScheduleTask）直接读写 JSON 文件，不依赖 Scheduler 实例。
//   - 每个定时任务在独立进程中运行，互不污染主会话状态。

pub mod cron;
pub mod store;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

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

/// 调度器配置（对应 agent.toml 的 [scheduler] 小节）。
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

/// 解析后的调度器路径信息。
#[derive(Clone)]
pub struct SchedulerPaths {
    pub store_path: String,
    pub log_path: PathBuf,
}

impl SchedulerPaths {
    pub fn from_config(config: &SchedulerConfig) -> Self {
        let store_path = config.store_path.clone().unwrap_or_else(|| {
            let mut p = dirs_or_default();
            p.push("scheduled_tasks.json");
            p.to_string_lossy().into_owned()
        });
        let log_path = log_path(&store_path);
        Self { store_path, log_path }
    }
}

/// 后台调度器：纯循环，不依赖 agent 运行时。
/// 每次 tick 从文件重新加载任务列表，保证与工具层（TaskManager）通过文件系统同步。
pub struct Scheduler {
    config: SchedulerConfig,
    paths: SchedulerPaths,
    running: Arc<Mutex<Vec<String>>>,
    /// 当前 moye 可执行文件路径（用于 spawn 子进程）。
    binary_path: PathBuf,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Result<Self> {
        let paths = SchedulerPaths::from_config(&config);
        let binary_path = std::env::current_exe()?;
        Ok(Scheduler { config, paths, running: Arc::new(Mutex::new(Vec::new())), binary_path })
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move { this.run_loop().await })
    }

    async fn run_loop(&self) {
        info!(
            "[scheduler] started (tick={}s, max_concurrent={}, binary={})",
            self.config.tick_secs,
            self.config.max_concurrent,
            self.binary_path.display()
        );
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(self.config.tick_secs));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("[scheduler] tick error: {e}");
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let now = Local::now();
        // 每次 tick 从文件重新加载，保证与工具层通过文件系统同步。
        // Reload from file each tick to stay in sync with tool-layer writes.
        let store = TaskStore::load(&self.paths.store_path)?;
        let running = self.running.lock().await;
        let mut due = Vec::new();
        for task in store.tasks() {
            if !task.enabled || running.contains(&task.id) { continue; }
            if let Some(max) = task.max_runs {
                if task.run_count >= max { continue; }
            }
            let cron = match CronExpr::parse(&task.cron) {
                Ok(c) => c,
                Err(e) => { warn!("[scheduler] task {} cron parse error: {e}", task.id); continue; }
            };
            let last = task.last_run.unwrap_or(task.created_at);
            if cron.has_triggered_since(&last, &now) {
                due.push(task.clone());
            }
        }
        drop(running);

        for task in due {
            {
                let running = self.running.lock().await;
                if running.len() >= self.config.max_concurrent {
                    info!("[scheduler] concurrency full ({}), skip {}", running.len(), task.id);
                    continue;
                }
            }
            self.execute_task(task).await;
        }
        Ok(())
    }

    async fn execute_task(&self, task: ScheduledTask) {
        let id = task.id.clone();
        let name = task.name.clone();
        let prompt = task.prompt.clone();
        let scheduled_at = Local::now();
        let bin = self.binary_path.clone();
        let store_path = self.paths.store_path.clone();
        let log_path = self.paths.log_path.clone();
        let retention = self.config.log_retention_days;

        let running = self.running.clone();

        {
            let mut r = running.lock().await;
            r.push(id.clone());
        }

        info!("[scheduler] executing task: {} ({})", name, id);

        tokio::spawn(async move {
            let started_at = Local::now();
            // 子进程执行：moye -p "prompt" --yes
            let result = tokio::process::Command::new(&bin)
                .args(["-p", &prompt, "--yes"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await;

            let finished_at = Local::now();
            let (ok, output_summary) = match result {
                Ok(output) => {
                    let code_ok = output.status.success();
                    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let summary = if code_ok {
                        truncate_summary(&stdout, 200)
                    } else {
                        let err_part = truncate_summary(&stderr, 150);
                        format!("exit {:?}: {}", output.status.code(), err_part)
                    };
                    (code_ok, summary)
                }
                Err(e) => (false, format!("spawn error: {e}")),
            };
            info!("[scheduler] task done: {} ({}) ok={}", name, id, ok);

            // 更新日志（从文件加载 → 追加 → 写回）
            let mut logs = load_logs(&log_path);
            logs.push(RunLog {
                task_id: id.clone(), task_name: name.clone(),
                scheduled_at, started_at, finished_at, ok,
                output_summary: output_summary.clone(),
            });
            let cutoff = Local::now() - chrono::Duration::days(retention as i64);
            logs.retain(|l| l.finished_at > cutoff);
            if let Ok(json) = serde_json::to_string_pretty(&logs) {
                let _ = std::fs::write(&log_path, json);
            }

            // 更新任务状态（从文件加载 → 修改 → 写回）
            if let Ok(mut store) = TaskStore::load(&store_path) {
                if let Some(t) = store.task_mut(&id) {
                    t.last_run = Some(finished_at);
                    t.run_count += 1;
                }
                let _ = store.save();
            }

            {
                let mut r = running.lock().await;
                r.retain(|x| x != &id);
            }
        });
    }
}

// ── 工具层直接读写文件，不依赖 Scheduler 实例 ──
// ── Tool layer reads/writes files directly, no Scheduler instance needed ──

/// 工具层使用的任务管理器：直接操作 JSON 文件。
/// Task manager used by the tool layer: operates directly on the JSON file.
#[derive(Clone)]
pub struct TaskManager {
    paths: SchedulerPaths,
}

impl TaskManager {
    pub fn from_config(config: &SchedulerConfig) -> Self {
        Self { paths: SchedulerPaths::from_config(config) }
    }

    pub fn add_task(&self, name: String, cron: String, prompt: String, max_runs: Option<u64>) -> Result<ScheduledTask> {
        CronExpr::parse(&cron)?;
        let mut store = TaskStore::load(&self.paths.store_path)?;
        let task = ScheduledTask {
            id: short_uuid(), name, cron, prompt,
            created_at: Local::now(), last_run: None,
            enabled: true, max_runs, run_count: 0,
        };
        store.add(task.clone());
        store.save()?;
        info!("[scheduler] added task: {} ({})", task.name, task.id);
        Ok(task)
    }

    pub fn list_tasks(&self) -> Result<Vec<ScheduledTask>> {
        let store = TaskStore::load(&self.paths.store_path)?;
        Ok(store.tasks().to_vec())
    }

    pub fn remove_task(&self, id: &str) -> Result<bool> {
        let mut store = TaskStore::load(&self.paths.store_path)?;
        let removed = store.remove(id);
        if removed { store.save()?; }
        Ok(removed)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let mut store = TaskStore::load(&self.paths.store_path)?;
        if let Some(t) = store.task_mut(id) {
            t.enabled = enabled;
            store.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn get_logs(&self, limit: usize) -> Vec<RunLog> {
        load_logs(&self.paths.log_path)
            .into_iter()
            .rev()
            .take(limit)
            .collect()
    }
}

// ── helpers ──

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
