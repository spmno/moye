//! unified diff 渲染器：把 `edit_file` 的 old/new 文本渲染为带颜色的
//! ratatui `Line` 列表，用于 TUI 消息流展示。
//!
//! - 删除行红色（`theme::tool_result_err()`）、插入行绿色（`theme::tool_result_ok()`）、
//!   上下文行暗灰（`theme::info()`）。
//! - 不变行段折叠：每个变化前后最多保留 2 行上下文，多余折叠为单行标记。
//! - 总行数上限 60，超出截断并追加标记行。
//! - 内容行行级样式为 `theme::code_block()`（`wrap.rs:is_code_line` 契约），
//!   per-span 颜色叠加其上。
//!
//! Unified diff renderer: turns the old/new text of an `edit_file` call into a
//! colored list of ratatui `Line`s for the TUI message stream.
//!
//! - Deletes are red (`theme::tool_result_err()`), inserts green (`theme::tool_result_ok()`),
//!   context lines dark-gray (`theme::info()`).
//! - Unchanged runs are collapsed: at most 2 context lines around each change;
//!   longer middle runs collapse to a single marker line.
//! - Total emitted lines capped at 60; overflow is truncated with a marker line.
//! - Content lines carry `theme::code_block()` as their line-level style (the
//!   `wrap.rs:is_code_line` contract); per-span colors patch over it.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::event::FileEdit;
use crate::ui::theme;

/// 输出总行数上限（含标记行）。超出时截断并追加 dim 截断标记行。
/// Total output line cap (including markers). When exceeded, a dim
/// truncation marker is appended.
const MAX_LINES: usize = 60;

/// 每个变化前后保留的上下文行数（类似 `diff -U2`）。
/// Context lines kept around each change (like `diff -U2`).
const CONTEXT_RADIUS: usize = 2;

/// 把一个 edit_file 载荷渲染为 unified diff 行列表。
///
/// 纯函数，无副作用，可独立测试。渲染规则见模块级文档。
/// Render an edit_file payload into a unified-diff list of `Line`s.
///
/// Pure function, no side effects, independently testable. See module docs.
pub fn unified_diff_lines(edit: &FileEdit, expand: bool) -> Vec<Line<'static>> {
    // 空 diff（old == new）：输出单行标记，绝不返回空 Vec、绝不 panic。
    // Empty diff (old == new): emit a single marker line, never an empty Vec, never panic.
    if edit.old == edit.new {
        return vec![marker_line("  \u{ff08}\u{65e0}\u{53d8}\u{5316} / no changes\u{ff09}")];
    }

    // 展开时取消行数上限（CONTEXT_RADIUS 折叠在两种模式下都生效）。
    // When expanded, bypass the line cap (CONTEXT_RADIUS folding still applies in both modes).
    let cap = if expand { usize::MAX } else { MAX_LINES };

    let diff = TextDiff::from_lines(&edit.old, &edit.new);
    let raw: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.value().trim_end_matches('\n').to_string()))
        .collect();

    if raw.is_empty() {
        return vec![marker_line("  \u{ff08}\u{65e0}\u{53d8}\u{5316} / no changes\u{ff09}")];
    }

    // 计算 keep 掩码：Delete/Insert 恒保留；Equal 当其前后 CONTEXT_RADIUS
    // 范围内有非 Equal 行时保留，否则折叠。
    // Compute a keep mask: Delete/Insert are always kept; an Equal line is kept
    // when a non-Equal line exists within CONTEXT_RADIUS before or after it.
    let keep = keep_mask(&raw);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if keep[i] {
            let (tag, val) = &raw[i];
            let line = match tag {
                ChangeTag::Delete => content_line("-", val, theme::tool_result_err()),
                ChangeTag::Insert => content_line("+", val, theme::tool_result_ok()),
                ChangeTag::Equal => content_line(" ", val, theme::info()),
            };
            out.push(line);
            if out.len() >= cap {
                let remaining = estimate_remaining(&raw, &keep, i + 1);
                if remaining > 0 {
                    out.push(marker_line(&format!(
                        "  \u{2026} (diff \u{8fc7}\u{957f}\u{5df2}\u{622a}\u{65ad} / truncated, {remaining} more lines)"
                    )));
                }
                return out;
            }
            i += 1;
        } else {
            // 连续的被折叠行数（全 Equal 且 keep=false）。
            // Count the consecutive folded lines (all Equal with keep=false).
            let mut j = i;
            while j < raw.len() && !keep[j] {
                j += 1;
            }
            let skipped = j - i;
            // 仅当被折叠段夹在两个保留区之间时才插入标记行
            // （开头的未变段与结尾的未变段静默裁剪，不插标记）。
            // Only insert a collapse marker when the folded run sits between
            // two kept regions (leading/trailing context is trimmed silently).
            if i > 0 && j < raw.len() {
                out.push(marker_line(&format!(
                    "  \u{22ef} ({skipped} \u{884c}\u{672a}\u{53d8} / unchanged)"
                )));
                if out.len() >= cap {
                    let remaining = estimate_remaining(&raw, &keep, j);
                    if remaining > 0 {
                        out.push(marker_line(&format!(
                            "  \u{2026} (diff \u{8fc7}\u{957f}\u{5df2}\u{622a}\u{65ad} / truncated, {remaining} more lines)"
                        )));
                    }
                    return out;
                }
            }
            i = j;
        }
    }

    // 兜底：若输出恰好为空（理论上不会发生），返回无变化标记。
    // Fallback: if output is somehow empty, return the no-changes marker.
    if out.is_empty() {
        out.push(marker_line("  \u{ff08}\u{65e0}\u{53d8}\u{5316} / no changes\u{ff09}"));
    }
    out
}

/// 计算 keep 掩码：Delete/Insert → true；Equal → 当其前后 CONTEXT_RADIUS 行
/// 内存在非 Equal 行时为 true，否则 false。
///
/// Compute the keep mask: Delete/Insert → true; Equal → true only if a
/// non-Equal line exists within CONTEXT_RADIUS before or after it.
fn keep_mask(raw: &[(ChangeTag, String)]) -> Vec<bool> {
    let n = raw.len();
    let mut mask = vec![false; n];
    for i in 0..n {
        if raw[i].0 != ChangeTag::Equal {
            mask[i] = true;
            continue;
        }
        let before = i.saturating_sub(CONTEXT_RADIUS)..i;
        let after = (i + 1)..(i + 1 + CONTEXT_RADIUS).min(n);
        mask[i] = before.chain(after).any(|j| raw[j].0 != ChangeTag::Equal);
    }
    mask
}

/// 估算从 idx 起还会输出多少行（折叠后）：被折叠段不再产出单行
/// （仅产出一个标记），保留段每行一个。粗略计数即可。
///
/// Estimate how many more lines would be emitted from `idx` onward (after
/// folding): folded runs produce at most 1 marker, kept lines produce 1 each.
/// A rough count suffices for the truncation marker.
fn estimate_remaining(raw: &[(ChangeTag, String)], keep: &[bool], idx: usize) -> usize {
    let mut count = 0;
    let mut i = idx;
    while i < raw.len() {
        if keep[i] {
            count += 1;
            i += 1;
        } else {
            while i < raw.len() && !keep[i] {
                i += 1;
            }
            if i < raw.len() {
                count += 1; // 折叠标记行 / collapse marker line
            }
        }
    }
    count
}

/// 构建内容行：前缀 + 内容，行级样式 `theme::code_block()`，span 颜色为给定 fg。
/// Build a content line: prefix + value, line-level style `theme::code_block()`,
/// span fg from the given style.
fn content_line(prefix: &str, value: &str, fg_style: Style) -> Line<'static> {
    Line {
        spans: vec![
            Span::raw(format!("{prefix} ")),
            Span::styled(value.to_string(), fg_style),
        ],
        style: theme::code_block(),
        alignment: None,
    }
}

/// 构建标记/截断行：default 行级样式（非 code_block），dim。
/// Build a marker/truncation line: default line-level style (not code_block), dim.
fn marker_line(text: &str) -> Line<'static> {
    Line::styled(text.to_string(), theme::info())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FileEdit;
    use ratatui::style::Color;

    /// 拼接一行的全部 span 内容为字符串。
    /// Concatenate all span content of one line into a string.
    fn joined(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect()
    }

    /// 取一行的有效前景色（首个有 fg 的 span，否则行级 style 的 fg）。
    /// Get the effective fg color of a line (first styled span fg, else line style fg).
    fn line_fg(line: &Line<'_>) -> Option<Color> {
        for s in &line.spans {
            if s.style.fg.is_some() {
                return s.style.fg;
            }
        }
        line.style.fg
    }

    fn make_edit(old: &str, new: &str) -> FileEdit {
        FileEdit {
            path: "test.rs".to_string(),
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    #[test]
    fn delete_line_has_minus_prefix_and_red_fg() {
        let edit = make_edit("line one\n", "line two\n");
        let lines = unified_diff_lines(&edit, false);
        let deletes: Vec<&Line> = lines
            .iter()
            .filter(|l| joined(l).starts_with("- "))
            .collect();
        assert!(!deletes.is_empty(), "expected at least one delete line");
        for d in &deletes {
            assert_eq!(
                line_fg(d),
                theme::tool_result_err().fg,
                "delete line must be LightRed"
            );
        }
    }

    #[test]
    fn insert_line_has_plus_prefix_and_green_fg() {
        let edit = make_edit("line one\n", "line two\n");
        let lines = unified_diff_lines(&edit, false);
        let inserts: Vec<&Line> = lines
            .iter()
            .filter(|l| joined(l).starts_with("+ "))
            .collect();
        assert!(!inserts.is_empty(), "expected at least one insert line");
        for ins in &inserts {
            assert_eq!(
                line_fg(ins),
                theme::tool_result_ok().fg,
                "insert line must be Green"
            );
        }
    }

    #[test]
    fn context_line_has_two_space_prefix_and_dim_fg() {
        let edit = make_edit("context\nold\ncontext\n", "context\nnew\ncontext\n");
        let lines = unified_diff_lines(&edit, false);
        let contexts: Vec<&Line> = lines
            .iter()
            .filter(|l| joined(l).starts_with("  "))
            .collect();
        assert!(!contexts.is_empty(), "expected context lines");
        for c in &contexts {
            assert_eq!(
                line_fg(c),
                theme::info().fg,
                "context line must be DarkGray"
            );
        }
    }

    #[test]
    fn long_unchanged_run_collapses_to_marker() {
        // 20 行不变段夹在两个变化之间，折叠后总行数远小于未折叠。
        // 20-line unchanged run between two changes collapses; output much smaller.
        let mut old = String::from("change_top\n");
        for i in 0..20 {
            old.push_str(&format!("unchanged_{i}\n"));
        }
        old.push_str("change_bottom\n");
        let mut new = String::from("CHANGED_top\n");
        for i in 0..20 {
            new.push_str(&format!("unchanged_{i}\n"));
        }
        new.push_str("CHANGED_bottom\n");
        let edit = make_edit(&old, &new);
        let lines = unified_diff_lines(&edit, false);
        // 未折叠会有 ~2 删除 + 2 插入 + 20 上下文 = 24+ 行。
        // 折叠后上下文最多 2+2=4 行，加 2 删 + 2 插 + 1 标记 = ~9 行，远小于 24。
        assert!(
            lines.len() < 24,
            "expected collapsed output to be much smaller than 24, got {}",
            lines.len()
        );
        // 必须存在折叠标记行。
        // Must contain a collapse marker line.
        let has_marker = lines.iter().any(|l| {
            let j = joined(l);
            j.contains("unchanged") || j.contains("\u{672a}\u{53d8}")
        });
        assert!(has_marker, "expected a collapse marker line in the output");
    }

    #[test]
    fn diff_over_60_lines_hits_cap_with_truncation_marker() {
        // 80 行删除，远超 60 行上限，最后一行应为截断标记。
        // 80 deletions, exceeds the 60-line cap; last line must be the truncation marker.
        let mut old = String::new();
        for i in 0..80 {
            old.push_str(&format!("line_{i}\n"));
        }
        let edit = make_edit(&old, "");
        let lines = unified_diff_lines(&edit, false);
        assert!(
            lines.len() <= 61,
            "expected at most 60 content lines + 1 truncation marker = 61, got {}",
            lines.len()
        );
        let last = joined(lines.last().unwrap());
        assert!(
            last.contains("truncated"),
            "last line must be truncation marker, got: {last}"
        );
    }

    #[test]
    fn diff_expanded_renders_all_lines_no_truncation_but_collapse_present() {
        // 混合：变化 + 不变段 + 大段删除，展开后 >60 行、无截断标记、折叠标记仍在。
        // Mixed: changes + unchanged run + large delete block; expanded → >60 lines,
        // no truncation marker, collapse marker still present (CONTEXT_RADIUS applies
        // in both modes).
        let mut old = String::from("top\n");
        for i in 0..20 {
            old.push_str(&format!("same_{i}\n"));
        }
        old.push_str("mid\n");
        for i in 0..60 {
            old.push_str(&format!("del_{i}\n"));
        }
        let mut new = String::from("TOP\n");
        for i in 0..20 {
            new.push_str(&format!("same_{i}\n"));
        }
        new.push_str("MID\n");
        let edit = make_edit(&old, &new);
        let lines = unified_diff_lines(&edit, true);
        assert!(
            lines.len() > 60,
            "expanded must render >60 lines, got {}",
            lines.len()
        );
        let all: String = lines.iter().map(|l| joined(l)).collect::<Vec<_>>().join("");
        assert!(
            !all.contains("truncated"),
            "no truncation marker in expanded mode, got: {all}"
        );
        assert!(
            all.contains("\u{672a}\u{53d8}") || all.contains("unchanged"),
            "collapse marker must still be present in expanded mode, got: {all}"
        );
    }

    #[test]
    fn old_eq_new_emits_no_changes_line() {
        let edit = make_edit("same\ncontent\n", "same\ncontent\n");
        let lines = unified_diff_lines(&edit, false);
        assert_eq!(lines.len(), 1, "expected exactly one line for no-changes");
        let text = joined(&lines[0]);
        assert!(
            text.contains("no changes") || text.contains("\u{65e0}\u{53d8}\u{5316}"),
            "expected no-changes marker, got: {text}"
        );
    }

    #[test]
    fn empty_old_is_pure_insertion() {
        // 空 old = 纯插入，所有行应为 + 前缀绿色。
        // Empty old = pure insertion; all lines must be + prefix green.
        let edit = make_edit("", "new content\nsecond line\n");
        let lines = unified_diff_lines(&edit, false);
        assert!(!lines.is_empty(), "expected non-empty output for pure insertion");
        let inserts: Vec<&Line> = lines
            .iter()
            .filter(|l| joined(l).starts_with("+ "))
            .collect();
        assert!(
            !inserts.is_empty(),
            "expected at least one insert line for pure insertion"
        );
    }

    #[test]
    fn empty_new_is_pure_deletion() {
        // 空 new = 纯删除，所有行应为 - 前缀红色。
        // Empty new = pure deletion; all lines must be - prefix red.
        let edit = make_edit("old content\nsecond line\n", "");
        let lines = unified_diff_lines(&edit, false);
        assert!(!lines.is_empty(), "expected non-empty output for pure deletion");
        let deletes: Vec<&Line> = lines
            .iter()
            .filter(|l| joined(l).starts_with("- "))
            .collect();
        assert!(
            !deletes.is_empty(),
            "expected at least one delete line for pure deletion"
        );
    }

    #[test]
    fn cjk_content_appears_verbatim() {
        let edit = make_edit("你好世界\n", "你好 Rust\n");
        let lines = unified_diff_lines(&edit, false);
        let all_text: String = lines
            .iter()
            .map(|l| joined(l))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            all_text.contains("你好"),
            "CJK content must appear verbatim, got: {all_text}"
        );
    }

    #[test]
    fn content_lines_have_code_block_line_style() {
        let edit = make_edit("a\n", "b\n");
        let lines = unified_diff_lines(&edit, false);
        let content_lines: Vec<&Line> = lines
            .iter()
            .filter(|l| {
                let j = joined(l);
                j.starts_with("- ") || j.starts_with("+ ") || j.starts_with("  ")
            })
            .collect();
        assert!(!content_lines.is_empty(), "expected content lines");
        for cl in &content_lines {
            assert_eq!(
                cl.style,
                theme::code_block(),
                "content line must have code_block line-level style"
            );
        }
    }
}
