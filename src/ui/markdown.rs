/// Markdown 渲染模块：把 Markdown 文本解析并渲染为 ratatui 的 [`Text`]，
/// 支持标题、代码块、行内代码、加粗、斜体、删除线、链接、列表、任务列表、
/// GFM 表格、引用块与段落。
/// Markdown rendering module: parses Markdown text and renders it into ratatui [`Text`],
/// supporting headings, code blocks, inline code, bold, italic, strikethrough,
/// links, lists, task lists, GFM tables, blockquotes, and paragraphs.
use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::ui::theme;
use crate::ui::wrap::char_display_width;

/// Render markdown text into ratatui `Text` with styled spans.
/// 把 Markdown 文本渲染为带样式的 ratatui `Text`。
///
/// Handles headings, code blocks, inline code, bold, italic, links,
/// lists, blockquotes, and paragraphs.
/// 处理标题、代码块、行内代码、加粗、斜体、链接、列表、引用块与段落。
pub fn render_markdown(text: &str) -> Text<'static> {
    let parser = Parser::new_ext(
        text,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    );
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
    // GFM 表格收集状态：单元格内联 span 落入 cur_cell，行末进 cur_row，
    // 表末交 render_table 统一计算列宽（显示宽）并渲染对齐行。
    // GFM table collection state: inline spans land in cur_cell, cells collect
    // into cur_row, and render_table at table end computes column widths
    // (display widths) and renders the aligned rows.
    let mut in_cell = false;
    let mut table_alignments: Vec<Alignment> = Vec::new();
    let mut table_rows: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    let mut cur_row: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur_cell: Vec<Span<'static>> = Vec::new();

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
                current_style = current_style.patch(theme::meta_info());
            }
            Event::Start(Tag::Emphasis) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::emph());
            }
            Event::Start(Tag::Strong) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::strong());
            }
            Event::Start(Tag::Strikethrough) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::strikethrough());
            }
            Event::Start(Tag::Link { .. }) => {
                style_stack.push(current_style);
                current_style = current_style.patch(theme::link());
            }
            Event::Start(Tag::List(_)) => {}
            Event::Start(Tag::Item) => {
                spans.push(Span::raw("  \u{2022} "));
            }
            // GFM 表格：记录列对齐；表头/数据行重置当前行；单元格开启收集。
            // GFM tables: record column alignments; head/data rows reset the
            // current row; a table cell starts collecting spans.
            Event::Start(Tag::Table(aligns)) => {
                table_alignments = aligns;
                table_rows.clear();
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                cur_row = Vec::new();
            }
            Event::Start(Tag::TableCell) => {
                in_cell = true;
                cur_cell = Vec::new();
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
            | Event::End(TagEnd::Link)
            | Event::End(TagEnd::Strikethrough) => {
                current_style = style_stack.pop().unwrap_or_default();
            }
            Event::End(TagEnd::Item) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
            }
            // 单元格结束：span 收进当前行；行结束：行收进行集；表结束：
            // render_table 渲染表头/分隔线/数据行。
            // Cell end: spans join the current row; row end: the row joins the
            // set; table end: render_table emits head/separator/data rows.
            Event::End(TagEnd::TableCell) => {
                in_cell = false;
                cur_row.push(std::mem::take(&mut cur_cell));
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                table_rows.push(std::mem::take(&mut cur_row));
            }
            Event::End(TagEnd::Table) => {
                lines.extend(render_table(&table_rows, &table_alignments));
                lines.push(Line::default());
            }
            Event::End(_) => {}

            // --- Content events ---
            // --- 内容事件 ---
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else if in_cell {
                    cur_cell.push(Span::styled(text.to_string(), current_style));
                } else {
                    spans.push(Span::styled(text.to_string(), current_style));
                }
            }
            Event::Code(code) => {
                let span = Span::styled(code.to_string(), theme::code_inline());
                if in_cell {
                    cur_cell.push(span);
                } else {
                    spans.push(span);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_cell {
                    // 单元格内软换行折叠为空格（GFM 单元格单行渲染）。
                    // Soft breaks inside a cell collapse to a space (GFM cells
                    // render on one line).
                    cur_cell.push(Span::raw(" "));
                } else if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
            }
            // 任务列表：把 Item 的圆点前缀替换为复选框（☑ 勾选 / ☐ 未勾选）。
            // Task lists: replace the Item bullet prefix with a checkbox (☑
            // checked / ☐ unchecked).
            Event::TaskListMarker(checked) => {
                if spans.last().is_some_and(|s| s.content == "  \u{2022} ") {
                    spans.pop();
                }
                let icon = if checked { "  \u{2611} " } else { "  \u{2610} " };
                let style = if checked {
                    theme::tool_result_ok()
                } else {
                    theme::meta_info()
                };
                spans.push(Span::styled(icon, style));
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

// ===== GFM 表格渲染 / GFM table rendering =====

/// 渲染 GFM 表格：列宽取各列单元格的最大显示宽度（CJK 按双宽计），表头行
/// 级样式加粗；分隔行列宽 = 数据列宽 + 1（冒号计入），使 ┼ 恰落在数据行 │
/// 的正下方。
///
/// Renders a GFM table: column widths are the max display width per column
/// (CJK counts double), the header row gets a bold line-level style; the
/// separator row mirrors the data-row structure (" ┼ " junctions, runs of
/// exactly the column display width) so ┼ sits at the same display column
/// as the data rows' │.
fn render_table(
    rows: &[Vec<Vec<Span<'static>>>],
    aligns: &[Alignment],
) -> Vec<Line<'static>> {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if rows.is_empty() || cols == 0 {
        return Vec::new();
    }
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (c, w) in widths.iter_mut().enumerate() {
            *w = (*w).max(row.get(c).map_or(0, |cell| spans_display_width(cell)));
        }
    }
    let mut out = Vec::with_capacity(rows.len() + 1);
    out.push(table_row_line(&rows[0], &widths, aligns, true));
    out.push(table_sep_line(&widths, aligns));
    for row in &rows[1..] {
        out.push(table_row_line(row, &widths, aligns, false));
    }
    out
}

/// 单元格 span 的总显示宽度（CJK=2，见 wrap::char_display_width）。
/// Total display width of a cell's spans (CJK=2, see wrap::char_display_width).
fn spans_display_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .flat_map(|s| s.content.chars())
        .map(|c| char_display_width(c) as usize)
        .sum()
}

/// 渲染一行表格：单元格按列宽与对齐方式补空格（见 align_padding），列间
/// " │ " 分隔，整体缩进两空格（与列表/代码块前缀一致）。
/// Renders one table row: cells padded to column widths per alignment (see
/// align_padding), columns separated by " │ ", indented two spaces (matching
/// the list/code prefix).
fn table_row_line(
    cells: &[Vec<Span<'static>>],
    widths: &[usize],
    aligns: &[Alignment],
    header: bool,
) -> Line<'static> {
    let mut out: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (c, w) in widths.iter().enumerate() {
        if c > 0 {
            out.push(Span::styled(" \u{2502} ", theme::meta_info()));
        }
        let cell = cells.get(c);
        let cw = cell.map_or(0, |cell| spans_display_width(cell));
        let (lpad, rpad) = align_padding(*w, cw, aligns.get(c));
        if lpad > 0 {
            out.push(Span::raw(" ".repeat(lpad)));
        }
        if let Some(cell) = cell {
            out.extend(cell.iter().cloned());
        }
        if rpad > 0 {
            out.push(Span::raw(" ".repeat(rpad)));
        }
    }
    let mut line = Line::from(out);
    if header {
        line.style = theme::strong();
    }
    line
}

/// 分隔行：结构镜像数据行（run 以 " ┼ " 连接），每个 run 的显示宽度恰为
/// 列宽（─ 按显示宽 2 计），因此 ┼ 与数据行的 │ 处于同一显示列。对齐
/// 冒号：Left ":…"、Right "…:"、Center ":…:"。
/// Separator row: structurally mirrors a data row (runs joined by " ┼ "),
/// each run's display width equals its column width (─ counts as 2), so ┼
/// sits at the same display column as the data rows' │. Alignment colons:
/// Left ":…", Right "…:", Center ":…:".
fn table_sep_line(widths: &[usize], aligns: &[Alignment]) -> Line<'static> {
    let mut out: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (c, w) in widths.iter().enumerate() {
        if c > 0 {
            out.push(Span::styled(" \u{253c} ", theme::meta_info()));
        }
        out.push(Span::styled(sep_run(*w, aligns.get(c)), theme::meta_info()));
    }
    Line::from(out)
}

/// 组成一个分隔单元格：冒号（各宽 1）+ `─`（显示宽 2）+ 至多 1 个补位
/// 空格，总显示宽度恰为 w。补位空格的位置镜像单元格填充方向——Right
/// 在左，其余在右。
/// Compose one separator cell: colons (width 1 each) + `─` (display width
/// 2) + at most one parity space, total display width exactly w. The parity
/// space mirrors the cell padding side — leading for Right, trailing
/// otherwise.
fn sep_run(w: usize, align: Option<&Alignment>) -> String {
    let (prefix, suffix): (&str, &str) = match align {
        Some(Alignment::Left) => (":", ""),
        Some(Alignment::Center) => (":", ":"),
        Some(Alignment::Right) => ("", ":"),
        _ => ("", ""),
    };
    let body = w.saturating_sub(prefix.len() + suffix.len());
    let (dashes, pad) = (body / 2, body % 2);
    match align {
        Some(Alignment::Right) => {
            format!("{}{}{}", " ".repeat(pad), "\u{2500}".repeat(dashes), suffix)
        }
        _ => format!(
            "{}{}{}{}",
            prefix,
            "\u{2500}".repeat(dashes),
            " ".repeat(pad),
            suffix
        ),
    }
}

/// 对齐填充 (左, 右)：Right 右对齐，Center 居中，Left/None 左对齐（默认）。
/// Alignment padding (left, right): Right aligns right, Center centers,
/// Left/None align left (the default).
fn align_padding(width: usize, cell: usize, align: Option<&Alignment>) -> (usize, usize) {
    let pad = width.saturating_sub(cell);
    match align {
        Some(Alignment::Right) => (pad, 0),
        Some(Alignment::Center) => {
            let l = (pad + 1) / 2;
            (l, pad - l)
        }
        _ => (0, pad),
    }
}

// ===== syntect 语法高亮胶水 / syntect highlighting glue =====

/// 用 syntect 把代码块按语言着色为带 per-span RGB 前景色的 `Line` 列表。
///
/// - 语法集/主题集通过 `OnceLock` 全局只加载一次（纯 Rust `fancy-regex` 后端）。
/// - 未知语言回退到 `find_syntax_plain_text()`，绝不 panic。
/// - 仅取每个 token 的**前景色**映射为 RGB；绝不设背景色（避免与终端背景冲突）。
/// - 每个输出行仍以 `theme::code_block()` 作为**行级样式**——这是 `wrap.rs:is_code_line`
///   识别代码行、给予硬断+缩进保留换行的契约，per-span RGB 样式叠加其上，颜色仍生效。
/// - 保留两空格左缩进（首 span 为 `Span::raw("  ")`）。
/// - `highlight_line` 出错时退化为该行原始文本单 span，绝不丢行。
///
/// Colorize a code block by language into `Line`s with per-span RGB foreground colors.
///
/// - Syntax/theme sets load once via `OnceLock` (pure-Rust `fancy-regex` backend).
/// - Unknown languages fall back to `find_syntax_plain_text()`, never panic.
/// - Only each token's **foreground** is mapped to an RGB value; never sets a
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
                    let color = theme::syntax_color(syn_st.foreground.r, syn_st.foreground.g, syn_st.foreground.b);
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
                if theme::is_rgb_fg(s.style) {
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

    // ===== GFM 表格 / GFM tables =====

    // 工具函数：每行跨 span 拼接为字符串。
    // Helper: each line joined across spans into a string.
    fn joined_lines(text: &Text<'_>) -> Vec<String> {
        text.lines.iter().map(joined_spans).collect()
    }

    // 工具函数：某字符首次出现处的显示列（字节偏移 → 显示宽度累计）。
    // Helper: display column of a char's first occurrence (byte offset ->
    // accumulated display width).
    fn display_col(line: &str, ch: char) -> usize {
        let idx = line.find(ch).unwrap_or_else(|| panic!("{ch:?} not in {line:?}"));
        line[..idx]
            .chars()
            .map(|c| char_display_width(c) as usize)
            .sum()
    }

    // 表格须渲染表头行、带 ┼ 的分隔行与数据行（列以 │ 分隔、按列宽补齐）。
    // A table must render a header row, a ┼ separator row, and data rows
    // (columns separated by │, padded to column widths).
    #[test]
    fn gfm_table_renders_header_separator_rows() {
        let md = "| name | value |\n| --- | --- |\n| a | 1 |\n| b | 2 |";
        let text = render_markdown(md);
        let lines = joined_lines(&text);
        assert!(
            lines.iter().any(|l| l.contains("name │ value")),
            "header row expected, got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains(" ┼ ")),
            "separator row expected, got {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("a    │ 1")));
        assert!(lines.iter().any(|l| l.contains("b    │ 2")));
    }

    // CJK 列按显示宽度对齐：分隔行 ┼ 与表头行 │ 处于同一显示列。
    // CJK columns align by display width: the separator's ┼ sits at the same
    // display column as the header's │.
    #[test]
    fn gfm_table_columns_align_by_display_width() {
        let md = "| 名字 | value |\n| --- | --- |\n| ab | 1 |";
        let text = render_markdown(md);
        let lines = joined_lines(&text);
        let header = lines.iter().find(|l| l.contains("名字")).expect("header row");
        let sep = lines.iter().find(|l| l.contains('┼')).expect("separator row");
        assert_eq!(
            display_col(header, '│'),
            display_col(sep, '┼'),
            "┼ must align under │: header={header:?} sep={sep:?}"
        );
    }

    // 分隔行按对齐方式渲染冒号：Left ":─…"、Right "─…:"、Center ":─…:"。
    // Separator colons per alignment: Left ":─…", Right "─…:", Center ":─…:".
    #[test]
    fn gfm_table_alignment_markers_on_separator() {
        let md = "| aaa | bbb | ccc |\n|:--|:-:|--:|\n| 1 | 2 | 3 |";
        let text = render_markdown(md);
        let lines = joined_lines(&text);
        let sep = lines.iter().find(|l| l.contains('┼')).expect("separator row");
        assert_eq!(sep, "  :─ ┼ : : ┼ ─:");
    }

    // 单元格保留内联样式：**bold** 带 BOLD 修饰、`x` 带 code_inline 样式。
    // Cells preserve inline styles: **bold** carries BOLD; `x` carries
    // code_inline.
    #[test]
    fn gfm_table_cell_preserves_inline_styles() {
        let md = "| a |\n| --- |\n| **bold** and `x` |";
        let text = render_markdown(md);
        let line = text
            .lines
            .iter()
            .find(|l| joined_spans(l).contains("bold"))
            .expect("cell row");
        assert!(
            line.spans
                .iter()
                .any(|s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD)),
            "bold cell text must carry BOLD, got {:?}",
            line.spans
        );
        assert!(
            line.spans.iter().any(|s| s.style == theme::code_inline()),
            "inline code in cell must keep code_inline style"
        );
    }

    // ~~text~~ 渲染为 CROSSED_OUT 修饰。
    // ~~text~~ renders with the CROSSED_OUT modifier.
    #[test]
    fn strikethrough_applies_crossed_out() {
        let text = render_markdown("~~gone~~");
        assert!(
            text.lines.iter().any(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("gone") && s.style.add_modifier.contains(Modifier::CROSSED_OUT))
            }),
            "strikethrough must render with CROSSED_OUT"
        );
    }

    // 任务列表：圆点前缀被替换为 ☑/☐ 复选框。
    // Task lists: the bullet prefix is replaced with ☑/☐ checkboxes.
    #[test]
    fn task_list_renders_checkbox_markers() {
        let text = render_markdown("- [x] done\n- [ ] todo");
        let lines = joined_lines(&text);
        assert!(
            lines.iter().any(|l| l.contains("☑ done")),
            "checked item expected, got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("☐ todo")),
            "unchecked item expected, got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|l| !l.contains("• ☑") && !l.contains("• ☐")),
            "bullet must be replaced, not doubled"
        );
    }
}
