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
use rig_core::agent::{
    AgentHook, Flow, HookContext, RequestPatch, StepEvent, StepEventKind,
};
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, Message, Usage};
use rig_core::providers::openai::CompletionModel as OpenAiModel;
use rig_core::tool::ToolDyn;
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
}

impl HitlHook {
    pub fn new(
        perms: ToolPerms,
        waiting: Arc<AtomicBool>,
        tx: EventSender,
        sandbox: Sandbox,
    ) -> Self {
        Self {
            perms: Arc::new(Mutex::new(perms)),
            waiting,
            tx,
            sandbox,
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

    async fn maybe_run_interactive(&self, tool_name: &str, args: &str) -> Option<Flow> {
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
        Some(Flow::Skip { reason: output })
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

impl<M: CompletionModel> AgentHook<M> for HitlHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            StepEvent::TextDelta { .. } => Flow::Continue,

            StepEvent::ModelTurnFinished { turn, usage, .. } => {
                let usage_str = format_usage(&usage);
                let _ = self
                    .tx
                    .send(AgentEvent::TurnFinished { turn, usage: usage_str });
                Flow::Continue
            }

            StepEvent::ToolCall { tool_name, args, .. } => {
                // ── 沙箱检查 ──
                // 在权限分级检查之前，先检查工具调用是否访问沙箱外的路径。
                // If the path is outside the sandbox and not yet authorized, prompt the user.
                // Sandbox check: before the permission tier check, verify that the tool call
                // doesn't access paths outside the sandbox. If it does and the directory
                // hasn't been authorized, prompt the user for authorization.
                if let Some(sandbox_err) = self.sandbox.check_tool(tool_name, args) {
                    let desc = format!(
                        "\u{1f512} \u{6c99}\u{7bb1}\u{5916}\u{8bbf}\u{95ee}\u{6388}\u{6743}\n\n{}\n\n\
                         \u{662f}\u{5426}\u{5141}\u{8bb8}\u{8bbf}\u{95ee}\u{6b64}\u{8def}\u{5f84}\u{ff1f}",
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
                        return Flow::Skip {
                            reason: format!("\u{6c99}\u{7bb1}\u{62d2}\u{7edd}\u{8bbf}\u{95ee}: {sandbox_err}"),
                        };
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
                        if let Some(flow) = self.maybe_run_interactive(tool_name, args).await {
                            return flow;
                        }
                        Flow::Continue
                    }
                    Permission::Deny => {
                        let _ = self.tx.send(AgentEvent::Info(
                            "  [HITL] \u{5df2}\u{62d2}\u{7edd}\u{ff08}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{ff09}".into(),
                        ));
                        Flow::Skip {
                            reason: format!(
                                "\u{5de5}\u{5177} `{tool_name}` \u{88ab}\u{5f53}\u{524d}\u{89d2}\u{8272}\u{7684}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{7981}\u{6b62}"
                            ),
                        }
                    }
                    Permission::Ask => {
                        let desc = format_tool_call_desc(tool_name, args);
                        if self.confirm(tool_name, &desc).await {
                            if let Some(flow) = self.maybe_run_interactive(tool_name, args).await {
                                return flow;
                            }
                            Flow::Continue
                        } else {
                            Flow::Skip {
                                reason: format!(
                                    "\u{7528}\u{6237}\u{62d2}\u{7edd}\u{4e86} `{tool_name}` \u{7684}\u{6267}\u{884c}"
                                ),
                            }
                        }
                    }
                }
            }

            StepEvent::ToolResult {
                tool_name, result, ..
            } => {
                let _ = self.tx.send(AgentEvent::ToolResult {
                    name: tool_name.to_string(),
                    result: result.to_string(),
                    ok: true,
                });
                Flow::Continue
            }

            StepEvent::InvalidToolCall(ctx) => {
                warn!("[\u{672a}\u{77e5}\u{5de5}\u{5177}] {}", ctx.tool_name);
                let _ = self.tx.send(AgentEvent::ToolResult {
                    name: ctx.tool_name.to_string(),
                    result: "unknown tool".to_string(),
                    ok: false,
                });
                Flow::Continue
            }

            _ => Flow::Continue,
        }
    }
}

/// 上下文管理 Hook：在每轮 API 调用前检测 token 溢出，溢出时压缩旧对话历史。
/// Context management Hook: detects token overflow before each API call,
/// compacting old conversation history when the threshold is exceeded.
///
/// 利用 rig 的 `Flow::PatchRequest` + `RequestPatch::history()`，
/// Uses rig's `Flow::PatchRequest` + `RequestPatch::history()`,
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
    ) -> Self {
        let budget = crate::context::TokenBudget::new(context_limit, config.max_output_tokens);
        Self {
            budget: Arc::new(Mutex::new(budget)),
            config,
            client,
            model,
            tx,
            compaction_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 检测溢出并在必要时压缩历史，返回 `Flow::PatchRequest` 或 `Flow::Continue`。
    /// Detect overflow and compact if needed, returning `Flow::PatchRequest` or `Flow::Continue`.
    async fn handle_completion_call(&self, history: &[Message], _turn: usize) -> Flow {
        let estimated = crate::context::estimate_history_tokens(history);
        let is_overflow = {
            let budget = self.budget.lock().unwrap();
            if budget.last_input_tokens() > 0 {
                budget.is_near_overflow(
                    budget.last_input_tokens() as usize,
                    self.config.compaction_threshold,
                )
            } else {
                budget.is_near_overflow(estimated, self.config.compaction_threshold)
            }
        };

        // Tier 1 触发：估算 token 超过微压缩阈值。
        // Tier 1 trigger: estimated tokens exceed microcompact threshold.
        let need_microcompact = estimated > self.config.microcompact_threshold;

        if !is_overflow && !need_microcompact {
            return Flow::Continue;
        }

        // 检查缓存：历史长度未变时复用上次的压缩结果。
        // Check cache: reuse last compaction when history length is unchanged.
        {
            let cache = self.compaction_cache.lock().unwrap();
            if let Some((cached_len, ref cached_history)) = *cache {
                if cached_len == history.len() {
                    info!(
                        cached_len,
                        "reusing cached compaction"
                    );
                    return Flow::patch_request(
                        RequestPatch::new().history(cached_history.clone()),
                    );
                }
            }
        }

        // Tier 1: 微压缩 — 清除旧工具结果（无 LLM 调用）。
        // Tier 1: Microcompact — clear old tool results (no LLM call).
        let base_history: Vec<Message> = if need_microcompact {
            let microcompacted = crate::context::microcompact(
                history,
                self.config.microcompact_protected_results,
            );
            let micro_tokens = crate::context::estimate_history_tokens(&microcompacted);

            // 重新检查微压缩后是否仍溢出。
            // Re-check overflow after microcompact.
            let still_overflow = {
                let budget = self.budget.lock().unwrap();
                if budget.last_input_tokens() > 0 {
                    budget.is_near_overflow(
                        budget.last_input_tokens() as usize,
                        self.config.compaction_threshold,
                    )
                } else {
                    budget.is_near_overflow(micro_tokens, self.config.compaction_threshold)
                }
            };

            if !still_overflow {
                if micro_tokens < estimated {
                    // 微压缩已足够，直接返回。
                    // Microcompact was sufficient — return immediately.
                    *self.compaction_cache.lock().unwrap() =
                        Some((history.len(), microcompacted.clone()));
                    let _ = self.tx.send(AgentEvent::ContextCompacted {
                        old_tokens: estimated,
                        new_tokens: micro_tokens,
                    });
                    let _ = self.tx.send(AgentEvent::Info(format!(
                        "  [微压缩 / microcompact] {estimated} → {micro_tokens} tokens"
                    )));
                    return Flow::patch_request(
                        RequestPatch::new().history(microcompacted),
                    );
                } else if !is_overflow {
                    // 无工具结果可清除，且未溢出。
                    // No tool results to clear, and no overflow.
                    return Flow::Continue;
                }
            }

            // 仍溢出 — 使用微压缩后的历史作为 Tier 2 的基础。
            // Still overflow — use microcompacted history as base for Tier 2.
            microcompacted
        } else {
            history.to_vec()
        };

        // Tier 2: LLM 摘要压缩。
        // Tier 2: LLM summarization compaction.
        // 切分历史：旧消息（待压缩）+ 近期消息（保留）。
        // Split history: old messages (to compact) + recent messages (to keep).
        let (old, recent) =
            crate::context::split_history(&base_history, self.config.keep_recent_turns);
        if old.is_empty() {
            return Flow::Continue;
        }

        // 通过 LLM 摘要旧消息。
        // Summarize old messages via LLM.
        let summary_prompt = crate::context::format_messages_for_summary(&old);
        let summary = match self.compact_via_llm(&summary_prompt).await {
            Ok(s) => s,
            Err(e) => {
                warn!("compaction LLM call failed: {e}");
                // 回退：用截断的旧消息文本作为摘要。
                // Fallback: use truncated old message text as summary.
                crate::context::format_messages_for_summary(&old)
            }
        };

        // 构建压缩后的历史：[摘要 system 消息, ...近期消息]。
        // Build compacted history: [summary system message, ...recent messages].
        let summary_msg = Message::system(format!(
            "[对话历史摘要 / Conversation Summary]\n{summary}"
        ));
        let compacted: Vec<Message> = std::iter::once(summary_msg)
            .chain(recent.iter().cloned())
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

        Flow::patch_request(RequestPatch::new().history(compacted))
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

impl<M: CompletionModel> AgentHook<M> for ContextHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            StepEvent::CompletionCall { history, turn, .. } => {
                self.handle_completion_call(history, turn).await
            }
            StepEvent::ModelTurnFinished { usage, .. } => {
                self.handle_model_turn_finished(&usage);
                Flow::Continue
            }
            _ => Flow::Continue,
        }
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
        "edit_file" | "write_file" => perms.edit_file,
        "web_fetch" => perms.web_fetch,
        "web_search" => perms.web_search,
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
/// `Ask` 在此解析为 `Flow::Skip`；线上 hook 应改用 `on_event` 中
/// `Ask` is resolved as `Flow::Skip` here; the production hook should use the
/// 的 match tier 分支，对 `Ask` 通过 `confirm()` 进行终端交互。
/// match tier branch in `on_event`, which handles `Ask` via `confirm()` for terminal interaction.
#[cfg(test)]
fn decide_flow(perms: &ToolPerms, tool_name: &str, args: &str) -> Flow {
    match decide_tier(perms, tool_name, args) {
        Permission::Allow => Flow::Continue,
        Permission::Deny => Flow::Skip {
            reason: format!("tool `{tool_name}` is denied by policy for this role"),
        },
        Permission::Ask => Flow::Skip {
            reason: format!("user declined to run `{tool_name}`"),
        },
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
/// 所有用户可见输出通过 `tx` channel 发送给 TUI。
/// All user-visible output is sent to the TUI via the `tx` channel.
pub async fn run_autonomous(
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    role: Role,
    goal: &str,
    tx: &EventSender,
) -> anyhow::Result<String> {
    const MAX_RETRIES: usize = 3;

    for attempt in 0..=MAX_RETRIES {
        let prompt = if attempt == 0 {
            goal.to_string()
        } else {
            format!(
                "{goal}\n\n\
                 [\u{7cfb}\u{7edf}\u{63d0}\u{793a}] \u{4e0a}\u{6b21}\u{6267}\u{884c}\u{56e0} SSE \u{8fde}\u{63a5}\u{4e2d}\u{65ad}\u{ff08}\u{7b2c} {attempt} \u{6b21}\u{91cd}\u{8bd5}\u{ff09}\u{3002}\
                 \u{8bf7}\u{7528} `ls -R .` \u{68c0}\u{67e5}\u{5df2}\u{521b}\u{5efa}\u{7684}\u{6587}\u{4ef6}\u{ff0c}\u{8df3}\u{8fc7}\u{5df2}\u{5b8c}\u{6210}\u{7684}\u{6b65}\u{9aa4}\u{ff0c}\u{7ee7}\u{7eed}\u{672a}\u{5b8c}\u{6210}\u{7684}\u{90e8}\u{5206}\u{3002}"
            )
        };

        let perms = registry.tool_perms(role);
        let max_turns = registry.max_turns();
        let hitl_waiting = Arc::new(AtomicBool::new(false));
        let hook = HitlHook::new(perms, hitl_waiting.clone(), tx.clone(), sandbox.clone());

        let model = registry
            .session_model()
            .or_else(|| registry.role_config(role).map(|c| c.model.clone()))
            .unwrap_or_else(|| registry.effective_model());
        let context_limit = crate::providers::context_limit_for_model(&model);
        let context_config = registry.context_config().clone();
        let context_client = crate::providers::create_client()?;
        let context_hook = ContextHook::new(
            context_limit,
            context_config,
            context_client,
            model,
            tx.clone(),
        );

        let agent = build_runner_agent(registry, role)?;
        let stream = agent
            .runner(&prompt)
            .max_turns(max_turns)
            .max_invalid_tool_call_retries(3)
            .add_hook(hook)
            .add_hook(context_hook)
            .stream()
            .await;

        match consume_stream(stream, Some(hitl_waiting), tx).await {
            Ok(output) => return Ok(output),
            Err(e) if is_stream_error(&e) && attempt < MAX_RETRIES => {
                let _ = tx.send(AgentEvent::Info(format!(
                    "[\u{91cd}\u{8bd5}] {role:?} \u{7b2c} {}/{} \u{6b21}\u{ff1a}SSE \u{8fde}\u{63a5}\u{4e2d}\u{65ad}\u{ff0c}\u{91cd}\u{65b0}\u{8c03}\u{7528}",
                    attempt + 1,
                    MAX_RETRIES
                )));
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(anyhow::anyhow!(
        "{role:?} \u{91cd}\u{8bd5} {MAX_RETRIES} \u{6b21}\u{540e}\u{4ecd}\u{5931}\u{8d25}"
    ))
}

/// 判断错误是否为 SSE 流式断连（可安全重试）。
/// Determines whether an error is an SSE stream disconnect (safe to retry).
pub fn is_stream_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("error decoding response body")
        || msg.contains("SSE error")
        || msg.contains("Reset(StreamId")
        || msg.contains("\u{6d41}\u{5f0f}\u{9519}\u{8bef}")
        || msg.contains("\u{7a7a}\u{95f2}\u{8d85}\u{65f6}")
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
    mut stream: rig_core::agent::StreamingResult<R>,
    hitl_waiting: Option<Arc<AtomicBool>>,
    tx: &EventSender,
) -> anyhow::Result<String> {
    use rig_core::agent::MultiTurnStreamItem;
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
                    return Err(anyhow::anyhow!(
                        "SSE \u{7a7a}\u{95f2}\u{8d85}\u{65f6}\u{ff08}{}\u{79d2}\u{65e0}\u{6570}\u{636e}\u{ff09}\u{ff0c}\u{53ef}\u{80fd}\u{8fde}\u{63a5}\u{5df2}\u{65ad}\u{5f00}",
                        CHUNK_TIMEOUT.as_secs()
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
                return Err(anyhow::anyhow!(
                    "\u{6d41}\u{5f0f}\u{9519}\u{8bef}: {e}"
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
) -> anyhow::Result<rig_core::agent::Agent<OpenAiModel>> {
    let rc = registry
        .role_config(role)
        .ok_or_else(|| anyhow::anyhow!("no config for role {role:?}"))?;
    let client = crate::providers::create_client()?;
    let preamble = std::fs::read_to_string(&rc.preamble)
        .unwrap_or_else(|_| format!("\u{4f60}\u{662f} {role:?} agent\u{3002}"));
    let preamble = crate::registry::inject_skills_public(&preamble);
    let model = registry.session_model().unwrap_or_else(|| rc.model.clone());
    let max_turns = registry.max_turns();
    info!("[runner] role={role:?} model={model} max_turns={max_turns}");
    let tools: Vec<Box<dyn ToolDyn>> = crate::tools::builtin_tools(registry.context_config())?;
    let params = crate::providers::provider_additional_params();
    let agent = client
        .agent(&model)
        .preamble(&preamble)
        .temperature(crate::providers::Provider::clamp_temperature(0.7))
        .tools(tools)
        .additional_params(params)
        .default_max_turns(max_turns)
        .build();
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::agent::Flow;

    fn perms() -> ToolPerms {
        ToolPerms {
            read_file: Permission::Allow,
            run_bash_readonly: Permission::Allow,
            run_bash_mutating: Permission::Ask,
            edit_file: Permission::Ask,
            web_fetch: Permission::Allow,
            web_search: Permission::Allow,
        }
    }

    #[test]
    fn read_file_auto_runs() {
        assert!(matches!(
            decide_flow(&perms(), "read_file", r#"{"path":"x"}"#),
            Flow::Continue
        ));
    }

    #[test]
    fn readonly_bash_auto_runs() {
        assert!(matches!(
            decide_flow(&perms(), "run_bash", r#"{"command":"ls -la"}"#),
            Flow::Continue
        ));
    }

    #[test]
    fn mutating_bash_asks() {
        assert!(matches!(
            decide_flow(&perms(), "run_bash", r#"{"command":"rm -rf x"}"#),
            Flow::Skip { .. }
        ));
    }

    #[test]
    fn edit_file_asks() {
        assert!(matches!(
            decide_flow(&perms(), "edit_file", r#"{"path":"x","old":"a","new":"b"}"#),
            Flow::Skip { .. }
        ));
    }

    #[test]
    fn write_file_asks_like_edit() {
        assert!(matches!(
            decide_flow(&perms(), "write_file", r#"{"path":"x","content":"hi"}"#),
            Flow::Skip { .. }
        ));
    }

    #[test]
    fn denied_tool_skips() {
        let mut p = perms();
        p.run_bash_readonly = Permission::Deny;
        assert!(matches!(
            decide_flow(&p, "run_bash", r#"{"command":"cat x"}"#),
            Flow::Skip { .. }
        ));
    }

    #[test]
    fn unknown_tool_asks() {
        assert!(matches!(
            decide_flow(&perms(), "mystery", r#"{}"#),
            Flow::Skip { .. }
        ));
    }
}
