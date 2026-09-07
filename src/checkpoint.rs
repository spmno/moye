//! 文件检查点模块：在每次任务开始前快照文件状态，支持 `/rewind` 回滚到任务前状态。
//! File checkpoint module: snapshots file state before each task, supports `/rewind`
//! to restore the pre-task state of all files a task touched.
//!
//! v1 限制（会话级内存，不持久化到磁盘）：
//! v1 limitations (session-scoped in-memory, no disk persistence):
//! - 检查点仅存在于内存，进程退出即丢失。持久化是后续工作。
//!   Checkpoints are in-memory only; lost on process exit. Persistence is future work.
//! - run_bash 产生的副作用不可追踪，不做检查点。
//!   Side effects from run_bash are not trackable; not checkpointed.
//! - 任务外的文件变更（current_task == 0）不做检查点——record 是 no-op。
//!   Mutations outside a task (current_task == 0) are not checkpointed — record is a no-op.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 单个文件的检查点：记录任务前该文件的内容（None = 文件不存在）。
/// A single file checkpoint: records the file's pre-task content
/// (None = file did not exist).
#[derive(Debug, Clone)]
pub struct FileCheckpoint {
    pub path: String,
    pub before: Option<String>,
}

/// 回滚单个文件的结果。
/// Outcome of rewinding a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindOutcome {
    /// 文件被恢复为任务前的内容（before=Some → 写回）。
    /// File restored to pre-task content (before=Some → written back).
    Restored,
    /// 文件在任务前不存在，已被删除（before=None → remove_file）。
    /// File did not exist before the task; has been deleted (before=None → remove_file).
    Deleted,
}

/// 会话级检查点存储：按任务 ID 分组记录每个文件被首次修改前的状态。
/// Session-scoped checkpoint store: records each file's pre-mutation state,
/// grouped by task ID.
///
/// `current` 是单调递增的任务计数器：`begin_task()` 递增，工具调用时
/// `current_task()` 读取当前值。`record()` 对同一任务内同一路径只记录首次
/// （first-write-wins 语义）。
pub struct CheckpointStore {
    tasks: Mutex<BTreeMap<u64, Vec<FileCheckpoint>>>,
    current: AtomicU64,
}

impl CheckpointStore {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(BTreeMap::new()),
            current: AtomicU64::new(0),
        }
    }

    /// 开始一个新任务：递增计数器并返回新任务 ID。在 `handle()` 顶部调用。
    /// Begin a new task: increments the counter and returns the new task ID.
    /// Called at the top of `handle()`.
    pub fn begin_task(&self) -> u64 {
        let id = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        id
    }

    /// 当前任务 ID（0 = 无活跃任务）。
    /// Current task ID (0 = no active task).
    pub fn current_task(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// 记录文件检查点：同一任务内同一路径只记录首次（first-write-wins）。
    /// Record a file checkpoint: first-write-wins for the same path within a task.
    ///
    /// `before` = 写入前的文件内容（Some=已存在的文件内容，None=文件不存在）。
    /// 当 `task == 0`（无活跃任务）时为 no-op。
    /// `before` = file content before the write (Some=existing file, None=file absent).
    /// No-op when `task == 0` (no active task).
    pub fn record(&self, task: u64, path: &str, before: Option<String>) {
        if task == 0 {
            return;
        }
        let mut tasks = self.tasks.lock().unwrap();
        let checkpoints = tasks.entry(task).or_default();
        // first-write-wins: skip if path already recorded for this task
        if checkpoints.iter().any(|c| c.path == path) {
            return;
        }
        checkpoints.push(FileCheckpoint {
            path: path.to_string(),
            before,
        });
    }

    /// 列出有检查点的任务（newest-first），返回 (task_id, file_count)。
    /// List tasks that have checkpoints (newest-first), as (task_id, file_count).
    pub fn tasks_with_files(&self) -> Vec<(u64, usize)> {
        let tasks = self.tasks.lock().unwrap();
        let mut result: Vec<(u64, usize)> =
            tasks.iter().map(|(id, cps)| (*id, cps.len())).collect();
        result.sort_by(|a, b| b.0.cmp(&a.0));
        result
    }

    /// 返回某任务触碰的文件路径列表。
    /// Return the list of file paths a task touched.
    pub fn task_paths(&self, task: u64) -> Vec<String> {
        let tasks = self.tasks.lock().unwrap();
        tasks
            .get(&task)
            .map(|cps| cps.iter().map(|c| c.path.clone()).collect())
            .unwrap_or_default()
    }

    /// 回滚某任务：对每个检查点，先快照当前内容到当前任务（undo 检查点），
    /// 再恢复 `before`（Some→写回，None→删除文件）。返回每个文件的结果。
    ///
    /// Rewind a task: for each checkpoint, first snapshot current content into
    /// the current task (undo checkpoint), then restore `before`
    /// (Some→write back, None→delete file). Returns per-file outcomes.
    pub fn rewind_task(&self, task: u64) -> Vec<(String, RewindOutcome)> {
        let checkpoints = {
            let tasks = self.tasks.lock().unwrap();
            tasks.get(&task).cloned().unwrap_or_default()
        };
        let current = self.current_task();
        let mut outcomes = Vec::new();
        for cp in &checkpoints {
            // 先快照当前内容到当前任务（undo 检查点），first-write-wins。
            // Snapshot current content into the current task first (undo checkpoint).
            let current_content = std::fs::read_to_string(&cp.path).ok();
            if current > 0 {
                self.record(current, &cp.path, current_content);
            }
            // 恢复 / Restore
            match &cp.before {
                Some(content) => {
                    let _ = std::fs::write(&cp.path, content);
                    outcomes.push((cp.path.clone(), RewindOutcome::Restored));
                }
                None => {
                    let _ = std::fs::remove_file(&cp.path);
                    outcomes.push((cp.path.clone(), RewindOutcome::Deleted));
                }
            }
        }
        outcomes
    }
}

impl Default for CheckpointStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 辅助：创建临时文件路径（唯一）。
    /// Helper: create a unique temp file path.
    fn tmp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("moye_checkpoint_test_{}_{}", name, std::process::id()));
        p.to_string_lossy().to_string()
    }

    /// 辅助：写入临时文件内容。
    /// Helper: write content to a temp file.
    fn write_file(path: &str, content: &str) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::File::create(path).unwrap();
        let _ = f.write_all(content.as_bytes());
    }

    /// 辅助：读取临时文件内容。
    /// Helper: read temp file content.
    fn read_file(path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    /// 辅助：清理临时文件。
    /// Helper: cleanup temp file.
    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    // ── begin_task: 单调递增 ──

    #[test]
    fn begin_task_monotonic_ids() {
        let store = CheckpointStore::new();
        let a = store.begin_task();
        let b = store.begin_task();
        let c = store.begin_task();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
        assert_eq!(store.current_task(), 3);
    }

    // ── record: first-write-wins ──

    #[test]
    fn record_first_write_wins() {
        let store = CheckpointStore::new();
        let task = store.begin_task();
        let path = tmp_path("fww");

        write_file(&path, "original");
        store.record(task, &path, Some("original".to_string()));

        // Second record with different before — should NOT overwrite.
        store.record(task, &path, Some("WRONG".to_string()));

        let paths = store.task_paths(task);
        assert_eq!(paths.len(), 1);

        // Rewind should restore "original", not "WRONG".
        write_file(&path, "modified");
        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(read_file(&path), Some("original".to_string()));

        cleanup(&path);
    }

    // ── record: before=None for missing file ──

    #[test]
    fn record_before_none_for_missing_file() {
        let store = CheckpointStore::new();
        let task = store.begin_task();
        let path = tmp_path("missing");

        // File doesn't exist → before=None
        store.record(task, &path, None);

        // Create the file (simulating write_file creating it)
        write_file(&path, "created");
        assert!(read_file(&path).is_some());

        // Rewind should delete the file
        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, RewindOutcome::Deleted);
        assert!(read_file(&path).is_none());

        cleanup(&path);
    }

    // ── record: per-task isolation ──

    #[test]
    fn record_per_task_isolation() {
        let store = CheckpointStore::new();
        let task1 = store.begin_task();
        let path = tmp_path("isolation");

        write_file(&path, "v1");
        store.record(task1, &path, Some("v1".to_string()));
        write_file(&path, "v2");

        let task2 = store.begin_task();
        store.record(task2, &path, Some("v2".to_string()));
        write_file(&path, "v3");

        // Rewind task2 → restore "v2"
        let outcomes = store.rewind_task(task2);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(read_file(&path), Some("v2".to_string()));

        // Rewind task1 → restore "v1"
        let outcomes = store.rewind_task(task1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(read_file(&path), Some("v1".to_string()));

        cleanup(&path);
    }

    // ── record: no-op when task == 0 ──

    #[test]
    fn record_noop_when_task_zero() {
        let store = CheckpointStore::new();
        // No begin_task called → current_task == 0
        store.record(0, "/some/path", Some("content".to_string()));
        assert!(store.tasks_with_files().is_empty());
    }

    // ── tasks_with_files: newest-first ordering ──

    #[test]
    fn tasks_with_files_newest_first() {
        let store = CheckpointStore::new();
        let t1 = store.begin_task();
        let p1 = tmp_path("twf1");
        write_file(&p1, "x");
        store.record(t1, &p1, Some("x".to_string()));

        let t2 = store.begin_task();
        let p2 = tmp_path("twf2");
        write_file(&p2, "y");
        store.record(t2, &p2, Some("y".to_string()));

        let t3 = store.begin_task();
        let p3 = tmp_path("twf3");
        write_file(&p3, "z");
        store.record(t3, &p3, Some("z".to_string()));

        let tasks = store.tasks_with_files();
        assert_eq!(tasks, vec![(t3, 1), (t2, 1), (t1, 1)]);

        cleanup(&p1);
        cleanup(&p2);
        cleanup(&p3);
    }

    // ── tasks_with_files: counts correct ──

    #[test]
    fn tasks_with_files_counts() {
        let store = CheckpointStore::new();
        let t = store.begin_task();
        let p1 = tmp_path("cnt1");
        let p2 = tmp_path("cnt2");
        let p3 = tmp_path("cnt3");

        write_file(&p1, "a");
        write_file(&p2, "b");
        write_file(&p3, "c");
        store.record(t, &p1, Some("a".to_string()));
        store.record(t, &p2, Some("b".to_string()));
        store.record(t, &p3, Some("c".to_string()));

        let tasks = store.tasks_with_files();
        assert_eq!(tasks, vec![(t, 3)]);

        cleanup(&p1);
        cleanup(&p2);
        cleanup(&p3);
    }

    // ── rewind_task: 修改的文件字节精确恢复 ──

    #[test]
    fn rewind_restores_modified_file_byte_exact() {
        let store = CheckpointStore::new();
        let task = store.begin_task();
        let path = tmp_path("restore_exact");

        let original = "line1\nline2\nline3\n中文内容\n";
        write_file(&path, original);
        store.record(task, &path, Some(original.to_string()));

        let modified = "MODIFIED\ncontent\n";
        write_file(&path, modified);

        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, RewindOutcome::Restored);
        assert_eq!(read_file(&path), Some(original.to_string()));

        cleanup(&path);
    }

    // ── rewind_task: 创建的文件被删除 ──

    #[test]
    fn rewind_deletes_created_file() {
        let store = CheckpointStore::new();
        let task = store.begin_task();
        let path = tmp_path("delete_created");

        // File doesn't exist before → before=None
        store.record(task, &path, None);
        // Simulate write_file creating it
        write_file(&path, "new content");
        assert!(read_file(&path).is_some());

        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, RewindOutcome::Deleted);
        assert!(read_file(&path).is_none());
    }

    // ── rewind_task: undo-of-undo works (rewind twice = back to rewound state) ──

    #[test]
    fn rewind_undo_of_undo_works() {
        let store = CheckpointStore::new();
        let task1 = store.begin_task();
        let path = tmp_path("undo_undo");

        // Task 1: file goes from "original" to "modified"
        let original = "original";
        write_file(&path, original);
        store.record(task1, &path, Some(original.to_string()));
        write_file(&path, "modified");

        // Task 2 (current): rewind task1 → restores "original", but snapshots "modified" first
        let task2 = store.begin_task();
        let outcomes = store.rewind_task(task1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(read_file(&path), Some(original.to_string()));

        // Verify undo checkpoint was created under task2 (current)
        let paths = store.task_paths(task2);
        assert_eq!(paths.len(), 1, "undo checkpoint should exist under current task");

        // Rewind task2 (undo the rewind) → should restore "modified"
        let outcomes = store.rewind_task(task2);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(read_file(&path), Some("modified".to_string()));

        cleanup(&path);
    }

    // ── rewind_task: zero files → empty outcomes ──

    #[test]
    fn rewind_zero_files_empty_outcomes() {
        let store = CheckpointStore::new();
        let task = store.begin_task();
        // No record calls → no checkpoints
        let outcomes = store.rewind_task(task);
        assert!(outcomes.is_empty());
    }

    // ── rewind_task: non-existent task → empty outcomes ──

    #[test]
    fn rewind_nonexistent_task_empty() {
        let store = CheckpointStore::new();
        let outcomes = store.rewind_task(999);
        assert!(outcomes.is_empty());
    }
}
