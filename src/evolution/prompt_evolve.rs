// 提示词进化模块（v2）：用与 AGENTS.md 领域直接相关的质量评分标准给候选提示词打分，
// Prompt evolution module (v2): scores candidate prompts against quality criteria directly
// 只有分数不低于当前版本才会被采用，从而防止提示词越改越差（漂移）。
// relevant to what AGENTS.md governs; only adopts a candidate if its score >= current.
//
// v2 改进要点 / Key improvements over v1:
// 1. 用领域相关的质量评分标准替代无关的编程题基准（Fibonacci/闭包/错误处理）。
//    Replaces irrelevant coding-trivia benchmarks (Fibonacci/closures/error-handling)
//    with quality criteria that directly measure what AGENTS.md should contain.
// 2. 将积累的经验教训注入提案提示词，让元 Agent 基于真实经验改进。
//    Injects accumulated lessons into the proposal prompt so the meta-agent
//    improves based on real experience, not blind rewriting.
// 3. 给元 Agent 结构化指导：保留章节、不删安全规则、保持简洁。
//    Gives the meta-agent structured guidance: preserve section structure,
//    don't remove safety rules, stay concise.
// 4. 多维度评分（满分 10），比旧的 0-3 二值通过/失败更精细。
//    Multi-dimensional scoring (max 10), more granular than the old 0-3 binary pass/fail.
// 5. 采纳时生成变更摘要，便于追踪改了什么。
//    Generates a changelog summary on adoption for traceability.
use crate::event::EventSender;
use crate::memory::Lesson;
use crate::registry::{AgentRegistry, Role};
use anyhow::Result;
use std::process::Command;

/// 质量评分标准：由审计者 Agent 对提示词文本逐条打分（每条 0-2 分），满分 10 分。
/// Quality scoring criteria: the Auditor agent scores each criterion 0-2, max 10.
/// 这些标准直接衡量 AGENTS.md 应当包含的内容，而非无关的编程题。
/// These criteria directly measure what AGENTS.md should contain, not irrelevant coding trivia.
const EVAL_CRITERIA: &[(&str, &str)] = &[
    ("roles", "Does the prompt clearly define the roles (Orchestrator, Planner, Builder, Auditor) and their responsibilities in the SDD pipeline?"),
    ("safety", "Does the prompt include safety rules: only Builder can edit files/run bash, audit required before accepting changes, build+test must pass or git rollback?"),
    ("memory", "Does the prompt mention lesson accumulation after each task and escalating repeated corrections to rules?"),
    ("conciseness", "Is the prompt concise — free of redundancy, unnecessary verbosity, or irrelevant content?"),
    ("clarity", "Are the instructions unambiguous and actionable? Could a new agent follow them without confusion?"),
];

/// 提示词进化器：持有注册表与被进化的 AGENTS.md 路径。
/// Prompt evolver: holds the registry and the path to the AGENTS.md being evolved.
pub struct PromptEvolver {
    registry: AgentRegistry,
    agents_md_path: String,
}

impl PromptEvolver {
    pub fn new(registry: AgentRegistry, agents_md_path: String) -> Self {
        Self {
            registry,
            agents_md_path,
        }
    }

    /// 读取当前提示词（AGENTS.md 的内容）。
    /// Read the current prompt (contents of AGENTS.md).
    pub fn current_preamble(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.agents_md_path)?)
    }

    /// 让审计者 Agent 直接对提示词文本按质量标准打分，返回总分（0-10）。
    /// Has the Auditor agent directly score the prompt text against quality criteria,
    /// returning the total score (0-10).
    ///
    /// 这比旧的"跑无关编程题再判通过/失败"更直接、更相关：
    /// This is more direct and relevant than the old "run irrelevant coding tasks
    /// then judge pass/fail" approach:
    /// - 旧方法测试的是编程知识，与 AGENTS.md 管控的行为无关。
    ///   The old method tested coding knowledge, unrelated to what AGENTS.md governs.
    /// - 新方法直接评估提示词是否包含角色定义、安全规则、记忆机制等关键内容。
    ///   The new method directly evaluates whether the prompt contains key content
    ///   like role definitions, safety rules, and memory mechanisms.
    pub async fn eval_preamble(&self, preamble: &str, tx: &EventSender) -> Result<usize> {
        let judge = self.registry.build(Role::Auditor)?;

        let criteria_text = EVAL_CRITERIA
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| format!("{}. [{name}] {desc} (score 0-2)", i + 1))
            .collect::<Vec<_>>()
            .join("\n");

        let eval_prompt = format!(
            "You are evaluating a system prompt for an AI agent that follows\n\
             Subagent-Driven Development (SDD) with roles: Orchestrator, Planner, Builder, Auditor.\n\n\
             Score each criterion 0-2:\n\
             0 = missing or very weak\n\
             1 = present but could be clearer\n\
             2 = clear and strong\n\n\
             Criteria:\n{criteria_text}\n\n\
             Prompt to evaluate:\n\
             ---\n\
             {preamble}\n\
             ---\n\n\
             Reply with ONLY the scores, one per line, in format 'name: score'.\n\
             On the final line write 'TOTAL: <sum of all scores>'."
        );

        let response = judge.run(&eval_prompt, tx).await?;
        let score = parse_eval_score(&response);
        Ok(score)
    }

    /// 由元 Agent 基于积累的经验教训提出改进版提示词，再用质量评分判定
    /// "起码不比旧的差"才采用。可回退：覆盖前用 git tag 给旧提示词打点。
    /// A meta-agent proposes an improved prompt guided by accumulated lessons, then
    /// quality scoring decides adoption only if "at least as good as the old one".
    /// Rollbackable: tags the old prompt with a git tag before overwriting.
    ///
    /// 与旧版的区别 / Differences from v1:
    /// - 注入积累的经验教训，让元 Agent 知道过去哪些模式反复出错。
    ///   Injects accumulated lessons so the meta-agent knows what patterns keep failing.
    /// - 给出结构化指导（保留章节、不删安全规则、保持简洁），避免盲目重写。
    ///   Provides structured guidance (preserve sections, don't remove safety rules,
    ///   stay concise) to prevent blind rewrites.
    pub async fn evolve(&self, lessons: &[Lesson], tx: &EventSender) -> Result<String> {
        let current = self.current_preamble()?;

        // 将积累的经验教训格式化后注入提案提示词。
        // Format accumulated lessons for injection into the proposal prompt.
        let lessons_text = if lessons.is_empty() {
            "(no lessons accumulated yet)".to_string()
        } else {
            lessons
                .iter()
                .map(|l| format!("- {}", l.summary))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let meta = self.registry.build(Role::Orchestrator)?;
        let proposal_prompt = format!(
            "You are a meta-agent tasked with improving the system prompt of an AI agent.\n\
             The agent follows Subagent-Driven Development (SDD) with roles:\n\
             Orchestrator, Planner, Builder, Auditor.\n\n\
             Below is the current system prompt and accumulated lessons from past tasks.\n\
             Improve the prompt based on these lessons. Follow these rules:\n\
             1. PRESERVE the section structure (roles, discipline, safety, memory).\n\
             2. ADD new rules or clarify existing ones based on the lessons.\n\
             3. DO NOT remove existing safety rules.\n\
             4. Keep it CONCISE — no unnecessary verbosity.\n\
             5. Output ONLY the new prompt text, no commentary or code fences.\n\n\
             Current prompt:\n\
             ---\n\
             {current}\n\
             ---\n\n\
             Accumulated lessons:\n\
             {lessons_text}"
        );
        let proposed_raw = meta.run(&proposal_prompt, tx).await?;
        let proposed = strip_code_fences(&proposed_raw);

        let old_score = self.eval_preamble(&current, tx).await?;
        let new_score = self.eval_preamble(&proposed, tx).await?;

        if new_score >= old_score {
            // Checkpoint the old version, then promote.
            // 给旧版本打 git tag，然后写入新版本。
            let _ = Command::new("git")
                .args(["tag", &format!("prompt-v{}", now())])
                .output();
            std::fs::write(&self.agents_md_path, &proposed)?;

            let changelog = generate_changelog(&current, &proposed);
            Ok(format!(
                "promoted new prompt (score {new_score} >= {old_score}); old tagged in git.\n\
                 Changes: {changelog}"
            ))
        } else {
            Ok(format!(
                "kept old prompt (new score {new_score} < {old_score}); no change"
            ))
        }
    }
}

/// 从审计者的评分回复中提取总分。先找 "TOTAL: N" 行；找不到则逐行累加。
/// Extracts the total score from the Auditor's evaluation response.
/// Looks for a "TOTAL: N" line first (case-insensitive); if not found, sums
/// individual criterion scores from lines like "name: N".
fn parse_eval_score(response: &str) -> usize {
    let max_score = EVAL_CRITERIA.len() * 2;

    // Look for "TOTAL: <number>" in the response (case-insensitive, last match wins).
    for line in response.lines().rev() {
        let line = line.trim();
        if let Some(rest) = line.to_lowercase().strip_prefix("total:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n.min(max_score);
            }
        }
    }

    // Fallback: sum up individual criterion scores (each line like "name: N").
    let mut total = 0;
    for line in response.lines() {
        if let Some(colon_pos) = line.rfind(':') {
            if let Ok(n) = line[colon_pos + 1..].trim().parse::<usize>() {
                total += n.min(2);
            }
        }
    }
    total.min(max_score)
}

/// 去除 LLM 输出可能包裹的 Markdown 代码围栏（```...```）。
/// Strips Markdown code fences that the LLM may wrap around its output.
fn strip_code_fences(s: &str) -> String {
    let s = s.trim();
    if !s.starts_with("```") {
        return s.to_string();
    }
    // Skip the opening fence line (including optional language tag like ```markdown).
    let rest: String = s.lines().skip(1).collect::<Vec<_>>().join("\n");
    // Remove closing fence if present.
    let rest = rest.trim_end_matches("```").trim();
    rest.to_string()
}

/// 生成新旧提示词之间的简要变更摘要，包含增删行数和最多 3 条新增行示例。
/// Generates a brief changelog summary between old and new prompts,
/// including line counts and up to 3 example added lines.
fn generate_changelog(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().map(|l| l.trim()).collect();
    let new_lines: Vec<&str> = new.lines().map(|l| l.trim()).collect();

    let added: Vec<&&str> = new_lines.iter().filter(|l| !old_lines.contains(l)).collect();
    let removed: Vec<&&str> = old_lines.iter().filter(|l| !new_lines.contains(l)).collect();

    let mut summary = format!(
        "{} line(s) added, {} line(s) changed/removed",
        added.len(),
        removed.len()
    );

    // Show up to 3 added lines as examples.
    if !added.is_empty() {
        summary.push_str("\n  Added:");
        for line in added.iter().take(3) {
            summary.push_str(&format!("\n    + {}", truncate_line(line, 80)));
        }
    }

    summary
}

/// 截断一行到最多 max_chars 个字符，超出时追加省略号。
/// Truncates a line to at most max_chars, appending an ellipsis if truncated.
fn truncate_line(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_eval_score_with_total_line() {
        let response = "roles: 2\nsafety: 2\nmemory: 1\nconciseness: 2\nclarity: 2\nTOTAL: 9";
        assert_eq!(parse_eval_score(response), 9);
    }

    #[test]
    fn parse_eval_score_case_insensitive_total() {
        let response = "roles: 1\ntotal: 7";
        assert_eq!(parse_eval_score(response), 7);
    }

    #[test]
    fn parse_eval_score_fallback_sum() {
        // No TOTAL line — should sum individual scores.
        let response = "roles: 2\nsafety: 1\nmemory: 2\nconciseness: 2\nclarity: 2";
        assert_eq!(parse_eval_score(response), 9);
    }

    #[test]
    fn parse_eval_score_caps_at_max() {
        // Each criterion capped at 2, 5 criteria → max 10.
        let response = "roles: 5\nsafety: 5\nTOTAL: 99";
        assert_eq!(parse_eval_score(response), 10);
    }

    #[test]
    fn strip_code_fences_basic() {
        let input = "```markdown\n# Title\nContent\n```";
        assert_eq!(strip_code_fences(input), "# Title\nContent");
    }

    #[test]
    fn strip_code_fences_no_fences() {
        let input = "# Title\nContent";
        assert_eq!(strip_code_fences(input), "# Title\nContent");
    }

    #[test]
    fn strip_code_fences_no_closing_fence() {
        let input = "```\n# Title\nContent";
        assert_eq!(strip_code_fences(input), "# Title\nContent");
    }

    #[test]
    fn generate_changelog_detects_additions() {
        let old = "line1\nline2";
        let new = "line1\nline2\nline3";
        let changelog = generate_changelog(old, new);
        assert!(changelog.contains("1 line(s) added"));
        assert!(changelog.contains("+ line3"));
    }

    #[test]
    fn generate_changelog_detects_removals() {
        let old = "line1\nline2\nline3";
        let new = "line1\nline3";
        let changelog = generate_changelog(old, new);
        assert!(changelog.contains("1 line(s) changed/removed"));
    }

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("hello", 10), "hello");
    }

    #[test]
    fn truncate_line_long_truncated() {
        let s = "a".repeat(100);
        let result = truncate_line(&s, 10);
        assert_eq!(result.chars().count(), 13); // 10 chars + "..."
        assert!(result.ends_with("..."));
    }
}
