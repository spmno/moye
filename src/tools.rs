// 内置工具模块：为 Agent 提供文件读写、命令执行与联网搜索能力。
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

// ─── 联网工具 ───────────────────────────────────────────────────────────

/// 抓取网页内容并转为纯文本返回。自动去除 HTML 标签、script/style 块，
/// 解码常见 HTML 实体，截断到合理长度以防 token 洪流。
#[derive(Deserialize)]
struct WebFetchArgs {
    url: String,
}

struct WebFetch;

impl Tool for WebFetch {
    const NAME: &'static str = "web_fetch";
    type Error = ToolError;
    type Args = WebFetchArgs;
    type Output = String;

    fn description(&self) -> String {
        "抓取指定 URL 的网页内容，返回纯文本（自动去除 HTML 标签）。支持 HTTP/HTTPS。"
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "要抓取的网页 URL（需包含 http:// 或 https://）" }
            },
            "required": ["url"],
        })
    }

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
            // 避免在多字节 UTF-8 字符中间切片导致 panic。
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

/// 用 DuckDuckGo Instant Answer API 搜索网络。返回摘要文本与相关链接。
#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
}

struct WebSearch;

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolError;
    type Args = WebSearchArgs;
    type Output = String;

    fn description(&self) -> String {
        "在互联网上搜索给定查询，返回摘要与相关链接。使用 DuckDuckGo 搜索引擎。"
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索查询关键词" }
            },
            "required": ["query"],
        })
    }

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
        if let Some(ans) = resp.get("Answer").and_then(|v| v.as_str()) {
            if !ans.is_empty() {
                parts.push(format!("回答: {ans}"));
            }
        }

        // 3) Definition（定义）
        if let Some(def) = resp.get("Definition").and_then(|v| v.as_str()) {
            if !def.is_empty() {
                parts.push(format!("定义: {def}"));
            }
        }

        // 4) RelatedTopics —— 提取文本与链接
        let mut links: Vec<(String, String)> = Vec::new();
        if let Some(related) = resp.get("RelatedTopics").and_then(|v| v.as_array()) {
            for item in related {
                // 直接条目
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
/// 移除 <script> / <style> 块，解码常见 HTML 实体，折叠空白。
fn strip_html(html: &str) -> String {
    // 移除 <script>...</script> 和 <style>...</style>（含内容）
    let without_scripts = remove_tags_with_content(html, "script");
    let without_styles = remove_tags_with_content(&without_scripts, "style");
    // 移除 HTML 注释
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
    text = decode_html_entities(&text);
    // 折叠连续空白为单个空格
    let collapsed: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed
}

/// 移除指定标签及其内容（如 <script>...</script>）。
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
        Box::new(WebFetch),
        Box::new(WebSearch),
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
