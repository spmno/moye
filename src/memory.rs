// 核心记忆/经验存储模块：会话 Turn + 教训 Lesson + 规则 Rule。
// 被 AppContext（cli/context.rs）在每次任务完成后调用：
// append_turn → record_lesson → observe_rule → promote_rule_to_agents_md。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 一轮对话记录，以 JSONL 形式追加写入会话文件。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Turn {
    pub role: String, // "user" | "agent"
    pub content: String,
    pub ts: u64,
}

/// 一条经验总结（任务完成后提取的可复用教训）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Lesson {
    pub summary: String,
    pub ts: u64,
}

/// 一条被反复观察到的行为规则，count 达到阈值后会被提升进 AGENTS.md。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Rule {
    pub text: String,
    pub count: u32,
    pub written_to: bool,
}

/// 记忆存储的路径配置（来自 agent.toml 的 [memory] 段）。
#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    pub dir: PathBuf,
    pub conversation_file: String,
    pub lessons_file: String,
    pub rules_file: String,
}

/// 记忆存储：管理会话、经验、规则的 JSONL / JSON 文件读写。
pub struct MemoryStore {
    conversation_file: PathBuf,
    lessons_file: PathBuf,
    rules_file: PathBuf,
}

impl MemoryStore {
    /// 按配置初始化存储目录与文件路径。
    pub fn new(cfg: &MemoryConfig) -> Result<Self> {
        std::fs::create_dir_all(&cfg.dir)?;
        Ok(Self {
            conversation_file: cfg.dir.join(&cfg.conversation_file),
            lessons_file: cfg.dir.join(&cfg.lessons_file),
            rules_file: cfg.dir.join(&cfg.rules_file),
        })
    }

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

    fn save_rules(&self, rules: &[Rule]) -> Result<()> {
        let raw = serde_json::to_string_pretty(rules)?;
        std::fs::write(&self.rules_file, raw)?;
        Ok(())
    }
}

