// 验证模块：在 Builder 产出后、Auditor 评审前自动运行构建/测试命令。
// Verification module: auto-runs build/test commands after the Builder produces
// code and before the Auditor reviews it.
//
// 验证门是一个有界修复循环：失败时将错误输出回传给 Builder 重试，
// 重试次数耗尽则跳过审计并以明确的失败说明返回。
// The verify gate is a bounded fix-and-reverify loop: on failure, the error
// output is fed back to the Builder for retry; when retries are exhausted,
// the audit is skipped and the task returns with a clear failure note.
//
// 安全说明：验证命令直接作为子进程 spawn（不经过 LLM 工具调用，不沙箱包装）。
// 这些是配置或检测到的受信命令，信任级别与用户手动运行 cargo test 相同。
// Security note: verify commands are spawned directly as subprocesses (not through
// an LLM tool call, not sandbox-wrapped). These are config-detected trusted
// commands, same trust level as the user running cargo test manually.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// 单条命令的执行结果。
/// Result of a single command execution.
#[derive(Debug, Clone, PartialEq)]
pub enum SingleOutcome {
    /// 命令成功退出。
    /// Command exited successfully.
    Ok,
    /// 命令失败：附命令名、输出尾部、退出码。
    /// Command failed: includes command, output tail, and exit code.
    Failed {
        command: String,
        output_tail: String,
        exit_code: Option<i32>,
    },
    /// 命令不可用（spawn 失败或退出码 127）——不视为验证失败。
    /// Command unavailable (spawn failure or exit code 127) — not a verify failure.
    Unavailable { command: String },
}

/// 验证结果。
/// Verification outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyOutcome {
    /// 所有命令通过，附已运行的命令列表。
    /// All commands passed, with the list of commands that ran.
    Passed { ran: Vec<String> },
    /// 某条命令失败：附命令名、输出尾部、退出码。
    /// A command failed: includes the command, output tail, and exit code.
    Failed {
        command: String,
        output_tail: String,
        exit_code: Option<i32>,
    },
    /// 跳过：未检测到命令或验证被禁用。
    /// Skipped: no commands detected or verification disabled.
    #[allow(dead_code)] // 构造于 verify_with_retries 的提前返回路径；next_gate_decision 中匹配。
    Skipped { reason: String },
    /// 不可用：命令无法 spawn（如工具链缺失）——不视为失败。
    /// Unavailable: command could not spawn (e.g. missing toolchain) — not a failure.
    Unavailable { command: String },
}

/// 验证门决策：纯函数，根据当前结果与剩余重试次数决定下一步。
/// Gate decision: pure function deciding the next step from the outcome and
/// remaining retries. Extracted so the retry-loop logic is exhaustively
/// testable without an LLM closure.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// 验证通过或被跳过/不可用——继续审计。
    /// Proceed: passed, skipped, or unavailable — continue to audit.
    Proceed,
    /// 验证失败且仍有重试——用失败上下文重跑 Builder。
    /// Retry: failed and retries remain — re-run Builder with failure context.
    Retry(String),
    /// 验证失败且重试耗尽——放弃，跳过审计。
    /// GiveUp: failed and retries exhausted — skip audit.
    GiveUp(String),
}

/// 根据验证结果与剩余重试次数决定下一步（纯函数）。
/// Decide the next gate step from the outcome and remaining retries (pure fn).
///
/// - Passed / Skipped / Unavailable → Proceed
/// - Failed + retries_left > 0 → Retry(failure context)
/// - Failed + retries_left == 0 → GiveUp(reason)
pub fn next_gate_decision(outcome: &VerifyOutcome, retries_left: u32) -> GateDecision {
    match outcome {
        VerifyOutcome::Passed { .. }
        | VerifyOutcome::Skipped { .. }
        | VerifyOutcome::Unavailable { .. } => GateDecision::Proceed,
        VerifyOutcome::Failed {
            command,
            output_tail,
            ..
        } => {
            if retries_left > 0 {
                GateDecision::Retry(format!(
                    "命令失败 / Command failed: {command}\n\
                     输出尾部 / Output tail:\n{output_tail}"
                ))
            } else {
                GateDecision::GiveUp(format!("验证未通过 / verification failed: {command}"))
            }
        }
    }
}

/// 根据项目根目录的标记文件检测验证命令（首个匹配的标记生效）。
/// Detect verify commands from marker files in the project root (first
/// matching marker wins).
///
/// - `Cargo.toml` → `["cargo build", "cargo test"]`
/// - `package.json` → `["npm test --if-present"]`
/// - `pyproject.toml` → `["pytest -q"]`
/// - `go.mod` → `["go test ./..."]`
/// - 无标记 → `None`
pub fn detect_commands(cwd: &Path) -> Option<Vec<String>> {
    if cwd.join("Cargo.toml").exists() {
        Some(vec![
            "cargo build".to_string(),
            "cargo test".to_string(),
        ])
    } else if cwd.join("package.json").exists() {
        Some(vec!["npm test --if-present".to_string()])
    } else if cwd.join("pyproject.toml").exists() {
        Some(vec!["pytest -q".to_string()])
    } else if cwd.join("go.mod").exists() {
        Some(vec!["go test ./...".to_string()])
    } else {
        None
    }
}

/// 截取输出的尾部（最多 `max_chars` 个字节对应的字符），保留最近的错误信息。
/// Truncate output to its tail (at most `max_chars` bytes), keeping the most
/// recent error info.
///
/// 在截断点回退到字符边界，防止拆分多字节字符。
/// Steps back to a char boundary to avoid splitting a multi-byte character.
pub fn truncate_tail(output: &str, max_chars: usize) -> &str {
    if output.len() <= max_chars {
        return output;
    }
    let mut start = output.len() - max_chars;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    &output[start..]
}

/// 运行单条命令，捕获合并 stdout+stderr，超时则中止。
/// Run a single command, capturing combined stdout+stderr, killing on timeout.
///
/// 退出码 127（命令未找到）视为 Unavailable 而非 Failed——工具链缺失不得
/// 导致任务失败。
/// Exit code 127 (command not found) is treated as Unavailable, not Failed —
/// missing toolchains must not fail the task.
pub async fn run_single_command(cmd: &str, cwd: &Path, timeout: Duration) -> SingleOutcome {
    let child = match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            return SingleOutcome::Unavailable {
                command: cmd.to_string(),
            }
        }
    };

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                SingleOutcome::Ok
            } else {
                let exit_code = output.status.code();
                // 退出码 127 = 命令未找到 → 工具链缺失，不视为失败。
                // Exit code 127 = command not found → missing toolchain, not a failure.
                if exit_code == Some(127) {
                    return SingleOutcome::Unavailable {
                        command: cmd.to_string(),
                    };
                }
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let tail = truncate_tail(&combined, 4000).to_string();
                SingleOutcome::Failed {
                    command: cmd.to_string(),
                    output_tail: tail,
                    exit_code,
                }
            }
        }
        Ok(Err(_)) => SingleOutcome::Unavailable {
            command: cmd.to_string(),
        },
        Err(_) => SingleOutcome::Failed {
            command: cmd.to_string(),
            output_tail: format!("命令超时 / Command timed out after {}s", timeout.as_secs()),
            exit_code: None,
        },
    }
}

/// 依次运行命令，首个失败即停止。
/// Run commands in sequence, stopping at the first failure.
#[allow(dead_code)] // 生产路径在 verify_with_retries 中逐条调用 run_single_command（含 Info 行）；
                    // 此函数是测试便捷封装 + 多命令顺序行为的测试接缝。
pub async fn run_commands(cmds: &[String], cwd: &Path, timeout: Duration) -> VerifyOutcome {
    let mut ran = Vec::new();
    for cmd in cmds {
        match run_single_command(cmd, cwd, timeout).await {
            SingleOutcome::Ok => ran.push(cmd.clone()),
            SingleOutcome::Failed {
                command,
                output_tail,
                exit_code,
            } => {
                return VerifyOutcome::Failed {
                    command,
                    output_tail,
                    exit_code,
                }
            }
            SingleOutcome::Unavailable { command } => {
                return VerifyOutcome::Unavailable { command }
            }
        }
    }
    VerifyOutcome::Passed { ran }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：创建一个唯一临时目录，测试结束时自动清理。
    /// Test helper: creates a unique temp directory, auto-cleaned on Drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "moye-verify-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── detect_commands（纯函数） ──
    // ── detect_commands (pure fn) ──

    #[test]
    fn detect_commands_cargo() {
        let tmp = TempDir::new("cargo");
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            detect_commands(&tmp),
            Some(vec![
                "cargo build".to_string(),
                "cargo test".to_string(),
            ])
        );
    }

    #[test]
    fn detect_commands_npm() {
        let tmp = TempDir::new("npm");
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_commands(&tmp),
            Some(vec!["npm test --if-present".to_string()])
        );
    }

    #[test]
    fn detect_commands_pytest() {
        let tmp = TempDir::new("pytest");
        std::fs::write(tmp.join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_commands(&tmp), Some(vec!["pytest -q".to_string()]));
    }

    #[test]
    fn detect_commands_go() {
        let tmp = TempDir::new("go");
        std::fs::write(tmp.join("go.mod"), "").unwrap();
        assert_eq!(
            detect_commands(&tmp),
            Some(vec!["go test ./...".to_string()])
        );
    }

    #[test]
    fn detect_commands_none_when_no_marker() {
        let tmp = TempDir::new("none");
        assert_eq!(detect_commands(&tmp), None);
    }

    #[test]
    fn detect_commands_first_marker_wins() {
        // Cargo.toml + package.json 同时存在 → Cargo 命令生效（首个匹配）。
        // Cargo.toml + package.json both present → Cargo commands win (first match).
        let tmp = TempDir::new("first");
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        let cmds = detect_commands(&tmp).unwrap();
        assert_eq!(cmds[0], "cargo build");
        assert_eq!(cmds.len(), 2);
    }

    // ── truncate_tail（纯函数） ──
    // ── truncate_tail (pure fn) ──

    #[test]
    fn truncate_tail_short_unchanged() {
        assert_eq!(truncate_tail("hello", 10), "hello");
    }

    #[test]
    fn truncate_tail_exact_boundary() {
        assert_eq!(truncate_tail("hello", 5), "hello");
    }

    #[test]
    fn truncate_tail_long_keeps_tail() {
        assert_eq!(truncate_tail("abcdefghij", 3), "hij");
    }

    #[test]
    fn truncate_tail_empty_stays_empty() {
        assert_eq!(truncate_tail("", 10), "");
    }

    #[test]
    fn truncate_tail_multibyte_no_split() {
        // 中文字符占 3 字节；在字节中间截断后必须回退到字符边界。
        // Chinese chars are 3 bytes; truncating mid-char must step to a boundary.
        let s = "abc你好世界";
        // 截取尾部 4 字节：需要从字节 10 开始（"好" = 3B），但 4 字节起点落在
        // "你"的中间，必须回退到 "好" 的起点（字节 9）。
        let tail = truncate_tail(s, 4);
        // 结果必须是原字符串的有效后缀。
        // Result must be a valid suffix of the original.
        assert!(s.ends_with(tail), "{tail} should be a suffix of {s}");
        assert!(tail.chars().count() <= s.chars().count());
    }

    // ── next_gate_decision（纯函数，穷尽测试） ──
    // ── next_gate_decision (pure fn, exhaustive) ──

    #[test]
    fn gate_decision_proceed_on_passed() {
        let outcome = VerifyOutcome::Passed {
            ran: vec!["cargo build".into()],
        };
        assert_eq!(next_gate_decision(&outcome, 0), GateDecision::Proceed);
        assert_eq!(next_gate_decision(&outcome, 5), GateDecision::Proceed);
    }

    #[test]
    fn gate_decision_proceed_on_skipped() {
        let outcome = VerifyOutcome::Skipped {
            reason: "no commands".into(),
        };
        assert_eq!(next_gate_decision(&outcome, 0), GateDecision::Proceed);
    }

    #[test]
    fn gate_decision_proceed_on_unavailable() {
        let outcome = VerifyOutcome::Unavailable {
            command: "cargo".into(),
        };
        assert_eq!(next_gate_decision(&outcome, 0), GateDecision::Proceed);
        assert_eq!(next_gate_decision(&outcome, 3), GateDecision::Proceed);
    }

    #[test]
    fn gate_decision_retry_on_failed_with_retries() {
        let outcome = VerifyOutcome::Failed {
            command: "cargo build".into(),
            output_tail: "error[E0308]...".into(),
            exit_code: Some(1),
        };
        let decision = next_gate_decision(&outcome, 2);
        assert!(matches!(decision, GateDecision::Retry(_)));
        if let GateDecision::Retry(ctx) = decision {
            assert!(ctx.contains("cargo build"));
            assert!(ctx.contains("error[E0308]"));
        }
    }

    #[test]
    fn gate_decision_giveup_on_failed_no_retries() {
        let outcome = VerifyOutcome::Failed {
            command: "cargo test".into(),
            output_tail: "panicked".into(),
            exit_code: Some(101),
        };
        let decision = next_gate_decision(&outcome, 0);
        assert!(matches!(decision, GateDecision::GiveUp(_)));
        if let GateDecision::GiveUp(reason) = decision {
            assert!(reason.contains("cargo test"));
        }
    }

    // ── run_commands（执行测试，使用 universally available/absent 命令） ──
    // ── run_commands (execution tests with universally available/absent cmds) ──

    #[tokio::test]
    async fn run_commands_true_passes() {
        let tmp = TempDir::new("true");
        let cmds = vec!["true".to_string()];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        assert!(matches!(outcome, VerifyOutcome::Passed { ref ran } if ran.len() == 1));
    }

    #[tokio::test]
    async fn run_commands_false_fails() {
        let tmp = TempDir::new("false");
        let cmds = vec!["false".to_string()];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        match outcome {
            VerifyOutcome::Failed {
                command,
                exit_code,
                ..
            } => {
                assert_eq!(command, "false");
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_commands_nonexistent_is_unavailable() {
        // 退出码 127（命令未找到）→ Unavailable，不是 Failed。
        // Exit code 127 (command not found) → Unavailable, not Failed.
        let tmp = TempDir::new("nonexistent");
        let cmds = vec!["nonexistent-cmd-xyz-12345".to_string()];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        assert!(matches!(outcome, VerifyOutcome::Unavailable { ref command } if command == "nonexistent-cmd-xyz-12345"));
    }

    #[tokio::test]
    async fn run_commands_stops_at_first_failure() {
        let tmp = TempDir::new("stop");
        let cmds = vec!["false".to_string(), "true".to_string()];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        // false 失败后不应继续运行 true。
        // After false fails, true should not be run.
        assert!(matches!(outcome, VerifyOutcome::Failed { command, .. } if command == "false"));
    }

    #[tokio::test]
    async fn run_commands_multiple_pass() {
        let tmp = TempDir::new("multi");
        let cmds = vec!["true".to_string(), "true".to_string()];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        assert!(matches!(outcome, VerifyOutcome::Passed { ran } if ran.len() == 2));
    }

    #[tokio::test]
    async fn run_commands_empty_returns_passed() {
        let tmp = TempDir::new("empty");
        let cmds: Vec<String> = vec![];
        let outcome = run_commands(&cmds, &tmp, Duration::from_secs(10)).await;
        assert!(matches!(outcome, VerifyOutcome::Passed { ran } if ran.is_empty()));
    }

    #[tokio::test]
    async fn run_single_command_timeout() {
        // 超时命令应返回 Failed 并标注超时。
        // A timing-out command should return Failed with a timeout note.
        let tmp = TempDir::new("timeout");
        let outcome =
            run_single_command("sleep 30", &tmp, Duration::from_millis(100)).await;
        match outcome {
            SingleOutcome::Failed {
                output_tail, ..
            } => {
                assert!(output_tail.contains("超时") || output_tail.contains("timed out"));
            }
            other => panic!("expected Failed (timeout), got {other:?}"),
        }
    }
}
