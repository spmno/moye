// 核心记忆/经验存储模块：会话 Turn + 教训 Lesson + 规则 Rule。
// Core memory/experience store module: conversation Turn + Lesson + Rule.
// 被 AppContext（cli/context.rs）在每次任务完成后调用：
// Called by AppContext (cli/context.rs) after each task completes:
// append_turn → record_lesson → observe_rule → promote_rule_to_agents_md。

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

/// 一条被反复观察到的行为规则，count 达到阈值后会被提升进 AGENTS.md。
/// A repeatedly observed behavior rule; once `count` reaches the threshold it is promoted into AGENTS.md.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Rule {
    pub text: String,
    pub count: u32,
    pub written_to: bool,
}

/// 记忆存储的路径配置（来自 agent.toml 的 [memory] 段）。
/// Path configuration for the memory store (from the `[memory]` section of agent.toml).
#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    pub dir: PathBuf,
    pub conversation_file: String,
    pub lessons_file: String,
    pub rules_file: String,
}

/// 记忆存储：管理会话、经验、规则的 JSONL / JSON 文件读写。
/// Memory store: manages read/write of conversation, lesson, and rule JSONL/JSON files.
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

    /// 记录一条被观察到的行为规则。返回 true 表示它刚刚跨过升级阈值，
    /// 应当被提升写入 AGENTS.md。
    /// Record an observed behavior rule. Returns true if it just crossed the escalation threshold
    /// and should be promoted into AGENTS.md.
    pub fn observe_rule(&self, text: &str, threshold: u32) -> Result<bool> {
        let mut rules = self.load_rules()?;
        let existing = rules.iter().position(|r| r.text == text);
        match existing {
            Some(idx) => {
                rules[idx].count += 1;
                let crossed = rules[idx].count >= threshold && !rules[idx].written_to;
                if crossed {
                    rules[idx].written_to = true;
                }
                self.save_rules(&rules)?;
                Ok(crossed)
            }
            None => {
                rules.push(Rule {
                    text: text.to_string(),
                    count: 1,
                    written_to: false,
                });
                let crossed = 1 >= threshold;
                if crossed {
                    rules.last_mut().unwrap().written_to = true;
                }
                self.save_rules(&rules)?;
                Ok(crossed)
            }
        }
    }

    /// 加载所有规则；规则文件不存在时返回空 Vec。
    /// Load all rules; returns an empty Vec when the rules file does not exist.
    pub fn load_rules(&self) -> Result<Vec<Rule>> {
        if !self.rules_file.exists() {
            return Ok(vec![]);
        }
        let raw = std::fs::read_to_string(&self.rules_file)?;
        let rules: Vec<Rule> = serde_json::from_str(&raw)?;
        Ok(rules)
    }

    /// 把已升级的规则追加到 AGENTS.md，使其成为持久的行为指令。
    /// 返回实际写入的新段落文本。
    /// Append an escalated rule to AGENTS.md so it becomes a persistent behavior directive.
    /// Returns the section text that was actually written.
    pub fn promote_rule_to_agents_md(&self, rule: &str, path: &str) -> Result<String> {
        let section = format!("\n## Escalated rule\n- {rule}\n");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{section}")?;
        Ok(section)
    }

    /// 持久化全部规则到 rules JSON 文件（pretty-printed）。
    /// Persist all rules to the rules JSON file (pretty-printed).
    fn save_rules(&self, rules: &[Rule]) -> Result<()> {
        let raw = serde_json::to_string_pretty(rules)?;
        std::fs::write(&self.rules_file, raw)?;
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
