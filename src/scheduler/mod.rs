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
pub mod os_cron;
pub mod store;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use self::cron::CronExpr;
use self::store::TaskStore;

/// 单个定时任务。两种触发方式二选一：
/// - `cron`：周期性触发（5 或 6 字段表达式）
/// - `at`：一次性触发（年月日时分秒），执行一次后不再触发
///
/// A single scheduled task. Exactly one of two trigger styles:
/// - `cron`: recurring (5- or 6-field expression)
/// - `at`: one-shot at an exact datetime; never fires again once executed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    /// cron 表达式（周期性任务）；at 任务为 None。
    /// Cron expression for recurring tasks; None for at-tasks.
    /// 旧版 JSON 中该字段为必填字符串，serde(default) 保证向后兼容。
    #[serde(default)]
    pub cron: Option<String>,
    /// 一次性执行时间；cron 任务为 None。
    /// One-shot execution time; None for cron tasks.
    #[serde(default)]
    pub at: Option<DateTime<Local>>,
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
/// 注意：Default 必须手动实现——整个小节缺失时 serde 走 Default::default()，
/// 派生 Default 会把 max_concurrent 置 0（所有任务永远无法派发）、mode 置空。
/// Note: Default is hand-implemented — when the whole section is absent serde
/// falls back to Default::default(), and a derived Default would zero out
/// max_concurrent (no task could ever dispatch) and blank the mode.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 调度模式 / Scheduling mode:
    ///   "os"      —— 向 OS 调度器注册每分钟心跳（crontab / Windows 任务计划），
    ///               moye 进程不在时任务照常触发；注册失败自动回退 "process"。
    ///   "process" —— 进程内循环扫描（moye 退出后任务不再触发）。
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub store_path: Option<String>,
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
}

fn default_mode() -> String { "os".to_string() }
fn default_tick_secs() -> u64 { 60 }
fn default_max_concurrent() -> usize { 2 }
fn default_log_retention_days() -> u32 { 30 }

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: default_mode(),
            tick_secs: default_tick_secs(),
            max_concurrent: default_max_concurrent(),
            store_path: None,
            log_retention_days: default_log_retention_days(),
        }
    }
}

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

    /// 心跳锁文件路径（与任务存储同目录），防 OS 心跳重入。
    /// Heartbeat lock file (next to the task store), prevents overlapping ticks.
    pub fn tick_lock(&self) -> PathBuf {
        let mut p = PathBuf::from(&self.store_path);
        p.set_file_name("scheduler_tick.lock");
        p
    }

    /// 心跳自身 stdout/stderr 的追加日志（crontab/schtasks 重定向目标）。
    /// Append target for the heartbeat's own stdout/stderr (cron/schtasks redirect).
    pub fn heartbeat_log(&self) -> PathBuf {
        let mut p = PathBuf::from(&self.store_path);
        p.set_file_name("scheduler_heartbeat.log");
        p
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
            // 一次性 at 任务：到点且从未执行过即触发（执行后 last_run 有值，自然失效）。
            // One-shot at-task: fires when due and never executed before (once
            // last_run is set it can never fire again).
            if let Some(at) = task.at {
                if task.last_run.is_none() && now >= at {
                    due.push(task.clone());
                }
                continue;
            }
            let cron_expr = match &task.cron {
                Some(c) => c,
                None => continue, // 既无 cron 也无 at 的脏数据，跳过。
            };
            let cron = match CronExpr::parse(cron_expr) {
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

    /// 单次 tick：供 OS 心跳（`moye --scheduler-tick`）调用。
    /// 派发本轮到期任务后阻塞等待其全部完成——进程退出前必须完成记账
    /// （last_run / run_count / 日志），否则下一分钟心跳会重复触发同一任务。
    /// 最多等待 MAX_TICK_WAIT，超时退出（子进程不会被杀，但记账可能丢失）。
    ///
    /// Single tick for the OS heartbeat (`moye --scheduler-tick`). After
    /// dispatching due tasks it blocks until they finish — bookkeeping must be
    /// written before the process exits, otherwise the next minute's heartbeat
    /// would re-trigger the same task. Waits at most MAX_TICK_WAIT; on timeout
    /// it exits (children survive, but their bookkeeping may be lost).
    pub async fn tick_once(&self, lock_path: &Path) -> Result<()> {
        self.tick().await?;
        let deadline = std::time::Instant::now() + MAX_TICK_WAIT;
        loop {
            if self.running.lock().await.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                warn!("[scheduler] tick_once: wait timeout, exiting (children keep running)");
                return Ok(());
            }
            // 续约锁文件 mtime：长任务运行期间，后续心跳看到新鲜锁会安静跳过，
            // 不会被误判为 stale 锁而重复派发。
            // Refresh the lock mtime: while a long task runs, later heartbeats
            // see a fresh lock and skip quietly instead of treating it as stale.
            std::fs::write(lock_path, std::process::id().to_string()).ok();
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
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

    pub fn add_task(&self, name: String, cron: Option<String>, at: Option<DateTime<Local>>, prompt: String, max_runs: Option<u64>) -> Result<ScheduledTask> {
        match (&cron, &at) {
            (Some(c), None) => { CronExpr::parse(c)?; }
            (None, Some(_)) => {}
            (Some(_), Some(_)) => bail!("cron 与 at 只能二选一（周期任务用 cron，一次性任务用 at）"),
            (None, None) => bail!("必须提供 cron（周期任务）或 at（一次性任务）之一"),
        }
        let mut store = TaskStore::load(&self.paths.store_path)?;
        let task = ScheduledTask {
            id: short_uuid(), name, cron, at, prompt,
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

// ── 心跳互斥锁 ──
// ── Heartbeat mutex ──

/// 心跳锁的 stale 阈值：锁文件 mtime 超过该时长视为上次进程异常退出（被 kill、
/// 断电等），允许强制接管。正常运行的 tick 会周期性刷新 mtime（见 tick_once）。
/// Stale threshold for the heartbeat lock: a lock file whose mtime is older
/// than this is assumed to belong to a dead process (killed, power loss, ...)
/// and may be taken over. A healthy tick refreshes the mtime (see tick_once).
pub const TICK_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(1800);

/// tick_once 等待本轮任务完成的最长时间。
/// Max time tick_once waits for its dispatched tasks to finish.
const MAX_TICK_WAIT: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 心跳互斥锁（create_new 原子占位 + mtime stale 回收），防止上一轮 tick
/// 未结束时本轮重复派发。Drop 时自动释放。
/// Heartbeat mutex (atomic create_new + mtime-based stale recovery), preventing
/// a new tick from dispatching while the previous one is still running.
/// Released automatically on drop.
pub struct TickLock {
    path: PathBuf,
}

impl TickLock {
    /// 尝试获取锁；锁被占用且未过期时返回 Ok(None)。
    /// Try to acquire; Ok(None) when a fresh lock is held by another tick.
    pub fn acquire(path: &Path, stale_after: std::time::Duration) -> Result<Option<Self>> {
        use std::fs::OpenOptions;
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let try_create = |path: &Path| -> Option<Self> {
            let mut f = OpenOptions::new().write(true).create_new(true).open(path).ok()?;
            write!(f, "{}", std::process::id()).ok();
            Some(Self { path: path.to_path_buf() })
        };
        if let Some(lock) = try_create(path) {
            return Ok(Some(lock));
        }
        // 锁已存在：未过期则放弃；过期（上次进程异常退出）则回收重试一次。
        // Lock exists: give up when fresh; reclaim + retry once when stale.
        let stale = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() > stale_after)
            .unwrap_or(false);
        if !stale {
            return Ok(None);
        }
        warn!("[scheduler] reclaiming stale tick lock: {}", path.display());
        std::fs::remove_file(path).ok();
        Ok(try_create(path))
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

// ── helpers ──

/// 解析一次性任务时间（年月日时分秒）。支持格式：
///   "2026-12-25 09:30:00" / "2026-12-25 09:30"（本地时间）
///   "2026-12-25T09:30:00" / RFC3339（带时区）
/// Parse a one-shot task datetime (year-month-day hour:minute[:second]).
/// Accepts local "YYYY-MM-DD HH:MM[:SS]" (also with a T separator) or RFC3339.
pub fn parse_at(s: &str) -> Result<DateTime<Local>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Local));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            match Local.from_local_datetime(&ndt).single() {
                Some(dt) => return Ok(dt),
                // 本地时间歧义/不存在（DST 跳变），尝试下一个格式或直接报错。
                None => bail!("本地时间 {s} 不存在或有歧义（可能是夏令时跳变点）"),
            }
        }
    }
    bail!("无法解析时间 '{s}'，支持格式：YYYY-MM-DD HH:MM[:SS] 或 RFC3339")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：整个 [scheduler] 小节缺失时走 Default::default()，必须与
    /// 小节存在但字段缺失时（serde 字段默认值）完全一致。曾经用派生
    /// Default，max_concurrent=0 导致所有任务永远无法派发。
    /// Regression: Default::default() (whole section missing) must match the
    /// serde field defaults (section present, fields missing). A derived
    /// Default used to zero max_concurrent, silently blocking all dispatch.
    #[test]
    fn default_config_matches_serde_defaults() {
        let d = SchedulerConfig::default();
        assert_eq!(d.mode, "os");
        assert_eq!(d.tick_secs, 60);
        assert_eq!(d.max_concurrent, 2);
        assert_eq!(d.log_retention_days, 30);
        assert!(!d.enabled);

        let de: SchedulerConfig = toml::from_str("").unwrap();
        assert_eq!(de.mode, "os");
        assert_eq!(de.tick_secs, 60);
        assert_eq!(de.max_concurrent, 2);
        assert_eq!(de.log_retention_days, 30);
        assert!(!de.enabled);
    }

    /// TickLock：占用时第二个 acquire 返回 None；stale 锁被回收；Drop 释放。
    #[test]
    fn tick_lock_exclusion_and_stale_reclaim() {
        let dir = std::env::temp_dir().join(format!("moye-lock-test-{}", std::process::id()));
        let lock_path = dir.join("tick.lock");
        let lock = TickLock::acquire(&lock_path, TICK_LOCK_STALE).unwrap();
        assert!(lock.is_some());
        // 未过期：第二个 acquire 失败。
        assert!(TickLock::acquire(&lock_path, TICK_LOCK_STALE).unwrap().is_none());
        // stale_after=0 → 任何存在的锁都视为过期，可回收。
        let reclaimed = TickLock::acquire(&lock_path, std::time::Duration::ZERO).unwrap();
        assert!(reclaimed.is_some());
        drop(reclaimed);
        drop(lock); // 第二个 guard 删文件无副作用（文件已被前一个 Drop 删除）。
        assert!(!lock_path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_at_formats() {
        let dt = parse_at("2026-12-25 09:30:00").unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-12-25 09:30:00");
        let dt2 = parse_at("2026-12-25 09:30").unwrap();
        assert_eq!(dt2.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-12-25 09:30:00");
        let dt3 = parse_at("2026-12-25T09:30:00").unwrap();
        assert_eq!(dt3.format("%Y-%m-%d %H:%M").to_string(), "2026-12-25 09:30");
        // RFC3339（带时区）也能解析。
        assert!(parse_at("2026-12-25T09:30:00+08:00").is_ok());
        assert!(parse_at("next friday").is_err());
        assert!(parse_at("2026-13-01 00:00").is_err());
    }

    #[test]
    fn scheduled_task_json_backcompat() {
        // 旧版存储：cron 为必填字符串、无 at 字段，升级后必须仍能反序列化。
        let old = r#"{"id":"a","name":"t","cron":"0 9 * * *","prompt":"p","created_at":"2026-01-01T00:00:00+08:00","last_run":null,"enabled":true,"max_runs":null,"run_count":0}"#;
        let t: ScheduledTask = serde_json::from_str(old).unwrap();
        assert_eq!(t.cron.as_deref(), Some("0 9 * * *"));
        assert!(t.at.is_none());
        // 新版 at 任务：cron 为 null。
        let new = r#"{"id":"b","name":"t2","cron":null,"at":"2026-12-25T09:30:00+08:00","prompt":"p","created_at":"2026-01-01T00:00:00+08:00","last_run":null,"enabled":true,"max_runs":null,"run_count":0}"#;
        let t2: ScheduledTask = serde_json::from_str(new).unwrap();
        assert!(t2.cron.is_none());
        assert!(t2.at.is_some());
    }
}
