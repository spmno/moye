// 主题样式：为 TUI 各组件提供颜色与修饰样式。
// Theme styles: provides colors and modifier styles for TUI components.
use ratatui::style::{Color, Modifier, Style};

// ===== OpenCode 风格调色板（dark）=====
// ===== OpenCode-style palette (dark) =====
// bg 层级（3-tier，由亮到暗）:
//   base    #212121  消息区背景（最亮）
//   sidebar #1a1a1a  侧边栏背景（比 base 暗一档）
//   divider #121212  1 格分隔列背景（最暗）
// bg tier (3-tier, lightest → darkest):
//   base    #212121  message area background (lightest)
//   sidebar #1a1a1a  sidebar background (one notch darker than base)
//   divider #121212  1-cell separator column background (darkest)
// accents: primary(orange) #fab283, secondary(blue) #5c9cf5, accent(purple) #9d7cd8,
//          error #e06c75, warning #f5a742, success #7fd88f, info(cyan) #56b6c2,
//          muted text #6a6a6a, text #e0e0e0
// diff tints: added bg #303A30, removed bg #3A3030

/// 消息区基础背景（base tier，最亮）。
/// Message area base background (base tier, lightest).
pub fn bg_base() -> Style {
    Style::new().bg(Color::Rgb(0x21, 0x21, 0x21))
}

/// 侧边栏背景（sidebar tier，比 base 暗一档 #1a1a1a）。
/// Sidebar background (sidebar tier, one notch darker than base #1a1a1a).
pub fn sidebar_bg() -> Style {
    Style::new().bg(Color::Rgb(0x1a, 0x1a, 0x1a))
}

/// 区域分隔带背景（divider tier，纯背景列、非线条字符，最暗 #121212）。
/// Region separator band background (divider tier — a pure background
/// column, not a line glyph; darkest tier #121212).
pub fn divider_bg() -> Style {
    Style::new().bg(Color::Rgb(0x12, 0x12, 0x12))
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

/// 消息正文文字色（default fg #e0e0e0）。
/// Message body text color (default fg #e0e0e0).
pub fn message_content() -> Style {
    Style::new().fg(Color::Rgb(0xe0, 0xe0, 0xe0))
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

/// 元信息文字色（dimmed #6a6a6a）：回合计数、token/用量行、CWD 路径、
/// 时间戳、git 信息、版本字符串、截断提示、diff 上下文行。
///
/// Meta-info text color (dimmed #6a6a6a): turn counters, token/usage
/// lines, CWD path, timestamps, git info, version strings, truncation
/// hints, diff context lines.
pub fn meta_info() -> Style {
    Style::new().fg(Color::Rgb(0x6a, 0x6a, 0x6a))
}

/// 系统消息色（blue fg）。
/// System message color (blue fg).
pub fn system() -> Style {
    Style::new().fg(Color::Blue)
}

/// 输入提示色（LightCyan + BOLD）。
/// Input prompt color (LightCyan + BOLD).
pub fn input_prompt() -> Style {
    Style::new()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD)
}

/// 流式输出色（gray fg）。
/// Streaming output color (gray fg).
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

// ===== 侧边栏样式 =====
// ===== Sidebar styles =====

/// 侧边栏分组标题样式（muted fg + BOLD，轻微加粗无色彩噪音）。
/// Sidebar group section title style (muted fg + BOLD; slight bold,
/// no color noise).
pub fn sidebar_title() -> Style {
    Style::new()
        .fg(Color::Rgb(0x6a, 0x6a, 0x6a))
        .add_modifier(Modifier::BOLD)
}

/// 侧边栏列表项样式（dimmed #6a6a6a，用于工具名、MCP 服务名、技能名）。
/// Sidebar list-item style (dimmed #6a6a6a, for tool names, MCP server
/// names, skill names).
pub fn tool_item() -> Style {
    Style::new().fg(Color::Rgb(0x6a, 0x6a, 0x6a))
}

/// 就绪/成功状态色（green）。
/// Ready/success status color (green).
pub fn status_ok() -> Style {
    Style::new().fg(Color::Green)
}

/// 错误状态色（error red #e06c75）。
/// Error status color (error red #e06c75).
pub fn status_error() -> Style {
    Style::new().fg(Color::Rgb(0xe0, 0x6c, 0x75))
}

pub fn status_thinking() -> Style {
    Style::new().fg(Color::LightYellow)
}

pub fn status_hitl() -> Style {
    Style::new()
        .fg(Color::LightRed)
        .add_modifier(Modifier::BOLD)
}

pub fn status_scroll() -> Style {
    Style::new().fg(Color::LightBlue)
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

// ===== syntect 胶水辅助 =====
// ===== syntect glue helpers =====

/// 从 syntect 高亮输出构造 RGB 颜色（避免在 markdown.rs 中直接引用 Color）。
/// Construct an RGB color from syntect highlighting output
/// (avoids a direct Color reference in markdown.rs).
pub fn syntax_color(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// 检查样式的 fg 是否为 RGB 颜色（用于区分 syntect 着色与单色路径）。
/// Check if a style's fg is an RGB color (used to distinguish syntect
/// coloring from the monochrome path).
#[cfg(test)]
pub fn is_rgb_fg(style: Style) -> bool {
    matches!(style.fg, Some(Color::Rgb(..)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 侧边栏背景 #1a1a1a 必须比消息区背景 #212121 更暗。
    /// Sidebar bg #1a1a1a must be darker than base bg #212121.
    #[test]
    fn sidebar_bg_is_darker_than_base() {
        let base_bg = bg_base().bg.expect("bg_base must have bg");
        let side_bg = sidebar_bg().bg.expect("sidebar_bg must have bg");
        match (base_bg, side_bg) {
            (Color::Rgb(br, bg, bb), Color::Rgb(sr, sg, sb)) => {
                assert!(sr < br, "sidebar R {sr:#x} must be < base R {br:#x}");
                assert!(sg < bg, "sidebar G {sg:#x} must be < base G {bg:#x}");
                assert!(sb < bb, "sidebar B {sb:#x} must be < base B {bb:#x}");
            }
            _ => panic!("both bg must be Color::Rgb"),
        }
    }

    /// 分隔列背景 #121212 必须比侧边栏背景 #1a1a1a 更暗（3-tier 最暗）。
    /// Divider bg #121212 must be darker than sidebar bg #1a1a1a
    /// (darkest of the 3 tiers).
    #[test]
    fn divider_is_darkest_tier() {
        let side_bg = sidebar_bg().bg.expect("sidebar_bg must have bg");
        let div_bg = divider_bg().bg.expect("divider_bg must have bg");
        match (side_bg, div_bg) {
            (Color::Rgb(sr, sg, sb), Color::Rgb(dr, dg, db)) => {
                assert!(dr < sr, "divider R {dr:#x} must be < sidebar R {sr:#x}");
                assert!(dg < sg, "divider G {dg:#x} must be < sidebar G {sg:#x}");
                assert!(db < sb, "divider B {db:#x} must be < sidebar B {sb:#x}");
            }
            _ => panic!("both bg must be Color::Rgb"),
        }
    }

    /// 侧边栏标题样式必须带 BOLD 修饰符。
    /// Sidebar title style must carry the BOLD modifier.
    #[test]
    fn sidebar_title_is_bold() {
        let style = sidebar_title();
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "sidebar_title must be BOLD"
        );
    }

    /// 输入框组件样式验证：border_user 不设 bg（本体透明），
    /// bg_base 设 bg（作为透明输入框下方的 base 层）。
    ///
    /// Input block component check: border_user sets no bg
    /// (transparent body), bg_base sets bg (the base tier underneath
    /// the transparent input).
    #[test]
    fn input_block_components_have_no_bg() {
        assert!(
            border_user().bg.is_none(),
            "border_user must not set bg (input body transparent)"
        );
        assert!(
            bg_base().bg.is_some(),
            "bg_base must set bg (base tier underneath transparent input)"
        );
    }
}
