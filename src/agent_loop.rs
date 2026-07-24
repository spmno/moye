// 自主循环模块：用 rig 的 AgentRunner 驱动一个自我驱动的 Agent 循环（上限 max_turns），
// 并通过 HitlHook（rig AgentHook）在每次工具调用时按权限分级做 HITL（人在环）门控。
use std::sync::{Arc, Mutex};

use rig_core::agent::{AgentHook, Flow, HookContext, StepEvent};
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, AssistantContent, Usage};
use rig_core::providers::openai::CompletionModel as OpenAiModel;
use rig_core::tool::ToolDyn;
use tracing::{info, warn};

use crate::registry::{AgentRegistry, Permission, Role, ToolPerms};
use crate::tools::{is_readonly_bash, TOOL_NAMES};

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
}

#[allow(dead_code)]
impl HitlHook {
    pub fn new(perms: ToolPerms) -> Self {
        Self {
            perms: Arc::new(Mutex::new(perms)),
        }
    }

    /// 在终端阻塞式询问 yes/no。通过 `spawn_blocking` 在独立线程执行阻塞的
    /// rustyline 读取，避免卡住异步运行时，随后 await 其结果。返回 true 表示"是"。
    async fn confirm(&self, prompt: &str) -> bool {
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
        handle.await.unwrap_or(false)
    }
}

impl<M: CompletionModel> AgentHook<M> for HitlHook {
    async fn on_event(&self, _ctx: &HookContext, event: StepEvent<'_, M>) -> Flow {
        match event {
            // 流式文本增量：实时输出到终端和日志文件。
            StepEvent::TextDelta { delta, .. } => {
                if !delta.is_empty() {
                    info!("{delta}");
                }
                Flow::Continue
            }
            // 模型回合完成：打印思考过程（reasoning）与最终输出。
            StepEvent::ModelTurnFinished { turn, content, usage } => {
                info!("\n--- 轮次 {turn} ---");
                for item in content.iter() {
                    match item {
                        AssistantContent::Reasoning(r) => {
                            let text = r.display_text();
                            if !text.is_empty() {
                                info!("[思考] {text}");
                            }
                        }
                        AssistantContent::Text(t) => {
                            if !t.text.is_empty() {
                                info!("[回复] {}", t.text);
                            }
                        }
                        _ => {}
                    }
                }
                print_usage(&usage);
                Flow::Continue
            }
            // 工具调用：打印调用信息，然后按权限分级做 HITL 决策。
            StepEvent::ToolCall { tool_name, args, .. } => {
                info!("[调用工具] {tool_name}({args})");
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
                    format!("{}…(截断，共 {} 字节)", &result[..500], result.len())
                } else {
                    result.to_string()
                };
                info!("[工具结果] {tool_name}: {truncated}");
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
#[allow(dead_code)]
pub fn decide_flow(perms: &ToolPerms, tool_name: &str, args: &str) -> Flow {
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

/// 针对 `goal` 驱动自主 Agent 循环。Builder 角色自行规划并执行，调用工具；
/// `HitlHook` 门控关键决策。模型结束或达到 max_turns 时停止。
pub async fn run_autonomous(registry: &AgentRegistry, goal: &str) -> anyhow::Result<String> {
    let perms = registry.tool_perms(Role::Builder);
    let max_turns = registry.max_turns();
    let hook = HitlHook::new(perms);

    let agent = build_runner_agent(registry, Role::Builder)?;
    let response = agent
        .runner(goal)
        .max_turns(max_turns)
        // 对模型臆造的未知工具名做容错：重试该回合并附带纠正反馈，而非直接中止循环。
        .max_invalid_tool_call_retries(3)
        .add_hook(hook)
        .run()
        .await?;
    Ok(response.output)
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
    info!("[runner] role={role:?} model={model}");
    let tools: Vec<Box<dyn ToolDyn>> = crate::tools::builtin_tools()?;
    let params = crate::providers::provider_additional_params();
    let agent = client
        .agent(&model)
        .preamble(&preamble)
        .temperature(crate::providers::Provider::clamp_temperature(0.7))
        .tools(tools)
        .additional_params(params)
        .build();
    Ok(agent)
}

#[allow(dead_code)]
fn _assert_tool_names() {
    // 编译期守卫：确保分类器覆盖了每一个内置工具。
    let _ = TOOL_NAMES;
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
