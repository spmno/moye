// 内置工具模块：为 Agent 提供文件读写与命令执行能力。
// 各工具的 `description()` 是面向模型（LLM）的自然语言提示，统一使用中文
// （本项目主要使用中文模型：DeepSeek / GLM / Kimi）。
use anyhow::Result;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

/// 工具统一错误类型。
#[derive(Debug, thiserror::Error)]
#[error("tool error: {0}")]
struct ToolError(String);

#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
}

struct ReadFile;

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = ToolError;
    type Args = ReadFileArgs;
    type Output = String;

    fn description(&self) -> String {
        "从项目工作树读取一个 UTF-8 文本文件。".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件的相对路径" }
            },
            "required": ["path"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        std::fs::read_to_string(&args.path).map_err(|e| ToolError(e.to_string()))
    }
}

#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    old: String,
    new: String,
}

struct EditFile;

impl Tool for EditFile {
    const NAME: &'static str = "edit_file";
    type Error = ToolError;
    type Args = EditFileArgs;
    type Output = String;

    fn description(&self) -> String {
        "把文件中第一次出现的 `old` 替换为 `new`。".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old": { "type": "string", "description": "要被替换的精确文本" },
                "new": { "type": "string", "description": "替换后的文本" }
            },
            "required": ["path", "old", "new"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let content = std::fs::read_to_string(&args.path).map_err(|e| ToolError(e.to_string()))?;
        if !content.contains(&args.old) {
            return Err(ToolError("old text not found in file".into()));
        }
        let updated = content.replacen(&args.old, &args.new, 1);
        std::fs::write(&args.path, updated).map_err(|e| ToolError(e.to_string()))?;
        Ok(format!("edited {}", args.path))
    }
}

#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

struct WriteFile;

impl Tool for WriteFile {
    const NAME: &'static str = "write_file";
    type Error = ToolError;
    type Args = WriteFileArgs;
    type Output = String;

    fn description(&self) -> String {
        "用给定内容创建或覆盖一个文件。".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件的相对路径" },
                "content": { "type": "string", "description": "要写入的完整文件内容" }
            },
            "required": ["path", "content"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        std::fs::write(&args.path, &args.content).map_err(|e| ToolError(e.to_string()))?;
        Ok(format!("wrote {}", args.path))
    }
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

struct RunBash;

impl Tool for RunBash {
    const NAME: &'static str = "run_bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        "在项目工作树内运行一条 shell 命令，返回 stdout+stderr。".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string", "description": "要运行的 shell 命令" } },
            "required": ["command"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .output()
            .map_err(|e| ToolError(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        Ok(format!(
            "exit={}\nstdout:\n{}\nstderr:\n{}",
            out.status.code().unwrap_or(-1),
            stdout,
            stderr
        ))
    }
}

/// 判断 shell 命令是否为只读（可安全自动执行）还是会改变状态（需询问）。
/// 拿不准时返回 false —— 循环会将其视为"会改变状态"并询问人类，
/// 因为自动执行破坏性命令比多一次确认更危险。
pub fn is_readonly_bash(command: &str) -> bool {
    const READONLY_PREFIXES: &[&str] = &[
        "ls", "cat", "head", "tail", "grep", "git status", "git log", "git diff", "git show",
        "pwd", "echo", "find", "wc", "tree", "which", "readlink",
    ];
    for segment in command.split(|c| c == '|' || c == ';' || c == '&' || c == '\n') {
        let s = segment.trim();
        if s.is_empty() {
            return false;
        }
        // Redirection / appending writes to a file: mutating, not read-only.
        if s.contains('>') || s.contains("2>") {
            return false;
        }
        if !READONLY_PREFIXES.iter().any(|p| s.starts_with(p)) {
            return false;
        }
    }
    true
}

/// 内置工具集合 + 动态工具（tools_ext）。
/// 动态工具由 /add-tool 命令生成，需重新 cargo build 后生效。
pub fn builtin_tools() -> Result<Vec<Box<dyn rig_core::tool::ToolDyn>>> {
    let mut tools: Vec<Box<dyn rig_core::tool::ToolDyn>> = vec![
        Box::new(ReadFile),
        Box::new(EditFile),
        Box::new(WriteFile),
        Box::new(RunBash),
    ];
    // 加载动态工具（由 ToolManifest 重新生成的 mod.rs 提供）
    tools.extend(crate::tools_ext::load_all());
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_commands_classified() {
        assert!(is_readonly_bash("ls -la"));
        assert!(is_readonly_bash("cat file.txt"));
        assert!(is_readonly_bash("git status"));
        assert!(is_readonly_bash("grep -r foo src | head"));
        assert!(is_readonly_bash("git log --oneline"));
    }

    #[test]
    fn mutating_commands_not_readonly() {
        assert!(!is_readonly_bash("rm -rf x"));
        assert!(!is_readonly_bash("git commit -m x"));
        assert!(!is_readonly_bash("cargo build"));
        assert!(!is_readonly_bash("ls && rm x"));
        assert!(!is_readonly_bash("echo hi > file"));
        assert!(!is_readonly_bash(""));
    }
}
