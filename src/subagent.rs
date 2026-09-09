// 子代理模块：并行扇出多个上下文隔离的子代理（Claude-Code 风格 Task fanout）。
// Subagent module: parallel fanout of multiple context-isolated subagents
// (Claude-Code-style Task fanout).
//
// 构建者/编排者调用 `task` 工具时，传入一组子任务；每个子任务运行一个
// 完整的 agent 循环，拥有全新的、隔离的对话历史；结果聚合为一个工具输出。
// The Builder/Orchestrator calls the `task` tool with a list of subtasks;
// each subtask runs a full agent loop with FRESH, isolated conversation history;
// results aggregate back into one tool output.
//
// 两个内置 agent 模板：`explore`（只读，复用 Investigator）和 `build`（工作者，
// 复用 Builder）。
// Two built-in agent templates: `explore` (read-only, reuses Investigator) and
// `build` (worker, reuses Builder).
//
// 深度守卫：子代理不获得 `task` 工具（防止递归扇出）。守卫通过 `AgentRegistry`
// 上的 `Arc<AtomicU32>` 深度计数器实现：`run_subtask` 在调用 `run_autonomous`
// 前用 RAII 守卫递增计数器，`task_ctx_for_role` 检查计数器 > 0 时返回 None。
// Depth guard: subagents do NOT receive the `task` tool (no recursive fanout).
// The guard is an `Arc<AtomicU32>` depth counter on `AgentRegistry`:
// `run_subtask` brackets its `run_autonomous` call with an RAII guard that
// increments the counter; `task_ctx_for_role` returns None when depth > 0.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::warn;

use crate::agent_loop::run_autonomous_spec;
use crate::event::{AgentEvent, EventSender};
use crate::registry::{AgentRegistry, Role};
use crate::sandbox::Sandbox;

/// 单个子任务：描述、提示词、可选 agent 模板名。
/// A single subtask: description, prompt, optional agent template name.
///
/// `agent` 为 `None` 时默认 `"explore"`。
/// When `agent` is `None`, defaults to `"explore"`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SubTask {
    pub description: String,
    pub prompt: String,
    #[serde(default)]
    pub agent: Option<String>,
}

/// 子代理运行上下文：持有 sandbox/trust/tx/depth。
/// Subagent run context: holds sandbox/trust/tx/depth.
///
/// 不包含 `AgentRegistry`（避免与 registry 的 `task_ctx` 槽形成引用环）。
/// `TaskTool` 从 `ToolDeps.task_registry` 获取 registry 克隆。
/// Does NOT contain `AgentRegistry` (avoids a reference cycle with the
/// registry's `task_ctx` slot). The `TaskTool` gets the registry clone from
/// `ToolDeps.task_registry`.
///
/// 所有字段均为 `Clone` + `Send` + `Sync`，使 `SubagentCtx` 可安全跨
/// async 任务边界传递。
/// All fields are `Clone` + `Send` + `Sync`, making `SubagentCtx` safe to
/// pass across async task boundaries.
#[derive(Clone)]
pub struct SubagentCtx {
    pub sandbox: Sandbox,
    pub trust_sandbox: Arc<AtomicBool>,
    pub tx: EventSender,
    pub depth: Arc<AtomicU32>,
}

/// 深度守卫：创建时递增深度计数器，销毁时递减。
/// Depth guard: increments the depth counter on creation, decrements on drop.
///
/// 防止子代理递归扇出——当深度 > 0 时，`task_ctx_for_role` 返回 None，
/// 子代理构建的 agent 不包含 `task` 工具。
/// Prevents recursive subagent fanout — when depth > 0,
/// `task_ctx_for_role` returns None, so the subagent's built agent
/// does NOT include the `task` tool.
pub struct SubagentDepthGuard {
    depth: Arc<AtomicU32>,
}

impl SubagentDepthGuard {
    pub fn new(depth: Arc<AtomicU32>) -> Self {
        depth.fetch_add(1, Ordering::Relaxed);
        Self { depth }
    }

    /// 当前深度值（用于测试）。
    /// Current depth value (for testing).
    #[allow(dead_code)]
    pub fn current(depth: &Arc<AtomicU32>) -> u32 {
        depth.load(Ordering::Relaxed)
    }
}

impl Drop for SubagentDepthGuard {
    fn drop(&mut self) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 解析 agent 模板名为 Role。
/// Resolves an agent template name to a Role.
///
/// - `"explore"` → `Role::Investigator`（只读调查者）
/// - `"build"` → `Role::Builder`（构建者，可编辑文件）
/// - 未知名 → `Err`，错误信息列出所有有效名称。
///
/// 使用 `match` 而非 `if/else`，方便未来扩展自定义 agent。
/// Uses `match` (not if/else) for extensibility — custom agents can be
/// added as new arms.
#[allow(dead_code)]
pub fn resolve_agent(name: &str) -> Result<Role, String> {
    match name {
        "explore" => Ok(Role::Investigator),
        "build" => Ok(Role::Builder),
        other => Err(format!(
            "unknown agent '{other}'. Valid names: explore, build"
        )),
    }
}

/// 解析 agent 名为 `AgentSpec`：内置名优先，其次自定义子代理。
/// Resolves an agent name to an `AgentSpec`: built-in names first, then custom.
///
/// - `"explore"` → Investigator spec（只读调查者）
/// - `"build"` → Builder spec（构建者，可编辑文件）
/// - 其他 → 查找 `[agents.custom.<name>]`，找到则返回自定义 spec
/// - 未知名 → `Err`，错误信息列出所有有效名称（内置 + 已配置的自定义）。
///
/// - Other → looks up `[agents.custom.<name>]`; found → custom spec
/// - Unknown → `Err`, error lists all valid names (built-ins + configured custom)
pub fn resolve_spec(
    name: &str,
    registry: &AgentRegistry,
) -> Result<crate::registry::AgentSpec, String> {
    match name {
        "explore" => Ok(registry.agent_spec(Role::Investigator)),
        "build" => Ok(registry.agent_spec(Role::Builder)),
        other => {
            if let Some(spec) = registry.custom_spec(other) {
                Ok(spec)
            } else {
                let mut valid: Vec<String> = vec!["explore".into(), "build".into()];
                valid.extend(registry.custom_names());
                Err(format!(
                    "unknown agent '{other}'. Valid names: {}",
                    valid.join(", ")
                ))
            }
        }
    }
}

/// 并行扇出多个子任务，聚合结果为一个字符串。
/// Fans out multiple subtasks in parallel, aggregating results into one string.
///
/// - 有界并发：通过 `Semaphore` 限制 `max_concurrent`。
/// - 输入顺序稳定：结果按输入顺序排列（不按完成顺序）。通过索引化 `JoinSet`
///   结果实现——每个 spawned 任务返回 `(idx, output)`，收集后按 `idx` 排序。
/// - 每段输出上限 8000 字符，超出截断并标注 `…(截断 / truncated)`。
/// - 单个失败不影响其他任务：错误变为 `[error] {e}` 段。
/// - 空列表不 panic：返回 `(no subtasks)`。
///
/// - Bounded concurrency via `Semaphore`, capped at `max_concurrent`.
/// - Input-order stability: results are ordered by input order, not completion
///   order. Implemented via indexed `JoinSet` results — each spawned task
///   returns `(idx, output)`, collected and sorted by `idx`.
/// - Per-section 8000-char cap with `…(截断 / truncated)` marker.
/// - One failing task never fails others: error becomes `[error] {e}` section.
/// - Empty list never panics: returns `(no subtasks)`.
pub async fn fanout<F, Fut>(tasks: Vec<SubTask>, max_concurrent: usize, runner: F) -> String
where
    F: Fn(SubTask) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = String> + Send + 'static,
{
    if tasks.is_empty() {
        return "(no subtasks)".to_string();
    }

    // 提取描述，保持输入顺序——runner 消费 SubTask 后仍需描述做段标题。
    // Extract descriptions preserving input order — the runner consumes the
    // SubTask, but we still need descriptions for section headers.
    let descriptions: Vec<String> = tasks.iter().map(|t| t.description.clone()).collect();
    let max_concurrent = max_concurrent.max(1);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let runner = Arc::new(runner);

    let mut join_set: JoinSet<(usize, String)> = JoinSet::new();

    for (idx, task) in tasks.into_iter().enumerate() {
        let sem = semaphore.clone();
        let runner = runner.clone();
        join_set.spawn(async move {
            // 信号量控制在途任务数 / semaphore bounds in-flight tasks.
            let _permit = sem.acquire().await;
            let output = runner(task).await;
            (idx, output)
        });
    }

    // 收集结果，按输入索引排序——保证输出顺序与输入顺序一致。
    // Collect results, sort by input index — guarantees output order matches input order.
    let mut results: Vec<(usize, String)> = Vec::with_capacity(join_set.len());
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(item) => results.push(item),
            Err(e) => warn!("subagent join error: {e}"),
        }
    }
    results.sort_by_key(|(idx, _)| *idx);

    // 聚合为 `## {description}\n{output}` 段，段间用 `\n\n` 连接。
    // Aggregate into `## {description}\n{output}` sections, joined by `\n\n`.
    let sections: Vec<String> = results
        .iter()
        .map(|(idx, output)| {
            let desc = &descriptions[*idx];
            let body = cap_chars(output, 8000);
            format!("## {desc}\n{body}")
        })
        .collect();
    sections.join("\n\n")
}

/// 运行单个子任务：解析 agent 名 → 创建独立事件通道 → 运行 `run_autonomous_spec`。
/// Runs a single subtask: resolves agent name → creates an isolated event
/// channel → runs `run_autonomous_spec`.
///
/// 上下文隔离：传入空的历史 Arc（父历史不泄漏），无 waterfall / pre_step
/// （SDD 监听器不对子代理触发）。
/// Context isolation: passes an EMPTY history Arc (parent history never leaks),
/// no waterfall / pre_step (SDD listeners must not fire for subagents).
///
/// 深度守卫：用 RAII 守卫递增 `ctx.depth`，使子代理构建的 agent 不包含
/// `task` 工具（`task_ctx` 传 None 给 `run_autonomous_spec`）。
/// Depth guard: an RAII guard increments `ctx.depth`; `run_autonomous_spec`
/// passes None for `task_ctx` — subagents do NOT receive the `task` tool.
///
/// TUI 可见性（v1）：仅向父 tx 发出 start/finish Info 行；子代理内部事件
/// 不泄露到父流。
/// TUI visibility (v1): emits only start/finish Info lines to the parent tx;
/// subagent internal events do NOT spam the parent's stream.
pub async fn run_subtask(registry: AgentRegistry, ctx: SubagentCtx, task: SubTask) -> String {
    // 解析 agent 模板名 → AgentSpec / resolve agent template name → AgentSpec.
    let agent_name = task.agent.as_deref().unwrap_or("explore");
    let spec = match resolve_spec(agent_name, &registry) {
        Ok(s) => s,
        Err(e) => {
            return format!("[error] {e}");
        }
    };

    let desc = task.description.clone();

    // 创建独立事件通道——子代理的事件不直接进入父流。
    // Create an isolated event channel — subagent events do NOT go directly
    // into the parent's stream.
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<AgentEvent>();

    // 排水任务：消费子代理事件，丢弃全部（v1 仅 start/finish Info 行由
    // run_subtask 自身发出，不从子代理事件转发）。
    // Drainer task: consumes subagent events, drops all (v1 — start/finish
    // Info lines are emitted by run_subtask itself, not forwarded from
    // subagent events).
    let drainer = tokio::spawn(async move {
        while sub_rx.recv().await.is_some() {
            // v1: 丢弃所有子代理内部事件 / drop all subagent internal events.
        }
    });

    // 向父 tx 发出启动 Info / emit start Info to parent tx.
    let _ = ctx.tx.send(AgentEvent::Info(format!(
        "\u{25b6} \u{5b50}\u{4ee3}\u{7406} [{desc}] \u{542f}\u{52a8} / subagent started"
    )));

    // 深度守卫：递增计数器，使子代理构建的 agent 不包含 task 工具。
    // Depth guard: increment the counter so the subagent's built agent
    // does NOT include the task tool.
    let _depth_guard = SubagentDepthGuard::new(ctx.depth.clone());

    // 空历史 Arc——上下文隔离 / empty history Arc — context isolation.
    let empty_history: Arc<Mutex<Vec<rig_core::completion::Message>>> =
        Arc::new(Mutex::new(Vec::new()));

    let result = run_autonomous_spec(
        &registry,
        &ctx.sandbox,
        ctx.trust_sandbox.clone(),
        spec,
        &task.prompt,
        &sub_tx,
        empty_history,
        // 无 waterfall / pre_step —— SDD 监听器不对子代理触发。
        // No waterfall / pre_step — SDD listeners must not fire for subagents.
        None,
        None,
    )
    .await;

    // 关闭子通道，让排水任务退出 / close the sub-channel to let the drainer exit.
    drop(sub_tx);
    let _ = drainer.await;

    // 向父 tx 发出完成 Info / emit finish Info to parent tx.
    let _ = ctx.tx.send(AgentEvent::Info(format!(
        "\u{2713} \u{5b50}\u{4ee3}\u{7406} [{desc}] \u{5b8c}\u{6210} / done"
    )));

    match result {
        Ok(output) => output,
        Err(e) => format!("[error] {e}"),
    }
}

/// 在 UTF-8 字符边界处截断字符串到 `max` 个字符，超出时附加截断标记。
/// Truncates a string at a UTF-8 char boundary to `max` chars, appending
/// a truncation marker when truncated.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(max);
    format!("{}\u{2026}(\u{622a}\u{65ad} / truncated)", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    // 使用 crate 根的共享 env 互斥锁（避免跨模块 env 竞争）。
    // Use the crate-root shared env mutex to avoid cross-module env races.
    use crate::TEST_ENV_MUTEX as ENV_MUTEX;

    // 测试隔离：临时覆盖/移除环境变量，避免外部 AGENT_PROFILE 泄漏到测试中。
    // Test isolation: temporarily override/remove env vars so external AGENT_PROFILE
    // does not leak into tests.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn new(key: &'static str, value: Option<&str>) -> Self {
            let prev = std::env::var(key).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
            EnvGuard { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    // ── resolve_agent 测试 / resolve_agent tests ──

    #[test]
    fn resolve_agent_explore_maps_to_investigator() {
        assert_eq!(resolve_agent("explore").unwrap(), Role::Investigator);
    }

    #[test]
    fn resolve_agent_build_maps_to_builder() {
        assert_eq!(resolve_agent("build").unwrap(), Role::Builder);
    }

    #[test]
    fn resolve_agent_unknown_returns_error_with_valid_names() {
        let err = resolve_agent("hacker").unwrap_err();
        assert!(err.contains("hacker"), "error should mention the bad name");
        assert!(err.contains("explore"), "error should list 'explore'");
        assert!(err.contains("build"), "error should list 'build'");
    }

    // ── fanout 测试 / fanout tests ──

    /// 空列表返回 "(no subtasks)"，不 panic。
    /// Empty list returns "(no subtasks)", no panic.
    #[tokio::test]
    async fn fanout_empty_list_returns_no_subtasks() {
        let result = fanout(Vec::new(), 4, |_task: SubTask| async { "ok".to_string() }).await;
        assert_eq!(result, "(no subtasks)");
    }

    /// 聚合格式：每段 `## {description}\n{output}`，段间 `\n\n` 连接。
    /// Aggregation format: each section `## {description}\n{output}`,
    /// joined by `\n\n`.
    #[tokio::test]
    async fn fanout_aggregation_format() {
        let tasks = vec![
            SubTask {
                description: "task-a".into(),
                prompt: "p1".into(),
                agent: None,
            },
            SubTask {
                description: "task-b".into(),
                prompt: "p2".into(),
                agent: None,
            },
        ];
        let result = fanout(tasks, 4, |t| async move { format!("out-{}", t.description) }).await;
        assert!(result.contains("## task-a\nout-task-a"), "result: {result}");
        assert!(result.contains("## task-b\nout-task-b"), "result: {result}");
        // 段间恰好两个换行 / sections separated by exactly two newlines.
        assert!(result.contains("\n\n## task-b"));
    }

    /// 输入顺序稳定：即使后启动的任务先完成，输出仍按输入顺序排列。
    /// Input-order stability: even if later tasks finish first, output
    /// follows input order.
    #[tokio::test]
    async fn fanout_input_order_stable_under_reverse_completion() {
        use tokio::sync::Barrier;

        // 3 个任务，用 Barrier 强制第 1 个最后完成。
        // 3 tasks, use Barrier to force task 1 to finish last.
        let barrier = Arc::new(Barrier::new(3));
        let tasks = vec![
            SubTask {
                description: "first".into(),
                prompt: "p".into(),
                agent: None,
            },
            SubTask {
                description: "second".into(),
                prompt: "p".into(),
                agent: None,
            },
            SubTask {
                description: "third".into(),
                prompt: "p".into(),
                agent: None,
            },
        ];
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let b3 = barrier.clone();
        let result = fanout(tasks, 4, move |t| {
            let b = match t.description.as_str() {
                "first" => b1.clone(),
                "second" => b2.clone(),
                _ => b3.clone(),
            };
            async move {
                b.wait().await;
                t.description
            }
        })
        .await;

        // 无论完成顺序，输出段按输入顺序排列。
        // Regardless of completion order, sections are in input order.
        let first_pos = result.find("## first").unwrap();
        let second_pos = result.find("## second").unwrap();
        let third_pos = result.find("## third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

    /// 一个任务失败不影响其他任务：错误变为 `[error]` 段。
    /// One failing task doesn't affect others: error becomes `[error]` section.
    #[tokio::test]
    async fn fanout_one_failing_others_succeed() {
        let tasks = vec![
            SubTask {
                description: "ok-task".into(),
                prompt: "p".into(),
                agent: None,
            },
            SubTask {
                description: "fail-task".into(),
                prompt: "p".into(),
                agent: Some("hacker".into()), // unknown agent → error
            },
            SubTask {
                description: "ok-task-2".into(),
                prompt: "p".into(),
                agent: None,
            },
        ];
        let result = fanout(tasks, 4, |t| async move {
            let name = t.agent.as_deref().unwrap_or("explore");
            match name {
                "explore" | "build" => format!("ok-{}", t.description),
                other => format!(
                    "[error] unknown agent '{other}'. Valid names: explore, build"
                ),
            }
        })
        .await;
        assert!(result.contains("## ok-task\nok-ok-task"), "result: {result}");
        assert!(
            result.contains("## fail-task\n[error]"),
            "result should contain error section: {result}"
        );
        assert!(result.contains("## ok-task-2\nok-ok-task-2"), "result: {result}");
    }

    /// 每段输出上限 8000 字符，超出截断并标注。
    /// Per-section 8000-char cap with truncation marker.
    #[tokio::test]
    async fn fanout_section_8000_char_cap() {
        let long_output: String = "X".repeat(10000);
        let tasks = vec![SubTask {
            description: "big".into(),
            prompt: "p".into(),
            agent: None,
        }];
        let result = fanout(tasks, 4, move |_| {
            let out = long_output.clone();
            async move { out }
        })
        .await;
        // 截断标记存在 / truncation marker present.
        assert!(result.contains("\u{2026}(\u{622a}\u{65ad} / truncated)"), "result should contain truncation marker");
        // 截断后段体不超过 8000 + 标记长度 / body capped at ~8000 + marker.
        let body_start = result.find('\n').unwrap() + 1;
        let body = &result[body_start..];
        assert!(
            body.chars().count() <= 8100,
            "body should be capped near 8000 chars, got {}",
            body.chars().count()
        );
    }

    /// max_concurrent 被遵守：使用原子计数器记录最大并发数。
    /// max_concurrent is respected: uses an atomic counter to record
    /// the max concurrency seen.
    #[tokio::test]
    async fn fanout_max_concurrent_respected() {
        use std::sync::atomic::AtomicU32;

        let max_concurrent = 2;
        let n_tasks = 6;
        let in_flight = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));

        let tasks: Vec<SubTask> = (0..n_tasks)
            .map(|i| SubTask {
                description: format!("task-{i}"),
                prompt: "p".into(),
                agent: None,
            })
            .collect();

        let inf = in_flight.clone();
        let mx = max_seen.clone();
        let result = fanout(tasks, max_concurrent, move |_t| {
            let inf = inf.clone();
            let mx = mx.clone();
            async move {
                let cur = inf.fetch_add(1, Ordering::SeqCst) + 1;
                // 记录最大并发 / record max concurrency.
                let mut seen = mx.load(Ordering::SeqCst);
                while cur > seen {
                    match mx.compare_exchange(seen, cur, Ordering::SeqCst, Ordering::SeqCst) {
                        Ok(_) => break,
                        Err(v) => seen = v,
                    }
                }
                // 短暂 yield 让其他任务有机会并发 / brief yield to allow overlap.
                tokio::task::yield_now().await;
                inf.fetch_sub(1, Ordering::SeqCst);
                "done".to_string()
            }
        })
        .await;

        assert!(!result.is_empty());
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            max_concurrent as u32,
            "max concurrency should be exactly {}, got {}",
            max_concurrent,
            max_seen.load(Ordering::SeqCst)
        );
    }

    // ── SubagentDepthGuard 测试 / SubagentDepthGuard tests ──

    #[test]
    fn depth_guard_increments_and_resets_on_drop() {
        let depth = Arc::new(AtomicU32::new(0));
        assert_eq!(SubagentDepthGuard::current(&depth), 0);
        {
            let _g = SubagentDepthGuard::new(depth.clone());
            assert_eq!(SubagentDepthGuard::current(&depth), 1);
        }
        assert_eq!(SubagentDepthGuard::current(&depth), 0, "depth must reset on drop");
    }

    #[test]
    fn depth_guard_nested() {
        let depth = Arc::new(AtomicU32::new(0));
        let _g1 = SubagentDepthGuard::new(depth.clone());
        assert_eq!(SubagentDepthGuard::current(&depth), 1);
        {
            let _g2 = SubagentDepthGuard::new(depth.clone());
            assert_eq!(SubagentDepthGuard::current(&depth), 2);
        }
        assert_eq!(SubagentDepthGuard::current(&depth), 1);
    }

    // ── cap_chars 测试 / cap_chars tests ──

    #[test]
    fn cap_chars_short_unchanged() {
        assert_eq!(cap_chars("hello", 10), "hello");
    }

    #[test]
    fn cap_chars_exact_boundary() {
        assert_eq!(cap_chars("hello", 5), "hello");
    }

    #[test]
    fn cap_chars_truncates_with_marker() {
        let result = cap_chars("hello world", 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("\u{2026}(\u{622a}\u{65ad} / truncated)"));
    }

    #[test]
    fn cap_chars_multibyte_safe() {
        // 中文字符每个 3 字节——截断不能在字符中间切片。
        // Chinese chars are 3 bytes each — truncation must not slice mid-char.
        let s = "\u{4e2d}\u{6587}\u{6d4b}\u{8bd5}\u{6587}\u{5b57}"; // 中文测试文字 (6 chars)
        let result = cap_chars(s, 3);
        assert!(result.contains("\u{2026}"));
        // 确保不 panic 且结果以完整字符结尾 / ensure no panic and result ends on a char boundary.
        assert!(result.chars().all(|c| c != '\u{fffd}'));
    }

    // ── resolve_spec 测试 / resolve_spec tests ──

    fn empty_mcp() -> std::sync::Arc<crate::mcp::McpManager> {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for test");
        let mcp = rt.block_on(crate::mcp::McpManager::connect_all(
            &std::collections::HashMap::new(),
        ));
        std::sync::Arc::new(mcp)
    }

    fn disabled_sandbox_provider() -> std::sync::Arc<dyn crate::seam::SandboxProvider> {
        std::sync::Arc::new(crate::sandbox::SimpleSandbox::with_backend(
            &[],
            crate::sandbox::SandboxBackend::Off,
        ))
    }

    fn registry_with_custom() -> AgentRegistry {
        use crate::config::Config;
        let toml_str = r#"
[agent]
default_model = "test-model"
max_turns = 10

[agents.investigator]
model = "test-model"
preamble = "prompts/investigator.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"

[agents.builder]
model = "test-model"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
permissions.edit_file = "allow"

[agents.custom.researcher]
preamble = "agents/researcher.md"
model = "glm-latest"
permissions.read_file = "allow"
permissions.edit_file = "deny"
"#;
        let cfg = std::sync::Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("config parse"),
        );
        AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider())
    }

    #[test]
    fn resolve_spec_explore_returns_investigator() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = EnvGuard::new("AGENT_PROFILE", None);
        let reg = registry_with_custom();
        let spec = resolve_spec("explore", &reg).unwrap();
        assert_eq!(spec.name, "investigator");
        assert!(spec.embedded_preamble.is_some());
    }

    #[test]
    fn resolve_spec_build_returns_builder() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = EnvGuard::new("AGENT_PROFILE", None);
        let reg = registry_with_custom();
        let spec = resolve_spec("build", &reg).unwrap();
        assert_eq!(spec.name, "builder");
        assert!(spec.embedded_preamble.is_some());
    }

    #[test]
    fn resolve_spec_custom_returns_custom_spec() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = EnvGuard::new("AGENT_PROFILE", None);
        let reg = registry_with_custom();
        let spec = resolve_spec("researcher", &reg).unwrap();
        assert_eq!(spec.name, "researcher");
        assert_eq!(spec.preamble_path, "agents/researcher.md");
        assert_eq!(spec.model.as_deref(), Some("glm-latest"));
        assert!(spec.embedded_preamble.is_none(), "custom has no embedded fallback");
    }

    #[test]
    fn resolve_spec_unknown_lists_all_valid_names() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = EnvGuard::new("AGENT_PROFILE", None);
        let reg = registry_with_custom();
        let err = resolve_spec("hacker", &reg).unwrap_err();
        assert!(err.contains("hacker"), "error should mention the bad name");
        assert!(err.contains("explore"), "error should list 'explore'");
        assert!(err.contains("build"), "error should list 'build'");
        assert!(err.contains("researcher"), "error should list configured custom");
    }

    #[test]
    fn resolve_spec_built_in_takes_precedence_over_custom() {
        // Even if a custom agent named "explore" exists, the built-in wins.
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _g = EnvGuard::new("AGENT_PROFILE", None);
        use crate::config::Config;
        let toml_str = r#"
[agent]
default_model = "test-model"

[agents.custom.explore]
preamble = "custom-explore.md"
"#;
        let cfg = std::sync::Arc::new(
            Config::from_str_with_profile(toml_str, None).expect("config parse"),
        );
        let reg = AgentRegistry::new(cfg, empty_mcp(), disabled_sandbox_provider());
        let spec = resolve_spec("explore", &reg).unwrap();
        assert_eq!(
            spec.name, "investigator",
            "built-in 'explore' must take precedence over custom"
        );
    }
}
