// 内置工具模块：为 Agent 提供文件读写、命令执行与联网搜索能力。
// Built-in tools module: provides the Agent with file I/O, command execution, and web search.
// 各工具的 `description()` 是面向模型（LLM）的自然语言提示，统一使用中文
// The `description()` of each tool is a natural-language prompt aimed at the model (LLM);
// （本项目主要使用中文模型：DeepSeek / GLM / Kimi）。
// (this project primarily uses Chinese models: DeepSeek / GLM / Kimi).
use anyhow::Result;
use rig_core::tool::Tool;
use serde::Deserialize;
use serde_json::json;

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
}

/// 读取项目工作树内 UTF-8 文本文件的工具。
/// Tool that reads a UTF-8 text file from the project worktree.
struct ReadFile {
    max_read_lines: usize,
}

/// 实现 `read_file` 工具：按路径读取文件内容并返回字符串。
/// Implements the `read_file` tool: reads file content by path and returns a string.
impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Error = ToolError;
    type Args = ReadFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        format!("从项目工作树读取一个 UTF-8 文本文件。仅可访问项目目录及子目录内的文件；访问其它目录需要用户授权。输出截断到 {} 行。", self.max_read_lines)
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件的相对路径" }
            },
            "required": ["path"],
        })
    }

    /// 执行工具：读取文件内容，截断到 500 行。失败时返回 `ToolError`。
    /// Executes the tool: reads file content, truncates to 500 lines. Returns `ToolError` on failure.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let content = std::fs::read_to_string(&args.path).map_err(|e| ToolError(e.to_string()))?;
        Ok(crate::context::truncate_lines(&content, self.max_read_lines))
    }
}

/// `edit_file` 工具的输入参数：路径 + 待替换文本 + 替换后文本。
/// Input args for the `edit_file` tool: path + text to replace + replacement text.
#[derive(Deserialize)]
struct EditFileArgs {
    path: String,
    old: String,
    new: String,
}

/// 对文件做精确文本替换的工具。
/// Tool that performs exact text replacement in a file.
struct EditFile;

/// 实现 `edit_file` 工具：把文件中第一次出现的 `old` 替换为 `new`。
/// Implements the `edit_file` tool: replaces the first occurrence of `old` with `new`.
impl Tool for EditFile {
    const NAME: &'static str = "edit_file";
    type Error = ToolError;
    type Args = EditFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "把文件中第一次出现的 `old` 替换为 `new`。".to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
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

    /// 执行工具：读取、替换、写回。`old` 不存在时返回错误。
    /// Executes the tool: read, replace, write back. Returns an error if `old` is absent.
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

/// `write_file` 工具的输入参数：路径 + 完整文件内容。
/// Input args for the `write_file` tool: path + full file content.
#[derive(Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

/// 用给定内容创建或覆盖文件的工具。
/// Tool that creates or overwrites a file with the given content.
struct WriteFile;

/// 实现 `write_file` 工具：用给定内容创建或覆盖一个文件。
/// Implements the `write_file` tool: creates or overwrites a file with the given content.
impl Tool for WriteFile {
    const NAME: &'static str = "write_file";
    type Error = ToolError;
    type Args = WriteFileArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "用给定内容创建或覆盖一个文件。".to_string()
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
        std::fs::write(&args.path, &args.content).map_err(|e| ToolError(e.to_string()))?;
        Ok(format!("wrote {}", args.path))
    }
}

/// `run_bash` 工具的输入参数：一条 shell 命令。
/// Input args for the `run_bash` tool: a single shell command.
#[derive(Deserialize)]
struct BashArgs {
    command: String,
}

/// 在项目工作树内运行 shell 命令的工具。
/// Tool that runs a shell command inside the project worktree.
struct RunBash {
    max_bash_output_chars: usize,
}

/// 实现 `run_bash` 工具：运行 shell 命令并返回 stdout+stderr。
/// Implements the `run_bash` tool: runs a shell command and returns stdout+stderr.
impl Tool for RunBash {
    const NAME: &'static str = "run_bash";
    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        format!("在项目工作树内运行一条 shell 命令，返回 stdout+stderr。输出截断到 {} 字符。", self.max_bash_output_chars)
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string", "description": "要运行的 shell 命令" } },
            "required": ["command"],
        })
    }

    /// 执行工具：通过 `sh -c` 运行命令，收集退出码与输出，截断到 20000 字符。
    /// Executes the tool: runs the command via `sh -c`, collecting exit code and output,
    /// truncating combined stdout+stderr to 20000 chars.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .output()
            .map_err(|e| ToolError(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!(
            "exit={}\nstdout:\n{}\nstderr:\n{}",
            out.status.code().unwrap_or(-1),
            stdout,
            stderr
        );
        Ok(crate::context::truncate_at_char_boundary(
            &combined,
            self.max_bash_output_chars,
        ))
    }
}

// ─── 联网工具 ───────────────────────────────────────────────────────────
// ─── Web tools ───────────────────────────────────────────────────────────

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
impl Tool for WebFetch {
    const NAME: &'static str = "web_fetch";
    type Error = ToolError;
    type Args = WebFetchArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "抓取指定 URL 的网页内容，返回纯文本（自动去除 HTML 标签）。支持 HTTP/HTTPS。"
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
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("my-agent/0.1 (web_fetch tool)")
            .build()
            .map_err(|e| ToolError(e.to_string()))?;
        let resp = client
            .get(&args.url)
            .send()
            .await
            .map_err(|e| ToolError(format!("请求失败: {e}")))?;
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
        let text = if content_type.contains("text/html") || content_type.contains("application/xhtml") {
            strip_html(&body)
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

/// `web_search` 工具的输入参数：一个查询关键词。
/// Input args for the `web_search` tool: a single query keyword.
#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
}

/// 用 DuckDuckGo Instant Answer API 搜索网络。返回摘要文本与相关链接。
/// Searches the web via the DuckDuckGo Instant Answer API. Returns summary text and related links.
struct WebSearch;

/// 实现 `web_search` 工具：调用 DuckDuckGo API 并整理摘要与链接。
/// Implements the `web_search` tool: calls the DuckDuckGo API and collates summaries and links.
impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolError;
    type Args = WebSearchArgs;
    type Output = String;

    /// 返回面向 LLM 的工具描述（中文）。
    /// Returns the LLM-facing tool description (Chinese).
    fn description(&self) -> String {
        "在互联网上搜索给定查询，返回摘要与相关链接。使用 DuckDuckGo 搜索引擎。"
            .to_string()
    }

    /// 返回 JSON Schema 形式的参数定义。
    /// Returns the JSON Schema parameter definition.
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索查询关键词" }
            },
            "required": ["query"],
        })
    }

    /// 执行工具：请求 DuckDuckGo API，提取摘要/回答/定义/相关链接并格式化。
    /// Executes the tool: requests the DuckDuckGo API, extracts abstract/answer/definition/related links, and formats them.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        const MAX_RESULTS: usize = 8;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("my-agent/0.1 (web_search tool)")
            .build()
            .map_err(|e| ToolError(e.to_string()))?;
        let resp: serde_json::Value = client
            .get("https://api.duckduckgo.com/")
            .query(&[
                ("q", args.query.as_str()),
                ("format", "json"),
                ("no_html", "1"),
                ("skip_disambig", "1"),
            ])
            .send()
            .await
            .map_err(|e| ToolError(format!("搜索请求失败: {e}")))?
            .json()
            .await
            .map_err(|e| ToolError(format!("解析搜索结果失败: {e}")))?;

        let mut parts = Vec::new();

        // 1) 摘要（AbstractText / AbstractURL）
        // 1) Abstract (AbstractText / AbstractURL)
        if let Some(abs) = resp.get("AbstractText").and_then(|v| v.as_str()) {
            if !abs.is_empty() {
                parts.push(format!("摘要: {abs}"));
            }
        }
        if let Some(abs_url) = resp.get("AbstractURL").and_then(|v| v.as_str()) {
            if !abs_url.is_empty() {
                parts.push(format!("来源: {abs_url}"));
            }
        }

        // 2) Answer（直接回答）
        // 2) Answer (direct answer)
        if let Some(ans) = resp.get("Answer").and_then(|v| v.as_str()) {
            if !ans.is_empty() {
                parts.push(format!("回答: {ans}"));
            }
        }

        // 3) Definition（定义）
        // 3) Definition
        if let Some(def) = resp.get("Definition").and_then(|v| v.as_str()) {
            if !def.is_empty() {
                parts.push(format!("定义: {def}"));
            }
        }

        // 4) RelatedTopics —— 提取文本与链接
        // 4) RelatedTopics — extract text and links
        let mut links: Vec<(String, String)> = Vec::new();
        if let Some(related) = resp.get("RelatedTopics").and_then(|v| v.as_array()) {
            for item in related {
                // 直接条目
                // Direct entry
                if let Some(text) = item.get("Text").and_then(|v| v.as_str()) {
                    let url = item
                        .get("FirstURL")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        links.push((text.to_string(), url.to_string()));
                    }
                }
                // 嵌套 Topics（子分组）
                // Nested Topics (sub-groups)
                if let Some(topics) = item.get("Topics").and_then(|v| v.as_array()) {
                    for sub in topics {
                        if let Some(text) = sub.get("Text").and_then(|v| v.as_str()) {
                            let url = sub
                                .get("FirstURL")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if !text.is_empty() {
                                links.push((text.to_string(), url.to_string()));
                            }
                        }
                    }
                }
            }
        }

        // 5) Results（直接结果）
        // 5) Results (direct results)
        if let Some(results) = resp.get("Results").and_then(|v| v.as_array()) {
            for item in results {
                if let Some(text) = item.get("Text").and_then(|v| v.as_str()) {
                    let url = item
                        .get("FirstURL")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        links.push((text.to_string(), url.to_string()));
                    }
                }
            }
        }

        if links.is_empty() && parts.is_empty() {
            return Ok(format!(
                "未找到与「{}」相关的即时结果。可用 web_fetch 抓取具体网页获取更多信息。",
                args.query
            ));
        }

        if !parts.is_empty() {
            parts.push(String::new()); // 空行分隔
            // Blank line separator
        }
        parts.push(format!("相关结果（共 {} 条）:", links.len()));
        for (i, (text, url)) in links.iter().take(MAX_RESULTS).enumerate() {
            if url.is_empty() {
                parts.push(format!("  {}. {text}", i + 1));
            } else {
                parts.push(format!("  {}. {text}\n     {url}", i + 1));
            }
        }
        if links.len() > MAX_RESULTS {
            parts.push(format!("  …（还有 {} 条结果已省略）", links.len() - MAX_RESULTS));
        }

        Ok(parts.join("\n"))
    }
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
    let collapsed: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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

/// 判断 shell 命令是否为只读（可安全自动执行）还是会改变状态（需询问）。
/// Determines whether a shell command is read-only (safe to auto-run) or mutating (needs prompting).
/// 拿不准时返回 false —— 循环会将其视为"会改变状态"并询问人类，
/// Returns false when unsure — the loop treats it as "mutating" and asks a human,
/// 因为自动执行破坏性命令比多一次确认更危险。
/// because auto-running a destructive command is more dangerous than one extra confirmation.
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
        // 重定向 / 追加写入文件：会改变状态，非只读。
        if s.contains('>') || s.contains("2>") {
            return false;
        }
        if !READONLY_PREFIXES.iter().any(|p| s.starts_with(p)) {
            return false;
        }
    }
    true
}

/// 内置工具名称列表（供侧边栏显示）。
/// Built-in tool name list (for sidebar display).
pub fn tool_names() -> Vec<&'static str> {
    vec!["read_file", "edit_file", "write_file", "run_bash", "web_fetch", "web_search"]
}

/// 内置工具集合 + 动态工具（tools_ext）。
/// Built-in tool set + dynamic tools (tools_ext).
/// 动态工具由 /add-tool 命令生成，需重新 cargo build 后生效。
/// Dynamic tools are generated by the /add-tool command and take effect after a fresh cargo build.
pub fn builtin_tools(
    config: &crate::context::ContextConfig,
) -> Result<Vec<Box<dyn rig_core::tool::ToolDyn>>> {
    let mut tools: Vec<Box<dyn rig_core::tool::ToolDyn>> = vec![
        Box::new(ReadFile {
            max_read_lines: config.max_read_lines,
        }),
        Box::new(EditFile),
        Box::new(WriteFile),
        Box::new(RunBash {
            max_bash_output_chars: config.max_bash_output_chars,
        }),
        Box::new(WebFetch),
        Box::new(WebSearch),
    ];
    // 加载动态工具（由 ToolManifest 重新生成的 mod.rs 提供）
    // Load dynamic tools (provided by the mod.rs regenerated by ToolManifest)
    tools.extend(crate::tools_ext::load_all());
    Ok(tools)
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
        assert!(!is_readonly_bash("echo hi > file"));
        assert!(!is_readonly_bash(""));
    }
}
