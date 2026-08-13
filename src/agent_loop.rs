// 自主循环模块：用 rig 的 AgentRunner 驱动一个自我驱动的 Agent 循环（上限 max_turns），
// Autonomous loop module: drives a self-driven Agent loop via rig's AgentRunner (capped at max_turns),
// 并通过 HitlHook（rig AgentHook）在每次工具调用时按权限分级做 HITL（人在环）门控。
// and gates each tool call by permission tier for HITL (Human-in-the-Loop) via HitlHook (rig AgentHook).
//
// 在 TUI 模式下，所有用户可见输出通过 `AgentEvent` channel 发送给 TUI 事件循环，
// In TUI mode, all user-visible output is sent to the TUI event loop via the `AgentEvent` channel,
// 而不是直接 print 到 stdout。内部日志仍走 tracing（仅文件）。
// instead of printing directly to stdout. Internal logs still go through tracing (file only).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rig_agent::agent::hook::CompletionCall;
use rig_agent::agent::{
    Agent, AgentHook, HookContext, InvalidToolCallAction, InvalidToolCallContext, ModelTurnAction,
    ModelTurnFinished, MultiTurnStreamItem, RequestPatch, StepEventKind, StreamingResult,
    ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent, CompletionCallAction,
};
use rig_agent::client::AgentClientExt;
use rig_core::completion::{Message, Usage};
use rig_core::providers::openai::CompletionModel as OpenAiModel;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::event::{AgentEvent, EventSender};
use crate::registry::{AgentRegistry, Permission, Role, ToolPerms};
use crate::sandbox::Sandbox;
use crate::tools::is_readonly_bash;

/// HITL（人在环）门控。实现为 rig 的 `AgentHook`，拦截每一次 `ToolCall` 并按角色的
/// HITL (Human-in-the-Loop) gate. Implemented as rig's `AgentHook`, intercepts every `ToolCall` by role,
/// 按工具权限分级处理：
/// handles by tool permission tier:
/// - `Allow` -> 静默执行（不询问）。像 `ls` 这样的琐碎步骤直接通过。
/// - `Allow` -> execute silently (no prompt). Trivial steps like `ls` pass through directly.
/// - `Ask`   -> 通过 channel 向 TUI 发送 `HitlPrompt`，等待用户按键确认。
/// - `Ask`   -> sends a `HitlPrompt` to the TUI via channel, waits for user keypress confirmation.
/// - `Deny`  -> 跳过调用并向模型说明原因。
/// - `Deny`  -> skips the call and explains the reason to the model.
///
/// 仅对 `ToolCall` 事件做门控；模型的回合、结果、增量事件原样通过。
/// Only gates `ToolCall` events; model turn, result, and delta events pass through unchanged.
/// 权限分级在循环启动时即已捕获。
/// Permission tiers are captured at loop startup.
#[derive(Clone)]
pub struct HitlHook {
    perms: Arc<Mutex<ToolPerms>>,
    waiting: Arc<AtomicBool>,
    tx: EventSender,
    sandbox: Sandbox,
    /// 信任模式：为 true 时沙箱外访问自动授权，不弹窗确认。
    /// Trust mode: when true, out-of-sandbox access is auto-authorized without prompting.
    trust_sandbox: Arc<AtomicBool>,
}

impl HitlHook {
    pub fn new(
        perms: ToolPerms,
        waiting: Arc<AtomicBool>,
        tx: EventSender,
        sandbox: Sandbox,
        trust_sandbox: Arc<AtomicBool>,
    ) -> Self {
        Self {
            perms: Arc::new(Mutex::new(perms)),
            waiting,
            tx,
            sandbox,
            trust_sandbox,
        }
    }

    /// 向 TUI 发送 HITL 确认请求，等待用户按键。
    /// Sends a HITL confirmation request to the TUI, waits for user keypress.
    async fn confirm(&self, tool_name: &str, desc: &str) -> bool {
        // Use a guard so `waiting` is always reset — even if this future is
        // dropped (cancelled) while awaiting the user's response.  Without
        // this, a cancelled `confirm()` would leave `waiting = true` forever,
        // causing `consume_stream` to skip the timeout on every subsequent
        // iteration.
        // 使用 guard 确保 `waiting` 总是被重置——即使此 future 在等待用户
        // 响应时被丢弃（取消）。否则，被取消的 `confirm()` 会永远留下
        // `waiting = true`，导致 `consume_stream` 在后续每次迭代中跳过超时。
        let _guard = WaitingGuard::new(self.waiting.clone());
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self.tx.send(AgentEvent::HitlPrompt {
            tool: tool_name.to_string(),
            desc: desc.to_string(),
            responder: resp_tx,
        });
        resp_rx.await.unwrap_or(false)
    }

    async fn maybe_run_interactive(&self, tool_name: &str, args: &str) -> Option<ToolCallAction> {
        if tool_name != "run_bash" {
            return None;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(args).ok()?;
        let cmd = parsed.get("command")?.as_str()?;
        if !needs_interactive_terminal(cmd) {
            return None;
        }
        let _ = self.tx.send(AgentEvent::Info(
            format!("  [\u{1f501}] \u{4ea4}\u{4e92}\u{5f0f}\u{547d}\u{4ee4}\u{ff0c}\u{6682}\u{505c} TUI: {cmd}"),
        ));
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self.tx.send(AgentEvent::SuspendTui {
            command: cmd.to_string(),
            responder: resp_tx,
        });
        let output = resp_rx.await.unwrap_or_default();
        Some(ToolCallAction::Skip(output))
    }
}

/// RAII guard that sets `waiting` to `true` on creation and `false` on drop.
/// RAII 守卫：创建时设置 `waiting` 为 `true`，销毁时重置为 `false`。
struct WaitingGuard {
    flag: Arc<AtomicBool>,
}

impl WaitingGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Relaxed);
        Self { flag }
    }
}

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

impl AgentHook for HitlHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let tool_name = event.tool_name;
        let args = event.args;
        // ── 沙箱检查 ──
        // 在权限分级检查之前，先检查工具调用是否访问沙箱外的路径。
        // If the path is outside the sandbox and not yet authorized, prompt the user.
        // Sandbox check: before the permission tier check, verify that the tool call
        // doesn't access paths outside the sandbox. If it does and the directory
        // hasn't been authorized, prompt the user for authorization.
        //
        // 信任模式（trust_sandbox = true）下，沙箱外访问自动授权，不弹窗确认。
        // In trust mode (trust_sandbox = true), out-of-sandbox access is
        // auto-authorized without prompting the user.
        if let Some(sandbox_err) = self.sandbox.check_tool(tool_name, args) {
            if self.trust_sandbox.load(Ordering::Relaxed) {
                // 信任模式：自动授权，不弹窗
                // Trust mode: auto-authorize without prompting
                self.sandbox.authorize_tool(tool_name, args);
                let _ = self.tx.send(AgentEvent::Info(format!(
                    "  [\u{6c99}\u{7bb1}] \u{4fe1}\u{4efb}\u{6a21}\u{5f0f}\u{81ea}\u{52a8}\u{6388}\u{6743}: {sandbox_err}"
                )));
                // 授权后继续进入权限分级检查
                // After authorization, fall through to the permission tier check
            } else {
                let desc = format!(
                    "\u{1f512} \u{6c99}\u{7bb1}\u{5916}\u{8bbf}\u{95ee}\u{6388}\u{6743}\n\n{}\n\n\
                     \u{662f}\u{5426}\u{5141}\u{8bb8}\u{8bbf}\u{95ee}\u{6b64}\u{8def}\u{5f84}\u{ff1f}\n\
                     \u{ff08}\u{8f93}\u{5165} /trust \u{53ef}\u{5f00}\u{542f}\u{4fe1}\u{4efb}\u{6a21}\u{5f0f}\u{ff0c}\u{81ea}\u{52a8}\u{6388}\u{6743}\u{6c99}\u{7bb1}\u{5916}\u{8bbf}\u{95ee}\u{ff09}",
                    sandbox_err
                );
                if self.confirm(tool_name, &desc).await {
                    // 用户授权——将涉及的目录加入授权列表
                    // User authorized — add the involved directories to the authorized list
                    self.sandbox.authorize_tool(tool_name, args);
                    let _ = self.tx.send(AgentEvent::Info(format!(
                        "  [\u{6c99}\u{7bb1}] \u{5df2}\u{6388}\u{6743}\u{8bbf}\u{95ee}: {sandbox_err}"
                    )));
                    // 授权后继续进入权限分级检查
                    // After authorization, fall through to the permission tier check
                } else {
                    let _ = self.tx.send(AgentEvent::Info(
                        "  [\u{6c99}\u{7bb1}] \u{8bbf}\u{95ee}\u{88ab}\u{62d2}\u{7edd}".into(),
                    ));
                    return ToolCallAction::Skip(
                        format!("\u{6c99}\u{7bb1}\u{62d2}\u{7edd}\u{8bbf}\u{95ee}: {sandbox_err}"),
                    );
                }
            }
        }

        // ── 权限分级检查（原有逻辑）──
        // Permission tier check (original logic)
        let perms = self.perms.lock().unwrap().clone();
        let tier = decide_tier(&perms, tool_name, args);
        match tier {
            Permission::Allow => {
                let _ = self.tx.send(AgentEvent::Info(
                    "  [HITL] \u{81ea}\u{52a8}\u{5141}\u{8bb8}".into(),
                ));
                if let Some(action) = self.maybe_run_interactive(tool_name, args).await {
                    return action;
                }
                ToolCallAction::Run
            }
            Permission::Deny => {
                let _ = self.tx.send(AgentEvent::Info(
                    "  [HITL] \u{5df2}\u{62d2}\u{7edd}\u{ff08}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{ff09}".into(),
                ));
                ToolCallAction::Skip(
                    format!(
                        "\u{5de5}\u{5177} `{tool_name}` \u{88ab}\u{5f53}\u{524d}\u{89d2}\u{8272}\u{7684}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{7981}\u{6b62}"
                    ),
                )
            }
            Permission::Ask => {
                let desc = format_tool_call_desc(tool_name, args);
                if self.confirm(tool_name, &desc).await {
                    if let Some(action) = self.maybe_run_interactive(tool_name, args).await {
                        return action;
                    }
                    ToolCallAction::Run
                } else {
                    ToolCallAction::Skip(
                        format!(
                            "\u{7528}\u{6237}\u{62d2}\u{7edd}\u{4e86} `{tool_name}` \u{7684}\u{6267}\u{884c}"
                        ),
                    )
                }
            }
        }
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        let _ = self.tx.send(AgentEvent::ToolResult {
            name: event.tool_name.to_string(),
            result: event
                .presentation
                .as_text()
                .unwrap_or("")
                .to_string(),
            ok: true,
        });
        ToolResultAction::Keep
    }

    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        warn!("[\u{672a}\u{77e5}\u{5de5}\u{5177}] {}", event.tool_name);
        let _ = self.tx.send(AgentEvent::ToolResult {
            name: event.tool_name.clone(),
            result: "unknown tool".to_string(),
            ok: false,
        });
        Some(InvalidToolCallAction::Skip {
            reason: format!(
                "\u{5de5}\u{5177} `{}` \u{4e0d}\u{5b58}\u{5728}\u{3002}\u{53ef}\u{7528}\u{5de5}\u{5177}\u{ff1a}{}\u{3002}\u{8bf7}\u{7528}\u{6b63}\u{786e}\u{7684}\u{5de5}\u{5177}\u{540d}\u{91cd}\u{8bd5}\u{3002}",
                event.tool_name,
                event.available_tools.join(", "),
            ),
        })
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let usage_str = format_usage(&event.usage);
        let _ = self
            .tx
            .send(AgentEvent::TurnFinished {
                turn: event.turn,
                usage: usage_str,
            });
        ModelTurnAction::Continue
    }
}

/// 上下文管理 Hook：在每轮 API 调用前检测 token 溢出，溢出时压缩旧对话历史。
/// Context management Hook: detects token overflow before each API call,
/// compacting old conversation history when the threshold is exceeded.
///
/// 利用 rig 的 `CompletionCallAction::Patch` + `RequestPatch::history()`，
/// Uses rig's `CompletionCallAction::Patch` + `RequestPatch::history()`,
/// 替换当轮发送给 API 的历史（不影响 rig 内部持久化的真实历史）。
/// replacing the history sent to the API for this turn only (without modifying
/// rig's internally persisted real history).
///
/// 同时在 `ModelTurnFinished` 时记录 API 返回的实际 token 用量，
/// Also records actual token usage from the API on `ModelTurnFinished`,
/// 用于校准溢出检测。
/// calibrating overflow detection.
pub struct ContextHook {
    budget: Arc<Mutex<crate::context::TokenBudget>>,
    config: crate::context::ContextConfig,
    client: crate::providers::CompletionsClient,
    model: String,
    tx: EventSender,
    /// 缓存：(压缩时的历史长度, 压缩后的历史)。历史未变时复用。
    /// Cache: (history length at compaction, compacted history). Reused when history unchanged.
    compaction_cache: Arc<Mutex<Option<(usize, Vec<Message>)>>>,
    /// SSE 重试时用于捕获对话历史的共享 Arc（None = 不捕获）。
    /// Shared Arc for capturing conversation history on SSE retry (None = no capture).
    history_capture: Option<Arc<Mutex<Vec<Message>>>>,
    /// SSE 重试时用于捕获当前轮次的共享 Arc。
    /// Shared Arc for capturing the current turn number on SSE retry.
    turn_capture: Option<Arc<Mutex<usize>>>,
    /// 自主循环轮数上限，用于轮次预算提醒。
    /// Max turns for the autonomous loop, used for turn-budget awareness.
    max_turns: usize,
}

impl ContextHook {
    /// 创建上下文管理 Hook。
    /// Create a context management hook.
    pub fn new(
        context_limit: usize,
        config: crate::context::ContextConfig,
        client: crate::providers::CompletionsClient,
        model: String,
        tx: EventSender,
        max_turns: usize,
    ) -> Self {
        let budget = crate::context::TokenBudget::new(context_limit);
        Self {
            budget: Arc::new(Mutex::new(budget)),
            config,
            client,
            model,
            tx,
            compaction_cache: Arc::new(Mutex::new(None)),
            history_capture: None,
            turn_capture: None,
            max_turns,
        }
    }

    /// 启用 SSE 重试历史捕获：传入共享 Arc，每轮 CompletionCall 时写入最新历史。
    /// Enable SSE-retry history capture: pass shared Arcs, written on every CompletionCall.
    pub fn with_history_capture(
        mut self,
        history: Arc<Mutex<Vec<Message>>>,
        turn: Arc<Mutex<usize>>,
    ) -> Self {
        self.history_capture = Some(history);
        self.turn_capture = Some(turn);
        self
    }

    /// 当轮次超过上限 70% 时返回收敛提醒文本。
    /// Returns a convergence reminder text when turns exceed 70% of the limit.
    fn turn_reminder(&self, turn: usize) -> Option<String> {
        if self.max_turns == 0 || turn < self.max_turns * 7 / 10 {
            return None;
        }
        let remaining = self.max_turns.saturating_sub(turn);
        Some(format!(
            "[\u{7cfb}\u{7edf}\u{63d0}\u{793a}] \u{4f60}\u{5df2}\u{4f7f}\u{7528} {}/{} \u{8f6e}\u{ff0c}\u{5269}\u{4f59} {} \u{8f6e}\u{3002}\u{8bf7}\u{4f18}\u{5148}\u{6536}\u{655b}\u{5230}\u{7ed3}\u{8bba}\u{ff0c}\u{907f}\u{514d}\u{8fc7}\u{5ea6}\u{63a2}\u{7d22}\u{3002}\n\
             [System] {}/{} turns used, {} remaining. Prioritize converging to a conclusion, avoid excessive exploration.",
            turn, self.max_turns, remaining,
            turn, self.max_turns, remaining,
        ))
    }

    /// 检测溢出并在必要时压缩历史，返回 `CompletionCallAction::Patch` 或 `CompletionCallAction::Continue`。
    /// Detect overflow and compact if needed, returning `CompletionCallAction::Patch` or `CompletionCallAction::Continue`.
    async fn handle_completion_call(&self, history: &[Message], _turn: usize) -> CompletionCallAction {
        if let Some(hc) = &self.history_capture {
            *hc.lock().unwrap() = history.to_vec();
        }
        if let Some(tc) = &self.turn_capture {
            *tc.lock().unwrap() = _turn;
        }

        let estimated = crate::context::estimate_history_tokens(history);
        let (is_overflow, last_input) = {
            let budget = self.budget.lock().unwrap();
            let last = budget.last_input_tokens();
            let overflow = budget.is_near_overflow(estimated, self.config.compaction_threshold)
                || (last > 0
                    && budget.is_near_overflow(last as usize, self.config.compaction_threshold));
            (overflow, last)
        };

        info!(
            history_len = history.len(),
            estimated_tokens = estimated,
            last_input_tokens = last_input,
            is_overflow,
            "context check"
        );

        if !is_overflow {
            if let Some(reminder) = self.turn_reminder(_turn) {
                let mut patched = history.to_vec();
                patched.push(Message::system(&reminder));
                return CompletionCallAction::Patch(RequestPatch::new().history(patched));
            }
            return CompletionCallAction::Continue;
        }

        let base_history: Vec<Message> = history.to_vec();

        // Tier 2: LLM 摘要压缩（锚定模式 — 更新上一次摘要而非从头创建）。
        // Tier 2: LLM summarization compaction (anchored mode — updates previous summary).
        let tail_budget = {
            let budget = self.budget.lock().unwrap();
            let eff = budget.effective_budget();
            eff / 4
        };
        let (old, recent) =
            crate::context::select_head_tail(&base_history, self.config.keep_recent_turns, tail_budget);
        if old.is_empty() {
            return CompletionCallAction::Continue;
        }

        let previous = crate::context::find_previous_summary(&base_history);
        let summary_prompt = crate::context::build_compaction_prompt(&old, previous.as_deref());
        let summary = match self.compact_via_llm(&summary_prompt).await {
            Ok(s) => s,
            Err(e) => {
                warn!("compaction LLM call failed: {e}");
                crate::context::format_messages_for_summary(&old)
            }
        };

        let summary_msg = Message::system(format!(
            "[对话历史摘要 / Conversation Summary]\n{summary}"
        ));
        let continue_msg = Message::system(
            "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.\n\
             \u{5982}\u{679c}\u{6709}\u{540e}\u{7eed}\u{6b65}\u{9aa4}\u{8bf7}\u{7ee7}\u{7eed}\u{ff0c}\u{5426}\u{5219}\u{8bf7}\u{6c42}\u{6f84}\u{6e05}\u{3002}"
        );
        let compacted: Vec<Message> = std::iter::once(summary_msg)
            .chain(recent.iter().cloned())
            .chain(std::iter::once(continue_msg))
            .collect();

        let new_tokens = crate::context::estimate_history_tokens(&compacted);

        // 缓存结果。
        // Cache the result.
        *self.compaction_cache.lock().unwrap() =
            Some((history.len(), compacted.clone()));

        let _ = self.tx.send(AgentEvent::ContextCompacted {
            old_tokens: estimated,
            new_tokens,
        });
        let _ = self.tx.send(AgentEvent::Info(format!(
            "  [上下文压缩 / context compacted] {estimated} → {new_tokens} tokens"
        )));

        CompletionCallAction::Patch(RequestPatch::new().history(compacted))
    }

    /// 调用 LLM 生成对话历史摘要。
    /// Call the LLM to generate a conversation history summary.
    async fn compact_via_llm(&self, prompt: &str) -> anyhow::Result<String> {
        let agent = self
            .client
            .agent(&self.model)
            .preamble(crate::context::COMPACTION_PREAMBLE)
            .temperature(0.0)
            .build();
        let resp = agent
            .runner(prompt)
            .max_turns(1)
            .run()
            .await
            .map_err(|e| anyhow::anyhow!("compaction failed: {e}"))?;
        Ok(resp.output)
    }

    /// 记录 API 返回的实际 token 用量。
    /// Record actual token usage from the API response.
    fn handle_model_turn_finished(&self, usage: &Usage) {
        let mut budget = self.budget.lock().unwrap();
        budget.record_usage(usage);
        info!(
            input = usage.input_tokens,
            output = usage.output_tokens,
            accumulated_input = budget.accumulated_input(),
            "token usage recorded"
        );
    }
}

impl AgentHook for ContextHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        self.handle_completion_call(event.history, event.turn).await
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        self.handle_model_turn_finished(&event.usage);
        ModelTurnAction::Continue
    }

    /// 只观察低频事件，跳过高频 delta 事件以提升性能。
    /// Observe only low-frequency events, skipping high-frequency deltas for performance.
    fn observes(&self, kind: StepEventKind) -> bool {
        matches!(
            kind,
            StepEventKind::CompletionCall | StepEventKind::ModelTurnFinished
        )
    }
}

/// 将工具调用参数格式化为人类可读的描述。
/// Formats tool call arguments into a human-readable description.
/// 例如 `read_file` 打印读取的文件路径，`run_bash` 打印执行的命令。
/// For example, `read_file` prints the file path being read, `run_bash` prints the command being executed.
fn needs_interactive_terminal(command: &str) -> bool {
    let lower = command.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.contains(&"sudo") && !words.contains(&"-n") {
        return true;
    }
    if words.contains(&"su") {
        return true;
    }
    if words.contains(&"passwd") {
        return true;
    }
    false
}

fn format_tool_call_desc(tool_name: &str, args: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(args).ok();
    let get_str = |key: &str| -> Option<String> {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    match tool_name {
        "read_file" => {
            let path = get_str("path").unwrap_or_default();
            format!("read_file \u{2192} \u{8bfb}\u{53d6}\u{6587}\u{4ef6}: {path}")
        }
        "edit_file" => {
            let path = get_str("path").unwrap_or_default();
            let old = get_str("old").unwrap_or_default();
            let new = get_str("new").unwrap_or_default();
            format!(
                "edit_file \u{2192} \u{7f16}\u{8f91}\u{6587}\u{4ef6}: {path}\n  \u{66ff}\u{6362}: {old}\n  \u{66ff}\u{6362}\u{4e3a}: {new}"
            )
        }
        "write_file" => {
            let path = get_str("path").unwrap_or_default();
            let content_len = parsed
                .as_ref()
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("write_file \u{2192} \u{5199}\u{5165}\u{6587}\u{4ef6}: {path} ({content_len} \u{5b57}\u{8282})")
        }
        "run_bash" => {
            let command = get_str("command").unwrap_or_default();
            format!("run_bash \u{2192} \u{6267}\u{884c}\u{547d}\u{4ee4}: {command}")
        }
        "run_file" => {
            let path = get_str("path").unwrap_or_default();
            format!("run_file \u{2192} \u{6267}\u{884c}\u{811a}\u{672c}: {path}")
        }
        "web_fetch" => {
            let url = get_str("url").unwrap_or_default();
            format!("web_fetch \u{2192} \u{6293}\u{53d6}\u{7f51}\u{9875}: {url}")
        }
        "web_search" => {
            let query = get_str("query").unwrap_or_default();
            format!("web_search \u{2192} \u{641c}\u{7d22}: {query}")
        }
        _ => format!("{tool_name}({args})"),
    }
}

/// 格式化 token 用量摘要为字符串。
/// Formats token usage summary into a string.
fn format_usage(usage: &Usage) -> String {
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    let cached = usage.cached_input_tokens;
    let reasoning = usage.reasoning_tokens;
    if input == 0 && output == 0 {
        return String::new();
    }
    let mut parts = vec![format!("\u{8f93}\u{5165}={input}")];
    if cached > 0 {
        parts.push(format!("\u{7f13}\u{5b58}={cached}"));
    }
    parts.push(format!("\u{8f93}\u{51fa}={output}"));
    if reasoning > 0 {
        parts.push(format!("\u{63a8}\u{7406}={reasoning}"));
    }
    parts.join("\u{ff0c}")
}

/// 纯函数形式的权限分级解析，可不依赖 hook 包装单独测试。`args` 为 JSON 形式的
/// Pure-function permission tier resolution, can be tested independently without hook wrapping. `args` is the JSON-form
/// 工具调用参数（用于从 `run_bash` 中提取 `command`）。
/// tool call arguments (used to extract `command` from `run_bash`).
pub fn decide_tier(perms: &ToolPerms, tool_name: &str, args: &str) -> Permission {
    match tool_name {
        "read_file" => perms.read_file,
        "edit_file" => perms.edit_file,
        "write_file" => perms.write_file,
        "web_fetch" => perms.web_fetch,
        "web_search" => perms.web_search,
        "run_file" => perms.run_bash_mutating,
        "run_bash" => {
            let command = serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(str::to_string))
                .unwrap_or_default();
            if is_readonly_bash(&command) {
                perms.run_bash_readonly
            } else {
                perms.run_bash_mutating
            }
        }
        _ => Permission::Ask,
    }
}

/// 纯函数形式的流程决策。仅用于单元测试，保证确定性与无 IO 依赖。
/// Pure-function flow decision. Used only for unit tests, ensuring determinism and no IO dependency.
/// `Ask` 在此解析为 `ToolCallAction::Skip`；线上 hook 应改用 `on_tool_call` 中
/// `Ask` is resolved as `ToolCallAction::Skip` here; the production hook should use the
/// 的 match tier 分支，对 `Ask` 通过 `confirm()` 进行终端交互。
/// match tier branch in `on_tool_call`, which handles `Ask` via `confirm()` for terminal interaction.
#[cfg(test)]
fn decide_flow(perms: &ToolPerms, tool_name: &str, args: &str) -> ToolCallAction {
    match decide_tier(perms, tool_name, args) {
        Permission::Allow => ToolCallAction::Run,
        Permission::Deny => ToolCallAction::Skip(
            format!("tool `{tool_name}` is denied by policy for this role"),
        ),
        Permission::Ask => ToolCallAction::Skip(
            format!("user declined to run `{tool_name}`"),
        ),
    }
}

/// 针对 `goal` 驱动自主 Agent 循环。指定角色的 Agent 自行规划并执行，调用工具；
/// Drives the autonomous Agent loop for a given `goal`. The role's Agent plans and executes on its own, calling tools;
/// `HitlHook` 门控关键决策。模型结束或达到 max_turns 时停止。
/// `HitlHook` gates critical decisions. Stops when the model finishes or max_turns is reached.
///
/// SSE 断连时自动重试最多 3 次，每次告知模型已完成的工作让其继续。
/// On SSE disconnect, auto-retries up to 3 times, each time telling the model what's done so it can continue.
///
/// `shared_history` 是由 Orchestrator 持有的共享对话历史 Arc。每轮 CompletionCall 时
/// `ContextHook` 会将最新历史写入此 Arc，因此即使 task 被 abort（Esc 中断），
/// partial history 仍保留在 Orchestrator 的状态中，下次对话能继承上下文。
/// `shared_history` is the Orchestrator's shared conversation history Arc. `ContextHook`
/// writes the latest history to it on every CompletionCall, so even if the task is
/// aborted (Esc interrupt), the partial history persists for the next message.
///
/// 所有用户可见输出通过 `tx` channel 发送给 TUI。
/// All user-visible output is sent to the TUI via the `tx` channel.
pub async fn run_autonomous(
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    trust_sandbox: Arc<AtomicBool>,
    role: Role,
    goal: &str,
    tx: &EventSender,
    shared_history: Arc<Mutex<Vec<Message>>>,
) -> anyhow::Result<String> {
    const MAX_RETRIES: usize = 3;

    let captured_history = shared_history;
    let captured_turn: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let mut turns_used: usize = 0;
    let total_max_turns = registry.max_turns_for_role(role);

    for attempt in 0..=MAX_RETRIES {
        let max_turns_remaining = total_max_turns.saturating_sub(turns_used);
        if max_turns_remaining == 0 {
            return Err(anyhow::anyhow!(
                "{role:?} \u{8f6e}\u{6b21}\u{5df2}\u{7528}\u{5c3d}\u{ff08}{turns_used}/{total_max_turns}\u{ff09}"
            ));
        }

        let prompt = if attempt == 0 {
            goal.to_string()
        } else {
            let remaining_retries = MAX_RETRIES - attempt;
            format!(
                "{goal}\n\n\
                 [系统提示 / System] 上次因 SSE 连接中断（第 {attempt}/{MAX_RETRIES} 次重试，剩余 {remaining_retries} 次）。\n\
                 已保留之前的对话历史（{hist_len} 条消息），已用 {turns_used}/{total_max_turns} 轮，剩余 {max_turns_remaining} 轮。\n\
                 请基于已有进展继续，注意：\n\
                 - 跳过已完成的探索和工具调用，不要重复\n\
                 - 直接从上次断点处继续执行\n\
                 - 优先收敛到结论，避免过度探索\n\
                 - 如果上次输出不完整，请从头生成完整版本\n\n\
                 [System] Previous SSE stream disconnected (attempt {attempt}/{MAX_RETRIES}, {remaining_retries} retries left). \
                 Conversation history preserved ({hist_len} messages), {turns_used}/{total_max_turns} turns used, {max_turns_remaining} remaining. \
                 Continue from where you left off — skip completed steps, avoid repetition, and prioritize convergence.",
                hist_len = captured_history.lock().unwrap().len()
            )
        };

        let perms = registry.tool_perms(role);
        let hitl_waiting = Arc::new(AtomicBool::new(false));
        let hook = HitlHook::new(perms, hitl_waiting.clone(), tx.clone(), sandbox.clone(), trust_sandbox.clone());

        let model = registry
            .session_model()
            .or_else(|| registry.role_config(role).map(|c| c.model.clone()))
            .unwrap_or_else(|| registry.effective_model());
        let model_for_log = model.clone();
        let context_limit = crate::providers::context_limit_for_model(&model);
        let context_config = registry.context_config().clone();
        let context_client = registry.create_client()?;
        let context_hook = ContextHook::new(
            context_limit,
            context_config,
            context_client,
            model,
            tx.clone(),
            max_turns_remaining,
        )
        .with_history_capture(captured_history.clone(), captured_turn.clone());

        let agent = build_runner_agent(registry, role)?;
        let prior_history = captured_history.lock().unwrap().clone();
        let mut runner = agent
            .runner(&prompt)
            .max_turns(max_turns_remaining)
            .max_invalid_tool_call_retries(3)
            .add_hook(hook)
            .add_hook(context_hook);
        if !prior_history.is_empty() {
            runner = runner.history(prior_history);
        }
        let stream = runner.stream().await;

        match consume_stream(stream, Some(hitl_waiting), tx).await {
            Ok(output) => return Ok(output),
            Err(e) if is_context_overflow_error(&e) && attempt < MAX_RETRIES => {
                let mut hist = captured_history.lock().unwrap();
                let old_len = hist.len();
                if old_len > 6 {
                    let keep = 4;
                    let retained: Vec<Message> = hist[old_len - keep..].to_vec();
                    *hist = std::iter::once(Message::system(
                        "[反应式压缩 / Reactive compaction] 上下文溢出，旧对话历史已截断。请基于保留的近期消息继续。\n\
                         [System] Context overflow — old history truncated. Continue from the retained recent messages."
                    ))
                    .chain(retained)
                    .collect();
                    let _ = tx.send(AgentEvent::Info(format!(
                        "[反应式压缩 / Reactive compaction] 上下文溢出，历史从 {old_len} 条截断为 {} 条消息",
                        hist.len()
                    )));
                    drop(hist);
                    continue;
                }
                drop(hist);
                let _ = tx.send(AgentEvent::Error(
                    "上下文溢出且历史过短，无法进一步压缩 / Context overflow with insufficient history to compact".to_string(),
                ));
                return Err(e);
            }
            Err(e) if is_stream_error(&e) && attempt < MAX_RETRIES => {
                turns_used = *captured_turn.lock().unwrap();
                let hist_len = captured_history.lock().unwrap().len();
                let err_snippet: String = e.to_string().chars().take(200).collect();
                warn!(
                    error = %e,
                    error_debug = ?e,
                    attempt = attempt + 1,
                    max_retries = MAX_RETRIES,
                    role = ?role,
                    turns_used,
                    total_max_turns,
                    captured_history_len = hist_len,
                    model = %model_for_log,
                    "SSE disconnect, retrying with preserved history"
                );
                let _ = tx.send(AgentEvent::Info(format!(
                    "[重试 / Retry] {role:?} 第 {}/{} 次：SSE 连接中断。\n  · 已用轮数: {turns_used}/{total_max_turns}（剩余 {max_turns_remaining} 轮）\n  · 保留历史: {hist_len} 条消息\n  · 使用模型: {model_for_log}\n  · 错误摘要: {err_snippet}",
                    attempt + 1,
                    MAX_RETRIES
                )));
                continue;
            }
            Err(e) => {
                let err_snippet: String = e.to_string().chars().take(300).collect();
                warn!(error = %e, error_debug = ?e, role = ?role, "stream error (non-retryable)");
                let _ = tx.send(AgentEvent::Error(format!(
                    "流错误（不可重试 / Non-retryable stream error）\n  · 角色: {role:?}\n  · 使用模型: {model_for_log}\n  · 已用轮数: {turns_used}/{total_max_turns}\n  · 错误详情: {err_snippet}"
                )));
                return Err(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "{role:?} 重试 {MAX_RETRIES} 次后仍失败（SSE 连接反复中断）。\n  · 已用轮数: {turns_used}/{total_max_turns}\n  · 保留历史: {} 条消息\n建议检查网络或 API 稳定性后重试。\n\
         [System] {role:?} failed after {MAX_RETRIES} retries (repeated SSE disconnects). Turns used: {turns_used}/{total_max_turns}. Check network/API stability and try again.",
        captured_history.lock().unwrap().len()
    ))
}

/// 判断错误是否为 SSE 流式断连（可安全重试）。
/// Determines whether an error is an SSE stream disconnect (safe to retry).
pub fn is_stream_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    if msg.contains("MaxTurnsError") || msg.contains("max turns limit") {
        return false;
    }
    msg.contains("error decoding response body")
        || msg.contains("SSE error")
        || msg.contains("Reset(StreamId")
        || msg.contains("流式错误")
        || msg.contains("空闲超时")
}

/// 判断错误是否为上下文窗口溢出（可安全压缩后重试）。
/// Determines whether an error is a context window overflow (safe to compact and retry).
pub fn is_context_overflow_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length_exceeded")
        || msg.contains("context length")
        || msg.contains("prompt_too_long")
        || msg.contains("maximum context")
        || msg.contains("token limit")
        || msg.contains("context window")
        || msg.contains("max_tokens")
        || msg.contains("上下文")
        || msg.contains("too long")
}

/// 消费流式输出：文本增量通过 channel 发送给 TUI，reasoning 实时发送。
/// Consumes streaming output: text deltas are sent to the TUI via channel, reasoning sent in real time.
/// 供 `run_autonomous` 和 `RoleAgent::run` 共用。
/// Shared by `run_autonomous` and `RoleAgent::run`.
///
/// 当模型将全部内容放在 reasoning 通道（content 字段为空）时，
/// When the model puts all content in the reasoning channel (content field is empty),
/// 用累积的 reasoning 内容作为输出回退，避免下游收到空计划。
/// the accumulated reasoning content is used as output fallback, avoiding empty plans downstream.
pub async fn consume_stream<R>(
    mut stream: StreamingResult<R>,
    hitl_waiting: Option<Arc<AtomicBool>>,
    tx: &EventSender,
) -> anyhow::Result<String> {
    use MultiTurnStreamItem;
    use rig_core::streaming::StreamedAssistantContent;

    const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

    let mut output = String::new();
    let mut all_reasoning = String::new();

    loop {
        // When a HITL prompt is active the stream is blocked inside the hook's
        // `confirm()` future awaiting the user's keypress.  The `hitl_waiting`
        // flag is set *during* `stream.next()` (inside `confirm()`), so it may
        // be false on entry but become true mid-call.  If the 120-second
        // timeout fires while HITL is active the stream future is dropped,
        // which cancels `confirm()` and orphans the oneshot responder still
        // held by the TUI.  To avoid this we retry without a timeout when the
        // flag is set.
        // 当 HITL 确认处于活动状态时，流被阻塞在 hook 的 `confirm()` future 中
        // 等待用户按键。`hitl_waiting` 标志是在 `stream.next()` 期间（在
        // `confirm()` 内部）设置的，因此进入时可能为 false 但中途变为 true。
        // 如果 120 秒超时在 HITL 活动期间触发，流 future 被丢弃，会取消
        // `confirm()` 并使 TUI 仍持有的 oneshot responder 成为孤儿。为避免
        // 此问题，当标志被设置时无超时重试。
        let next = match &hitl_waiting {
            Some(flag) if flag.load(Ordering::Relaxed) => stream.next().await,
            _ => match tokio::time::timeout(CHUNK_TIMEOUT, stream.next()).await {
                Ok(item) => item,
                Err(_) => {
                    // Check whether HITL started during the timed wait.
                    // 检查 HITL 是否在超时等待期间开始。
                    if let Some(flag) = &hitl_waiting {
                        if flag.load(Ordering::Relaxed) {
                            // HITL became active mid-timeout — retry without
                            // a deadline so the user has unlimited time to
                            // respond.
                            // HITL 在超时期间变为活动——无截止时间重试，
                            // 让用户有无限时间响应。
                            continue;
                        }
                    }
                    warn!(
                        timeout_secs = CHUNK_TIMEOUT.as_secs(),
                        accumulated_output_chars = output.len(),
                        accumulated_reasoning_chars = all_reasoning.len(),
                        "SSE idle timeout, connection may have dropped"
                    );
                    return Err(anyhow::anyhow!(
                        "SSE 空闲超时 / SSE idle timeout（{} 秒无数据），连接可能已断开。\n  · 已收到: {} 字符文本、{} 字符推理内容\n  · 上层会自动重试（如果是可重试错误）\n\
                         [System] SSE idle timeout (no data for {}s), connection may have dropped. \
                         Received {} chars text, {} chars reasoning. Upper layer will auto-retry if applicable.",
                        CHUNK_TIMEOUT.as_secs(),
                        output.len(),
                        all_reasoning.len(),
                        CHUNK_TIMEOUT.as_secs(),
                        output.len(),
                        all_reasoning.len()
                    ));
                }
            },
        };

        let Some(item) = next else { break };

        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                StreamedAssistantContent::Text(t) => {
                    if !t.text.is_empty() {
                        let _ = tx.send(AgentEvent::TextDelta(t.text));
                    }
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    let _ = tx.send(AgentEvent::ReasoningDelta(reasoning.clone()));
                    all_reasoning.push_str(&reasoning);
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    let desc = format_tool_call_desc(
                        &tool_call.function.name,
                        &tool_call.function.arguments.to_string(),
                    );
                    let _ = tx.send(AgentEvent::ToolCall {
                        name: tool_call.function.name.clone(),
                        desc,
                    });
                }
                _ => {}
            },
            Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                output = resp.output;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    error_debug = ?e,
                    accumulated_output_chars = output.len(),
                    accumulated_reasoning_chars = all_reasoning.len(),
                    "stream item error"
                );
                let msg = e.to_string();
                if msg.contains("MaxTurnsError") {
                    return Err(anyhow::anyhow!("{e}"));
                }
                return Err(anyhow::anyhow!(
                    "流式错误 / Stream error: {e}\n  · 已收到: {} 字符文本、{} 字符推理内容\n  · 如果是网络波动导致的断连，上层会自动重试\n\
                     [System] Stream error: {e}. Received {} chars text, {} chars reasoning. \
                     Upper layer will auto-retry if this is a transient network issue.",
                    output.len(),
                    all_reasoning.len(),
                    output.len(),
                    all_reasoning.len()
                ));
            }
            _ => {}
        }
    }

    if output.is_empty() {
        if !all_reasoning.is_empty() {
            output = all_reasoning;
        } else {
            output.push_str("(\u{65e0}\u{8f93}\u{51fa})");
        }
    }
    let _ = tx.send(AgentEvent::Agent(output.clone()));
    Ok(output)
}

/// 为某角色构建"可运行"的 rig `Agent`（带工具），并遵循会话级模型覆盖。
/// Builds a "runnable" rig `Agent` (with tools) for a role, respecting session-level model override.
/// 与 `AgentRegistry::build` 类似，但返回原始 `Agent`，以便附加 runner 与 hook。
/// Similar to `AgentRegistry::build`, but returns the raw `Agent` so runner and hooks can be attached.
fn build_runner_agent(
    registry: &AgentRegistry,
    role: Role,
) -> anyhow::Result<Agent<OpenAiModel>> {
    let rc = registry
        .role_config(role)
        .ok_or_else(|| anyhow::anyhow!("no config for role {role:?}"))?;
    let client = registry.create_client()?;
    let preamble = std::fs::read_to_string(&rc.preamble)
        .unwrap_or_else(|_| format!("\u{4f60}\u{662f} {role:?} agent\u{3002}"));
    let preamble = crate::registry::inject_skills_public(&preamble);
    let model = registry.session_model().unwrap_or_else(|| rc.model.clone());
    let max_turns = registry.max_turns_for_role(role);
    info!("[runner] role={role:?} model={model} max_turns={max_turns}");
    let params = crate::providers::provider_additional_params();
    let max_output = registry.context_config().max_output_tokens as u64;
    let reasoning = crate::providers::is_reasoning_model(&model);
    if reasoning {
        info!("[runner] reasoning model detected, skipping max_tokens (model default applies)");
    }
    let builder = client
        .agent(&model)
        .preamble(&preamble)
        .temperature(crate::providers::Provider::clamp_temperature(0.7));
    let builder = crate::tools::add_builtin_tools(builder, registry.context_config(), registry.sandbox())
        .additional_params(params)
        .default_max_turns(max_turns);
    let agent = if reasoning {
        builder.build()
    } else {
        builder.max_tokens(max_output).build()
    };
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms() -> ToolPerms {
        ToolPerms {
            read_file: Permission::Allow,
            run_bash_readonly: Permission::Allow,
            run_bash_mutating: Permission::Ask,
            edit_file: Permission::Ask,
            write_file: Permission::Ask,
            web_fetch: Permission::Allow,
            web_search: Permission::Allow,
        }
    }

    #[test]
    fn read_file_auto_runs() {
        assert!(matches!(
            decide_flow(&perms(), "read_file", r#"{"path":"x"}"#),
            ToolCallAction::Run
        ));
    }

    #[test]
    fn readonly_bash_auto_runs() {
        assert!(matches!(
            decide_flow(&perms(), "run_bash", r#"{"command":"ls -la"}"#),
            ToolCallAction::Run
        ));
    }

    #[test]
    fn mutating_bash_asks() {
        assert!(matches!(
            decide_flow(&perms(), "run_bash", r#"{"command":"rm -rf x"}"#),
            ToolCallAction::Skip(..)
        ));
    }

    #[test]
    fn edit_file_asks() {
        assert!(matches!(
            decide_flow(&perms(), "edit_file", r#"{"path":"x","old":"a","new":"b"}"#),
            ToolCallAction::Skip(..)
        ));
    }

    #[test]
    fn write_file_asks_like_edit() {
        assert!(matches!(
            decide_flow(&perms(), "write_file", r#"{"path":"x","content":"hi"}"#),
            ToolCallAction::Skip(..)
        ));
    }

    #[test]
    fn denied_tool_skips() {
        let mut p = perms();
        p.run_bash_readonly = Permission::Deny;
        assert!(matches!(
            decide_flow(&p, "run_bash", r#"{"command":"cat x"}"#),
            ToolCallAction::Skip(..)
        ));
    }

    #[test]
    fn unknown_tool_asks() {
        assert!(matches!(
            decide_flow(&perms(), "mystery", r#"{}"#),
            ToolCallAction::Skip(..)
        ));
    }

    #[test]
    fn overflow_error_detected() {
        assert!(is_context_overflow_error(&anyhow::anyhow!("context_length_exceeded")));
        assert!(is_context_overflow_error(&anyhow::anyhow!("prompt_too_long")));
        assert!(is_context_overflow_error(&anyhow::anyhow!("maximum context window exceeded")));
        assert!(is_context_overflow_error(&anyhow::anyhow!("token limit reached")));
        assert!(is_context_overflow_error(&anyhow::anyhow!("上下文超出限制")));
    }

    #[test]
    fn non_overflow_error_not_detected() {
        assert!(!is_context_overflow_error(&anyhow::anyhow!("network timeout")));
        assert!(!is_context_overflow_error(&anyhow::anyhow!("permission denied")));
        assert!(!is_context_overflow_error(&anyhow::anyhow!("file not found")));
    }
}
