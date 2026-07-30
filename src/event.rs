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
    /// 工具调用通知。
    /// Tool call notification.
    ToolCall { name: String, desc: String },
    /// 工具结果通知。
    /// Tool result notification.
    ToolResult { name: String, result: String, ok: bool },
    /// 回合完成，附 token 用量。
    /// Turn finished, with token usage stats.
    TurnFinished { turn: usize, usage: String },
    /// 错误消息。
    /// Error message.
    Error(String),
    /// 信息性消息。
    /// Informational message.
    Info(String),
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
