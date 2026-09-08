// 无头模式（非 TUI）：通过 `-p <prompt>` 端到端运行任务，结果直接输出到 stdout。
// Headless (non-TUI) mode: run a task end-to-end via `-p <prompt>`, printing
// the result to stdout without ever entering the TUI. The prerequisite for
// CI, scripting, and pre-commit integration.
//
// 输出契约：
//   text 模式 —— 助手文本增量实时流式打印到 stdout；工具/阶段/错误走 stderr。
//   json  模式 —— stdout 只输出一个最终 JSON 对象，增量被忽略（缓冲进最终结果）。
//   stdout 始终保持纯净（可管道）；所有工具噪声走 stderr。
// Output contract:
//   text mode — assistant text deltas stream to stdout immediately; tools/phases/errors
//               go to stderr.
//   json  mode — stdout receives exactly ONE final JSON object; deltas are ignored
//               (buffered into the final result).
//   stdout stays clean for piping; all tool noise goes to stderr.

use std::io::Write;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::cli::context::AppContext;
use crate::event::{AgentEvent, HitlDecision};

// ── OutputFormat ──────────────────────────────────────────────────────────

/// 无头模式的输出格式。
/// Output format for headless mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// 纯文本流式输出到 stdout。
    /// Plain text streamed to stdout.
    Text,
    /// 单个 JSON 对象输出到 stdout。
    /// A single JSON object printed to stdout.
    Json,
}

impl OutputFormat {
    /// 从字符串解析输出格式；未知值返回错误。
    /// Parse an output format from a string; unknown values return an error.
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unknown output format: {other} (expected: text|json)"
            )),
        }
    }
}

// ── HeadlessArgs ───────────────────────────────────────────────────────────

/// 解析后的无头模式 CLI 参数。
/// Parsed headless-mode CLI arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessArgs {
    pub prompt: String,
    pub format: OutputFormat,
    pub auto_yes: bool,
}

/// 解析无头模式 CLI 参数。
/// 返回 `Ok(None)` 表示 `-p` 未出现（走 TUI 路径）；`Ok(Some(args))` 表示解析成功；
/// `Err(msg)` 表示解析错误（应退出码 2）。
///
/// Parse headless CLI args.
/// Returns `Ok(None)` if `-p` is absent (TUI path); `Ok(Some(args))` on success;
/// `Err(msg)` on parse error (caller should exit 2).
pub fn parse_headless_args(args: &[String]) -> Result<Option<HeadlessArgs>, String> {
    let has_p = args.iter().any(|a| a == "-p" || a == "--print");
    if !has_p {
        return Ok(None);
    }
    let mut prompt = None;
    let mut format = OutputFormat::Text;
    let mut auto_yes = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--print" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing prompt value for -p/--print".to_string());
                }
                prompt = Some(args[i].clone());
            }
            "--output-format" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --output-format".to_string());
                }
                format = OutputFormat::from_str(&args[i])?;
            }
            "--yes" | "-y" => {
                auto_yes = true;
            }
            _ => {}
        }
        i += 1;
    }
    match prompt {
        Some(p) => Ok(Some(HeadlessArgs { prompt: p, format, auto_yes })),
        None => Err("prompt value missing for -p/--print".to_string()),
    }
}

// ── Sink routing ──────────────────────────────────────────────────────────

/// 事件路由决策：将一个 `AgentEvent` 映射为具体的 I/O 动作（纯函数，无副作用）。
/// Event routing decision: maps an `AgentEvent` to a concrete I/O action
/// (pure function, no side effects).
///
/// 这是无头消费者的核心——异步循环只是它的薄包装。`Agent` 文本的"记忆"和
/// `Error` 标志的追踪由异步循环在 Sink 之外处理（属于状态跟踪，非 I/O 动作）。
/// This is the heart of the headless consumer — the async loop is a thin wrapper.
/// "Remembering" the `Agent` text and tracking the `Error` flag are done by the
/// async loop outside of Sink (state tracking, not I/O actions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// 打印到 stdout（text 模式下的流式增量）。
    /// Print to stdout (streaming deltas in text mode).
    Stdout(String),
    /// 打印到 stderr（工具调用/结果、阶段、错误）。
    /// Print to stderr (tool calls/results, phases, errors).
    Stderr(String),
    /// 响应 HITL 提示（发送 Allow=批准 / Deny=拒绝）。
    /// Respond to a HITL prompt (Allow=approve / Deny=deny).
    RespondHitl(HitlDecision),
    /// 任务完成，停止消费。
    /// Task finished, stop consuming.
    Finish,
    /// 忽略该事件。
    /// Ignore this event.
    Ignore,
}

/// 将一个 `AgentEvent` 路由为 `Sink` 决策（纯函数）。
/// Route an `AgentEvent` to a `Sink` decision (pure function).
pub fn route_event(event: &AgentEvent, format: OutputFormat, auto_yes: bool) -> Sink {
    match event {
        AgentEvent::TextDelta(text) => {
            if format == OutputFormat::Text {
                Sink::Stdout(text.clone())
            } else {
                // json 模式：增量不打印，缓冲进最终结果。
                // json mode: deltas not printed, buffered into the final result.
                Sink::Ignore
            }
        }

        // Agent 的最终输出文本——异步循环负责"记忆"为 latest final output。
        // Sink 层面不做 I/O 动作（text 模式下增量已流式打印，重复打印会重复）。
        // Agent's final output text — the async loop "remembers" it as the latest
        // final output. No I/O action at the Sink level (in text mode deltas already
        // streamed; printing again would duplicate).
        AgentEvent::Agent(_) => Sink::Ignore,

        AgentEvent::ToolCall { name, .. } => {
            if format == OutputFormat::Text {
                Sink::Stderr(format!("[tool] {name}"))
            } else {
                Sink::Ignore
            }
        }
        AgentEvent::ToolResult { name, ok, .. } => {
            if format == OutputFormat::Text {
                let tag = if *ok { "[ok]" } else { "[fail]" };
                Sink::Stderr(format!("{tag} {name}"))
            } else {
                Sink::Ignore
            }
        }

        // HITL 提示——管道在 responder 响应前阻塞。--yes 映射为 Allow（非 Always：
        // 无头模式不持久化授权目录），否则 Deny。
        // HITL prompt — the pipeline blocks until the responder fires. --yes maps
        // to Allow (not Always: headless mode does not persist authorized dirs);
        // otherwise Deny.
        AgentEvent::HitlPrompt { .. } => {
            if auto_yes {
                Sink::RespondHitl(HitlDecision::Allow)
            } else {
                Sink::RespondHitl(HitlDecision::Deny)
            }
        }

        AgentEvent::Error(text) => {
            if format == OutputFormat::Text {
                Sink::Stderr(format!("[error] {text}"))
            } else {
                // json 模式：错误信息进入最终 JSON 对象，不单独打印。
                // json mode: error text goes into the final JSON object, not printed separately.
                Sink::Ignore
            }
        }

        AgentEvent::AgentFinished => Sink::Finish,

        AgentEvent::PhaseStart { role } => {
            if format == OutputFormat::Text {
                Sink::Stderr(format!("[phase] {role}"))
            } else {
                Sink::Ignore
            }
        }

        // 所有其他变体（User / System / ReasoningDelta / Reasoning / AgentStarted /
        // TurnFinished / Info / ContextCompacted / SuspendTui 等）——忽略。
        // SuspendTui 的 responder 在事件被丢弃时自动关闭，resp_rx.await 会
        // 返回 Err 并 unwrap_or_default() 为空串，不会死锁。
        // All other variants (User / System / ReasoningDelta / Reasoning /
        // AgentStarted / TurnFinished / Info / ContextCompacted / SuspendTui etc.)
        // — ignore. SuspendTui's responder auto-closes when the event is dropped;
        // resp_rx.await returns Err and unwrap_or_default() yields "", no deadlock.
        _ => Sink::Ignore,
    }
}

// ── format_final ───────────────────────────────────────────────────────────

/// 格式化最终输出字符串。
/// Format the final output string.
///
/// - text 模式：原样返回结果文本。
/// - json  模式：返回单个 JSON 对象（成功 `{"result":..., "ok":true}`，
///   失败 `{"error":..., "ok":false}`）。
///
/// - text mode: returns the result text as-is.
/// - json  mode: returns a single JSON object (success `{"result":..., "ok":true}`,
///   error `{"error":..., "ok":false}`).
pub fn format_final(result: &str, ok: bool, format: &OutputFormat) -> String {
    match format {
        OutputFormat::Text => result.to_string(),
        OutputFormat::Json => {
            if ok {
                serde_json::json!({ "result": result, "ok": true }).to_string()
            } else {
                serde_json::json!({ "error": result, "ok": false }).to_string()
            }
        }
    }
}

// ── run_headless ───────────────────────────────────────────────────────────

/// 运行无头模式：构建事件 channel，派发 Orchestrator 处理，消费事件并输出。
/// 返回退出码（0=成功 / 1=失败）。
///
/// Run headless mode: create an event channel, dispatch the Orchestrator, consume
/// events, and print output. Returns the exit code (0=success / 1=failure).
///
/// 注意：headless 路径不初始化 TUI（无 TerminalGuard / crossterm）。
/// 日志初始化复用 main.rs 的文件日志，此处不变。
/// Note: the headless path does NOT init the TUI (no TerminalGuard / crossterm).
/// Tracing init reuses main.rs's file-log init unchanged.
///
/// 未来工作（v1 跳过）：lesson 记录——run_goal_tui 在 handle 成功后会记录会话轮次
/// 并提取经验教训。headless v1 暂不实现，待后续迭代补充。
/// Future work (skipped in v1): lesson recording — run_goal_tui records session turns
/// and extracts lessons after handle succeeds. Headless v1 omits this; defer to a
/// later iteration.
pub async fn run_headless(
    ctx: Arc<AppContext>,
    prompt: &str,
    format: OutputFormat,
    auto_yes: bool,
) -> anyhow::Result<i32> {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();

    // 派发 Orchestrator 到独立任务——与消费者循环并发运行。
    // Dispatch the Orchestrator to a separate task — runs concurrently with the
    // consumer loop. tx is moved into the task; when handle returns (or errors),
    // tx is dropped and rx.recv() yields None, exiting the loop.
    let ctx_clone = Arc::clone(&ctx);
    let prompt_owned = prompt.to_string();
    let handle_task = tokio::spawn(async move {
        ctx_clone.orchestrator.handle(&prompt_owned, &tx).await
    });

    let mut final_text = String::new();
    let mut failed = false;
    let mut streamed_any = false;

    while let Some(event) = rx.recv().await {
        // 状态跟踪（在 Sink 之前，因为 RespondHitl 可能移动 event）。
        // State tracking (before Sink, since RespondHitl may move the event).
        match &event {
            AgentEvent::Agent(text) => final_text = text.clone(),
            AgentEvent::Error(_) => failed = true,
            _ => {}
        }

        // 路由决策 + 执行。
        // Route decision + execute.
        match route_event(&event, format, auto_yes) {
            Sink::Stdout(s) => {
                print!("{s}");
                std::io::stdout().flush()?;
                streamed_any = true;
            }
            Sink::Stderr(s) => {
                eprintln!("{s}");
            }
            Sink::RespondHitl(decision) => {
                // Every HitlPrompt MUST receive exactly one response or the pipeline deadlocks.
                if let AgentEvent::HitlPrompt { tool, responder, .. } = event {
                    if decision == HitlDecision::Deny {
                        eprintln!("[hitl] auto-denied: {tool} (use --yes to approve)");
                    }
                    let _ = responder.send(decision);
                }
            }
            Sink::Finish => break,
            Sink::Ignore => {}
        }
    }

    // 等待 Orchestrator 返回最终结果。
    // Await the Orchestrator's final result.
    let (text, ok) = match handle_task.await {
        Ok(Ok(out)) => {
            // handle 的返回值是权威的最终输出。
            // handle's return value is the authoritative final output.
            if !out.is_empty() {
                final_text = out;
            }
            (final_text, !failed)
        }
        Ok(Err(e)) => (format!("orchestrator error: {e}"), false),
        Err(e) => (format!("orchestrator task panicked: {e}"), false),
    };

    // 最终输出。
    // Final output.
    match (format, ok) {
        (OutputFormat::Json, _) => {
            println!("{}", format_final(&text, ok, &format));
        }
        (OutputFormat::Text, true) => {
            // 增量已流式打印到 stdout；若无增量但有最终文本，补打一次。
            // Deltas already streamed to stdout; if no deltas were streamed but we
            // have final text, print it once.
            if !streamed_any && !text.is_empty() {
                println!("{}", text);
            }
        }
        (OutputFormat::Text, false) => {
            // 错误走 stderr（增量可能已部分打印到 stdout）。
            // Errors go to stderr (deltas may have partially printed to stdout).
            eprintln!("{}", text);
        }
    }

    Ok(if ok { 0 } else { 1 })
}

// ═══════════════════════════════════════════════════════════════════════════
// 测试 / Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FileEdit;
    use tokio::sync::oneshot;

    // ── OutputFormat::from_str ──────────────────────────────────────────────

    #[test]
    fn output_format_parse_text() {
        // Given: "text".
        // When: from_str.
        // Then: Ok(Text).
        assert_eq!(OutputFormat::from_str("text"), Ok(OutputFormat::Text));
    }

    #[test]
    fn output_format_parse_json() {
        // Given: "json".
        // When: from_str.
        // Then: Ok(Json).
        assert_eq!(OutputFormat::from_str("json"), Ok(OutputFormat::Json));
    }

    #[test]
    fn output_format_parse_unknown_returns_error() {
        // Given: an unknown format string.
        // When: from_str.
        // Then: Err with a descriptive message.
        let result = OutputFormat::from_str("yaml");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("yaml"), "error must mention the unknown value: {msg}");
        assert!(msg.contains("text") || msg.contains("json"), "error must hint valid formats: {msg}");
    }

    #[test]
    fn output_format_parse_empty_string_returns_error() {
        // Given: empty string.
        // When: from_str.
        // Then: Err.
        assert!(OutputFormat::from_str("").is_err());
    }

    // ── format_final ────────────────────────────────────────────────────────

    #[test]
    fn format_final_text_ok_returns_as_is() {
        // Given: a result string and ok=true, text format.
        // When: format_final.
        // Then: returns the string unchanged.
        assert_eq!(format_final("done", true, &OutputFormat::Text), "done");
    }

    #[test]
    fn format_final_text_error_returns_as_is() {
        // Given: an error string and ok=false, text format.
        // When: format_final.
        // Then: returns the string unchanged (caller decides stdout vs stderr).
        assert_eq!(format_final("oops", false, &OutputFormat::Text), "oops");
    }

    #[test]
    fn format_final_json_ok_produces_result_json() {
        // Given: a result string and ok=true, json format.
        // When: format_final.
        // Then: a valid JSON object {"result":"...","ok":true}.
        let out = format_final("all good", true, &OutputFormat::Json);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("must be valid JSON");
        assert_eq!(parsed["result"], serde_json::Value::String("all good".to_string()));
        assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
        assert!(parsed.get("error").is_none(), "ok=true must not have an error field");
    }

    #[test]
    fn format_final_json_error_produces_error_json() {
        // Given: an error string and ok=false, json format.
        // When: format_final.
        // Then: a valid JSON object {"error":"...","ok":false}.
        let out = format_final("something failed", false, &OutputFormat::Json);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("must be valid JSON");
        assert_eq!(parsed["error"], serde_json::Value::String("something failed".to_string()));
        assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
        assert!(parsed.get("result").is_none(), "ok=false must not have a result field");
    }

    #[test]
    fn format_final_json_empty_result() {
        // Given: an empty result string, ok=true, json format.
        // When: format_final.
        // Then: {"result":"","ok":true}.
        let out = format_final("", true, &OutputFormat::Json);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("must be valid JSON");
        assert_eq!(parsed["result"], serde_json::Value::String("".to_string()));
        assert_eq!(parsed["ok"], serde_json::Value::Bool(true));
    }

    // ── route_event: TextDelta ──────────────────────────────────────────────

    #[test]
    fn route_text_delta_text_mode_streams_to_stdout() {
        // Given: a TextDelta event and text format.
        // When: route_event.
        // Then: Sink::Stdout with the delta text.
        let event = AgentEvent::TextDelta("hello".to_string());
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stdout("hello".to_string())
        );
    }

    #[test]
    fn route_text_delta_json_mode_ignored() {
        // Given: a TextDelta event and json format.
        // When: route_event.
        // Then: Sink::Ignore (deltas buffered into final result, not printed).
        let event = AgentEvent::TextDelta("hello".to_string());
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── route_event: Agent ──────────────────────────────────────────────────

    #[test]
    fn route_agent_text_ignored_by_sink() {
        // Given: an Agent event with final text.
        // When: route_event.
        // Then: Sink::Ignore (the async loop tracks the text separately as "latest
        //   final output"; printing it again would duplicate the streamed deltas).
        let event = AgentEvent::Agent("final answer".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── route_event: ToolCall ────────────────────────────────────────────────

    #[test]
    fn route_tool_call_text_mode_stderr() {
        // Given: a ToolCall event, text format.
        // When: route_event.
        // Then: Sink::Stderr("[tool] {name}").
        let event = AgentEvent::ToolCall {
            name: "read_file".to_string(),
            desc: "reading src/main.rs".to_string(),
            diff: None,
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[tool] read_file".to_string())
        );
    }

    #[test]
    fn route_tool_call_json_mode_ignored() {
        // Given: a ToolCall event, json format.
        // When: route_event.
        // Then: Sink::Ignore (tool noise stays off stdout in json mode).
        let event = AgentEvent::ToolCall {
            name: "edit_file".to_string(),
            desc: "editing file".to_string(),
            diff: None,
        };
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    #[test]
    fn route_tool_call_with_diff_uses_name_only() {
        // Given: a ToolCall with a diff payload (edit_file).
        // When: route_event in text mode.
        // Then: Sink::Stderr("[tool] {name}") — the diff is TUI-specific, headless
        //   only shows the tool name.
        let event = AgentEvent::ToolCall {
            name: "edit_file".to_string(),
            desc: "editing main.rs".to_string(),
            diff: Some(Box::new(FileEdit {
                path: "src/main.rs".to_string(),
                old: "old".to_string(),
                new: "new".to_string(),
            })),
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[tool] edit_file".to_string())
        );
    }

    // ── route_event: ToolResult ─────────────────────────────────────────────

    #[test]
    fn route_tool_result_ok_text_mode_stderr() {
        // Given: a successful ToolResult, text format.
        // When: route_event.
        // Then: Sink::Stderr("[ok] {name}").
        let event = AgentEvent::ToolResult {
            name: "read_file".to_string(),
            result: "file contents".to_string(),
            ok: true,
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[ok] read_file".to_string())
        );
    }

    #[test]
    fn route_tool_result_fail_text_mode_stderr() {
        // Given: a failed ToolResult, text format.
        // When: route_event.
        // Then: Sink::Stderr("[fail] {name}").
        let event = AgentEvent::ToolResult {
            name: "run_bash".to_string(),
            result: "command not found".to_string(),
            ok: false,
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[fail] run_bash".to_string())
        );
    }

    #[test]
    fn route_tool_result_json_mode_ignored() {
        // Given: a ToolResult, json format.
        // When: route_event.
        // Then: Sink::Ignore.
        let event = AgentEvent::ToolResult {
            name: "run_bash".to_string(),
            result: "output".to_string(),
            ok: true,
        };
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── route_event: HitlPrompt ─────────────────────────────────────────────

    #[test]
    fn route_hitl_prompt_auto_yes_true_responds_allow() {
        // Given: a HitlPrompt event and auto_yes=true.
        // When: route_event.
        // Then: Sink::RespondHitl(Allow) — the async loop sends the approval.
        let (tx, _rx) = oneshot::channel();
        let event = AgentEvent::HitlPrompt {
            tool: "run_bash".to_string(),
            desc: "rm -rf /tmp".to_string(),
            responder: tx,
            allow_always: false,
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, true),
            Sink::RespondHitl(HitlDecision::Allow)
        );
        assert_eq!(
            route_event(&event, OutputFormat::Json, true),
            Sink::RespondHitl(HitlDecision::Allow)
        );
    }

    #[test]
    fn route_hitl_prompt_auto_yes_false_responds_deny() {
        // Given: a HitlPrompt event and auto_yes=false.
        // When: route_event.
        // Then: Sink::RespondHitl(Deny) — the async loop sends the denial and
        //   prints a stderr note. The routing decision is the same for text and json
        //   (HITL must always be answered regardless of output format).
        let (tx, _rx) = oneshot::channel();
        let event = AgentEvent::HitlPrompt {
            tool: "edit_file".to_string(),
            desc: "modifying src/main.rs".to_string(),
            responder: tx,
            allow_always: false,
        };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::RespondHitl(HitlDecision::Deny)
        );
        assert_eq!(
            route_event(&event, OutputFormat::Json, false),
            Sink::RespondHitl(HitlDecision::Deny)
        );
    }

    // ── route_event: Error ───────────────────────────────────────────────────

    #[test]
    fn route_error_text_mode_stderr() {
        // Given: an Error event, text format.
        // When: route_event.
        // Then: Sink::Stderr("[error] {text}").
        let event = AgentEvent::Error("something broke".to_string());
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[error] something broke".to_string())
        );
    }

    #[test]
    fn route_error_json_mode_ignored() {
        // Given: an Error event, json format.
        // When: route_event.
        // Then: Sink::Ignore (error text goes into the final JSON object).
        let event = AgentEvent::Error("something broke".to_string());
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── route_event: AgentFinished ──────────────────────────────────────────

    #[test]
    fn route_agent_finished_finishes() {
        // Given: an AgentFinished event.
        // When: route_event.
        // Then: Sink::Finish (break the consumer loop).
        let event = AgentEvent::AgentFinished;
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Finish);
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Finish);
    }

    // ── route_event: PhaseStart ─────────────────────────────────────────────

    #[test]
    fn route_phase_start_text_mode_stderr() {
        // Given: a PhaseStart event, text format.
        // When: route_event.
        // Then: Sink::Stderr("[phase] {role}").
        let event = AgentEvent::PhaseStart { role: "investigator".to_string() };
        assert_eq!(
            route_event(&event, OutputFormat::Text, false),
            Sink::Stderr("[phase] investigator".to_string())
        );
    }

    #[test]
    fn route_phase_start_json_mode_ignored() {
        // Given: a PhaseStart event, json format.
        // When: route_event.
        // Then: Sink::Ignore.
        let event = AgentEvent::PhaseStart { role: "builder".to_string() };
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── route_event: all other variants → Ignore ────────────────────────────

    #[test]
    fn route_user_event_ignored() {
        let event = AgentEvent::User("hello".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_system_event_ignored() {
        let event = AgentEvent::System("banner".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_reasoning_delta_ignored() {
        let event = AgentEvent::ReasoningDelta("thinking...".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    #[test]
    fn route_reasoning_event_ignored() {
        let event = AgentEvent::Reasoning("full reasoning trace".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_agent_started_ignored() {
        let event = AgentEvent::AgentStarted;
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_turn_finished_ignored() {
        let event = AgentEvent::TurnFinished { turn: 3, usage: "1.2k tok".to_string() };
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_info_ignored() {
        let event = AgentEvent::Info(" FYI".to_string());
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_context_compacted_ignored() {
        let event = AgentEvent::ContextCompacted { old_tokens: 50000, new_tokens: 12000 };
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
    }

    #[test]
    fn route_suspend_tui_ignored() {
        // SuspendTui is TUI-specific — in headless mode it is ignored. The
        // responder is dropped when the event is dropped, causing resp_rx.await to
        // return Err and unwrap_or_default() to yield "". No deadlock.
        let (tx, _rx) = oneshot::channel();
        let event = AgentEvent::SuspendTui {
            command: "sudo apt install".to_string(),
            responder: tx,
        };
        assert_eq!(route_event(&event, OutputFormat::Text, false), Sink::Ignore);
        assert_eq!(route_event(&event, OutputFormat::Json, false), Sink::Ignore);
    }

    // ── parse_headless_args ─────────────────────────────────────────────────

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_no_p_flag_returns_none() {
        // Given: args without -p or --print.
        // When: parse_headless_args.
        // Then: Ok(None) — TUI path.
        assert_eq!(parse_headless_args(&args(&["moye"])), Ok(None));
        assert_eq!(parse_headless_args(&args(&["moye", "--continue"])), Ok(None));
    }

    #[test]
    fn parse_p_with_value_ok() {
        // Given: -p "fix the bug".
        // When: parse_headless_args.
        // Then: Ok(Some(HeadlessArgs { prompt: "fix the bug", format: Text, auto_yes: false })).
        let result = parse_headless_args(&args(&["moye", "-p", "fix the bug"])).unwrap();
        let h = result.expect("should be Some");
        assert_eq!(h.prompt, "fix the bug");
        assert_eq!(h.format, OutputFormat::Text);
        assert!(!h.auto_yes);
    }

    #[test]
    fn parse_print_long_form_ok() {
        // Given: --print "do something".
        // When: parse_headless_args.
        // Then: Ok(Some(...)).
        let result = parse_headless_args(&args(&["moye", "--print", "do something"])).unwrap();
        assert_eq!(result.unwrap().prompt, "do something");
    }

    #[test]
    fn parse_p_missing_value_err() {
        // Given: -p with no following value (end of args).
        // When: parse_headless_args.
        // Then: Err (caller should exit 2).
        let result = parse_headless_args(&args(&["moye", "-p"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing prompt"));
    }

    #[test]
    fn parse_print_missing_value_err() {
        // Given: --print at end of args with no value.
        // When: parse_headless_args.
        // Then: Err.
        let result = parse_headless_args(&args(&["moye", "--print"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_output_format_text() {
        // Given: -p "task" --output-format text.
        // When: parse_headless_args.
        // Then: format = Text.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "--output-format", "text"])).unwrap();
        assert_eq!(result.unwrap().format, OutputFormat::Text);
    }

    #[test]
    fn parse_output_format_json() {
        // Given: -p "task" --output-format json.
        // When: parse_headless_args.
        // Then: format = Json.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "--output-format", "json"])).unwrap();
        assert_eq!(result.unwrap().format, OutputFormat::Json);
    }

    #[test]
    fn parse_output_format_unknown_err() {
        // Given: -p "task" --output-format yaml.
        // When: parse_headless_args.
        // Then: Err.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "--output-format", "yaml"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_output_format_missing_value_err() {
        // Given: --output-format at end of args with no value.
        // When: parse_headless_args.
        // Then: Err.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "--output-format"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_yes_short_flag() {
        // Given: -p "task" -y.
        // When: parse_headless_args.
        // Then: auto_yes = true.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "-y"])).unwrap();
        assert!(result.unwrap().auto_yes);
    }

    #[test]
    fn parse_yes_long_flag() {
        // Given: -p "task" --yes.
        // When: parse_headless_args.
        // Then: auto_yes = true.
        let result = parse_headless_args(&args(&["moye", "-p", "task", "--yes"])).unwrap();
        assert!(result.unwrap().auto_yes);
    }

    #[test]
    fn parse_combined_all_flags() {
        // Given: -p "task" --output-format json --yes.
        // When: parse_headless_args.
        // Then: prompt="task", format=Json, auto_yes=true.
        let result = parse_headless_args(&args(&[
            "moye", "-p", "task", "--output-format", "json", "--yes",
        ])).unwrap();
        let h = result.unwrap();
        assert_eq!(h.prompt, "task");
        assert_eq!(h.format, OutputFormat::Json);
        assert!(h.auto_yes);
    }

    #[test]
    fn parse_combined_flags_reordered() {
        // Given: flags in a different order: --yes -p "task" --output-format json.
        // When: parse_headless_args.
        // Then: all parsed correctly.
        let result = parse_headless_args(&args(&[
            "moye", "--yes", "-p", "task", "--output-format", "json",
        ])).unwrap();
        let h = result.unwrap();
        assert_eq!(h.prompt, "task");
        assert_eq!(h.format, OutputFormat::Json);
        assert!(h.auto_yes);
    }

    #[test]
    fn parse_continue_with_p_allowed() {
        // --continue + -p: 允许组合（--continue 恢复会话上下文，-p 无头执行）。
        // --continue + -p: combination allowed (--continue resumes session context,
        // -p runs headless with that context).
        let result = parse_headless_args(&args(&[
            "moye", "--continue", "-p", "fix the bug",
        ])).unwrap();
        let h = result.expect("should be Some");
        assert_eq!(h.prompt, "fix the bug");
    }
}
