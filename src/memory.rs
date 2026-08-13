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

/// 一条从反复出现的教训自动提升来的规则。
/// A rule auto-escalated from a lesson that recurred N times.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Rule {
    pub text: String,
    pub count: usize,
    pub created_ts: u64,
}

/// 记忆存储的路径配置（来自 agent.toml 的 [memory] 段）。
/// Path configuration for the memory store (from the `[memory]` section of agent.toml).
#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    #[serde(default)]
    pub dir: PathBuf,
    #[serde(default)]
    pub conversation_file: String,
    #[serde(default)]
    pub lessons_file: String,
    #[serde(default = "default_rules_file")]
    pub rules_file: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::default(),
            conversation_file: String::default(),
            lessons_file: String::default(),
            rules_file: default_rules_file(),
        }
    }
}

fn default_rules_file() -> String {
    "rules.json".to_string()
}

/// 记忆存储：管理会话与经验的 JSONL 文件读写。
/// Memory store: manages read/write of conversation and lesson JSONL files.
pub struct MemoryStore {
    conversation_file: PathBuf,
    lessons_file: PathBuf,
    rules_file: PathBuf,
}

impl MemoryStore {
    /// 按配置初始化存储目录与文件路径。
    /// Initialize the storage directory and file paths from the given config.
    pub fn new(cfg: &MemoryConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        Ok(Self {
            conversation_file: cfg.dir.join(&cfg.conversation_file),
            lessons_file: cfg.dir.join(&cfg.lessons_file),
            rules_file: cfg.dir.join(&cfg.rules_file),
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

    /// 加载所有已提升的规则（JSON 反序列化）。文件不存在或为空时返回空列表。
    /// Load all escalated rules (JSON deserialization). Returns empty list if file missing or empty.
    pub fn load_rules(&self) -> Result<Vec<Rule>> {
        if !self.rules_file.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&self.rules_file)?;
        if raw.trim().is_empty() {
            return Ok(vec![]);
        }
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    /// 检查一条教训是否反复出现 `threshold` 次；达到则提升为规则并写入 rules.json。
    /// threshold 为 0 时禁用（返回 None）；已提升过的教训不重复提升。
    pub fn check_and_escalate_rule(
        &self,
        lesson: &Lesson,
        threshold: usize,
    ) -> Result<Option<Rule>> {
        if threshold == 0 {
            return Ok(None);
        }
        let lessons = self.load_lessons()?;
        let count = lessons.iter().filter(|l| l.summary == lesson.summary).count();
        if count < threshold {
            return Ok(None);
        }
        let rules = self.load_rules()?;
        if rules.iter().any(|r| r.text == lesson.summary) {
            return Ok(None);
        }
        let rule = Rule {
            text: lesson.summary.clone(),
            count,
            created_ts: lesson.ts,
        };
        let mut rules = rules;
        rules.push(rule.clone());
        let json = serde_json::to_string_pretty(&rules)?;
        std::fs::write(&self.rules_file, json)?;
        Ok(Some(rule))
    }
}

/// 从文件路径加载规则列表（供 registry build 注入 preamble）。
/// 读取失败或文件不存在时返回空列表。
/// Load rules from a file path (for registry build to inject into preamble).
/// Returns empty list on read failure or missing file.
pub fn load_rules_from_file(path: &std::path::Path) -> Vec<Rule> {
    if !path.exists() {
        return vec![];
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    if raw.trim().is_empty() {
        return vec![];
    }
    serde_json::from_str(&raw).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!(
            "moye-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = MemoryConfig {
            dir,
            conversation_file: "conv.jsonl".into(),
            lessons_file: "lessons.jsonl".into(),
            rules_file: "rules.json".into(),
        };
        MemoryStore::new(&cfg).unwrap()
    }

    #[test]
    fn escalate_below_threshold_returns_none() {
        let store = tmp_store("below");
        let lesson = Lesson { summary: "always commit after edit".into(), ts: 1 };
        store.record_lesson(&lesson).unwrap();
        assert!(store.check_and_escalate_rule(&lesson, 3).unwrap().is_none());
    }

    #[test]
    fn escalate_at_threshold_creates_rule() {
        let store = tmp_store("at");
        let lesson = Lesson { summary: "always commit after edit".into(), ts: 1 };
        for _ in 0..3 {
            store.record_lesson(&lesson).unwrap();
        }
        let rule = store.check_and_escalate_rule(&lesson, 3).unwrap();
        assert!(rule.is_some());
        let rule = rule.unwrap();
        assert_eq!(rule.text, lesson.summary);
        assert_eq!(rule.count, 3);
        assert_eq!(store.load_rules().unwrap().len(), 1);
    }

    #[test]
    fn escalate_does_not_duplicate() {
        let store = tmp_store("dup");
        let lesson = Lesson { summary: "always commit after edit".into(), ts: 1 };
        for _ in 0..3 {
            store.record_lesson(&lesson).unwrap();
        }
        assert!(store.check_and_escalate_rule(&lesson, 3).unwrap().is_some());
        assert!(store.check_and_escalate_rule(&lesson, 3).unwrap().is_none());
        assert_eq!(store.load_rules().unwrap().len(), 1);
    }

    #[test]
    fn escalate_threshold_zero_disabled() {
        let store = tmp_store("zero");
        let lesson = Lesson { summary: "always commit after edit".into(), ts: 1 };
        store.record_lesson(&lesson).unwrap();
        assert!(store.check_and_escalate_rule(&lesson, 0).unwrap().is_none());
    }

    #[test]
    fn load_rules_empty_when_no_file() {
        let store = tmp_store("empty");
        assert!(store.load_rules().unwrap().is_empty());
    }

    #[test]
    fn load_rules_from_file_missing_returns_empty() {
        let path = std::env::temp_dir().join("moye-nonexistent-rules.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_rules_from_file(&path).is_empty());
    }

    #[test]
    fn load_rules_from_file_parses_json() {
        let dir = std::env::temp_dir().join(format!(
            "moye-test-rf-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        let rules = vec![
            Rule { text: "rule a".into(), count: 3, created_ts: 1 },
            Rule { text: "rule b".into(), count: 5, created_ts: 2 },
        ];
        std::fs::write(&path, serde_json::to_string_pretty(&rules).unwrap()).unwrap();
        let loaded = load_rules_from_file(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text, "rule a");
        assert_eq!(loaded[1].count, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
