// 上下文管理模块：token 估算、预算追踪、对话历史压缩（compaction）与工具输出截断。
// Context management module: token estimation, budget tracking, conversation history
// compaction, and tool output truncation.
//
// 受 OpenCode 策略启发，提供多层防护：
// Inspired by OpenCode's strategy, provides multiple layers of protection:
// 1. TokenBudget — 追踪累计用量，检测上下文窗口溢出。
//    TokenBudget — tracks accumulated usage, detects context window overflow.
// 2. estimate_tokens / estimate_history_tokens — 基于字符的 token 估算（无需 tokenizer）。
//    estimate_tokens / estimate_history_tokens — char-based token estimation (no tokenizer needed).
// 3. split_history — 将对话历史切分为旧消息（待压缩）与近期消息（保留）。
//    split_history — splits conversation history into old messages (to compact) and recent (to keep).
// 4. format_messages_for_summary — 将消息格式化为摘要 LLM 的提示词。
//    format_messages_for_summary — formats messages into a prompt for the summarization LLM.
// 5. truncate_lines / truncate_at_char_boundary — 工具输出截断。
//    truncate_lines / truncate_at_char_boundary — tool output truncation.

use rig_core::OneOrMany;
use rig_core::completion::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
use rig_core::completion::{Message, Usage};
use serde::Deserialize;
use std::collections::HashMap;

// ─── 配置 ────────────────────────────────────────────────────────────────────
// ─── Configuration ────────────────────────────────────────────────────────────

/// 上下文管理配置，从 agent.toml 的 `[context]` 节加载。
/// Context management config, loaded from the `[context]` section of agent.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    /// 输出 token 上限：0 = 跳过（用模型自身默认输出预算；推理模型始终跳过）；
    /// >0 = 显式上限，作为 max_tokens 发给非推理模型（勿超过目标模型输出天花板，否则 400）。
    /// Output token cap: 0 = skip (use model's own default output budget; reasoning
    /// models always skip); >0 = explicit cap sent as max_tokens to non-reasoning
    /// models (must not exceed the target model's output ceiling, or 400 returns).
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: usize,

    /// 触发压缩的阈值（占有效预算的比例，0.0–1.0）。
    /// Compaction trigger threshold (fraction of effective budget, 0.0–1.0).
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,

    /// 压缩时保留的最近对话轮数（user+assistant 算一轮）。
    /// Recent turns to keep during compaction (user+assistant = 1 turn).
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: usize,

    /// run_bash 输出截断字符数。
    /// Max chars for run_bash combined stdout+stderr.
    #[serde(default = "default_max_bash_output")]
    pub max_bash_output_chars: usize,

    /// read_file 输出截断行数。
    /// Max lines for read_file output.
    #[serde(default = "default_max_read_lines")]
    pub max_read_lines: usize,

    /// 触发微压缩（Tier 1）的 token 阈值（估算 token 数超过此值时触发）。
    /// Token threshold to trigger microcompact (Tier 1) when estimated tokens exceed this.
    #[allow(dead_code)]
    #[serde(default = "default_microcompact_threshold")]
    pub microcompact_threshold: usize,

    /// 微压缩时保护的最近工具结果数量（最近 N 个 ToolResult 不清除）。
    /// Number of recent ToolResults to protect during microcompact (last N are not cleared).
    #[allow(dead_code)]
    #[serde(default = "default_microcompact_protected_results")]
    pub microcompact_protected_results: usize,
}

pub(crate) fn default_max_output_tokens() -> usize {
    0
}
pub(crate) fn default_compaction_threshold() -> f64 {
    0.5
}
pub(crate) fn default_keep_recent_turns() -> usize {
    2
}
pub(crate) fn default_max_bash_output() -> usize {
    20000
}
pub(crate) fn default_max_read_lines() -> usize {
    500
}
pub(crate) fn default_microcompact_threshold() -> usize {
    20000
}
pub(crate) fn default_microcompact_protected_results() -> usize {
    3
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_output_tokens: default_max_output_tokens(),
            compaction_threshold: default_compaction_threshold(),
            keep_recent_turns: default_keep_recent_turns(),
            max_bash_output_chars: default_max_bash_output(),
            max_read_lines: default_max_read_lines(),
            microcompact_threshold: default_microcompact_threshold(),
            microcompact_protected_results: default_microcompact_protected_results(),
        }
    }
}

// ─── Token 预算追踪 ──────────────────────────────────────────────────────────
// ─── Token Budget Tracking ─────────────────────────────────────────────────────

/// Token 预算追踪器：记录 API 返回的实际用量，检测上下文窗口溢出。
/// Token budget tracker: records actual usage from API responses, detects context window overflow.
#[derive(Debug, Clone)]
pub struct TokenBudget {
    /// 模型的上下文窗口大小（tokens）。
    /// Model's context window size (tokens).
    context_limit: usize,

    /// 最近一次 API 返回的输入 token 数（校准估算用）。
    /// Last API-reported input token count (for calibrating estimates).
    last_input_tokens: u64,

    /// 累计输入 token 数。
    /// Accumulated input tokens.
    accumulated_input: u64,
}

impl TokenBudget {
    /// 创建预算追踪器。
    /// Create a budget tracker.
    pub fn new(context_limit: usize) -> Self {
        Self {
            context_limit,
            last_input_tokens: 0,
            accumulated_input: 0,
        }
    }

    /// 有效预算 = 上下文窗口的 85%（预留 15% 给模型输出）。
    /// Effective budget = 85% of context window (reserve 15% for model output).
    pub fn effective_budget(&self) -> usize {
        self.context_limit * 85 / 100
    }

    /// 记录一轮 API 返回的实际 token 用量。
    /// Record actual token usage from an API response.
    pub fn record_usage(&mut self, usage: &Usage) {
        if usage.input_tokens > 0 {
            self.last_input_tokens = usage.input_tokens;
            self.accumulated_input += usage.input_tokens;
        }
    }

    /// 最近一次 API 返回的输入 token 数。
    /// Last API-reported input token count.
    pub fn last_input_tokens(&self) -> u64 {
        self.last_input_tokens
    }

    /// 累计输入 token 数。
    /// Accumulated input tokens.
    pub fn accumulated_input(&self) -> u64 {
        self.accumulated_input
    }

    /// 判断 token 数是否接近溢出阈值。
    /// Check if token count is near the overflow threshold.
    /// `ratio` 为当前估算 token 数占有效预算的比例。
    /// `ratio` is the fraction of the effective budget currently estimated.
    pub fn is_near_overflow(&self, estimated_tokens: usize, threshold: f64) -> bool {
        if self.context_limit == 0 {
            return false; // 未知窗口大小，不触发。
            // Unknown window size, don't trigger.
        }
        let budget = self.effective_budget();
        if budget == 0 {
            return false;
        }
        let ratio = estimated_tokens as f64 / budget as f64;
        ratio >= threshold
    }
}

// ─── Token 估算 ──────────────────────────────────────────────────────────────
// ─── Token Estimation ──────────────────────────────────────────────────────────

/// 判断字符是否为 CJK 字符（中日韩）。
/// Check if a character is a CJK (Chinese/Japanese/Korean) character.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK 统一汉字 / CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK 扩展 A / CJK Extension A
        | '\u{3000}'..='\u{30FF}' // CJK 符号与标点 + 平假名 + 片假名 / CJK Symbols + Hiragana + Katakana
        | '\u{FF00}'..='\u{FFEF}' // 全角字符 / Fullwidth forms
        | '\u{AC00}'..='\u{D7AF}' // 韩文音节 / Hangul Syllables
    )
}

/// 估算文本的 token 数。
/// Estimate the token count of a text.
///
/// 启发式规则（保守估算，偏高以避免上下文溢出）：
/// Heuristic rules (conservative — over-estimates to avoid context overflow):
/// - CJK 字符 ≈ 1 token/char。
///   CJK chars ≈ 1 token/char.
/// - 拉丁字符 ≈ 0.33 token/char（即 3 char/token）。
///   Latin chars ≈ 0.33 token/char (i.e., 3 char/token).
/// - 每条消息额外 4 token 开销（角色标签、分隔符）。
///   Per-message overhead of 4 tokens (role tags, delimiters).
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut cjk_count = 0usize;
    let mut other_count = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            other_count += 1;
        }
    }
    let cjk_tokens = cjk_count;
    let latin_tokens = other_count.div_ceil(3);
    cjk_tokens + latin_tokens
}

/// 从 Message 提取纯文本内容（用于 token 估算和摘要格式化）。
/// Extract plain text from a Message (for token estimation and summary formatting).
pub fn extract_text(msg: &Message) -> String {
    match msg {
        Message::System { content } => content.clone(),
        Message::User { content } => {
            let mut parts = Vec::new();
            for item in content.iter() {
                match item {
                    UserContent::Text(t) => parts.push(t.text.clone()),
                    UserContent::ToolResult(tr) => {
                        // 提取工具结果文本 / Extract tool result text
                        for c in tr.content.iter() {
                            if let ToolResultContent::Text(t) = c {
                                parts.push(format!("[tool_result: {}]", t.text));
                            }
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        Message::Assistant { content, .. } => {
            let mut parts = Vec::new();
            use rig_core::completion::AssistantContent;
            for item in content.iter() {
                match item {
                    AssistantContent::Text(t) => parts.push(t.text.clone()),
                    AssistantContent::ToolCall(tc) => {
                        parts.push(format!(
                            "[tool_call: {}({})]",
                            tc.function.name, tc.function.arguments
                        ));
                    }
                    AssistantContent::Reasoning(r) => {
                        // 提取 reasoning 文本用于 token 估算和摘要，跳过加密/不可读变体。
                        // Extract reasoning text for token estimation and summaries; skip opaque variants.
                        use rig_core::completion::message::ReasoningContent;
                        for rc in &r.content {
                            match rc {
                                ReasoningContent::Text { text, .. } => parts.push(text.clone()),
                                ReasoningContent::Summary(s) => parts.push(s.clone()),
                                ReasoningContent::Encrypted(_) | ReasoningContent::Redacted { .. } => {}
                            _ => {}
                            }
                        }
                    }
                    AssistantContent::Image(_) => {} // 图像内容无可提取文本
                }
            }
            parts.join("\n")
        }
    }
}

/// 估算一组 Message 的总 token 数（含每条消息 4 token 开销）。
/// Estimate total tokens for a slice of Messages (including 4-token overhead per message).
pub fn estimate_history_tokens(history: &[Message]) -> usize {
    if history.is_empty() {
        return 0;
    }
    let mut total = 0;
    for msg in history {
        let text = extract_text(msg);
        total += estimate_tokens(&text) + 4; // +4 overhead per message
    }
    total
}

// ─── 历史切分 ────────────────────────────────────────────────────────────────
// ─── History Splitting ─────────────────────────────────────────────────────────

/// 将对话历史切分为 (旧消息, 近期消息)。
/// Split conversation history into (old_messages, recent_messages).
///
/// `keep_recent_turns` 指定保留的最近 user→assistant 对话轮数。
/// `keep_recent_turns` specifies how many recent user→assistant turns to keep.
///
/// 一轮 = 一条 User 消息（非 ToolResult）+ 后续的 Assistant 消息和 ToolResult 消息，
/// 直到下一条 User 消息。
/// A turn = one User message (non-ToolResult) + subsequent Assistant and ToolResult
/// messages until the next User message.
#[allow(dead_code)]
pub fn split_history(
    history: &[Message],
    keep_recent_turns: usize,
) -> (Vec<Message>, Vec<Message>) {
    if history.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // 找到所有"用户对话轮起点"的索引。
    // Find all "user conversation turn start" indices.
    // 用户对话轮起点 = 非 ToolResult 的 User 消息。
    // A user turn start = a User message that is NOT a ToolResult.
    let mut turn_starts: Vec<usize> = Vec::new();
    for (i, msg) in history.iter().enumerate() {
        if is_user_text_message(msg) {
            turn_starts.push(i);
        }
    }

    // 如果用户轮数 <= keep_recent_turns，全部保留。
    // If user turn count <= keep_recent_turns, keep everything.
    if turn_starts.len() <= keep_recent_turns {
        return (Vec::new(), history.to_vec());
    }

    // 保留最近 keep_recent_turns 轮。
    // Keep the last keep_recent_turns turns.
    let split_idx = turn_starts[turn_starts.len() - keep_recent_turns];
    let old = history[..split_idx].to_vec();
    let recent = history[split_idx..].to_vec();
    (old, recent)
}

/// 判断 Message 是否为"用户文本消息"（非 ToolResult 的 User 消息）。
/// Check if a Message is a "user text message" (a User message that is NOT a ToolResult).
fn is_user_text_message(msg: &Message) -> bool {
    match msg {
        Message::User { content } => {
            // 如果所有内容都是 ToolResult，则不是用户文本消息。
            // If all content items are ToolResult, this is not a user text message.
            !content
                .iter()
                .all(|c| matches!(c, UserContent::ToolResult(_)))
        }
        _ => false,
    }
}

// ─── 摘要格式化 ───────────────────────────────────────────────────────────────
// ─── Summary Formatting ──────────────────────────────────────────────────────────

/// 将一组 Message 格式化为摘要 LLM 的提示词文本。
/// Format a slice of Messages into prompt text for the summarization LLM.
///
/// 每条消息截断到 ~4000 字符以防提示词过大，但保留足够的调查报告细节。
/// Each message is truncated to ~4000 chars to keep the summarization prompt manageable
/// while preserving enough detail from investigation reports.
pub fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        let role = match msg {
            Message::System { .. } => "System",
            Message::User { .. } => "User",
            Message::Assistant { .. } => "Assistant",
        };
        let text = extract_text(msg);
        let truncated = truncate_at_char_boundary(&text, 4000);
        parts.push(format!("[{}]: {}", role, truncated));
    }
    parts.join("\n\n")
}

// ─── 微压缩（Tier 1）─────────────────────────────────────────────────────────
// ─── Microcompact (Tier 1) ──────────────────────────────────────────────────────

/// 从工具结果文本中提取关键信息（函数签名、类型定义等），生成压缩摘要。
/// Extracts key info (function signatures, type defs, etc.) from tool result text,
/// producing a compact summary that preserves discoverability.
fn summarize_tool_result(tool_name: &str, tool_args: &str, content: &str) -> String {
    if content.len() <= 300 {
        return content.to_string();
    }

    // 从工具参数中提取文件路径或命令。
    // Extract file path or command from tool args.
    let args: Option<serde_json::Value> = serde_json::from_str(tool_args).ok();
    let descriptor = match (tool_name, &args) {
        ("read_file", Some(a)) => a
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!(
                "\u{5df2}\u{8bfb}\u{6587}\u{4ef6}: {p} \u{2014} \u{8bf7}\u{52ff}\u{91cd}\u{590d}\u{8bfb}\u{53d6}\u{ff0c}\u{5982}\u{9700}\u{7ec6}\u{8282}\u{8bf7}\u{53c2}\u{8003}\u{4e0b}\u{65b9}\u{7b7e}\u{540d} / DO NOT re-read: {p}"
            ))
            .unwrap_or_else(|| "\u{5df2}\u{8bfb}\u{6587}\u{4ef6}".to_string()),
        ("run_bash", Some(a)) => a
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| format!("\u{5df2}\u{6267}\u{884c}: {c}"))
            .unwrap_or_else(|| "\u{5df2}\u{6267}\u{884c}\u{547d}\u{4ee4}".to_string()),
        _ => format!("\u{5df2}\u{6e05}\u{9664}\u{5de5}\u{5177}\u{7ed3}\u{679c}: {tool_name}"),
    };

    // 提取代码签名行：fn / struct / enum / impl / trait / class / def 等。
    // Extract code signature lines.
    let sig_prefixes = [
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "async fn ",
        "pub async fn ",
        "struct ",
        "pub struct ",
        "enum ",
        "pub enum ",
        "impl ",
        "trait ",
        "pub trait ",
        "class ",
        "def ",
        "async def ",
    ];
    let signatures: Vec<&str> = content
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            sig_prefixes.iter().any(|p| t.starts_with(p))
        })
        .map(|line| line.trim().trim_end_matches('{').trim_end())
        .take(40)
        .collect();

    if !signatures.is_empty() {
        return format!(
            "[{descriptor}]\n\u{5173}\u{952e}\u{7b7e}\u{540d}:\n  {}",
            signatures.join("\n  "),
        );
    }

    // 无代码签名——保留首尾各 3 行。
    // No code signatures — keep first 3 + last 3 lines.
    let lines: Vec<&str> = content.lines().collect();
    let head: Vec<&str> = lines.iter().take(3).cloned().collect();
    let tail: Vec<&str> = lines.iter().rev().take(3).rev().cloned().collect();
    format!(
        "[{descriptor}]\n\u{9996}:\n{}\n...\n\u{5c3e}:\n{}",
        head.join("\n"),
        tail.join("\n"),
    )
}

/// 微压缩：无需 LLM 调用，将旧工具结果替换为保留关键信息的摘要。
/// Microcompact: replaces old tool results with a metadata-preserving summary (no LLM call).
///
/// 扫描历史中的 `ToolResult`，保护最近 `protected_results` 个，
/// 将更早的 ToolResult 内容替换为函数签名/首尾行摘要（而非完全清除）。
/// Scans history for `ToolResult` content, protects the last `protected_results`,
/// and replaces older tool result content with a summary of key signatures / first-last lines.
///
/// 返回修改后的历史副本；若无 ToolResult 需清除则返回原历史的克隆。
/// Returns a modified copy of the history; if no ToolResults need clearing,
/// returns a clone of the original.
#[allow(dead_code)]
pub fn microcompact(history: &[Message], protected_results: usize) -> Vec<Message> {
    // 构建 ToolCall ID → (tool_name, args) 映射，用于在清除时保留工具元信息。
    // Build ToolCall ID → (tool_name, args) map for preserving tool metadata on clear.
    let mut tool_call_info: HashMap<String, (String, String)> = HashMap::new();
    for msg in history {
        if let Message::Assistant { content, .. } = msg {
            for item in content.iter() {
                if let AssistantContent::ToolCall(tc) = item {
                    tool_call_info.insert(
                        tc.id.clone(),
                        (tc.function.name.clone(), tc.function.arguments.to_string()),
                    );
                }
            }
        }
    }

    // 收集所有 ToolResult 在历史中的 (msg_idx, content_idx) 位置。
    // Collect all (msg_idx, content_idx) positions of ToolResult in history.
    let mut tool_result_positions: Vec<(usize, usize)> = Vec::new();
    for (msg_idx, msg) in history.iter().enumerate() {
        if let Message::User { content } = msg {
            for (ci, item) in content.iter().enumerate() {
                if matches!(item, UserContent::ToolResult(_)) {
                    tool_result_positions.push((msg_idx, ci));
                }
            }
        }
    }

    // 工具结果数量 <= 保护数量，无需清除。
    // If tool result count <= protected count, nothing to clear.
    if tool_result_positions.len() <= protected_results {
        return history.to_vec();
    }

    // 待清除的位置：除了最后 `protected_results` 个。
    // Positions to clear: all except the last `protected_results`.
    let clear_count = tool_result_positions.len() - protected_results;
    let to_clear: std::collections::HashSet<(usize, usize)> = tool_result_positions[..clear_count]
        .iter()
        .cloned()
        .collect();

    let mut result: Vec<Message> = Vec::with_capacity(history.len());
    for (msg_idx, msg) in history.iter().enumerate() {
        match msg {
            Message::User { content } => {
                // 检查本消息是否有需要清除的 ToolResult。
                // Check if this message has any ToolResult to clear.
                let needs_clearing = content
                    .iter()
                    .enumerate()
                    .any(|(ci, _)| to_clear.contains(&(msg_idx, ci)));

                if !needs_clearing {
                    result.push(msg.clone());
                    continue;
                }

                // 重建 content，将待清除的 ToolResult 替换为保留元信息的摘要。
                // Rebuild content, replacing cleared ToolResults with a metadata summary.
                let mut new_items: Vec<UserContent> = Vec::new();
                for (ci, item) in content.iter().enumerate() {
                    if to_clear.contains(&(msg_idx, ci)) {
                        if let UserContent::ToolResult(tr) = item {
                            // 查找对应的 ToolCall 获取工具名和参数。
                            // Find matching ToolCall for tool name and args.
                            let (tool_name, tool_args) = tool_call_info
                                .get(&tr.id)
                                .or_else(|| {
                                    tr.call_id.as_ref().and_then(|cid| tool_call_info.get(cid))
                                })
                                .cloned()
                                .unwrap_or(("unknown".to_string(), "{}".to_string()));

                            // 提取工具结果的文本内容。
                            // Extract text content from the tool result.
                            let text_content: String = tr
                                .content
                                .iter()
                                .filter_map(|c| {
                                    if let ToolResultContent::Text(t) = c {
                                        Some(t.text.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n");

                            let summary =
                                summarize_tool_result(&tool_name, &tool_args, &text_content);
                            new_items.push(UserContent::ToolResult(ToolResult {
                                id: tr.id.clone(),
                                call_id: tr.call_id.clone(),
                                content: OneOrMany::one(ToolResultContent::text(summary)),
                            }));
                        }
                    } else {
                        new_items.push(item.clone());
                    }
                }

                let new_content = OneOrMany::many(new_items)
                    .expect("non-empty: original message had at least one content item");
                result.push(Message::User {
                    content: new_content,
                });
            }
            _ => result.push(msg.clone()),
        }
    }
    result
}

/// 压缩系统提示词（发给摘要 LLM 的 preamble）。
/// Compaction system prompt (preamble sent to the summarization LLM).
///
/// 采用 5 段锚定摘要模板（受 OpenCode 启发），支持增量更新。
/// Uses a 5-section anchored summary template (inspired by OpenCode), supporting incremental updates.
pub const COMPACTION_PREAMBLE: &str = "\
你是对话历史锚定摘要助手（anchored context summarization assistant）。\n\
如果提示中包含 <previous-summary> 块，请将其视为当前锚定摘要，\
保留仍然成立的细节，删除过时内容，合并新事实。\n\
否则，从对话历史创建新的锚定摘要。\n\
\n严格使用以下 5 个小节格式输出：\n\
\n## 1. Objective\n用户想要达成的目标（1-2 句）。\n\
\n## 2. Important Details\n约束/偏好、决策原因、重要事实/假设，或写\"(none)\"。\n\
\n## 3. Work State\n### Completed\n已完成的工作、验证过的事实、已做的更改，或写\"(none)\"。\n\
### Active\n当前进行的工作、部分更改、调查状态，或写\"(none)\"。\n\
### Blocked\n阻塞项、失败命令、未知项，或写\"(none)\"。\n\
\n## 4. Next Move\n紧接着需要做什么（1-2 句）。\n\
\n## 5. Relevant Files\n文件/目录路径及重要性，或写\"(none)\"。\n\
\n用简洁的中文输出。保留代码片段、文件路径、错误信息的原文。\
不要回答对话本身。不要提及你在做摘要。";

/// 在历史中查找上一次压缩的摘要文本（锚定摘要）。
/// Find the previous compaction summary text in history (anchored summary).
pub fn find_previous_summary(history: &[Message]) -> Option<String> {
    for msg in history.iter().rev() {
        if let Message::System { content } = msg
            && content.starts_with("[对话历史摘要")
        {
            return Some(content.clone());
        }
    }
    None
}

/// 构建压缩提示词，包含可选的上一次摘要（锚定模式）。
/// Build the compaction prompt, optionally including the previous summary (anchored mode).
pub fn build_compaction_prompt(messages: &[Message], previous_summary: Option<&str>) -> String {
    let history_text = format_messages_for_summary(messages);
    match previous_summary {
        Some(prev) => format!(
            "{history_text}\n\n\
             Update the anchored summary below using the conversation history above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{prev}\n</previous-summary>"
        ),
        None => format!(
            "{history_text}\n\n\
             Create a new anchored summary from the conversation history above."
        ),
    }
}

/// 基于 token 预算的尾部选择：保留最近 N 轮中能放入预算的部分。
/// Token-budget-aware tail selection: keep as many recent turns as fit in the budget.
pub fn select_head_tail(
    history: &[Message],
    tail_turns: usize,
    token_budget: usize,
) -> (Vec<Message>, Vec<Message>) {
    if history.is_empty() || tail_turns == 0 {
        return (history.to_vec(), Vec::new());
    }

    let turn_starts = find_turn_starts(history);
    if turn_starts.is_empty() {
        return (Vec::new(), history.to_vec());
    }

    if turn_starts.len() <= tail_turns {
        return (Vec::new(), history.to_vec());
    }

    let recent_starts: Vec<usize> = turn_starts.iter().rev().take(tail_turns).copied().collect();
    let earliest_recent = *recent_starts.last().unwrap();

    let mut total = 0usize;
    let mut keep_from = earliest_recent;

    for &start in recent_starts.iter() {
        let end = history.len();
        let turn_tokens = estimate_history_tokens(&history[start..end]);
        if total + turn_tokens <= token_budget {
            total += turn_tokens;
            keep_from = start;
        } else {
            break;
        }
    }

    if keep_from == 0 {
        return (Vec::new(), history.to_vec());
    }

    let head = history[..keep_from].to_vec();
    let tail = history[keep_from..].to_vec();
    (head, tail)
}

fn find_turn_starts(history: &[Message]) -> Vec<usize> {
    let mut starts = Vec::new();
    for (i, msg) in history.iter().enumerate() {
        if is_user_text_message(msg) {
            starts.push(i);
        }
    }
    starts
}

// ─── 截断工具 ─────────────────────────────────────────────────────────────────
// ─── Truncation Utilities ───────────────────────────────────────────────────────

/// 在 UTF-8 字符边界处截断字符串到 max_chars 字符，并追加截断提示。
/// Truncate a string to max_chars at a UTF-8 char boundary, appending a truncation notice.
pub fn truncate_at_char_boundary(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take(max_chars)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let total = s.chars().count();
    format!("{}…(截断 / truncated, {} chars total)", &s[..end], total)
}

/// 将文本截断到 max_lines 行，并在截断时追加提示。
/// Truncate text to max_lines, appending a notice when truncated.
#[allow(dead_code)]
pub fn truncate_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.to_string();
    }
    let head = lines[..max_lines].join("\n");
    format!("{}\n…(截断 / truncated, {} lines total)", head, lines.len())
}

// ─── 测试 ────────────────────────────────────────────────────────────────────
// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── estimate_tokens ──

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_english() {
        // "hello world" = 11 chars → ceil(11/3) = 4 tokens
        assert_eq!(estimate_tokens("hello world"), 4);
    }

    #[test]
    fn estimate_tokens_cjk() {
        // "你好世界" = 4 CJK chars → 4 tokens
        assert_eq!(estimate_tokens("你好世界"), 4);
    }

    #[test]
    fn estimate_tokens_mixed() {
        // "hello 你好" = 6 Latin + 2 CJK
        // Latin: ceil(6/3) = 2; CJK: 2; total = 4
        assert_eq!(estimate_tokens("hello 你好"), 4);
    }

    // ── estimate_history_tokens ──

    #[test]
    fn estimate_history_tokens_empty() {
        assert_eq!(estimate_history_tokens(&[]), 0);
    }

    #[test]
    fn estimate_history_tokens_with_messages() {
        let msgs = vec![Message::user("hello"), Message::assistant("hi there")];
        // "hello" = ceil(5/3) = 2 + 4 overhead = 6
        // "hi there" = ceil(8/3) = 3 + 4 overhead = 7
        let est = estimate_history_tokens(&msgs);
        assert!(est > 0);
        // Two messages with 4 overhead each = at least 8 + content tokens
        assert!(est >= 8);
    }

    // ── TokenBudget ──

    #[test]
    fn token_budget_effective_budget() {
        let budget = TokenBudget::new(128_000);
        assert_eq!(budget.effective_budget(), 108_800);
    }

    #[test]
    fn token_budget_effective_budget_zero_context() {
        let budget = TokenBudget::new(0);
        assert_eq!(budget.effective_budget(), 0);
    }

    #[test]
    fn token_budget_near_overflow_below_threshold() {
        let budget = TokenBudget::new(128_000);
        // 50% of 108800 = 54400
        assert!(!budget.is_near_overflow(50_000, 0.75));
    }

    #[test]
    fn token_budget_near_overflow_at_threshold() {
        let budget = TokenBudget::new(128_000);
        // 80% of 108800 ≈ 87040
        assert!(budget.is_near_overflow(100_000, 0.75));
    }

    #[test]
    fn token_budget_near_overflow_unknown_limit() {
        let budget = TokenBudget::new(0);
        assert!(!budget.is_near_overflow(999_999, 0.75));
    }

    #[test]
    fn token_budget_record_usage() {
        let mut budget = TokenBudget::new(128_000);
        let usage = Usage {
            input_tokens: 5000,
            output_tokens: 800,
            total_tokens: 5800,
            cached_input_tokens: 1000,
            cache_creation_input_tokens: 0,
            tool_use_prompt_tokens: 0,
            reasoning_tokens: 200,
        };
        budget.record_usage(&usage);
        assert_eq!(budget.last_input_tokens(), 5000);
        assert_eq!(budget.accumulated_input(), 5000);
    }

    // ── split_history ──

    #[test]
    fn split_history_basic() {
        // 5 user turns, keep 2 → old = first 3 turns, recent = last 2 turns
        let mut msgs = Vec::new();
        for i in 0..5 {
            msgs.push(Message::user(format!("user msg {i}")));
            msgs.push(Message::assistant(format!("assistant reply {i}")));
        }
        let (old, recent) = split_history(&msgs, 2);
        // old should have 3 turns = 6 messages
        assert_eq!(old.len(), 6);
        // recent should have 2 turns = 4 messages
        assert_eq!(recent.len(), 4);
        // recent starts with "user msg 3"
        assert!(extract_text(&recent[0]).contains("user msg 3"));
    }

    #[test]
    fn split_history_fewer_than_keep() {
        let msgs = vec![Message::user("hello"), Message::assistant("hi")];
        let (old, recent) = split_history(&msgs, 6);
        assert!(old.is_empty());
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn split_history_empty() {
        let (old, recent) = split_history(&[], 3);
        assert!(old.is_empty());
        assert!(recent.is_empty());
    }

    #[test]
    fn split_history_with_tool_results() {
        // User → Assistant(tool_call) → User(tool_result) → Assistant(text) → User(next turn)
        let msgs = vec![
            Message::user("do task 1"),
            Message::assistant_with_id("m1".into(), "calling tool"),
            Message::tool_result("t1", "result 1"),
            Message::assistant("task 1 done"),
            Message::user("do task 2"),
            Message::assistant("task 2 done"),
        ];
        // keep 1 turn → recent = last user msg onwards
        let (old, recent) = split_history(&msgs, 1);
        // old = first 4 messages (1 full turn)
        assert_eq!(old.len(), 4);
        // recent = last 2 messages
        assert_eq!(recent.len(), 2);
        assert!(extract_text(&recent[0]).contains("do task 2"));
    }

    // ── format_messages_for_summary ──

    #[test]
    fn format_messages_for_summary_basic() {
        let msgs = vec![Message::user("hello"), Message::assistant("hi there")];
        let formatted = format_messages_for_summary(&msgs);
        assert!(formatted.contains("[User]: hello"));
        assert!(formatted.contains("[Assistant]: hi there"));
    }

    // ── microcompact ──

    #[test]
    fn microcompact_clears_old_tool_results() {
        // 5 tool results, protect last 2 → first 3 should be summarized.
        let long_content =
            "fn hello() {}\nfn world() {}\npub fn foo() {}\nstruct Bar {}\n".repeat(20);
        let msgs = vec![
            Message::user("do task 1"),
            Message::assistant_with_id("m1".into(), "calling tool 1"),
            Message::tool_result("t1", &long_content),
            Message::assistant_with_id("m2".into(), "calling tool 2"),
            Message::tool_result("t2", &long_content),
            Message::assistant_with_id("m3".into(), "calling tool 3"),
            Message::tool_result("t3", &long_content),
            Message::assistant_with_id("m4".into(), "calling tool 4"),
            Message::tool_result("t4", "result 4 protected"),
            Message::assistant_with_id("m5".into(), "calling tool 5"),
            Message::tool_result("t5", "result 5 protected"),
        ];
        let compacted = microcompact(&msgs, 2);
        // Same length.
        assert_eq!(compacted.len(), msgs.len());

        // Old tool results should be summarized (signatures preserved, full content removed).
        let cleared = extract_text(&compacted[2]);
        assert!(cleared.contains("fn hello"));
        assert!(cleared.contains("fn world"));
        assert!(!cleared.contains(&long_content));

        let cleared2 = extract_text(&compacted[4]);
        assert!(cleared2.contains("fn foo"));
        assert!(!cleared2.contains(&long_content));

        // Last 2 tool results (index 8, 10) should be preserved.
        let preserved4 = extract_text(&compacted[8]);
        assert!(preserved4.contains("result 4 protected"));

        let preserved5 = extract_text(&compacted[10]);
        assert!(preserved5.contains("result 5 protected"));
    }

    #[test]
    fn microcompact_fewer_than_protected() {
        // 2 tool results, protect 5 → nothing should be cleared.
        let msgs = vec![
            Message::user("do task"),
            Message::assistant_with_id("m1".into(), "calling tool"),
            Message::tool_result("t1", "result 1"),
            Message::assistant("done"),
        ];
        let compacted = microcompact(&msgs, 5);
        // Should be identical (no clearing).
        let text = extract_text(&compacted[2]);
        assert!(text.contains("result 1"));
        assert!(!text.contains("cleared"));
    }

    #[test]
    fn microcompact_no_tool_results() {
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("how are you"),
        ];
        let compacted = microcompact(&msgs, 3);
        assert_eq!(compacted.len(), msgs.len());
    }

    #[test]
    fn microcompact_preserves_non_tool_content() {
        // User message with both Text and ToolResult content.
        let long_content = "fn alpha() {}\nfn beta() {}\n".repeat(20);
        let msgs = vec![
            Message::user("start"),
            Message::assistant_with_id("m1".into(), "call tool"),
            Message::tool_result("t1", &long_content),
            Message::assistant_with_id("m2".into(), "call tool 2"),
            Message::tool_result("t2", &long_content),
            Message::assistant_with_id("m3".into(), "call tool 3"),
            Message::tool_result("t3", "protected result 3"),
        ];
        let compacted = microcompact(&msgs, 1);
        // First two tool results summarized, last one preserved.
        let r1 = extract_text(&compacted[2]);
        assert!(r1.contains("fn alpha"));
        assert!(!r1.contains(&long_content));
        let r2 = extract_text(&compacted[4]);
        assert!(r2.contains("fn beta"));
        assert!(!r2.contains(&long_content));
        assert!(extract_text(&compacted[6]).contains("protected result 3"));
        // Assistant messages preserved.
        assert!(extract_text(&compacted[1]).contains("call tool"));
        assert!(extract_text(&compacted[3]).contains("call tool 2"));
    }

    // ── truncate_at_char_boundary ──

    #[test]
    fn truncate_at_char_boundary_short() {
        assert_eq!(truncate_at_char_boundary("hello", 10), "hello");
    }

    #[test]
    fn truncate_at_char_boundary_cjk() {
        let s = "你好世界你好世界你好世界"; // 12 CJK chars
        let truncated = truncate_at_char_boundary(s, 5);
        assert!(truncated.contains("截断"));
        // Should contain first 5 CJK chars
        assert!(truncated.contains("你好世界你"));
    }

    // ── truncate_lines ──

    #[test]
    fn truncate_lines_basic() {
        let s: String = (0..1000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_lines(&s, 10);
        assert!(truncated.contains("截断"));
        // Should contain first 10 lines
        assert!(truncated.contains("line 0"));
        assert!(truncated.contains("line 9"));
        assert!(!truncated.contains("line 10\n")); // Not in the kept part
    }

    #[test]
    fn truncate_lines_short() {
        let s = "line 1\nline 2\nline 3";
        assert_eq!(truncate_lines(s, 10), s);
    }

    // ── ContextConfig defaults ──

    #[test]
    fn context_config_defaults() {
        let cfg = ContextConfig::default();
        assert_eq!(cfg.max_output_tokens, 0);
        assert!((cfg.compaction_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.keep_recent_turns, 2);
        assert_eq!(cfg.max_bash_output_chars, 20000);
        assert_eq!(cfg.max_read_lines, 500);
        assert_eq!(cfg.microcompact_threshold, 20000);
        assert_eq!(cfg.microcompact_protected_results, 3);
    }

    #[test]
    fn context_config_from_toml() {
        let toml_str = r#"
max_output_tokens = 8192
compaction_threshold = 0.8
keep_recent_turns = 4
max_bash_output_chars = 30000
max_read_lines = 300
microcompact_threshold = 15000
microcompact_protected_results = 5
"#;
        let cfg: ContextConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.max_output_tokens, 8192);
        assert!((cfg.compaction_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.keep_recent_turns, 4);
        assert_eq!(cfg.max_bash_output_chars, 30000);
        assert_eq!(cfg.max_read_lines, 300);
        assert_eq!(cfg.microcompact_threshold, 15000);
        assert_eq!(cfg.microcompact_protected_results, 5);
    }

    #[test]
    fn context_config_defaults_when_missing() {
        let toml_str = "";
        let cfg: ContextConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.max_output_tokens, 0);
        assert_eq!(cfg.keep_recent_turns, 2);
    }
}
