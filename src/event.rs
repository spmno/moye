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

/// edit_file 工具调用的结构化编辑载荷：渲染 unified diff 用。
/// Structured edit payload for the edit_file tool call: used to render a unified diff.
#[derive(Debug)]
pub struct FileEdit {
    pub path: String,
    pub old: String,
    pub new: String,
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
    HitlPrompt {
        tool: String,
        desc: String,
        responder: oneshot::Sender<bool>,
    },
    /// 暂停 TUI 以运行交互式命令（如 sudo），完成后恢复 TUI 并返回输出。
    /// Suspend the TUI to run an interactive command (e.g., sudo), resume after completion
    /// and return the captured stdout+stderr to the agent loop.
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
}

/// 事件 channel 发送端。
/// Event channel sender.
pub type EventSender = mpsc::UnboundedSender<AgentEvent>;
/// 事件 channel 接收端。
/// Event channel receiver.
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;
