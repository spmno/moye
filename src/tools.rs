// 内置工具模块：为 Agent 提供文件读写、命令执行与联网搜索能力。
// Built-in tools module: provides the Agent with file I/O, command execution, and web search.
// 各工具的 `description()` 是面向模型（LLM）的自然语言提示，统一使用中文
// The `description()` of each tool is a natural-language prompt aimed at the model (LLM);
// （本项目主要使用中文模型：DeepSeek / GLM / Kimi）。
// (this project primarily uses Chinese models: DeepSeek / GLM / Kimi).
use std::collections::HashMap;
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Result;
use html2md_rs::to_md::safe_from_html_to_md;
use rig_core::tool::PortableTool;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::event::{AgentEvent, EventSender, TodoItem, TodoStatus};
use crate::seam::{FileSystemProvider, SandboxProvider, ShellExecutor};
use crate::shell::{BackgroundRegistry, LazyShell};

/// 3-stage waterfall 工具执行管线（todo 9）。
/// 3-stage waterfall tool execution pipeline (todo 9).
#[allow(dead_code)] // infrastructure for future phases
pub mod pipeline;

/// 工具统一错误类型。
/// Unified error type for tools.
#[derive(Debug, thiserror::Error)]
#[error("tool error: {0}")]
struct ToolError(String);

/// `read_file` 工具的输入参数：仅一个文件路径。
/// Input args for the `read_file` tool: just a file path.
#[derive(Deserialize)]
struct ReadFileArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// 读取项目工作树内 UTF-8 文本文件的工具。
/// Tool that reads a UTF-8 text file from the project worktree.
struct ReadFile {
    max_read_lines: usize,
}

/// 实现 `read_file` 工具：按路径读取文件内容并返回字符串。
/// Implements the `read_file` tool: reads file content by path and returns a string.
impl PortableTool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = ToolError;
    type Args = ReadFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        format!(
            "从项目工作树读取一个 UTF-8 文本文件。支持 offset/limit 分页读取大文件。默认从第0行开始，读取前{}行。使用 offset 跳过已读部分。",
            self.max_read_lines
        )
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件的相对路径" },
                "offset": { "type": "number", "description": "起始行号（从0开始），默认0", "default": 0 },
                "limit": { "type": "number", "description": "读取行数，默认为工具配置的最大行数", "default": self.max_read_lines }
            },
            "required": ["path"],
        })
    }

    /// 执行工具：读取文件内容，支持 offset/limit 分页。失败时返回 `ToolError`。
    /// Executes the tool: reads file content with optional offset/limit pagination. Returns `ToolError` on failure.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = crate::sandbox::expand_tilde(&args.path);
        let content = std::fs::read_to_string(&path).map_err(|e| ToolError(e.to_string()))?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(String::new());
        }
        let offset = args.offset.unwrap_or(0).min(total);
        let limit = args.limit.unwrap_or(self.max_read_lines);
        let end = (offset + limit).min(total);
        let selected = lines[offset..end].join("\n");
        if offset == 0 && end == total {
            Ok(selected)
        } else {
            let start = offset + 1;
            Ok(format!(
                "{selected}\n…(截断 / truncated, {total} lines total, showing lines {start}-{end}, use offset={end} to read the next batch)"
            ))
        }
    }
}

/// `edit_file` 工具的输入参数。
///
/// 支持两种互斥形式：
/// - 单次替换：`{path, old, new}` —— 把文件中首次出现的 `old` 替换为 `new`。
/// - 批量替换：`{path, edits: [{old, new}, ...]}` —— 按顺序原子应用全部对；
///   任一对不匹配则不修改文件。
///
/// Input args for the `edit_file` tool.
///
/// Supports two mutually-exclusive forms:
/// - Single: `{path, old, new}` — replaces the first occurrence of `old` with `new`.
/// - Multi: `{path, edits: [{old, new}, ...]}` — applies all pairs atomically in order;
///   if any pair is missing, nothing is written.
#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    #[serde(default)]
    edits: Option<Vec<EditPair>>,
}

/// 批量编辑中的一对替换：`{old, new}`。
/// A single replacement pair in a multi-edit: `{old, new}`.
#[derive(Deserialize)]
struct EditPair {
    old: String,
    new: String,
}

/// 模糊匹配提示：当 `old` 精确匹配失败时，定位文件中最相似的位置。
/// Fuzzy hint: when an exact `old` match fails, locates the closest region in the file.
///
/// 设计决策：仅提示，绝不自动应用模糊匹配 —— 静默近似替换可能插入错误代码，
/// 由模型重新发起编辑。
/// Design decision: hint only, NEVER auto-apply a fuzzy match — a silent near-match
/// replacement can insert wrong code; the model re-issues the edit.
#[derive(Debug)]
struct ClosestMatch {
    /// 起始行号（1-based）。
    /// Start line (1-based).
    start_line: usize,
    /// 结束行号（1-based, 含）。
    /// End line (1-based, inclusive).
    end_line: usize,
    /// 相似度 0.0–1.0。
    /// Similarity 0.0–1.0.
    similarity: f64,
    /// 该位置的文件文本片段（已截断）。
    /// The file text snippet at that location (truncated).
    snippet: String,
}

/// 在 `file_content` 中滑动窗口寻找与 `needle` 最相似的文本段。
/// Slides a window over `file_content` to find the region most similar to `needle`.
///
/// 窗口大小为 `needle.lines().count()` 及其 ±1 行变体；逐个用
/// `similar::TextDiff::from_lines(window, needle).ratio()` 评分，取最高。
/// 最高相似度 < 0.5 时返回 None（无有用信息可展示）。
/// Window size is `needle.lines().count()` with ±1 line variants; each scored by
/// `similar::TextDiff::from_lines(window, needle).ratio()`, keeping the best.
/// Returns None when best similarity < 0.5 (nothing useful to show).
fn closest_match(file_content: &str, needle: &str, snippet_max_lines: usize) -> Option<ClosestMatch> {
    let file_lines: Vec<&str> = file_content.lines().collect();
    if file_lines.is_empty() || needle.is_empty() {
        return None;
    }
    let needle_lines: Vec<&str> = needle.lines().collect();
    let needle_count = needle_lines.len().max(1);
    let needle_text = needle_lines.join("\n");

    // 尝试 needle_count 及其 ±1 行窗口大小 / try needle_count and ±1 line window sizes
    let mut best: Option<(usize, usize, f32)> = None; // (start_idx, window_size, similarity)
    for &window_size in &[needle_count.saturating_sub(1), needle_count, needle_count + 1] {
        if window_size == 0 || window_size > file_lines.len() {
            continue;
        }
        for start in 0..=(file_lines.len() - window_size) {
            let window_text: String = file_lines[start..start + window_size].join("\n");
            let ratio = similar::TextDiff::from_lines(&window_text, &needle_text).ratio();
            if best.map_or(true, |(_, _, prev)| ratio > prev) {
                best = Some((start, window_size, ratio));
            }
        }
    }

    let (start_idx, window_size, ratio) = best?;
    if ratio < 0.5 {
        return None;
    }

    let start_line = start_idx + 1; // 1-based
    let end_line = start_idx + window_size; // 1-based inclusive

    // 构建截断 snippet：先按行截断，再按字符截断到 ~800 字符。
    // Build truncated snippet: cap lines first, then cap chars to ~800.
    let end_idx = (start_idx + window_size).min(file_lines.len());
    let raw_lines = &file_lines[start_idx..end_idx];
    let mut snippet = if raw_lines.len() > snippet_max_lines {
        let kept = &raw_lines[..snippet_max_lines];
        format!(
            "{}\n\u{2026}({} more lines)",
            kept.join("\n"),
            raw_lines.len() - snippet_max_lines
        )
    } else {
        raw_lines.join("\n")
    };
    const MAX_SNIPPET_CHARS: usize = 800;
    if snippet.chars().count() > MAX_SNIPPET_CHARS {
        let end = snippet
            .char_indices()
            .take_while(|(i, _)| *i <= MAX_SNIPPET_CHARS)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(MAX_SNIPPET_CHARS);
        snippet.truncate(end);
        snippet.push('\u{2026}');
    }

    Some(ClosestMatch {
        start_line,
        end_line,
        similarity: ratio as f64,
        snippet,
    })
}

/// 构建"未找到 old 文本"的错误信息，含最近似匹配的行号、相似度与片段。
/// Builds the "old text not found" error message with closest-match line range,
/// similarity percentage, and a snippet of the file text at that location.
///
/// `pair_label` 为 `Some((k, n))` 时在消息前缀添加 `edit pair {k} of {n}: `（批量编辑，
/// k 为 1-based 索引）。单次替换形式传 None。
/// When `pair_label` is `Some((k, n))`, prefixes the message with
/// `edit pair {k} of {n}: ` (multi-edit, k is 1-based). Single-form passes None.
fn not_found_error(content: &str, needle: &str, pair_label: Option<(usize, usize)>) -> ToolError {
    let prefix = match pair_label {
        Some((k, n)) => format!("edit pair {k} of {n}: "),
        None => String::new(),
    };
    let body = match closest_match(content, needle, 15) {
        Some(m) => {
            let pct = (m.similarity * 100.0).round() as u8;
            format!(
                "old text not found in file. Closest match at lines {}-{} (similarity {}%):\n{}",
                m.start_line, m.end_line, pct, m.snippet
            )
        }
        None => "old text not found in file".to_string(),
    };
    let msg = format!("{prefix}{body}");
    // 总消息长度控制在 ~1000 字符以内 / cap total message under ~1000 chars
    const MAX_MSG_CHARS: usize = 1000;
    let msg = if msg.chars().count() > MAX_MSG_CHARS {
        let end = msg
            .char_indices()
            .take_while(|(i, _)| *i <= MAX_MSG_CHARS)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(MAX_MSG_CHARS);
        format!("{}\u{2026}", &msg[..end])
    } else {
        msg
    };
    ToolError(msg)
}

/// 对文件做精确文本替换的工具。
/// Tool that performs exact text replacement in a file.
struct EditFile {
    /// 文件检查点存储——在写入前快照文件原始内容。
    /// File checkpoint store — snapshots original content before writing.
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
}

/// 实现 `edit_file` 工具。
///
/// 单次形式 `{path, old, new}`：把文件中首次出现的 `old` 替换为 `new`，行为与旧版一致。
/// 批量形式 `{path, edits: [{old, new}, ...]}`：两阶段应用——先在内存中按序校验全部
/// 替换对（pair N 可能 patch pair N-1 的输出，故校验须基于当前内存状态），任一对不匹配
/// 则不写盘并报错（错误含最近似匹配提示与失败对的 1-based 索引）；全部通过后写一次。
///
/// Implements the `edit_file` tool.
///
/// Single form `{path, old, new}`: replaces the first occurrence of `old` with `new`
/// (byte-identical to legacy).
/// Multi form `{path, edits: [{old, new}, ...]}`: two-phase apply — validate ALL pairs
/// against the in-memory sequential state first (pair N may patch text produced by
/// pair N-1, so validation simulates apply-then-check); if any pair is missing, write
/// NOTHING and report the failing pair index (1-based) with its fuzzy hint; if all
/// pass, write the final content once.
impl PortableTool for EditFile {
    const NAME: &'static str = "edit_file";
    type Error = ToolError;
    type Args = EditFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "\u{7cbe}\u{786e}\u{6587}\u{672c}\u{66ff}\u{6362}\u{5de5}\u{5177}\u{3002}\
         \u{5355}\u{6b21}\u{66ff}\u{6362}\u{7528} {path, old, new}\u{ff08}\u{66ff}\u{6362}\u{9996}\u{6b21}\u{51fa}\u{73b0}\u{7684} old\u{ff09}\u{ff1b}\
         \u{6279}\u{91cf}\u{66ff}\u{6362}\u{7528} {path, edits: [{old, new}, ...]}\u{ff08}\u{6309}\u{5e8}\u{539f}\u{5b50}\u{5e94}\u{7528}\u{ff0c}\
         \u{5168}\u{90e8}\u{6821}\u{9a8c}\u{901a}\u{8fc7}\u{624d}\u{5199}\u{5165}\u{ff0c}\u{4efb}\u{4e00}\u{5bf9}\u{4e0d}\u{5339}\u{914d}\u{5219}\u{4e0d}\u{4fee}\u{6539}\u{6587}\u{4ef6}\u{ff09}\u{3002}\
         old \u{4e0d}\u{5339}\u{914d}\u{65f6}\u{9519}\u{8bef}\u{542b}\u{6700}\u{8fd1}\u{4f3c}\u{5339}\u{914d}\u{7684}\u{884c}\u{53f7}\u{4e0e}\u{76f8}\u{4f3c}\u{5ea6}\u{ff0c}\u{4fbf}\u{4e8e}\u{4e00}\u{6b21}\u{6027}\u{4fee}\u{6b63}\u{3002}"
            .to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "\u{6587}\u{4ef6}\u{7684}\u{76f8}\u{5bf9}\u{8def}\u{5f84}" },
                "old": { "type": "string", "description": "\u{5355}\u{6b21}\u{66ff}\u{6362}\u{ff1a}\u{8981}\u{88ab}\u{66ff}\u{6362}\u{7684}\u{7cbe}\u{786e}\u{6587}\u{672c}\u{ff08}\u{4e0e} new \u{914d}\u{5bf9}\u{4f7f}\u{7528}\u{ff09}" },
                "new": { "type": "string", "description": "\u{5355}\u{6b21}\u{66ff}\u{6362}\u{ff1a}\u{66ff}\u{6362}\u{540e}\u{7684}\u{6587}\u{672c}\u{ff08}\u{4e0e} old \u{914d}\u{5bf9}\u{4f7f}\u{7528}\u{ff09}" },
                "edits": {
                    "type": "array",
                    "description": "\u{6279}\u{91cf}\u{66ff}\u{6362}\u{ff1a}{old, new} \u{5bf9}\u{7684}\u{6570}\u{7ec4}\u{ff0c}\u{6309}\u{987a}\u{5e8}\u{539f}\u{5b50}\u{5e94}\u{7528}\u{ff08}\u{5168}\u{90e8}\u{6821}\u{9a8c}\u{901a}\u{8fc7}\u{624d}\u{5199}\u{5165}\u{ff09}",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": { "type": "string", "description": "\u{8981}\u{88ab}\u{66ff}\u{6362}\u{7684}\u{7cbe}\u{786e}\u{6587}\u{672c}" },
                            "new": { "type": "string", "description": "\u{66ff}\u{6362}\u{540e}\u{7684}\u{6587}\u{672c}" }
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    /// 执行工具：归一化参数形式，两阶段校验后写回。
    /// Executes the tool: normalizes the form, two-phase validates, then writes back.
    ///
    /// 形式归一化：单次 `{old, new}` → 含一对的 vec；批量 `edits` → 直接取；
    /// 两种形式同时出现或都没有 → 清晰错误。
    /// Form normalization: single `{old, new}` → a one-pair vec; multi `edits` → as-is;
    /// both present or neither → clear error.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = crate::sandbox::expand_tilde(&args.path);

        // ── 形式归一化 / form normalization ──
        let has_single = args.old.is_some() || args.new.is_some();
        let has_multi = args.edits.is_some();
        if has_single && has_multi {
            return Err(ToolError(
                "cannot specify both `edits` array and `old`/`new` fields; use one form".into(),
            ));
        }
        let is_multi = has_multi;
        let pairs: Vec<(String, String)> = if let Some(edits) = args.edits {
            edits.into_iter().map(|p| (p.old, p.new)).collect()
        } else {
            match (args.old, args.new) {
                (Some(old), Some(new)) => vec![(old, new)],
                _ => {
                    return Err(ToolError(
                        "must specify either `edits` array or both `old`+`new` fields".into(),
                    ))
                }
            }
        };
        if pairs.is_empty() {
            return Err(ToolError("`edits` array must not be empty".into()));
        }
        let n = pairs.len();

        let content = std::fs::read_to_string(&path).map_err(|e| ToolError(e.to_string()))?;

        // 在写入前记录检查点（first-write-wins：同一任务内同一路径只记录首次）。
        // Record checkpoint before writing (first-write-wins: same path in same task recorded once).
        self.checkpoints
            .record(self.checkpoints.current_task(), &path, Some(content.clone()));

        // ── Phase 1: 在内存中按序校验全部替换对 ──
        // pair N 可能 patch pair N-1 的输出，故校验须基于当前内存状态。
        // Phase 1: validate ALL pairs against the in-memory sequential state.
        // Pair N may patch text produced by pair N-1, so validation simulates
        // apply-then-check against the running in-memory string.
        let mut working = content;
        for (i, (old, new)) in pairs.iter().enumerate() {
            if !working.contains(old.as_str()) {
                let label = if is_multi {
                    Some((i + 1, n)) // 1-based for humans
                } else {
                    None
                };
                return Err(not_found_error(&working, old.as_str(), label));
            }
            working = working.replacen(old.as_str(), new.as_str(), 1);
        }

        // ── Phase 2: 全部校验通过 → 写一次 ──
        // Phase 2: all pairs valid → write the final content once.
        std::fs::write(&path, &working).map_err(|e| ToolError(e.to_string()))?;

        if is_multi {
            Ok(format!("edited {} ({} edits)", args.path, n))
        } else {
            Ok(format!("edited {}", args.path))
        }
    }
}

/// `write_file` 工具的输入参数：路径 + 完整文件内容。
/// Input args for the `write_file` tool: path + full file content.
#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

/// 用给定内容创建或覆盖文件的工具。
/// Tool that creates or overwrites a file with the given content.
struct WriteFile {
    checkpoints: Arc<crate::checkpoint::CheckpointStore>,
}

/// 实现 `write_file` 工具：用给定内容创建或覆盖一个文件。
/// Implements the `write_file` tool: creates or overwrites a file with the given content.
impl PortableTool for WriteFile {
    const NAME: &'static str = "write_file";
    type Error = ToolError;
    type Args = WriteFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "创建一个新文件，或用给定的完整内容覆盖已存在的文件。\
         当用户要求『写/生成/创建一个文件』（如 HTML、脚本、配置、文档等）时，必须使用本工具，\
         把完整文件内容放入 content 参数，不要只在回复里贴代码。"
            .to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
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

    /// 执行工具：写入文件并返回确认信息。
    /// Executes the tool: writes the file and returns a confirmation message.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = crate::sandbox::expand_tilde(&args.path);
        // 写入前读取当前内容（None=文件不存在），记录检查点。
        // Read current content before writing (None=file absent), record checkpoint.
        let prior = std::fs::read_to_string(&path).ok();
        self.checkpoints
            .record(self.checkpoints.current_task(), &path, prior);
        std::fs::write(&path, &args.content).map_err(|e| ToolError(e.to_string()))?;
        Ok(format!("wrote {}", args.path))
    }
}

/// `run_bash` 工具的输入参数：一条 shell 命令 + 可选后台标志。
/// Input args for the `run_bash` tool: a shell command + optional background flag.
#[derive(Deserialize)]
struct BashArgs {
    command: String,
    /// true=后台运行（长命令如 dev server / watch），返回 shell id。
    /// true=run in background (long commands like dev server / watch), returns shell id.
    #[serde(default)]
    background: bool,
}

/// 在项目工作树内运行 shell 命令的工具。
/// Tool that runs a shell command inside the project worktree.
struct RunBash {
    max_bash_output_chars: usize,
    sandbox: Arc<dyn SandboxProvider>,
    timeout_secs: u64,
    /// Per-agent-run persistent shell (cwd/env survive across calls).
    /// 每 agent-run 持久 shell（cwd/env 跨调用保留）。
    shells: Arc<LazyShell>,
    /// Shared background registry (shells outlive the turn).
    /// 共享后台注册表（shell 寿命超过单轮）。
    bg: Arc<BackgroundRegistry>,
}

/// 实现 `run_bash` 工具：运行 shell 命令并返回 stdout+stderr。
/// Implements the `run_bash` tool: runs a shell command and returns stdout+stderr.
impl PortableTool for RunBash {
    const NAME: &'static str = "run_bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        format!(
            "在项目工作树内运行一条 shell 命令，返回 stdout+stderr。同一 agent run 内 cd/export 等状态会持久化。输出截断到 {} 字符。设置 background=true 可在后台启动长命令（开发服务器/watch 模式），返回 shell id；用 bash_output 读取输出、kill_shell 停止。",
            self.max_bash_output_chars
        )
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要运行的 shell 命令" },
                "background": { "type": "boolean", "default": false, "description": "true=后台运行（长命令如 dev server / watch），返回 shell id" }
            },
            "required": ["command"],
        })
    }

    /// 执行工具：前台通过持久 shell（LazyShell），后台通过 BackgroundRegistry。
    /// Executes the tool: foreground via persistent shell (LazyShell), background via registry.
    ///
    /// 前台路径：替换原有的每调用 spawn，使用同一 agent run 内持久化的 shell
    /// （cd/export 跨调用保留）。输出格式与原实现完全一致（stdout/stderr 分离、
    /// 截断、退出码报告）。
    /// 后台路径：启动分离的 `sh -c` 子进程，返回 shell id。
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if args.background {
            let id = self
                .bg
                .start(&args.command, self.sandbox.as_ref())
                .map_err(|e| ToolError(e.to_string()))?;
            return Ok(format!(
                "started background shell {id} (bash_output to read, kill_shell to stop)"
            ));
        }

        let timeout = Duration::from_secs(self.timeout_secs);
        let (stdout, stderr, code) = self
            .shells
            .exec(&args.command, timeout)
            .await
            .map_err(ToolError)?;

        let stdout = stdout.trim().to_string();
        let stderr = stderr.trim().to_string();
        let combined = if code == 0 {
            if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                format!("(stderr) {}", stderr)
            } else {
                format!("{}\n(stderr) {}", stdout, stderr)
            }
        } else {
            let mut parts = vec![format!("(exit {})", code)];
            if !stdout.is_empty() {
                parts.push(stdout);
            }
            if !stderr.is_empty() {
                parts.push(format!("(stderr) {}", stderr));
            }
            parts.join("\n")
        };
        Ok(crate::context::truncate_at_char_boundary(
            &combined,
            self.max_bash_output_chars,
        ))
    }
}

// ─── bash_output / kill_shell 工具 ──────────────────────────────────────
// ─── bash_output / kill_shell tools ──────────────────────────────────────

/// `bash_output` 工具的输入参数：后台 shell id。
/// Input args for the `bash_output` tool: a background shell id.
#[derive(Deserialize)]
struct BashOutputArgs {
    id: String,
}

/// 读取后台 shell 累积输出与状态的工具。
/// Tool that reads accumulated output + status of a background shell.
struct BashOutput {
    bg: Arc<BackgroundRegistry>,
}

impl PortableTool for BashOutput {
    const NAME: &'static str = "bash_output";
    type Error = ToolError;
    type Args = BashOutputArgs;
    type Output = String;

    fn description(&self) -> String {
        "读取后台 shell 的累积输出与状态（running / exited(N)）".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "后台 shell id（bg-N）" }
            },
            "required": ["id"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        match self.bg.read(&args.id) {
            Some((buffer, exit)) => {
                let status = match exit {
                    Some(code) => format!("exited({code})"),
                    None => "running".to_string(),
                };
                Ok(format!("status: {status}\n{buffer}"))
            }
            None => Ok(format!("{} not found", args.id)),
        }
    }
}

/// `kill_shell` 工具的输入参数：后台 shell id。
/// Input args for the `kill_shell` tool: a background shell id.
#[derive(Deserialize)]
struct KillShellArgs {
    id: String,
}

/// 终止后台 shell 的工具（发送 SIGKILL）。
/// Tool that terminates a background shell (sends SIGKILL).
struct KillShell {
    bg: Arc<BackgroundRegistry>,
}

impl PortableTool for KillShell {
    const NAME: &'static str = "kill_shell";
    type Error = ToolError;
    type Args = KillShellArgs;
    type Output = String;

    fn description(&self) -> String {
        "终止后台 shell（发送 SIGKILL）".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "后台 shell id（bg-N）" }
            },
            "required": ["id"],
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        if self.bg.kill(&args.id) {
            Ok(format!("killed {}", args.id))
        } else {
            Ok(format!("{} not found or already exited", args.id))
        }
    }
}

/// `run_file` 工具的输入参数：脚本文件路径。
/// Input args for the `run_file` tool: a script file path.
#[derive(Deserialize)]
struct RunFileArgs {
    path: String,
}

/// 执行脚本文件的工具。按扩展名自动选择解释器：
/// `.sh` → bash, `.py` → python3, `.js` → node。
/// Tool that executes a script file. Auto-selects interpreter by extension:
/// `.sh` → bash, `.py` → python3, `.js` → node.
struct RunFile {
    max_output_chars: usize,
    sandbox: Arc<dyn SandboxProvider>,
}

impl PortableTool for RunFile {
    const NAME: &'static str = "run_file";
    type Error = ToolError;
    type Args = RunFileArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "执行脚本文件并返回 stdout+stderr。按扩展名自动选择解释器：.sh→bash, .py→python3, .js→node。输出截断到 {} 字符。",
            self.max_output_chars
        )
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "脚本文件路径（支持 .sh / .py / .js）" }
            },
            "required": ["path"],
        })
    }

    /// 执行脚本：按扩展名选择解释器，收集退出码与输出，截断到 max_output_chars 字符。
    /// Executes the script: selects interpreter by extension, collects exit code and output,
    /// truncating combined stdout+stderr to max_output_chars.
    ///
    /// 与 RunBash 相同：使用 tokio::process + kill_on_drop 确保 Esc 中断时子进程被清理。
    /// Same as RunBash: uses tokio::process + kill_on_drop so Esc-abort kills the child process.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let expanded = crate::sandbox::expand_tilde(&args.path);
        let path = std::path::Path::new(&expanded);
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let interpreter = match ext {
            "sh" => "bash",
            "py" => "python3",
            "js" => "node",
            _ => {
                return Err(ToolError(format!(
                    "不支持的文件类型: .{ext}。支持 .sh / .py / .js"
                )));
            }
        };
        let out = if let Some(bwrap_argv) = self.sandbox.grant_args(&[], &[]) {
            let mut cmd = tokio::process::Command::new(&bwrap_argv[0]);
            for arg in &bwrap_argv[1..] {
                cmd.arg(arg);
            }
            cmd.arg("--")
                .arg(interpreter)
                .arg(&args.path)
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| ToolError(e.to_string()))?
        } else {
            tokio::process::Command::new(interpreter)
                .arg(&args.path)
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| ToolError(e.to_string()))?
        };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().unwrap_or(-1);
        let combined = if code == 0 {
            if stderr.is_empty() {
                stdout
            } else if stdout.is_empty() {
                format!("(stderr) {}", stderr)
            } else {
                format!("{}\n(stderr) {}", stdout, stderr)
            }
        } else {
            let mut parts = vec![format!("(exit {})", code)];
            if !stdout.is_empty() {
                parts.push(stdout);
            }
            if !stderr.is_empty() {
                parts.push(format!("(stderr) {}", stderr));
            }
            parts.join("\n")
        };
        Ok(crate::context::truncate_at_char_boundary(
            &combined,
            self.max_output_chars,
        ))
    }
}

// ─── 联网工具 ───────────────────────────────────────────────────────────
// ─── Web tools ───────────────────────────────────────────────────────────

/// 构建带代理支持的 reqwest Client。
/// 优先级：AGENT_PROXY > HTTPS_PROXY > HTTP_PROXY。均未设置时不使用代理。
/// Builds a reqwest Client with proxy support.
/// Priority: AGENT_PROXY > HTTPS_PROXY > HTTP_PROXY. No proxy when none are set.
fn build_web_client() -> std::result::Result<reqwest::Client, ToolError> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
        );
    let proxy_url = std::env::var("AGENT_PROXY")
        .or_else(|_| std::env::var("HTTPS_PROXY"))
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .ok();
    if let Some(url) = proxy_url {
        let proxy = reqwest::Proxy::https(&url)
            .or_else(|_| reqwest::Proxy::http(&url))
            .map_err(|e| ToolError(format!("代理配置无效: {e}")))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|e| ToolError(e.to_string()))
}

/// 将 HTML 转换为 Markdown，保留标题/列表/代码块/链接等结构；
/// 转换失败时退化为纯文本剥离。
/// Converts HTML to Markdown, preserving headings/lists/code blocks/links;
/// falls back to plain-text stripping on failure.
fn html_to_markdown(html: &str) -> String {
    match safe_from_html_to_md(html.to_string()) {
        Ok(md) => {
            let trimmed = md.trim();
            if trimmed.is_empty() {
                strip_html(html)
            } else {
                trimmed.to_string()
            }
        }
        Err(_) => strip_html(html),
    }
}

/// 抓取网页内容并转为纯文本返回。自动去除 HTML 标签、script/style 块，
/// 解码常见 HTML 实体，截断到合理长度以防 token 洪流。
/// Fetches a web page and returns it as plain text. Strips HTML tags, script/style
/// blocks, decodes common HTML entities, and truncates to a sane length to avoid token floods.
#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
}

/// 抓取指定 URL 并转为纯文本的工具。
/// Tool that fetches a URL and converts it to plain text.
struct WebFetch;

/// 实现 `web_fetch` 工具：抓取网页、剥离 HTML、截断并返回。
/// Implements the `web_fetch` tool: fetches a page, strips HTML, truncates, returns.
impl PortableTool for WebFetch {
    const NAME: &'static str = "web_fetch";
    type Error = ToolError;
    type Args = WebFetchArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "抓取指定 URL 的网页内容，返回 Markdown（自动去除脚本/样式，保留标题、列表、代码块与链接）。支持 HTTP/HTTPS。"
            .to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的网页 URL（需包含 http:// 或 https://）" }
            },
            "required": ["url"],
        })
    }

    /// 执行工具：发起 HTTP 请求、剥离 HTML、按字符边界截断后返回。
    /// Executes the tool: issues an HTTP request, strips HTML, truncates at char boundary, returns.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        const MAX_CONTENT: usize = 8000;
        let client = build_web_client()?;
        let resp = client.get(&args.url).send().await.map_err(|e| {
            ToolError(format!(
                "请求失败: {e}。提示: 可能需要设置代理，如 export AGENT_PROXY=http://127.0.0.1:7890"
            ))
        })?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError(format!("读取响应体失败: {e}")))?;
        let text =
            if content_type.contains("text/html") || content_type.contains("application/xhtml") {
                html_to_markdown(&body)
            } else {
                body
            };
        let truncated = if text.len() > MAX_CONTENT {
            // 先找到不超过 MAX_CONTENT 的最后一个字符边界，再切片，
            // First find the last char boundary not exceeding MAX_CONTENT, then slice,
            // 避免在多字节 UTF-8 字符中间切片导致 panic。
            // avoiding a panic from slicing in the middle of a multi-byte UTF-8 char.
            let end = text
                .char_indices()
                .take_while(|(i, _)| *i <= MAX_CONTENT)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            format!("{}…(截断，共 {} 字符)", &text[..end], text.chars().count())
        } else {
            text
        };
        Ok(format!("HTTP {status} | {content_type}\n{truncated}"))
    }
}

/// `web_search` 工具的输入参数：查询关键词 + 可选的结果数量与搜索深度。
/// Input args for the `web_search` tool: query keyword + optional result count and depth.
#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default = "default_search_depth")]
    search_depth: String,
}

fn default_max_results() -> usize {
    5
}

fn default_search_depth() -> String {
    "basic".to_string()
}

/// 进程内搜索结果缓存：同一会话内重复查询不重复消耗 Tavily 额度。
/// In-process search cache: repeated queries within a session don't re-consume Tavily quota.
static WEB_SEARCH_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 搜索网络：优先 Tavily API（结构化正文），无密钥时回退 DuckDuckGo + 全文抓取。
/// Searches the web: prefers the Tavily API (structured content), falling back to
/// DuckDuckGo + full-text fetch when no API key is configured.
///
/// 历史背景：早期使用 Instant Answer API（api.duckduckgo.com/?format=json）对绝大多数
/// 普通查询返回空结果；后改用 HTML 端点手写解析器，但仅能拿到标题+摘要（snippet），
/// 模型仍需二次 web_fetch 才能获得正文。现改为：设置 `TAVILY_API_KEY` 时走 Tavily
/// 官方 API（每条结果自带清洗后的正文 `content`，并可返回 AI 综合回答），体验对齐
/// 主流搜索工具；未配置密钥时回退到 DuckDuckGo HTML 端点，并自动抓取首条结果的
/// 全文 Markdown，省去模型的多余调用。
/// Historical note: the old DDG HTML endpoint only returned titles + snippets, forcing the
/// model to do extra web_fetch round-trips for full text. We now use the Tavily API
/// (TAVILY_API_KEY) for structured per-result content and an AI answer, matching mainstream
/// search tools; without a key we fall back to the DDG HTML endpoint and auto-fetch the top
/// result's full Markdown.
struct WebSearch;

/// 实现 `web_search` 工具：先尝试 Tavily，失败或无密钥则回退 DuckDuckGo。
/// Implements `web_search`: tries Tavily first, then falls back to DuckDuckGo.
impl PortableTool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolError;
    type Args = WebSearchArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "仅在必要时联网搜索（Tavily 有额度限制）：通用常识、训练知识内已知的事实、或已知 URL 都不要搜索；已知 URL 请直接用 web_fetch 抓取。确需搜索时返回标题、URL、正文摘要与（Tavily 时）AI 综合回答；未配置 TAVILY_API_KEY 时回退到 DuckDuckGo。"
            .to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索查询关键词" },
                "max_results": { "type": "integer", "description": "返回结果数量（1-10，默认 5）" },
                "search_depth": { "type": "string", "enum": ["basic", "advanced"], "description": "搜索深度：basic 更快省额度（默认），advanced 更全面但更贵" }
            },
            "required": ["query"],
        })
    }

    /// 执行工具：先查进程内缓存，再 Tavily，失败/无密钥则 DuckDuckGo 兜底。
    /// Executes the tool: checks the in-process cache first, then Tavily, then DuckDuckGo.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let max_results = args.max_results.clamp(1, 10);
        let depth = if args.search_depth == "basic" {
            "basic"
        } else {
            "advanced"
        };

        // 缓存命中直接返回：同一 query 同一会话内不重复烧额度。
        // Cache hit: return immediately — don't double-burn quota for the same query in a session.
        let cache_key = format!("{}|{max_results}|{depth}", args.query.trim().to_lowercase());
        if let Some(cached) = WEB_SEARCH_CACHE
            .lock()
            .ok()
            .and_then(|c| c.get(&cache_key).cloned())
        {
            return Ok(cached);
        }

        let client = build_web_client()?;

        let output: String = if let Ok(key) = std::env::var("TAVILY_API_KEY") {
            if !key.trim().is_empty() {
                match tavily_search(&client, &key, &args.query, max_results, depth).await {
                    Ok(output) => output,
                    Err(e) => {
                        let quota_exhausted = e.0.contains("额度已用尽");
                        tracing::warn!("Tavily 搜索失败，回退 DuckDuckGo: {e}");
                        let mut out = ddg_search(&client, &args.query, max_results).await?;
                        if quota_exhausted {
                            out = format!(
                                "[提示] Tavily 额度已用尽或限流，本次已改用免费 DuckDuckGo（质量可能下降）。请尽量少用 web_search，或改用 web_fetch 抓取已知 URL。\n\n{out}"
                            );
                        }
                        out
                    }
                }
            } else {
                ddg_search(&client, &args.query, max_results).await?
            }
        } else {
            ddg_search(&client, &args.query, max_results).await?
        };

        if let Ok(mut cache) = WEB_SEARCH_CACHE.lock() {
            cache.insert(cache_key, output.clone());
        }
        Ok(output)
    }
}

/// Tavily 请求体。
/// Tavily request body.
#[derive(Serialize)]
struct TavilyRequest {
    api_key: String,
    query: String,
    search_depth: String,
    max_results: usize,
    include_answer: bool,
}

/// Tavily 响应体。
/// Tavily response body.
#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    results: Vec<TavilyResult>,
}

/// Tavily 单条结果。
/// A single Tavily result.
#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
    #[serde(default)]
    score: Option<f64>,
}

/// 调用 Tavily Search API 并格式化为文本。
/// Calls the Tavily Search API and formats the response as text.
async fn tavily_search(
    client: &reqwest::Client,
    key: &str,
    query: &str,
    max_results: usize,
    depth: &str,
) -> Result<String, ToolError> {
    let req = TavilyRequest {
        api_key: key.to_string(),
        query: query.to_string(),
        search_depth: depth.to_string(),
        max_results,
        include_answer: true,
    };
    let resp = client
        .post("https://api.tavily.com/search")
        .json(&req)
        .send()
        .await
        .map_err(|e| ToolError(format!(
            "Tavily 请求失败: {e}。提示: 在中国大陆可能需要设置代理，如 export AGENT_PROXY=http://127.0.0.1:7890"
        )))?;
    let status = resp.status();
    if !status.is_success() {
        // 429 = 限流，402 = 额度/付费问题；其余为一般错误。
        // 429 = rate limited, 402 = quota/payment; the rest are generic errors.
        let code = status.as_u16();
        let msg = match code {
            429 | 402 => format!("Tavily 额度已用尽或被限流（HTTP {status}）"),
            _ => format!("Tavily 返回 HTTP {status}"),
        };
        return Err(ToolError(msg));
    }
    let parsed: TavilyResponse = resp
        .json()
        .await
        .map_err(|e| ToolError(format!("解析 Tavily 响应失败: {e}")))?;

    if parsed.results.is_empty() && parsed.answer.as_deref().is_none_or(|a| a.trim().is_empty()) {
        return Err(ToolError("Tavily 返回空结果".into()));
    }

    let mut parts = Vec::with_capacity(parsed.results.len() + 2);
    if let Some(answer) = parsed.answer.as_deref().filter(|a| !a.trim().is_empty()) {
        parts.push(format!("AI 综合回答：\n{answer}\n"));
    }
    parts.push(format!("搜索结果（共 {} 条）:", parsed.results.len()));
    for (i, r) in parsed.results.iter().enumerate() {
        let title = r.title.trim();
        let content = r.content.trim();
        let score = r
            .score
            .map(|s| format!("（相关度 {:.0}%）", (s * 100.0).round()))
            .unwrap_or_default();
        if content.is_empty() {
            parts.push(format!("  {}. {title}{score}\n     {}", i + 1, r.url));
        } else {
            parts.push(format!(
                "  {}. {title}{score}\n     {content}\n     {}",
                i + 1,
                r.url
            ));
        }
    }
    Ok(parts.join("\n"))
}

/// 回退搜索：请求 DuckDuckGo HTML 端点，解析结果并抓取首条结果的全文 Markdown。
/// Fallback search: requests the DDG HTML endpoint, parses results, and fetches the
/// top result's full Markdown (saves the model a follow-up web_fetch round-trip).
async fn ddg_search(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<String, ToolError> {
    let resp = client
        .get("https://html.duckduckgo.com/html/")
        .header("Accept", "text/html,application/xhtml+xml")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| ToolError(format!(
            "搜索请求失败: {e}。提示: 在中国大陆可能需要设置代理，如 export AGENT_PROXY=http://127.0.0.1:7890"
        )))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ToolError(format!("搜索返回 HTTP {status}")));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| ToolError(format!("读取搜索响应失败: {e}")))?;
    let results = parse_ddg_html(&body);

    if results.is_empty() {
        return Ok(format!(
            "未找到与「{query}」相关的结果。可尝试更换关键词，或用 web_fetch 直接抓取已知 URL。"
        ));
    }

    let mut parts = Vec::with_capacity(results.len() * 2 + 2);
    parts.push(format!(
        "搜索结果（共 {} 条，显示前 {} 条）:",
        results.len(),
        results.len().min(max_results)
    ));
    for (i, item) in results.iter().take(max_results).enumerate() {
        let title = item.title.trim();
        let snippet = item.snippet.trim();
        if snippet.is_empty() {
            parts.push(format!("  {}. {title}\n     {}", i + 1, item.url));
        } else {
            parts.push(format!(
                "  {}. {title}\n     {snippet}\n     {}",
                i + 1,
                item.url
            ));
        }
    }
    if results.len() > max_results {
        parts.push(format!(
            "  …（还有 {} 条结果已省略）",
            results.len() - max_results
        ));
    }

    // P3：抓取首条结果的全文 Markdown，省去模型的二次 web_fetch。
    if let Some(top) = results.first()
        && let Some(md) = fetch_page_markdown(client, &top.url, 4000).await
    {
        parts.push(format!("\n── 首条结果全文（Markdown）──\n{md}"));
    }

    Ok(parts.join("\n"))
}

/// 抓取 URL 并转换为 Markdown，返回前 `max_chars` 个字符；失败返回 None。
/// Fetches a URL and converts to Markdown, truncated to `max_chars`; returns None on failure.
async fn fetch_page_markdown(
    client: &reqwest::Client,
    url: &str,
    max_chars: usize,
) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.ok()?;
    let markdown = if ct.contains("text/html") || ct.contains("application/xhtml") {
        html_to_markdown(&body)
    } else {
        body
    };
    let (truncated, was_truncated) = truncate_chars(&markdown, max_chars);
    if was_truncated {
        Some(format!(
            "{truncated}…(截断，共 {} 字符)",
            markdown.chars().count()
        ))
    } else {
        Some(truncated)
    }
}

/// 在 UTF-8 字符边界处截断字符串，返回 (截断结果, 是否发生了截断)。
/// Truncates a string at a UTF-8 char boundary; returns (result, was_truncated).
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    (s[..end].to_string(), true)
}

/// DuckDuckGo HTML 端点解析出的单条搜索结果。
/// A single parsed search result from the DuckDuckGo HTML endpoint.
#[derive(Debug, Default)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// 从 DuckDuckGo HTML 结果页中提取搜索结果。
/// Extracts search results from a DuckDuckGo HTML SERP.
///
/// 页面结构（截至 2026 年）：每条结果包裹在
/// `<div class="result ..."> ... <a class="result__a" href="...">Title</a>
/// ... <a class="result__snippet" ...>snippet text</a> ... </div>`。
/// 链接可能是 `//duckduckgo.com/l/?uddg=<encoded target>&...` 形式的跳转链接，
/// 需要解码出真实目标 URL。我们用字符串扫描而非正则，避免额外依赖，
/// 且对 DuckDuckGo 偶尔调整 class 名有一定容忍度（按 result__a / result__snippet 子串匹配）。
/// Page structure (as of 2026): each result is wrapped in
/// `<div class="result ..."> ... <a class="result__a" href="...">Title</a>
/// ... <a class="result__snippet" ...>snippet</a> ... </div>`.
/// Links may be DDG redirects of the form `//duckduckgo.com/l/?uddg=<encoded>&...`,
/// which we decode to recover the real target. We scan by string rather than
/// regex to avoid a new dependency, and match class substrings so minor DDG
/// markup tweaks don't break us.
fn parse_ddg_html(html: &str) -> Vec<SearchResult> {
    let lower = html.to_lowercase();
    let mut results = Vec::new();
    let mut pos = 0usize;

    while pos < html.len() {
        // 找下一条结果块起点：class 中含 "result__a"
        // Find the next result block anchor: class containing "result__a"
        let anchor_rel = match lower[pos..].find("result__a") {
            Some(r) => r,
            None => break,
        };
        // 回退到该锚点所属 `<a ` 标签起点
        // Walk back to the start of the enclosing `<a ` tag
        let tag_start_abs = match html[..pos + anchor_rel].rfind("<a ") {
            Some(s) => s,
            None => {
                pos += anchor_rel + "result__a".len();
                continue;
            }
        };
        // 该 `<a ...>` 标签的闭合 `>`
        // Closing `>` of this `<a ...>` tag
        let tag_open_end_abs = match html[tag_start_abs..].find('>') {
            Some(e) => tag_start_abs + e + 1,
            None => {
                pos = tag_start_abs + 3;
                continue;
            }
        };
        // 从 `<a ...>` 中提取 href="..."
        // Extract href="..." from the opening tag
        let open_tag = &html[tag_start_abs..tag_open_end_abs];
        let raw_url = extract_attr(open_tag, "href").unwrap_or_default();
        let url = decode_ddg_redirect(&raw_url);

        // 找 </a>，取中间文本作为标题
        // Find </a>; the inner text is the title
        let after_close = match lower[tag_open_end_abs..].find("</a>") {
            Some(c) => tag_open_end_abs + c,
            None => {
                pos = tag_open_end_abs;
                continue;
            }
        };
        let title_html = &html[tag_open_end_abs..after_close];
        let title = strip_html_inline(title_html);

        // 在该结果块后续一小段窗口内找 result__snippet
        // Look for result__snippet within a short window after this anchor
        let window_end = lower.floor_char_boundary((after_close + 4096).min(lower.len()));
        let snippet = lower[after_close..window_end]
            .find("result__snippet")
            .and_then(|rel| {
                let snip_tag_abs = after_close + rel;
                // 回退到 `<a ` 或 `<div ` 起点
                // Walk back to the enclosing tag start
                let snip_open = html[..snip_tag_abs]
                    .rfind("<a ")
                    .or_else(|| html[..snip_tag_abs].rfind("<div "))
                    .unwrap_or(snip_tag_abs);
                html[snip_open..].find('>').map(|e| snip_open + e + 1)
            })
            .and_then(|content_start| {
                lower[content_start..]
                    .find("</a>")
                    .or_else(|| lower[content_start..].find("</div>"))
                    .map(|end| content_start + end)
            })
            .map(|content_end| {
                let content_end = html.floor_char_boundary(content_end.min(html.len()));
                let start = html.floor_char_boundary(content_end.saturating_sub(2048).max(after_close));
                strip_html_inline(&html[start..content_end])
            })
            .unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }

        pos = after_close + 4;
        if results.len() >= 32 {
            break; // 安全上限 / safety cap
        }
    }

    results
}

/// 从一段 HTML 开标签中提取指定属性值（支持单/双引号）。
/// Extracts an attribute value from an HTML opening tag (supports single/double quotes).
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let pat = format!("{attr}=");
    let idx = lower.find(&pat)?;
    let after = &tag[idx + pat.len()..];
    let bytes = after.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0];
    if quote != b'"' && quote != b'\'' {
        // 无引号属性，读到空白为止
        // Unquoted attribute: read until whitespace
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(after.len());
        return Some(after[..end].to_string());
    }
    let rest = &after[1..];
    let end = rest.find(quote as char)?;
    Some(rest[..end].to_string())
}

/// 把 DuckDuckGo 的跳转链接 `//duckduckgo.com/l/?uddg=<encoded>&...`
/// 解码为真实目标 URL；非跳转链接原样返回（补全协议）。
/// Decodes a DuckDuckGo redirect URL `//duckduckgo.com/l/?uddg=<encoded>&...`
/// into the real target; non-redirect URLs are returned as-is (with protocol filled in).
fn decode_ddg_redirect(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // 补全协议前缀 / Normalize protocol-relative URLs
    let normalized = if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if trimmed.starts_with('/') {
        format!("https://duckduckgo.com{trimmed}")
    } else {
        trimmed.to_string()
    };

    // 在 query 串中找 uddg= 参数 / Find the uddg= query parameter
    if let Some(qpos) = normalized.find('?') {
        let query = &normalized[qpos + 1..];
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix("uddg=")
                && let Ok(decoded) = urlencoding_decode(value)
            {
                return decoded;
            }
        }
    }
    normalized
}

/// 极简 percent-decoding：仅处理 %XX 与 '+'，足够用于 DDG 跳转链接。
/// Minimal percent-decoding: handles %XX and '+', sufficient for DDG redirects.
fn urlencoding_decode(s: &str) -> Result<String, ToolError> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'%' => {
                return Err(ToolError("不完整的 percent 编码".into()));
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| ToolError(format!("UTF-8 解码失败: {e}")))
}

/// 把单个十六进制 ASCII 字符转为数值。
/// Converts a single hex ASCII character to its value.
fn hex_val(c: u8) -> Result<u8, ToolError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ToolError(format!("非法十六进制字符: {c}"))),
    }
}

/// 去除一段 HTML 片段中的标签并解码实体，用于标题/摘要等行内文本。
/// Strips tags from an HTML fragment and decodes entities; used for inline title/snippet text.
fn strip_html_inline(s: &str) -> String {
    let mut text = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_html_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 去除 HTML 标签，保留纯文本内容。
/// Strips HTML tags, keeping plain-text content.
/// 移除 <script> / <style> 块，解码常见 HTML 实体，折叠空白。
/// Removes <script> / <style> blocks, decodes common HTML entities, collapses whitespace.
fn strip_html(html: &str) -> String {
    // 移除 <script>...</script> 和 <style>...</style>（含内容）
    // Remove <script>...</script> and <style>...</style> (including their content)
    let without_scripts = remove_tags_with_content(html, "script");
    let without_styles = remove_tags_with_content(&without_scripts, "style");
    // 移除 HTML 注释
    // Remove HTML comments
    let mut text = String::with_capacity(without_styles.len());
    let mut in_tag = false;
    for ch in without_styles.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    // 解码常见 HTML 实体
    // Decode common HTML entities
    text = decode_html_entities(&text);
    // 折叠连续空白为单个空格
    // Collapse consecutive whitespace into a single space
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

/// 移除指定标签及其内容（如 <script>...</script>）。
/// Removes a given tag and its content (e.g. <script>...</script>).
fn remove_tags_with_content(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let lower = html.to_lowercase();
    let mut result = String::with_capacity(html.len());
    let mut pos = 0;
    while pos < html.len() {
        if let Some(start) = lower[pos..].find(&open) {
            result.push_str(&html[pos..pos + start]);
            let after_open = pos + start;
            if let Some(end_rel) = lower[after_open..].find(&close) {
                pos = after_open + end_rel + close.len();
            } else {
                // 没有闭合标签，跳过剩余
                // No closing tag; skip the remainder
                break;
            }
        } else {
            result.push_str(&html[pos..]);
            break;
        }
    }
    result
}

/// 解码常见 HTML 实体。
/// Decodes common HTML entities.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
}

/// 将命令按 `|` `;` `&` `\n` 分割，但尊重引号和反斜杠转义。
/// 引号内的分隔符不触发分割；反斜杠转义的下一个字符也跳过。
/// Splits a command by `|` `;` `&` `\n`, respecting quotes and backslash escaping.
/// Separators inside quotes do not trigger a split; escaped chars are passed through.
fn split_shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if escaped {
            current.push(ch);
            escaped = false;
            i += 1;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
                current.push(ch);
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            '|' | ';' | '\n' if !in_single && !in_double => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    segments.push(trimmed);
                }
                current.clear();
            }
            // `&>` = redirect both stdout+stderr; `>&` = redirect to fd.
            // These are NOT separators — `&` is part of the redirection syntax.
            // `&&` and `command &` are still separators.
            '&' if !in_single && !in_double => {
                let next_is_gt = i + 1 < chars.len() && chars[i + 1] == '>';
                let prev_is_gt = current.ends_with('>');
                if next_is_gt || prev_is_gt {
                    current.push(ch);
                } else {
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        segments.push(trimmed);
                    }
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
        i += 1;
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        segments.push(trimmed);
    }
    segments
}

/// 检查字符是否在引号外出现（用于检测重定向 `>` 等）。
/// Checks whether a character appears outside quotes (for detecting `>` redirection etc.).
#[allow(dead_code)]
fn contains_unquoted(s: &str, target: char) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            c if c == target && !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
}

/// 检查是否存在写文件的重定向（排除 `/dev/null` 和文件描述符重定向如 `>&2`）。
/// Checks for file-writing redirection (excluding `/dev/null` and fd redirects like `>&2`).
#[allow(dead_code)]
fn has_file_redirect(s: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match chars[i] {
            '\\' if !in_single => {
                escaped = true;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '>' if !in_single && !in_double => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] == '>' {
                    j += 1;
                }
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let rest: String = chars[j..].iter().collect();
                if rest.starts_with("/dev/null")
                    || rest.starts_with("/dev/stdout")
                    || rest.starts_with("/dev/stderr")
                    || rest.starts_with("&1")
                    || rest.starts_with("&2")
                {
                    i = j;
                    continue;
                }
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Token-boundary 前缀匹配：段 `s` 匹配前缀 `p` 当且仅当 `s == p` 或
/// `s` 以 `p` 开头且 `p` 之后紧跟一个空白字符（词边界）。
/// 这防止 `lsd` 误匹配 `ls`、`cat-bomb` 误匹配 `cat`、`findstr` 误匹配 `find`。
/// 多词前缀（如 `"git status"`）同样适用：`git statusish` 不会匹配 `git status`。
///
/// Token-boundary prefix match: segment `s` matches prefix `p` iff `s == p` or
/// `s` starts with `p` and the char right after `p` is whitespace (word boundary).
/// This prevents `lsd` from matching `ls`, `cat-bomb` from matching `cat`,
/// `findstr` from matching `find`. Multi-word prefixes (e.g. `"git status"`)
/// work the same way: `git statusish` does not match `git status`.
fn prefix_matches(s: &str, p: &str) -> bool {
    s == p || (s.starts_with(p) && s[p.len()..].starts_with(char::is_whitespace))
}

/// 判断 shell 命令是否为只读（可安全自动执行）还是会改变状态（需询问）。
/// Determines whether a shell command is read-only (safe to auto-run) or mutating (needs prompting).
/// 拿不准时返回 false —— 循环会将其视为"会改变状态"并询问人类，
/// Returns false when unsure — the loop treats it as "mutating" and asks a human,
/// 因为自动执行破坏性命令比多一次确认更危险。
/// because auto-running a destructive command is more dangerous than one extra confirmation.
pub fn is_readonly_bash(command: &str) -> bool {
    const READONLY_PREFIXES: &[&str] = &[
        // 目录/文件查看 / Directory & file viewing
        "ls",
        "cat",
        "head",
        "tail",
        "tree",
        "file",
        "stat",
        "du",
        "df",
        "wc",
        "nl",
        "tac",
        "rev",
        "bat",
        "eza",
        "exa",
        // 搜索 / Search
        "grep",
        "rg",
        "ag",
        "ack",
        "find",
        "fd",
        // Git 只读 / Git read-only
        "git status",
        "git log",
        "git diff",
        "git show",
        // 文本处理 / Text processing
        "sort",
        "uniq",
        "cut",
        "tr",
        "diff",
        "comm",
        "join",
        "paste",
        "fold",
        "fmt",
        "pr",
        "column",
        "expand",
        "unexpand",
        "shuf",
        "tsort",
        "seq",
        "sed",
        "awk",
        // 路径 / Path utilities
        "pwd",
        "basename",
        "dirname",
        "realpath",
        "readlink",
        "which",
        // 校验和 / Checksums
        "md5sum",
        "sha1sum",
        "sha256sum",
        "sha512sum",
        // 十六进制/字符串 / Hex & strings
        "xxd",
        "od",
        "hexdump",
        "strings",
        // 编码 / Encoding
        "iconv",
        "base64",
        "base32",
        // 系统信息 / System info
        "printenv",
        "whoami",
        "uname",
        "arch",
        "nproc",
        "uptime",
        "hostname",
        "echo",
        "date",
        "test",
        "true",
        "false",
    ];
    // 命令替换（$(...) / 反引号）可在只读前缀内部执行任意命令——例如
    // `ls $(rm -rf ~)` 首 token 是只读的 `ls`，但实际会删除用户主目录。
    // 只要存在命令替换，一律视为会改变状态，交给 HITL 询问。
    // Command substitution ($(...) / backticks) can run arbitrary commands inside a
    // read-only prefix — e.g. `ls $(rm -rf ~)` starts with the read-only `ls` but
    // actually deletes the user's home directory. Any command substitution means the
    // command must be treated as mutating and routed to HITL.
    if command.trim().is_empty() {
        return false;
    }
    if command.contains("$(") || command.contains('`') {
        return false;
    }
    for s in split_shell_segments(command) {
        if s.is_empty() {
            return false;
        }
        // `find` 的 -delete / -exec / -execdir 可删除或执行文件；-fprint 系列会写文件。
        // 仅当分段以 find 开头时检查，避免误伤其它含这些子串的命令。
        // `find` flags like -delete / -exec / -execdir can delete or execute files;
        // -fprint variants write files. Only check when the segment starts with find
        // to avoid false positives on other commands.
        if s.starts_with("find")
            && (s.contains(" -delete")
                || s.contains(" -exec")
                || s.contains(" -execdir")
                || s.contains(" -ok")
                || s.contains(" -okdir")
                || s.contains(" -fprint")
                || s.contains(" -fprintf")
                || s.contains(" -fls"))
        {
            return false;
        }
        // `sed -i` 原地编辑会修改文件；`--in-place` 同理。
        // 合并标志（如 -ni）中的 i 也要检测。
        // `sed -i` modifies files in place; `--in-place` likewise.
        // Also detect `i` in combined short flags (e.g. `-ni`).
        if s.starts_with("sed") {
            for token in s.split_whitespace() {
                if token == "--in-place" || token.starts_with("--in-place=") {
                    return false;
                }
                if token.starts_with('-')
                    && !token.starts_with("--")
                    && token.len() > 1
                    && token.contains('i')
                {
                    return false;
                }
            }
        }
        // `awk` 的 system() 可执行任意命令；`| getline` 可从命令管道读取。
        // `awk`'s system() can execute arbitrary commands; `| getline` reads from command pipes.
        if s.starts_with("awk") && (s.contains("system(") || s.contains("| getline")) {
            return false;
        }
        // xargs 后跟只读命令则安全（如 `xargs grep`），否则需 HITL。
        // xargs 后的命令可能是危险的（如 `xargs rm`），需检查内部命令。
        // xargs followed by a read-only command is safe (e.g. `xargs grep`);
        // xargs followed by a mutating command (e.g. `xargs rm`) is dangerous.
        if s.starts_with("xargs") {
            let after = s.strip_prefix("xargs").unwrap_or("").trim();
            let inner = after.split_whitespace().find(|t| !t.starts_with('-'));
            match inner {
                Some(cmd) if !READONLY_PREFIXES.iter().any(|p| prefix_matches(cmd, p)) => {
                    return false;
                }
                _ => {} // None = xargs defaults to echo (safe); Some = inner is readonly
            }
            continue;
        }
        if !READONLY_PREFIXES.iter().any(|p| prefix_matches(&s, p)) {
            return false;
        }
    }
    true
}

/// 内置工具名称列表（供侧边栏显示）。
/// Built-in tool name list (for sidebar display).
pub fn tool_names() -> Vec<&'static str> {
    vec![
        "read_file",
        "edit_file",
        "write_file",
        "run_bash",
        "run_file",
        "web_fetch",
        "web_search",
        "todo_write",
        "task",
        "bash_output",
        "kill_shell",
    ]
}

// ─── todo_write 工具 ────────────────────────────────────────────────────
// ─── todo_write tool ────────────────────────────────────────────────────

/// todo_write 工具的共享状态 + 事件发送端。
/// Shared store + event sender for the todo_write tool.
///
/// 工具持有此结构的 clone，在 call 时更新 store 并通过 tx 发出 TodoUpdate 事件。
/// 依赖方向：tool → event → TUI，永不 tool → TUI 直接调用。
/// The tool holds a clone of this; on call it updates the store and emits a
/// TodoUpdate event via tx. Dependency direction: tool → event → TUI, never
/// tool → TUI directly.
#[derive(Clone)]
pub struct TodoContext {
    /// 共享任务列表——同一 Orchestrator 的所有 TodoWrite 实例共享同一份。
    /// Shared todo list — all TodoWrite instances within one Orchestrator share the same store.
    pub store: Arc<Mutex<Vec<TodoItem>>>,
    /// 事件发送端（mpsc::UnboundedSender<AgentEvent> 的 clone）。
    /// Event sender (a clone of mpsc::UnboundedSender<AgentEvent>).
    pub tx: EventSender,
}

/// `todo_write` 工具的输入参数：完整任务列表（每次调用整体替换）。
/// Input args for the `todo_write` tool: the full todo list (replaced wholesale on each call).
#[derive(Deserialize)]
struct TodoWriteArgs {
    todos: Vec<TodoItem>,
}

/// 用于规划和跟踪多步骤工作的任务列表工具。
/// Tool for planning and tracking multi-step work with a todo list.
///
/// 每次调用整体替换共享列表，校验后发出 `AgentEvent::TodoUpdate`。
/// Each call replaces the shared list wholesale; after validation it emits
/// `AgentEvent::TodoUpdate`.
struct TodoWrite {
    ctx: TodoContext,
}

impl TodoWrite {
    fn new(ctx: TodoContext) -> Self {
        Self { ctx }
    }
}

impl PortableTool for TodoWrite {
    const NAME: &'static str = "todo_write";
    type Error = ToolError;
    type Args = TodoWriteArgs;
    type Output = String;

    fn description(&self) -> String {
        "用于规划和跟踪多步骤工作的任务列表工具。每次调用用完整列表整体替换之前的列表。\
         任一时刻恰好一项为 in_progress；每完成一项立即标记 completed 再推进下一项。\
         3 步及以上的任务请先用本工具建清单。"
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "完整的任务列表（每次调用整体替换）",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "任务唯一标识（非空）" },
                            "content": { "type": "string", "description": "任务内容描述" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "任务状态" }
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // 校验：非空 id、无重复 id、最多 1 项 in_progress。
        // Validate: non-empty ids, no duplicate ids, at most 1 in_progress.
        let mut ids = std::collections::HashSet::new();
        let mut in_progress_count = 0;
        for item in &args.todos {
            if item.id.is_empty() {
                return Err(ToolError("todo id must not be empty / id 不得为空".into()));
            }
            if !ids.insert(&item.id) {
                return Err(ToolError(format!(
                    "duplicate todo id: {} / 重复的 id: {}",
                    item.id, item.id
                )));
            }
            if item.status == TodoStatus::InProgress {
                in_progress_count += 1;
            }
        }
        if in_progress_count > 1 {
            return Err(ToolError(format!(
                "at most 1 in_progress item allowed, got {in_progress_count} / \
                 最多 1 项 in_progress，实际 {in_progress_count} 项"
            )));
        }

        // 原子替换共享列表 / atomically replace the shared list.
        {
            let mut store = self
                .ctx
                .store
                .lock()
                .map_err(|e| ToolError(format!("store lock failed: {e}")))?;
            *store = args.todos.clone();
        }

        // 发出 TodoUpdate 事件——TUI 侧边栏消费此事件渲染列表。
        // Emit TodoUpdate event — the TUI sidebar consumes this to render the list.
        let _ = self.ctx.tx.send(AgentEvent::TodoUpdate {
            todos: args.todos.clone(),
        });

        let n = args.todos.len();
        Ok(format!(
            "todos updated: {n} items ({in_progress_count} in progress)"
        ))
    }
}

// ─── schedule_task 工具（定时任务管理）────────────────────────────────
// ─── schedule_task tool (scheduled task management) ──────────────────

/// `schedule_task` 工具的输入参数：操作类型 + 任务参数。
/// Input args for the `schedule_task` tool: action type + task parameters.
#[derive(Deserialize)]
struct ScheduleTaskArgs {
    /// 操作类型：add / list / remove / enable / disable / logs。
    /// Action type: add / list / remove / enable / disable / logs.
    action: String,
    /// 任务名称（add 时必填）。
    /// Task name (required for add).
    #[serde(default)]
    name: Option<String>,
    /// cron 表达式（周期任务，与 at 二选一）。
    /// 5 字段：分 时 日 月 周；6 字段：秒 分 时 日 月 周。
    /// Cron expression (recurring; mutually exclusive with `at`).
    /// 5 fields: min hour day month weekday; 6 fields: sec min hour day month weekday.
    #[serde(default)]
    cron: Option<String>,
    /// 一次性执行时间（与 cron 二选一），格式 "YYYY-MM-DD HH:MM[:SS]"（本地时间）或 RFC3339。
    /// One-shot datetime (mutually exclusive with `cron`), local
    /// "YYYY-MM-DD HH:MM[:SS]" or RFC3339.
    #[serde(default)]
    at: Option<String>,
    /// 任务 prompt（add 时必填），即要执行的 agent 指令。
    /// Task prompt (required for add), i.e. the agent instruction to execute.
    #[serde(default)]
    prompt: Option<String>,
    /// 任务 ID（remove / enable / disable 时必填）。
    /// Task ID (required for remove / enable / disable).
    #[serde(default)]
    id: Option<String>,
    /// 最大执行次数（可选，仅 add 时有效）。
    /// Max execution count (optional, only valid for add).
    #[serde(default)]
    max_runs: Option<u64>,
    /// 日志条数（logs 时可选，默认 10）。
    /// Log count (optional for logs, default 10).
    #[serde(default)]
    limit: Option<usize>,
}

/// 定时任务管理工具：创建、查看、删除、启用/禁用定时任务，查看执行日志。
/// 直接通过 TaskManager 操作 JSON 文件，与 Scheduler 通过文件系统同步。
struct ScheduleTask {
    manager: crate::scheduler::TaskManager,
}

impl PortableTool for ScheduleTask {
    const NAME: &'static str = "schedule_task";
    type Error = ToolError;
    type Args = ScheduleTaskArgs;
    type Output = String;

    fn description(&self) -> String {
        "定时任务管理工具。支持以下操作：\n\
         - add: 创建定时任务（需提供 name, prompt，以及 cron 或 at 之一）\n\
         - list: 列出所有任务\n\
         - remove: 删除任务（需提供 id）\n\
         - enable/disable: 启用/禁用任务（需提供 id）\n\
         - logs: 查看最近执行日志（可选 limit）\n\n\
         触发方式（二选一）：\n\
         1) cron 周期任务：5 字段『分 时 日 月 周』（如 '0 9 * * *' 每天 9 点，'*/15 * * * *' 每 15 分钟），\n\
            或 6 字段『秒 分 时 日 月 周』（如 '30 0 9 * * *' 每天 09:00:30）\n\
         2) at 一次性任务：指定年月日时分秒（如 '2026-12-25 09:30:00'），执行一次后自动失效\n\n\
         注意：os 调度模式下系统心跳粒度为 1 分钟，秒级任务在到点后的下一次心跳触发。"
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove", "enable", "disable", "logs"],
                    "description": "操作类型"
                },
                "name": { "type": "string", "description": "任务名称（add 时必填）" },
                "cron": { "type": "string", "description": "cron 表达式（周期任务，与 at 二选一）。5 字段『分 时 日 月 周』如 '0 9 * * *'；6 字段『秒 分 时 日 月 周』如 '30 0 9 * * *'" },
                "at": { "type": "string", "description": "一次性执行时间（与 cron 二选一），格式 'YYYY-MM-DD HH:MM[:SS]'，如 '2026-12-25 09:30:00'" },
                "prompt": { "type": "string", "description": "要执行的 agent prompt（add 时必填）" },
                "id": { "type": "string", "description": "任务 ID（remove/enable/disable 时必填）" },
                "max_runs": { "type": "number", "description": "最大执行次数（可选，仅 add 时有效）" },
                "limit": { "type": "number", "description": "日志条数（logs 时可选，默认 10）" }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        match args.action.as_str() {
            "add" => {
                let name = args.name.ok_or_else(|| ToolError("name is required for add".into()))?;
                let prompt = args.prompt.ok_or_else(|| ToolError("prompt is required for add".into()))?;
                let at = match &args.at {
                    Some(s) => Some(crate::scheduler::parse_at(s)
                        .map_err(|e| ToolError(format!("at 时间解析失败: {e}")))?),
                    None => None,
                };
                match self.manager.add_task(name, args.cron, at, prompt, args.max_runs) {
                    Ok(task) => {
                        let trigger = match (&task.cron, &task.at) {
                            (Some(c), _) => format!("Cron: {c}"),
                            (None, Some(at)) => format!("At: {}（一次性）", at.format("%Y-%m-%d %H:%M:%S")),
                            (None, None) => unreachable!(), // add_task 已校验
                        };
                        Ok(format!(
                            "✅ 定时任务已创建\nID: {}\n名称: {}\n{}\n最大执行次数: {}",
                            task.id, task.name, trigger,
                            task.max_runs.map(|n| n.to_string()).unwrap_or_else(|| "无限".into())
                        ))
                    }
                    Err(e) => Err(ToolError(format!("创建任务失败: {e}"))),
                }
            }
            "list" => {
                match self.manager.list_tasks() {
                    Ok(tasks) => {
                        if tasks.is_empty() {
                            Ok("📋 暂无定时任务".to_string())
                        } else {
                            let mut out = String::from("📋 定时任务列表：\n");
                            for t in &tasks {
                                let status = if t.enabled { "✅" } else { "⏸️" };
                                let last = t.last_run
                                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| "从未".into());
                                let trigger = match (&t.cron, &t.at) {
                                    (Some(c), _) => format!("cron: {c}"),
                                    (None, Some(at)) => format!("at: {}（一次性）", at.format("%Y-%m-%d %H:%M:%S")),
                                    (None, None) => "trigger: 未设置".to_string(),
                                };
                                out.push_str(&format!(
                                    "{} [{}] {} ({}, 已执行 {} 次, 上次: {})\n",
                                    status, t.id, t.name, trigger, t.run_count, last
                                ));
                            }
                            Ok(out)
                        }
                    }
                    Err(e) => Err(ToolError(format!("读取任务列表失败: {e}"))),
                }
            }
            "remove" => {
                let id = args.id.ok_or_else(|| ToolError("id is required for remove".into()))?;
                match self.manager.remove_task(&id) {
                    Ok(true) => Ok(format!("✅ 任务 {} 已删除", id)),
                    Ok(false) => Err(ToolError(format!("任务 {} 不存在", id))),
                    Err(e) => Err(ToolError(format!("删除失败: {e}"))),
                }
            }
            "enable" => {
                let id = args.id.ok_or_else(|| ToolError("id is required for enable".into()))?;
                match self.manager.set_enabled(&id, true) {
                    Ok(true) => Ok(format!("✅ 任务 {} 已启用", id)),
                    Ok(false) => Err(ToolError(format!("任务 {} 不存在", id))),
                    Err(e) => Err(ToolError(format!("启用失败: {e}"))),
                }
            }
            "disable" => {
                let id = args.id.ok_or_else(|| ToolError("id is required for disable".into()))?;
                match self.manager.set_enabled(&id, false) {
                    Ok(true) => Ok(format!("✅ 任务 {} 已禁用", id)),
                    Ok(false) => Err(ToolError(format!("任务 {} 不存在", id))),
                    Err(e) => Err(ToolError(format!("禁用失败: {e}"))),
                }
            }
            "logs" => {
                let limit = args.limit.unwrap_or(10);
                let logs = self.manager.get_logs(limit);
                if logs.is_empty() {
                    Ok("📊 暂无执行日志".to_string())
                } else {
                    let mut out = String::from("📊 最近执行日志：\n");
                    for l in &logs {
                        let status = if l.ok { "✅" } else { "❌" };
                        out.push_str(&format!(
                            "{} [{}] {} ({} → {}, {})\n",
                            status, l.task_id, l.task_name,
                            l.started_at.format("%m-%d %H:%M"),
                            l.finished_at.format("%H:%M"),
                            l.output_summary
                        ));
                    }
                    Ok(out)
                }
            }
            other => Err(ToolError(format!("未知操作: {other}，支持 add/list/remove/enable/disable/logs"))),
        }
    }
}

// ─── task 工具（子代理并行扇出）────────────────────────────────────────
// ─── task tool (parallel subagent fanout) ──────────────────────────────

/// `task` 工具的输入参数：子任务列表 + 可选并发上限。
/// Input args for the `task` tool: subtask list + optional concurrency cap.
#[derive(Deserialize)]
struct TaskToolArgs {
    tasks: Vec<crate::subagent::SubTask>,
    #[serde(default)]
    max_concurrent: Option<usize>,
}

/// 并行扇出多个上下文隔离的子代理，聚合结果。
/// Fans out multiple context-isolated subagents in parallel, aggregating results.
///
/// 持有 `SubagentCtx`（sandbox/trust/tx/depth）和 `AgentRegistry` 克隆，
/// 调用 `fanout` → `run_subtask` → `run_autonomous`。
/// Holds `SubagentCtx` (sandbox/trust/tx/depth) and an `AgentRegistry` clone;
/// calls `fanout` → `run_subtask` → `run_autonomous`.
struct TaskTool {
    ctx: crate::subagent::SubagentCtx,
    registry: crate::registry::AgentRegistry,
}

impl TaskTool {
    fn new(ctx: crate::subagent::SubagentCtx, registry: crate::registry::AgentRegistry) -> Self {
        Self { ctx, registry }
    }
}

impl PortableTool for TaskTool {
    const NAME: &'static str = "task";
    type Error = ToolError;
    type Args = TaskToolArgs;
    type Output = String;

    fn description(&self) -> String {
        "并行扇出多个上下文隔离的子代理执行子任务并聚合结果。每个子任务拥有独立的\
         对话历史，不继承父历史。agent 模板：explore（只读调查）或 build（可编辑）。\
         适用于独立子任务并行化——例如分别调查多个模块、并行实现互不依赖的组件。"
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "子任务列表",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": { "type": "string", "description": "子任务简述（3-5 词）" },
                            "prompt": { "type": "string", "description": "子任务完整提示词" },
                            "agent": { "type": "string", "enum": ["explore", "build"], "description": "agent 模板（默认 explore）" }
                        },
                        "required": ["description", "prompt"]
                    }
                },
                "max_concurrent": { "type": "integer", "description": "最大并发数（默认 4）" }
            },
            "required": ["tasks"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let max_concurrent = args.max_concurrent.unwrap_or(4);
        let ctx = self.ctx.clone();
        let registry = self.registry.clone();
        Ok(crate::subagent::fanout(args.tasks, max_concurrent, move |task| {
            let ctx = ctx.clone();
            let registry = registry.clone();
            async move { crate::subagent::run_subtask(registry, ctx, task).await }
        })
        .await)
    }
}

/// 内置工具集合 + 动态工具（tools_ext）。
/// Built-in tool set + dynamic tools (tools_ext).
/// 动态工具由 /add-tool 命令生成，需重新 cargo build 后生效。
/// Dynamic tools are generated by the /add-tool command and take effect after a fresh cargo build.
// TODO: add_builtin_tools() 未接入 load_all()，/add-tool 动态工具暂不注册到 0.41 builder。
#[allow(dead_code)]
pub fn builtin_tools(
    config: &crate::context::ContextConfig,
    sandbox: Arc<dyn SandboxProvider>,
) -> anyhow::Result<rig_agent::tool::ToolSet> {
    let mut tools = rig_agent::tool::ToolSet::default();
    let shells = Arc::new(LazyShell::new(sandbox.clone(), config.max_bash_output_chars));
    let bg = Arc::new(BackgroundRegistry::new(config.max_bash_output_chars));
    let checkpoints = Arc::new(crate::checkpoint::CheckpointStore::new());
    tools.add_tool(ReadFile {
        max_read_lines: config.max_read_lines,
    });
    tools.add_tool(EditFile {
        checkpoints: checkpoints.clone(),
    });
    tools.add_tool(WriteFile { checkpoints });
    tools.add_tool(RunBash {
        max_bash_output_chars: config.max_bash_output_chars,
        sandbox: sandbox.clone(),
        timeout_secs: 300,
        shells,
        bg: bg.clone(),
    });
    tools.add_tool(RunFile {
        max_output_chars: config.max_bash_output_chars,
        sandbox: sandbox.clone(),
    });
    tools.add_tool(WebFetch);
    tools.add_tool(WebSearch);
    tools.add_tool(BashOutput { bg: bg.clone() });
    tools.add_tool(KillShell { bg });
    tools.add_tools(crate::tools_ext::load_all());
    Ok(tools)
}

/// 工具依赖注入容器——把 sandbox / todo_ctx / shells / bg 打包为一个参数。
/// Dependency injection container — bundles sandbox / todo_ctx / shells / bg into one param.
///
/// 每个 agent 获得自己的 `LazyShell`（per-agent-run 隔离），但共享
/// Orchestrator 级别的 `BackgroundRegistry`（shell 寿命超过单轮）。
/// Each agent gets its own `LazyShell` (per-agent-run isolation), but shares
/// the Orchestrator-level `BackgroundRegistry` (shells outlive the turn).
pub struct ToolDeps {
    pub sandbox: Arc<dyn SandboxProvider>,
    pub todo_ctx: Option<TodoContext>,
    /// task 工具（子代理扇出）上下文——仅 Builder + Orchestrator（且深度为 0）时有值。
    /// task tool (subagent fanout) context — only Some for Builder + Orchestrator
    /// (and only when subagent depth is 0).
    pub task_ctx: Option<crate::subagent::SubagentCtx>,
    /// AgentRegistry 克隆，供 TaskTool 传给 run_subtask → run_autonomous。
    /// 廉价克隆（所有字段为 Arc 共享），不形成引用环（registry 的 task_ctx 槽
    /// 只存 SubagentCtx，不含 registry 本身）。
    /// AgentRegistry clone for TaskTool to pass to run_subtask → run_autonomous.
    /// Cheap clone (all fields Arc-shared); no reference cycle (the registry's
    /// task_ctx slot stores SubagentCtx only, not the registry itself).
    pub task_registry: crate::registry::AgentRegistry,
    pub shells: Arc<LazyShell>,
    pub bg: Arc<BackgroundRegistry>,
    pub checkpoints: Arc<crate::checkpoint::CheckpointStore>,
    /// 调度器任务管理器（可选，仅当 [scheduler].enabled = true 时有值）。
    /// Scheduler task manager (optional, only Some when [scheduler].enabled = true).
    pub scheduler_mgr: Option<crate::scheduler::TaskManager>,
}

/// 将内置工具逐一注册到 builder 上（rig 0.41 的 `.tool()` 链式调用）。
/// Registers the built-in tools onto a builder one by one (rig 0.41's `.tool()` chain).
///
/// 沙箱以 `Arc<dyn SandboxProvider>` trait 对象注入（todo 4 迁移）——使后端可在
/// 配置层切换，build() 不再依赖具体 `SimpleSandbox` 类型。
///
/// todo 9: 每个工具用 `TimeoutRetryTool::passthrough` 包裹（around-execute 层）。
/// 工具体本身不变——仅 `call()` 外层多一道 300s 超时安全网（0 次重试，避免对会改变
/// 状态的工具双重执行）。pre/post 层由 `HitlHook`（rig hook）独立处理。
/// todo 9: each tool is wrapped with `TimeoutRetryTool::passthrough` (around-execute layer).
/// Tool bodies are unchanged — `call()` only gains a 300s timeout safety net (0 retries,
/// avoiding double-execution of state-changing tools). The pre/post layer is handled
/// independently by `HitlHook` (rig hook).
pub fn add_builtin_tools<M>(
    builder: rig_agent::agent::AgentBuilder<M, rig_agent::agent::NoToolConfig>,
    config: &crate::context::ContextConfig,
    deps: &ToolDeps,
) -> rig_agent::agent::AgentBuilder<M, rig_agent::agent::WithBuilderTools>
where
    M: rig_core::completion::CompletionModel,
{
    use pipeline::TimeoutRetryTool;
    let builder = builder
        .tool(TimeoutRetryTool::passthrough(ReadFile {
            max_read_lines: config.max_read_lines,
        }))
        .tool(TimeoutRetryTool::passthrough(EditFile {
            checkpoints: deps.checkpoints.clone(),
        }))
        .tool(TimeoutRetryTool::passthrough(WriteFile {
            checkpoints: deps.checkpoints.clone(),
        }))
        .tool(TimeoutRetryTool::passthrough(RunBash {
            max_bash_output_chars: config.max_bash_output_chars,
            sandbox: deps.sandbox.clone(),
            timeout_secs: 300,
            shells: deps.shells.clone(),
            bg: deps.bg.clone(),
        }))
        .tool(TimeoutRetryTool::passthrough(RunFile {
            max_output_chars: config.max_bash_output_chars,
            sandbox: deps.sandbox.clone(),
        }))
        .tool(TimeoutRetryTool::passthrough(WebFetch))
        .tool(TimeoutRetryTool::passthrough(WebSearch))
        .tool(TimeoutRetryTool::passthrough(BashOutput {
            bg: deps.bg.clone(),
        }))
        .tool(TimeoutRetryTool::passthrough(KillShell {
            bg: deps.bg.clone(),
        }));
    // todo_write 仅在有共享 store + sender 时注册（Builder + Orchestrator 角色）。
    // todo_write is only registered when a shared store + sender is available
    // (Builder + Orchestrator roles).
    // task 同理：仅在有 task_ctx 时注册（Builder + Orchestrator，且深度为 0）。
    // task likewise: only registered when task_ctx is available
    // (Builder + Orchestrator, and only when subagent depth is 0).
    let builder = if let Some(todo_ctx) = &deps.todo_ctx {
        builder.tool(TodoWrite::new(todo_ctx.clone()))
    } else {
        builder
    };

    let builder = if let Some(task_ctx) = &deps.task_ctx {
        builder.tool(TimeoutRetryTool::passthrough(TaskTool::new(
            task_ctx.clone(),
            deps.task_registry.clone(),
        )))
    } else {
        builder
    };

    // schedule_task 仅在调度器启用时注册。
    // schedule_task is only registered when the scheduler is enabled.
    if let Some(mgr) = &deps.scheduler_mgr {
        builder.tool(TimeoutRetryTool::passthrough(ScheduleTask {
            manager: mgr.clone(),
        }))
    } else {
        builder
    }
}

// ---------------------------------------------------------------------------
// Seam trait impls —— 把现有 read_file/edit_file/write_file/run_bash 核心逻辑
// 包装为 FileSystemProvider / ShellExecutor trait 实现 (todo 4)。
// 现有 PortableTool impl（带分页/截断/格式化）保持不变，行为等价。
// ---------------------------------------------------------------------------

/// `FileSystemProvider` 的默认实现：包装现有 read_file/edit_file/write_file 的
/// 核心 `std::fs` 逻辑（无分页/截断/格式化——那是 PortableTool 层的职责）。
/// 未在 production code 直接构造；todo 9 工具管线接入后使用。
#[allow(dead_code)]
pub struct SimpleFileSystem;

impl FileSystemProvider for SimpleFileSystem {
    fn read(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(anyhow::Error::from)
    }

    fn write(&self, path: &Path, content: &str) -> Result<()> {
        std::fs::write(path, content).map_err(anyhow::Error::from)
    }

    fn edit(&self, path: &Path, old: &str, new: &str) -> Result<()> {
        let content = std::fs::read_to_string(path).map_err(anyhow::Error::from)?;
        if !content.contains(old) {
            anyhow::bail!("old text not found in file");
        }
        let updated = content.replacen(old, new, 1);
        std::fs::write(path, updated).map_err(anyhow::Error::from)
    }
}

/// `ShellExecutor` 的默认实现：包装现有 run_bash 的核心 `sh -c` 执行逻辑。
///
/// 同步阻塞（trait 要求 `-> Result<Output>`）；与现有 `RunBash` 的
/// `tokio::process::Command` 路径行为等价（无 bwrap、无超时、无截断——
/// 那些是 PortableTool 层 RunBash 的职责）。bwrap 包装由 SandboxProvider
/// 的 `grant_args` 负责，调用方在需要时组合两者。
#[allow(dead_code)]
pub struct SimpleShell;

impl ShellExecutor for SimpleShell {
    fn run(&self, command: &str) -> Result<Output> {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(anyhow::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只读命令应被归类为只读。
    /// Read-only commands should be classified as read-only.
    #[test]
    fn readonly_commands_classified() {
        assert!(is_readonly_bash("ls -la"));
        assert!(is_readonly_bash("cat file.txt"));
        assert!(is_readonly_bash("git status"));
        assert!(is_readonly_bash("grep -r foo src | head"));
        assert!(is_readonly_bash("git log --oneline"));
    }

    /// 改变状态的命令不应被归类为只读。
    /// Mutating commands should not be classified as read-only.
    #[test]
    fn mutating_commands_not_readonly() {
        assert!(!is_readonly_bash("rm -rf x"));
        assert!(!is_readonly_bash("git commit -m x"));
        assert!(!is_readonly_bash("cargo build"));
        assert!(!is_readonly_bash("ls && rm x"));
        assert!(!is_readonly_bash(""));
    }

    /// 命令替换可绕过首 token 检查（`ls $(rm -rf ~)`），必须视为会改变状态。
    /// Command substitution can bypass the first-token check (`ls $(rm -rf ~)`);
    /// must be classified as mutating.
    #[test]
    fn command_substitution_not_readonly() {
        assert!(!is_readonly_bash("ls $(rm -rf ~)"));
        assert!(!is_readonly_bash("echo $(rm -rf .)"));
        assert!(!is_readonly_bash("cat `rm -rf x`"));
        assert!(!is_readonly_bash("grep -r x . $(sudo rm -rf /)"));
    }

    /// find 的破坏性标志应使其归类为会改变状态。
    /// Destructive find flags should make the command mutating.
    #[test]
    fn find_destructive_flags_not_readonly() {
        assert!(!is_readonly_bash("find . -delete"));
        assert!(!is_readonly_bash("find / -name x -exec rm {} \\;"));
        assert!(!is_readonly_bash("find . -execdir chmod 777 {} +"));
    }

    /// 扩展的只读命令（sort/uniq/cut/tr/diff/stat/rg/fd 等）应被归类为只读。
    /// Extended read-only commands (sort/uniq/cut/tr/diff/stat/rg/fd etc.) should be read-only.
    #[test]
    fn extended_readonly_commands() {
        assert!(is_readonly_bash("sort file.txt"));
        assert!(is_readonly_bash("uniq -c file.txt"));
        assert!(is_readonly_bash("cut -d: -f1 /etc/passwd"));
        assert!(is_readonly_bash("tr 'a-z' 'A-Z'"));
        assert!(is_readonly_bash("diff a.txt b.txt"));
        assert!(is_readonly_bash("stat file.txt"));
        assert!(is_readonly_bash("rg pattern src/"));
        assert!(is_readonly_bash("fd . --type f"));
        assert!(is_readonly_bash("bat README.md"));
        assert!(is_readonly_bash("md5sum file.bin"));
        assert!(is_readonly_bash("xxd hex.bin"));
        assert!(is_readonly_bash("strings binary"));
        assert!(is_readonly_bash("basename /a/b/c.txt"));
        assert!(is_readonly_bash("seq 1 10"));
        assert!(is_readonly_bash("date"));
    }

    /// 管道中的只读命令组合应被归类为只读（之前 sort 缺失导致 bug）。
    /// Piped read-only commands should be read-only (sort was previously missing, causing a bug).
    #[test]
    fn piped_readonly_commands() {
        assert!(is_readonly_bash("find src -type f | sort"));
        assert!(is_readonly_bash("grep -r foo . | sort | uniq"));
        assert!(is_readonly_bash("cat file | tr 'a-z' 'A-Z' | head"));
        assert!(is_readonly_bash(
            "git log --oneline | head -10 | cut -d' ' -f1"
        ));
        assert!(is_readonly_bash("ls -la | sort -k5 -n | tail -10"));
    }

    /// xargs 后跟只读命令应被归类为只读，跟危险命令则非只读。
    /// xargs with a read-only inner command should be read-only;
    /// with a mutating inner command it should not.
    #[test]
    fn xargs_readonly_vs_mutating() {
        assert!(is_readonly_bash("find . -name '*.rs' | xargs grep 'foo'"));
        assert!(is_readonly_bash("find . -name '*.go' | xargs wc -l"));
        assert!(!is_readonly_bash("find . -name '*.tmp' | xargs rm"));
        assert!(!is_readonly_bash("find . | xargs chmod 644"));
    }

    /// sed 只读用法（-n 打印行范围）应放行；sed -i 原地编辑应拒绝。
    /// sed read-only usage (-n print range) should pass; sed -i in-place editing should not.
    #[test]
    fn sed_readonly_vs_mutating() {
        assert!(is_readonly_bash("sed -n '1140,1645p' src/ui/tui.rs"));
        assert!(is_readonly_bash("sed 's/foo/bar/' file.txt"));
        assert!(!is_readonly_bash("sed -i 's/foo/bar/' file.txt"));
        assert!(!is_readonly_bash("sed -ni 's/foo/bar/' file.txt"));
        assert!(!is_readonly_bash("sed --in-place 's/foo/bar/' file.txt"));
    }

    /// awk 只读用法应放行；awk system() 应拒绝。
    /// awk read-only usage should pass; awk system() should not.
    #[test]
    fn awk_readonly_vs_mutating() {
        assert!(is_readonly_bash("awk '{print $1}' file.txt"));
        assert!(is_readonly_bash("awk -F: '{print $2}' /etc/passwd"));
        assert!(!is_readonly_bash("awk 'BEGIN { system(\"rm -rf /\") }'"));
        assert!(!is_readonly_bash("awk '{print | getline cmd}' file"));
    }

    /// 引号内的管道符不应触发分段（grep 正则中的 `\|` 是或操作符，不是管道）。
    /// Pipe characters inside quotes should not trigger segmentation
    /// (`\|` in grep regex is an alternation operator, not a pipe).
    #[test]
    fn quoted_pipe_not_segmented() {
        assert!(is_readonly_bash("grep -rn 'foo\\|bar' src | sort"));
        assert!(is_readonly_bash("grep 'a|b' file"));
        assert!(is_readonly_bash("grep \"a|b\" file"));
        assert!(is_readonly_bash(
            "grep -rn 'ContextHook::new\\|context_limit' ./src --include='*.rs' | grep -v 'target/'"
        ));
    }

    /// 引号内的 `>` 不应被视为重定向。
    /// `>` inside quotes should not be treated as redirection.
    #[test]
    fn quoted_redirect_not_mutating() {
        assert!(is_readonly_bash("grep 'a>b' file"));
        assert!(is_readonly_bash("grep \"a>b\" file"));
    }

    /// 重定向到 /dev/null、文件描述符、文件都允许——沙盒负责路径安全。
    /// Redirection to /dev/null, file descriptors, or files is all allowed —
    /// the sandbox handles path security.
    #[test]
    fn devnull_redirect_is_readonly() {
        assert!(is_readonly_bash("git diff HEAD~1 2>/dev/null | head -500"));
        assert!(is_readonly_bash("grep pattern file 2>/dev/null"));
        assert!(is_readonly_bash("echo hello >/dev/null"));
        assert!(is_readonly_bash("grep pattern file 2>&1 | head"));
        assert!(is_readonly_bash("echo hello > file.txt"));
        assert!(is_readonly_bash("cat file >> output.txt"));
    }

    /// 反斜杠转义的分隔符不应触发分段。
    /// Backslash-escaped separators should not trigger segmentation.
    #[test]
    fn escaped_separator_not_segmented() {
        assert!(is_readonly_bash("grep 'a\\;b' file"));
    }

    /// Token-boundary 守护：命令名恰好以只读前缀开头但后跟非空白字符时，
    /// 不应被误判为只读。`lsd` / `cat-bomb` / `findstr` 应归为会改变状态。
    /// Token-boundary guard: a command whose name merely starts with a readonly
    /// prefix but is followed by a non-whitespace char must NOT be classified
    /// read-only. `lsd` / `cat-bomb` / `findstr` should be mutating.
    #[test]
    fn prefix_boundary_rejects_suffixed_commands() {
        assert!(!is_readonly_bash("lsd --delete"));
        assert!(!is_readonly_bash("cat-bomb"));
        assert!(!is_readonly_bash("findstr x"));
        // 多词前缀也需边界守护：`git statusish` 不应匹配 `git status`
        assert!(!is_readonly_bash("git statusish"));
        assert!(!is_readonly_bash("git logish"));
    }

    /// Token-boundary 回归守护：合法的只读命令不得因边界修复而被拒。
    /// Token-boundary regression guard: legitimate read-only commands must
    /// still pass after the boundary fix.
    #[test]
    fn prefix_boundary_keeps_legitimate_readonly() {
        assert!(is_readonly_bash("ls -la"));
        assert!(is_readonly_bash("cat file"));
        assert!(is_readonly_bash("git status"));
        assert!(is_readonly_bash("git log --oneline"));
        assert!(is_readonly_bash("find . -name x"));
        assert!(is_readonly_bash("sed -n '1p' file"));
        assert!(is_readonly_bash("awk '{print}' file"));
        assert!(is_readonly_bash("grep pattern src"));
        assert!(is_readonly_bash("head -10 file"));
        assert!(is_readonly_bash("stat file.txt"));
    }

    /// xargs 内部命令也需 token-boundary 守护：`xargs lsd` 应归为会改变状态，
    /// `xargs grep` 应保持只读。
    /// xargs inner command needs the token-boundary guard too: `xargs lsd`
    /// should be mutating, `xargs grep` should stay read-only.
    #[test]
    fn xargs_inner_command_boundary() {
        assert!(!is_readonly_bash("xargs lsd"));
        assert!(!is_readonly_bash("find . | xargs cat-bomb"));
        assert!(is_readonly_bash("find . -name '*.rs' | xargs grep 'foo'"));
        assert!(is_readonly_bash("find . -name '*.go' | xargs wc -l"));
    }

    /// offset/limit 分页：默认从第 0 行读取 max_read_lines 行。
    /// offset/limit pagination: default reads from line 0 up to max_read_lines.
    #[tokio::test]
    async fn read_file_default_pagination() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_read_file_pagination.txt");
        let content: String = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let tool = ReadFile { max_read_lines: 10 };
        let args = ReadFileArgs {
            path: path.to_string_lossy().to_string(),
            offset: None,
            limit: None,
        };
        let result = tool.call(args).await.unwrap();

        assert!(result.contains("line 0"));
        assert!(result.contains("line 9"));
        assert!(!result.contains("line 10\n"));
        assert!(result.contains("截断"));

        std::fs::remove_file(&path).ok();
    }

    /// offset 跳过前 N 行，limit 控制读取行数。
    /// offset skips first N lines, limit controls how many lines to read.
    #[tokio::test]
    async fn read_file_offset_limit() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_read_file_offset.txt");
        let content: String = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        let tool = ReadFile { max_read_lines: 10 };
        let args = ReadFileArgs {
            path: path.to_string_lossy().to_string(),
            offset: Some(20),
            limit: Some(5),
        };
        let result = tool.call(args).await.unwrap();

        assert!(result.contains("line 20"));
        assert!(result.contains("line 24"));
        assert!(!result.contains("line 19"));
        assert!(!result.contains("line 25\n"));
        assert!(result.contains("截断"));
        assert!(result.contains("showing lines 21-25"));

        std::fs::remove_file(&path).ok();
    }

    /// offset 超过文件末尾时返回空字符串。
    /// offset past end of file returns empty string.
    #[tokio::test]
    async fn read_file_offset_past_end() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_read_file_past_end.txt");
        std::fs::write(&path, "only one line\n").unwrap();

        let tool = ReadFile { max_read_lines: 10 };
        let args = ReadFileArgs {
            path: path.to_string_lossy().to_string(),
            offset: Some(100),
            limit: None,
        };
        let result = tool.call(args).await.unwrap();

        // offset is clamped to total (1 line), end = min(1+10, 1) = 1, selected = lines[1..1] = empty
        // Since selected is empty and offset==0 is false, it goes to the truncation branch
        assert!(result.contains("截断") || result.is_empty());

        std::fs::remove_file(&path).ok();
    }

    // ── edit_file 模糊匹配与多编辑测试 ──
    // ── edit_file fuzzy-match and multi-edit tests ──

    /// 最近似匹配：off-by-one-space 的 near-miss 应被找到，相似度 ≥ 0.9，行号正确。
    /// Closest match: an off-by-one-space near miss should be found with
    /// similarity ≥ 0.9 and the correct line range.
    #[test]
    fn closest_match_off_by_one_space() {
        // 文件 20 行，needle 11 行（前 11 行，第 6 行多一个空格）。
        // ratio = 2*10/(11+11) ≈ 0.909 ≥ 0.9。
        // 20-line file, 11-line needle (first 11 lines, line 6 has an extra space).
        // ratio = 2*10/(11+11) ≈ 0.909 ≥ 0.9.
        let file_lines: Vec<String> =
            (0..20).map(|i| format!("line {i}: content here")).collect();
        let file = file_lines.join("\n") + "\n";
        let mut needle_lines = file_lines[..11].to_vec();
        needle_lines[5] = "line 5:  content here".to_string();
        let needle = needle_lines.join("\n");
        let m = closest_match(&file, &needle, 15).expect("should find near match");
        assert!(m.similarity >= 0.9, "similarity was {}", m.similarity);
        assert_eq!(m.start_line, 1);
        assert_eq!(m.end_line, 11);
    }

    /// 完全无关的文本应返回 None。
    /// Completely unrelated text should return None.
    #[test]
    fn closest_match_unrelated_returns_none() {
        let file = "fn foo() {\n    let x = 1;\n}\n";
        let needle = "THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG 1234567890";
        assert!(closest_match(file, needle, 15).is_none());
    }

    /// 错误信息应包含行号范围、相似度百分比和文件片段文本。
    /// Error message should contain the line range, similarity percentage,
    /// and the file text snippet at that location.
    #[tokio::test]
    async fn edit_file_error_contains_fuzzy_hint() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_fuzzy_hint.txt");
        let file_content = "fn foo() {\n    let x = 1;\n    bar(x);\n}\n";
        std::fs::write(&path, file_content).unwrap();
        // old 4 行中有 3 行匹配（line 2 "let x = 2" vs "let x = 1" 不同）→ ratio = 0.75 ≥ 0.5
        let old = "fn foo() {\n    let x = 2;\n    bar(x);\n}\n";
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: path.to_string_lossy().to_string(),
            old: Some(old.into()),
            new: Some("replaced".into()),
            edits: None,
        };
        let err = tool.call(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("lines 1-4"), "msg should contain line range: {msg}");
        assert!(msg.contains("75%"), "msg should contain similarity pct: {msg}");
        assert!(msg.contains("let x = 1"), "msg should contain snippet text: {msg}");
        std::fs::remove_file(&path).ok();
    }

    /// 单次替换 happy path：行为与旧版字节一致（仅替换首次出现）。
    /// Single-form happy path: byte-identical to legacy (first occurrence only).
    #[tokio::test]
    async fn edit_file_single_form_happy_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_single.txt");
        std::fs::write(&path, "alpha beta beta gamma\n").unwrap();
        let raw_path = path.to_string_lossy().to_string();
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: raw_path.clone(),
            old: Some("beta".into()),
            new: Some("BETA".into()),
            edits: None,
        };
        let result = tool.call(args).await.unwrap();
        assert_eq!(result, format!("edited {raw_path}"));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "alpha BETA beta gamma\n");
        std::fs::remove_file(&path).ok();
    }

    /// 批量编辑：3 对按序应用，pair 2 patch pair 1 的输出（证明内存顺序语义）。
    /// Multi-edit: 3 pairs applied in order where pair 2 patches pair 1's output
    /// (proves in-memory sequential semantics).
    #[tokio::test]
    async fn edit_file_multi_three_pairs_sequential() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_multi_seq.txt");
        std::fs::write(&path, "AAA BBB CCC\n").unwrap();
        let raw_path = path.to_string_lossy().to_string();
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: raw_path.clone(),
            old: None,
            new: None,
            edits: Some(vec![
                EditPair {
                    old: "AAA".into(),
                    new: "XXX".into(),
                },
                EditPair {
                    old: "XXX BBB".into(),
                    new: "YYY ZZZ".into(),
                },
                EditPair {
                    old: "CCC".into(),
                    new: "DDD".into(),
                },
            ]),
        };
        let result = tool.call(args).await.unwrap();
        assert_eq!(result, format!("edited {raw_path} (3 edits)"));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "YYY ZZZ DDD\n");
        std::fs::remove_file(&path).ok();
    }

    /// 批量编辑：3 对中 pair 2 缺失 → 文件不变，错误含 pair 索引 2。
    /// Multi-edit: pair 2 of 3 missing → file UNCHANGED on disk, error names pair index 2.
    #[tokio::test]
    async fn edit_file_multi_missing_pair_unchanged() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_multi_missing.txt");
        let original = "alpha\nbeta\ngamma\n";
        std::fs::write(&path, original).unwrap();
        let raw_path = path.to_string_lossy().to_string();
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: raw_path.clone(),
            old: None,
            new: None,
            edits: Some(vec![
                EditPair {
                    old: "alpha".into(),
                    new: "ALPHA".into(),
                },
                EditPair {
                    old: "NONEXISTENT".into(),
                    new: "X".into(),
                },
                EditPair {
                    old: "gamma".into(),
                    new: "GAMMA".into(),
                },
            ]),
        };
        let err = tool.call(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pair 2"), "error should name pair index 2: {msg}");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original);
        std::fs::remove_file(&path).ok();
    }

    /// 两种形式同时出现 → 清晰错误。
    /// Both forms present → clear error.
    #[tokio::test]
    async fn edit_file_both_forms_error() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_both_forms.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: path.to_string_lossy().to_string(),
            old: Some("hello".into()),
            new: Some("world".into()),
            edits: Some(vec![EditPair {
                old: "hello".into(),
                new: "world".into(),
            }]),
        };
        let err = tool.call(args).await.unwrap_err();
        assert!(
            err.to_string().contains("cannot specify both"),
            "should reject both forms"
        );
        std::fs::remove_file(&path).ok();
    }

    /// 两种形式都没有 → 清晰错误。
    /// Neither form present → clear error.
    #[tokio::test]
    async fn edit_file_neither_form_error() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_neither.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let tool = EditFile {
            checkpoints: Arc::new(crate::checkpoint::CheckpointStore::new()),
        };
        let args = EditFileArgs {
            path: path.to_string_lossy().to_string(),
            old: None,
            new: None,
            edits: None,
        };
        let err = tool.call(args).await.unwrap_err();
        assert!(
            err.to_string().contains("must specify"),
            "should reject empty args"
        );
        std::fs::remove_file(&path).ok();
    }

    /// EditFile 应在写入前记录恰好一个检查点，保留原始内容。
    /// EditFile should record exactly one checkpoint with the original content before writing.
    #[tokio::test]
    async fn edit_file_records_checkpoint_before_write() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_edit_checkpoint.txt");
        let original = "alpha\n";
        std::fs::write(&path, original).unwrap();

        let store = Arc::new(crate::checkpoint::CheckpointStore::new());
        let task = store.begin_task();
        let tool = EditFile {
            checkpoints: store.clone(),
        };
        let args = EditFileArgs {
            path: path.to_string_lossy().to_string(),
            old: Some("alpha".into()),
            new: Some("ALPHA".into()),
            edits: None,
        };
        let _ = tool.call(args).await.unwrap();

        // Exactly one checkpoint for this task
        let tasks = store.tasks_with_files();
        assert_eq!(tasks, vec![(task, 1)]);

        // Checkpoint before = original content
        let paths = store.task_paths(task);
        assert_eq!(paths.len(), 1);

        // Rewind restores the original
        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, original);

        std::fs::remove_file(&path).ok();
    }

    /// WriteFile 应在写入前记录检查点（新建文件 before=None）。
    /// WriteFile should record a checkpoint before writing (new file → before=None).
    #[tokio::test]
    async fn write_file_records_checkpoint_for_new_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_write_checkpoint_new.txt");
        std::fs::remove_file(&path).ok();

        let store = Arc::new(crate::checkpoint::CheckpointStore::new());
        let task = store.begin_task();
        let tool = WriteFile {
            checkpoints: store.clone(),
        };
        let args = WriteFileArgs {
            path: path.to_string_lossy().to_string(),
            content: "hello\n".into(),
        };
        let _ = tool.call(args).await.unwrap();

        // One checkpoint, before=None (file didn't exist)
        let tasks = store.tasks_with_files();
        assert_eq!(tasks, vec![(task, 1)]);

        // Rewind deletes the created file
        let outcomes = store.rewind_task(task);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, crate::checkpoint::RewindOutcome::Deleted);
        assert!(!std::path::Path::new(&path).exists());
    }

    // ── todo_write tests ──

    fn todo_ctx_for_test() -> (
        Arc<Mutex<Vec<TodoItem>>>,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
        TodoContext,
    ) {
        let store = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let ctx = TodoContext {
            store: store.clone(),
            tx,
        };
        (store, rx, ctx)
    }

    #[tokio::test]
    async fn todo_write_empty_id_rejected() {
        let (_store, _rx, ctx) = todo_ctx_for_test();
        let tool = TodoWrite::new(ctx);
        let args = TodoWriteArgs {
            todos: vec![TodoItem {
                id: String::new(),
                content: "x".into(),
                status: TodoStatus::Pending,
            }],
        };
        let err = tool.call(args).await.unwrap_err();
        assert!(err.to_string().contains("empty"), "empty id rejected: {err}");
    }

    #[tokio::test]
    async fn todo_write_duplicate_id_rejected() {
        let (_store, _rx, ctx) = todo_ctx_for_test();
        let tool = TodoWrite::new(ctx);
        let args = TodoWriteArgs {
            todos: vec![
                TodoItem {
                    id: "a".into(),
                    content: "first".into(),
                    status: TodoStatus::Pending,
                },
                TodoItem {
                    id: "a".into(),
                    content: "dup".into(),
                    status: TodoStatus::Pending,
                },
            ],
        };
        let err = tool.call(args).await.unwrap_err();
        assert!(err.to_string().contains("duplicate"), "duplicate id rejected: {err}");
    }

    #[tokio::test]
    async fn todo_write_two_in_progress_rejected() {
        let (_store, _rx, ctx) = todo_ctx_for_test();
        let tool = TodoWrite::new(ctx);
        let args = TodoWriteArgs {
            todos: vec![
                TodoItem {
                    id: "a".into(),
                    content: "x".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    id: "b".into(),
                    content: "y".into(),
                    status: TodoStatus::InProgress,
                },
            ],
        };
        let err = tool.call(args).await.unwrap_err();
        assert!(
            err.to_string().contains("in_progress") || err.to_string().contains("in progress"),
            "two in_progress rejected: {err}"
        );
    }

    #[tokio::test]
    async fn todo_write_valid_replaces_store_and_confirms() {
        let (store, mut rx, ctx) = todo_ctx_for_test();
        let tool = TodoWrite::new(ctx);
        let args = TodoWriteArgs {
            todos: vec![
                TodoItem {
                    id: "a".into(),
                    content: "step 1".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    id: "b".into(),
                    content: "step 2".into(),
                    status: TodoStatus::Pending,
                },
            ],
        };
        let out = tool.call(args).await.unwrap();
        assert!(
            out.contains("todos updated") && out.contains("2 items") && out.contains("1 in progress"),
            "confirmation string: {out}"
        );
        let stored = store.lock().unwrap().clone();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].id, "a");
        assert_eq!(stored[1].status, TodoStatus::Pending);
        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::TodoUpdate { todos } => assert_eq!(todos.len(), 2),
            _ => panic!("expected TodoUpdate event"),
        }
    }

    #[test]
    fn todo_status_serde_roundtrip() {
        let json = serde_json::to_string(&TodoStatus::InProgress).unwrap();
        assert_eq!(json, "\"in_progress\"");
        let back: TodoStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn todo_write_store_isolation_shared_arc() {
        // 两个 TodoWrite 工具共享同一 Arc<Mutex<Vec<TodoItem>>>。
        // Two TodoWrite tools sharing one Arc — writes from one are visible to the other.
        let (store, _rx, ctx) = todo_ctx_for_test();
        let tool1 = TodoWrite::new(ctx.clone());
        let tool2 = TodoWrite::new(ctx.clone());
        let args1 = TodoWriteArgs {
            todos: vec![TodoItem {
                id: "x".into(),
                content: "from tool1".into(),
                status: TodoStatus::Completed,
            }],
        };
        tool1.call(args1).await.unwrap();
        let stored = store.lock().unwrap().clone();
        assert_eq!(stored.len(), 1, "store should have tool1's item");
        assert_eq!(stored[0].id, "x");
        let args2 = TodoWriteArgs {
            todos: vec![
                TodoItem {
                    id: "y".into(),
                    content: "from tool2".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    id: "z".into(),
                    content: "also tool2".into(),
                    status: TodoStatus::Pending,
                },
            ],
        };
        tool2.call(args2).await.unwrap();
        let stored = store.lock().unwrap().clone();
        assert_eq!(stored.len(), 2, "store should be replaced by tool2's list");
        assert_eq!(stored[0].id, "y");
        assert_eq!(stored[1].id, "z");
    }

    // ─── BashArgs / tool_names tests ────────────────────────────────────
    // ─── BashArgs / tool_names tests ────────────────────────────────────

    /// `BashArgs` without `background` field deserializes to `background: false`.
    /// `BashArgs` 不含 `background` 字段时反序列化为 `background: false`。
    #[test]
    fn bash_args_background_defaults_false() {
        let args: BashArgs = serde_json::from_str(r#"{"command": "ls"}"#).expect("parse");
        assert!(!args.background, "background should default to false");
    }

    /// `BashArgs` with `background: true` deserializes correctly.
    /// `BashArgs` 含 `background: true` 时正确反序列化。
    #[test]
    fn bash_args_background_true() {
        let args: BashArgs =
            serde_json::from_str(r#"{"command": "cargo watch", "background": true}"#).expect("parse");
        assert!(args.background);
        assert_eq!(args.command, "cargo watch");
    }

    /// `tool_names()` includes `bash_output` and `kill_shell`.
    /// `tool_names()` 包含 `bash_output` 和 `kill_shell`。
    #[test]
    fn tool_names_includes_new_tools() {
        let names = tool_names();
        assert!(names.contains(&"bash_output"), "should contain bash_output");
        assert!(names.contains(&"kill_shell"), "should contain kill_shell");
        assert!(names.contains(&"run_bash"), "should still contain run_bash");
        assert!(names.contains(&"task"), "should contain task");
    }
}
