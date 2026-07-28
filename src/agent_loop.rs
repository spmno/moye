// 自主循环模块：用 rig 的 AgentRunner 驱动一个自我驱动的 Agent 循环（上限 max_turns），
// 并通过 HitlHook（rig AgentHook）在每次工具调用时按权限分级做 HITL（人在环）门控。
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rig_core::agent::{AgentHook, Flow, HookContext, StepEvent};
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, Usage};
use rig_core::providers::openai::CompletionModel as OpenAiModel;
use rig_core::tool::ToolDyn;
use tracing::{info, warn};

use crate::registry::{AgentRegistry, Permission, Role, ToolPerms};
use crate::tools::is_readonly_bash;

/// HITL（人在环）门控。实现为 rig 的 `AgentHook`，拦截每一次 `ToolCall` 并按角色的
/// 按工具权限分级处理：
/// - `Allow` -> 静默执行（不询问）。像 `ls` 这样的琐碎步骤直接通过。
/// - `Ask`   -> 在终端暂停询问用户；yes 执行，no 跳过。
/// - `Deny`  -> 跳过调用并向模型说明原因。
///
/// 仅对 `ToolCall` 事件做门控；模型的回合、结果、增量事件原样通过。
/// 权限分级在循环启动时即已捕获。
#[derive(Clone)]
pub struct HitlHook {
    perms: Arc<Mutex<ToolPerms>>,
    waiting: Arc<AtomicBool>,
}

impl HitlHook {
    pub fn new(perms: ToolPerms, waiting: Arc<AtomicBool>) -> Self {
        Self {
            perms: Arc::new(Mutex::new(perms)),
            waiting,
        }
    }

    async fn confirm(&self, prompt: &str) -> bool {
        self.waiting.store(true, Ordering::Relaxed);
        let prompt = prompt.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            use rustyline::DefaultEditor;
            let mut rl = match DefaultEditor::new() {
                Ok(rl) => rl,
                Err(_) => return false,
            };
            match rl.readline(&prompt) {
                Ok(line) => {
                    let a = line.trim().to_lowercase();
                    a == "y" || a == "yes"
                }
                Err(_) => false,
            }
        });
        let result = handle.await.unwrap_or(false);
        self.waiting.store(false, Ordering::Relaxed);
        result
    }
}

impl<M: CompletionModel> AgentHook<M> for HitlHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            // 流式模式下文本增量由 stream consumer 直接 print 到 stdout，
            // hook 不再重复打印。
            StepEvent::TextDelta { .. } => Flow::Continue,
            // 模型回合完成：仅打印轮次标记与用量（文本已由流式增量输出）。
            StepEvent::ModelTurnFinished { turn, usage, .. } => {
                info!("\n--- 轮次 {turn} 完成 ---");
                print_usage(&usage);
                Flow::Continue
            }
            // 工具调用：打印调用信息，然后按权限分级做 HITL 决策。
            StepEvent::ToolCall { tool_name, args, .. } => {
                let desc = format_tool_call_desc(tool_name, args);
                info!("[调用工具] {desc}");
                let perms = self.perms.lock().unwrap().clone();
                let tier = decide_tier(&perms, tool_name, args);
                match tier {
                    Permission::Allow => {
                        info!("  [HITL] 自动允许");
                        Flow::Continue
                    }
                    Permission::Deny => {
                        info!("  [HITL] 已拒绝（安全策略）");
                        Flow::Skip {
                            reason: format!("工具 `{tool_name}` 被当前角色的安全策略禁止"),
                        }
                    }
                    Permission::Ask => {
                        let prompt =
                            format!("  [HITL] 允许执行 `{tool_name}`？[y/N] ");
                        if self.confirm(&prompt).await {
                            Flow::Continue
                        } else {
                            Flow::Skip {
                                reason: format!(
                                    "用户拒绝了 `{tool_name}` 的执行"
                                ),
                            }
                        }
                    }
                }
            }
            // 工具结果：打印执行结果。
            StepEvent::ToolResult { tool_name, result, .. } => {
                let truncated = if result.len() > 500 {
                    format!("{}…(截断，共 {} 字节)", &result[..result.floor_char_boundary(500)], result.len())
                } else {
                    result.to_string()
                };
                info!("[工具结果] {tool_name}:\n{truncated}");
                Flow::Continue
            }
            // 未知工具名：打印警告。
            StepEvent::InvalidToolCall(ctx) => {
                warn!("[未知工具] {}", ctx.tool_name);
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }
}

/// 将工具调用参数格式化为人类可读的描述。
/// 例如 `read_file` 打印读取的文件路径，`run_bash` 打印执行的命令。
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
            format!("read_file → 读取文件: {path}")
        }
        "edit_file" => {
            let path = get_str("path").unwrap_or_default();
            let old = get_str("old").unwrap_or_default();
            let new = get_str("new").unwrap_or_default();
            format!(
                "edit_file → 编辑文件: {path}\n  替换: {old}\n  替换为: {new}"
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
            format!("write_file → 写入文件: {path} ({content_len} 字节)")
        }
        "run_bash" => {
            let command = get_str("command").unwrap_or_default();
            format!("run_bash → 执行命令: {command}")
        }
        "web_fetch" => {
            let url = get_str("url").unwrap_or_default();
            format!("web_fetch → 抓取网页: {url}")
        }
        "web_search" => {
            let query = get_str("query").unwrap_or_default();
            format!("web_search → 搜索: {query}")
        }
        _ => format!("{tool_name}({args})"),
    }
}

/// 打印 token 用量摘要。
fn print_usage(usage: &Usage) {
    let input = usage.input_tokens;
    let output = usage.output_tokens;
    let cached = usage.cached_input_tokens;
    let reasoning = usage.reasoning_tokens;
    if input == 0 && output == 0 {
        return;
    }
    let mut parts = vec![format!("输入={input}")];
    if cached > 0 {
        parts.push(format!("缓存={cached}"));
    }
    parts.push(format!("输出={output}"));
    if reasoning > 0 {
        parts.push(format!("推理={reasoning}"));
    }
    info!("[用量] {}", parts.join("，"));
}

/// 纯函数形式的权限分级解析，可不依赖 hook 包装单独测试。`args` 为 JSON 形式的
/// 工具调用参数（用于从 `run_bash` 中提取 `command`）。
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
/// `Ask` 在此解析为 `Flow::Skip`；线上 hook 应改用 `on_event` 中
/// 的 match tier 分支，对 `Ask` 通过 `confirm()` 进行终端交互询问。
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
/// `HitlHook` 门控关键决策。模型结束或达到 max_turns 时停止。
///
/// SSE 断连时自动重试最多 3 次，每次告知模型已完成的工作让其继续。
pub async fn run_autonomous(
    registry: &AgentRegistry,
    role: Role,
    goal: &str,
) -> anyhow::Result<String> {
    const MAX_RETRIES: usize = 3;

    for attempt in 0..=MAX_RETRIES {
        let prompt = if attempt == 0 {
            goal.to_string()
        } else {
            format!(
                "{goal}\n\n\
                 [系统提示] 上次执行因 SSE 连接中断（第 {attempt} 次重试）。\
                 请用 `ls -R .` 检查已创建的文件，跳过已完成的步骤，继续未完成的部分。"
            )
        };

        let perms = registry.tool_perms(role);
        let max_turns = registry.max_turns();
        let hitl_waiting = Arc::new(AtomicBool::new(false));
        let hook = HitlHook::new(perms, hitl_waiting.clone());

        let agent = build_runner_agent(registry, role)?;
        let stream = agent
            .runner(&prompt)
            .max_turns(max_turns)
            .max_invalid_tool_call_retries(3)
            .add_hook(hook)
            .stream()
            .await;

        match consume_stream(stream, Some(hitl_waiting)).await {
            Ok(output) => return Ok(output),
            Err(e) if is_stream_error(&e) && attempt < MAX_RETRIES => {
                info!(
                    "[重试] {role:?} 第 {}/{} 次：SSE 连接中断，重新调用",
                    attempt + 1,
                    MAX_RETRIES
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(anyhow::anyhow!("{role:?} 重试 {MAX_RETRIES} 次后仍失败"))
}

/// 判断错误是否为 SSE 流式断连（可安全重试）。
pub fn is_stream_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("error decoding response body")
        || msg.contains("SSE error")
        || msg.contains("Reset(StreamId")
        || msg.contains("流式错误")
        || msg.contains("空闲超时")
}

/// 消费流式输出：文本增量直接 print 到 stdout，reasoning 缓冲后整块输出。
/// 供 `run_autonomous` 和 `RoleAgent::run` 共用。
///
/// 当模型将全部内容放在 reasoning 通道（content 字段为空）时，
/// 用累积的 reasoning 内容作为输出回退，避免下游收到空计划。
pub async fn consume_stream<R>(
    mut stream: rig_core::agent::StreamingResult<R>,
    hitl_waiting: Option<Arc<AtomicBool>>,
) -> anyhow::Result<String> {
    use rig_core::agent::MultiTurnStreamItem;
    use rig_core::streaming::StreamedAssistantContent;

    const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);
    const THINK_HINT_DELAY: Duration = Duration::from_secs(5);

    let mut output = String::new();
    let mut reasoning_buf = String::new();
    let mut all_reasoning = String::new();
    let mut reasoning_start: Option<std::time::Instant> = None;
    let mut thinking_hint_shown = false;

    let flush_reasoning = |buf: &mut String, all: &mut String, hint: &mut bool| {
        if !buf.is_empty() {
            if *hint {
                println!();
                std::io::stdout().flush().ok();
                *hint = false;
            }
            info!("[思考] {}", buf.trim());
            all.push_str(buf.trim());
            all.push('\n');
            buf.clear();
        }
    };

    loop {
        let next = match &hitl_waiting {
            Some(flag) if flag.load(Ordering::Relaxed) => stream.next().await,
            _ => match tokio::time::timeout(CHUNK_TIMEOUT, stream.next()).await {
                Ok(item) => item,
                Err(_) => {
                    flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                    return Err(anyhow::anyhow!(
                        "SSE 空闲超时（{}秒无数据），可能连接已断开",
                        CHUNK_TIMEOUT.as_secs()
                    ));
                }
            },
        };

        let Some(item) = next else { break };

        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
                match content {
                    StreamedAssistantContent::Text(t) => {
                        flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                        if !t.text.is_empty() {
                            print!("{}", t.text);
                            std::io::stdout().flush().ok();
                        }
                    }
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                        if reasoning_start.is_none() {
                            reasoning_start = Some(std::time::Instant::now());
                        }
                        if !thinking_hint_shown {
                            if let Some(start) = &reasoning_start {
                                if start.elapsed() >= THINK_HINT_DELAY {
                                    print!("思考中");
                                    std::io::stdout().flush().ok();
                                    thinking_hint_shown = true;
                                }
                            }
                        }
                        reasoning_buf.push_str(&reasoning);
                    }
                    StreamedAssistantContent::ToolCall { tool_call, .. } => {
                        flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                        let desc = format_tool_call_desc(
                            &tool_call.function.name,
                            &tool_call.function.arguments.to_string(),
                        );
                        info!("[模型调用] {desc}");
                    }
                    _ => {
                        flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                    }
                }
            }
            Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                output = resp.output;
            }
            Err(e) => {
                flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
                return Err(anyhow::anyhow!("流式错误: {e}"));
            }
            _ => {
                flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
            }
        }
    }

    flush_reasoning(&mut reasoning_buf, &mut all_reasoning, &mut thinking_hint_shown);
    if output.is_empty() {
        if !all_reasoning.is_empty() {
            output = all_reasoning;
        } else {
            output.push_str("(无输出)");
        }
    }
    Ok(output)
}

/// 为某角色构建"可运行"的 rig `Agent`（带工具），并遵循会话级模型覆盖。
/// 与 `AgentRegistry::build` 类似，但返回原始 `Agent`，以便附加 runner 与 hook。
fn build_runner_agent(
    registry: &AgentRegistry,
    role: Role,
) -> anyhow::Result<rig_core::agent::Agent<OpenAiModel>> {
    let rc = registry
        .role_config(role)
        .ok_or_else(|| anyhow::anyhow!("no config for role {role:?}"))?;
    let client = crate::providers::create_client()?;
    let preamble = std::fs::read_to_string(&rc.preamble)
        .unwrap_or_else(|_| format!("你是 {role:?} agent。"));
    // 与 registry 的 build 一致：把相关技能指令注入提示词。
    let preamble = crate::registry::inject_skills_public(&preamble);
    let model = registry.session_model().unwrap_or_else(|| rc.model.clone());
    let max_turns = registry.max_turns();
    info!("[runner] role={role:?} model={model} max_turns={max_turns}");
    let tools: Vec<Box<dyn ToolDyn>> = crate::tools::builtin_tools()?;
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
        // 纯函数决策中 Ask 分级解析为 Skip（线上 hook 会改为交互询问）。
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
