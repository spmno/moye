//! 输入历史模块：把用户提交过的输入持久化到 `~/.config/moye/input_history.json`，
//! 跨会话恢复。Up/Down 浏览历史时从这份持久化历史加载，提交新输入时追加并写回。
//!
//! Input history module: persists submitted inputs to
//! `~/.config/moye/input_history.json`, restored across sessions. Up/Down
//! browsing seeds from this persisted history; new submissions append and save.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 历史保留的最大条目数，超出按最旧优先丢弃。
/// Maximum number of entries kept; oldest are dropped when exceeded.
const MAX_RECORDS: usize = 200;

/// 输入历史集合：有序（最旧在前）的字符串列表。JSON 存储使内嵌 `\n` 安全
/// （serde 会转义），因此多行输入可原样持久化与恢复。
/// Input history collection: an ordered (oldest-first) list of strings. JSON
/// storage makes embedded `\n` safe (serde escapes it), so multiline inputs
/// persist and restore byte-exactly.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct InputHistory {
    #[serde(default)]
    pub entries: Vec<String>,
}

impl InputHistory {
    /// 记录一条输入：跳过与最近一条相同的连续重复；超出上限时丢弃最旧的。
    /// Record an input: skip consecutive duplicates; drop oldest when over the cap.
    pub fn record(&mut self, entry: String) {
        // 连续重复抑制：与最后一条相同则跳过。
        // Consecutive-dup suppression: skip when equal to the current last entry.
        if self.entries.last().is_some_and(|last| last == &entry) {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > MAX_RECORDS {
            let excess = self.entries.len() - MAX_RECORDS;
            self.entries.drain(0..excess);
        }
    }

    /// 从指定路径加载；文件不存在或解析失败时返回空历史（不阻断启动）。
    /// Load from the given path; returns empty (without blocking startup) when
    /// the file is missing or fails to parse.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 把历史写回指定路径；自动创建目录。失败仅返回错误。
    /// Persist history to the given path, creating directories as needed.
    /// Failures are returned as errors.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    /// 从默认路径（`~/.config/moye/input_history.json`）加载；HOME 未设置或
    /// 文件不存在/损坏时返回空历史。
    /// Load from the default path (`~/.config/moye/input_history.json`); returns
    /// empty when HOME is unset or the file is missing/corrupt.
    pub fn load() -> Self {
        let Some(path) = input_history_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// 把历史写回默认路径。失败仅返回错误，不影响会话。
    /// Persist history to the default path. Failures are returned as errors
    /// and do not affect the session.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = input_history_path() else {
            return Ok(());
        };
        self.save_to(&path)
    }
}

/// 返回历史文件路径 `~/.config/moye/input_history.json`；HOME 未设置时返回 None。
/// Return the history file path `~/.config/moye/input_history.json`; None when
/// HOME is unset. Mirrors `model_history::history_path` resolution.
fn input_history_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("moye")
            .join("input_history.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_pushs_entry() {
        // 记录一条新输入：追加到列表末尾。
        // Record a new input: appended to the end.
        let mut h = InputHistory::default();
        h.record("hello".into());
        assert_eq!(h.entries, vec!["hello"]);
    }

    #[test]
    fn record_skips_consecutive_duplicate() {
        // 与最近一条相同则跳过（连续重复抑制）。
        // Skip when equal to the last entry (consecutive-dup suppression).
        let mut h = InputHistory::default();
        h.record("hello".into());
        h.record("hello".into());
        assert_eq!(h.entries, vec!["hello"]);
    }

    #[test]
    fn record_keeps_non_consecutive_duplicate() {
        // 非连续重复保留（中间有不同条目）。
        // Non-consecutive duplicates are kept (different entry in between).
        let mut h = InputHistory::default();
        h.record("hello".into());
        h.record("world".into());
        h.record("hello".into());
        assert_eq!(h.entries, vec!["hello", "world", "hello"]);
    }

    #[test]
    fn record_caps_at_max_dropping_oldest() {
        // 超出 MAX_RECORDS 时从前面丢弃最旧的。
        // Drop oldest from the front when exceeding MAX_RECORDS.
        let mut h = InputHistory::default();
        for i in 0..(MAX_RECORDS + 10) {
            h.record(format!("entry-{i}"));
        }
        assert_eq!(h.entries.len(), MAX_RECORDS);
        // 最旧的 10 条被丢弃 / oldest 10 dropped
        assert_eq!(h.entries[0], format!("entry-10"));
        assert_eq!(
            h.entries.last().unwrap(),
            &format!("entry-{}", MAX_RECORDS + 9)
        );
    }

    #[test]
    fn save_load_roundtrip_preserves_multiline() {
        // 多行输入（含 \n、emoji、tab）经 save→load 后逐字节一致。
        // Multiline inputs (with \n, emoji, tab) survive save→load byte-exactly.
        let tmp = std::env::temp_dir().join("moye_input_history_roundtrip.json");
        let _ = std::fs::remove_file(&tmp);

        let mut h = InputHistory::default();
        h.record("line1\nline2\nline3".into());
        h.record("emoji \u{1f600} test".into());
        h.record("tab\tchar".into());
        h.save_to(&tmp).unwrap();

        let loaded = InputHistory::load_from(&tmp);
        assert_eq!(loaded.entries, h.entries);
        // 内嵌换行原样保留 / embedded newlines preserved
        assert_eq!(loaded.entries[0], "line1\nline2\nline3");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn corrupt_json_returns_default() {
        // 损坏的 JSON 文件不应 panic，返回空历史。
        // Corrupt JSON must not panic; returns empty history.
        let tmp = std::env::temp_dir().join("moye_input_history_corrupt.json");
        std::fs::write(&tmp, "{{{{not json").unwrap();
        let h = InputHistory::load_from(&tmp);
        assert!(h.entries.is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn missing_file_returns_default() {
        // 文件不存在时返回空历史。
        // Missing file returns empty history.
        let path = std::env::temp_dir().join("moye_input_history_nonexistent.json");
        let _ = std::fs::remove_file(&path);
        let h = InputHistory::load_from(&path);
        assert!(h.entries.is_empty());
    }
}
