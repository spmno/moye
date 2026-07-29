// 代码自修改模块：对源码做精确 old->new 替换，再用 `cargo build` 验证。
// Code self-modification module: performs an exact old->new replacement on source, then verifies with `cargo build`.
// 失败则回退到修改前内容，并把编译错误返回，便于上层 LLM 自我纠正后重试。
// On failure it reverts to the pre-edit content and returns the compile errors so the upstream LLM can self-correct and retry.
// 这是 OMO 子代理改代码的真实 Rust 版本：编译即验证关卡。
// This is the real-Rust version of an OMO subagent editing code: the compile step is the verification gate.
use anyhow::Result;
use std::process::Command;

/// 对源文件做精确 old->new 替换，然后用 `cargo build` 验证。失败时文件回退到
/// 修改前内容，并返回编译错误，以便 LLM 调用方自我纠正后重试。这是 OMO 子代理
/// 编辑代码库的现实 Rust 类比：编译即验证关卡。
/// Performs an exact old->new replacement on a source file, then verifies with `cargo build`. On failure the file
/// reverts to its pre-edit content and the compile errors are returned so the calling LLM can self-correct and retry.
/// This is the real-Rust analogue of an OMO subagent editing a codebase: the compile step is the verification gate.
pub fn evolve_code(file: &str, old: &str, new: &str) -> Result<String> {
    let content = std::fs::read_to_string(file)?;
    if !content.contains(old) {
        return Err(anyhow::anyhow!("old text not found in {file}"));
    }
    // Back up the pre-edit content so we can restore it deterministically on
    // failure (works regardless of git tracking state).
    // 备份修改前的内容，以便失败时确定性地还原（与 git 跟踪状态无关）。
    let backup = format!("{file}.evo.bak");
    std::fs::write(&backup, &content)?;
    let updated = content.replacen(old, new, 1);
    std::fs::write(file, updated)?;

    match run_build() {
        Ok(out) if out.status.success() => {
            let _ = std::fs::remove_file(&backup);
            let _ = run_tests();
            Ok(format!("evolved {file} (build passed)"))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            revert(file, &backup);
            Err(anyhow::anyhow!("build failed; reverted {file}. error:\n{stderr}"))
        }
        Err(e) => {
            revert(file, &backup);
            Err(e)
        }
    }
}

/// 运行 `cargo build` 并返回其输出。
/// Runs `cargo build` and returns its output.
fn run_build() -> Result<std::process::Output> {
    Command::new("cargo")
        .args(["build"])
        .output()
        .map_err(|e| anyhow::anyhow!("cargo build failed to spawn: {e}"))
}

/// 运行 `cargo test` 并返回其输出。
/// Runs `cargo test` and returns its output.
fn run_tests() -> Result<std::process::Output> {
    Command::new("cargo")
        .args(["test"])
        .output()
        .map_err(|e| anyhow::anyhow!("cargo test failed to spawn: {e}"))
}

/// 从备份还原文件并删除备份文件。
/// Restores the file from the backup and removes the backup file.
fn revert(file: &str, backup: &str) {
    // Restore the pre-edit content from the backup, then remove the backup.
    // 从备份还原修改前的内容，然后删除备份文件。
    if let Ok(prev) = std::fs::read_to_string(backup) {
        let _ = std::fs::write(file, prev);
    }
    let _ = std::fs::remove_file(backup);
}
