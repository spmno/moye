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
}
