// 用户级会话存储模块：把「每次打开工具」抽象为一次 session。
// User-level session store: each tool invocation is a session.
//
// 每个 session 在 `<memory>/sessions/<id>/` 下持久化：
//   - `meta.json`          会话元数据（id、创建/更新时间、标题）
//   - `conversation.jsonl` 本次会话的完整对话轮次（user / agent）
// `--continue` 启动时加载最新 session，把其中对话重建为 `Vec<Message>`
// 注入 Orchestrator 历史，从而「继续」上一次的会话。
// Each session persists under `<memory>/sessions/<id>/`:
//   - `meta.json`          session metadata (id, created/updated, title)
//   - `conversation.jsonl` the full conversation turns of this session
// On `--continue` startup, the latest session is loaded and its conversation is
// rebuilt into `Vec<Message>` and injected into the Orchestrator's history.

use anyhow::Result;
use rig_core::completion::Message;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 会话子目录名（位于 memory 目录之下）。
/// Session subdirectory name (under the memory directory).
pub const SESSIONS_DIR: &str = "sessions";

/// 会话中的一轮对话（只保留角色与文本，忽略工具调用细节）。
/// One conversation turn within a session (keeps role + text, no tool-call detail).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTurn {
    /// "user" | "agent"
    pub role: String,
    pub content: String,
    /// Unix 时间戳（秒）。
    /// Unix timestamp (seconds).
    pub ts: u64,
}

/// 会话元数据，序列化到 `meta.json`。
/// Session metadata, serialized to `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    /// 创建/更新时间，Unix 纳秒，用于排序最新会话。
    /// Created/updated time, Unix nanoseconds, used to sort by recency.
    pub created_at: u128,
    pub updated_at: u128,
    #[serde(default)]
    pub title: String,
}

/// 一个用户级会话：内存中持有元数据与轮次，并同步写入磁盘。
/// A user-level session: holds metadata and turns in memory, syncing to disk.
pub struct Session {
    pub meta: SessionMeta,
    dir: PathBuf,
    turns: Vec<SessionTurn>,
}

impl Session {
    /// 从会话目录加载（读取 meta.json 与 conversation.jsonl）。
    /// Load a session from its directory (reads meta.json and conversation.jsonl).
    pub fn load(dir: &Path) -> Result<Session> {
        let meta: SessionMeta = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
        let conv_path = dir.join("conversation.jsonl");
        let turns = if conv_path.exists() {
            let raw = std::fs::read_to_string(&conv_path)?;
            raw.lines()
                .filter_map(|l| serde_json::from_str::<SessionTurn>(l).ok())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Session {
            meta,
            dir: dir.to_path_buf(),
            turns,
        })
    }

    /// 追加一轮对话：写入内存并追加到 conversation.jsonl，刷新 updated_at。
    /// Append a conversation turn: pushes to memory, appends to conversation.jsonl, refreshes updated_at.
    pub fn append_turn(&mut self, role: &str, content: &str) -> Result<()> {
        let turn = SessionTurn {
            role: role.to_string(),
            content: content.to_string(),
            ts: now_secs(),
        };
        self.turns.push(turn.clone());

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("conversation.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&turn)?)?;

        if self.meta.title.is_empty() && role == "user" {
            self.meta.title = truncate_title(content);
        }
        self.meta.updated_at = now_nanos();
        std::fs::write(
            self.dir.join("meta.json"),
            serde_json::to_string_pretty(&self.meta)?,
        )?;
        Ok(())
    }

    /// 已记录的对话轮次。
    /// The recorded conversation turns.
    pub fn turns(&self) -> &[SessionTurn] {
        &self.turns
    }

    /// 把对话轮次重建为 model-visible `Vec<Message>`（供 --continue 注入历史）。
    /// Rebuilds the turns into a model-visible `Vec<Message>` (for --continue history seeding).
    pub fn messages(&self) -> Vec<Message> {
        self.turns
            .iter()
            .map(|t| match t.role.as_str() {
                "user" => Message::user(t.content.clone()),
                _ => Message::assistant(t.content.clone()),
            })
            .collect()
    }
}

/// 会话仓库：管理 `<base>/sessions/` 下的所有会话。
/// Session store: manages all sessions under `<base>/sessions/`.
pub struct SessionStore {
    base: PathBuf,
}

impl SessionStore {
    /// 以 memory 目录为基准构造仓库（`<memory>/sessions/`）。
    /// Construct a store rooted at the memory directory (`<memory>/sessions/`).
    pub fn new(memory_dir: &Path) -> Self {
        Self {
            base: memory_dir.to_path_buf(),
        }
    }

    /// sessions 根目录路径。
    /// The sessions root directory path.
    pub fn sessions_dir(&self) -> PathBuf {
        self.base.join(SESSIONS_DIR)
    }

    /// 会话目录路径。
    /// The directory path for a given session id.
    pub fn dir_for(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id)
    }

    /// 新建一个会话（生成 id、写入 meta.json），返回可追加轮次的会话。
    /// Creates a new session (generates id, writes meta.json), returning it for appending.
    pub fn start(&self) -> Result<Session> {
        let now = chrono::Local::now();
        let nanos = now_nanos();
        let base = now.format("%Y-%m-%d_%H-%M-%S").to_string();
        let id = format!("{base}-{:06}", nanos % 1_000_000);
        let dir = self.dir_for(&id);
        std::fs::create_dir_all(&dir)?;

        let meta = SessionMeta {
            id,
            created_at: nanos,
            updated_at: nanos,
            title: String::new(),
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Session::load(&dir)
    }

    /// 列出所有会话的元数据（按 created_at 升序）。
    /// Lists metadata of all sessions (ascending by created_at).
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let root = self.sessions_dir();
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut metas = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let meta_path = entry.path().join("meta.json");
            if !meta_path.exists() {
                continue;
            }
            if let Ok(meta) =
                serde_json::from_str::<SessionMeta>(&std::fs::read_to_string(&meta_path)?)
            {
                metas.push(meta);
            }
        }
        metas.sort_by_key(|m| m.created_at);
        Ok(metas)
    }

    /// 加载最近一次会话（None 表示尚无历史会话）。
    /// Loads the most recent session (None when no session exists yet).
    pub fn latest(&self) -> Result<Option<Session>> {
        let mut metas = self.list()?;
        metas.pop().map(|m| Session::load(&self.dir_for(&m.id))).transpose()
    }
}

/// 截断标题到合理长度（不会在 UTF-8 字符中间切断）。
/// Truncates a title to a reasonable length without splitting UTF-8 characters.
fn truncate_title(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 40 {
        s.to_string()
    } else {
        let mut t = s[..s.floor_char_boundary(40)].to_string();
        t.push('\u{2026}');
        t
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "moye-session-{tag}-{}",
            now_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (SessionStore::new(&dir), dir)
    }

    #[test]
    fn start_writes_meta_and_loads_back() {
        let (store, _dir) = tmp_store("start");
        let sess = store.start().unwrap();
        assert!(sess.dir.join("meta.json").exists());
        let reloaded = Session::load(&sess.dir).unwrap();
        assert_eq!(reloaded.meta.id, sess.meta.id);
    }

    #[test]
    fn append_turn_persists_and_sets_title() {
        let (store, _dir) = tmp_store("append");
        let mut sess = store.start().unwrap();
        sess.append_turn("user", "帮我做一个历史复习工具").unwrap();
        sess.append_turn("agent", "已完成").unwrap();

        let reloaded = Session::load(&sess.dir).unwrap();
        assert_eq!(reloaded.turns().len(), 2);
        assert_eq!(reloaded.turns()[0].role, "user");
        assert_eq!(reloaded.meta.title, "帮我做一个历史复习工具");
    }

    #[test]
    fn messages_maps_user_and_agent() {
        let (store, _dir) = tmp_store("messages");
        let mut sess = store.start().unwrap();
        sess.append_turn("user", "hello").unwrap();
        sess.append_turn("agent", "hi").unwrap();

        let msgs = sess.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs,
            vec![Message::user("hello"), Message::assistant("hi")]
        );
    }

    #[test]
    fn latest_returns_newest_session() {
        let (store, _dir) = tmp_store("latest");
        let a = store.start().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = store.start().unwrap();

        let latest = store.latest().unwrap().expect("should have a latest");
        assert_eq!(latest.meta.id, b.meta.id);
        assert_ne!(b.meta.id, a.meta.id);
    }

    #[test]
    fn latest_returns_none_when_empty() {
        let (store, _dir) = tmp_store("empty");
        assert!(store.latest().unwrap().is_none());
    }

    #[test]
    fn list_skips_non_session_entries() {
        // SessionLog 也会把 `<role>-<nanos>.jsonl` 文件写进 sessions/ 目录，
        // list 必须忽略这些非目录条目（以及含 meta.json 的才算是 session）。
        let (store, _dir) = tmp_store("skip");
        store.start().unwrap();
        // 写一个普通 jsonl 文件（模拟 SessionLog）——不应被当作 session。
        std::fs::write(store.sessions_dir().join("Builder-123.jsonl"), "{}").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn title_truncation_respects_char_boundary() {
        let long = "很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长很长".to_string();
        let t = truncate_title(&long);
        assert!(t.ends_with('\u{2026}'));
        assert!(t.len() <= 40 + 3);
    }
}