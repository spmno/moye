/// Markdown 渲染模块：把 Markdown 文本解析并渲染为 ratatui 的 [`Text`]，
/// 支持标题、代码块、行内代码、加粗、斜体、链接、列表、引用块与段落。
/// Markdown rendering module: parses Markdown text and renders it into ratatui [`Text`],
/// supporting headings, code blocks, inline code, bold, italic, links, lists,
/// blockquotes, and paragraphs.
use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::ui::theme;

/// Render markdown text into ratatui `Text` with styled spans.
/// 把 Markdown 文本渲染为带样式的 ratatui `Text`。
///
/// Handles headings, code blocks, inline code, bold, italic, links,
/// lists, blockquotes, and paragraphs.
/// 处理标题、代码块、行内代码、加粗、斜体、链接、列表、引用块与段落。
pub fn render_markdown(text: &str) -> Text<'static> {
    let parser = Parser::new(text);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style_stack: Vec<Style> = Vec::new();
    let mut current_style: Style = Style::new();
    let mut in_code_block = false;
    let mut code_buf = String::new();
    // 当前代码块的语言标签（fence 内）；空串表示无标签或缩进块 → 走原单色路径。
    // Language tag of the current code block (inside the fence); empty means no tag
    // or an indented block → use the original monochrome path.
    let mut code_lang = String::new();

    for event in parser {
        match event {
            // --- Start tags ---
            // --- 起始标签 ---
            Event::Start(Tag::Heading { .. }) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::heading());
            }
            Event::Start(Tag::Paragraph) => {}
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Event::Start(Tag::BlockQuote(_)) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::info());
            }
            Event::Start(Tag::Emphasis) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::emph());
            }
            Event::Start(Tag::Strong) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::strong());
            }
            Event::Start(Tag::Link { .. }) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::link());
            }
            Event::Start(Tag::List(_)) => {}
            Event::Start(Tag::Item) => {
                spans.push(Span::raw("  \u{2022} "));
            }
            Event::Start(_) => {}

            // --- End tags ---
            // --- 结束标签 ---
            Event::End(TagEnd::Heading(_)) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                current_style = style_stack.pop().unwrap_or_default();
            }
            Event::End(TagEnd::Paragraph) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                lines.push(Line::default());
            }
            Event::End(TagEnd::CodeBlock) => {
                if code_lang.is_empty() {
                    // 无语言标签：保留原单色路径，逐行套 theme::code_block()。
                    // No language tag: keep the original monochrome path.
                    for code_line in code_buf.lines() {
                        lines.push(Line::styled(format!("  {code_line}"), theme::code_block()));
                    }
                } else {
                    // 有语言标签：交给 syntect 按语言分词着色。
                    // Has a language tag: delegate to syntect for per-token coloring.
                    lines.extend(highlight_code_block(&code_buf, &code_lang));
                }
                lines.push(Line::default());
                in_code_block = false;
                code_buf.clear();
                code_lang.clear();
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                current_style = style_stack.pop().unwrap_or_default();
            }
            Event::End(TagEnd::Emphasis)
            | Event::End(TagEnd::Strong)
            | Event::End(TagEnd::Link) => {
                current_style = style_stack.pop().unwrap_or_default();
            }
            Event::End(TagEnd::Item) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
            }
            Event::End(_) => {}

            // --- Content events ---
            // --- 内容事件 ---
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else {
                    spans.push(Span::styled(text.to_string(), current_style));
                }
            }
            Event::Code(code) => {
                spans.push(Span::styled(code.to_string(), theme::code_inline()));
            }
            Event::SoftBreak | Event::HardBreak if !spans.is_empty() => {
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            _ => {}
        }
    }

    // Flush remaining spans
    // 刷新剩余未提交的 span
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }

    // 若没有解析出任何行，退化为把原始文本作为单行输出，保证非空 Text。
    // If parsing produced no lines, fall back to emitting the raw text as a single line
    // so the returned Text is never empty.
    if lines.is_empty() {
        lines.push(Line::raw(text.to_string()));
    }

    Text::from(lines)
}

// ===== syntect 语法高亮胶水 / syntect highlighting glue =====

/// 用 syntect 把代码块按语言着色为带 per-span RGB 前景色的 `Line` 列表。
///
/// - 语法集/主题集通过 `OnceLock` 全局只加载一次（纯 Rust `fancy-regex` 后端）。
/// - 未知语言回退到 `find_syntax_plain_text()`，绝不 panic。
/// - 仅取每个 token 的**前景色**映射为 `Color::Rgb`；绝不设背景色（避免与终端背景冲突）。
/// - 每个输出行仍以 `theme::code_block()` 作为**行级样式**——这是 `wrap.rs:is_code_line`
///   识别代码行、给予硬断+缩进保留换行的契约，per-span RGB 样式叠加其上，颜色仍生效。
/// - 保留两空格左缩进（首 span 为 `Span::raw("  ")`）。
/// - `highlight_line` 出错时退化为该行原始文本单 span，绝不丢行。
///
/// Colorize a code block by language into `Line`s with per-span RGB foreground colors.
///
/// - Syntax/theme sets load once via `OnceLock` (pure-Rust `fancy-regex` backend).
/// - Unknown languages fall back to `find_syntax_plain_text()`, never panic.
/// - Only each token's **foreground** is mapped to `Color::Rgb`; never sets a
///   background (would clash with the user's terminal background).
/// - Each output line still carries `theme::code_block()` as its **line-level style** —
///   this is the contract `wrap.rs:is_code_line` uses to detect code lines and give them
///   hard-break + indent-preserving wrap; per-span RGB patches over it, colors still win.
/// - Preserves the two-space left indent (first span is `Span::raw("  ")`).
/// - On `highlight_line` error, falls back to that line's raw text as a single span.
fn highlight_code_block(code: &str, lang: &str) -> Vec<Line<'static>> {
    // 默认语法集/主题集只加载一次（函数内 static，与 config.rs 的 OnceLock 模式一致）。
    // Default syntax/theme sets load once (function-local static, matching config.rs's OnceLock pattern).
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let theme_set = THEME_SET.get_or_init(ThemeSet::load_defaults);
    let theme = &theme_set.themes["base16-ocean.dark"];

    let syntax = syntax_set
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut out: Vec<Line<'static>> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let spans: Vec<Span<'static>> = match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len() + 1);
                // 两空格左缩进，与单色路径一致。
                // Two-space left indent, matching the monochrome path.
                spans.push(Span::raw("  "));
                for (syn_st, text) in &ranges {
                    // 去掉每行尾随换行（LinesWithEndings 保留 \n，但一个 ratatui Line 不应含 \n）。
                    // Strip trailing newline (LinesWithEndings keeps \n; a ratatui Line must not contain \n).
                    let t = text.trim_end_matches('\n');
                    if t.is_empty() {
                        continue;
                    }
                    let color = Color::Rgb(syn_st.foreground.r, syn_st.foreground.g, syn_st.foreground.b);
                    let mut span_style = Style::new().fg(color);
                    // 字体加粗映射（可选但琐碎）：syntect BOLD → ratatui BOLD。
                    // Map bold if trivial: syntect BOLD → ratatui BOLD.
                    if syn_st.font_style.contains(syntect::highlighting::FontStyle::BOLD) {
                        span_style = span_style.add_modifier(Modifier::BOLD);
                    }
                    spans.push(Span::styled(t.to_string(), span_style));
                }
                spans
            }
            Err(_) => {
                // 高亮失败：退化为该行原始文本（去尾随 \n）作为单 span，绝不丢行。
                // Highlight failed: fall back to the raw line text (newline trimmed) as a single span.
                let raw = line.trim_end_matches('\n');
                vec![Span::raw(format!("  {raw}"))]
            }
        };
        // 行级样式保持 theme::code_block()（load-bearing 契约），per-span RGB 叠加其上。
        // Line-level style stays theme::code_block() (load-bearing contract); per-span RGB patches over it.
        out.push(Line {
            spans,
            style: theme::code_block(),
            alignment: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use std::collections::HashSet;

    // 工具函数：提取渲染后代码块行（line.style == theme::code_block()）。
    // Helper: collect rendered code-block lines (line.style == theme::code_block()).
    fn code_lines<'a>(text: &'a Text<'a>) -> Vec<&'a Line<'a>> {
        text.lines
            .iter()
            .filter(|l| l.style == theme::code_block())
            .collect()
    }

    // 工具函数：拼接一行的全部 span 内容。
    // Helper: concatenate all span content of one line.
    fn joined_spans(line: &Line<'_>) -> String {
        line.spans.iter().flat_map(|s| s.content.chars()).collect()
    }

    // 渲染 ```rust 代码块后，span 前景色应有 ≥2 种不同颜色（证明 syntect 真正分词着色）。
    // A ```rust block must yield spans with ≥2 distinct foreground colors (proves syntect tokenized).
    #[test]
    fn highlighted_rust_block_has_multiple_colors() {
        let md = "```rust\nfn main() {\n    let x = 1; // comment\n}\n```";
        let text = render_markdown(md);
        let lines = code_lines(&text);
        assert!(!lines.is_empty(), "expected code-block lines");
        let colors: HashSet<Option<Color>> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.style.fg)
            .collect();
        assert!(
            colors.len() >= 2,
            "expected ≥2 distinct fg colors from syntect, got {colors:?}"
        );
    }

    // 每个 fence 内代码行必须保持 line.style == theme::code_block()（wrap.rs 换行契约）。
    // Every code line inside the fence must keep line.style == theme::code_block() (wrap.rs contract).
    #[test]
    fn code_lines_keep_code_block_line_style() {
        let md = "```rust\nfn main() {\n    let x = 1;\n}\n```";
        let text = render_markdown(md);
        let lines = code_lines(&text);
        assert!(!lines.is_empty());
        for l in &lines {
            assert_eq!(
                l.style,
                theme::code_block(),
                "code line must keep line-level code_block style"
            );
        }
    }

    // 未知语言标签不得 panic，内容须保留，行级样式仍为 code_block。
    // An unknown language tag must not panic; content preserved; line style still code_block.
    #[test]
    fn unknown_language_falls_back_without_panic() {
        let md = "```notareallang\nlet x = 1\n```";
        let text = render_markdown(md); // must not panic
        let lines = code_lines(&text);
        assert!(!lines.is_empty());
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect();
        assert!(
            joined.contains("let x = 1"),
            "content must be preserved; got: {joined:?}"
        );
        for l in &lines {
            assert_eq!(l.style, theme::code_block());
        }
    }

    // 无语言标签的 fence 走原单色路径：span 不带 RGB 着色（继承行级 LightGreen）。
    // A fence with no language tag uses the monochrome path: spans carry no RGB highlighting.
    #[test]
    fn no_language_tag_uses_monochrome_path() {
        let md = "```\nlet x = 1\n```";
        let text = render_markdown(md);
        let lines = code_lines(&text);
        assert!(!lines.is_empty());
        for l in &lines {
            assert_eq!(l.style, theme::code_block());
            for s in &l.spans {
                if let Some(Color::Rgb(..)) = s.style.fg {
                    panic!(
                        "no-tag block must stay monochrome (no RGB), found fg: {:?}",
                        s.style.fg
                    );
                }
            }
        }
    }

    // 代码文本（去掉两空格缩进后）须逐字出现在输出 span 中，无字符丢失或改动。
    // The code text (modulo the 2-space indent) must appear verbatim in output spans.
    #[test]
    fn code_content_preserved_verbatim() {
        let md = "```rust\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n```";
        let text = render_markdown(md);
        let lines = code_lines(&text);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                let c = joined_spans(l);
                c.strip_prefix("  ").map(|s| s.to_string()).unwrap_or(c)
            })
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            rendered,
            ["fn add(a: i32, b: i32) -> i32 {", "    a + b", "}"],
            "code content must be preserved verbatim (modulo 2-space indent)"
        );
    }
}
