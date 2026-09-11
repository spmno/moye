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
    Agent, AgentHook, CompletionCallAction, HookContext, InvalidToolCallAction,
    InvalidToolCallContext, ModelTurnAction, ModelTurnFinished, MultiTurnStreamItem, RequestPatch,
    StepEventKind, StreamingResult, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_agent::client::AgentClientExt;
use rig_core::completion::message::{AssistantContent, ToolCall as MessageToolCall, ToolFunction};
use rig_core::completion::{Message, Usage};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::event::{AgentEvent, EventSender, FileEdit, HitlDecision};
use crate::events::{PreStepState, WaterfallAction, WaterfallEvent, WaterfallRegistry};
use crate::registry::{AgentRegistry, ApprovalChain, DefaultApproval, Permission, Role, ToolPerms};
use crate::sandbox::Sandbox;
use crate::seam::{ApprovalRequest, ApprovalVerdict, ToolApproval};
use crate::tools::is_readonly_bash;
use crate::tools::pipeline::{PipelineCall, PipelineResult, PostAction, PreAction};

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
    approval: Arc<ApprovalChain>,
    role: String,
    waiting: Arc<AtomicBool>,
    tx: EventSender,
    sandbox: Sandbox,
    /// 信任模式：为 true 时沙箱外访问自动授权，不弹窗确认。
    /// Trust mode: when true, out-of-sandbox access is auto-authorized without prompting.
    trust_sandbox: Arc<AtomicBool>,
    /// todo 9: 简化 pre/post 监听器（pre/post 层，独立于 around-execute 层）。
    /// `Arc` 使 `HitlHook` 保持 `Clone`（`PipelineHooks` 本身不可 Clone——内部是
    /// `Vec<Box<dyn Fn>>`）。pre 监听器在 `on_tool_call` 现有沙箱/权限检查之前运行；
    /// post 监听器在 `on_tool_result` 现有通知之后运行。原 y/n HITL 逻辑不变。
    /// todo 9: simplified pre/post listeners (pre/post layer, independent of the
    /// around-execute layer). `Arc` keeps `HitlHook` `Clone` (`PipelineHooks` itself is
    /// not Clone — it holds `Vec<Box<dyn Fn>>`). pre listeners run in `on_tool_call`
    /// BEFORE the existing sandbox/permission check; post listeners run in
    /// `on_tool_result` AFTER the existing notification. The original y/n HITL logic is unchanged.
    pipeline_hooks: Arc<crate::tools::pipeline::PipelineHooks>,
    /// todo 11: 全功能 waterfall 监听器注册表（emit/waterfall/serial）。
    /// 在 `on_tool_call` / `on_tool_result` 中派发 ToolsPreExecute /
    /// ToolsPostExecute 事件。空注册表为 no-op（默认行为不变）。
    /// todo 11: full waterfall listener registry (emit/waterfall/serial).
    /// Dispatches ToolsPreExecute / ToolsPostExecute from `on_tool_call` /
    /// `on_tool_result`. Empty registry is a no-op (default behavior unchanged).
    waterfall: Arc<WaterfallRegistry>,
}

impl HitlHook {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        approval: Arc<ApprovalChain>,
        role: String,
        waiting: Arc<AtomicBool>,
        tx: EventSender,
        sandbox: Sandbox,
        trust_sandbox: Arc<AtomicBool>,
        pipeline_hooks: Arc<crate::tools::pipeline::PipelineHooks>,
        waterfall: Arc<WaterfallRegistry>,
    ) -> Self {
        Self {
            approval,
            role,
            waiting,
            tx,
            sandbox,
            trust_sandbox,
            pipeline_hooks,
            waterfall,
        }
    }

    /// 向 TUI 发送 HITL 确认请求，等待用户按键。
    /// Sends a HITL confirmation request to the TUI, waits for user keypress.
    ///
    /// `allow_always` 为 true 时提示包含 [a] 总是授权选项（沙箱外路径提示）；
    /// 为 false 时仅 y/n（审批层 Ask 提示）。
    /// `allow_always` = true → prompt includes [a] always-authorize (sandbox path);
    /// false → y/n only (approval-tier Ask).
    async fn confirm(&self, tool_name: &str, desc: &str, allow_always: bool) -> HitlDecision {
        let _guard = WaitingGuard::new(self.waiting.clone());
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = self.tx.send(AgentEvent::HitlPrompt {
            tool: tool_name.to_string(),
            desc: desc.to_string(),
            responder: resp_tx,
            allow_always,
        });
        resp_rx.await.unwrap_or(HitlDecision::Deny)
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

/// 从 `SandboxError` 中提取要持久化的目录路径。
/// 与 `sandbox.authorize_path` 一致：取路径的父目录。
/// Extract the directory path to persist from a `SandboxError`.
/// Mirrors `sandbox.authorize_path`: takes the parent directory of the path.
fn extract_dir_from_sandbox_err(err: &crate::sandbox::SandboxError) -> String {
    use crate::sandbox::SandboxError;
    match err {
        SandboxError::OutsideSandbox { path } => {
            let expanded = crate::sandbox::expand_tilde(path);
            let p = std::path::Path::new(&expanded);
            if let Some(parent) = p.parent() {
                parent.to_string_lossy().to_string()
            } else {
                expanded
            }
        }
    }
}

impl AgentHook for HitlHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let tool_name = event.tool_name;
        let args = event.args;
        // ── pre-execute 监听器（todo 9 pre/post 层）──
        // 在沙箱/权限检查之前运行。Skip→拒绝；Rewrite→替换 args 重新派发（rig 重入
        // on_tool_call）；Run→继续下方现有逻辑。
        // pre-execute listeners (todo 9 pre/post layer), run BEFORE sandbox/permission
        // check. Skip -> deny; Rewrite -> replace args and re-dispatch (rig re-enters
        // on_tool_call); Run -> continue into existing logic below.
        let parsed_args =
            serde_json::from_str::<serde_json::Value>(args).unwrap_or(serde_json::Value::Null);
        // ── todo 11 waterfall 派发（emit + waterfall）──
        // emit 是 fire-and-forget；waterfall 顺序派发，ShortCircuit → 拒绝执行。
        // 空注册表为 no-op（waterfall 返回 Continue），不影响现有行为。
        // todo 11 waterfall dispatch (emit + waterfall). emit is fire-and-forget;
        // waterfall runs sequentially; ShortCircuit -> deny the call. Empty
        // registry is a no-op (waterfall returns Continue), preserving behavior.
        let pre_wf_event = WaterfallEvent::ToolsPreExecute {
            tool_name: tool_name.to_string(),
            args: parsed_args.clone(),
        };
        self.waterfall.emit(&pre_wf_event);
        if matches!(
            self.waterfall.waterfall(&pre_wf_event),
            WaterfallAction::ShortCircuit
        ) {
            let _ = self.tx.send(AgentEvent::Info(
                "  [waterfall] pre-execute short-circuited".into(),
            ));
            return ToolCallAction::Skip("short-circuited by waterfall listener".into());
        }
        let pre_call = PipelineCall {
            tool_name: tool_name.to_string(),
            args: parsed_args,
            role: None,
        };
        match self.pipeline_hooks.pre_execute(&pre_call) {
            PreAction::Run => {}
            PreAction::Skip(reason) => {
                let _ = self
                    .tx
                    .send(AgentEvent::Info(format!("  [pre] denied: {reason}")));
                return ToolCallAction::Skip(reason);
            }
            PreAction::Rewrite(new_args) => {
                let _ = self
                    .tx
                    .send(AgentEvent::Info("  [pre] rewrite args".into()));
                return ToolCallAction::Rewrite(new_args);
            }
        }
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
                match self.confirm(tool_name, &desc, true).await {
                    HitlDecision::Allow => {
                        self.sandbox.authorize_tool(tool_name, args);
                        let _ = self.tx.send(AgentEvent::Info(format!(
                            "  [\u{6c99}\u{7bb1}] \u{5df2}\u{6388}\u{6743}\u{8bbf}\u{95ee}: {sandbox_err}"
                        )));
                    }
                    HitlDecision::Always => {
                        // 先收集本次调用涉及的全部越界父目录（authorize_tool 之后
                        // 这些路径已通过检查，outside_parent_dirs 会返回空），
                        // 逐个持久化——只持久化第一个会让其他目录重启后再次弹窗。
                        // Collect all out-of-sandbox parent dirs BEFORE authorize_tool
                        // (afterwards they pass the check and nothing collects), then
                        // persist each — persisting only the first would re-prompt
                        // for the others after a restart.
                        let mut dirs = self.sandbox.outside_parent_dirs(tool_name, args);
                        if dirs.is_empty() {
                            dirs.push(extract_dir_from_sandbox_err(&sandbox_err));
                        }
                        self.sandbox.authorize_tool(tool_name, args);
                        let mut failed: Vec<String> = Vec::new();
                        for d in &dirs {
                            if let Err(e) = crate::config::persist_authorized_dir(d) {
                                warn!("Failed to persist authorized dir {d}: {e}");
                                failed.push(d.clone());
                            }
                        }
                        let joined = dirs.join(", ");
                        let _ = self.tx.send(AgentEvent::Info(if failed.is_empty() {
                            format!(
                                "  [\u{6c99}\u{7bb1}] \u{5df2}\u{6388}\u{6743}\u{5e76}\u{6301}\u{4e45}\u{5316}\u{76ee}\u{5f55}: {joined}"
                            )
                        } else {
                            format!(
                                "  [\u{6c99}\u{7bb1}] \u{5df2}\u{6388}\u{6743}\u{8bbf}\u{95ee}\u{ff1b}\u{6301}\u{4e45}\u{5316}\u{5931}\u{8d25}: {} \u{ff08}\u{5df2}\u{6301}\u{4e45}\u{5316}: {joined}\u{ff09}",
                                failed.join(", ")
                            )
                        }));
                    }
                    HitlDecision::Deny => {
                        let _ = self.tx.send(AgentEvent::Info(
                            "  [\u{6c99}\u{7bb1}] \u{8bbf}\u{95ee}\u{88ab}\u{62d2}\u{7edd}".into(),
                        ));
                        return ToolCallAction::Skip(format!(
                            "\u{6c99}\u{7bb1}\u{62d2}\u{7edd}\u{8bbf}\u{95ee}: {sandbox_err}"
                        ));
                    }
                }
            }
        }

        // ── 审批链检查（todo 10：从 decide_tier 升级为 ApprovalChain）──
        // Approval chain check (todo 10: upgraded from decide_tier to ApprovalChain)
        let req = ApprovalRequest {
            tool_name: tool_name.to_string(),
            args: pre_call.args.clone(),
            role: self.role.clone(),
        };
        match self.approval.request(&req) {
            ApprovalVerdict::Allow => {
                let _ = self.tx.send(AgentEvent::Info(
                    "  [HITL] \u{81ea}\u{52a8}\u{5141}\u{8bb8}".into(),
                ));
                if let Some(action) = self.maybe_run_interactive(tool_name, args).await {
                    return action;
                }
                ToolCallAction::Run
            }
            ApprovalVerdict::Deny => {
                let _ = self.tx.send(AgentEvent::Info(
                    "  [HITL] \u{5df2}\u{62d2}\u{7edd}\u{ff08}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{ff09}".into(),
                ));
                ToolCallAction::Skip(format!(
                    "\u{5de5}\u{5177} `{tool_name}` \u{88ab}\u{5f53}\u{524d}\u{89d2}\u{8272}\u{7684}\u{5b89}\u{5168}\u{7b56}\u{7565}\u{7981}\u{6b62}"
                ))
            }
            ApprovalVerdict::Ask => {
                let desc = format_tool_call_desc(tool_name, args);
                // Approval Ask is strictly y/n (allow_always = false). The [a]
                // option is hidden, so Always is unreachable here — but keep
                // the mapping total: Allow/Always → Run, Deny → Skip.
                match self.confirm(tool_name, &desc, false).await {
                    HitlDecision::Allow | HitlDecision::Always => {
                        if let Some(action) = self.maybe_run_interactive(tool_name, args).await {
                            return action;
                        }
                        ToolCallAction::Run
                    }
                    HitlDecision::Deny => ToolCallAction::Skip(format!(
                        "\u{7528}\u{6237}\u{62d2}\u{7edd}\u{4e86} `{tool_name}` \u{7684}\u{6267}\u{884c}"
                    )),
                }
            }
        }
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        let content = event.presentation.as_text().unwrap_or("").to_string();
        // ── todo 11 waterfall 派发（emit + waterfall）──
        // ShortCircuit → 停止循环。空注册表为 no-op，不影响现有行为。
        // todo 11 waterfall dispatch (emit + waterfall). ShortCircuit -> halt
        // the loop. Empty registry is a no-op, preserving existing behavior.
        let post_wf_event = WaterfallEvent::ToolsPostExecute {
            tool_name: event.tool_name.to_string(),
            result: content.clone(),
            ok: event.raw_result.is_success(),
        };
        self.waterfall.emit(&post_wf_event);
        if matches!(
            self.waterfall.waterfall(&post_wf_event),
            WaterfallAction::ShortCircuit
        ) {
            return ToolResultAction::Stop("short-circuited by waterfall listener".into());
        }
        let _ = self.tx.send(AgentEvent::ToolResult {
            name: event.tool_name.to_string(),
            result: content.clone(),
            ok: true,
        });
        // ── post-execute 监听器（todo 9 pre/post 层）──
        // 在现有通知之后运行。Keep→透传；Rewrite→替换模型可见结果；Stop→停止循环。
        // post-execute listeners (todo 9 pre/post layer), run AFTER the existing
        // notification. Keep -> passthrough; Rewrite -> replace model-visible result; Stop -> halt.
        let post_result = PipelineResult {
            tool_name: event.tool_name.to_string(),
            content,
            ok: event.raw_result.is_success(),
        };
        match self.pipeline_hooks.post_execute(&post_result) {
            PostAction::Keep => ToolResultAction::Keep,
            PostAction::Rewrite(new_content) => {
                let _ = self
                    .tx
                    .send(AgentEvent::Info("  [post] rewrite result".into()));
                ToolResultAction::rewrite(new_content)
            }
            PostAction::Stop => {
                let _ = self.tx.send(AgentEvent::Info("  [post] stop".into()));
                ToolResultAction::Stop("stopped by post-execute hook".into())
            }
        }
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
        let _ = self.tx.send(AgentEvent::TurnFinished {
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
/// 缓存：(压缩时的历史长度, 压缩后的历史)。历史未变时复用。
/// Cache: (history length at compaction, compacted history). Reused when history unchanged.
type CompactionCache = Arc<Mutex<Option<(usize, Vec<Message>)>>>;

pub struct ContextHook {
    budget: Arc<Mutex<crate::context::TokenBudget>>,
    config: crate::context::ContextConfig,
    client: crate::providers::CompletionsClient,
    model: String,
    tx: EventSender,
    /// 缓存：(压缩时的历史长度, 压缩后的历史)。历史未变时复用。
    /// Cache: (history length at compaction, compacted history). Reused when history unchanged.
    compaction_cache: CompactionCache,
    /// SSE 重试时用于捕获对话历史的共享 Arc（None = 不捕获）。
    /// Shared Arc for capturing conversation history on SSE retry (None = no capture).
    history_capture: Option<Arc<Mutex<Vec<Message>>>>,
    /// SSE 重试时用于捕获当前轮次的共享 Arc。
    /// Shared Arc for capturing the current turn number on SSE retry.
    turn_capture: Option<Arc<Mutex<usize>>>,
    /// 自主循环轮数上限，用于轮次预算提醒。
    /// Max turns for the autonomous loop, used for turn-budget awareness.
    max_turns: usize,
    /// todo 11: waterfall 注册表，派发 AgentRequest 事件（fire-and-forget）。
    /// todo 11: waterfall registry, dispatches AgentRequest events (fire-and-forget).
    waterfall: Arc<WaterfallRegistry>,
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
        waterfall: Arc<WaterfallRegistry>,
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
            waterfall,
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
            "[\u{7cfb}\u{7edf}\u{63d0}\u{793a}] \u{4f60}\u{5df2}\u{4f7f}\u{7528} {}/{} \u{8f6e}\u{ff0c}\u{5269}\u{4f59} {} \u{8f6e}\u{3002}\u{8bf7}\u{4f18}\u{5148}\u{6536}\u{655b}\u{5230}\u{7ed3}\u{8bba}\u{ff0c}\u{907f}\u{514d}\u{8fc7}\u{5ea6}\u{63a2}\u{7d22}\u{3002}\u{4e0d}\u{8981}\u{5411}\u{7528}\u{6237}\u{63d0}\u{95ee}\u{ff0c}\u{9047}\u{5230}\u{56f0}\u{96be}\u{81ea}\u{884c}\u{5224}\u{65ad}\u{5e76}\u{7ee7}\u{7eed}\u{3002}\n\
             [System] {}/{} turns used, {} remaining. Prioritize converging to a conclusion, avoid excessive exploration. Do not ask the user questions — make your own judgment and continue.",
            turn, self.max_turns, remaining, turn, self.max_turns, remaining,
        ))
    }

    /// 检测溢出并在必要时压缩历史，返回 `CompletionCallAction::Patch` 或 `CompletionCallAction::Continue`。
    /// Detect overflow and compact if needed, returning `CompletionCallAction::Patch` or `CompletionCallAction::Continue`.
    async fn handle_completion_call(
        &self,
        history: &[Message],
        prompt: &Message,
        _turn: usize,
    ) -> CompletionCallAction {
        // todo 11: fire-and-forget AgentRequest 事件（空注册表为 no-op）。
        // todo 11: fire-and-forget AgentRequest event (empty registry is a no-op).
        self.waterfall.emit(&WaterfallEvent::AgentRequest {
            message: format!("turn={_turn}"),
        });
        // 先清洗历史中的非法工具调用参数（截断的流会留下非 JSON 的
        // `function.arguments`，严格供应商会 400 拒绝）；有改动时本轮请求
        // 改用清洗后的历史（PatchRequest 非粘性，不动 rig 内部真实历史）。
        // Sanitize invalid tool-call arguments first (truncated streams leave
        // non-JSON `function.arguments` that strict providers reject with 400);
        // when changed, this request uses the cleaned history (PatchRequest is
        // non-sticky — rig's internal transcript is untouched).
        let (sanitized_history, sanitized) = sanitize_history_tool_calls(history);
        let history: &[Message] = if sanitized {
            &sanitized_history
        } else {
            history
        };
        if let Some(hc) = &self.history_capture {
            // 补上当前 prompt（工具循环里就是上一轮的 tool 结果）。rig 的
            // CompletionCall.history 不含当前 prompt，只捕获它会得到一条以未应答
            // assistant tool_call 结尾的历史，重新注入新 runner 时被 400 拒绝。
            let mut full = history.to_vec();
            full.push(prompt.clone());
            *hc.lock().unwrap() = full;
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
            if sanitized {
                return CompletionCallAction::Patch(
                    RequestPatch::new().history(history.to_vec()),
                );
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
        let (old, recent) = crate::context::select_head_tail(
            &base_history,
            self.config.keep_recent_turns,
            tail_budget,
        );
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

        let summary_msg =
            Message::system(format!("[对话历史摘要 / Conversation Summary]\n{summary}"));
        let continue_msg = Message::system(
            "Continue executing the task based on the summary above. Do not stop to ask questions — make your best judgment and proceed.\n\
             基于上方摘要继续执行任务，不要停下来提问，自行判断并推进。",
        );
        let compacted: Vec<Message> = std::iter::once(summary_msg)
            .chain(recent.iter().cloned())
            .chain(std::iter::once(continue_msg))
            .collect();

        let new_tokens = crate::context::estimate_history_tokens(&compacted);

        // 缓存结果。
        // Cache the result.
        *self.compaction_cache.lock().unwrap() = Some((history.len(), compacted.clone()));

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
            .temperature(crate::providers::Provider::clamp_temperature(
                0.0,
                &self.model,
            ))
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

/// 清洗待发送历史中的工具调用参数，返回 (历史, 是否有改动)。rig 流式聚合遇到
/// 截断/非对象参数时会把原始串保留为 `Value::String`（或 `Value::Null`），重发时
/// 序列化成非法的 `function.arguments`，严格供应商（如 DashScope code 模型）400
/// 拒绝整个请求。可解析的字符串还原为 JSON 值，其余替换为空对象 `{}`。
/// Sanitize tool-call arguments in outbound history. rig's streaming accumulator
/// keeps truncated/non-object arguments as `Value::String`/`Value::Null`, which
/// re-serialize into an invalid `function.arguments` and strict providers reject the
/// whole request with 400. Parseable strings are restored; the rest become `{}`.
fn sanitize_history_tool_calls(history: &[Message]) -> (Vec<Message>, bool) {
    let mut changed = false;
    let sanitized: Vec<Message> = history
        .iter()
        .map(|msg| {
            let Message::Assistant { id, content } = msg else {
                return msg.clone();
            };
            let mut msg_changed = false;
            let items: Vec<AssistantContent> = content
                .iter()
                .map(|item| match item {
                    AssistantContent::ToolCall(tc) => {
                        let fixed = sanitize_tool_arguments(&tc.function.arguments);
                        if fixed == tc.function.arguments {
                            item.clone()
                        } else {
                            msg_changed = true;
                            AssistantContent::ToolCall(MessageToolCall {
                                function: ToolFunction {
                                    arguments: fixed,
                                    ..tc.function.clone()
                                },
                                ..tc.clone()
                            })
                        }
                    }
                    other => other.clone(),
                })
                .collect();
            if !msg_changed {
                return msg.clone();
            }
            changed = true;
            Message::Assistant {
                id: id.clone(),
                content: items,
            }
        })
        .collect();
    (sanitized, changed)
}

fn sanitize_tool_arguments(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return serde_json::json!({});
            }
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(parsed @ serde_json::Value::Object(_)) => parsed,
                // 合法 JSON 但非对象（工具参数必须是对象），或截断导致的非法 JSON。
                // Valid JSON but not an object (tool args must be objects), or
                // invalid JSON from a truncated stream.
                _ => serde_json::json!({}),
            }
        }
        serde_json::Value::Null => serde_json::json!({}),
        other => other.clone(),
    }
}

impl AgentHook for ContextHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        self.handle_completion_call(event.history, event.prompt, event.turn)
            .await
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
            // 对多行 old/new 做缩进并截断，使代码块在 TUI 中更易读
            // Indent and cap multi-line old/new for TUI readability
            let fmt_block = |s: &str| -> String {
                let lines: Vec<&str> = s.lines().collect();
                if lines.is_empty() {
                    return String::new();
                }
                if lines.len() == 1 {
                    return lines[0].to_string();
                }
                const MAX: usize = 10;
                let head = &lines[..lines.len().min(MAX)];
                let mut out = format!("\n    {}", head.join("\n    "));
                if lines.len() > MAX {
                    out.push_str(&format!("\n    \u{2026}({})", lines.len()));
                }
                out
            };
            // 批量编辑形式检测：edits 数组 → 仅预览第一对
            // 多编辑 diff 渲染为未来工作（见 parse_file_edit），此处仅文字预览。
            // Detect multi-edit form: edits array → preview first pair only.
            // Multi-edit diff rendering is future work (see parse_file_edit);
            // here we only show a text preview of the first pair.
            let edits_arr = parsed
                .as_ref()
                .and_then(|v| v.get("edits"))
                .and_then(|v| v.as_array());
            if let Some(arr) = edits_arr {
                let n = arr.len();
                let (old0, new0) = arr
                    .first()
                    .and_then(|p| {
                        let o = p.get("old").and_then(|v| v.as_str()).unwrap_or("");
                        let nw = p.get("new").and_then(|v| v.as_str()).unwrap_or("");
                        Some((o, nw))
                    })
                    .unwrap_or(("", ""));
                format!(
                    "edit_file \u{2192} \u{7f16}\u{8f91}\u{6587}\u{4ef6}: {path}\u{ff08}{n} \u{5904}\u{66ff}\u{6362} / edits\u{ff09}\n  \u{66ff}\u{6362}: {}\n  \u{66ff}\u{6362}\u{4e3a}: {}",
                    fmt_block(old0),
                    fmt_block(new0),
                )
            } else {
                let old = get_str("old").unwrap_or_default();
                let new = get_str("new").unwrap_or_default();
                format!(
                    "edit_file \u{2192} \u{7f16}\u{8f91}\u{6587}\u{4ef6}: {path}\n  \u{66ff}\u{6362}: {}\n  \u{66ff}\u{6362}\u{4e3a}: {}",
                    fmt_block(&old),
                    fmt_block(&new),
                )
            }
        }
        "write_file" => {
            let path = get_str("path").unwrap_or_default();
            let content_len = parsed
                .as_ref()
                .and_then(|v| v.get("content"))
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!(
                "write_file \u{2192} \u{5199}\u{5165}\u{6587}\u{4ef6}: {path} ({content_len} \u{5b57}\u{8282})"
            )
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

/// 从工具调用参数中解析 `edit_file` 的结构化编辑载荷。
/// 仅当 `tool_name == "edit_file"` 且 JSON 含 `path`/`old`/`new` 键时返回 Some；
/// 任何解析失败或缺失键 → None（退回纯文本 desc 渲染，绝不 panic）。
///
/// 批量编辑形式（`edits` 数组）返回 None —— 多编辑 unified diff 渲染为未来工作，
/// 退回纯文本 desc 渲染（`format_tool_call_desc` 已预览第一对）。
///
/// Parse the structured edit payload from tool-call args.
/// Returns Some only when `tool_name == "edit_file"` and the JSON contains
/// `path`/`old`/`new`; any parse failure or missing key → None (falls back
/// to the plain-text desc rendering, never panics).
///
/// Multi-edit form (`edits` array) returns None — multi-edit unified diff
/// rendering is future work; falls back to the plain-text desc rendering
/// (`format_tool_call_desc` already previews the first pair).
fn parse_file_edit(tool_name: &str, args: &str) -> Option<Box<FileEdit>> {
    if tool_name != "edit_file" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    // 批量编辑形式（edits 数组）不解析为 FileEdit —— 多编辑 diff 渲染为未来工作。
    // Multi-edit form (edits array) does not parse into FileEdit —
    // multi-edit diff rendering is future work.
    if parsed.get("edits").is_some() {
        return None;
    }
    let get = |k: &str| {
        parsed.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
    };
    Some(Box::new(FileEdit {
        path: get("path")?,
        old: get("old")?,
        new: get("new")?,
    }))
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
#[allow(dead_code)] // used in tests
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
                .and_then(|v| {
                    v.get("command")
                        .and_then(|c| c.as_str())
                        .map(str::to_string)
                })
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
        Permission::Deny => ToolCallAction::Skip(format!(
            "tool `{tool_name}` is denied by policy for this role"
        )),
        Permission::Ask => ToolCallAction::Skip(format!("user declined to run `{tool_name}`")),
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
#[allow(clippy::too_many_arguments)]
/// 自主循环共享内核：内置角色和自定义子代理共用此函数。
/// Shared autonomous-loop core: built-in roles and custom sub-agents share this function.
///
/// `run_autonomous(role)` 和 `run_autonomous_spec(spec)` 都委托至此。
/// Both `run_autonomous(role)` and `run_autonomous_spec(spec)` delegate here.
#[allow(clippy::too_many_arguments)]
async fn run_autonomous_inner(
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    trust_sandbox: Arc<AtomicBool>,
    spec: &crate::registry::AgentSpec,
    task_ctx: Option<crate::subagent::SubagentCtx>,
    todo_ctx: Option<crate::tools::TodoContext>,
    goal: &str,
    tx: &EventSender,
    shared_history: Arc<Mutex<Vec<Message>>>,
    shared_waterfall: Option<Arc<WaterfallRegistry>>,
    pre_step: Option<Arc<PreStepState>>,
) -> anyhow::Result<String> {
    const MAX_RETRIES: usize = 3;

    // todo 12: dispatch AgentPreStep via shared waterfall (if provided).
    // Serial listeners (InvestigatorListener, PlannerListener) fire here,
    // before the retry loop starts. They may set escape hatch or goal_override.
    let goal_owned: Option<String> = if let (Some(wf), Some(ps)) = (&shared_waterfall, &pre_step) {
        let event = WaterfallEvent::AgentPreStep {
            role: spec.name.clone(),
            goal: goal.to_string(),
        };
        wf.emit(&event);
        wf.serial(&event).await;
        if ps.escape.load(Ordering::Relaxed) {
            return Ok(ps.investigation.lock().unwrap().clone().unwrap_or_default());
        }
        if let Some(err) = ps.error.lock().unwrap().take() {
            return Err(anyhow::anyhow!(err));
        }
        ps.goal_override.lock().unwrap().clone()
    } else {
        None
    };
    let goal: &str = goal_owned.as_deref().unwrap_or(goal);

    let captured_history = shared_history;
    let captured_turn: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let mut turns_used: usize = 0;
    let total_max_turns = spec.max_turns.unwrap_or_else(|| registry.max_turns());

    // SessionLog: append-only 旁路日志，与 ContextHook history capture 并存。
    // 追加失败不影响 model-visible 行为（仅 warn），SessionLog 是侧信道。
    let session_dir = crate::config::config()
        .map(|c| c.memory.dir.clone())
        .unwrap_or_else(|| std::path::PathBuf::from("memory"));
    let session_id = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{nanos}", spec.name)
    };
    let mut session_log = crate::session_log::SessionLog::new(session_id, session_dir);
    if let Err(e) = session_log.append(crate::session_log::SessionEvent::UserMessage {
        content: goal.to_string(),
    }) {
        warn!(error = %e, "session_log append user_message failed");
    }

    for attempt in 0..=MAX_RETRIES {
        let max_turns_remaining = total_max_turns.saturating_sub(turns_used);
        if max_turns_remaining == 0 {
            return Err(anyhow::anyhow!(
                "{} \u{8f6e}\u{6b21}\u{5df2}\u{7528}\u{5c3d}\u{ff08}{turns_used}/{total_max_turns}\u{ff09}",
                spec.name
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

        let perms = spec.permissions.clone();
        let mut approval = ApprovalChain::new();
        approval.add(Box::new(DefaultApproval::new(perms)));
        let hitl_waiting = Arc::new(AtomicBool::new(false));
        // todo 11: 每个自主循环创建独立的 WaterfallRegistry（按任务隔离监听器）。
        // todo 11: each autonomous loop gets its own WaterfallRegistry (per-task isolation).
        let waterfall = Arc::new(WaterfallRegistry::new());
        let hook = HitlHook::new(
            Arc::new(approval),
            spec.name.clone(),
            hitl_waiting.clone(),
            tx.clone(),
            sandbox.clone(),
            trust_sandbox.clone(),
            std::sync::Arc::new(crate::tools::pipeline::PipelineHooks::new()),
            waterfall.clone(),
        );

        let model = registry
            .session_model()
            .or_else(|| spec.model.clone())
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
            waterfall.clone(),
        )
        .with_history_capture(captured_history.clone(), captured_turn.clone());

        let agent = build_runner_agent_spec(
            registry,
            spec,
            task_ctx.clone(),
            todo_ctx.clone(),
        )?;
        let prior_history = crate::context::repair_orphan_tool_calls(
            captured_history.lock().unwrap().clone(),
        );
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

        match consume_stream(stream, Some(hitl_waiting), sse_idle_timeout(), tx).await {
            Ok(output) => {
                if let Err(e) =
                    session_log.append(crate::session_log::SessionEvent::AssistantMessage {
                        content: output.clone(),
                    })
                {
                    warn!(error = %e, "session_log append assistant_message failed");
                }
                return Ok(output);
            }
            Err(e) if is_context_overflow_error(&e) && attempt < MAX_RETRIES => {
                let mut hist = captured_history.lock().unwrap();
                let old_len = hist.len();
                if old_len > 6 {
                    let keep = 4;
                    let retained: Vec<Message> = hist[old_len - keep..].to_vec();
                    *hist = std::iter::once(Message::system(
                        "[反应式压缩 / Reactive compaction] 上下文溢出，旧对话历史已截断。请基于保留的近期消息继续，不要停下来提问。\n\
                         [System] Context overflow — old history truncated. Continue from the retained recent messages. Do not stop to ask questions."
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
                    agent_name = %spec.name,
                    turns_used,
                    total_max_turns,
                    captured_history_len = hist_len,
                    model = %model_for_log,
                    "SSE disconnect, retrying with preserved history"
                );
                let _ = tx.send(AgentEvent::Info(format!(
                    "[重试 / Retry] {} 第 {}/{} 次：SSE 连接中断。\n  · 已用轮数: {turns_used}/{total_max_turns}（剩余 {max_turns_remaining} 轮）\n  · 保留历史: {hist_len} 条消息\n  · 使用模型: {model_for_log}\n  · 错误摘要: {err_snippet}",
                    spec.name,
                    attempt + 1,
                    MAX_RETRIES
                )));
                continue;
            }
            Err(e) => {
                let err_snippet: String = e.to_string().chars().take(300).collect();
                warn!(error = %e, error_debug = ?e, agent_name = %spec.name, "stream error (non-retryable)");
                let _ = tx.send(AgentEvent::Error(format!(
                    "流错误（不可重试 / Non-retryable stream error）\n  · 角色: {}\n  · 使用模型: {model_for_log}\n  · 已用轮数: {turns_used}/{total_max_turns}\n  · 错误详情: {err_snippet}",
                    spec.name
                )));
                return Err(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "{} 重试 {MAX_RETRIES} 次后仍失败（SSE 连接反复中断）。\n  · 已用轮数: {turns_used}/{total_max_turns}\n  · 保留历史: {} 条消息\n建议检查网络或 API 稳定性后重试。\n\
         [System] {agent_name} failed after {MAX_RETRIES} retries (repeated SSE disconnects). Turns used: {turns_used}/{total_max_turns}. Check network/API stability and try again.",
        spec.name,
        captured_history.lock().unwrap().len(),
        agent_name = spec.name,
    ))
}

/// 自主循环入口（角色路径）：`role → agent_spec → run_autonomous_inner`。
/// Autonomous loop entry (role path): `role → agent_spec → run_autonomous_inner`.
#[allow(clippy::too_many_arguments)]
pub async fn run_autonomous(
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    trust_sandbox: Arc<AtomicBool>,
    role: Role,
    goal: &str,
    tx: &EventSender,
    shared_history: Arc<Mutex<Vec<Message>>>,
    shared_waterfall: Option<Arc<WaterfallRegistry>>,
    pre_step: Option<Arc<PreStepState>>,
) -> anyhow::Result<String> {
    let spec = registry.agent_spec(role);
    run_autonomous_inner(
        registry,
        sandbox,
        trust_sandbox,
        &spec,
        registry.task_ctx_for_role(role),
        registry.todo_ctx_for_role(role),
        goal,
        tx,
        shared_history,
        shared_waterfall,
        pre_step,
    )
    .await
}

/// 自主循环入口（spec 路径）：自定义子代理通过 `run_autonomous_spec` 进入。
/// Autonomous loop entry (spec path): custom sub-agents enter via `run_autonomous_spec`.
///
/// 子代理隔离：`task_ctx` / `todo_ctx` 均为 None（无 task 工具、无 todo_write）。
/// Sub-agent isolation: `task_ctx` / `todo_ctx` are both None (no task tool, no todo_write).
#[allow(clippy::too_many_arguments)]
pub async fn run_autonomous_spec(
    registry: &AgentRegistry,
    sandbox: &Sandbox,
    trust_sandbox: Arc<AtomicBool>,
    spec: crate::registry::AgentSpec,
    goal: &str,
    tx: &EventSender,
    shared_history: Arc<Mutex<Vec<Message>>>,
    shared_waterfall: Option<Arc<WaterfallRegistry>>,
    pre_step: Option<Arc<PreStepState>>,
) -> anyhow::Result<String> {
    run_autonomous_inner(
        registry,
        sandbox,
        trust_sandbox,
        &spec,
        None,
        None,
        goal,
        tx,
        shared_history,
        shared_waterfall,
        pre_step,
    )
    .await
}

/// 从配置解析 SSE 空闲超时 Duration。
/// Resolve the SSE idle timeout Duration from config.
///
/// 读取 `[context].sse_idle_timeout_secs`（默认 300 秒）。
/// 设为 0 时返回一个极长超时（实质禁用空闲检测，不推荐但可用）。
/// Reads `[context].sse_idle_timeout_secs` (default 300s).
/// When set to 0, returns a very long duration (effectively disabling idle
/// detection — not recommended but available).
pub fn sse_idle_timeout() -> Duration {
    let secs = crate::config::config()
        .map(|c| c.context.sse_idle_timeout_secs)
        .unwrap_or(300);
    if secs == 0 {
        Duration::from_secs(86_400 * 365) // 1 year — effectively disabled
    } else {
        Duration::from_secs(secs)
    }
}

/// 判断错误是否为 SSE 流式断连（可安全重试）。
/// Determines whether an error is an SSE stream disconnect (safe to retry).
pub fn is_stream_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    if msg.contains("MaxTurnsError") || msg.contains("max turns limit") {
        return false;
    }
    // HTTP 4xx 是确定性客户端错误（不支持的参数、非法历史等）：同一请求重试
    // 必然同样失败，不消耗重试次数。上下文溢出类 400 由调用方先经
    // is_context_overflow_error 分支拦截，不受影响。
    // HTTP 4xx are deterministic client errors (unsupported parameter, invalid
    // history, etc.): retrying the identical request fails identically, so don't
    // burn retries. Overflow-style 400s are intercepted by the caller's
    // is_context_overflow_error branch first and stay retryable.
    if msg.contains("Invalid status code 4") {
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
pub async fn consume_stream(
    mut stream: StreamingResult,
    hitl_waiting: Option<Arc<AtomicBool>>,
    idle_timeout: Duration,
    tx: &EventSender,
) -> anyhow::Result<String> {
    use MultiTurnStreamItem;
    use rig_core::streaming::StreamedAssistantContent;

    let chunk_timeout = idle_timeout;

    let mut output = String::new();
    let mut all_reasoning = String::new();

    loop {
        // When a HITL prompt is active the stream is blocked inside the hook's
        // `confirm()` future awaiting the user's keypress.  The `hitl_waiting`
        // flag is set *during* `stream.next()` (inside `confirm()`), so it may
        // be false on entry but become true mid-call.  If the idle timeout
        // timeout fires while HITL is active the stream future is dropped,
        // which cancels `confirm()` and orphans the oneshot responder still
        // held by the TUI.  To avoid this we retry without a timeout when the
        // flag is set.
        // 当 HITL 确认处于活动状态时，流被阻塞在 hook 的 `confirm()` future 中
        // 等待用户按键。`hitl_waiting` 标志是在 `stream.next()` 期间（在
        // `confirm()` 内部）设置的，因此进入时可能为 false 但中途变为 true。
        // 如果空闲超时在 HITL 活动期间触发，流 future 被丢弃，会取消
        // `confirm()` 并使 TUI 仍持有的 oneshot responder 成为孤儿。为避免
        // 此问题，当标志被设置时无超时重试。
        let next = match &hitl_waiting {
            Some(flag) if flag.load(Ordering::Relaxed) => stream.next().await,
            _ => match tokio::time::timeout(chunk_timeout, stream.next()).await {
                Ok(item) => item,
                Err(_) => {
                    // Check whether HITL started during the timed wait.
                    // 检查 HITL 是否在超时等待期间开始。
                    if let Some(flag) = &hitl_waiting
                        && flag.load(Ordering::Relaxed)
                    {
                        // HITL became active mid-timeout — retry without
                        // a deadline so the user has unlimited time to
                        // respond.
                        // HITL 在超时期间变为活动——无截止时间重试，
                        // 让用户有无限时间响应。
                        continue;
                    }
                    warn!(
                        timeout_secs = chunk_timeout.as_secs(),
                        accumulated_output_chars = output.len(),
                        accumulated_reasoning_chars = all_reasoning.len(),
                        "SSE idle timeout, connection may have dropped"
                    );
                    return Err(anyhow::anyhow!(
                        "SSE 空闲超时 / SSE idle timeout（{} 秒无数据），连接可能已断开。\n  · 已收到: {} 字符文本、{} 字符推理内容\n  · 上层会自动重试（如果是可重试错误）\n\
                         [System] SSE idle timeout (no data for {}s), connection may have dropped. \
                         Received {} chars text, {} chars reasoning. Upper layer will auto-retry if applicable.",
                        chunk_timeout.as_secs(),
                        output.len(),
                        all_reasoning.len(),
                        chunk_timeout.as_secs(),
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
                    // edit_file 时尝试解析结构化载荷用于 unified diff 渲染；
                    // 解析失败或非 edit_file → None，退回 desc 纯文本渲染。
                    // For edit_file, try to parse structured args for unified
                    // diff rendering; parse failure or non-edit_file → None,
                    // falling back to the plain-text desc rendering.
                    let diff = parse_file_edit(&tool_call.function.name, &tool_call.function.arguments.to_string());
                    let _ = tx.send(AgentEvent::ToolCall {
                        name: tool_call.function.name.clone(),
                        desc,
                        diff,
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

/// 从 `AgentSpec` 构建"可运行"的 rig `Agent`（带工具）。
/// Builds a "runnable" rig `Agent` (with tools) from an `AgentSpec`.
///
/// 泛化入口：内置角色和自定义子代理共用此函数。`task_ctx` / `todo_ctx`
/// 由调用方传入——内置角色路径从 registry 按 role 获取，子代理路径传 None。
///
/// Generalized entry: built-in roles and custom sub-agents share this function.
/// `task_ctx` / `todo_ctx` are passed by the caller — the Role-based path gets
/// them from the registry per role; the subagent path passes None.
fn build_runner_agent_spec(
    registry: &AgentRegistry,
    spec: &crate::registry::AgentSpec,
    task_ctx: Option<crate::subagent::SubagentCtx>,
    todo_ctx: Option<crate::tools::TodoContext>,
) -> anyhow::Result<Agent> {
    let client = registry.create_client()?;
    // preamble：文件优先；内置角色有内嵌回退，自定义子代理无。
    // Preamble: file first; built-in roles have embedded fallback, custom don't.
    let preamble = match std::fs::read_to_string(&spec.preamble_path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!(
                path = %spec.preamble_path,
                error = %e,
                "preamble file not found, using embedded fallback if available"
            );
            spec.embedded_preamble
                .map(|s| s.to_string())
                .unwrap_or_default()
        }
    };
    let preamble = crate::registry::inject_skills_public(&preamble);
    let model = registry
        .session_model()
        .or_else(|| spec.model.clone())
        .unwrap_or_else(|| registry.effective_model());
    let max_turns = spec.max_turns.unwrap_or_else(|| registry.max_turns());
    info!("[runner] agent={} model={model} max_turns={max_turns}", spec.name);
    let params = crate::providers::provider_additional_params();
    let max_output = registry.context_config().max_output_tokens as u64;
    let reasoning = crate::providers::is_reasoning_model(&model);
    let effective_max_tokens: Option<u64> = if reasoning {
        None
    } else if max_output > 0 {
        Some(max_output)
    } else {
        None
    };
    if reasoning {
        info!("[runner] reasoning model detected, skipping max_tokens (model default output budget)");
    } else if effective_max_tokens.is_none() {
        info!("[runner] non-reasoning model, skipping max_tokens (model default output budget; set [context].max_output_tokens>0 to cap)");
    }
    let builder = client
        .agent(&model)
        .preamble(&preamble)
        .temperature(crate::providers::Provider::clamp_temperature(0.7, &model));
    let sandbox_provider: Arc<dyn crate::seam::SandboxProvider> = registry.sandbox();
    let deps = crate::tools::ToolDeps {
        sandbox: sandbox_provider.clone(),
        todo_ctx,
        task_ctx,
        task_registry: registry.clone(),
        shells: Arc::new(crate::shell::LazyShell::new(
            sandbox_provider.clone(),
            registry.context_config().max_bash_output_chars,
        )),
        bg: registry.bg(),
        checkpoints: registry.checkpoints(),
        scheduler_mgr: registry.scheduler_mgr(),
    };
    let builder =
        crate::tools::add_builtin_tools(builder, registry.context_config(), &deps)
            .additional_params(params)
            .default_max_turns(max_turns);
    let agent = if let Some(v) = effective_max_tokens {
        builder.max_tokens(v).build()
    } else {
        builder.build()
    };
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextConfig;
    use crate::providers::CompletionsClient;

    fn perms() -> ToolPerms {
        ToolPerms {
            read_file: Permission::Allow,
            run_bash_readonly: Permission::Allow,
            run_bash_mutating: Permission::Ask,
            edit_file: Permission::Ask,
            write_file: Permission::Ask,
            web_fetch: Permission::Allow,
            web_search: Permission::Allow,
            command_rules: Vec::new(),
        }
    }

    fn assistant_tool_call_msg(arguments: serde_json::Value) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::tool_call("call_1", "read_file", arguments)],
        }
    }

    fn tool_call_arguments(msg: &Message) -> &serde_json::Value {
        match msg {
            Message::Assistant { content, .. } => match content.iter().next() {
                Some(AssistantContent::ToolCall(tc)) => &tc.function.arguments,
                _ => panic!("expected tool call content"),
            },
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn sanitize_repairs_truncated_stream_arguments() {
        // 流式中断留下的半截 JSON：重发会被严格供应商 400 拒绝，必须替换为空对象。
        // Truncated-stream partial JSON must be replaced: strict providers 400 on it.
        let history = vec![assistant_tool_call_msg(serde_json::Value::String(
            "{\"path\": \"src/".to_string(),
        ))];
        let (out, changed) = sanitize_history_tool_calls(&history);
        assert!(changed);
        assert_eq!(tool_call_arguments(&out[0]), &serde_json::json!({}));
    }

    #[test]
    fn sanitize_restores_stringified_object_arguments() {
        let history = vec![assistant_tool_call_msg(serde_json::Value::String(
            "{\"path\":\"src/main.rs\"}".to_string(),
        ))];
        let (out, changed) = sanitize_history_tool_calls(&history);
        assert!(changed);
        assert_eq!(
            tool_call_arguments(&out[0]),
            &serde_json::json!({"path": "src/main.rs"})
        );
    }

    #[test]
    fn sanitize_rewrites_null_arguments_to_empty_object() {
        let history = vec![assistant_tool_call_msg(serde_json::Value::Null)];
        let (out, changed) = sanitize_history_tool_calls(&history);
        assert!(changed);
        assert_eq!(tool_call_arguments(&out[0]), &serde_json::json!({}));
    }

    #[test]
    fn sanitize_leaves_valid_history_untouched() {
        let history = vec![
            Message::system("sys"),
            assistant_tool_call_msg(serde_json::json!({"path": "x"})),
        ];
        let (out, changed) = sanitize_history_tool_calls(&history);
        assert!(!changed);
        assert_eq!(out, history);
    }

    #[test]
    fn stream_error_classifier_rejects_http_4xx() {
        // litellm 兜底链耗尽时把 400 包装成 429：确定性失败，不应再当 SSE 断连重试。
        // litellm wraps fallback-chain 400s as 429: deterministic, must not retry.
        let wrapped = anyhow::anyhow!(
            "流式错误 / Stream error: CompletionError: HttpError: Invalid status code 429 Too Many Requests"
        );
        assert!(!is_stream_error(&wrapped));
        let bad_request = anyhow::anyhow!(
            "流式错误 / Stream error: CompletionError: HttpError: Invalid status code 400 Bad Request"
        );
        assert!(!is_stream_error(&bad_request));
        let network = anyhow::anyhow!("流式错误 / Stream error: error decoding response body");
        assert!(is_stream_error(&network));
        let idle = anyhow::anyhow!("流式错误 / Stream error: 空闲超时");
        assert!(is_stream_error(&idle));
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

    // ── parse_file_edit 形式分发测试 ──
    // ── parse_file_edit form-dispatch tests ──

    /// 单次形式 `{path, old, new}` 应解析为 Some(FileEdit)。
    /// Single form `{path, old, new}` should parse to Some(FileEdit).
    #[test]
    fn parse_file_edit_single_returns_some() {
        let fe = parse_file_edit("edit_file", r#"{"path":"src/main.rs","old":"a","new":"b"}"#);
        assert!(fe.is_some(), "single form should return Some");
        let fe = fe.unwrap();
        assert_eq!(fe.path, "src/main.rs");
        assert_eq!(fe.old, "a");
        assert_eq!(fe.new, "b");
    }

    /// 批量形式 `{path, edits: [...]}` 应返回 None（退回纯文本 desc 渲染）。
    /// Multi form `{path, edits: [...]}` should return None
    /// (falls back to plain-text desc rendering; multi-edit diff is future work).
    #[test]
    fn parse_file_edit_multi_returns_none() {
        let args = r#"{"path":"src/main.rs","edits":[{"old":"a","new":"b"},{"old":"c","new":"d"}]}"#;
        assert!(
            parse_file_edit("edit_file", args).is_none(),
            "multi form should return None (diff rendering is future work)"
        );
    }

    #[test]
    fn overflow_error_detected() {
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "context_length_exceeded"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "prompt_too_long"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "maximum context window exceeded"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "token limit reached"
        )));
        assert!(is_context_overflow_error(&anyhow::anyhow!(
            "上下文超出限制"
        )));
    }

    #[test]
    fn non_overflow_error_not_detected() {
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "network timeout"
        )));
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "permission denied"
        )));
        assert!(!is_context_overflow_error(&anyhow::anyhow!(
            "file not found"
        )));
    }

    // ── ContextHook history-capture characterization ──
    // Characterization test: locks existing history/turn capture behavior that
    // run_autonomous relies on. MUST stay green before AND after the SessionLog
    // append wiring (append is additive, must not change model-visible behavior).

    /// Build a dummy client. `handle_completion_call` only touches the client in
    /// the compaction path, which is unreachable with a tiny history, so a
    /// placeholder key/URL is safe here.
    fn dummy_client() -> CompletionsClient {
        let http = reqwest::Client::builder()
            .build()
            .expect("reqwest client build");
        let http = crate::http_trace::TracingHttpClient::new(http).expect("tracing client build");
        crate::providers::openai::CompletionsClient::builder()
            .api_key("dummy-key".to_string())
            .base_url("http://localhost:1")
            .http_client(http)
            .build()
            .expect("completions client build")
    }

    #[tokio::test]
    async fn context_hook_captures_history_and_turn_baseline() {
        // Given: a ContextHook with history capture enabled and a tiny history
        // (well below any overflow threshold → no LLM compaction path).
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let history_arc: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
        let turn_arc: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let hook = ContextHook::new(
            128_000,
            ContextConfig::default(),
            dummy_client(),
            "test-model".to_string(),
            tx,
            30,
            Arc::new(crate::events::WaterfallRegistry::new()),
        )
        .with_history_capture(history_arc.clone(), turn_arc.clone());

        let history = vec![Message::user("hello"), Message::assistant("hi there")];
        let prompt = Message::user("latest tool result");

        // When: handle_completion_call runs at turn 2 with this history.
        let action = hook.handle_completion_call(&history, &prompt, 2).await;

        // Then: the shared Arcs capture history + prompt and the turn, and a
        // small history yields Continue (no compaction, no turn reminder).
        let captured = history_arc.lock().unwrap().clone();
        assert_eq!(captured.len(), 3);
        assert!(matches!(captured[0], Message::User { .. }));
        assert!(matches!(captured[1], Message::Assistant { .. }));
        assert_eq!(captured[2], prompt);
        assert_eq!(*turn_arc.lock().unwrap(), 2);
        assert!(
            matches!(action, CompletionCallAction::Continue),
            "small history must not trigger compaction or a turn reminder"
        );
    }

    #[tokio::test]
    async fn context_hook_captures_prompt_so_history_has_no_orphan_tool_call() {
        // rig's CompletionCall.history excludes the current prompt; in the tool
        // loop that prompt is the latest tool result. Capturing only `history`
        // yields a transcript ending in an unanswered assistant tool_call, which
        // providers reject when the history is re-seeded. The hook must capture
        // history + prompt so the transcript stays complete.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let history_arc: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
        let turn_arc: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let hook = ContextHook::new(
            128_000,
            ContextConfig::default(),
            dummy_client(),
            "test-model".to_string(),
            tx,
            30,
            Arc::new(crate::events::WaterfallRegistry::new()),
        )
        .with_history_capture(history_arc.clone(), turn_arc.clone());

        let history = vec![
            Message::user("goal"),
            assistant_tool_call_msg(serde_json::json!({"command": "ls"})),
        ];
        let prompt = Message::tool_result("call_1", "tool", "output");

        let _ = hook.handle_completion_call(&history, &prompt, 2).await;

        let captured = history_arc.lock().unwrap().clone();
        assert_eq!(captured.len(), 3, "capture must include the prompt");
        assert_eq!(
            crate::context::repair_orphan_tool_calls(captured.clone()),
            captured,
            "captured history must be orphan-free"
        );
    }

    // ── todo 11: ContextHook dispatches AgentRequest via WaterfallRegistry ──
    // Characterization: a registered emit listener fires on handle_completion_call.
    // Empty registry behavior unchanged (baseline test above). This proves the
    // wiring is live without changing model-visible behavior.

    #[tokio::test]
    async fn context_hook_dispatches_agent_request_via_waterfall() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let history_arc: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));
        let turn_arc: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let waterfall = Arc::new(crate::events::WaterfallRegistry::new());
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        waterfall.register_emit(move |e| {
            if matches!(e, crate::events::WaterfallEvent::AgentRequest { .. }) {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        let hook = ContextHook::new(
            128_000,
            ContextConfig::default(),
            dummy_client(),
            "test-model".to_string(),
            tx,
            30,
            waterfall,
        )
        .with_history_capture(history_arc.clone(), turn_arc.clone());

        let history = vec![Message::user("hello")];
        let _ = hook
            .handle_completion_call(&history, &Message::user("prompt"), 1)
            .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "AgentRequest emit listener must fire on completion_call"
        );
    }
}
