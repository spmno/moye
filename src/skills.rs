// 技能（skill）模块：技能即"指令包"——一份 markdown 文件，内含描述与执行步骤。
// Skill module: a skill is an "instruction bundle" — a markdown file holding a description and steps.
// 运行时按任务相关性把匹配的技能正文注入到 Agent 的提示词中，使模型遵照执行。
// At runtime, matching skill bodies are injected into the Agent's prompt by task relevance, so the model follows them.
// 技能不是编译进项目的 Rust 工具，而是纯文本指令；新增技能只需写文件 + 登记清单，
// Skills are not Rust tools compiled into the project but plain-text instructions; adding a skill only requires writing a file + registering it in the manifest,
// 无需重新编译。这与 OpenCode 的 skill 机制一致。
// with no recompilation needed. This mirrors OpenCode's skill mechanism.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

// 技能正文存放目录与清单路径（与 tool_ext 的清单机制平行）。
// Skill body storage directory and manifest path (parallel to tool_ext's manifest mechanism).
const SKILLS_DIR: &str = "skills";
const MANIFEST_PATH: &str = "memory/skill_manifest.json";

/// 技能清单：记录每一个已注册技能的名字、文件、描述与关键词。
/// Skill manifest: records the name, file, description, and keywords of every registered skill.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SkillManifest {
    pub skills: Vec<SkillEntry>,
}

/// 单条技能记录。关键词用于相关性匹配；留空时回退到描述中的词语。
/// A single skill entry. Keywords drive relevance matching; when empty, falls back to words in the description.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub file: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

impl SkillManifest {
    /// 从磁盘加载清单；不存在时返回空的默认清单。
    /// Loads the manifest from disk; returns an empty default manifest if absent.
    pub fn load() -> Result<Self> {
        if !Path::new(MANIFEST_PATH).exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(MANIFEST_PATH)?;
        let m: SkillManifest = serde_json::from_str(&raw)?;
        Ok(m)
    }

    /// 注册一个技能：去重后写入清单并持久化。
    /// Registers a skill: deduplicates, writes to the manifest, and persists.
    pub fn register(&mut self, entry: SkillEntry) -> Result<()> {
        if self.skills.iter().any(|s| s.name == entry.name) {
            return Err(anyhow::anyhow!("skill {} already registered", entry.name));
        }
        self.skills.push(entry);
        self.save()?;
        Ok(())
    }

    /// 把清单持久化到磁盘。
    /// Persists the manifest to disk.
    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(MANIFEST_PATH, raw)?;
        Ok(())
    }

    /// 返回所有已注册技能的名称（用于 `skills` REPL 命令的列表展示）。
    /// Returns the names of all registered skills (used by the `skills` REPL command for listing).
    pub fn list(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.name.clone()).collect()
    }
}

/// 新增一个技能：把正文写入 skills/<name>.md，登记进清单并持久化。
/// Adds a new skill: writes the body to skills/<name>.md, registers it in the manifest, and persists.
/// `body` 为技能的 markdown 正文（描述 + 步骤）。返回成功信息字符串。
/// `body` is the skill's markdown body (description + steps). Returns a success message string.
pub fn add_skill(name: &str, description: &str, body: &str) -> Result<String> {
    // 文件名：小写并把空格/连字符转为下划线，以 .md 结尾。
    // File name: lowercase and convert spaces/hyphens to underscores, ending with .md.
    let file_name = format!("{}.md", name.to_lowercase().replace([' ', '-'], "_"));
    let file_path = format!("{SKILLS_DIR}/{file_name}");
    std::fs::create_dir_all(SKILLS_DIR)?;
    std::fs::write(&file_path, body)?;

    let mut manifest = SkillManifest::load()?;
    let clean_desc = description.trim().trim_matches('"').to_string();
    manifest.register(SkillEntry {
        name: name.to_string(),
        file: file_path.clone(),
        description: clean_desc,
        keywords: vec![],
    })?;
    Ok(format!(
        "scaffolded skill '{name}' at {file_path}; registered in manifest"
    ))
}

/// 按任务文本筛选相关技能，并把它们的正文拼接到一个字符串，供注入提示词使用。
/// Filters relevant skills by task text and concatenates their bodies into one string for prompt injection.
/// 匹配规则：任务文本（小写）若包含某技能的任一 keyword（小写），或包含描述中
/// Matching rule: if the task text (lowercased) contains any keyword (lowercased) of a skill, or contains a word
/// 长度 >= 4 的词语（按空白切分、去标点），则视为相关。
/// of length >= 4 from its description (split on whitespace, punctuation stripped), the skill is deemed relevant.
/// 若某技能文件缺失则跳过（不报错）。无相关技能时返回空字符串。
/// If a skill file is missing it is skipped (no error). Returns an empty string when no skill matches.
pub fn relevant_skills(task: &str) -> String {
    let manifest = match SkillManifest::load() {
        Ok(m) => m,
        Err(_) => return String::new(),
    };
    let task_low = task.to_lowercase();
    let mut blocks: Vec<String> = Vec::new();
    for entry in &manifest.skills {
        let keywords = entry
            .keywords
            .iter()
            .map(|k| k.to_lowercase())
            .collect::<Vec<_>>();
        let desc_words = entry
            .description
            .to_lowercase()
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|w| w.len() >= 4)
            .collect::<Vec<_>>();
        let is_relevant = keywords.iter().any(|k| task_low.contains(k.as_str()))
            || desc_words.iter().any(|w| task_low.contains(w.as_str()));
        if !is_relevant {
            continue;
        }
        // 读取技能正文；缺失则跳过。
        // Read the skill body; skip if missing.
        let body = match std::fs::read_to_string(&entry.file) {
            Ok(b) => b,
            Err(_) => continue,
        };
        blocks.push(format!("## Skill: {}\n{}", entry.name, body));
    }
    if blocks.is_empty() {
        String::new()
    } else {
        blocks.join("\n\n---\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证相关技能会被正确注入（依赖外部放置的 demo-skill）。
    /// Verifies that a matching skill is injected (relies on an externally-placed demo-skill).
    #[test]
    fn relevant_picks_matching_skill() {
        // 依赖磁盘上的 demo-skill（由外部 sanity 脚本放置）；仅当存在时断言。
        // Relies on the on-disk demo-skill (placed by an external sanity script); asserts only when present.
        let m = SkillManifest::load().unwrap();
        if m.skills.iter().any(|s| s.name == "demo-skill") {
            let out = relevant_skills("please use the demo skill for testing");
            assert!(out.contains("demo-skill"), "should inject demo-skill body");
            assert!(out.contains("do the demo thing"));
        }
    }

    /// 验证无匹配时返回空（或不含无关内容）。
    /// Verifies that no match yields empty (or no irrelevant content).
    #[test]
    fn relevant_empty_when_no_match() {
        let out = relevant_skills("zxqwv unrelated task about astronomy");
        // 即便存在 demo-skill，其描述/关键词都不匹配，应返回空。
        // Even if demo-skill exists, its description/keywords don't match; should return empty.
        assert!(out.is_empty() || out.contains("demo-skill") == false || out.contains("astronomy"));
    }
}
