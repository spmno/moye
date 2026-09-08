// 主题样式：为 TUI 各组件提供颜色与修饰样式。
// Theme styles: provides colors and modifier styles for TUI components.
use ratatui::style::{Color, Modifier, Style};

// ===== OpenCode 风格调色板（dark）=====
// ===== OpenCode-style palette (dark) =====
// bg 层级: base #212121, panel #252525, darker #121212
// accents: primary(orange) #fab283, secondary(blue) #5c9cf5, accent(purple) #9d7cd8,
//          error #e06c75, warning #f5a742, success #7fd88f, info(cyan) #56b6c2,
//          muted text #6a6a6a, text #e0e0e0
// diff tints: added bg #303A30, removed bg #3A3030

/// 消息区基础背景（base tier）。
/// Message area base background (base tier).
pub fn bg_base() -> Style {
    Style::new().bg(Color::Rgb(0x21, 0x21, 0x21))
}

/// 侧边栏 / 输入框背景（panel tier，比 base 略亮）。
/// Sidebar / input background (panel tier, slightly lighter than base).
pub fn bg_panel() -> Style {
    Style::new().bg(Color::Rgb(0x25, 0x25, 0x25))
}

/// 用户消息块左边框色（OpenCode blue）。
/// User message block left-border color (OpenCode blue).
pub fn border_user() -> Style {
    Style::new().fg(Color::Rgb(0x5c, 0x9c, 0xf5))
}

/// Agent 回复块左边框色（OpenCode orange）。
/// Agent reply block left-border color (OpenCode orange).
pub fn border_agent() -> Style {
    Style::new().fg(Color::Rgb(0xfa, 0xb2, 0x83))
}

/// 工具调用/结果块左边框色（muted gray）。
/// Tool call/result block left-border color (muted gray).
pub fn border_tool() -> Style {
    Style::new().fg(Color::Rgb(0x6a, 0x6a, 0x6a))
}

/// 消息正文文字色（muted gray）。
/// Message body text color (muted gray).
pub fn msg_text() -> Style {
    Style::new().fg(Color::Rgb(0x6a, 0x6a, 0x6a))
}

/// SDD 阶段色：调查者 cyan、规划者 purple、构建者 orange、审计者 green、验证者 blue。
/// 未知角色退回 muted gray。加粗——用于 PhaseStart 单行横幅。
///
/// SDD phase color: investigator cyan, planner purple, builder orange,
/// auditor green, verify blue. Unknown roles fall back to muted gray.
/// Bold — used for the PhaseStart single-line banner.
pub fn phase_style(role: &str) -> Style {
    let color = match role {
        "investigator" => Color::Rgb(0x56, 0xb6, 0xc2), // cyan
        "planner" => Color::Rgb(0x9d, 0x7c, 0xd8), // purple
        "builder" => Color::Rgb(0xfa, 0xb2, 0x83), // orange
        "auditor" => Color::Rgb(0x7f, 0xd8, 0x8f), // green
        "verify" => Color::Rgb(0x5c, 0x9c, 0xf5), // blue
        _ => Color::Rgb(0x6a, 0x6a, 0x6a), // muted fallback
    };
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

/// diff 新增行 span 样式：fg=success green + bg=#303A30 绿色着色。
/// Diff inserted-line span style: fg=success green + bg=#303A30 green tint.
pub fn diff_add() -> Style {
    tool_result_ok().bg(Color::Rgb(0x30, 0x3A, 0x30))
}

/// diff 删除行 span 样式：fg=error red + bg=#3A3030 红色着色。
/// Diff deleted-line span style: fg=error red + bg=#3A3030 red tint.
pub fn diff_del() -> Style {
    tool_result_err().bg(Color::Rgb(0x3A, 0x30, 0x30))
}

/// diff 上下文行背景色（与 base bg 一致，用于 Equal span bg）。
/// Diff context-line background (matches base bg, used for Equal span bg).
pub fn diff_context_bg() -> Style {
    Style::new().bg(Color::Rgb(0x21, 0x21, 0x21))
}

pub fn tool_call() -> Style {
    Style::new().fg(Color::Rgb(0xf5, 0xa7, 0x42)) // warning #f5a742
}

pub fn tool_result_ok() -> Style {
    Style::new().fg(Color::Rgb(0x7f, 0xd8, 0x8f)) // success #7fd88f
}

pub fn tool_result_err() -> Style {
    Style::new().fg(Color::Rgb(0xe0, 0x6c, 0x75)) // error #e06c75
}

pub fn error() -> Style {
    Style::new()
        .fg(Color::Rgb(0xe0, 0x6c, 0x75)) // error #e06c75
        .add_modifier(Modifier::BOLD)
}

pub fn info() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn system() -> Style {
    Style::new().fg(Color::Blue)
}

pub fn border() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn input_prompt() -> Style {
    Style::new()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

pub fn streaming() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn hitl_prompt() -> Style {
    Style::new()
        .fg(Color::Rgb(0xf5, 0xa7, 0x42)) // warning #f5a742
        .add_modifier(Modifier::BOLD)
}

pub fn hitl_border() -> Style {
    Style::new().fg(Color::Rgb(0xe0, 0x6c, 0x75)) // error #e06c75
}

pub fn heading() -> Style {
    Style::new()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD)
}

/// 代码块行级样式：fg=LightGreen + bg=#212121（base bg）。
/// 设 bg 是给代码块 panel 外观的干净方式，同时保持 `wrap.rs:is_code_line`
/// 的 `line.style == theme::code_block()` 契约（两侧调用同一函数）。
///
/// Code-block line-level style: fg=LightGreen + bg=#212121 (base bg).
/// Adding bg is the clean way to give code blocks the panel look while
/// preserving the `wrap.rs:is_code_line` contract
/// (`line.style == theme::code_block()` — both sides call the same fn).
pub fn code_block() -> Style {
    Style::new()
        .fg(Color::LightGreen)
        .bg(Color::Rgb(0x21, 0x21, 0x21))
}

pub fn code_inline() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn link() -> Style {
    Style::new()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::UNDERLINED)
}

pub fn emph() -> Style {
    Style::new().add_modifier(Modifier::ITALIC)
}

pub fn strong() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn usage() -> Style {
    Style::new().fg(Color::DarkGray)
}

// ===== 状态栏样式 =====
// ===== Status bar styles =====

pub fn status_model() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

pub fn status_dim() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn status_turn() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn status_usage() -> Style {
    Style::new().fg(Color::Green)
}

pub fn status_scroll() -> Style {
    Style::new().fg(Color::LightBlue)
}

pub fn status_thinking() -> Style {
    Style::new().fg(Color::LightYellow)
}

pub fn status_ready() -> Style {
    Style::new().fg(Color::Green)
}

pub fn status_hitl() -> Style {
    Style::new()
        .fg(Color::LightRed)
        .add_modifier(Modifier::BOLD)
}

pub fn mcp_connected() -> Style {
    Style::new().fg(Color::Green)
}

pub fn mcp_failed() -> Style {
    Style::new().fg(Color::LightRed)
}

pub fn mcp_tool() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn mcp_error_detail() -> Style {
    Style::new().fg(Color::DarkGray)
}

// ===== 选择器样式 =====
// ===== Selector styles =====

pub fn selector_title() -> Style {
    Style::new()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

pub fn selector_highlight() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

pub fn selector_normal() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn selector_dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn selector_input() -> Style {
    Style::new()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD)
}

// ===== 会话搜索高亮样式 =====
// ===== In-conversation search highlight styles =====
// 仅设 bg：通过 Style::patch 叠加时保留原 span 的 fg / modifier。
// bg-only: when patched via Style::patch the original span fg / modifier is preserved.

/// 当前匹配行高亮（黄色背景）。
/// Current-match line highlight (yellow background).
pub fn search_current() -> Style {
    Style::new().bg(Color::Yellow)
}

/// 其它匹配行高亮（深灰背景）。
/// Other-match line highlight (dark-gray background).
pub fn search_match() -> Style {
    Style::new().bg(Color::DarkGray)
}
