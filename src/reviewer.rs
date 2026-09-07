// 两阶段评审门（ReviewGate）。被 Orchestrator 在 SDD 管线中调用：
// Two-stage review gate (ReviewGate). Called by the Orchestrator in the SDD pipeline:
// Builder 产出后，Auditor 先做规格符合性评审，再做代码质量评审。
// After Builder produces, the Auditor first does spec compliance review, then code quality review.
// 两者都 APPROVE 才算通过，否则带反馈退回。
// Both must APPROVE to pass; otherwise returned with feedback.
//
// 评审基于**实际改动**（git diff + 新增文件内容）+ 构建验证结果，而非仅凭产出文本。
// Review is based on **actual changes** (git diff + new file contents) + build verification,
// not solely on the builder's narrative output.

use crate::event::EventSender;
use crate::registry::{AgentRegistry, Role};
use std::io::Read;

/// 评审结论：通过 / 驳回（附反馈）/ 需要澄清（附问题）。
/// Review verdict: Approve / Reject (with feedback) / Clarify (with question).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reject(String), // 返回给构建者的反馈
    // Feedback returned to the builder
    Clarify(String),
}

// ── 改动材料收集：纯函数 + 集成函数（separated for testing）──
// ── Change material collection: pure fns + integration fn (separated for testing) ──

/// `git status --porcelain` 行分类。
/// Classifies a `git status --porcelain` status line.
///
/// 注/局限：不处理引号路径（含空格/特殊字符的路径），直接取状态前缀之后的原始字符串。
/// rename `R  old -> new` 会取 `new` 路径。
/// Note/limitation: does NOT handle quoted paths (paths with spaces/special chars);
/// takes the raw string after the 3-char status prefix. Rename `R  old -> new`
/// resolves to the `new` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    TrackedModified(String), // 跟踪文件的修改（M/A/D/R/C/T 等）/ tracked file modification
    Untracked(String),       // 未跟踪文件或目录 / untracked file or directory
    Ignored,                 // 忽略或空行 / ignored or empty
}

/// 纯函数：解析 `git status --porcelain` 的一行。
/// Pure fn: parse a single `git status --porcelain` line.
pub fn classify_porcelain_line(line: &str) -> ChangeKind {
    // porcelain v1 格式：XY<space>path（X=index 状态，Y=worktree 状态）。
    // porcelain v1 format: XY<space>path (X=index status, Y=worktree status).
    if line.len() < 3 {
        return ChangeKind::Ignored;
    }
    let x = line.as_bytes()[0];
    let y = line.as_bytes()[1];
    // line[2] 应为空格分隔符 / line[2] should be a space separator
    let raw_path = line[3..].to_string();

    // 处理 rename/copy："old -> new" → 取 new。
    // Handle rename/copy: "old -> new" → take new path.
    let path = if let Some(idx) = raw_path.find(" -> ") {
        raw_path[idx + 4..].to_string()
    } else {
        raw_path
    };

    match (x, y) {
        (b'?', b'?') => ChangeKind::Untracked(path),
        (b'!', b'!') => ChangeKind::Ignored,
        // 任一状态非空格 → 跟踪文件有改动。
        // Any status char non-space → tracked file has changes.
        (x, y) if x != b' ' || y != b' ' => ChangeKind::TrackedModified(path),
        _ => ChangeKind::Ignored,
    }
}

/// 检查文件是否疑似二进制：前 512 字节含 NUL 或读取失败。
/// Checks if a file is likely binary: NUL byte in first 512 bytes, or read error.
fn is_binary_ish(path: &std::path::Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return true; // 读取失败 → 跳过 / read error → skip
    };
    let mut buf = [0u8; 512];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return true,
    };
    buf[..n].contains(&0)
}

/// 纯函数：将已收集的 diff 与新文件内容格式化为最终材料字符串，按 max_chars 截断。
/// Pure fn: format collected diff and new-file contents into the final material string,
/// capped at max_chars. Truncation note appended when capped.
pub fn format_change_material(
    diff_output: &str,
    new_files: &[(&str, &str)], // (path, content)
    max_chars: usize,
) -> String {
    let mut out = String::new();

    if !diff_output.is_empty() {
        out.push_str("── diff ──\n");
        out.push_str(diff_output);
        if !diff_output.ends_with('\n') {
            out.push('\n');
        }
    }

    for (path, content) in new_files {
        out.push_str(&format!("── new file: {path} ──\n"));
        out.push_str(content);
        if !content.ends_with('\n') {
            out.push('\n');
        }
    }

    if out.len() > max_chars {
        // 截断时需对齐 UTF-8 字符边界，否则 truncate 会 panic。
        // Truncate must align to UTF-8 char boundaries, else truncate panics.
        let mut end = max_chars;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n…(材料已截断 / material truncated)");
    }

    out
}

/// 收集实际改动材料：tracked 修改取 git diff；untracked 新文件读全文（逐个与总量截断）。
/// Collect actual change material: git diff for tracked modifications; full content
/// for untracked new files (per-file and total caps). Non-git dirs → None (graceful).
///
/// 注：此处 git 仅做只读操作（status + diff），无任何变异。
/// Note: git is read-only here (status + diff only); no mutations.
pub fn collect_change_material(cwd: &std::path::Path, max_chars: usize) -> Option<String> {
    // 1. git status --porcelain
    let status_output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()?; // git 不存在或执行失败 → None
    if !status_output.status.success() {
        return None; // 非 git 仓库 / not a git repo
    }

    let status_str = String::from_utf8_lossy(&status_output.stdout);

    let mut tracked_paths: Vec<String> = Vec::new();
    let mut untracked_paths: Vec<String> = Vec::new();

    for line in status_str.lines() {
        match classify_porcelain_line(line) {
            ChangeKind::TrackedModified(p) => tracked_paths.push(p),
            ChangeKind::Untracked(p) => untracked_paths.push(p),
            ChangeKind::Ignored => {}
        }
    }

    // 2. git diff for tracked modifications（只读）。
    //    git diff for tracked modifications (read-only).
    let diff_output = if tracked_paths.is_empty() {
        String::new()
    } else {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(cwd).arg("diff").arg("--");
        for p in &tracked_paths {
            cmd.arg(p);
        }
        match cmd.output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => String::new(), // diff 失败 → 跳过 diff 段（降级处理）
        }
    };

    // 3. 读取 untracked 新文件全文（per-file ~3000 字符截断，跳过二进制文件）。
    //    Read untracked new files (per-file ~3000 char cap, skip binary files).
    let per_file_cap = 3000usize;
    let mut new_files: Vec<(String, String)> = Vec::new();

    for p in &untracked_paths {
        let full = cwd.join(p);
        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue, // 读取失败 → 跳过
        };

        if meta.is_dir() {
            // 未跟踪目录：浅 read_dir，仅列文件路径（最多 20 条），不读内容。
            // Untracked dir: shallow read_dir, list file paths only (up to 20), no content.
            if let Ok(rd) = std::fs::read_dir(&full) {
                let mut listing = String::new();
                let mut count = 0usize;
                for entry in rd.flatten() {
                    if count >= 20 {
                        break;
                    }
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_file() {
                            listing.push_str(&format!("  {}\n", entry.file_name().to_string_lossy()));
                            count += 1;
                        }
                    }
                }
                if !listing.is_empty() {
                    new_files.push((format!("{p}/ (listing)"), listing));
                }
            }
        } else if meta.is_file() {
            if is_binary_ish(&full) {
                continue; // 二进制文件 → 跳过
            }
            match std::fs::read_to_string(&full) {
                Ok(content) => {
                    let capped = if content.len() > per_file_cap {
                        let mut end = per_file_cap;
                        while end > 0 && !content.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}…(文件已截断 / file truncated)", &content[..end])
                    } else {
                        content
                    };
                    new_files.push((p.clone(), capped));
                }
                Err(_) => continue, // 读取失败 → 跳过
            }
        }
    }

    let new_file_refs: Vec<(&str, &str)> = new_files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    Some(format_change_material(&diff_output, &new_file_refs, max_chars))
}

// ── 审计提示词构建（纯函数，便于结构性测试）──
// ── Audit prompt builders (pure fn for structural testing) ──

/// 构建两阶段审计提示词（纯函数，遵循 sdd_*_prompt 模式）。
/// Builds both audit prompts (spec compliance + code quality) as a pure function,
/// mirroring the sdd_*_prompt pure-fn pattern for structural testing.
pub fn build_audit_prompts(
    task: &str,
    produced: &str,
    material: Option<&str>,
    verify_note: Option<&str>,
) -> (String, String) {
    let material_section = match material {
        Some(m) => m,
        _ => "(无 git 仓库，仅依据产出文本评审)",
    };
    let verify_line = verify_note.unwrap_or("(未运行)");

    let spec_prompt = format!(
        "你是规格符合性评审员（第 1/2 阶段）。\
         请求的任务：\n{task}\n\n\
         产出的工作：\n{produced}\n\n\
         实际改动（git diff 与新增文件）/ actual changes:\n{material_section}\n\n\
         构建验证 / build verification: {verify_line}\n\n\
         该工作是否实现了所要求的内容？请基于**实际改动**评审（产出文本仅供参考）。\
         恰好回复一行：'APPROVE' 或 'REJECT: <缺失或错误之处>'。"
    );

    let qual_prompt = format!(
        "你是代码质量评审员（第 2/2 阶段）。\
         请求的任务：\n{task}\n\n\
         产出的工作：\n{produced}\n\n\
         实际改动（git diff 与新增文件）/ actual changes:\n{material_section}\n\n\
         构建验证 / build verification: {verify_line}\n\n\
         检查安全性、正确性与可维护性。请基于**实际改动**评审（产出文本仅供参考）。\
         恰好回复一行：'APPROVE' 或 'REJECT: <问题>' 或 'CLARIFY: <疑问>'。"
    );

    (spec_prompt, qual_prompt)
}

/// SDD 两阶段评审门。遵循 OMO 的纪律：在审计者通过两个阶段之前，任务不算完成：
/// SDD two-stage review gate. Follows OMO discipline: a task is not complete until the Auditor passes both stages:
///   1. 规格符合性 —— 是否实现了所要求的内容？
///   1. Spec compliance — does it implement what was required?
///   2. 代码质量   —— 安全性、正确性、可维护性。
///   2. Code quality   — security, correctness, maintainability.
///      两者都必须 Approve，否则带着反馈退回。
///      Both must Approve; otherwise returned with feedback.
pub struct ReviewGate {
    registry: AgentRegistry,
}

impl ReviewGate {
    /// 构造评审门（持有 registry 以构建审计者 Agent）。
    /// Constructs the review gate (holds registry to build the Auditor Agent).
    pub fn new(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    /// 对产物执行两阶段评审，返回最终结论。
    /// `material` — 实际改动材料（git diff + 新文件内容）；None 表示无 git 仓库。
    /// `verify_note` — 构建验证结果（如 "✓ cargo build, cargo test"）；None 表示未运行验证。
    ///
    /// Executes two-stage review on the produced work, returns the final verdict.
    /// `material` — actual change material (git diff + new file contents); None = no git repo.
    /// `verify_note` — build verification outcome; None = verification not run.
    pub async fn review(
        &self,
        task: &str,
        produced: &str,
        material: Option<&str>,
        verify_note: Option<&str>,
        tx: &EventSender,
    ) -> anyhow::Result<Verdict> {
        let auditor = self.registry.build(Role::Auditor)?;

        let (spec_prompt, qual_prompt) = build_audit_prompts(task, produced, material, verify_note);

        let spec_out = auditor.run(&spec_prompt, tx).await?;

        if !spec_out.to_uppercase().contains("APPROVE") {
            let fb = spec_out
                .lines()
                .find(|l| l.to_uppercase().contains("REJECT"))
                .unwrap_or("spec compliance failed")
                .to_string();
            return Ok(Verdict::Reject(fb));
        }

        let qual_out = auditor.run(&qual_prompt, tx).await?;
        let up = qual_out.to_uppercase();
        if up.contains("APPROVE") {
            Ok(Verdict::Approve)
        } else if up.contains("CLARIFY") {
            Ok(Verdict::Clarify(
                qual_out
                    .lines()
                    .find(|l| l.to_uppercase().contains("CLARIFY"))
                    .unwrap_or("")
                    .to_string(),
            ))
        } else {
            Ok(Verdict::Reject(
                qual_out
                    .lines()
                    .find(|l| l.to_uppercase().contains("REJECT"))
                    .unwrap_or("quality failed")
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── porcelain 行分类测试 / porcelain classify tests ──

    #[test]
    fn classify_unstaged_modified() {
        assert_eq!(
            classify_porcelain_line(" M src/foo.rs"),
            ChangeKind::TrackedModified("src/foo.rs".to_string())
        );
    }

    #[test]
    fn classify_staged_modified() {
        assert_eq!(
            classify_porcelain_line("M  src/foo.rs"),
            ChangeKind::TrackedModified("src/foo.rs".to_string())
        );
    }

    #[test]
    fn classify_both_staged_and_unstaged() {
        assert_eq!(
            classify_porcelain_line("MM src/foo.rs"),
            ChangeKind::TrackedModified("src/foo.rs".to_string())
        );
    }

    #[test]
    fn classify_staged_add() {
        assert_eq!(
            classify_porcelain_line("A  src/new.rs"),
            ChangeKind::TrackedModified("src/new.rs".to_string())
        );
    }

    #[test]
    fn classify_untracked_file() {
        assert_eq!(
            classify_porcelain_line("?? src/untracked.rs"),
            ChangeKind::Untracked("src/untracked.rs".to_string())
        );
    }

    #[test]
    fn classify_untracked_dir() {
        assert_eq!(
            classify_porcelain_line("?? src/newdir/"),
            ChangeKind::Untracked("src/newdir/".to_string())
        );
    }

    #[test]
    fn classify_ignored() {
        assert_eq!(classify_porcelain_line("!! target/"), ChangeKind::Ignored);
    }

    #[test]
    fn classify_clean_line_ignored() {
        // "  foo.rs" (both statuses space = unmodified) → Ignored
        assert_eq!(classify_porcelain_line("  foo.rs"), ChangeKind::Ignored);
    }

    #[test]
    fn classify_short_line_ignored() {
        assert_eq!(classify_porcelain_line(""), ChangeKind::Ignored);
        assert_eq!(classify_porcelain_line("a"), ChangeKind::Ignored);
        assert_eq!(classify_porcelain_line("ab"), ChangeKind::Ignored);
    }

    #[test]
    fn classify_rename_resolves_to_new_path() {
        assert_eq!(
            classify_porcelain_line("R  old_path.rs -> new_path.rs"),
            ChangeKind::TrackedModified("new_path.rs".to_string())
        );
    }

    #[test]
    fn classify_spaces_in_filename_limitation() {
        // 局限说明：含空格的文件名被当作普通字符串解析，不做引号处理。
        // Limitation note: filenames with spaces are parsed as plain strings,
        // no quoted-path handling. "foo bar.rs" is treated as one token.
        assert_eq!(
            classify_porcelain_line(" M foo bar.rs"),
            ChangeKind::TrackedModified("foo bar.rs".to_string())
        );
    }

    // ── 材料截断测试 / material capping tests ──

    #[test]
    fn format_change_material_no_truncation_when_small() {
        let diff = "small diff content";
        let result = format_change_material(diff, &[], 8000);
        assert!(!result.contains("截断"));
        assert!(result.contains("small diff content"));
    }

    #[test]
    fn format_change_material_truncates_when_exceeds_max() {
        let diff = "a".repeat(5000);
        let result = format_change_material(&diff, &[], 3000);
        assert!(
            result.len() <= 3000 + 100,
            "result should be ~max_chars + truncation note"
        );
        assert!(
            result.contains("材料已截断"),
            "truncation note should be appended"
        );
    }

    #[test]
    fn format_change_material_includes_new_files() {
        let result = format_change_material(
            "diff here",
            &[("src/new.rs", "fn main() {}")],
            8000,
        );
        assert!(result.contains("diff here"));
        assert!(result.contains("src/new.rs"));
        assert!(result.contains("fn main() {}"));
    }

    #[test]
    fn format_change_material_empty_returns_empty() {
        let result = format_change_material("", &[], 8000);
        assert!(result.is_empty());
    }

    // ── build_audit_prompts 结构性测试 / prompt builder structural tests ──

    #[test]
    fn build_audit_prompts_contains_material_and_verify_sections() {
        let (spec, qual) = build_audit_prompts(
            "implement auth",
            "fn auth() {}",
            Some("diff content here"),
            Some("✓ cargo build, cargo test"),
        );

        // Both prompts contain the material section
        assert!(spec.contains("实际改动"), "spec must contain material section");
        assert!(qual.contains("实际改动"), "qual must contain material section");
        assert!(spec.contains("diff content here"));
        assert!(qual.contains("diff content here"));

        // Both prompts contain the verify line
        assert!(spec.contains("构建验证"));
        assert!(qual.contains("构建验证"));
        assert!(spec.contains("✓ cargo build, cargo test"));
        assert!(qual.contains("✓ cargo build, cargo test"));

        // Both prompts say narrative is reference only
        assert!(spec.contains("产出文本仅供参考"));
        assert!(qual.contains("产出文本仅供参考"));

        // Verdict protocol preserved
        assert!(spec.contains("APPROVE"));
        assert!(spec.contains("REJECT"));
        assert!(qual.contains("APPROVE"));
        assert!(qual.contains("REJECT"));
        assert!(qual.contains("CLARIFY"));

        // Both prompts contain task and produced
        assert!(spec.contains("implement auth"));
        assert!(spec.contains("fn auth() {}"));
        assert!(qual.contains("implement auth"));
        assert!(qual.contains("fn auth() {}"));
    }

    #[test]
    fn build_audit_prompts_none_material_fallback_note() {
        let (spec, qual) = build_audit_prompts("task", "produced", None, None);
        assert!(
            spec.contains("无 git 仓库"),
            "None material → fallback note in spec"
        );
        assert!(
            qual.contains("无 git 仓库"),
            "None material → fallback note in qual"
        );
        assert!(spec.contains("未运行"), "None verify → fallback note in spec");
        assert!(qual.contains("未运行"), "None verify → fallback note in qual");
    }

    #[test]
    fn build_audit_prompts_some_material_empty_string_shows_empty() {
        // Some("") → empty material section (not the fallback note)
        let (spec, _) = build_audit_prompts("task", "produced", Some(""), Some("verify ok"));
        assert!(!spec.contains("无 git 仓库"), "Some(\"\") should NOT show fallback");
        assert!(spec.contains("verify ok"));
    }

    #[test]
    fn build_audit_prompts_verify_note_with_failure() {
        let (spec, qual) = build_audit_prompts(
            "task",
            "produced",
            Some("diff"),
            Some("failed: cargo build"),
        );
        assert!(spec.contains("failed: cargo build"));
        assert!(qual.contains("failed: cargo build"));
    }

    // ── 集成测试：collect_change_material ──
    // ── Integration tests: collect_change_material ──

    /// 检查 git 二进制是否可用 / Check if git binary is available.
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 创建临时 git 仓库目录（调用方负责清理）。
    /// Create a temporary git repo dir (caller cleans up).
    fn temp_git_repo(suffix: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("moye_test_material_{suffix}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // git init
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .arg("init")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            "git init should succeed"
        );

        // 设置 user 以便 commit / set user for commit
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["config", "user.email", "test@test.com"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();

        tmp
    }

    fn cleanup_temp(p: &std::path::Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn collect_change_material_non_git_dir_returns_none() {
        // 用一个无效 .git 文件确保 git 不向上查找父仓库。
        // Use an invalid .git file to ensure git doesn't walk up to a parent repo.
        let tmp = std::env::temp_dir().join(format!(
            "moye_test_nongit_{}_{}",
            std::process::id(),
            "non_git"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // 创建指向不存在路径的 .git 文件 → git 报错 → None。
        std::fs::write(tmp.join(".git"), "gitdir: /nonexistent/path\n").unwrap();

        let material = collect_change_material(&tmp, 8000);
        cleanup_temp(&tmp);

        assert!(material.is_none(), "non-git dir should return None");
    }

    #[test]
    fn collect_change_material_skips_binary_files() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }

        let tmp = temp_git_repo("binary");

        // 创建并提交一个初始文件 / create and commit an initial file
        std::fs::write(tmp.join("init.txt"), "init\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["add", "."])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();

        // 创建二进制 untracked 文件（含 NUL 字节）/ create binary untracked file
        std::fs::write(tmp.join("binary.bin"), b"hello\x00world\n").unwrap();
        // 创建文本 untracked 文件 / create text untracked file
        std::fs::write(tmp.join("text.txt"), "text content\n").unwrap();

        let material = collect_change_material(&tmp, 8000);
        cleanup_temp(&tmp);

        let material = material.expect("valid git repo should return Some");
        assert!(
            !material.contains("hello\x00world"),
            "binary file should be skipped"
        );
        assert!(
            material.contains("text content"),
            "text file should be included"
        );
    }

    #[test]
    fn collect_change_material_temp_repo_has_diff_and_new_file() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }

        let tmp = temp_git_repo("integration");

        // 创建并提交初始文件 / create and commit initial file
        std::fs::write(tmp.join("hello.txt"), "hello world\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["add", "."])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["commit", "-m", "initial"])
            .output()
            .unwrap();

        // 修改已跟踪文件 / modify tracked file
        std::fs::write(tmp.join("hello.txt"), "hello modified\n").unwrap();
        // 创建新 untracked 文件 / create new untracked file
        std::fs::write(tmp.join("new_file.txt"), "new content here\n").unwrap();

        let material = collect_change_material(&tmp, 8000);
        cleanup_temp(&tmp);

        let material = material.expect("valid git repo should return Some");
        // 材料应同时包含 diff 内容和新文件内容。
        // Material should contain BOTH the diff hunk and the new file's content.
        assert!(
            material.contains("hello"),
            "material should contain diff with 'hello'"
        );
        assert!(
            material.contains("new content here"),
            "material should contain new file content"
        );
    }

    #[test]
    fn collect_change_material_clean_repo_returns_empty_some() {
        if !git_available() {
            eprintln!("skipping: git binary not available");
            return;
        }

        let tmp = temp_git_repo("clean");

        // 创建并提交初始文件 / create and commit initial file
        std::fs::write(tmp.join("init.txt"), "init\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["add", "."])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();

        // 无修改、无新文件 → Some("") (git 命令成功，但无改动内容)。
        // No modifications, no new files → Some("") (git succeeded, no changes).
        let material = collect_change_material(&tmp, 8000);
        cleanup_temp(&tmp);

        let material = material.expect("valid git repo should return Some");
        assert!(material.is_empty(), "clean repo → empty material string");
    }
}
