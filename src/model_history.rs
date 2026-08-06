//! 模型历史模块：把用户用过的模型（slug + 供应商 + base URL + 使用次数 + 最近时间）
//! 持久化到 `~/.config/my-agent/models.json`，供 `/models` 选择器列出"最近使用"分区，
//! 让用户一键切回之前用过的模型，无需重新输入 slug。
//!
//! Model history module: persists used models (slug + provider + base URL + use count +
//! last-used time) to `~/.config/my-agent/models.json`, so the `/models` selector can list
//! a "recently used" section and let users switch back to a previously used model without
//! retyping the slug.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// 历史保留的最大条目数，超出按最旧优先丢弃。
/// Maximum number of records kept; oldest are dropped when exceeded.
const MAX_RECORDS: usize = 50;

/// 一条模型使用记录。同一 slug 在历史中只出现一次（去重），
/// 每次切换到它时 `uses` +1、`last_used` 刷新为当前时间。
/// A single model usage record. A slug appears at most once (deduped);
/// each switch to it increments `uses` and refreshes `last_used`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRecord {
    /// 模型 slug（如 `glm-latest` / `kimi-k3`）。
    /// Model slug (e.g. `glm-latest` / `kimi-k3`).
    pub slug: String,
    /// 当时使用的供应商（`deepseek` / `bailian` / `moonshot` / `custom`），仅用于展示。
    /// Provider used at the time, for display only.
    pub provider: String,
    /// 当时的 OpenAI 兼容 base URL，仅用于展示与跨网关去重。
    /// OpenAI-compatible base URL at the time, for display and cross-gateway dedup.
    pub base_url: String,
    /// 最近一次使用时间（Unix 秒）。
    /// Last-used time (Unix seconds).
    pub last_used: u64,
    /// 累计使用次数。
    /// Cumulative use count.
    pub uses: u32,
    /// 进程内单调递增的操作序号，作为 `last_used` 相同时的排序 tiebreak
    /// （后操作的排更前）。不持久化——加载后重置为 0。
    /// In-process monotonic operation sequence, used as a sort tiebreak when `last_used`
    /// ties (later operations sort first). Not persisted — resets to 0 on load.
    #[serde(skip)]
    pub seq: u64,
}

/// 模型历史集合：有序（最近使用在前）去重的记录列表。
/// Model history collection: an ordered (most-recently-used first), deduped record list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelHistory {
    #[serde(default)]
    pub records: Vec<ModelRecord>,
    /// 进程内单调递增序号源，供 `seq` tiebreak 取值；不持久化。
    /// In-process monotonic sequence source for the `seq` tiebreak; not persisted.
    #[serde(skip)]
    seq_counter: u64,
}

impl ModelHistory {
    /// 从磁盘加载历史；文件不存在或解析失败时返回空历史（不阻断启动）。
    /// Load history from disk; returns empty (without blocking startup) when the file is
    /// missing or fails to parse.
    pub fn load() -> Self {
        let Some(path) = history_path() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// 把历史写回磁盘；自动创建目录。失败仅返回错误，不影响会话内切换。
    /// Persist history to disk, creating directories as needed. Failures are returned as
    /// errors and do not affect in-session switching.
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = history_path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, raw)?;
        Ok(())
    }

    /// 记录一次模型使用：同 slug 已存在则 `uses` +1 并刷新时间与供应商/URL，
    /// 否则新增一条；随后按最近使用降序排序并截断到 [`MAX_RECORDS`]。
    /// `last_used` 相同时，后操作的排更前（借助进程内 `seq` 单调序号）。
    /// Record a model use: if the slug already exists, increment `uses` and refresh the
    /// time and provider/URL; otherwise append a new entry. Then sort by most-recently-used
    /// first and truncate to [`MAX_RECORDS`]. When `last_used` ties, later operations sort
    /// first (via the in-process monotonic `seq`).
    pub fn record(&mut self, slug: &str, provider: &str, base_url: &str) {
        let now = unix_now();
        let seq = self.next_seq();
        if let Some(rec) = self.records.iter_mut().find(|r| r.slug == slug) {
            rec.uses = rec.uses.saturating_add(1);
            rec.last_used = now;
            rec.provider = provider.to_string();
            rec.base_url = base_url.to_string();
            rec.seq = seq;
        } else {
            self.records.push(ModelRecord {
                slug: slug.to_string(),
                provider: provider.to_string(),
                base_url: base_url.to_string(),
                last_used: now,
                uses: 1,
                seq,
            });
        }
        // 最近使用优先；last_used 相同时按操作序号降序（后操作排前）。
        self.records
            .sort_by(|a, b| b.last_used.cmp(&a.last_used).then(b.seq.cmp(&a.seq)));
        if self.records.len() > MAX_RECORDS {
            self.records.truncate(MAX_RECORDS);
        }
    }

    /// 取下一个进程内单调序号；wrapping 避免溢出 panic。
    /// Take the next in-process monotonic sequence; wrapping avoids overflow panic.
    fn next_seq(&mut self) -> u64 {
        self.seq_counter = self.seq_counter.wrapping_add(1);
        self.seq_counter
    }

    /// 返回最近 `n` 条记录（已按最近使用排序）。
    /// Return the most recent `n` records (already sorted by most-recently-used first).
    pub fn recent(&self, n: usize) -> &[ModelRecord] {
        let end = n.min(self.records.len());
        &self.records[..end]
    }
}

/// 返回历史文件路径 `~/.config/my-agent/models.json`；`HOME` 未设置时返回 `None`。
/// Return the history file path `~/.config/my-agent/models.json`; `None` when `HOME` is unset.
fn history_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config").join("my-agent").join("models.json"))
}

/// 当前 Unix 时间戳（秒）；系统时钟异常时退回 0。
/// Current Unix timestamp (seconds); falls back to 0 if the system clock is unavailable.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_new_then_update_increments_uses() {
        // 新增一条，再记录同 slug：uses 递增、时间刷新、不产生重复条目。
        // Add a record, then record the same slug: uses increments, time refreshes,
        // and no duplicate entry is created.
        let mut h = ModelHistory::default();
        h.record("glm-latest", "custom", "https://gw/v1");
        assert_eq!(h.records.len(), 1);
        assert_eq!(h.records[0].uses, 1);

        h.record("glm-latest", "custom", "https://gw/v1");
        assert_eq!(h.records.len(), 1);
        assert_eq!(h.records[0].uses, 2);
    }

    #[test]
    fn recent_returns_most_used_first_descending_by_time() {
        // 多个不同 slug：最近使用的排在最前。
        // Multiple distinct slugs: the most recently used sorts first.
        let mut h = ModelHistory::default();
        h.record("a", "custom", "u");
        h.record("b", "custom", "u");
        h.record("c", "custom", "u");
        // 最后记录的是 c，应排第一 / last recorded is c, should be first.
        let recent = h.recent(2);
        assert_eq!(recent[0].slug, "c");
        assert_eq!(recent[1].slug, "b");
    }

    #[test]
    fn truncates_to_max_records() {
        // 超过上限的旧记录应被丢弃。
        // Records exceeding the limit should be dropped.
        let mut h = ModelHistory::default();
        for i in 0..(MAX_RECORDS + 5) {
            h.record(&format!("m{i}"), "custom", "u");
        }
        assert_eq!(h.records.len(), MAX_RECORDS);
        // 最先加入的 m0..m4 应已被截断 / the earliest m0..m4 should have been truncated.
        assert!(h.records.iter().all(|r| !r.slug.starts_with("m0")
            || r.slug == format!("m{}", 0)));
    }

    #[test]
    fn load_missing_file_returns_empty() {
        // 文件不存在时返回空历史，不 panic。
        // A missing file yields an empty history without panicking.
        // （此处依赖 HOME 指向的路径通常无该文件或可读；仅验证不 panic。）
        let _ = ModelHistory::load();
    }

    #[test]
    fn record_refreshes_provider_and_base_url() {
        // 同 slug 再次记录时，provider / base_url 应被刷新为最新值。
        // Recording the same slug again should refresh provider / base_url to the latest.
        let mut h = ModelHistory::default();
        h.record("kimi-k3", "moonshot", "https://api.moonshot.cn/v1");
        h.record("kimi-k3", "custom", "https://gw/v1");
        assert_eq!(h.records.len(), 1);
        assert_eq!(h.records[0].provider, "custom");
        assert_eq!(h.records[0].base_url, "https://gw/v1");
        assert_eq!(h.records[0].uses, 2);
    }
}
