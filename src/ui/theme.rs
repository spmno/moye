// 主题样式：为 TUI 各组件提供颜色与修饰样式。
// Theme styles: provides colors and modifier styles for TUI components.
use ratatui::style::{Color, Modifier, Style};

pub fn user_msg() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

pub fn tool_call() -> Style {
    Style::new().fg(Color::LightYellow)
}

pub fn tool_result_ok() -> Style {
    Style::new().fg(Color::Green)
}

pub fn tool_result_err() -> Style {
    Style::new().fg(Color::LightRed)
}

pub fn error() -> Style {
    Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD)
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
    Style::new().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
}

pub fn streaming() -> Style {
    Style::new().fg(Color::Gray)
}

pub fn hitl_prompt() -> Style {
    Style::new()
        .fg(Color::LightYellow)
        .add_modifier(Modifier::BOLD)
}

pub fn hitl_border() -> Style {
    Style::new().fg(Color::LightRed)
}

pub fn heading() -> Style {
    Style::new()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD)
}

pub fn code_block() -> Style {
    Style::new().fg(Color::LightGreen)
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
    Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD)
}

// ===== 选择器样式 =====
// ===== Selector styles =====

pub fn selector_title() -> Style {
    Style::new().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
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
    Style::new().fg(Color::LightYellow).add_modifier(Modifier::BOLD)
}
