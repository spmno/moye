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
use crate::ui::{diff, markdown, theme};
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

    /// 在光标处插入换行符（Alt+Enter / Ctrl+J）。缓冲区与光标逻辑已支持 `\n`
    /// （paste 路径同样经 insert_str 写入），此处复用 insert_char 按字节推进。
    /// Insert a newline at the cursor (Alt+Enter / Ctrl+J). The buffer/cursor
    /// logic already handles `\n` (paste path also writes via insert_str);
    /// reuse insert_char for byte-accurate advance.
    fn insert_newline(&mut self) {
        self.insert_char('\n');
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

// ===== 会话搜索状态 =====
// ===== In-conversation search state =====

/// Ctrl+F 搜索模式状态。打开时输入框变为搜索提示，匹配在消息区高亮。
/// query/cursor 复用 InputState 的字节级 CJK 安全逻辑（按 char 插入、
/// floor_char_boundary 退格/左移），但不触碰常规输入缓冲。
///
/// matches 是"显示行"索引（与 draw_messages 渲染的软换行向量对齐），
/// 仅在 dirty_key 变化时于 draw 时重算——这样索引与用户所见一致。
/// current 是 matches 中的游标；Enter/Down→next、Up→prev（均环绕）。
///
/// Ctrl+F search-mode state. When open the input box becomes a search prompt
/// and matches are highlighted in the message area. query/cursor reuse
/// InputState's byte-level CJK-safe logic (insert by char, floor_char_boundary
/// for backspace/left) but never touch the normal input buffer.
///
/// matches are display-line indices (aligned with the soft-wrapped vector
/// draw_messages renders), recomputed at draw only when dirty_key changes —
/// so indices match what the user sees. current is the cursor into matches;
/// Enter/Down→next, Up→prev (both wrap around).
struct SearchState {
    query: String,
    cursor: usize,
    matches: Vec<usize>,
    current: usize,
    dirty_key: (usize, u16, String),
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            current: 0,
            // 不会与真实 key (messages.len(), width, query) 同时相等（除非退化
            // 终端 + 空消息 + 空查询，此时重算也得空匹配，无害）。
            // Won't match the real key except in a degenerate terminal with no
            // messages and an empty query — where recompute also yields empty.
            dirty_key: (0, 0, String::new()),
        }
    }

    fn insert_char(&mut self, c: char) {
        self.query.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.invalidate();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.query.floor_char_boundary(self.cursor - 1);
            self.query.remove(prev);
            self.cursor = prev;
            self.invalidate();
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.query.floor_char_boundary(self.cursor - 1);
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.query.len() {
            let char_len = self.query[self.cursor..]
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
        self.cursor = self.query.len();
    }

    /// 标记匹配脏（下次 draw 重算）。
    /// Mark matches dirty (recompute at next draw).
    fn invalidate(&mut self) {
        self.dirty_key = (0, 0, String::new());
    }
}

// ===== Command palette =====
// ===== 命令面板 =====

/// 命令面板条目选中后的动作类型。
/// Action type when a command palette entry is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteAction {
    /// 直接执行（零参数命令）。
    /// Execute immediately (zero-arg command).
    Execute,
    /// 植入输入框供用户补全参数。
    /// Plant into the input box for the user to complete args.
    PlantInput,
}

/// 命令面板的静态命令表：(命令, 双语描述, 动作)。
/// Static command table for the palette: (command, bilingual desc, action).
/// 每条命令字符串必须能被 ReplCommand::parse 识别——table_completeness 测试锁定此约束。
/// Every command string must be recognized by ReplCommand::parse — the
/// table_completeness test locks this constraint.
const PALETTE_COMMANDS: &[(&str, &str, PaletteAction)] = &[
    // ── 零参数命令 → 直接执行 / zero-arg commands → Execute ──
    ("/models", "打开供应商/模型选择 / provider & model picker", PaletteAction::Execute),
    ("/skills", "列出已注册技能 / list registered skills", PaletteAction::Execute),
    ("/lessons", "查看经验教训 / show accumulated lessons", PaletteAction::Execute),
    ("/evolve", "触发提示词进化 / trigger prompt evolution", PaletteAction::Execute),
    ("/context", "查看当前上下文 / show current context", PaletteAction::Execute),
    ("/help", "显示帮助 / show help", PaletteAction::Execute),
    ("/trust", "切换沙箱信任模式 / toggle sandbox trust", PaletteAction::Execute),
    ("/quit", "退出程序 / quit", PaletteAction::Execute),
    // ── 带参数命令 → 植入输入框 / arg-taking commands → PlantInput ──
    ("/model", "切换模型 / switch model <slug>", PaletteAction::PlantInput),
    ("/history", "查看对话记录 / show history [n]", PaletteAction::PlantInput),
    ("/plan", "查看或切换套餐 / show or switch plan", PaletteAction::PlantInput),
    ("/evolve-code", "代码自修改 / code self-modify <file> <old> <new>", PaletteAction::PlantInput),
    ("/add-tool", "生成新工具脚手架 / add tool <name> <desc>", PaletteAction::PlantInput),
    ("/add-skill", "添加运行时技能 / add skill <name> <desc>", PaletteAction::PlantInput),
];

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
    /// Ctrl+F 会话搜索模式。Some 时键盘进入搜索态，Esc 关闭。
    /// Ctrl+F in-conversation search mode. When Some the keyboard enters
    /// search mode; Esc closes it.
    search: Option<SearchState>,
    /// 消息内容区 Rect：draw_messages 时写入（Block::inner 后），handle_mouse_event 命中测试时读。
    /// Message content Rect: written in draw_messages (after Block::inner), read in
    /// handle_mouse_event for hit-testing. Stores the actual content area, not the bordered area.
    msg_area: Rect,
    /// 消息区 Paragraph scroll 值（跳过的顶部行数），draw 时存，mouse 事件时读以算屏幕行→逻辑行。
    /// Message Paragraph scroll (top lines skipped); written at draw time, read at
    /// mouse-event time for screen-row→logical-line mapping.
    msg_scroll: u16,
    /// 是否展开截断的工具结果 / diff。Ctrl+E 切换；为 false 时保持 500 字符 / 15 行 / diff 60 行上限。
    /// Whether truncated tool results / diffs are expanded. Toggled by Ctrl+E;
    /// false keeps the 500-char / 15-line / diff 60-line caps.
    expand_tool_results: bool,
    /// 跨会话持久化的输入历史。提交时 record+save，启动时 load 并灌入 InputState.history。
    /// Cross-session persisted input history. record+save on submit; load at
    /// startup and seed into InputState.history.
    input_history: crate::input_history::InputHistory,
    /// 输入区滚动偏移（内容超出窗口时，使光标行可见）。draw 时算，draw_input 时用。
    /// Input area scroll offset (keeps cursor visible when content overflows).
    /// Computed in draw, consumed in draw_input.
    input_scroll: u16,
    /// 命令面板激活标志：为 true 时选择器 Enter 走面板路由而非 /models 路由。
    /// Command palette flag: when true, selector Enter routes to palette logic
    /// instead of the /models model-selection path.
    palette_active: bool,
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
        // 从磁盘加载持久化输入历史，灌入 InputState.history 供 Up/Down 浏览。
        // Load persisted input history from disk, seed into InputState.history
        // for Up/Down browsing. InputState.history is oldest→newest, matching
        // the persisted order (history_up walks backwards from the end).
        let input_history = crate::input_history::InputHistory::load();
        let mut input = InputState::new();
        input.history = input_history.entries.clone();
        Self {
            messages: Vec::new(),
            input,
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
            search: None,
            msg_area: Rect::new(0, 0, 0, 0),
            msg_scroll: 0,
            expand_tool_results: false,
            input_history,
            input_scroll: 0,
            palette_active: false,
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
            lines.extend(render_event(msg, self.expand_tool_results));
        }
        lines
    }

    /// 将事件推入消息历史，同时写入 tracing 文件日志。
    /// 这样文件 log 与终端 TUI 显示的内容保持一致。
    fn push_event(&mut self, event: AgentEvent) {
        log_event(&event);
        self.messages.push(event);
    }

    /// 切换模型/供应商后，更新首条 System 消息使其反映当前状态。
    /// 显示窗口（消息区）的首行是启动时推入的 System 消息，内含 provider/model；
    /// 若不刷新，切模型后该行仍显示旧值，而侧栏已更新，造成信息不一致。
    /// Refresh the first System message to reflect the current provider/model after a switch.
    /// The display window's first line is the System message pushed at startup; without
    /// refreshing it, the old model/provider lingers while the sidebar already updated.
    fn refresh_system_header(&mut self) {
        if let Some(AgentEvent::System(text)) = self.messages.iter_mut().next() {
            *text = format!("moye ({}) | model: {}", self.provider, self.model);
        }
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
        AgentEvent::ToolCall { name, desc, .. } => {
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
        AgentEvent::PhaseStart { role } => {
            info!("[TUI] SDD \u{9636}\u{6bb5}: {role}");
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
        AgentEvent::Reasoning(text) => {
            info!("[TUI] \u{601d}\u{8003}\u{8fc7}\u{7a0b}: {} \u{5b57}\u{7b26}", text.chars().count());
        }
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

/// SDD 角色 ID → (图标, 双语标签) 的纯映射，用于 PhaseStart 分隔线。
/// 未知角色返回 None，调用方用原始 role 字符串生成通用分隔线。
/// Pure mapping from SDD role id → (icon, bilingual label) for PhaseStart dividers.
/// Unknown roles return None; the caller falls back to a generic divider with the raw string.
fn phase_label(role: &str) -> Option<(&'static str, &'static str)> {
    match role {
        "investigator" => Some(("\u{1f50d}", "\u{8c03}\u{67e5}\u{4e2d} / Investigating")),
        "planner" => Some(("\u{1f4cb}", "\u{89c4}\u{5212}\u{4e2d} / Planning")),
        "builder" => Some(("\u{1f528}", "\u{6784}\u{5efa}\u{4e2d} / Building")),
        "auditor" => Some(("\u{2705}", "\u{5ba1}\u{8ba1}\u{4e2d} / Auditing")),
        _ => None,
    }
}

/// 工具结果截断上限：先按字符截断到 500（floor_char_boundary 防止拆分多字节
/// 字符），再按行截断到 15 行。
/// Tool result truncation limits: first char-truncate to 500 (floor_char_boundary
/// avoids splitting multibyte chars), then line-truncate to 15 lines.
const TOOL_RESULT_MAX_CHARS: usize = 500;
const TOOL_RESULT_MAX_LINES: usize = 15;

/// 纯函数：把工具结果字符串截断为显示行列表 + 隐藏行数。
/// expand=true → 全量输出，返回 None（无提示）。
/// expand=false → 500 字符 / 15 行上限（与改动前语义完全一致）；
///   返回 Some(hidden) 当字符或行被截断时，hidden 为原始结果中未完整显示的行数。
///
/// Pure helper: truncate a tool result string into display lines + hidden line count.
/// expand=true → full output, returns None (no hint).
/// expand=false → 500-char / 15-line cap (byte-identical to prior behavior);
///   returns Some(hidden) when chars or lines were truncated, where hidden is the
///   count of original lines not fully shown.
fn truncate_result_lines(result: &str, expand: bool) -> (Vec<String>, Option<usize>) {
    let original_count = result.lines().count();
    if expand {
        return (result.lines().map(String::from).collect(), None);
    }
    let char_truncated = result.len() > TOOL_RESULT_MAX_CHARS;
    let trunc = if char_truncated {
        format!(
            "{}\u{2026}",
            &result[..result.floor_char_boundary(TOOL_RESULT_MAX_CHARS)]
        )
    } else {
        result.to_string()
    };
    let all: Vec<&str> = trunc.lines().collect();
    let displayed: Vec<String> = if all.len() > TOOL_RESULT_MAX_LINES {
        all[..TOOL_RESULT_MAX_LINES]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        all.iter().map(|s| s.to_string()).collect()
    };
    let hidden = original_count.saturating_sub(displayed.len());
    if char_truncated || hidden > 0 {
        (displayed, Some(hidden))
    } else {
        (displayed, None)
    }
}

fn render_event(event: &AgentEvent, expand: bool) -> Vec<Line<'static>> {
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
        AgentEvent::ToolCall { name, desc, diff } => {
            let sty = theme::tool_call();
            // 有结构化编辑载荷 → 渲染 unified diff（红删 / 绿增 / 暗灰上下文）；
            // 无载荷 → 退回原纯文本 desc 渲染，行为与之前完全一致。
            // With a structured edit payload → render a unified diff
            // (red deletes / green inserts / dim context); without it →
            // fall back to the original plain-text desc rendering, unchanged.
            if let Some(edit) = diff {
                let mut v: Vec<Line<'static>> = vec![Line::from(vec![
                    Span::styled("\u{1f527} ", sty),
                    Span::styled(format!("{name} \u{2192} \u{7f16}\u{8f91}\u{6587}\u{4ef6}: {}", edit.path), sty),
                ])];
                v.extend(diff::unified_diff_lines(edit, expand));
                v.push(Line::default());
                return v;
            }
            let mut v: Vec<Line<'static>> = vec![];
            // 按 \n 拆分为多行：ratatui 的 Line 不识别内嵌换行符，
            // 若把多行 desc 塞进单个 Span，所有内容会被压成一行，
            // 超出终端宽度后截断，代码无法阅读（edit_file 的 old/new、
            // run_bash 的多行命令均受此影响）。
            // Split desc on \n into separate Lines: ratatui's Line does
            // not honor embedded newlines, so a multi-line desc in a single
            // Span gets squashed into one visual row and truncated.
            let mut lines = desc.split('\n');
            if let Some(first) = lines.next() {
                v.push(Line::from(vec![
                    Span::styled("\u{1f527} ", sty),
                    Span::styled(format!("{name}: {first}"), sty),
                ]));
            }
            for line in lines {
                v.push(Line::from(Span::styled(line.to_string(), sty)));
            }
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
            let mut v = vec![];
            v.push(Line::from(vec![Span::styled(
                format!("{icon} {name}"),
                sty,
            )]));
            let (lines, hidden) = truncate_result_lines(result, expand);
            for line in &lines {
                v.push(Line::from(Span::raw(line.to_string())));
            }
            if let Some(n) = hidden {
                v.push(Line::styled(
                    format!(
                        "  \u{22ef} (\u{5df2}\u{622a}\u{65ad} {n} \u{884c} \u{00b7} Ctrl+E \u{5c55}\u{5f00} / truncated \u{00b7} Ctrl+E to expand)"
                    ),
                    theme::info(),
                ));
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
        AgentEvent::PhaseStart { role } => {
            let sty = theme::usage();
            let line = match phase_label(role) {
                Some((icon, label)) => format!(
                    "\u{2500}\u{2500}\u{2500} {icon} {label} \u{2500}\u{2500}\u{2500}"
                ),
                None => format!("\u{2500}\u{2500}\u{2500} {role} \u{2500}\u{2500}\u{2500}"),
            };
            vec![Line::styled(line, sty), Line::default()]
        }
        AgentEvent::Reasoning(text) => {
            let line_count = text.lines().count();
            if expand {
                let mut v: Vec<Line<'static>> = vec![Line::styled(
                    "\u{1f4ad} \u{601d}\u{8003}\u{8fc7}\u{7a0b} / Reasoning:",
                    theme::info(),
                )];
                // 按 \n 拆分为多行：ratatui 的 Line 不识别内嵌换行符，
                // 若把多行文本塞进单个 Line，所有内容会被压成一行。
                // Split on \n into separate Lines: ratatui's Line does not
                // honor embedded newlines, so a multi-line body in a single
                // Line gets squashed into one visual row.
                for line in text.split('\n') {
                    v.push(Line::styled(line.to_string(), theme::streaming()));
                }
                v.push(Line::default());
                v
            } else {
                vec![
                    Line::styled(
                        format!(
                            "\u{1f4ad} \u{601d}\u{8003}\u{8fc7}\u{7a0b} ({line_count} \u{884c} \u{00b7} Ctrl+E \u{5c55}\u{5f00} / reasoning \u{00b7} Ctrl+E to expand)"
                        ),
                        theme::info(),
                    ),
                    Line::default(),
                ]
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
        AgentEvent::ToolCall { name, desc, diff } => {
            if diff.is_some() {
                format!("[ToolCall] {name} (diff)")
            } else {
                format!("[ToolCall] {name}: {}", truncate_ctx(desc, 120))
            }
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
        AgentEvent::PhaseStart { role } => {
            format!("[Phase] {role}")
        }
        AgentEvent::ContextCompacted {
            old_tokens,
            new_tokens,
        } => {
            format!("[ContextCompacted] {old_tokens} → {new_tokens} tokens")
        }
        AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) => String::new(),
        AgentEvent::Reasoning(text) => {
            format!("[Reasoning] ({} chars)", text.chars().count())
        }
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
        "Enter \u{53d1}\u{9001}\u{4efb}\u{52a1} | Alt+Enter \u{6362}\u{884c} | /help \u{5e2e}\u{52a9} | Esc \u{4e2d}\u{65ad}\u{4efb}\u{52a1} | Ctrl+C \u{9000}\u{51fa}".into(),
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
            state.palette_active = false;
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
                // 命令面板：选中后按动作路由（Execute → handle_command；PlantInput → 植入输入框）。
                // Command palette: route by action (Execute → handle_command; PlantInput →
                // plant into input buffer).
                if state.palette_active {
                    if let Some(item) = selected {
                        match apply_palette_selection(state, &item) {
                            Some(PaletteExec::Execute(cmd)) => {
                                handle_command(cmd, state, ctx, action_tx);
                            }
                            Some(PaletteExec::Planted) | None => {}
                        }
                    }
                    return;
                }
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
                    ctx.cmd_model(Some(item.label.clone()), provider.clone(), base_url);
                    state.model = ctx.current_model();
                    if let Some(p) = provider.as_ref() {
                        state.provider = format!("{:?}", crate::providers::parse_provider(p));
                    }
                    state.refresh_system_header();
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
                state.palette_active = false;
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

    // 搜索模式：键盘进入搜索态。置于选择器守卫之后、Esc 中断守卫之前——
    // 使 Esc 关闭搜索而非中断任务；选择器与搜索不会共存（选择器打开时不进入搜索）。
    // Search mode: keyboard input goes to the search state. Placed after the
    // selector guard and before the Esc-thinking guard so Esc closes search
    // instead of aborting the task; selector and search never co-exist (search
    // is never entered while a selector is open).
    if state.search.is_some() && apply_search_key(state, key) {
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
            KeyCode::Char('e') => {
                state.expand_tool_results = !state.expand_tool_results;
                if state.expand_tool_results {
                    state.push_event(AgentEvent::Info(
                        "\u{25b6} \u{622a}\u{65ad}\u{7ed3}\u{679c}\u{5df2}\u{5c55}\u{5f00} / truncated results expanded (Ctrl+E \u{6298}\u{53e0})"
                            .into(),
                    ));
                } else {
                    state.push_event(AgentEvent::Info(
                        "\u{25c0} \u{622a}\u{65ad}\u{7ed3}\u{679c}\u{5df2}\u{6298}\u{53e0} / truncated results collapsed (Ctrl+E \u{5c55}\u{5f00})"
                            .into(),
                    ));
                }
                return;
            }
            KeyCode::Char('j') => {
                // Ctrl+J 插入换行（与 Alt+Enter 等价）。
                // Ctrl+J inserts a newline (equivalent to Alt+Enter).
                state.input.insert_newline();
                return;
            }
            KeyCode::Char('f') => {
                // Ctrl+F 切换搜索模式：已搜索则关闭，否则以空查询开启。
                // Ctrl+F toggles search: close if searching, otherwise open with empty query.
                state.search = match state.search.take() {
                    Some(_) => None,
                    None => Some(SearchState::new()),
                };
                return;
            }
            KeyCode::Char('p') => {
                open_palette(state);
                return;
            }
            _ => {}
        }
    }

    // Alt+Enter 插入换行（须在普通 Enter 提交前拦截，否则会被 Enter 提交路径捕获）。
    // Alt+Enter inserts a newline (must intercept before the plain Enter submit
    // arm, otherwise it would be captured by the Enter submit path).
    // 注意：Shift+Enter 未实现——终端无 kitty keyboard-protocol 时 Shift+Enter
    // 与 Enter 不可区分，实现它会静默提交。详见报告。
    // Note: Shift+Enter is NOT implemented — without the kitty keyboard protocol,
    // Shift+Enter is indistinguishable from Enter and would silently submit.
    // See the report for details.
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT) {
        state.input.insert_newline();
        return;
    }

    match key.code {
        KeyCode::Enter => {
            // 仅在修饰键为空（或仅 SHIFT）时提交；Alt+Enter 已在上方拦截为换行。
            // Submit only when modifiers are empty (or SHIFT-only); Alt+Enter was
            // intercepted above as a newline.
            let shift_only =
                key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
            if shift_only && !state.thinking
                && let Some(input) = state.input.take_submitted()
            {
                // 持久化输入历史（save-on-submit 是崩溃安全的；文件很小）。
                // Persist input history (save-on-submit is crash-safe; tiny file).
                state.input_history.record(input.clone());
                if let Err(e) = state.input_history.save() {
                    warn!("failed to save input history: {e}");
                }
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

// ===== 会话搜索纯函数 / in-conversation search pure functions =====
// 纯函数：无副作用，便于单测；draw 与 key handler 共用。

/// 返回行文本包含 query（大小写无关）的显示行索引。空 query → 空结果。
/// CJK 安全：基于 String::to_lowercase + contains，绝不切片 mid-char。
///
/// Returns display-line indices whose concatenated span text contains query
/// (case-insensitive). Empty query → empty result. CJK-safe: operates on
/// full Strings via to_lowercase + contains, never slicing mid-char.
fn find_matches(lines: &[Line<'static>], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
            if text.to_lowercase().contains(&q) {
                Some(i)
            } else {
                None
            }
        })
        .collect()
}

/// 下一个匹配（环绕）。len==0 时返回 0（调用方应先判空）。
/// Next match with wraparound. len==0 returns 0 (caller must guard).
fn next_match(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (current + 1) % len
    }
}

/// 上一个匹配（环绕）。len==0 时返回 0。
/// Previous match with wraparound. len==0 returns 0.
fn prev_match(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (current + len - 1) % len
    }
}

/// 计算使显示行 idx 可见所需的 scroll_offset。镜像 draw_messages 算术：
///   base = total.saturating_sub(height)
///   scroll(Paragraph 跳过行) = base.saturating_sub(scroll_offset)
/// 使 idx 置顶 → scroll=idx（clamp 到 base），换算 scroll_offset = base - scroll。
///
/// Compute the scroll_offset that brings display-line idx into view. Mirrors
/// draw_messages arithmetic: base = total - height; scroll (Paragraph skip) =
/// base - scroll_offset. Putting idx at the top → scroll=idx (clamped to
/// base), converted back: scroll_offset = base - scroll.
fn jump_target_scroll(idx: usize, total: usize, height: u16) -> u16 {
    let height = height.max(1);
    let total_u16 = total as u16;
    let base = total_u16.saturating_sub(height);
    // 目标 scroll（Paragraph 跳过行）= idx 置顶，但不超过 base（贴底）。
    // Target scroll (Paragraph skip) = idx at top, clamped to base (bottom).
    let target_scroll = (idx as u16).min(base);
    base.saturating_sub(target_scroll)
}

/// 对匹配行叠加搜索高亮 bg（当前=Yellow，其它=DarkGray），保留原 span fg。
/// 用 Style::patch：highlight 仅设 bg，patch 时 fg/modifier 保持不变。
/// 未匹配行原样返回。
///
/// Overlay search-highlight bg on matched lines (current=Yellow, others=
/// DarkGray), preserving the original span fg. Uses Style::patch: the
/// highlight sets bg only, so fg/modifier survive patching. Non-matched
/// lines are returned untouched.
fn highlight_matches(
    lines: Vec<Line<'static>>,
    matches: &[usize],
    current: Option<usize>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            if matches.contains(&i) {
                let highlight = if Some(i) == current {
                    theme::search_current()
                } else {
                    theme::search_match()
                };
                for span in line.spans.iter_mut() {
                    // patch 仅覆盖 bg（highlight 的 fg=None），原 fg/modifier 保留。
                    // patch overwrites bg only (highlight fg=None); original
                    // fg/modifier survive — honors the code_block line-style contract.
                    span.style = span.style.patch(highlight);
                }
            }
            line
        })
        .collect()
}

/// 搜索模式按键处理（从 handle_key_event 抽出以便单测）。
/// 返回 true=已处理（搜索独占键盘），false=交给主链（仅 Ctrl+C/D 退出）。
///
/// Search-mode key handler (extracted from handle_key_event for unit testing).
/// Returns true when handled (search owns the keyboard), false to delegate
/// to the main chain (only Ctrl+C/D, so quitting still works).
fn apply_search_key(state: &mut TuiState, key: KeyEvent) -> bool {
    // Ctrl+C/D 交给主链退出（搜索不应拦截退出）。
    // Ctrl+C/D delegates to the main chain to quit (search must not trap it).
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        return false;
    }
    // Esc / Ctrl+F 关闭搜索（无需操作 query，直接置 None）。
    // Esc / Ctrl+F close search (no query manipulation; set None directly).
    if key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('f') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        state.search = None;
        return true;
    }
    let Some(search) = state.search.as_mut() else {
        return false;
    };
    match key.code {
        // 仅无 CONTROL 修饰的字符才插入（Ctrl+X 已在上方处理或交给主链）。
        // Insert only chars without CONTROL (Ctrl+X handled above or delegated).
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            search.insert_char(c);
            true
        }
        KeyCode::Backspace => {
            search.backspace();
            true
        }
        KeyCode::Left => {
            search.cursor_left();
            true
        }
        KeyCode::Right => {
            search.cursor_right();
            true
        }
        KeyCode::Home => {
            search.cursor_home();
            true
        }
        KeyCode::End => {
            search.cursor_end();
            true
        }
        KeyCode::Enter | KeyCode::Down => {
            let len = search.matches.len();
            if len > 0 {
                search.current = next_match(search.current, len);
            }
            true
        }
        KeyCode::Up => {
            let len = search.matches.len();
            if len > 0 {
                search.current = prev_match(search.current, len);
            }
            true
        }
        _ => true,
    }
}

/// draw_messages 中调用：重算搜索匹配（对齐渲染用的软换行向量）、
/// 确保当前匹配行可见（仅在离开可见区时滚动）、叠加高亮。
/// 返回（可能已高亮的）显示行向量。
///
/// Called from draw_messages: recompute search matches (aligned with the
/// soft-wrapped vector used for rendering), ensure the current match is
/// visible (scroll only when outside the visible area), and overlay
/// highlights. Returns the (possibly highlighted) display-line vector.
fn apply_search_draw(
    state: &mut TuiState,
    display_lines: Vec<Line<'static>>,
    total: u16,
    base: u16,
    inner: Rect,
) -> Vec<Line<'static>> {
    let Some(search) = state.search.as_mut() else {
        return display_lines;
    };
    // 重算匹配：dirty_key = (messages.len(), inner.width, query)
    let key = (state.messages.len(), inner.width, search.query.clone());
    if search.dirty_key != key {
        search.matches = find_matches(&display_lines, &search.query);
        if search.current >= search.matches.len() {
            search.current = 0;
        }
        search.dirty_key = key;
    }
    // 确保当前匹配行在可见区 [scroll_now, scroll_now+height) 内；
    // 仅在离开时跳转（避免每次按键都跳动）。
    // Keep the current match inside [scroll_now, scroll_now+height); jump
    // only when it's outside (avoids jumping on every keystroke).
    if !search.matches.is_empty() {
        let idx = search.matches[search.current.min(search.matches.len() - 1)];
        let scroll_now = base.saturating_sub(state.scroll_offset);
        let idx16 = idx as u16;
        if idx16 < scroll_now || idx16 >= scroll_now.saturating_add(inner.height) {
            state.scroll_offset = jump_target_scroll(idx, total as usize, inner.height);
            state.user_scrolled = true;
        }
    }
    if search.matches.is_empty() {
        display_lines
    } else {
        let cur = search.current.min(search.matches.len() - 1);
        highlight_matches(display_lines, &search.matches, Some(cur))
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
            state.refresh_system_header();
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

// ===== Command palette routing =====
// ===== 命令面板路由 =====

/// 命令面板选中后的执行结果。
/// Execution result after a palette selection.
enum PaletteExec {
    /// 待执行的命令字符串（零参数命令），由调用方传给 handle_command。
    /// Command string to execute (zero-arg); the caller passes it to handle_command.
    Execute(String),
    /// 已植入输入框，等待用户补全参数。
    /// Planted into input; the user completes the args.
    Planted,
}

/// 打开命令面板：用 PALETTE_COMMANDS 填充选择器，设置 palette_active 标志。
/// Open the command palette: populate the selector with PALETTE_COMMANDS
/// and set the palette_active flag.
fn open_palette(state: &mut TuiState) {
    let items: Vec<SelectorItem> = PALETTE_COMMANDS
        .iter()
        .map(|(cmd, desc, _)| SelectorItem {
            label: cmd.to_string(),
            detail: desc.to_string(),
            data: Some(cmd.to_string()),
        })
        .collect();
    state.selector = Some(SelectorState::new(
        "命令 / Commands".into(),
        items,
        false,
    ));
    state.palette_active = true;
}

/// 应用命令面板选择：关闭选择器、清除 palette_active，按动作路由。
/// 未找到匹配命令时返回 None（调用方应视为 no-op）。
/// Apply a palette selection: close the selector, clear palette_active,
/// route by the action. Returns None when the command is not found in
/// PALETTE_COMMANDS (the caller treats it as a no-op).
fn apply_palette_selection(
    state: &mut TuiState,
    item: &SelectorItem,
) -> Option<PaletteExec> {
    let cmd = item.data.as_deref().unwrap_or(&item.label);
    let entry = PALETTE_COMMANDS.iter().find(|(c, _, _)| *c == cmd)?;
    let cmd_str = entry.0.to_string();
    let action = entry.2;
    state.selector = None;
    state.palette_active = false;
    match action {
        PaletteAction::Execute => Some(PaletteExec::Execute(cmd_str)),
        PaletteAction::PlantInput => {
            state.input.buffer.clear();
            state.input.cursor = 0;
            state.input.insert_str(&format!("{cmd_str} "));
            Some(PaletteExec::Planted)
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

    // persist_switch_to_env 只写 .env 文件，不更新进程环境；current_plan() /
    // Provider::from_env() 读的是进程 env，不注入则侧栏 plan 和后续 cmd_plan
    // 仍显示旧值。session 级 override 已由 set_session_* 设置，这里同步 env 仅为
    // 让读取 env 的显示路径（侧栏 plan、日志）与本会话一致。
    unsafe {
        std::env::set_var("AGENT_PROVIDER", slug);
        if provider_enum != crate::providers::Provider::Custom
            && flow.plan != crate::providers::ApiPlan::Standard
        {
            std::env::set_var("AGENT_PLAN", flow.plan.slug());
        } else {
            std::env::remove_var("AGENT_PLAN");
        }
    }

    state.provider = format!("{provider_enum:?}");
    state.model = ctx.current_model();
    state.refresh_system_header();
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

/// 将累积的推理文本刷出为持久 Reasoning 事件。空缓冲区为 no-op。
/// Pure helper: flush accumulated reasoning text into a persistent Reasoning
/// event. No-op when the buffer is empty.
fn flush_reasoning(state: &mut TuiState) {
    if !state.streaming_reasoning.is_empty() {
        let text = std::mem::take(&mut state.streaming_reasoning);
        state.push_event(AgentEvent::Reasoning(text));
    }
}

fn handle_action(event: AgentEvent, state: &mut TuiState) {
    match event {
        AgentEvent::TextDelta(text) => {
            state.streaming.push_str(&text);
        }
        AgentEvent::ReasoningDelta(text) => {
            state.streaming_reasoning.push_str(&text);
        }
        AgentEvent::ToolCall { name, desc, diff } => {
            state.push_event(AgentEvent::ToolCall { name, desc, diff });
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
            // Flush accumulated reasoning BEFORE the answer — reasoning must
            // land before its answer in the message list. All ReasoningDeltas
            // precede the final Agent event by stream construction, so the
            // buffer is complete here.
            // 先刷出累积推理，再推入答案——推理必须在答案之前。
            flush_reasoning(state);
            // Streaming was a preview of this final output — discard it.
            // 流式文本是最终输出的预览——丢弃。
            state.streaming.clear();
            if !text.is_empty() {
                state.push_event(AgentEvent::Agent(text));
            }
            state.reset_scroll();
        }
        AgentEvent::Error(text) => {
            // Preserve reasoning before clearing — an errored turn's reasoning
            // is exactly what you want to inspect.
            // 保留推理再清空——出错回合的推理正是需要检查的内容。
            flush_reasoning(state);
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
        AgentEvent::PhaseStart { role } => {
            state.push_event(AgentEvent::PhaseStart { role });
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
            // Flush reasoning FIRST — covers the safety-net path where the
            // final Agent event never arrived (e.g. aborted mid-stream).
            // 先刷出推理——覆盖 Agent 事件未到达的安全兜底路径。
            flush_reasoning(state);
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
        AgentEvent::User(_) | AgentEvent::System(_) | AgentEvent::Reasoning(_) => {}
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
    // 使用 split('\n') 而非 lines()，以正确计数尾随换行产生的空行。
    // Use split('\n') instead of lines() to correctly count the empty line
    // produced by a trailing newline.
    for (i, line) in buffer.split('\n').enumerate() {
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

/// 计算光标所在的显示行号（含显式换行与软换行），用于输入区滚动窗口定位。
/// Compute the display line the cursor is on (including explicit newlines and
/// soft-wrap), used for input area scroll-window positioning.
fn cursor_display_line(buffer: &str, cursor: usize, inner_width: usize) -> u16 {
    let mut x: usize = 0;
    let mut y: u16 = 0;
    for c in buffer[..cursor].chars() {
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
    y
}

/// 计算输入区高度与滚动偏移：高度 = clamp(content_lines + 2, 3, max)；
/// 内容超出窗口时，偏移使光标行始终落在可见区域内。
/// Compute input area height and scroll offset: height = clamp(content_lines + 2, 3, max);
/// when content overflows the window, the offset keeps the cursor line visible.
fn input_window(total_lines: u16, cursor_line: u16, max: u16) -> (u16, u16) {
    let height = (total_lines + 2).min(max).max(3);
    if total_lines + 2 <= max {
        return (height, 0);
    }
    let visible = height.saturating_sub(1); // 上边框占 1 行 / top border takes 1 row
    let offset = cursor_line.saturating_sub(visible.saturating_sub(1));
    (height, offset)
}

fn draw(f: &mut Frame, state: &mut TuiState) {
    let area = f.area();

    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(34)])
        .split(area);

    let display = state.input.display_text();
    let input_lines = estimate_input_lines(&display, h_chunks[0].width);
    let inner_width = (h_chunks[0].width.saturating_sub(2)).max(1) as usize;
    let cursor_line = cursor_display_line(&display, state.input.cursor, inner_width);
    let (input_height, input_scroll) = input_window(input_lines, cursor_line, 10);
    let input_height = input_height.min(area.height / 2);
    state.input_scroll = input_scroll;

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

    // 搜索：draw 时重算匹配（对齐上方软换行向量）、确保当前匹配可见、叠加高亮。
    // 须在算 scroll 之前调用——跳转会改写 scroll_offset。
    // Search: recompute matches at draw (aligned with the wrapped vector above),
    // ensure the current match is visible, overlay highlights. Must run before
    // computing scroll — a jump rewrites scroll_offset.
    let display_lines = apply_search_draw(state, display_lines, total, base, inner);

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
    // 搜索模式：输入框显示搜索提示与匹配计数，不复用常规输入缓冲/历史。
    // 搜索关闭后常规渲染字节不变。
    // Search mode: the input box shows the search prompt and match count;
    // the normal input buffer / history are not rendered. When search is
    // None the rendering below is byte-identical to before.
    if let Some(search) = state.search.as_ref() {
        let total = search.matches.len();
        let cur = if total == 0 { 0 } else { search.current.min(total - 1) };
        let prompt = format!(
            "\u{1f50d} \u{641c}\u{7d22} / Search: {}  ({}/{})",
            search.query, cur, total
        );
        let input = Paragraph::new(prompt)
            .style(theme::selector_input())
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(theme::border())
                    .padding(Padding::horizontal(1)),
            );
        f.render_widget(input, area);

        // 光标定位在 query 光标处（prefix + query[..cursor] 之后）。
        // Position the cursor at the query cursor (after prefix + query[..cursor]).
        let prefix = "\u{1f50d} \u{641c}\u{7d22} / Search: ";
        let before = &search.query[..search.cursor];
        let mut x: u16 = 0;
        for c in prefix.chars().chain(before.chars()) {
            let w = if c.is_ascii() { 1 } else { 2 };
            x = x.saturating_add(w);
        }
        let inner_right = area.x.saturating_add(area.width).saturating_sub(1);
        let cx = (area.x + 1 + x).min(inner_right);
        let cy = area.y + 1;
        f.set_cursor_position((cx, cy));
        return;
    }

    let prompt = if state.thinking {
        "\u{2026}".to_string()
    } else {
        state.input.display_text()
    };

    let input = Paragraph::new(prompt)
        .style(theme::input_prompt())
        .wrap(Wrap { trim: false })
        .scroll((state.input_scroll, 0))
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
        // 减去滚动偏移，使光标在可见区域内定位。
        // Subtract scroll offset so the cursor positions within the visible area.
        let visible_y = y.saturating_sub(state.input_scroll);
        let cy = (area.y + 1 + visible_y).min(max_y);
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

/// 把当前工作目录格式化为适合侧边栏窄列显示的短路径：
/// home 目录替换为 `~`；超长时从前面按路径段丢弃并加 `…/` 前缀，
/// 尽量保留尾部完整段（当前目录名优先可见）。
/// Format the current working directory into a short path for the narrow
/// sidebar column: home dir → `~`; when too long, leading segments are dropped
/// with a `…/` prefix, keeping trailing segments (cwd name) visible.
fn format_workdir(max_len: usize) -> String {
    let path = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return String::from("?"),
    };
    let mut s = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && s.starts_with(&home) {
            s = format!("~{}", &s[home.len()..]);
        }
    }
    if s.chars().count() <= max_len {
        return s;
    }
    let parts: Vec<&str> = s.split('/').collect();
    // 从尾部保留尽可能多的完整路径段，前面用 …/ 省略。
    for keep in (1..parts.len()).rev() {
        let joined = parts[parts.len() - keep..].join("/");
        let with_prefix = format!("\u{2026}/{}", joined);
        if with_prefix.chars().count() <= max_len {
            return with_prefix;
        }
    }
    // 连最后一个段都放不下，截断末尾字符。
    let last = parts.last().copied().unwrap_or("");
    let chars: Vec<char> = last.chars().collect();
    let take = max_len.saturating_sub(1);
    let tail: String = chars[chars.len().saturating_sub(take)..].iter().collect();
    format!("\u{2026}{tail}")
}

fn draw_sidebar(f: &mut Frame, area: Rect, state: &TuiState) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme::border())
        .padding(Padding::horizontal(1));

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::styled("CWD", theme::status_dim()));
    lines.push(Line::styled(
        format!(" {}", format_workdir(area.width.saturating_sub(5) as usize)),
        theme::status_model(),
    ));
    lines.push(Line::default());

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

    #[test]
    fn phase_label_maps_all_four_sdd_roles() {
        assert_eq!(
            phase_label("investigator"),
            Some(("\u{1f50d}", "\u{8c03}\u{67e5}\u{4e2d} / Investigating"))
        );
        assert_eq!(
            phase_label("planner"),
            Some(("\u{1f4cb}", "\u{89c4}\u{5212}\u{4e2d} / Planning"))
        );
        assert_eq!(
            phase_label("builder"),
            Some(("\u{1f528}", "\u{6784}\u{5efa}\u{4e2d} / Building"))
        );
        assert_eq!(
            phase_label("auditor"),
            Some(("\u{2705}", "\u{5ba1}\u{8ba1}\u{4e2d} / Auditing"))
        );
    }

    #[test]
    fn phase_label_unknown_role_returns_none() {
        assert_eq!(phase_label("unknown"), None);
        assert_eq!(phase_label(""), None);
        assert_eq!(phase_label("Orchestrator"), None);
    }

    // render_event 对带 diff 的 ToolCall 输出应含红色 span（删除）和绿色 span（插入）。
    // render_event on a ToolCall WITH diff must contain a red span (delete)
    // and a green span (insert) — proves wiring, not just the pure fn.
    #[test]
    fn render_event_toolcall_with_diff_has_red_and_green_spans() {
        use crate::event::FileEdit;
        let edit = Box::new(FileEdit {
            path: "foo.rs".to_string(),
            old: "old line\n".to_string(),
            new: "new line\n".to_string(),
        });
        let event = AgentEvent::ToolCall {
            name: "edit_file".to_string(),
            desc: String::new(),
            diff: Some(edit),
        };
        let lines = render_event(&event, false);
        let has_red = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.fg == theme::tool_result_err().fg)
        });
        let has_green = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.fg == theme::tool_result_ok().fg)
        });
        assert!(has_red, "render_event with diff must have a red delete span");
        assert!(
            has_green,
            "render_event with diff must have a green insert span"
        );
    }

    // ── truncate_result_lines / render_event ToolResult contracts ──

    #[test]
    fn tui_state_expand_defaults_false() {
        let s = TuiState::new(
            "provider".into(),
            "model".into(),
            10,
            vec![],
            vec![],
            vec![],
        );
        assert!(
            !s.expand_tool_results,
            "expand_tool_results must default to false"
        );
    }

    #[test]
    fn tool_result_under_limit_identical_both_modes() {
        let result = "line one\nline two\nline three".to_string();
        let ev = AgentEvent::ToolResult {
            name: "read".into(),
            result,
            ok: true,
        };
        let collapsed = render_event(&ev, false);
        let expanded = render_event(&ev, true);
        assert_eq!(
            collapsed, expanded,
            "under-limit result must be byte-identical in both modes"
        );
        let has_hint = collapsed.iter().any(|l| {
            let t: String = l.spans.iter().flat_map(|s| s.content.chars()).collect();
            t.contains("Ctrl+E")
        });
        assert!(!has_hint, "no hint line for under-limit result");
    }

    #[test]
    fn tool_result_over_limit_collapsed_has_hint_with_hidden_count() {
        // 30 short lines (<500 chars, >15 lines) → line-level truncation, hidden=15.
        let result: String = (0..30)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ev = AgentEvent::ToolResult {
            name: "bash".into(),
            result,
            ok: true,
        };
        let lines = render_event(&ev, false);
        let hints: Vec<String> = lines
            .iter()
            .filter_map(|l| {
                let t: String = l.spans.iter().flat_map(|s| s.content.chars()).collect();
                if t.contains("Ctrl+E") {
                    Some(t)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(hints.len(), 1, "exactly one hint line, got {}", hints.len());
        assert!(
            hints[0].chars().any(|c| c.is_ascii_digit()),
            "hint must contain a hidden-count number, got: {}",
            hints[0]
        );
    }

    #[test]
    fn tool_result_over_limit_expanded_full_no_hint() {
        // Same fixture: 30 short lines. Expanded → all 30 lines, no hint.
        let result: String = (0..30)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ev = AgentEvent::ToolResult {
            name: "bash".into(),
            result,
            ok: true,
        };
        let lines = render_event(&ev, true);
        let has_hint = lines.iter().any(|l| {
            let t: String = l.spans.iter().flat_map(|s| s.content.chars()).collect();
            t.contains("Ctrl+E")
        });
        assert!(!has_hint, "no hint line when expanded");
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            all.contains("L0") && all.contains("L29"),
            "expanded must contain first and last lines verbatim"
        );
    }

    #[test]
    fn tool_result_cjk_long_truncation_does_not_split_multibyte() {
        // 200 CJK chars × 3 bytes = 600 bytes > 500; floor_char_boundary must not split.
        let result: String = "\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(50);
        let ev = AgentEvent::ToolResult {
            name: "read".into(),
            result,
            ok: true,
        };
        let lines = render_event(&ev, false);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            !all.contains('\u{fffd}'),
            "no replacement char from split multibyte"
        );
        assert!(
            all.contains("\u{4f60}\u{597d}"),
            "CJK content must appear verbatim, not corrupted"
        );
    }

    // ── insert_newline（多行输入）──

    #[test]
    fn insert_newline_inserts_at_cursor_mid_buffer() {
        // 在 "ab" 的 a 与 b 之间插入换行：结果 "a\nb"，光标在 \n 之后。
        // Insert \n between a and b in "ab": result "a\nb", cursor after \n.
        let mut s = InputState::new();
        s.insert_char('a');
        s.insert_char('b');
        s.cursor_left();
        s.insert_newline();
        assert_eq!(s.buffer, "a\nb");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn insert_newline_cjk_safe() {
        // CJK 字符间插入换行：3 字节 CJK + 1 字节 \n，光标按字节推进到 4。
        // Insert \n between CJK chars: 3-byte CJK + 1-byte \n, cursor advances to 4.
        let mut s = InputState::new();
        s.insert_char('\u{4f60}'); // 你 (3 bytes)
        s.insert_char('\u{597d}'); // 好 (3 bytes)
        s.cursor_left(); // 光标回到 你 之后 / cursor after 你
        s.insert_newline();
        assert_eq!(s.buffer, "\u{4f60}\n\u{597d}");
        assert_eq!(s.cursor, 4); // 3 (你) + 1 (\n)
    }

    // ── input_window（输入区高度与滚动偏移）──

    #[test]
    fn input_window_empty_height_3() {
        // 空缓冲区：1 行（estimate_input_lines 返回 1），高度 3（含边框）。
        // Empty buffer: 1 line (estimate_input_lines returns 1), height 3.
        let (h, off) = input_window(1, 0, 10);
        assert_eq!(h, 3);
        assert_eq!(off, 0);
    }

    #[test]
    fn input_window_two_lines_height_4() {
        // 2 行内容 → 高度 4（2 + 2 边框/缓冲）。
        // 2 lines of content → height 4 (2 + 2 border/buffer).
        let (h, off) = input_window(2, 0, 10);
        assert_eq!(h, 4);
        assert_eq!(off, 0);
    }

    #[test]
    fn input_window_overflow_caps_at_max_cursor_at_top() {
        // 20 行内容 → 高度 10（钳制上限）；光标在第 0 行 → 偏移 0。
        // 20 lines → height 10 (clamped to max); cursor at line 0 → offset 0.
        let (h, off) = input_window(20, 0, 10);
        assert_eq!(h, 10);
        assert_eq!(off, 0);
    }

    #[test]
    fn input_window_overflow_caps_at_max_cursor_at_bottom() {
        // 20 行内容 → 高度 10；光标在第 19 行 → 偏移使光标行可见。
        // 20 lines → height 10; cursor at line 19 → offset keeps cursor visible.
        let (h, off) = input_window(20, 19, 10);
        assert_eq!(h, 10);
        assert!(off > 0, "offset must be non-zero when cursor beyond window");
        // 光标行在可见窗口内 / cursor line within the visible window
        assert!(off <= 19 && (19 - off) < 10, "cursor line must be visible");
    }

    // ── flush_reasoning / Reasoning 事件 ──
    // ── flush_reasoning / Reasoning event ──

    fn tui_state_for_test() -> TuiState {
        TuiState::new(
            "test".into(),
            "test-model".into(),
            10,
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn flush_reasoning_pushes_event_and_clears() {
        let mut s = tui_state_for_test();
        s.streaming_reasoning = "thinking...".to_string();
        flush_reasoning(&mut s);
        assert_eq!(s.messages.len(), 1, "exactly one event pushed");
        match &s.messages[0] {
            AgentEvent::Reasoning(text) => assert_eq!(text, "thinking..."),
            _ => panic!("expected Reasoning event"),
        }
        assert!(
            s.streaming_reasoning.is_empty(),
            "buffer must be cleared after flush"
        );
    }

    #[test]
    fn flush_reasoning_noop_when_empty() {
        let mut s = tui_state_for_test();
        flush_reasoning(&mut s);
        assert!(s.messages.is_empty(), "no event pushed when buffer empty");
    }

    #[test]
    fn reasoning_lands_before_answer_in_message_list() {
        let mut s = tui_state_for_test();
        handle_action(AgentEvent::ReasoningDelta("think".to_string()), &mut s);
        handle_action(AgentEvent::Agent("answer".to_string()), &mut s);
        let len = s.messages.len();
        assert!(len >= 2, "expected at least 2 messages, got {len}");
        match &s.messages[len - 2] {
            AgentEvent::Reasoning(t) => assert_eq!(t, "think"),
            _ => panic!("expected Reasoning at len-2"),
        }
        match &s.messages[len - 1] {
            AgentEvent::Agent(t) => assert_eq!(t, "answer"),
            _ => panic!("expected Agent at len-1"),
        }
    }

    #[test]
    fn reasoning_preserved_on_error_path() {
        let mut s = tui_state_for_test();
        handle_action(
            AgentEvent::ReasoningDelta("think before error".to_string()),
            &mut s,
        );
        handle_action(AgentEvent::Error("boom".to_string()), &mut s);
        let has_reasoning = s
            .messages
            .iter()
            .any(|m| matches!(m, AgentEvent::Reasoning(_)));
        assert!(
            has_reasoning,
            "reasoning must not be dropped on error path"
        );
    }

    #[test]
    fn render_reasoning_collapsed_one_hint_line_plus_blank() {
        let ev = AgentEvent::Reasoning("secret body line\nsecond line".to_string());
        let lines = render_event(&ev, false);
        assert_eq!(
            lines.len(),
            2,
            "collapsed: exactly header + blank, got {}",
            lines.len()
        );
        let header: String = lines[0]
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            header.contains("Ctrl+E"),
            "header must contain Ctrl+E hint"
        );
        assert!(
            header.contains("\u{601d}\u{8003}\u{8fc7}\u{7a0b}"),
            "header must contain 思考过程"
        );
        assert!(
            !header.contains("secret body line"),
            "collapsed must NOT show body text"
        );
        assert!(
            lines[1].spans.is_empty(),
            "trailing line must be blank"
        );
    }

    #[test]
    fn render_reasoning_expanded_shows_body_verbatim() {
        let body = "\u{7b2c}\u{4e00}\u{884c}\nsecond line";
        let ev = AgentEvent::Reasoning(body.to_string());
        let lines = render_event(&ev, true);
        let header: String = lines[0]
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            header.contains("\u{601d}\u{8003}\u{8fc7}\u{7a0b}"),
            "expanded header must contain 思考过程"
        );
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            all.contains("\u{7b2c}\u{4e00}\u{884c}"),
            "expanded must show CJK body verbatim"
        );
        assert!(
            all.contains("second line"),
            "expanded must show second line verbatim"
        );
    }

    #[test]
    fn agent_with_empty_reasoning_buffer_pushes_no_reasoning_event() {
        let mut s = tui_state_for_test();
        handle_action(AgentEvent::Agent("answer".to_string()), &mut s);
        let has_reasoning = s
            .messages
            .iter()
            .any(|m| matches!(m, AgentEvent::Reasoning(_)));
        assert!(
            !has_reasoning,
            "no Reasoning event when buffer is empty"
        );
    }

    // ===== 会话搜索测试 / in-conversation search tests =====

    fn s_line(text: &str) -> Line<'static> {
        Line::from(Span::raw(text.to_string()))
    }

    fn key_esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())
    }
    fn key_char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty())
    }
    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn key_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())
    }
    fn key_down() -> KeyEvent {
        KeyEvent::new(KeyCode::Down, KeyModifiers::empty())
    }
    fn key_up() -> KeyEvent {
        KeyEvent::new(KeyCode::Up, KeyModifiers::empty())
    }
    fn key_backspace() -> KeyEvent {
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty())
    }

    #[test]
    fn find_matches_basic_substring() {
        let lines = vec![s_line("hello world"), s_line("foo bar"), s_line("hello again")];
        assert_eq!(find_matches(&lines, "hello"), vec![0, 2]);
    }

    #[test]
    fn find_matches_case_insensitive() {
        let lines = vec![s_line("Hello World"), s_line("HELLO there"), s_line("nope")];
        assert_eq!(find_matches(&lines, "hello"), vec![0, 1]);
    }

    #[test]
    fn find_matches_cjk_safe() {
        let lines = vec![s_line("你好世界"), s_line("再见"), s_line("你好 again")];
        assert_eq!(find_matches(&lines, "你好"), vec![0, 2]);
    }

    #[test]
    fn find_matches_empty_query_is_empty() {
        let lines = vec![s_line("hello"), s_line("world")];
        assert!(find_matches(&lines, "").is_empty());
    }

    #[test]
    fn next_match_wraps_forward() {
        assert_eq!(next_match(0, 3), 1);
        assert_eq!(next_match(1, 3), 2);
        assert_eq!(next_match(2, 3), 0);
    }

    #[test]
    fn prev_match_wraps_backward() {
        assert_eq!(prev_match(0, 3), 2);
        assert_eq!(prev_match(2, 3), 1);
        assert_eq!(prev_match(1, 3), 0);
    }

    #[test]
    fn jump_target_scroll_line_zero_vs_last() {
        // total=50, height=20 → base=30
        // line 0: scroll_offset=30 → draw scroll = base - offset = 0 (top line at top)
        assert_eq!(jump_target_scroll(0, 50, 20), 30);
        // last line (49): scroll_offset=0 → draw scroll = 30 (shows [30,50), 49 at bottom)
        assert_eq!(jump_target_scroll(49, 50, 20), 0);
    }

    #[test]
    fn jump_target_scroll_fits_without_scroll() {
        // total ≤ height → base=0, no scroll anywhere
        assert_eq!(jump_target_scroll(0, 10, 20), 0);
        assert_eq!(jump_target_scroll(9, 10, 20), 0);
    }

    #[test]
    fn highlight_matches_patches_only_matched_keeps_fg() {
        use ratatui::style::{Color, Style};
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let lines = vec![
            Line::from(vec![Span::styled("aaa".to_string(), red)]),
            Line::from(vec![Span::styled("bbb".to_string(), blue)]),
            Line::from(vec![Span::raw("ccc".to_string())]),
        ];
        let out = highlight_matches(lines, &[0, 1], Some(0));
        // 当前行（idx 0）：bg=Yellow，fg 仍为 Red
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(out[0].spans[0].style.bg, Some(Color::Yellow));
        // 其它匹配行（idx 1）：bg=DarkGray，fg 仍为 Blue
        assert_eq!(out[1].spans[0].style.fg, Some(Color::Blue));
        assert_eq!(out[1].spans[0].style.bg, Some(Color::DarkGray));
        // 未匹配行（idx 2）：原样，无 bg
        assert_eq!(out[2].spans[0].style.fg, None);
        assert_eq!(out[2].spans[0].style.bg, None);
    }

    #[test]
    fn search_open_then_esc_closes() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new());
        assert!(s.search.is_some());
        apply_search_key(&mut s, key_esc());
        assert!(s.search.is_none(), "Esc must close search mode");
    }

    #[test]
    fn search_esc_does_not_touch_task_state() {
        let mut s = tui_state_for_test();
        s.thinking = true;
        s.search = Some(SearchState::new());
        apply_search_key(&mut s, key_esc());
        assert!(s.search.is_none(), "search closed");
        assert!(s.thinking, "Esc in search must NOT abort the running task");
    }

    #[test]
    fn search_ctrl_f_toggles_closed() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new());
        apply_search_key(&mut s, key_ctrl('f'));
        assert!(s.search.is_none(), "Ctrl+F while searching must close it");
    }

    #[test]
    fn search_ctrl_c_delegates_to_quit() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new());
        // Ctrl+C 须交给主链退出，不被搜索吞掉
        assert!(!apply_search_key(&mut s, key_ctrl('c')), "Ctrl+C must delegate to quit");
        assert!(s.search.is_some(), "search still open until quit clears it");
    }

    #[test]
    fn search_typing_populates_query_cjk_safe() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new());
        apply_search_key(&mut s, key_char('a'));
        apply_search_key(&mut s, key_char('\u{4f60}'));
        let q = s.search.as_ref().unwrap().query.clone();
        assert_eq!(q, "a\u{4f60}");
        assert_eq!(s.search.as_ref().unwrap().cursor, 4);
    }

    #[test]
    fn search_backspace_removes_full_cjk() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new());
        apply_search_key(&mut s, key_char('\u{4f60}'));
        apply_search_key(&mut s, key_char('\u{597d}'));
        apply_search_key(&mut s, key_backspace());
        assert_eq!(s.search.as_ref().unwrap().query, "\u{4f60}");
        assert_eq!(s.search.as_ref().unwrap().cursor, 3);
    }

    #[test]
    fn search_enter_down_up_wrap() {
        let mut s = tui_state_for_test();
        let mut st = SearchState::new();
        st.matches = vec![0, 5, 10];
        st.current = 0;
        s.search = Some(st);
        apply_search_key(&mut s, key_enter());
        assert_eq!(s.search.as_ref().unwrap().current, 1);
        apply_search_key(&mut s, key_down());
        assert_eq!(s.search.as_ref().unwrap().current, 2);
        apply_search_key(&mut s, key_enter()); // 2 → 0 wrap
        assert_eq!(s.search.as_ref().unwrap().current, 0);
        apply_search_key(&mut s, key_up()); // 0 → 2 wrap
        assert_eq!(s.search.as_ref().unwrap().current, 2);
    }

    #[test]
    fn search_no_matches_enter_is_noop() {
        let mut s = tui_state_for_test();
        s.search = Some(SearchState::new()); // empty matches
        apply_search_key(&mut s, key_enter());
        assert_eq!(s.search.as_ref().unwrap().current, 0);
        assert!(s.search.as_ref().unwrap().matches.is_empty());
    }

    // ===== 命令面板测试 / Command palette tests =====

    /// 表完整性：每条 PALETTE_COMMANDS 条目的命令字符串必须被 ReplCommand::parse 识别。
    /// Execute 条目解析为具体变体（非 InvalidUsage）；PlantInput 条目解析为带可选参数
    /// 的变体（None 参数）或已知命令的 InvalidUsage（usage: ...），而非 unknown command。
    ///
    /// Table completeness: every PALETTE_COMMANDS entry's command string must be
    /// recognized by ReplCommand::parse. Execute entries parse to a concrete variant
    /// (not InvalidUsage); PlantInput entries parse to a variant with optional args
    /// (None args) or a known-command InvalidUsage (usage: ...), not "unknown command".
    #[test]
    fn palette_commands_all_parse_to_expected_variants() {
        for (cmd, _, action) in PALETTE_COMMANDS {
            let parsed = ReplCommand::parse(cmd);
            match action {
                PaletteAction::Execute => {
                    assert!(
                        !matches!(parsed, ReplCommand::InvalidUsage(_)),
                        "Execute entry {cmd} must parse to a concrete variant, got InvalidUsage"
                    );
                    assert!(
                        !matches!(parsed, ReplCommand::Goal(_)),
                        "Execute entry {cmd} must not parse as Goal"
                    );
                }
                PaletteAction::PlantInput => match parsed {
                    ReplCommand::Model { slug: None } => {}
                    ReplCommand::History { limit: None } => {}
                    ReplCommand::Plan { plan: None } => {}
                    ReplCommand::InvalidUsage(msg) => {
                        assert!(
                            !msg.contains("unknown command"),
                            "PlantInput entry {cmd} parsed as unknown command: {msg}"
                        );
                    }
                    _ => panic!("PlantInput entry {cmd} parsed as unexpected variant"),
                },
            }
        }
    }

    /// 漂移守卫：ReplCommand 中每个斜杠命令变体都必须在 PALETTE_COMMANDS 中有对应条目。
    /// 防止 repl.rs 新增命令后遗漏面板条目。
    ///
    /// Drift guard: every slash-command variant in ReplCommand must have a
    /// corresponding entry in PALETTE_COMMANDS. Prevents the table from
    /// going stale when repl.rs adds new commands.
    #[test]
    fn palette_commands_cover_all_slash_command_variants() {
        let known_commands = [
            "/model", "/models", "/plan", "/evolve", "/evolve-code", "/add-tool",
            "/add-skill", "/skills", "/context", "/help", "/history", "/lessons",
            "/quit", "/trust",
        ];
        for cmd in known_commands {
            let found = PALETTE_COMMANDS.iter().any(|(c, _, _)| *c == cmd);
            assert!(
                found,
                "slash command {cmd} not represented in PALETTE_COMMANDS"
            );
        }
        let count = PALETTE_COMMANDS.len();
        let unique: std::collections::HashSet<&str> =
            PALETTE_COMMANDS.iter().map(|(c, _, _)| *c).collect();
        assert_eq!(
            count,
            unique.len(),
            "PALETTE_COMMANDS has duplicate entries"
        );
    }

    /// PlantInput 行为：选中 /model 后输入缓冲区变为 "/model "（尾随空格），
    /// 光标在末尾，选择器已关闭，palette_active 已清除。
    ///
    /// PlantInput behavior: selecting /model sets the input buffer to "/model "
    /// (trailing space), cursor at end, selector cleared, palette_active cleared.
    #[test]
    fn palette_plant_input_sets_buffer_and_clears_selector() {
        let mut s = tui_state_for_test();
        let item = SelectorItem {
            label: "/model".into(),
            detail: "".into(),
            data: Some("/model".into()),
        };
        let result = apply_palette_selection(&mut s, &item);
        assert!(matches!(result, Some(PaletteExec::Planted)));
        assert!(
            s.selector.is_none(),
            "selector must be cleared after planting"
        );
        assert!(
            !s.palette_active,
            "palette_active must be cleared after planting"
        );
        assert_eq!(
            s.input.buffer, "/model ",
            "input buffer must be '/model ' with trailing space"
        );
        assert_eq!(
            s.input.cursor,
            s.input.buffer.len(),
            "cursor at end of buffer"
        );
    }

    /// Execute 行为：选中 /skills 后返回 Execute("/skills")，选择器已关闭。
    ///
    /// Execute behavior: selecting /skills returns Execute("/skills"),
    /// selector cleared.
    #[test]
    fn palette_execute_returns_command_and_clears_selector() {
        let mut s = tui_state_for_test();
        let item = SelectorItem {
            label: "/skills".into(),
            detail: "".into(),
            data: Some("/skills".into()),
        };
        let result = apply_palette_selection(&mut s, &item);
        match result {
            Some(PaletteExec::Execute(cmd)) => assert_eq!(cmd, "/skills"),
            _ => panic!("expected Execute for /skills"),
        }
        assert!(
            s.selector.is_none(),
            "selector must be cleared after Execute"
        );
        assert!(
            !s.palette_active,
            "palette_active must be cleared after Execute"
        );
    }

    /// open_palette 后选择器非空，条目数等于 PALETTE_COMMANDS.len()。
    ///
    /// After open_palette, the selector is Some with exactly PALETTE_COMMANDS.len()
    /// items and the palette_active flag is set.
    #[test]
    fn palette_open_populates_all_commands() {
        let mut s = tui_state_for_test();
        open_palette(&mut s);
        assert!(s.palette_active, "palette_active must be set");
        let sel = s
            .selector
            .as_ref()
            .expect("selector must be Some after open_palette");
        assert_eq!(sel.title(), "命令 / Commands");
        assert_eq!(
            sel.visible().len(),
            PALETTE_COMMANDS.len(),
            "visible items must equal PALETTE_COMMANDS length"
        );
    }

    /// 未知命令返回 None，选择器与 palette_active 不变。
    ///
    /// Unknown command returns None; selector and palette_active stay unchanged.
    #[test]
    fn palette_selection_none_for_unknown_command() {
        let mut s = tui_state_for_test();
        s.selector = Some(SelectorState::new("t".into(), vec![], false));
        s.palette_active = true;
        let item = SelectorItem {
            label: "/nonexistent".into(),
            detail: "".into(),
            data: Some("/nonexistent".into()),
        };
        let result = apply_palette_selection(&mut s, &item);
        assert!(
            result.is_none(),
            "unknown command should return None"
        );
    }
}
