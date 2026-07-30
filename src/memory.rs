// 核心记忆/经验存储模块：会话 Turn + 教训 Lesson。
// Core memory/experience store module: conversation Turn + Lesson.
// 被 AppContext（cli/context.rs）在每次任务完成后调用：
// Called by AppContext (cli/context.rs) after each task completes:
// append_turn → record_lesson。
// 教训积累后，由 PromptEvolver（evolution/prompt_evolve.rs）在 /evolve 时
// 汇总注入元 Agent 的提案提示词，驱动 benchmark 门控的提示词进化——
// 而非直接向 AGENTS.md 追加低质量自动文本。
// Lessons are aggregated by PromptEvolver during /evolve, injected into the
// meta-agent's proposal prompt to drive benchmark-gated prompt evolution—
// rather than crudely auto-appending to AGENTS.md.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一轮对话记录，以 JSONL 形式追加写入会话文件。
/// A single conversation turn, appended to the conversation file as JSONL.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Turn {
    pub role: String, // "user" | "agent"
    pub content: String,
    pub ts: u64,
}

/// 一条经验总结（任务完成后提取的可复用教训）。
/// An experience summary (a reusable lesson extracted after a task completes).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Lesson {
    pub summary: String,
    pub ts: u64,
}

/// 记忆存储的路径配置（来自 agent.toml 的 [memory] 段）。
/// Path configuration for the memory store (from the `[memory]` section of agent.toml).
#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    pub dir: PathBuf,
    pub conversation_file: String,
    pub lessons_file: String,
}

/// 记忆存储：管理会话与经验的 JSONL 文件读写。
/// Memory store: manages read/write of conversation and lesson JSONL files.
pub struct MemoryStore {
    conversation_file: PathBuf,
    lessons_file: PathBuf,
}

impl MemoryStore {
    /// 按配置初始化存储目录与文件路径。
    /// Initialize the storage directory and file paths from the given config.
    pub fn new(cfg: &MemoryConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        Ok(Self {
            conversation_file: cfg.dir.join(&cfg.conversation_file),
            lessons_file: cfg.dir.join(&cfg.lessons_file),
        })
    }

    /// 追加一轮对话到会话 JSONL 文件。
    /// Append a conversation turn to the JSONL conversation file.
    pub fn append_turn(&self, turn: &Turn) -> Result<()> {
        let line = serde_json::to_string(turn)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.conversation_file)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// 追加一条经验教训到 lessons JSONL 文件。
    /// Append a lesson to the lessons JSONL file.
    pub fn record_lesson(&self, lesson: &Lesson) -> Result<()> {
        let line = serde_json::to_string(lesson)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.lessons_file)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// 加载最近的对话轮次（JSONL 逐行反序列化）。`limit` 为 None 时返回全部，
    /// 为 Some(n) 时只返回最后 n 轮。跳过无法解析的行（不报错）。
    /// Load recent conversation turns (per-line JSONL deserialization).
    /// `limit` of None returns all; `Some(n)` returns only the last n turns.
    /// Unparseable lines are skipped silently (no error).
    pub fn load_turns(&self, limit: Option<usize>) -> Result<Vec<Turn>> {
        if !self.conversation_file.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&self.conversation_file)?;
        let turns: Vec<Turn> = raw
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let turns = match limit {
            Some(n) if n < turns.len() => turns[turns.len() - n..].to_vec(),
            _ => turns,
        };
        Ok(turns)
    }

    /// 加载所有经验教训（JSONL 逐行反序列化）。跳过无法解析的行。
    /// Load all lessons (per-line JSONL deserialization). Unparseable lines are skipped.
    pub fn load_lessons(&self) -> Result<Vec<Lesson>> {
        if !self.lessons_file.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&self.lessons_file)?;
        let lessons: Vec<Lesson> = raw
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(lessons)
    }
}
