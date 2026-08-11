// 内置工具模块：为 Agent 提供文件读写、命令执行与联网搜索能力。
// Built-in tools module: provides the Agent with file I/O, command execution, and web search.
// 各工具的 `description()` 是面向模型（LLM）的自然语言提示，统一使用中文
// The `description()` of each tool is a natural-language prompt aimed at the model (LLM);
// （本项目主要使用中文模型：DeepSeek / GLM / Kimi）。
// (this project primarily uses Chinese models: DeepSeek / GLM / Kimi).
use anyhow::Result;
use rig_core::tool::PortableTool;
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
impl PortableTool for ReadFile {
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
impl PortableTool for EditFile {
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
         把完整文件内容放入 content 参数，不要只在回复里贴代码。".to_string()
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
    sandbox: crate::sandbox::Sandbox,
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
    ///
    /// 用 tokio::process 而非 std::process：阻塞式 Command 会卡住整个 async runtime
    /// （长命令期间 TUI 无法重绘、无法响应 Esc）。kill_on_drop 确保任务被 abort（Esc）
    /// 时子进程一并被杀，而不是成为孤儿进程继续运行。
    /// Uses tokio::process instead of std::process: a blocking Command stalls the whole
    /// async runtime (no TUI redraw, no Esc response during long commands). kill_on_drop
    /// ensures aborting the task (Esc) also kills the child instead of orphaning it.
    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let out = if let Some(bwrap_argv) = self.sandbox.wrap_command() {
            let mut cmd = tokio::process::Command::new(&bwrap_argv[0]);
            for arg in &bwrap_argv[1..] {
                cmd.arg(arg);
            }
            cmd.arg("--")
                .arg("sh")
                .arg("-c")
                .arg(&args.command)
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| ToolError(e.to_string()))?
        } else {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&args.command)
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
            self.max_bash_output_chars,
        ))
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
    sandbox: crate::sandbox::Sandbox,
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
        let path = std::path::Path::new(&args.path);
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
        let out = if let Some(bwrap_argv) = self.sandbox.wrap_command() {
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
/// 优先级：MY_AGENT_PROXY > HTTPS_PROXY > HTTP_PROXY。均未设置时不使用代理。
/// Builds a reqwest Client with proxy support.
/// Priority: MY_AGENT_PROXY > HTTPS_PROXY > HTTP_PROXY. No proxy when none are set.
fn build_web_client() -> std::result::Result<reqwest::Client, ToolError> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("my-agent/0.1 (web tool)");
    let proxy_url = std::env::var("MY_AGENT_PROXY")
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
        let client = build_web_client()?;
        let resp = client
            .get(&args.url)
            .send()
            .await
            .map_err(|e| ToolError(format!("请求失败: {e}。提示: 可能需要设置代理，如 export MY_AGENT_PROXY=http://127.0.0.1:7890")))?;
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
impl PortableTool for WebSearch {
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
        let client = build_web_client()?;
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
            .map_err(|e| ToolError(format!("搜索请求失败: {e}。提示: 在中国大陆可能需要设置代理，如 export MY_AGENT_PROXY=http://127.0.0.1:7890")))?
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
            '\\' if !in_single => { escaped = true; current.push(ch); }
            '\'' if !in_double => { in_single = !in_single; current.push(ch); }
            '"' if !in_single => { in_double = !in_double; current.push(ch); }
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
            _ => { current.push(ch); }
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
            '\\' if !in_single => { escaped = true; }
            '\'' if !in_double => { in_single = !in_single; }
            '"' if !in_single => { in_double = !in_double; }
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
            '\\' if !in_single => { escaped = true; }
            '\'' if !in_double => { in_single = !in_single; }
            '"' if !in_single => { in_double = !in_double; }
            '>' if !in_single && !in_double => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] == '>' { j += 1; }
                while j < chars.len() && chars[j] == ' ' { j += 1; }
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

/// 判断 shell 命令是否为只读（可安全自动执行）还是会改变状态（需询问）。
/// Determines whether a shell command is read-only (safe to auto-run) or mutating (needs prompting).
/// 拿不准时返回 false —— 循环会将其视为"会改变状态"并询问人类，
/// Returns false when unsure — the loop treats it as "mutating" and asks a human,
/// 因为自动执行破坏性命令比多一次确认更危险。
/// because auto-running a destructive command is more dangerous than one extra confirmation.
pub fn is_readonly_bash(command: &str) -> bool {
    const READONLY_PREFIXES: &[&str] = &[
        // 目录/文件查看 / Directory & file viewing
        "ls", "cat", "head", "tail", "tree", "file", "stat", "du", "df",
        "wc", "nl", "tac", "rev", "bat", "eza", "exa",
        // 搜索 / Search
        "grep", "rg", "ag", "ack", "find", "fd",
        // Git 只读 / Git read-only
        "git status", "git log", "git diff", "git show",
        // 文本处理 / Text processing
        "sort", "uniq", "cut", "tr", "diff", "comm", "join", "paste",
        "fold", "fmt", "pr", "column", "expand", "unexpand",
        "shuf", "tsort", "seq", "sed", "awk",
        // 路径 / Path utilities
        "pwd", "basename", "dirname", "realpath", "readlink", "which",
        // 校验和 / Checksums
        "md5sum", "sha1sum", "sha256sum", "sha512sum",
        // 十六进制/字符串 / Hex & strings
        "xxd", "od", "hexdump", "strings",
        // 编码 / Encoding
        "iconv", "base64", "base32",
        // 系统信息 / System info
        "printenv", "whoami", "uname", "arch", "nproc", "uptime", "hostname",
        "echo", "date", "test", "true", "false",
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
                Some(cmd) if !READONLY_PREFIXES.iter().any(|p| cmd.starts_with(p)) => {
                    return false;
                }
                _ => {} // None = xargs defaults to echo (safe); Some = inner is readonly
            }
            continue;
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
    vec!["read_file", "edit_file", "write_file", "run_bash", "run_file", "web_fetch", "web_search"]
}

/// 内置工具集合 + 动态工具（tools_ext）。
/// Built-in tool set + dynamic tools (tools_ext).
/// 动态工具由 /add-tool 命令生成，需重新 cargo build 后生效。
/// Dynamic tools are generated by the /add-tool command and take effect after a fresh cargo build.
// TODO: add_builtin_tools() 未接入 load_all()，/add-tool 动态工具暂不注册到 0.41 builder。
#[allow(dead_code)]
pub fn builtin_tools(
    config: &crate::context::ContextConfig,
    sandbox: &crate::sandbox::Sandbox,
) -> anyhow::Result<rig_agent::tool::ToolSet> {
    let mut tools = rig_agent::tool::ToolSet::default();
    tools.add_tool(ReadFile {
        max_read_lines: config.max_read_lines,
    });
    tools.add_tool(EditFile);
    tools.add_tool(WriteFile);
    tools.add_tool(RunBash {
        max_bash_output_chars: config.max_bash_output_chars,
        sandbox: sandbox.clone(),
    });
    tools.add_tool(RunFile {
        max_output_chars: config.max_bash_output_chars,
        sandbox: sandbox.clone(),
    });
    tools.add_tool(WebFetch);
    tools.add_tool(WebSearch);
    tools.add_tools(crate::tools_ext::load_all());
    Ok(tools)
}

/// 将 6 个内置工具逐一注册到 builder 上（rig 0.41 的 `.tool()` 链式调用）。
/// Registers the 6 built-in tools onto a builder one by one (rig 0.41's `.tool()` chain).
/// `builtin_tools()` 返回 `ToolSet` 供侧边栏/列举使用；builder 不接受 `ToolSet`，
/// `builtin_tools()` returns a `ToolSet` for sidebar/listing use; the builder does not accept
/// 故在两处 builder 调用点使用此 helper 逐一注册。
/// a `ToolSet`, so this helper registers them one by one at the two builder call sites.
pub fn add_builtin_tools<M>(
    builder: rig_agent::agent::AgentBuilder<M, rig_agent::agent::NoToolConfig>,
    config: &crate::context::ContextConfig,
    sandbox: &crate::sandbox::Sandbox,
) -> rig_agent::agent::AgentBuilder<M, rig_agent::agent::WithBuilderTools>
where
    M: rig_core::completion::CompletionModel,
{
    builder
        .tool(ReadFile {
            max_read_lines: config.max_read_lines,
        })
        .tool(EditFile)
        .tool(WriteFile)
        .tool(RunBash {
            max_bash_output_chars: config.max_bash_output_chars,
            sandbox: sandbox.clone(),
        })
        .tool(RunFile {
            max_output_chars: config.max_bash_output_chars,
            sandbox: sandbox.clone(),
        })
        .tool(WebFetch)
        .tool(WebSearch)
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
        assert!(is_readonly_bash("git log --oneline | head -10 | cut -d' ' -f1"));
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
}
