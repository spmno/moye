use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::cli::context::AppContext;
use crate::cli::repl::ReplCommand;
use crate::event::{AgentEvent, EventReceiver, EventSender};
use crate::ui::clipboard;
use crate::ui::selection::Selection;
use crate::ui::selector::{SelectorItem, SelectorState};
use crate::ui::{markdown, theme};
use tracing::{info, warn};

const SPINNER_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
const TICK_MS: u64 = 120;

// ===== Terminal guard (panic-safe cleanup) =====
// ===== 终端守卫（panic 安全清理） =====

use std::sync::OnceLock;

static SAVED_TERMIOS: OnceLock<Option<libc::termios>> = OnceLock::new();

struct TerminalGuard {
    original_termios: Option<libc::termios>,
}

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        let original_termios = {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            let fd = tty_fd();
            let ok = unsafe { libc::tcgetattr(fd, &mut t) } == 0;
            if ok {
                let _ = SAVED_TERMIOS.set(Some(t));
                Some(t)
            } else {
                None
            }
        };

        enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self { original_termios })
    }
}

fn tty_fd() -> i32 {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        libc::STDIN_FILENO
    } else if let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        use std::os::unix::io::IntoRawFd;
        file.into_raw_fd()
    } else {
        libc::STDIN_FILENO
    }
}

fn restore_terminal(original: Option<&libc::termios>) {
    use std::io::Write;

    // Write escape sequences to /dev/tty directly, not stdout.
    // This ensures they reach the terminal even if stdout is redirected.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        let _ = tty.write_all(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?1049l\x1b[?25h");
        let _ = tty.flush();
    }

    // Also write to stdout as fallback.
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = std::io::stdout().flush();

    if let Some(t) = original {
        let fd = tty_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
    }

    let _ = disable_raw_mode();

    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg("stty sane < /dev/tty 2>/dev/null")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(SAVED_TERMIOS.get().and_then(|opt| opt.as_ref()));
        default_hook(info);
    }));
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(self.original_termios.as_ref());
    }
}

// ===== HITL state =====
// ===== HITL（人在环）状态 =====

struct HitlState {
    tool: String,
    desc: String,
    responder: oneshot::Sender<bool>,
}

// ===== Input state (extracted from TuiState) =====
// ===== 输入状态（从 TuiState 中抽取） =====

struct InputState {
    buffer: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
}

impl InputState {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
        }
    }

    fn display_text(&self) -> String {
        self.buffer.clone()
    }

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn insert_str(&mut self, s: &str) {
        self.buffer.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.buffer.floor_char_boundary(self.cursor - 1);
            self.buffer.remove(prev);
            self.cursor = prev;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.buffer.floor_char_boundary(self.cursor - 1);
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.buffer.len() {
            let char_len = self.buffer[self.cursor..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor += char_len;
        }
    }

    fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn cursor_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history_idx = match self.history_idx {
            None => Some(self.history.len() - 1),
            Some(i) if i > 0 => Some(i - 1),
            Some(i) => Some(i),
        };
        if let Some(idx) = self.history_idx {
            self.buffer = self.history[idx].clone();
            self.cursor = self.buffer.len();
        }
    }

    fn history_down(&mut self) {
        if let Some(idx) = self.history_idx {
            if idx + 1 < self.history.len() {
                self.history_idx = Some(idx + 1);
                self.buffer = self.history[idx + 1].clone();
            } else {
                self.history_idx = None;
                self.buffer.clear();
            }
            self.cursor = self.buffer.len();
        }
    }

    fn take_submitted(&mut self) -> Option<String> {
        let typed = self.buffer.trim().to_string();
        if typed.is_empty() {
            return None;
        }
        self.history.push(typed.clone());
        self.history_idx = None;
        self.buffer.clear();
        self.cursor = 0;
        Some(typed)
    }
}

// ===== TUI state =====
// ===== TUI 状态 =====

/// `/models` 供应商级切换流：供应商 → 套餐 → 模型（custom 供应商先输入 base URL）。
/// `/models` provider-level switch flow: provider → plan → model (custom providers
/// prompt for a base URL first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchStage {
    Provider,
    Plan,
    CustomUrl,
    Model,
    ApiKey,
}

struct SwitchFlow {
    stage: SwitchStage,
    provider: &'static crate::ui::setup::ProviderEntry,
    plan: crate::providers::ApiPlan,
    base_url: Option<String>,
    /// 模型页已选定、等待 API key 输入时暂存的模型 slug。
    /// The model slug chosen on the model page while waiting for the API key input.
    pending_model: Option<String>,
}

impl SwitchFlow {
    /// 启动供应商选择页。
    /// Start at the provider picker.
    fn start() -> (Self, SelectorState) {
        let items: Vec<SelectorItem> = crate::ui::setup::PROVIDERS
            .iter()
            .map(|p| SelectorItem {
                label: p.label.to_string(),
                detail: p.detail.to_string(),
                data: Some(p.slug.to_string()),
            })
            .collect();
        let selector = SelectorState::new(
            "Select Provider / 选择供应商".into(),
            items,
            false,
        );
        (
            Self {
                stage: SwitchStage::Provider,
                // 占位：Provider 页选中后立即覆盖；PROVIDERS 非空。
                provider: &crate::ui::setup::PROVIDERS[0],
                plan: crate::providers::ApiPlan::Standard,
                base_url: None,
                pending_model: None,
            },
            selector,
        )
    }

    fn provider_enum(&self) -> crate::providers::Provider {
        crate::providers::parse_provider(self.provider.slug)
    }

    /// 进入套餐选择页；供应商只有一个套餐时自动跳到模型页。
    /// Open the plan picker; auto-advances to the model picker when the provider
    /// has only one plan.
    fn goto_plan(&mut self) -> Option<SelectorState> {
        let provider = self.provider_enum();
        let plans = provider.supported_plans();
        if plans.len() <= 1 {
            self.plan = crate::providers::ApiPlan::Standard;
            return self.goto_model();
        }
        let items: Vec<SelectorItem> = plans
            .iter()
            .map(|p| SelectorItem {
                label: p.label().to_string(),
                detail: crate::ui::setup::plan_detail(provider, *p).to_string(),
                data: Some(p.slug().to_string()),
            })
            .collect();
        self.stage = SwitchStage::Plan;
        Some(SelectorState::new(
            "Select Plan / 选择套餐".into(),
            items,
            false,
        ))
    }

    /// 进入模型选择页（custom 供应商无内置清单，靠输入自定义模型 ID）。
    /// Open the model picker (custom has no catalog; the typed filter becomes the model ID).
    fn goto_model(&mut self) -> Option<SelectorState> {
        let provider = self.provider_enum();
        let models = crate::providers::provider_models_for_plan(provider, self.plan);
        let items: Vec<SelectorItem> = models
            .iter()
            .map(|m| SelectorItem {
                label: m.slug.clone(),
                detail: m.desc.to_string(),
                ..Default::default()
            })
            .collect();
        self.stage = SwitchStage::Model;
        Some(SelectorState::new(
            format!("Select Model / 选择模型 ({})", self.provider.label),
            items,
            true,
        ))
    }

    /// 进入 API key 输入页（空列表 + 自定义输入，回车提交输入内容）。
    /// Open the API key input page (empty list + custom input; Enter submits the typed value).
    fn goto_api_key(&mut self) -> SelectorState {
        self.stage = SwitchStage::ApiKey;
        SelectorState::new(
            format!(
                "API Key / 输入 {} 后回车（Esc 返回）",
                self.provider.api_key_env
            ),
            vec![],
            true,
        )
    }
}

struct TuiState {
    messages: Vec<AgentEvent>,
    input: InputState,
    streaming: String,
    streaming_reasoning: String,
    thinking: bool,
    spinner: usize,
    hitl: Option<HitlState>,
    scroll_offset: u16,
    /// 用户是否手动上翻了消息区。为 true 时，新事件不会自动跳到底部。
    /// Whether the user has manually scrolled up the message area. When true, new events do NOT auto-jump to the bottom.
    user_scrolled: bool,
    should_quit: bool,
    provider: String,
    model: String,
    max_turns: usize,
    current_turn: usize,
    total_tokens: u64,
    last_usage: String,
    tool_names: Vec<String>,
    mcp_servers: Vec<crate::mcp::McpServerDisplay>,
    skill_names: Vec<String>,
    /// 当前运行中的后台任务句柄。按 Esc 可 abort 中断。
    /// Handle to the currently running background task. Press Esc to abort.
    task_handle: Option<JoinHandle<()>>,
    needs_full_redraw: bool,
    /// 打开中的选择器（如 /models 模型选择）。非 None 时键盘输入进入选择器。
    /// Open selector (e.g. the /models model picker). When Some, keyboard input goes to it.
    selector: Option<SelectorState>,
    /// `/models` 供应商级切换流状态；非 None 时选择器 Enter 结果进入下一级（供应商→套餐→模型）。
    /// `/models` provider-level switch flow; when Some, selector Enter results advance
    /// through the stages (provider → plan → model).
    switch_flow: Option<SwitchFlow>,
    /// 当前文本选区（鼠标拖拽产生）。
    /// Current text selection (produced by mouse drag).
    selection: Option<Selection>,
    /// 消息内容区 Rect：draw_messages 时写入（Block::inner 后），handle_mouse_event 命中测试时读。
    /// Message content Rect: written in draw_messages (after Block::inner), read in
    /// handle_mouse_event for hit-testing. Stores the actual content area, not the bordered area.
    msg_area: Rect,
    /// 消息区 Paragraph scroll 值（跳过的顶部行数），draw 时存，mouse 事件时读以算屏幕行→逻辑行。
    /// Message Paragraph scroll (top lines skipped); written at draw time, read at
    /// mouse-event time for screen-row→logical-line mapping.
    msg_scroll: u16,
}

impl TuiState {
    fn new(
        provider: String,
        model: String,
        max_turns: usize,
        tool_names: Vec<String>,
        mcp_servers: Vec<crate::mcp::McpServerDisplay>,
        skill_names: Vec<String>,
    ) -> Self {
        Self {
            messages: Vec::new(),
            input: InputState::new(),
            streaming: String::new(),
            streaming_reasoning: String::new(),
            thinking: false,
            spinner: 0,
            hitl: None,
            scroll_offset: 0,
            user_scrolled: false,
            should_quit: false,
            provider,
            model,
            max_turns,
            current_turn: 0,
            total_tokens: 0,
            last_usage: String::new(),
            tool_names,
            mcp_servers,
            skill_names,
            task_handle: None,
            needs_full_redraw: false,
            selector: None,
            switch_flow: None,
            selection: None,
            msg_area: Rect::new(0, 0, 0, 0),
            msg_scroll: 0,
        }
    }

    fn tick(&mut self) {
        if self.thinking {
            self.spinner = (self.spinner + 1) % SPINNER_FRAMES.len();
        }
    }

    fn all_message_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for msg in &self.messages {
            lines.extend(render_event(msg));
        }
        lines
    }

    /// 将事件推入消息历史，同时写入 tracing 文件日志。
    /// 这样文件 log 与终端 TUI 显示的内容保持一致。
    fn push_event(&mut self, event: AgentEvent) {
        log_event(&event);
        self.messages.push(event);
    }

    /// 新事件到达时调用：仅在用户未手动上翻时自动滚到底部。
    /// Called when a new event arrives: only auto-scrolls to bottom if the user hasn't manually scrolled up.
    fn reset_scroll(&mut self) {
        if !self.user_scrolled {
            self.scroll_offset = 0;
        }
    }
}

/// 将 AgentEvent 写入 tracing 文件日志，使文件 log 与终端输出保持一致。
fn log_event(event: &AgentEvent) {
    match event {
        AgentEvent::User(text) => {
            info!("[TUI] 用户: {text}");
        }
        AgentEvent::System(text) => {
            info!("[TUI] 系统: {text}");
        }
        AgentEvent::Agent(text) => {
            info!("[TUI] Agent 输出:\n{text}");
        }
        AgentEvent::ToolCall { name, desc } => {
            info!("[TUI] 工具调用: {name} | {desc}");
        }
        AgentEvent::ToolResult { name, result, ok } => {
            if *ok {
                info!("[TUI] 工具结果 ✓ {name}: {result}");
            } else {
                warn!("[TUI] 工具结果 ✗ {name}: {result}");
            }
        }
        AgentEvent::TurnFinished { turn, usage } => {
            info!("[TUI] 回合 {turn} 完成 | {usage}");
        }
        AgentEvent::Error(text) => {
            warn!("[TUI] 错误: {text}");
        }
        AgentEvent::Info(text) => {
            if !text.is_empty() {
                info!("[TUI] 信息: {text}");
            }
        }
        AgentEvent::AgentStarted => {
            info!("[TUI] === Agent 开始执行 ===");
        }
        AgentEvent::AgentFinished => {
            info!("[TUI] === Agent 执行结束 ===");
        }
        AgentEvent::HitlPrompt { tool, desc, .. } => {
            info!("[TUI] HITL 确认请求: {tool} | {desc}");
        }
        AgentEvent::SuspendTui { command, .. } => {
            info!(
                "[TUI] \u{6682}\u{505c} TUI \u{8fd0}\u{884c}\u{4ea4}\u{4e92}\u{5f0f}\u{547d}\u{4ee4}: {command}"
            );
        }
        AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => {}
        AgentEvent::ContextCompacted {
            old_tokens,
            new_tokens,
        } => {
            info!("[TUI] 上下文压缩: {old_tokens} → {new_tokens} tokens");
        }
    }
}

// ===== Event rendering (replaces TuiMessage::to_lines) =====
// ===== 事件渲染（替代 TuiMessage::to_lines） =====

fn render_event(event: &AgentEvent) -> Vec<Line<'static>> {
    match event {
        AgentEvent::User(text) => {
            let mut v = vec![Line::styled(format!("\u{276f} {text}"), theme::user_msg())];
            v.push(Line::default());
            v
        }
        AgentEvent::System(text) => {
            let mut v = vec![Line::styled(text.clone(), theme::system())];
            v.push(Line::default());
            v
        }
        AgentEvent::Agent(text) => {
            let rendered = markdown::render_markdown(text);
            rendered.into_iter().collect()
        }
        AgentEvent::ToolCall { name, desc } => {
            let mut v = vec![Line::from(vec![
                Span::styled("\u{1f527} ", theme::tool_call()),
                Span::styled(format!("{name}: {desc}"), theme::tool_call()),
            ])];
            v.push(Line::default());
            v
        }
        AgentEvent::ToolResult { name, result, ok } => {
            let icon = if *ok { "\u{2713}" } else { "\u{2717}" };
            let sty = if *ok {
                theme::tool_result_ok()
            } else {
                theme::tool_result_err()
            };
            let trunc = if result.len() > 500 {
                format!("{}\u{2026}", &result[..result.floor_char_boundary(500)])
            } else {
                result.clone()
            };
            let mut v = vec![];
            v.push(Line::from(vec![Span::styled(
                format!("{icon} {name}"),
                sty,
            )]));
            for line in trunc.lines().take(15) {
                v.push(Line::from(Span::raw(line.to_string())));
            }
            v.push(Line::default());
            v
        }
        AgentEvent::TurnFinished { turn, usage } => {
            let mut v = vec![
                Line::styled(
                    format!(
                        "\u{2500}\u{2500}\u{2500} \u{8f6e}\u{6b21} {turn} \u{5b8c}\u{6210} \u{2500}\u{2500}\u{2500}"
                    ),
                    theme::usage(),
                ),
                Line::styled(format!("  {usage}"), theme::usage()),
            ];
            v.push(Line::default());
            v
        }
        AgentEvent::Error(text) => {
            let mut v = vec![Line::styled(format!("\u{2717} {text}"), theme::error())];
            v.push(Line::default());
            v
        }
        AgentEvent::Info(text) => {
            if text.is_empty() {
                vec![Line::default()]
            } else {
                // 按 \n 拆分为多行，避免 ratatui Line 不识别换行符导致
                // 整段文本挤在一行、超出终端宽度后被截断。
                // Split on \n into separate Lines: ratatui's Line does not
                // honor embedded newlines, so a multi-line string in a single
                // Line gets squashed into one visual row and truncated at the
                // terminal edge.
                let mut v: Vec<Line<'static>> = text
                    .split('\n')
                    .map(|line| Line::styled(line.to_owned(), theme::info()))
                    .collect();
                v.push(Line::default());
                v
            }
        }
        _ => vec![],
    }
}

/// 将 AgentEvent 格式化为一行摘要，用于 `/context` 命令输出。
/// Format an AgentEvent as a one-line summary for the `/context` command output.
fn format_event_for_context(event: &AgentEvent) -> String {
    match event {
        AgentEvent::User(text) => {
            format!("[User] {}", truncate_ctx(text, 120))
        }
        AgentEvent::System(text) => {
            format!("[System] {}", truncate_ctx(text, 120))
        }
        AgentEvent::Agent(text) => {
            format!("[Agent] {}", truncate_ctx(text, 200))
        }
        AgentEvent::ToolCall { name, desc } => {
            format!("[ToolCall] {name}: {}", truncate_ctx(desc, 120))
        }
        AgentEvent::ToolResult { name, result, ok } => {
            let icon = if *ok { "✓" } else { "✗" };
            format!("[ToolResult] {icon} {name}: {}", truncate_ctx(result, 120))
        }
        AgentEvent::TurnFinished { turn, usage } => {
            format!("[TurnFinished] turn {turn} | {usage}")
        }
        AgentEvent::Error(text) => {
            format!("[Error] {}", truncate_ctx(text, 120))
        }
        AgentEvent::Info(text) => {
            if text.is_empty() {
                "[Info]".to_string()
            } else {
                format!("[Info] {}", truncate_ctx(text, 120))
            }
        }
        AgentEvent::ContextCompacted {
            old_tokens,
            new_tokens,
        } => {
            format!("[ContextCompacted] {old_tokens} → {new_tokens} tokens")
        }
        AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => String::new(),
        AgentEvent::AgentStarted => "[AgentStarted]".to_string(),
        AgentEvent::AgentFinished => "[AgentFinished]".to_string(),
        AgentEvent::HitlPrompt { .. } | AgentEvent::SuspendTui { .. } => String::new(),
    }
}

/// 截断字符串用于上下文摘要显示（按字符数截断，追加省略号）。
/// Truncate a string for context summary display (by char count, with ellipsis).
fn truncate_ctx(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.replace('\n', " ")
    } else {
        let end = s
            .char_indices()
            .take(max_chars)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("{}…", s[..end].replace('\n', " "))
    }
}

// ===== Entry point =====
// ===== 入口 =====

pub async fn run_tui(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    let provider = format!("{:?}", crate::providers::current_provider());
    let model = ctx.current_model();
    let max_turns = ctx.registry.max_turns();
    let tool_names: Vec<String> = crate::tools::tool_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mcp_servers = ctx.registry.mcp_server_displays();
    let skill_names: Vec<String> = crate::skills::SkillManifest::load()
        .map(|m| m.list())
        .unwrap_or_default();

    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<AgentEvent>();

    let mut state = TuiState::new(
        provider,
        model,
        max_turns,
        tool_names,
        mcp_servers,
        skill_names,
    );
    state.push_event(AgentEvent::System(format!(
        "moye ({}) | model: {}",
        state.provider, state.model
    )));
    state.push_event(AgentEvent::Info(
        "Enter \u{53d1}\u{9001}\u{4efb}\u{52a1} | /help \u{5e2e}\u{52a9} | Esc \u{4e2d}\u{65ad}\u{4efb}\u{52a1} | Ctrl+C \u{9000}\u{51fa}".into(),
    ));

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(TICK_MS));

    let result = run_loop(
        &mut terminal,
        &mut state,
        &ctx,
        &action_tx,
        &mut action_rx,
        &mut events,
        &mut tick,
    )
    .await;

    drop(events);
    drop(tick);

    restore_terminal(SAVED_TERMIOS.get().and_then(|opt| opt.as_ref()));

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut TuiState,
    ctx: &Arc<AppContext>,
    action_tx: &EventSender,
    action_rx: &mut EventReceiver,
    events: &mut EventStream,
    tick: &mut tokio::time::Interval,
) -> anyhow::Result<()> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        if state.needs_full_redraw {
            if let Ok(size) = terminal.size() {
                let _ = terminal.resize(Rect::new(0, 0, size.width, size.height));
            }
            state.needs_full_redraw = false;
        }
        terminal.draw(|f| draw(f, state))?;

        tokio::select! {
            Some(Ok(event)) = events.next() => {
                match event {
                    crossterm::event::Event::Key(key) => {
                        handle_key_event(key, state, ctx, action_tx);
                    }
                    crossterm::event::Event::Mouse(mouse) => {
                        handle_mouse_event(mouse, state);
                    }
                    crossterm::event::Event::Paste(text)
                        if !state.thinking && state.hitl.is_none() =>
                    {
                        let text = text.replace("\r\n", "\n").replace('\r', "\n");
                        // 选择器（/models 切换流，含 API key 输入页）打开时粘贴进过滤/输入框。
                        // Paste into the filter/input box while a selector (the /models
                        // switch flow, incl. the API key page) is open.
                        if let Some(sel) = state.selector.as_mut() {
                            sel.input_paste(text.trim());
                        } else {
                            state.input.insert_str(&text);
                        }
                    }
                    _ => {}
                }
            }
            Some(action) = action_rx.recv() => {
                handle_action(action, state);
            }
            _ = tick.tick() => {
                state.tick();
            }
            _ = &mut ctrl_c => {
                state.should_quit = true;
            }
        }

        if state.should_quit {
            break;
        }
    }
    Ok(())
}

// ===== Key handling =====
// ===== 按键处理 =====

fn handle_key_event(
    key: KeyEvent,
    state: &mut TuiState,
    ctx: &Arc<AppContext>,
    action_tx: &EventSender,
) {
    if state.hitl.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(h) = state.hitl.take() {
                    let _ = h.responder.send(true);
                    state.push_event(AgentEvent::Info(format!(
                        "\u{26a0} \u{5141}\u{8bb8}\u{6267}\u{884c} {}",
                        h.tool
                    )));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Some(h) = state.hitl.take() {
                    let _ = h.responder.send(false);
                    state.push_event(AgentEvent::Info(format!(
                        "\u{26a0} \u{62d2}\u{7edd}\u{6267}\u{884c} {}",
                        h.tool
                    )));
                }
            }
            _ => {}
        }
        return;
    }

    // Selector（命令面板）模式：优先于常规输入与 Esc 中断。
    // Selector (command palette) mode: takes priority over regular input and Esc interrupt.
    if state.selector.is_some() {
        // Ctrl+C/D 在面板中视为取消，避免误退出程序。
        // Ctrl+C/D cancels the panel instead of quitting the program.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
        {
            state.selector = None;
            return;
        }
        match key.code {
            KeyCode::Up => {
                if let Some(s) = &mut state.selector {
                    s.move_cursor(-1);
                }
            }
            KeyCode::Down => {
                if let Some(s) = &mut state.selector {
                    s.move_cursor(1);
                }
            }
            KeyCode::Enter => {
                let selected = state.selector.as_ref().and_then(|s| s.selection());
                // 供应商级切换流：把选择结果交给流程状态机推进，不在此关闭选择器。
                // Provider switch flow: hand the result to the flow state machine;
                // it replaces the selector with the next stage (or closes it).
                if state.switch_flow.is_some() {
                    if let Some(item) = selected {
                        handle_switch_select(state, ctx, item);
                    }
                    return;
                }
                state.selector = None;
                if let Some(item) = selected {
                    // 历史项在 data 里编码了 "provider\nbase_url"；解码后连同恢复，否则只切 slug。
                    // History items encode "provider\nbase_url" in data; decode and restore
                    // together, otherwise only switch the slug.
                    let (provider, base_url) = item
                        .data
                        .as_ref()
                        .and_then(|s| {
                            let mut it = s.splitn(2, '\n');
                            let p = it.next()?.to_string();
                            let b = it.next()?.to_string();
                            Some((Some(p), Some(b)))
                        })
                        .unwrap_or((None, None));
                    ctx.cmd_model(Some(item.label.clone()), provider, base_url);
                    state.model = ctx.current_model();
                    state.push_event(AgentEvent::Info(format!("model: {}", state.model)));
                }
            }
            KeyCode::Esc => {
                // 切换流中 Esc 逐级返回：key→模型；模型/套餐/URL 页→供应商页；供应商页→退出。
                // In the switch flow, Esc goes back one stage: key→model; model/plan/URL
                // pages → provider picker; provider page → cancel.
                if let Some(flow) = &state.switch_flow {
                    match flow.stage {
                        SwitchStage::Provider => {
                            state.switch_flow = None;
                            state.selector = None;
                        }
                        SwitchStage::ApiKey => {
                            if let Some(flow) = state.switch_flow.as_mut() {
                                flow.pending_model = None;
                                state.selector = flow.goto_model();
                            }
                        }
                        _ => {
                            let (flow, selector) = SwitchFlow::start();
                            state.switch_flow = Some(flow);
                            state.selector = Some(selector);
                        }
                    }
                    return;
                }
                state.selector = None;
            }
            KeyCode::Backspace => {
                if let Some(s) = &mut state.selector {
                    s.backspace();
                }
            }
            KeyCode::Char(c) => {
                if let Some(s) = &mut state.selector {
                    s.input_char(c);
                }
            }
            _ => {}
        }
        return;
    }

    // Esc: interrupt the running task (only when thinking and not in HITL mode).
    // Esc：中断正在运行的任务（仅在 thinking 且非 HITL 模式时生效）。
    if key.code == KeyCode::Esc && state.thinking {
        // Safety net: clear any HITL prompt that may have arrived after abort
        // (race condition: HitlPrompt event still in channel buffer).
        // 安全兜底：清除 abort 后可能到达的 HITL 提示（竞态：HitlPrompt 仍在 channel 缓冲区中）。
        if let Some(h) = state.hitl.take() {
            let _ = h.responder.send(false);
        }
        if let Some(handle) = state.task_handle.take() {
            handle.abort();
        }
        state.thinking = false;
        state.streaming.clear();
        state.streaming_reasoning.clear();
        state.push_event(AgentEvent::Info(
            "\u{26a0} \u{4efb}\u{52a1}\u{5df2}\u{4e2d}\u{65ad} (Esc)".into(),
        ));
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => {
                state.should_quit = true;
                return;
            }
            KeyCode::Char('y') => {
                copy_selection(state);
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => {
            if !state.thinking
                && let Some(input) = state.input.take_submitted()
            {
                handle_command(input, state, ctx, action_tx);
            }
        }
        KeyCode::Char(c) => {
            state.input.insert_char(c);
        }
        KeyCode::Backspace => {
            state.input.backspace();
        }
        KeyCode::Delete => {
            state.input.delete();
        }
        KeyCode::Left => {
            state.input.cursor_left();
        }
        KeyCode::Right => {
            state.input.cursor_right();
        }
        KeyCode::Home => {
            state.input.cursor_home();
        }
        KeyCode::End => {
            state.input.cursor_end();
        }
        KeyCode::Up => {
            state.input.history_up();
        }
        KeyCode::Down => {
            state.input.history_down();
        }
        KeyCode::PageUp => {
            state.scroll_offset = state.scroll_offset.saturating_add(5);
            state.user_scrolled = true;
        }
        KeyCode::PageDown => {
            state.scroll_offset = state.scroll_offset.saturating_sub(5);
            if state.scroll_offset == 0 {
                state.user_scrolled = false;
            }
        }
        _ => {}
    }
}

fn copy_selection(state: &mut TuiState) {
    if let Some(sel) = state.selection.take() {
        // 选区坐标是"显示行"索引，须用与渲染一致的软换行结果来提取，
        // 否则长行换行后索引与文本错位。
        // Selection coords are display-line indices; extract against the same
        // soft-wrapped layout used for rendering, or indices misalign with text
        // once long lines wrap.
        let lines = crate::ui::wrap::wrap_lines(&state.all_message_lines(), state.msg_area.width);
        let text = sel.extract(&lines);
        if text.is_empty() {
            return;
        }
        if clipboard::copy_to_clipboard(&text) {
            state.push_event(AgentEvent::Info(
                "\u{2713} \u{5df2}\u{590d}\u{5236}\u{9009}\u{533a}".into(),
            ));
        } else {
            state.push_event(AgentEvent::Info(
                "\u{2717} \u{590d}\u{5236}\u{5931}\u{8d25}\u{ff1a}\u{672a}\u{68c0}\u{6d4b}\u{5230}\u{53ef}\u{7528}\u{7684}\u{526a}\u{8d34}\u{677f}\u{3002}\u{8bf7}\u{5b89}\u{88c5} wl-clipboard / xclip / xsel \u{540e}\u{91cd}\u{8bd5}\u{ff08}\u{6216}\u{786e}\u{8ba4}\u{7ec8}\u{7aef}\u{652f}\u{6301} OSC52\u{ff09}\u{3002}".into(),
            ));
        }
    }
}

fn handle_mouse_event(mouse: MouseEvent, state: &mut TuiState) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll_offset = state.scroll_offset.saturating_add(3);
            state.user_scrolled = true;
        }
        MouseEventKind::ScrollDown => {
            state.scroll_offset = state.scroll_offset.saturating_sub(3);
            if state.scroll_offset == 0 {
                state.user_scrolled = false;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let row = mouse.row;
            let col = mouse.column;
            let inner = state.msg_area;
            if row >= inner.y && row < inner.y.saturating_add(inner.height) {
                let logical = Selection::screen_to_logical(row, inner.y, state.msg_scroll);
                let rel_col = col.saturating_sub(inner.x);
                state.selection = Some(Selection::new_at(logical, rel_col));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let row = mouse.row;
            let col = mouse.column;
            let inner = state.msg_area;
            let rel_col = col.saturating_sub(inner.x);
            if row < inner.y {
                // 拖出顶部：向上滚一行，焦点=滚后可见首行
                state.scroll_offset = state.scroll_offset.saturating_add(1);
                state.user_scrolled = true;
                let new_scroll = state.msg_scroll.saturating_sub(1);
                let logical = Selection::screen_to_logical(inner.y, inner.y, new_scroll);
                if let Some(sel) = state.selection.as_mut() {
                    sel.extend(logical, rel_col);
                }
            } else if row >= inner.y.saturating_add(inner.height) {
                // 拖出底部：向下滚一行，焦点=滚后可见末行
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
                let new_scroll = state.msg_scroll.saturating_add(1);
                let bottom = inner.y.saturating_add(inner.height).saturating_sub(1);
                let logical = Selection::screen_to_logical(bottom, inner.y, new_scroll);
                if let Some(sel) = state.selection.as_mut() {
                    sel.extend(logical, rel_col);
                }
            } else {
                let logical = Selection::screen_to_logical(row, inner.y, state.msg_scroll);
                if let Some(sel) = state.selection.as_mut() {
                    sel.extend(logical, rel_col);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            copy_selection(state);
        }
        _ => {}
    }
}

fn handle_command(
    input: String,
    state: &mut TuiState,
    ctx: &Arc<AppContext>,
    action_tx: &EventSender,
) {
    match ReplCommand::parse(&input) {
        ReplCommand::Quit => {
            state.should_quit = true;
        }
        ReplCommand::Trust => {
            let flag = ctx.orchestrator.trust_sandbox();
            let new_val = !flag.load(std::sync::atomic::Ordering::Relaxed);
            flag.store(new_val, std::sync::atomic::Ordering::Relaxed);
            if new_val {
                state.push_event(AgentEvent::Info(
                    "🔒 沙箱信任模式已开启：沙箱外访问将自动授权，不再弹窗确认。".into(),
                ));
            } else {
                state.push_event(AgentEvent::Info(
                    "🔒 沙箱信任模式已关闭：沙箱外访问将恢复弹窗确认。".into(),
                ));
            }
        }
        ReplCommand::Goal(goal) => {
            state.push_event(AgentEvent::User(goal.clone()));
            state.thinking = true;
            state.streaming.clear();
            state.streaming_reasoning.clear();
            let _ = action_tx.send(AgentEvent::AgentStarted);

            let ctx = Arc::clone(ctx);
            let tx = action_tx.clone();
            let handle = tokio::spawn(async move {
                ctx.run_goal_tui(&goal, &tx).await;
                let _ = tx.send(AgentEvent::AgentFinished);
            });
            state.task_handle = Some(handle);
        }
        ReplCommand::Model { slug } => {
            ctx.cmd_model(slug, None, None);
            state.model = ctx.current_model();
            state.push_event(AgentEvent::Info(format!("model: {}", state.model)));
        }
        ReplCommand::Models => {
            // 供应商级切换流：供应商 → 套餐 → 模型（即时生效 + .env 持久化）。
            // Provider-level switch flow: provider → plan → model (applies live, persists to .env).
            let (flow, selector) = SwitchFlow::start();
            state.switch_flow = Some(flow);
            state.selector = Some(selector);
        }
        ReplCommand::Plan { plan } => {
            let msg = ctx.cmd_plan(plan);
            state.push_event(AgentEvent::Info(msg));
        }
        ReplCommand::Context => {
            let mut out = format!(
                "─── 上下文 / Context ───\n\
                 Provider: {}\n\
                 Model: {}\n\
                 Turn: {} / {}\n\
                 Total tokens: {}\n\
                 Last usage: {}\n\
                 Tools: {}\n\
                 Skills: {}\n\
                 Messages: {}\n",
                state.provider,
                state.model,
                state.current_turn,
                state.max_turns,
                state.total_tokens,
                if state.last_usage.is_empty() {
                    "N/A"
                } else {
                    &state.last_usage
                },
                state.tool_names.len(),
                state.skill_names.len(),
                state.messages.len(),
            );
            out.push_str("─── 消息历史 / Message History ───\n");
            for (i, msg) in state.messages.iter().enumerate() {
                let line = format_event_for_context(msg);
                out.push_str(&format!("  {}. {}\n", i + 1, line));
            }
            state.push_event(AgentEvent::Info(out));
        }
        ReplCommand::Help => {
            state.push_event(AgentEvent::Info(ctx.cmd_help()));
        }
        ReplCommand::Skills => {
            state.push_event(AgentEvent::Info(ctx.cmd_list_skills()));
        }
        ReplCommand::History { limit } => {
            state.push_event(AgentEvent::Info(ctx.cmd_history(limit)));
        }
        ReplCommand::Lessons => {
            state.push_event(AgentEvent::Info(ctx.cmd_list_lessons()));
        }
        ReplCommand::Evolve => {
            state.thinking = true;
            let _ = action_tx.send(AgentEvent::AgentStarted);
            let ctx = Arc::clone(ctx);
            let tx = action_tx.clone();
            let handle = tokio::spawn(async move {
                let result = ctx.cmd_evolve(&tx).await;
                let _ = tx.send(AgentEvent::Info(result));
                let _ = tx.send(AgentEvent::AgentFinished);
            });
            state.task_handle = Some(handle);
        }
        ReplCommand::EvolveCode { file, old, new } => {
            let msg = ctx.cmd_evolve_code(&file, &old, &new);
            state.push_event(AgentEvent::Info(msg));
        }
        ReplCommand::AddTool { name, description } => {
            let msg = ctx.cmd_add_tool(&name, &description);
            state.push_event(AgentEvent::Info(msg));
        }
        ReplCommand::AddSkill { name, description } => {
            let msg = ctx.cmd_add_skill(&name, &description);
            state.push_event(AgentEvent::Info(msg));
        }
        ReplCommand::InvalidUsage(msg) => {
            state.push_event(AgentEvent::Error(msg.to_string()));
        }
    }
}

/// 处理 `/models` 供应商切换流中选择器 Enter 的结果，按当前阶段推进或完成切换。
/// Handles a selector Enter in the `/models` provider switch flow: advances the
/// stage, or finalizes the switch at the model stage.
fn handle_switch_select(state: &mut TuiState, ctx: &Arc<AppContext>, item: SelectorItem) {
    let Some(flow) = state.switch_flow.as_mut() else {
        state.selector = None;
        return;
    };
    match flow.stage {
        SwitchStage::Provider => {
            let slug = item.data.as_deref().unwrap_or("");
            let Some(entry) = crate::ui::setup::PROVIDERS
                .iter()
                .find(|p| p.slug == slug)
            else {
                state.switch_flow = None;
                state.selector = None;
                return;
            };
            flow.provider = entry;
            flow.plan = crate::providers::ApiPlan::Standard;
            flow.base_url = None;
            if entry.slug == "custom" {
                // custom 无内置目录：先让用户输入 base URL，再输入模型 ID。
                // custom has no catalog: prompt for base URL, then the model ID.
                flow.stage = SwitchStage::CustomUrl;
                state.selector = Some(SelectorState::new(
                    "Custom Base URL / 自定义网关（输入完整 URL 后回车）".into(),
                    vec![],
                    true,
                ));
            } else if let Some(next) = flow.goto_plan() {
                state.selector = Some(next);
            }
        }
        SwitchStage::Plan => {
            if let Some(plan_slug) = item.data.as_deref() {
                flow.plan = crate::providers::ApiPlan::parse(plan_slug);
            }
            state.selector = flow.goto_model();
        }
        SwitchStage::CustomUrl => {
            let url = item.label.trim().to_string();
            if url.is_empty() {
                return;
            }
            flow.base_url = Some(url);
            state.selector = Some(SelectorState::new(
                "Custom Model ID / 模型 ID（输入后回车）".into(),
                vec![],
                true,
            ));
            flow.stage = SwitchStage::Model;
        }
        SwitchStage::Model => {
            let model = item.label.trim().to_string();
            if model.is_empty() {
                return;
            }
            let api_key_env = flow.provider.api_key_env;
            let key_present = std::env::var(api_key_env).is_ok()
                || crate::config::config()
                    .map(|c| c.keys.contains_key(api_key_env))
                    .unwrap_or(false);
            if key_present {
                let flow = state.switch_flow.take().unwrap();
                state.selector = None;
                finalize_switch(state, ctx, &flow, model, None);
            } else {
                flow.pending_model = Some(model);
                state.selector = Some(flow.goto_api_key());
            }
        }
        SwitchStage::ApiKey => {
            let key = item.label.trim().to_string();
            if key.is_empty() {
                return;
            }
            let model = flow.pending_model.clone().unwrap_or_default();
            let flow = state.switch_flow.take().unwrap();
            state.selector = None;
            finalize_switch(state, ctx, &flow, model, Some(key));
        }
    }
}

/// 切换流终点：应用 session 级 provider/base_url/model 覆盖（即时生效），
/// 持久化到 .env（重启保持）；用户输入了 API key 时一并写入并注入当前进程环境。
/// Flow endpoint: applies session-level provider/base_url/model overrides (live),
/// persists to .env (survives restart); when the user entered an API key, writes it
/// to .env and injects it into the current process env so it works without restart.
fn finalize_switch(
    state: &mut TuiState,
    ctx: &Arc<AppContext>,
    flow: &SwitchFlow,
    model: String,
    api_key: Option<String>,
) {
    let provider_enum = flow.provider_enum();
    let slug = flow.provider.slug;
    let base_url = flow
        .base_url
        .clone()
        .unwrap_or_else(|| provider_enum.base_url_for_plan(flow.plan).to_string());
    let api_key_env = flow.provider.api_key_env;

    ctx.registry.set_session_provider(slug);
    ctx.registry.set_session_base_url(&base_url);
    ctx.registry.set_session_model(&model);

    {
        let mut hist = ctx.model_history.lock().unwrap();
        hist.record(&model, slug, &base_url);
        let _ = hist.save();
    }

    let mut env_err: Option<String> = None;
    if let Err(e) = persist_switch_to_env(slug, flow.plan, flow.base_url.as_deref()) {
        env_err = Some(e.to_string());
    }
    if let Some(ref key) = api_key {
        if let Err(e) = persist_key_to_env(api_key_env, key) {
            env_err = Some(e.to_string());
        } else {
            // 注入当前进程环境：create_client_with 从 env 读 key，本会话立即生效，无需重启。
            // Inject into the current process env: create_client_with reads the key from
            // env, so the new key takes effect this session without a restart.
            unsafe {
                std::env::set_var(api_key_env, key);
            }
        }
    }

    state.provider = format!("{provider_enum:?}");
    state.model = ctx.current_model();
    match env_err {
        Some(e) => state.push_event(AgentEvent::Info(format!(
            "已切换到 {} / {}（.env 写入失败: {e}；本次会话生效）",
            flow.provider.label, state.model
        ))),
        None => state.push_event(AgentEvent::Info(format!(
            "provider: {} ({}) | model: {} | base_url: {}",
            flow.provider.label,
            flow.plan.label(),
            state.model,
            base_url,
        ))),
    }
    if api_key.is_some() {
        state.push_event(AgentEvent::Info(format!(
            "API key 已保存到 .env（{api_key_env}），本次会话已生效。"
        )));
    } else if std::env::var(api_key_env).is_err() {
        state.push_event(AgentEvent::Info(format!(
            "⚠ 未检测到 {api_key_env}。请在项目根 .env 中添加 `{api_key_env}=<你的 key>` 后重启 moye。"
        )));
    }
}

/// 把 API key 写入项目根 `.env`：更新或新增 `<KEY_ENV>=<key>` 行，不触碰其他行。
/// Writes the API key to the project-root `.env`: updates or adds the `<KEY_ENV>=<key>`
/// line, leaving every other line untouched.
fn persist_key_to_env(key_env: &str, key: &str) -> std::io::Result<()> {
    let path = ".env";
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut written = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#')
            && trimmed.contains('=')
            && trimmed.split('=').next().unwrap_or("").trim() == key_env
        {
            out.push_str(&format!("{key_env}={key}\n"));
            written = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !written {
        out.push_str(&format!("{key_env}={key}\n"));
    }
    std::fs::write(path, out)
}

/// 把切换结果写入项目根 `.env`：更新 AGENT_PROVIDER / AGENT_PLAN / AGENT_BASE_URL 行，
/// 不触碰任何 API key 行。文件不存在时创建。
/// Persists the switch to the project-root `.env`: updates AGENT_PROVIDER /
/// AGENT_PLAN / AGENT_BASE_URL lines, never touches API-key lines. Creates the file if missing.
fn persist_switch_to_env(
    provider: &str,
    plan: crate::providers::ApiPlan,
    base_url: Option<&str>,
) -> std::io::Result<()> {
    let path = ".env";
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut keys: Vec<(&str, Option<String>)> = vec![("AGENT_PROVIDER", Some(provider.to_string()))];
    let plan_val = if provider != "custom" && plan != crate::providers::ApiPlan::Standard {
        Some(plan.slug().to_string())
    } else {
        None
    };
    keys.push(("AGENT_PLAN", plan_val));
    let url_val = if provider == "custom" {
        base_url.map(str::to_string)
    } else {
        None
    };
    keys.push(("AGENT_BASE_URL", url_val));

    let mut handled = vec![false; keys.len()];
    let mut out = String::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || !trimmed.contains('=') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if let Some(idx) = keys.iter().position(|(k, _)| *k == key) {
            if let Some(val) = &keys[idx].1 {
                out.push_str(&format!("{key}={val}\n"));
            }
            handled[idx] = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    for (i, (k, val)) in keys.iter().enumerate() {
        if !handled[i] {
            if let Some(v) = val {
                out.push_str(&format!("{k}={v}\n"));
            }
        }
    }
    std::fs::write(path, out)
}

// ===== Action handling =====
// ===== 动作处理 =====

fn handle_action(event: AgentEvent, state: &mut TuiState) {
    match event {
        AgentEvent::TextDelta(text) => {
            state.streaming.push_str(&text);
        }
        AgentEvent::ReasoningDelta(text) => {
            state.streaming_reasoning.push_str(&text);
        }
        AgentEvent::ToolCall { name, desc } => {
            state.push_event(AgentEvent::ToolCall { name, desc });
            state.reset_scroll();
        }
        AgentEvent::ToolResult { name, result, ok } => {
            state.push_event(AgentEvent::ToolResult { name, result, ok });
            state.reset_scroll();
        }
        AgentEvent::TurnFinished { turn, usage } => {
            state.current_turn = turn;
            state.total_tokens += parse_usage_tokens(&usage);
            state.last_usage = usage.clone();
            state.push_event(AgentEvent::TurnFinished { turn, usage });
            state.reset_scroll();
        }
        AgentEvent::Agent(text) => {
            // Streaming was a preview of this final output — discard it.
            // 流式文本是最终输出的预览——丢弃。
            state.streaming.clear();
            if !text.is_empty() {
                state.push_event(AgentEvent::Agent(text));
            }
            state.reset_scroll();
        }
        AgentEvent::Error(text) => {
            state.push_event(AgentEvent::Error(text));
            state.task_handle = None;
            state.thinking = false;
            state.streaming.clear();
            state.streaming_reasoning.clear();
            state.reset_scroll();
        }
        AgentEvent::Info(text) => {
            state.push_event(AgentEvent::Info(text));
            state.reset_scroll();
        }
        AgentEvent::HitlPrompt {
            tool,
            desc,
            responder,
        } => {
            // If the task was already aborted (thinking is false), auto-reject
            // to avoid a dangling HITL overlay with no live task.
            // 如果任务已被中断（thinking 为 false），自动拒绝，
            // 避免出现没有活动任务的悬空 HITL 弹窗。
            if !state.thinking {
                let _ = responder.send(false);
            } else {
                state.hitl = Some(HitlState {
                    tool,
                    desc,
                    responder,
                });
            }
        }
        AgentEvent::SuspendTui { command, responder } => {
            let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            let _ = disable_raw_mode();

            println!("\n--- \u{4ea4}\u{4e92}\u{5f0f}\u{547d}\u{4ee4} / Interactive command ---");
            println!("$ {}\n", command);

            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output();

            let output = match out {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let msg = format!(
                        "exit={}\nstdout:\n{}\nstderr:\n{}",
                        out.status.code().unwrap_or(-1),
                        stdout,
                        stderr
                    );
                    println!("\n{}", msg);
                    msg
                }
                Err(e) => {
                    let msg = format!("Error: {}", e);
                    println!("\n{}", msg);
                    msg
                }
            };

            println!(
                "\n--- \u{6309} Enter \u{8fd4}\u{56de} TUI / Press Enter to return to TUI ---"
            );
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);

            let _ = enable_raw_mode();
            let _ = execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture);

            state.needs_full_redraw = true;
            let _ = responder.send(output);
        }
        AgentEvent::AgentStarted => {
            state.thinking = true;
            state.spinner = 0;
            state.current_turn = 0;
        }
        AgentEvent::AgentFinished => {
            state.task_handle = None;
            // Safety net: flush any unflushed streaming.
            // 安全兜底：刷新未刷新的流式文本。
            if !state.streaming.is_empty() {
                let flushed = std::mem::take(&mut state.streaming);
                state.push_event(AgentEvent::Agent(flushed));
            }
            state.thinking = false;
            state.streaming_reasoning.clear();
            state.reset_scroll();
        }
        // User and System events are pushed directly by handle_command —
        // they never arrive through the channel.
        // User 和 System 事件由 handle_command 直接 push——
        // 它们不经过 channel 传递。
        AgentEvent::User(_) | AgentEvent::System(_) => {}
        AgentEvent::ContextCompacted {
            old_tokens,
            new_tokens,
        } => {
            state.push_event(AgentEvent::ContextCompacted {
                old_tokens,
                new_tokens,
            });
            state.reset_scroll();
        }
    }
}

// ===== Rendering =====
// ===== 渲染 =====

fn estimate_input_lines(buffer: &str, area_width: u16) -> u16 {
    let inner_width = (area_width.saturating_sub(2)).max(1) as usize;
    if buffer.is_empty() {
        return 1;
    }
    let mut total: usize = 0;
    for (i, line) in buffer.lines().enumerate() {
        let avail = if i == 0 {
            inner_width.saturating_sub(2)
        } else {
            inner_width
        };
        let avail = avail.max(1);
        let mut width: usize = 0;
        for c in line.chars() {
            width += if c.is_ascii() { 1 } else { 2 };
        }
        total += width.div_ceil(avail).max(1);
    }
    total as u16
}

fn draw(f: &mut Frame, state: &mut TuiState) {
    let area = f.area();

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(34)])
        .split(area);

    let display = state.input.display_text();
    let input_lines = estimate_input_lines(&display, h_chunks[0].width);
    let input_height = (input_lines + 2).max(3).min(area.height / 2);

    let v_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(input_height),
        ])
        .split(h_chunks[0]);

    draw_messages(f, v_chunks[0], state);
    draw_streaming(f, v_chunks[1], state);
    draw_input(f, v_chunks[2], state);
    draw_sidebar(f, h_chunks[1], state);

    if state.hitl.is_some() {
        draw_hitl_overlay(f, state);
    }
    if state.selector.is_some() {
        draw_selector(f, state);
    }
}

/// 渲染选择器弹窗（opencode 风格）：标题 + 可滚动列表 + 过滤/自定义输入行。
/// Renders the selector dialog (opencode-style): title + scrollable list + filter/custom input line.
fn draw_selector(f: &mut Frame, state: &mut TuiState) {
    let area = f.area();
    // 终端过小时不渲染选择器（避免尺寸运算下溢）。
    // Skip rendering when the terminal is too small (avoids size arithmetic underflow).
    if area.width < 10 || area.height < 8 {
        return;
    }
    let sel = state.selector.as_ref().unwrap();
    let visible = sel.visible();
    let list_cap = 8usize;
    let dw = 60u16.min(area.width.saturating_sub(4));
    let shown = visible.len().min(list_cap);
    // 高度 = 列表行 + 空行 + 输入行 + 提示行 + 边框(2) + 内边距(2)
    let dh = (shown as u16 + 7).max(8).min(area.height.saturating_sub(4));
    let dx = (area.width.saturating_sub(dw)) / 2;
    let dy = (area.height.saturating_sub(dh)) / 2;
    let dialog_area = Rect::new(dx, dy, dw, dh);

    let mut lines: Vec<Line> = Vec::new();

    let count = visible.len();
    if count > 0 {
        // 光标保持在可见窗口内：窗口顶部 = cursor - (list_cap - 1)
        let scroll = sel.cursor().saturating_sub(list_cap - 1);
        for (i, item) in visible.iter().enumerate().skip(scroll).take(list_cap) {
            let marker = if i == sel.cursor() { "\u{25b6}" } else { " " };
            let text = format!(" {marker} {}", item.label);
            if i == sel.cursor() {
                lines.push(Line::styled(
                    format!("{text}  {}", item.detail),
                    theme::selector_highlight(),
                ));
            } else {
                lines.push(Line::styled(
                    format!("{text}  {}", item.detail),
                    theme::selector_normal(),
                ));
            }
        }
    } else if !sel.filter().trim().is_empty() {
        lines.push(Line::styled(
            format!(
                "\u{65e0}\u{5339}\u{914d}\u{300c}{}\u{300d}\u{ff0c}Enter \u{4f7f}\u{7528}\u{81ea}\u{5b9a}\u{4e49}\u{6a21}\u{578b}",
                sel.filter().trim()
            ),
            theme::selector_dim(),
        ));
    } else {
        lines.push(Line::styled(
            "\u{ff08}\u{65e0}\u{53ef}\u{7528}\u{6a21}\u{578b}\u{ff0c}\u{8f93}\u{5165}\u{81ea}\u{5b9a}\u{4e49}\u{6a21}\u{578b} ID\u{ff09}",
            theme::selector_dim(),
        ));
    }

    lines.push(Line::default());
    let is_custom_mode = count == 0 && !sel.filter().trim().is_empty();
    let input_label = if is_custom_mode {
        format!("\u{81ea}\u{5b9a}\u{4e49} custom: {}", sel.filter())
    } else {
        format!("\u{8fc7}\u{6ee4} filter: {}", sel.filter())
    };
    lines.push(Line::styled(input_label, theme::selector_input()));
    lines.push(Line::styled(
        "\u{2191}/\u{2193} \u{9009}\u{62e9} | Enter \u{786e}\u{8ba4} | Esc \u{53d6}\u{6d88}",
        theme::selector_dim(),
    ));

    let dialog = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::selector_title())
                .title(format!(" {} ", sel.title()))
                .title_alignment(Alignment::Center)
                .padding(Padding::uniform(1)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(Clear, dialog_area);
    f.render_widget(dialog, dialog_area);

    // filter 输入光标（input 行 = y + 边框1 + 内边距1 + 列表 + 空行1）
    let prefix = if is_custom_mode {
        "\u{81ea}\u{5b9a}\u{4e49} custom: "
    } else {
        "\u{8fc7}\u{6ee4} filter: "
    };
    let prefix_w: usize = prefix
        .chars()
        .map(|c| if c.is_ascii() { 1 } else { 2 })
        .sum();
    let filter_w: usize = sel
        .filter()
        .chars()
        .map(|c| if c.is_ascii() { 1 } else { 2 })
        .sum();
    let cx = (dialog_area.x + 2 + (prefix_w + filter_w) as u16)
        .min(dialog_area.x + dw.saturating_sub(2));
    let cy = dialog_area.y + 3 + shown as u16;
    f.set_cursor_position((cx, cy));
}

fn draw_messages(f: &mut Frame, area: Rect, state: &mut TuiState) {
    let all_lines = state.all_message_lines();

    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(area);
    // 存消息内容区，供 handle_mouse_event 命中测试读取屏幕行→行映射。
    // Store the message content area for handle_mouse_event hit-testing.
    state.msg_area = inner;

    // 按内容区宽度软换行：长行拆成多个"显示行"，避免被终端右边缘截断。
    // 渲染仍用不带 .wrap() 的 Paragraph，因此"1 显示行 == 1 屏幕行"不变量
    // 成立，选区 / 高亮 / 滚动逻辑无需改动即可正确工作。
    // Soft-wrap to content width: long lines split into multiple "display lines"
    // so they aren't clipped at the terminal's right edge. The Paragraph is still
    // rendered without .wrap(), so the "1 display line == 1 screen row" invariant
    // holds and the selection / highlight / scroll logic works unchanged.
    let display_lines = crate::ui::wrap::wrap_lines(&all_lines, inner.width);
    let total = display_lines.len() as u16;

    let base = total.saturating_sub(inner.height);
    let scroll = base.saturating_sub(state.scroll_offset);
    state.msg_scroll = scroll;

    let text = Text::from(display_lines);
    let messages = Paragraph::new(text).scroll((scroll, 0)).block(block);

    f.render_widget(messages, area);

    // 选区高亮：把选中显示行在屏幕上对应的 Cell 叠加反色。
    // 因为 Paragraph 无 .wrap()（已在上方手动软换行），显示行→屏幕行 =
    // inner.y + (display_line - scroll)。
    // Selection highlight: overlay REVERSED on screen cells for selected display
    // lines. Since the Paragraph has no .wrap() (we soft-wrapped manually above),
    // display-line→screen row = inner.y + (display_line - scroll).
    if let Some(sel) = state.selection {
        let (lo_line, lo_col, hi_line, hi_col) = sel.bounds();
        let buf = f.buffer_mut();
        for logical in lo_line..=hi_line {
            let logical_u16 = match u16::try_from(logical) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if logical_u16 < scroll {
                continue;
            }
            let screen_row = inner.y + (logical_u16 - scroll);
            if screen_row >= inner.y + inner.height {
                continue;
            }
            // 行内高亮列范围：首行从 lo_col 起，末行到 hi_col 止，中间整行
            let right = inner.x.saturating_add(inner.width);
            let col_start = if logical == lo_line {
                inner.x.saturating_add(lo_col).min(right)
            } else {
                inner.x
            };
            let col_end = if logical == hi_line {
                inner.x.saturating_add(hi_col).min(right)
            } else {
                right
            };
            for col in col_start..col_end {
                if let Some(cell) = buf.cell_mut((col, screen_row)) {
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    if total > inner.height {
        let mut sb_state = ScrollbarState::default()
            .content_length(total as usize)
            .position(scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("\u{2191}"))
                .end_symbol(Some("\u{2193}")),
            area,
            &mut sb_state,
        );
    }
}

fn draw_streaming(f: &mut Frame, area: Rect, state: &mut TuiState) {
    let content = if state.thinking {
        let sp = SPINNER_FRAMES[state.spinner];
        if !state.streaming.is_empty() {
            format!("{sp} {}", state.streaming)
        } else if !state.streaming_reasoning.is_empty() {
            let r = &state.streaming_reasoning;
            let preview = if r.len() > 80 {
                let end = r.floor_char_boundary(80);
                format!("{}...", &r[..end])
            } else {
                r.clone()
            };
            format!("{sp} \u{601d}\u{8003}\u{4e2d}: {preview}")
        } else {
            format!("{sp} \u{601d}\u{8003}\u{4e2d}...")
        }
    } else {
        String::new()
    };

    let streaming = Paragraph::new(content)
        .style(theme::streaming())
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme::border())
                .padding(Padding::horizontal(1)),
        )
        .wrap(Wrap { trim: false });

    f.render_widget(streaming, area);
}

fn draw_input(f: &mut Frame, area: Rect, state: &mut TuiState) {
    let prompt = if state.thinking {
        "\u{2026}".to_string()
    } else {
        state.input.display_text()
    };

    let input = Paragraph::new(prompt)
        .style(theme::input_prompt())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme::border())
                .padding(Padding::horizontal(1)),
        );

    f.render_widget(input, area);

    if !state.thinking {
        let inner_width = (area.width.saturating_sub(2)).max(1) as usize;
        let mut x: usize = 0;
        let mut y: u16 = 0;
        for c in state.input.buffer[..state.input.cursor].chars() {
            if c == '\n' {
                y += 1;
                x = 0;
            } else {
                let w = if c.is_ascii() { 1 } else { 2 };
                if x + w > inner_width {
                    y += 1;
                    x = w;
                } else {
                    x += w;
                }
            }
        }
        let cx = area.x + 1 + x as u16;
        let max_y = area.y + area.height.saturating_sub(1);
        let cy = (area.y + 1 + y).min(max_y);
        f.set_cursor_position((cx, cy));
    }
}

fn parse_usage_tokens(usage: &str) -> u64 {
    usage
        .split('\u{ff0c}')
        .filter_map(|part| part.split('=').nth(1))
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .sum()
}

fn draw_sidebar(f: &mut Frame, area: Rect, state: &TuiState) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme::border())
        .padding(Padding::horizontal(1));

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled("Provider", theme::status_dim()));
    lines.push(Line::styled(
        format!(" {}", state.provider),
        theme::status_model(),
    ));
    let plan = crate::providers::current_plan();
    if plan != crate::providers::ApiPlan::Standard {
        lines.push(Line::styled(
            format!("  ↳ {}", plan.label()),
            theme::status_dim(),
        ));
    }
    lines.push(Line::default());

    lines.push(Line::styled("Model", theme::status_dim()));
    lines.push(Line::styled(
        format!(" {}", state.model),
        theme::status_model(),
    ));
    lines.push(Line::default());

    lines.push(Line::styled("Context", theme::status_dim()));
    lines.push(Line::styled(
        format!(" {} tok", state.total_tokens),
        theme::status_usage(),
    ));
    if !state.last_usage.is_empty() {
        lines.push(Line::styled(
            format!(" {}", state.last_usage),
            theme::status_dim(),
        ));
    }
    lines.push(Line::default());

    lines.push(Line::styled("Progress", theme::status_dim()));
    let bar_w = 15usize;
    let max = state.max_turns.max(1);
    // Use floating-point division with rounding so the bar fills proportionally.
    // Integer division (bar_w * current_turn / max) truncates and causes the
    // bar to under-fill, especially in the mid-range (e.g. turn 3/25 → 0 blocks).
    // 浮点除法 + 四舍五入，使进度条按比例填充。
    // 整数除法会截断，导致中间段进度条不满（如 3/25 → 0 格）。
    let filled = (bar_w as f64 * state.current_turn as f64 / max as f64).round() as usize;
    let filled = filled.min(bar_w);
    let bar: String = "\u{2588}".repeat(filled) + &"\u{2591}".repeat(bar_w - filled);
    lines.push(Line::styled(
        format!("[{bar}] {}/{}", state.current_turn, state.max_turns),
        theme::status_turn(),
    ));

    let (status_text, status_style) = if state.hitl.is_some() {
        ("\u{26a0} HITL".to_string(), theme::status_hitl())
    } else if state.thinking {
        let sp = SPINNER_FRAMES[state.spinner];
        (format!("{sp} thinking"), theme::status_thinking())
    } else {
        ("\u{2713} ready".to_string(), theme::status_ready())
    };
    lines.push(Line::styled(status_text, status_style));
    lines.push(Line::default());

    lines.push(Line::styled(
        format!("Tools ({})", state.tool_names.len()),
        theme::status_dim(),
    ));
    for name in &state.tool_names {
        lines.push(Line::raw(format!(" \u{2022} {name}")));
    }
    lines.push(Line::default());

    if !state.mcp_servers.is_empty() {
        let connected = state.mcp_servers.iter().filter(|s| s.connected).count();
        let total_tools: usize = state.mcp_servers.iter().map(|s| s.tool_names.len()).sum();
        lines.push(Line::styled(
            format!(
                "MCP ({connected}/{} servers, {total_tools} tools)",
                state.mcp_servers.len()
            ),
            theme::status_dim(),
        ));
        for server in &state.mcp_servers {
            if server.connected {
                lines.push(Line::styled(
                    format!(
                        " \u{2713} {} ({} tools)",
                        server.name,
                        server.tool_names.len()
                    ),
                    theme::mcp_connected(),
                ));
                for tn in &server.tool_names {
                    lines.push(Line::styled(format!("   \u{2022} {tn}"), theme::mcp_tool()));
                }
            } else {
                lines.push(Line::styled(
                    format!(" \u{2717} {}", server.name),
                    theme::mcp_failed(),
                ));
                if let Some(ref err) = server.error {
                    let truncated = if err.len() > 40 {
                        format!("   {err:.40}...")
                    } else {
                        format!("   {err}")
                    };
                    lines.push(Line::styled(truncated, theme::mcp_error_detail()));
                }
            }
        }
        lines.push(Line::default());
    }

    if !state.skill_names.is_empty() {
        lines.push(Line::styled(
            format!("Skills ({})", state.skill_names.len()),
            theme::status_dim(),
        ));
        for name in &state.skill_names {
            lines.push(Line::raw(format!(" \u{2022} {name}")));
        }
        lines.push(Line::default());
    }

    if state.user_scrolled && state.scroll_offset > 0 {
        lines.push(Line::styled(
            format!("\u{2191} {} lines", state.scroll_offset),
            theme::status_scroll(),
        ));
    }

    let sidebar = Paragraph::new(lines).block(block);
    f.render_widget(sidebar, area);
}

fn draw_hitl_overlay(f: &mut Frame, state: &mut TuiState) {
    let area = f.area();
    let dw = 60u16.min(area.width.saturating_sub(4));

    let h = state.hitl.as_ref().unwrap();
    let content = format!(
        "\u{26a0} \u{786e}\u{8ba4}\u{6267}\u{884c}\n\n{}\n\n[y] \u{5141}\u{8bb8}  [n] \u{62d2}\u{7edd}",
        h.desc
    );

    // ── Dynamic dialog height ──
    // The old fixed height of 7 was too small: with borders (2) + padding (2)
    // only 3 content lines were visible, so the "[y] 允许  [n] 拒绝" prompt was
    // clipped whenever the description occupied more than one line.  Calculate
    // the needed height from the content, accounting for wrapping and CJK width.
    // 动态弹窗高度：旧固定值 7 太小——减去边框(2)+内边距(2)后仅 3 行可见，
    // 描述超过一行时 "[y] 允许  [n] 拒绝" 提示被截断，用户看不到该按什么键。
    let inner_width = (dw.saturating_sub(4)).max(1) as usize; // 2 borders + 2 padding
    let est_lines: u16 = content
        .lines()
        .map(|line| {
            // Estimate display width: ASCII = 1 col, CJK = 2 cols.
            let display_width: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            if display_width == 0 {
                1u16
            } else {
                (display_width.div_ceil(inner_width) as u16).max(1)
            }
        })
        .sum();
    let dh = (est_lines + 4).min(area.height.saturating_sub(4)).max(7);

    let dx = (area.width.saturating_sub(dw)) / 2;
    let dy = (area.height.saturating_sub(dh)) / 2;
    let dialog_area = Rect::new(dx, dy, dw, dh);

    f.render_widget(Clear, dialog_area);

    let dialog = Paragraph::new(content)
        .style(theme::hitl_prompt())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::hitl_border())
                .title(" HITL ")
                .title_alignment(Alignment::Center)
                .padding(Padding::uniform(1)),
        )
        .wrap(Wrap { trim: true });

    f.render_widget(dialog, dialog_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_ascii_advances_one_byte() {
        let mut s = InputState::new();
        s.insert_char('a');
        assert_eq!(s.buffer, "a");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn insert_cjk_does_not_panic_on_second_char() {
        let mut s = InputState::new();
        s.insert_char('\u{4f60}');
        s.insert_char('\u{597d}');
        assert_eq!(s.buffer, "\u{4f60}\u{597d}");
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn backspace_removes_full_cjk_char() {
        let mut s = InputState::new();
        s.insert_char('\u{4f60}');
        s.insert_char('\u{597d}');
        s.backspace();
        assert_eq!(s.buffer, "\u{4f60}");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn cursor_left_right_traverses_char_boundaries() {
        let mut s = InputState::new();
        s.insert_char('\u{4f60}');
        s.insert_char('\u{597d}');
        s.cursor_left();
        assert_eq!(s.cursor, 3);
        s.cursor_left();
        assert_eq!(s.cursor, 0);
        s.cursor_right();
        assert_eq!(s.cursor, 3);
        s.cursor_right();
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn delete_removes_full_cjk_char() {
        let mut s = InputState::new();
        s.insert_char('\u{4f60}');
        s.insert_char('\u{597d}');
        s.cursor_left();
        s.delete();
        assert_eq!(s.buffer, "\u{4f60}");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn insert_at_cursor_mid_buffer() {
        let mut s = InputState::new();
        s.insert_char('b');
        s.insert_char('c');
        s.cursor_left();
        s.insert_char('a');
        assert_eq!(s.buffer, "bac");
        assert_eq!(s.cursor, 2);
    }
}
