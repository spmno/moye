//! 按显示宽度对 ratatui [`Line`] 做软换行（词边界优先，超长词硬断），
//! 使消息区能在固定宽度下完整显示长行而不被终端右边缘截断。
//!
//! 渲染时把这些"显示行"喂给 **不带** `.wrap()` 的 `Paragraph`，从而保持
//! "1 显示行 == 1 屏幕行"的不变量——文本选择 / 滚动逻辑无需任何改动即可
//! 正确工作（选区、高亮、滚动都以显示行为单位）。
//!
//! Soft-wrap ratatui [`Line`]s by display width (word-boundary first, hard-break
//! for overlong words) so the message area shows long lines fully instead of
//! being clipped at the terminal's right edge.
//!
//! The wrapped "display lines" are fed to a `Paragraph` **without** `.wrap()`,
//! preserving the "1 display line == 1 screen row" invariant — the text-selection
//! / scroll logic needs no changes and keeps working correctly (selection,
//! highlight, and scrolling all operate on display lines).

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::theme;

/// 字符显示宽度：ASCII=1，其余=2。
/// 与 `selection.rs` 的列→字符 offset 映射共用同一宽度模型，保证换行点
/// 与选区列计算一致。
///
/// Character display width: ASCII=1, others=2. Shared with `selection.rs`'s
/// column→char-offset mapping so wrap points agree with selection column math.
pub(crate) fn char_display_width(c: char) -> u16 {
    if c.is_ascii() { 1 } else { 2 }
}

/// 把单个 [`Line`] 按显示宽度 `width` 拆成若干显示行。
///
/// - 词边界优先换行：连续非空白为一个"词"，连续空白作为分隔词保留。
/// - 单个词超过 `width` 时在词内按字符硬断（绝不死循环）。
/// - 保留原 span 的逐字符样式；输出行继承原行的行级 `style` 与 `alignment`。
/// - 空行（无 span）产出单个空显示行，保持段落间距。
///
/// `width == 0` 时不换行，原样返回单行（退化终端下避免除零 / 死循环）。
///
/// Split a single [`Line`] into display lines each fitting within `width`.
///
/// - Wraps at word boundaries (runs of non-space form a "word"; runs of spaces
///   are preserved as separator tokens).
/// - A word longer than `width` is hard-broken char-by-char (never loops).
/// - Per-char span styles are preserved; output lines inherit the original
///   line's line-level `style` and `alignment`.
/// - An empty line (no spans) yields one empty display line, keeping paragraph
///   spacing.
///
/// With `width == 0` no wrapping is done (degenerate terminal; avoids div-by-zero
/// / infinite loops).
pub(crate) fn wrap_line(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    // 代码块行（由 markdown::render_markdown 以 theme::code_block() 行级样式发出）
    // 用硬断+续行保留缩进的策略，避免词边界换行破坏代码结构。
    // Code-block lines (line-level style == theme::code_block()) hard-break
    // per char and preserve indentation on continuations, so wrapped code
    // keeps its structure instead of being word-wrapped into a mangled blob.
    if is_code_line(line) {
        return wrap_code_line(line, width);
    }

    let line_style = line.style;
    let line_alignment = line.alignment;

    if width == 0 {
        return vec![line.clone()];
    }
    let width = width as usize;

    // 展开为 (字符, 该字符所属 span 样式) 序列。
    // Flatten into a (char, owning-span style) sequence.
    let units: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();

    if units.is_empty() {
        return vec![Line::default()];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w: usize = 0;

    // 把相邻同"空白类"的字符合成一个 token（词或空白串），逐 token 累积。
    // Group adjacent chars of the same whitespace-class into a token (a word or
    // a run of spaces), then accumulate token by token.
    let mut i = 0;
    while i < units.len() {
        let token_start = i;
        let is_space = units[i].0.is_whitespace();
        while i < units.len() && units[i].0.is_whitespace() == is_space {
            i += 1;
        }
        let token = &units[token_start..i];
        let token_w: usize = token
            .iter()
            .map(|(c, _)| char_display_width(*c) as usize)
            .sum();

        // 放得下当前行 → 直接追加。
        if cur_w + token_w <= width {
            cur.extend_from_slice(token);
            cur_w += token_w;
            continue;
        }

        // 放不下：先把已累积的行 flush，再处理超宽 token。
        if !cur.is_empty() {
            out.push(make_line(&cur, line_style, line_alignment));
            cur.clear();
            cur_w = 0;
        }

        if token_w <= width {
            // 词本身不超宽，开新行放它。
            cur.extend_from_slice(token);
            cur_w = token_w;
        } else {
            // 词本身超宽：逐字符硬断，满即 flush（cur 为空时强制塞首字符避免死循环）。
            for (c, st) in token {
                let cw = char_display_width(*c) as usize;
                if cur_w + cw > width && !cur.is_empty() {
                    out.push(make_line(&cur, line_style, line_alignment));
                    cur.clear();
                    cur_w = 0;
                }
                cur.push((*c, *st));
                cur_w += cw;
            }
        }
    }
    if !cur.is_empty() {
        out.push(make_line(&cur, line_style, line_alignment));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// 识别代码块行：`render_markdown` 以 `theme::code_block()` 作为行级样式
/// 发出代码块每行，借此区分代码与普通文本，无需在 ratatui `Line` 上挂额外标记。
///
/// Detect a code-block line: `render_markdown` emits each code-block line with
/// `theme::code_block()` as its line-level style, so we branch on that without
/// needing extra metadata on ratatui's `Line`.
fn is_code_line(line: &Line<'_>) -> bool {
    line.style == theme::code_block()
}

/// 代码块行专用换行：按字符硬断（绝不词边界断），续行重复原行前导空白，
/// 使换行后代码仍对齐缩进。字符级 span 样式保留，输出行继承行级样式/对齐。
///
/// `width == 0` 时不换行，原样返回单行（退化终端，避免除零/死循环）。
/// 前导空白宽度上限为 `width/2`（窄终端下截断缩进，保证每行仍能放下内容）。
///
/// Code-block-specific wrapping: hard-break per character (never at word
/// boundaries), repeating the original leading whitespace on every continuation
/// line so wrapped code stays indentation-aligned. Per-char span styles are
/// preserved; output lines inherit the line-level style / alignment.
///
/// With `width == 0` no wrapping is done (degenerate terminal; avoids div-by-zero
/// / infinite loops). Leading-whitespace width is capped at `width/2` so a narrow
/// terminal still shows some content per row.
fn wrap_code_line(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let line_style = line.style;
    let line_alignment = line.alignment;

    if width == 0 {
        return vec![line.clone()];
    }
    let width = width as usize;

    let units: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();

    if units.is_empty() {
        return vec![Line::default()];
    }

    // 原行前导空白：续行重复它，保持缩进对齐。
    // Original leading whitespace: repeated on continuation lines to keep
    // indentation aligned.
    let indent: Vec<(char, Style)> = units
        .iter()
        .take_while(|(c, _)| c.is_whitespace())
        .map(|(c, s)| (*c, *s))
        .collect();
    let indent_w: usize = indent
        .iter()
        .map(|(c, _)| char_display_width(*c) as usize)
        .sum();

    // 窄终端下截断缩进，保留至多 width/2 列，确保续行仍有内容空间。
    // Truncate indentation in narrow terminals to at most width/2 columns so
    // continuation lines still have room for content.
    let max_indent = width.checked_sub(2).map(|w| w / 2).unwrap_or(0);
    let (indent, indent_w) = if indent_w <= max_indent {
        (indent, indent_w)
    } else {
        let mut kept: Vec<(char, Style)> = Vec::new();
        let mut kept_w: usize = 0;
        for (c, s) in &indent {
            let cw = char_display_width(*c) as usize;
            if kept_w + cw > max_indent {
                break;
            }
            kept.push((*c, *s));
            kept_w += cw;
        }
        (kept, kept_w)
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w: usize = 0;

    for (c, st) in &units {
        let cw = char_display_width(*c) as usize;
        if cur_w + cw > width && !cur.is_empty() {
            out.push(make_line(&cur, line_style, line_alignment));
            cur.clear();
            // 续行先放缩进前缀，再继续放溢出字符。
            cur.extend_from_slice(&indent);
            cur_w = indent_w;
        }
        cur.push((*c, *st));
        cur_w += cw;
    }
    if !cur.is_empty() {
        out.push(make_line(&cur, line_style, line_alignment));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// 由 (字符, 样式) 序列构造显示行：合并相邻同样式字符以减少 span 数量，
/// 并套上行级样式 / 对齐。
///
/// Build a display line from a (char, style) sequence: merge adjacent same-style
/// chars to cut span count, then apply line-level style / alignment.
fn make_line(chars: &[(char, Style)], line_style: Style, line_alignment: Option<ratatui::layout::Alignment>) -> Line<'static> {
    Line { spans: spans_from_chars(chars), style: line_style, alignment: line_alignment }
}

fn spans_from_chars(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_style: Option<Style> = None;
    for (c, st) in chars {
        if Some(*st) != cur_style {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), cur_style.unwrap_or_default()));
            }
            cur_style = Some(*st);
        }
        buf.push(*c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style.unwrap_or_default()));
    }
    spans
}

/// 对多行逐行做软换行，返回扁平的显示行列表。
/// Soft-wrap each input line; return a flat list of display lines.
pub(crate) fn wrap_lines(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    fn widths(lines: &[Line<'static>]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .flat_map(|s| s.content.chars())
                    .map(char_display_width as fn(char) -> u16)
                    .map(|w| w as usize)
                    .sum()
            })
            .collect()
    }

    #[test]
    fn short_line_unchanged() {
        let line = Line::from(Span::raw("hello"));
        let out = wrap_line(&line, 80);
        assert_eq!(out.len(), 1);
        assert_eq!(widths(&out), [5]);
    }

    #[test]
    fn wraps_long_ascii_line() {
        let line = Line::from(Span::raw("alpha beta gamma delta"));
        let out = wrap_line(&line, 11);
        // 每行 ≤ 11 列。
        for w in &widths(&out) {
            assert!(*w <= 11, "line too wide: {w}");
        }
        assert!(out.len() > 1);
        // 拼接后内容等价（空白可能重新分布，但字符集不变）。
        let joined: String = out
            .iter()
            .flat_map(|l| l.spans.iter().flat_map(|s| s.content.chars()))
            .collect();
        assert_eq!(joined.replace('\n', ""), "alpha beta gamma delta".replace('\n', ""));
    }

    #[test]
    fn wraps_cjk_line() {
        // 6 个 CJK 字符 = 12 列，width=4 → 每行最多 2 个字符（4 列）。
        let line = Line::from(Span::raw("你好世界再见"));
        let out = wrap_line(&line, 4);
        for w in &widths(&out) {
            assert!(*w <= 4, "cjk line too wide: {w}");
        }
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn empty_line_yields_one_empty() {
        let line = Line::default();
        let out = wrap_line(&line, 80);
        assert_eq!(out.len(), 1);
        assert!(out[0].spans.is_empty());
    }

    #[test]
    fn width_zero_no_wrap() {
        let line = Line::from(Span::raw("a very long line that would normally wrap"));
        let out = wrap_line(&line, 0);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn preserves_line_level_style() {
        let sty = Style::new().fg(Color::Red);
        let line = Line::styled("hello world foo", sty);
        let out = wrap_line(&line, 6);
        assert!(out.len() > 1);
        // 每个显示行都继承行级样式。
        for l in &out {
            assert_eq!(l.style, sty);
        }
    }

    #[test]
    fn preserves_span_styles() {
        let s1 = Style::new().fg(Color::Red);
        let s2 = Style::new().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("aaaaa", s1),
            Span::styled("bbbbb", s2),
        ]);
        let out = wrap_line(&line, 5);
        assert_eq!(out.len(), 2);
        // 第一行全是 s1，第二行全是 s2。
        assert_eq!(out[0].spans[0].style, s1);
        assert_eq!(out[1].spans[0].style, s2);
    }

    #[test]
    fn overlong_word_hard_breaks() {
        // 单个 20 字符词，width=5 → 硬断成 4 行。
        let line = Line::from(Span::raw("aaaaaaaaaaaaaaaaaaaa"));
        let out = wrap_line(&line, 5);
        assert_eq!(out.len(), 4);
        for w in &widths(&out) {
            assert!(*w <= 5);
        }
    }

    #[test]
    fn wrap_lines_flattens() {
        let lines = vec![
            Line::from(Span::raw("short")),
            Line::from(Span::raw("a very long line that needs wrapping")),
            Line::default(),
        ];
        let out = wrap_lines(&lines, 10);
        assert!(out.len() > lines.len());
    }

    // ===== 代码块换行测试 / code-block wrapping tests =====

    fn code_line(content: &str) -> Line<'static> {
        Line::styled(content.to_string(), theme::code_block())
    }

    fn joined(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().flat_map(|s| s.content.chars()))
            .collect()
    }

    #[test]
    fn code_line_short_unchanged() {
        let line = code_line("  let x = 1;");
        let out = wrap_line(&line, 80);
        assert_eq!(out.len(), 1);
        assert_eq!(joined(&out), "  let x = 1;");
        // 行级样式保持 code_block。
        for l in &out {
            assert_eq!(l.style, theme::code_block());
        }
    }

    #[test]
    fn code_line_preserves_indent_on_continuation() {
        // 6 列前导空白 + 长内容，width=20（max_indent=9 ≥ 6，缩进完整保留）。
        // 续行须以同样的 6 列空白开头，保持代码缩进对齐。
        let line = code_line("      let result = some_function(a, b, c, d);");
        let out = wrap_line(&line, 20);
        assert!(out.len() > 1);
        for w in &widths(&out) {
            assert!(*w <= 20, "code line too wide: {w}");
        }
        // 第一行原样开头。
        assert!(joined(&[out[0].clone()].to_vec()).starts_with("      let"));
        // 续行都以 6 列空白开头（保留缩进）。
        for l in out.iter().skip(1) {
            let s: String = l
                .spans
                .iter()
                .flat_map(|sp| sp.content.chars())
                .take_while(|c| c.is_whitespace())
                .collect();
            assert_eq!(s.len(), 6, "continuation lost indentation prefix");
        }
    }

    #[test]
    fn code_line_no_word_boundary_break() {
        // 词边界换行会把 "foo(arg1, arg2)" 拆在空格处；硬断应保持非空白串完整
        // 直到宽度用尽，而非优先在空格断开。
        let line = code_line("  foo(arg1,arg2,arg3,arg4,arg5,arg6,arg7);");
        let out = wrap_line(&line, 14);
        assert!(out.len() > 1);
        // 续行前缀是 2 空白，之后不应出现以 "arg" 开头却丢掉缩进的情况。
        for l in &out {
            let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert!(
                s.starts_with("  "),
                "continuation must keep the 2-space prefix: {s:?}"
            );
        }
    }

    #[test]
    fn code_line_width_zero_no_wrap() {
        let line = code_line("  let x = 1;");
        let out = wrap_line(&line, 0);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn code_line_empty_yields_one_empty() {
        let line = code_line("");
        let out = wrap_line(&line, 80);
        assert_eq!(out.len(), 1);
        assert!(out[0].spans.is_empty());
    }

    #[test]
    fn code_line_deep_indent_capped() {
        // 30 列缩进，width=10 → 缩进被截到 width/2=4，保证续行有内容空间。
        let line = code_line(&format!("{}content", " ".repeat(30)));
        let out = wrap_line(&line, 10);
        assert!(out.len() > 1);
        for w in &widths(&out) {
            assert!(*w <= 10, "capped-indent line too wide: {w}");
        }
    }

    #[test]
    fn non_code_line_uses_word_wrap() {
        // 普通行（非 code_block 样式）走词边界换行，不受代码策略影响。
        let line = Line::from(Span::raw("alpha beta gamma delta epsilon"));
        let out = wrap_line(&line, 11);
        assert!(out.len() > 1);
        for w in &widths(&out) {
            assert!(*w <= 11);
        }
    }
}
