// 事件模块：定义 Agent 领域层与表示层之间的共享事件类型。
// Event module: defines the shared event type between the agent domain layer
// and the presentation layer.
//
// 领域层（orchestrator、agent_loop、reviewer、prompt_evolve）只依赖此模块，
// 不依赖 ui::tui。这保证了依赖方向正确：表示层 → 共享抽象 ← 领域层。
// The domain layer (orchestrator, agent_loop, reviewer, prompt_evolve) depends
// only on this module, never on ui::tui. This ensures correct dependency direction:
// presentation → shared abstraction ← domain.
//
// 未来要加 CLI / API server 等新消费者时，只需创建一个新的 channel 并传入
// EventSender，无需修改任何领域层代码。
// To add a new consumer (CLI, API server, etc.), create a new channel and pass
// the EventSender — no domain-layer code needs to change.

use tokio::sync::{mpsc, oneshot};

/// HITL 决策：用户对工具确认请求的响应。
/// HITL decision: the user's response to a tool confirmation request.
///
/// - `Allow`  —— 允许本次执行（沙箱路径提示的"允许一次"）。
/// - `Deny`   —— 拒绝执行。
/// - `Always` —— 授权目录（仅沙箱路径提示）：本次会话不再询问，并持久化到 agent.toml。
///
/// - `Allow`  — approve this one execution (sandbox path prompt's "allow once").
/// - `Deny`   — deny execution.
/// - `Always` — authorize the directory (sandbox path prompt only): no more prompts
///   this session, and the directory is persisted to agent.toml for future sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitlDecision {
    Allow,
    Deny,
    Always,
}

/// edit_file 工具调用的结构化编辑载荷：渲染 unified diff 用。
/// Structured edit payload for the edit_file tool call: used to render a unified diff.
#[derive(Debug)]
pub struct FileEdit {
    pub path: String,
    pub old: String,
    pub new: String,
}

/// todo_write 工具的任务状态。
/// Task status for the todo_write tool.
///
/// 序列化为 `snake_case`：`pending` / `in_progress` / `completed`。
/// Serialized as `snake_case`: `pending` / `in_progress` / `completed`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// todo_write 工具的单条任务项：id + 内容 + 状态。
/// A single todo item for the todo_write tool: id + content + status.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// Agent 事件。领域层产生，表示层消费。
/// Agent events. Produced by the domain layer, consumed by the presentation layer.
///
/// 事件分两类：
/// Events fall into two categories:
/// - **瞬态**（TextDelta、ReasoningDelta、AgentStarted、AgentFinished、HitlPrompt）：
///   影响 TUI 运行时状态但不进入消息历史。
///   **Transient** (TextDelta, ReasoningDelta, AgentStarted, AgentFinished, HitlPrompt):
///   affect TUI runtime state but are not stored in message history.
/// - **持久**（User、System、Agent、ToolCall、ToolResult、TurnFinished、Error、Info）：
///   存入消息历史用于展示。
///   **Persistent** (User, System, Agent, ToolCall, ToolResult, TurnFinished, Error, Info):
///   stored in message history for display.
pub enum AgentEvent {
    // ===== 瞬态：流式 / 生命周期 =====
    // ===== Transient: streaming / lifecycle =====
    /// LLM 文本增量。
    /// LLM text delta.
    TextDelta(String),
    /// LLM 推理增量。
    /// LLM reasoning delta.
    ReasoningDelta(String),
    /// Agent 执行开始。
    /// Agent execution started.
    AgentStarted,
    /// Agent 执行结束。
    /// Agent execution finished.
    AgentFinished,
    /// HITL 确认请求——阻塞等待用户响应。
    /// HITL confirmation request — blocks until the user responds.
    ///
    /// `allow_always` 区分两种提示变体：沙箱外路径提示为 true（渲染 [a] 总是授权
    /// 选项并允许 `HitlDecision::Always`）；审批层 Ask 提示为 false（仅 y/n）。
    /// `allow_always` distinguishes two prompt variants: the sandbox out-of-path
    /// prompt sets it to true (renders the [a] always-authorize option and permits
    /// `HitlDecision::Always`); the approval-tier Ask prompt sets it to false (y/n only).
    HitlPrompt {
        tool: String,
        desc: String,
        responder: oneshot::Sender<HitlDecision>,
        allow_always: bool,
    },
    /// 在 TUI 内嵌入 PTY 运行交互式命令（如 sudo），输出实时渲染在 TUI 面板中，
    /// 完成后返回输出给 agent loop。不再离开备用屏幕。
    /// Run an interactive command (e.g., sudo) in an embedded PTY within the
    /// TUI. Output is rendered live in a TUI panel; the alternate screen is
    /// never left. Returns the accumulated output to the agent loop on finish.
    SuspendTui {
        command: String,
        responder: oneshot::Sender<String>,
    },

    // ===== 持久：存入消息历史 =====
    // ===== Persistent: stored in message history =====
    /// 用户输入。
    /// User input.
    User(String),
    /// 系统消息（横幅、状态等）。
    /// System message (banner, status, etc.).
    System(String),
    /// Agent 某阶段的最终输出。
    /// Agent's final output for a given stage.
    Agent(String),
    /// LLM 推理过程（TUI 内部事件）——由 handle_action 中的 flush_reasoning
    /// 直接推入消息历史，不经过 domain channel 传递（与 User / System 同例）。
    /// 在最终答案到达时由累积的 ReasoningDelta 刷出，以折叠块形式展示在答案之前。
    /// LLM reasoning trace (TUI-internal event) — pushed directly into message
    /// history by flush_reasoning inside handle_action, never sent through the
    /// domain channel (same precedent as User / System). Flushed from accumulated
    /// ReasoningDelta when the final answer arrives, shown as a collapsed block
    /// preceding the answer it produced.
    Reasoning(String),
    /// 工具调用通知。`diff` 为 Some 时表示 edit_file 的结构化编辑载荷，
    /// TUI 用它渲染 unified diff；None 时退回 desc 纯文本渲染。
    /// Tool call notification. `diff` is Some when the call is an edit_file
    /// with parsed args, used by the TUI to render a unified diff; None falls
    /// back to the plain-text desc rendering.
    ToolCall {
        name: String,
        desc: String,
        diff: Option<Box<FileEdit>>,
    },
    /// 工具结果通知。
    /// Tool result notification.
    ToolResult {
        name: String,
        result: String,
        ok: bool,
    },
    /// 回合完成，附 token 用量。
    /// Turn finished, with token usage stats.
    TurnFinished { turn: usize, usage: String },
    /// 错误消息。
    /// Error message.
    Error(String),
    /// 信息性消息。
    /// Informational message.
    Info(String),
    /// SDD 管线阶段开始——调查者/规划者/构建者/审计者。
    /// SDD pipeline phase start — investigator/planner/builder/auditor.
    PhaseStart { role: String },
    /// 上下文压缩完成——旧消息被摘要替代以适应 token 预算。
    /// Context compacted — old messages were summarized to fit within the token budget.
    ContextCompacted {
        old_tokens: usize,
        new_tokens: usize,
    },
    /// todo_write 工具更新了任务列表——领域→表示层事件，由侧边栏消费。
    /// The todo_write tool updated the todo list — domain→presentation event,
    /// consumed by the sidebar. Sidebar-only: never rendered in the message stream
    /// (the tool call/result lines already show in-stream).
    TodoUpdate { todos: Vec<TodoItem> },
}

/// 事件 channel 发送端。
/// Event channel sender.
pub type EventSender = mpsc::UnboundedSender<AgentEvent>;
/// 事件 channel 接收端。
/// Event channel receiver.
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;
