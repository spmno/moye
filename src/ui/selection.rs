//! 字符级文本选择状态机：把消息区屏幕坐标（行, 列）映射到逻辑行 + 字符 offset，
//! 支持鼠标拖拽选区与文本提取。
//!
//! 设计前提：消息区 `Paragraph` 不开 `.wrap()`，且 `markdown::render_markdown`
//! 不按宽度换行，因此 **1 Line == 1 屏幕行**，屏幕行→逻辑行映射是一次减法。
//! 屏幕列→字符 offset 按简易显示宽度（ASCII=1，其余=2）累加，与项目其他
//! 宽度估算（`estimate_input_lines`、`draw_hitl_overlay`）一致。
//!
//! Character-level text selection state machine: maps message-area screen
//! coordinates (row, col) to logical line + char offset, supporting mouse-drag
//! selection and text extraction.
//!
//! Design premise: the message `Paragraph` is rendered without `.wrap()`, and
//! `markdown::render_markdown` does not wrap by width, so **1 Line == 1 screen
//! row**; the screen-row→logical-line map is a single subtraction. Screen-col→
//! char offset uses a simple display width (ASCII=1, others=2), consistent with
//! the project's other width estimates (`estimate_input_lines`, `draw_hitl_overlay`).

use ratatui::text::Line;

/// 字符级选区。坐标存"逻辑行 + 屏幕相对列"（列相对消息内容区左边，0 = 内容首列）。
/// anchor 在 start_*，focus 在 end_*；选择方向（正向/反向）由 `bounds()` 规范化。
///
/// Character-level selection. Coordinates are "logical line + screen-relative
/// column" (column relative to the content area's left edge; 0 = first content
/// column). Anchor at start_*, focus at end_*; direction is normalized by `bounds()`.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    start_line: usize,
    start_col: u16,
    end_line: usize,
    end_col: u16,
}

impl Selection {
    pub fn new_at(line: usize, col: u16) -> Self {
        Self {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
        }
    }

    /// 屏幕行→逻辑行：`logical = scroll + (row - inner_y)`。
    /// 因 Paragraph 无 Wrap，1 Line == 1 屏幕行，映射是一次减法。
    pub fn screen_to_logical(row: u16, inner_y: u16, scroll: u16) -> usize {
        scroll as usize + row.saturating_sub(inner_y) as usize
    }

    pub fn extend(&mut self, line: usize, col: u16) {
        self.end_line = line;
        self.end_col = col;
    }

    /// 规范化后的起终点：(lo_line, lo_col, hi_line, hi_col)，
    /// 保证 lo_line <= hi_line，且同行时 lo_col <= hi_col。
    pub fn bounds(&self) -> (usize, u16, usize, u16) {
        if self.start_line < self.end_line
            || (self.start_line == self.end_line && self.start_col <= self.end_col)
        {
            (self.start_line, self.start_col, self.end_line, self.end_col)
        } else {
            (self.end_line, self.end_col, self.start_line, self.start_col)
        }
    }

    /// 提取选区文本。单行 = [lo_col..hi_col]；多行 = 首行[lo_col..]、
    /// 中间行整行、末行[..hi_col]。屏幕列经 `screen_col_to_char_offset`
    /// 映射到字符串字节 offset 后切片。越界索引 clamp，不 panic。
    pub fn extract(&self, lines: &[Line]) -> String {
        if lines.is_empty() {
            return String::new();
        }
        let last = lines.len() - 1;
        let (lo_line, lo_col, hi_line, hi_col) = self.bounds();
        let lo_line = lo_line.min(last);
        let hi_line = hi_line.min(last);
        if lo_line > hi_line {
            return String::new();
        }
        let mut out = String::new();
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(lo_line)
            .take(hi_line - lo_line + 1)
        {
            if i > lo_line {
                out.push('\n');
            }
            let s = line_to_string(line);
            let start = if i == lo_line {
                screen_col_to_char_offset(&s, lo_col)
            } else {
                0
            };
            let end = if i == hi_line {
                screen_col_to_char_offset(&s, hi_col)
            } else {
                s.len()
            };
            if start <= end {
                out.push_str(&s[start..end]);
            }
        }
        out
    }
}

fn line_to_string(line: &Line) -> String {
    let mut s = String::new();
    for span in line.spans.iter() {
        s.push_str(&span.content);
    }
    s
}

/// 屏幕相对列→字符串字节 offset。按 `char_display_width` 累加列数，
/// 达到 `target_col` 时返回当前字符的字节起点。越界返回字符串末尾。
fn screen_col_to_char_offset(s: &str, target_col: u16) -> usize {
    let mut col: u16 = 0;
    for (i, ch) in s.char_indices() {
        if col >= target_col {
            return i;
        }
        col = col.saturating_add(char_display_width(ch));
    }
    s.len()
}

fn char_display_width(c: char) -> u16 {
    if c.is_ascii() { 1 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::{Line, Span};

    fn sample_lines() -> Vec<Line<'static>> {
        vec![
            Line::from(vec![Span::raw("alpha")]),
            Line::from(vec![Span::raw("beta"), Span::raw("-gamma")]),
            Line::from(vec![Span::raw("delta")]),
            Line::from(vec![Span::raw("epsilon")]),
        ]
    }

    fn cjk_line() -> Vec<Line<'static>> {
        // "你好world" — 你=2列, 好=2列, w/o/r/l/d 各1列 = 总 9 列
        vec![Line::from(vec![Span::raw("你好world")])]
    }

    #[test]
    fn new_at_is_single_point() {
        let s = Selection::new_at(1, 2);
        let (lo, lc, hi, hc) = s.bounds();
        assert_eq!((lo, lc, hi, hc), (1, 2, 1, 2));
    }

    #[test]
    fn extend_updates_focus_only() {
        let mut s = Selection::new_at(2, 0);
        s.extend(0, 3);
        let (lo, _, hi, _) = s.bounds();
        assert_eq!((lo, hi), (0, 2));
    }

    #[test]
    fn bounds_normalizes_reverse() {
        let mut s = Selection::new_at(3, 5);
        s.extend(1, 2);
        let (lo, lc, hi, hc) = s.bounds();
        assert_eq!((lo, lc, hi, hc), (1, 2, 3, 5));
    }

    #[test]
    fn bounds_same_line_reverse_swaps_cols() {
        let mut s = Selection::new_at(1, 8);
        s.extend(1, 2);
        let (lo, lc, hi, hc) = s.bounds();
        assert_eq!((lo, lc, hi, hc), (1, 2, 1, 8));
    }

    #[test]
    fn screen_to_logical_basic() {
        assert_eq!(Selection::screen_to_logical(4, 2, 5), 7);
    }

    #[test]
    fn screen_to_logical_clamps_underflow() {
        assert_eq!(Selection::screen_to_logical(0, 5, 3), 3);
    }

    #[test]
    fn extract_single_line_full() {
        let l = sample_lines();
        let mut s = Selection::new_at(1, 0);
        s.extend(1, 100);
        assert_eq!(s.extract(&l), "beta-gamma");
    }

    #[test]
    fn extract_single_line_substring() {
        let l = sample_lines();
        let mut s = Selection::new_at(1, 0);
        s.extend(1, 4);
        assert_eq!(s.extract(&l), "beta");
    }

    #[test]
    fn extract_single_line_reverse_substring() {
        let l = sample_lines();
        let mut s = Selection::new_at(1, 4);
        s.extend(1, 2);
        assert_eq!(s.extract(&l), "ta");
    }

    #[test]
    fn extract_multi_line_forward() {
        let l = sample_lines();
        let mut s = Selection::new_at(1, 0);
        s.extend(2, 100);
        assert_eq!(s.extract(&l), "beta-gamma\ndelta");
    }

    #[test]
    fn extract_multi_line_reverse() {
        let l = sample_lines();
        let mut s = Selection::new_at(2, 100);
        s.extend(1, 0);
        assert_eq!(s.extract(&l), "beta-gamma\ndelta");
    }

    #[test]
    fn extract_three_lines_middle_full() {
        let l = sample_lines();
        let mut s = Selection::new_at(0, 0);
        s.extend(2, 100);
        assert_eq!(s.extract(&l), "alpha\nbeta-gamma\ndelta");
    }

    #[test]
    fn extract_clamps_past_end() {
        let l = sample_lines();
        let mut s = Selection::new_at(2, 0);
        s.extend(100, 50);
        assert_eq!(s.extract(&l), "delta\nepsilon");
    }

    #[test]
    fn extract_empty_lines() {
        let l: Vec<Line<'static>> = vec![];
        let s = Selection::new_at(0, 0);
        assert_eq!(s.extract(&l), "");
    }

    #[test]
    fn screen_col_to_char_offset_ascii() {
        assert_eq!(screen_col_to_char_offset("hello", 0), 0);
        assert_eq!(screen_col_to_char_offset("hello", 2), 2);
        assert_eq!(screen_col_to_char_offset("hello", 5), 5);
        assert_eq!(screen_col_to_char_offset("hello", 100), 5);
    }

    #[test]
    fn screen_col_to_char_offset_cjk() {
        // "你好world"：你=col0-1, 好=col2-3, w=col4, o=5, r=6, l=7, d=8
        let s = "你好world";
        assert_eq!(screen_col_to_char_offset(s, 0), 0);
        assert_eq!(screen_col_to_char_offset(s, 2), 3);
        assert_eq!(screen_col_to_char_offset(s, 4), 6);
        assert_eq!(screen_col_to_char_offset(s, 9), 11);
    }

    #[test]
    fn extract_cjk_substring() {
        let l = cjk_line();
        let mut s = Selection::new_at(0, 2);
        s.extend(0, 4);
        assert_eq!(s.extract(&l), "好");
    }

    #[test]
    fn extract_cjk_to_ascii_boundary() {
        let l = cjk_line();
        let mut s = Selection::new_at(0, 0);
        s.extend(0, 4);
        assert_eq!(s.extract(&l), "你好");
    }
}
