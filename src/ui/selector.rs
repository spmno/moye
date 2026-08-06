// 通用选择器组件：供 /models 等命令打开的交互式列表选择（opencode 风格）。
// Generic selector component: interactive list picking for commands like /models
// (opencode-style). 支持输入过滤；过滤无匹配时可将输入作为自定义值（如自定义模型 ID）。
// Supports typed filtering; when the filter matches nothing, the typed input can be
// used as a custom value (e.g. a custom model ID).

/// 选择器中的一个条目。
/// A single item in the selector.
#[derive(Debug, Clone, Default)]
pub struct SelectorItem {
    /// 条目标识（选中的返回值）。
    /// Item identifier (returned when selected).
    pub label: String,
    /// 条目说明（渲染时显示在 label 之后）。
    /// Item detail (rendered after label).
    pub detail: String,
    /// 通用负载：模型选择器的历史项编码 `"provider\nbase_url"`，选中时据此恢复网关；
    /// 其他用途可放任意标识。默认 `None`。
    /// Generic payload: the model selector's history items encode `"provider\nbase_url"`,
    /// used on selection to restore the gateway; other uses may store any id. Defaults to `None`.
    pub data: Option<String>,
}

/// 选择器状态：条目列表 + 过滤关键字 + 光标。
/// Selector state: item list + filter keyword + cursor.
#[derive(Debug)]
pub struct SelectorState {
    title: String,
    items: Vec<SelectorItem>,
    filter: String,
    cursor: usize,
    /// 过滤无匹配时允许把输入作为自定义值返回。
    /// Allow the typed filter to be returned as a custom value when nothing matches.
    allow_custom: bool,
}

impl SelectorState {
    pub fn new(title: String, items: Vec<SelectorItem>, allow_custom: bool) -> Self {
        Self {
            title,
            items,
            filter: String::new(),
            cursor: 0,
            allow_custom,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// 当前过滤关键字。
    /// The current filter keyword.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// 过滤后可见的条目（filter 为空时返回全部；按 label/detail 不区分大小写子串匹配）。
    /// Filtered visible items (all when filter is empty; case-insensitive substring
    /// match on label/detail).
    pub fn visible(&self) -> Vec<&SelectorItem> {
        let f = self.filter.trim();
        if f.is_empty() {
            return self.items.iter().collect();
        }
        let f = f.to_lowercase();
        self.items
            .iter()
            .filter(|it| {
                it.label.to_lowercase().contains(&f)
                    || it.detail.to_lowercase().contains(&f)
            })
            .collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 追加过滤字符并回到列表顶部（列表随过滤变化）。
    /// Append a filter char and reset the cursor to the top (the list changed).
    pub fn input_char(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
    }

    pub fn backspace(&mut self) {
        if !self.filter.is_empty() {
            self.filter.pop();
        }
        self.cursor = 0;
    }

    /// 在可见列表上移动光标（边界夹紧）。
    /// Move the cursor over the visible list (clamped to bounds).
    pub fn move_cursor(&mut self, delta: isize) {
        let count = self.visible().len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        let new = self.cursor as isize + delta;
        self.cursor = if new < 0 {
            0
        } else if new >= count as isize {
            count - 1
        } else {
            new as usize
        };
    }

    /// 当前选中项。可见列表非空时返回光标所指条目；
    /// 否则若允许自定义且过滤非空，返回该过滤值作为自定义条目；均不满足返回 None。
    /// The current selection. Returns the item at the cursor when the visible list is
    /// non-empty; otherwise returns the filter value as a custom item when custom input
    /// is allowed and the filter is non-empty; None when neither applies.
    pub fn selection(&self) -> Option<SelectorItem> {
        let visible = self.visible();
        if !visible.is_empty() {
            let idx = self.cursor.min(visible.len() - 1);
            return Some(visible[idx].clone());
        }
        let f = self.filter.trim();
        if self.allow_custom && !f.is_empty() {
            return Some(SelectorItem {
                label: f.to_string(),
                detail: "自定义 / custom".to_string(),
                ..Default::default()
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<SelectorItem> {
        vec![
            SelectorItem { label: "deepseek-v4-pro".into(), detail: "旗舰".into(), ..Default::default() },
            SelectorItem { label: "deepseek-v4-flash".into(), detail: "快速".into(), ..Default::default() },
            SelectorItem { label: "kimi-k3".into(), detail: "长上下文".into(), ..Default::default() },
        ]
    }

    #[test]
    fn visible_all_when_no_filter() {
        let s = SelectorState::new("t".into(), models(), true);
        assert_eq!(s.visible().len(), 3);
    }

    #[test]
    fn filter_matches_substring_case_insensitive() {
        let mut s = SelectorState::new("t".into(), models(), true);
        s.input_char('K');
        s.input_char('I');
        s.input_char('M');
        s.input_char('I');
        assert_eq!(s.visible().len(), 1);
        assert_eq!(s.visible()[0].label, "kimi-k3");
    }

    #[test]
    fn filter_matches_detail() {
        let mut s = SelectorState::new("t".into(), models(), true);
        for c in "旗舰".chars() {
            s.input_char(c);
        }
        assert_eq!(s.visible().len(), 1);
        assert_eq!(s.visible()[0].label, "deepseek-v4-pro");
    }

    #[test]
    fn move_cursor_clamps() {
        let mut s = SelectorState::new("t".into(), models(), true);
        s.move_cursor(-1);
        assert_eq!(s.cursor(), 0);
        s.move_cursor(5);
        assert_eq!(s.cursor(), 2);
        s.move_cursor(1);
        assert_eq!(s.cursor(), 2);
    }

    #[test]
    fn selection_returns_item_at_cursor() {
        let mut s = SelectorState::new("t".into(), models(), true);
        s.move_cursor(1);
        let sel = s.selection().unwrap();
        assert_eq!(sel.label, "deepseek-v4-flash");
    }

    #[test]
    fn selection_falls_back_to_custom_when_no_match() {
        let mut s = SelectorState::new("t".into(), models(), true);
        for c in "gpt-4o".chars() {
            s.input_char(c);
        }
        assert!(s.visible().is_empty());
        let sel = s.selection().unwrap();
        assert_eq!(sel.label, "gpt-4o");
        assert!(sel.detail.contains("custom"));
    }

    #[test]
    fn selection_none_when_empty_filter_no_items() {
        let s = SelectorState::new("t".into(), vec![], true);
        assert!(s.selection().is_none());
    }

    #[test]
    fn selection_none_when_custom_disallowed_and_no_match() {
        let mut s = SelectorState::new("t".into(), models(), false);
        for c in "gpt-4o".chars() {
            s.input_char(c);
        }
        assert!(s.selection().is_none());
    }

    #[test]
    fn backspace_clears_filter() {
        let mut s = SelectorState::new("t".into(), models(), true);
        s.input_char('k');
        s.input_char('i');
        s.backspace();
        assert_eq!(s.filter(), "k");
        s.backspace();
        assert_eq!(s.filter(), "");
    }
}
