use std::sync::Arc;
use std::time::Duration;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, EventStream, KeyCode, KeyEvent, KeyModifiers,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame, Terminal,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::cli::context::AppContext;
use crate::cli::repl::ReplCommand;
use crate::event::{AgentEvent, EventReceiver, EventSender};
use crate::ui::{markdown, theme};
use tracing::{info, warn};

const SPINNER_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
    "\u{2827}", "\u{2807}", "\u{280f}",
];
const TICK_MS: u64 = 120;

// ===== Terminal guard (panic-safe cleanup) =====
// ===== 终端守卫（panic 安全清理） =====

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
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

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
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
        let trimmed = self.buffer.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        self.history.push(trimmed.clone());
        self.history_idx = None;
        self.buffer.clear();
        self.cursor = 0;
        Some(trimmed)
    }
}

// ===== TUI state =====
// ===== TUI 状态 =====

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
    /// 当前运行中的后台任务句柄。按 Esc 可 abort 中断。
    /// Handle to the currently running background task. Press Esc to abort.
    task_handle: Option<JoinHandle<()>>,
}

impl TuiState {
    fn new(provider: String, model: String) -> Self {
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
            task_handle: None,
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
        // 瞬态流式事件不单独记录（最终 Agent 输出已覆盖）
        AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => {}
    }
}

// ===== Event rendering (replaces TuiMessage::to_lines) =====
// ===== 事件渲染（替代 TuiMessage::to_lines） =====

fn render_event(event: &AgentEvent) -> Vec<Line<'static>> {
    match event {
        AgentEvent::User(text) => {
            let mut v = vec![Line::styled(format!("\u{00bb} {text}"), theme::user_msg())];
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
                format!(
                    "{}\u{2026}",
                    &result[..result.floor_char_boundary(500)]
                )
            } else {
                result.clone()
            };
            let mut v = vec![Line::from(vec![
                Span::styled(format!("{icon} {name}: "), sty),
                Span::raw(trunc),
            ])];
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
                let mut v = vec![Line::styled(text.clone(), theme::info())];
                v.push(Line::default());
                v
            }
        }
        _ => vec![],
    }
}

// ===== Entry point =====
// ===== 入口 =====

pub async fn run_tui(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    let provider = format!("{:?}", crate::providers::current_provider());
    let model = ctx.current_model();

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<AgentEvent>();

    let mut state = TuiState::new(provider, model);
    state.push_event(AgentEvent::System(format!(
        "my-agent ({}) | model: {}",
        state.provider, state.model
    )));
    state.push_event(AgentEvent::Info(
        "Enter \u{53d1}\u{9001}\u{4efb}\u{52a1} | /help \u{5e2e}\u{52a9} | Esc \u{4e2d}\u{65ad}\u{4efb}\u{52a1} | Ctrl+C \u{9000}\u{51fa}".into(),
    ));

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(TICK_MS));

    run_loop(
        &mut terminal,
        &mut state,
        &ctx,
        &action_tx,
        &mut action_rx,
        &mut events,
        &mut tick,
    )
    .await
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
    loop {
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
                    _ => {}
                }
            }
            Some(action) = action_rx.recv() => {
                handle_action(action, state);
            }
            _ = tick.tick() => {
                state.tick();
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
                        "\u{26a0} \u{5141}\u{8bb8}\u{6267}\u{884c} {}", h.tool
                    )));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Some(h) = state.hitl.take() {
                    let _ = h.responder.send(false);
                    state.push_event(AgentEvent::Info(format!(
                        "\u{26a0} \u{62d2}\u{7edd}\u{6267}\u{884c} {}", h.tool
                    )));
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
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => {
            if !state.thinking {
                if let Some(input) = state.input.take_submitted() {
                    handle_command(input, state, ctx, action_tx);
                }
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
            ctx.cmd_model(slug);
            state.model = ctx.current_model();
            state.push_event(AgentEvent::Info(format!("model: {}", state.model)));
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
        AgentEvent::AgentStarted => {
            state.thinking = true;
            state.spinner = 0;
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
    }
}

// ===== Rendering =====
// ===== 渲染 =====

fn draw(f: &mut Frame, state: &mut TuiState) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(f, chunks[0], state);
    draw_messages(f, chunks[1], state);
    draw_streaming(f, chunks[2], state);
    draw_input(f, chunks[3], state);

    if state.hitl.is_some() {
        draw_hitl_overlay(f, state);
    }
}

fn draw_header(f: &mut Frame, area: Rect, state: &TuiState) {
    let header = Paragraph::new(format!(
        " my-agent ({}) | model: {} ",
        state.provider, state.model
    ))
    .style(theme::header())
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme::border()),
    );
    f.render_widget(header, area);
}

fn draw_messages(f: &mut Frame, area: Rect, state: &mut TuiState) {
    let all_lines = state.all_message_lines();

    let block = Block::default().padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let total = all_lines.len() as u16;

    let base = if total > inner.height {
        total - inner.height
    } else {
        0
    };
    let scroll = base.saturating_sub(state.scroll_offset);

    let text = Text::from(all_lines);
    let messages = Paragraph::new(text)
        .scroll((scroll, 0))
        .block(block);

    f.render_widget(messages, area);

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
        format!("\u{00bb} {}", state.input.buffer)
    };

    let input = Paragraph::new(prompt)
        .style(theme::input_prompt())
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme::border())
                .padding(Padding::horizontal(1)),
        );

    f.render_widget(input, area);

    if !state.thinking {
        let display_width: usize = state.input.buffer[..state.input.cursor]
            .chars()
            .map(|c| if c.is_ascii() { 1 } else { 2 })
            .sum();
        let cx = area.x + 3 + display_width as u16;
        let cy = area.y + 1;
        f.set_cursor_position((cx, cy));
    }
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
                (((display_width + inner_width - 1) / inner_width) as u16).max(1)
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
