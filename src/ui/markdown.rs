/// Markdown 渲染模块：把 Markdown 文本解析并渲染为 ratatui 的 [`Text`]，
/// 支持标题、代码块、行内代码、加粗、斜体、链接、列表、引用块与段落。
/// Markdown rendering module: parses Markdown text and renders it into ratatui [`Text`],
/// supporting headings, code blocks, inline code, bold, italic, links, lists,
/// blockquotes, and paragraphs.
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};

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

    for event in parser {
        match event {
            // --- Start tags ---
            // --- 起始标签 ---
            Event::Start(Tag::Heading { .. }) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::heading());
            }
            Event::Start(Tag::Paragraph) => {}
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                code_buf.clear();
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
                for code_line in code_buf.lines() {
                    lines.push(Line::styled(
                        format!("  {code_line}"),
                        theme::code_block(),
                    ));
                }
                lines.push(Line::default());
                in_code_block = false;
                code_buf.clear();
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
                spans.push(Span::styled(
                    code.to_string(),
                    theme::code_inline(),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
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
