use std::io::stdout;

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};

use crate::config;
use crate::providers::{provider_models_for_plan, ApiPlan, Provider};
use crate::ui::selector::{SelectorItem, SelectorState};

struct ProviderEntry {
    slug: &'static str,
    label: &'static str,
    detail: &'static str,
    api_key_env: &'static str,
}

const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry { slug: "deepseek", label: "DeepSeek", detail: "V4 Pro / Flash · 1M 上下文", api_key_env: "DEEPSEEK_API_KEY" },
    ProviderEntry { slug: "openai", label: "OpenAI", detail: "GPT-5.6 Sol / Terra / Luna", api_key_env: "OPENAI_API_KEY" },
    ProviderEntry { slug: "claude", label: "Anthropic Claude", detail: "Fable 5 / Opus 5 / Sonnet 5", api_key_env: "ANTHROPIC_API_KEY" },
    ProviderEntry { slug: "mimo", label: "Xiaomi MiMo", detail: "MiMo v2.5 Pro · 1M 上下文", api_key_env: "MIMO_API_KEY" },
    ProviderEntry { slug: "gemini", label: "Google Gemini", detail: "Gemini 3.1 Pro / 3.6 Flash", api_key_env: "GEMINI_API_KEY" },
    ProviderEntry { slug: "zhipu", label: "Zhipu GLM", detail: "智谱 GLM-5.2 / GLM-5", api_key_env: "ZAI_API_KEY" },
    ProviderEntry { slug: "bailian", label: "Bailian (DashScope)", detail: "百炼 · Qwen3.8 Max / Qwen3.7 Plus", api_key_env: "DASHSCOPE_API_KEY" },
    ProviderEntry { slug: "moonshot", label: "Moonshot Kimi", detail: "Kimi K3 · 1M 上下文", api_key_env: "MOONSHOT_API_KEY" },
    ProviderEntry { slug: "volcengine", label: "Volcengine Ark", detail: "Doubao Seed Evolving · 周迭代", api_key_env: "ARK_API_KEY" },
    ProviderEntry { slug: "custom", label: "Custom (OpenAI-compatible)", detail: "自定义 base URL + API key", api_key_env: "MY_AGENT_API_KEY" },
];

enum Phase {
    Provider,
    Plan,
    Model,
    CustomUrl,
    CustomModel,
    ApiKey,
    Done,
}

struct SetupState {
    phase: Phase,
    selector: Option<SelectorState>,
    input: String,
    cursor: usize,
    provider: Option<&'static ProviderEntry>,
    plan: ApiPlan,
    model: Option<String>,
    base_url: Option<String>,
    api_key: String,
}

impl SetupState {
    fn new() -> Self {
        let items: Vec<SelectorItem> = PROVIDERS
            .iter()
            .map(|p| SelectorItem {
                label: p.label.to_string(),
                detail: p.detail.to_string(),
                data: Some(p.slug.to_string()),
            })
            .collect();
        Self {
            phase: Phase::Provider,
            selector: Some(SelectorState::new(
                "Select Provider / 选择供应商".into(),
                items,
                false,
            )),
            input: String::new(),
            cursor: 0,
            provider: None,
            plan: ApiPlan::Standard,
            model: None,
            base_url: None,
            api_key: String::new(),
        }
    }

    fn start_plan_select(&mut self) {
        let provider = parse_provider_entry(self.provider.unwrap().slug);
        let plans: Vec<SelectorItem> = provider
            .supported_plans()
            .iter()
            .map(|p| SelectorItem {
                label: p.label().to_string(),
                detail: plan_detail(provider, *p).to_string(),
                data: Some(p.slug().to_string()),
            })
            .collect();
        let only_one = plans.len() == 1;
        self.selector = Some(SelectorState::new(
            "Select Plan / 选择套餐".into(),
            plans,
            false,
        ));
        self.phase = Phase::Plan;
        if only_one {
            self.plan = ApiPlan::Standard;
            self.start_model_select();
        }
    }

    fn start_model_select(&mut self) {
        let provider = parse_provider_entry(self.provider.unwrap().slug);
        let models = provider_models_for_plan(provider, self.plan);
        let items: Vec<SelectorItem> = models
            .iter()
            .map(|m| SelectorItem {
                label: m.slug.clone(),
                detail: m.desc.to_string(),
                data: None,
            })
            .collect();
        self.selector = Some(SelectorState::new(
            format!("Select Model / 选择模型 ({})", self.provider.unwrap().label),
            items,
            true,
        ));
        self.phase = Phase::Model;
    }

    fn start_api_key(&mut self) {
        self.selector = None;
        self.input.clear();
        self.cursor = 0;
        self.phase = Phase::ApiKey;
    }

    fn start_custom_url(&mut self) {
        self.selector = None;
        self.input.clear();
        self.cursor = 0;
        self.phase = Phase::CustomUrl;
    }

    fn input_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let mut idx = self.cursor - 1;
            while !self.input.is_char_boundary(idx) && idx > 0 {
                idx -= 1;
            }
            self.input.replace_range(idx..self.cursor, "");
            self.cursor = idx;
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            let mut idx = self.cursor - 1;
            while !self.input.is_char_boundary(idx) && idx > 0 {
                idx -= 1;
            }
            self.cursor = idx;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.input.len() {
            let mut idx = self.cursor + 1;
            while idx < self.input.len() && !self.input.is_char_boundary(idx) {
                idx += 1;
            }
            self.cursor = idx;
        }
    }

    fn home(&mut self) { self.cursor = 0; }
    fn end(&mut self) { self.cursor = self.input.len(); }

    fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }
}

fn parse_provider_entry(slug: &str) -> Provider {
    crate::providers::parse_provider(slug)
}

fn plan_detail(provider: Provider, plan: ApiPlan) -> &'static str {
    match (provider, plan) {
        (Provider::Volcengine, ApiPlan::Agent) => "api/plan/v3 · 订阅制，含多模态",
        (Provider::Volcengine, ApiPlan::Coding) => "api/coding/v3 · 编程套餐",
        (Provider::Volcengine, ApiPlan::Standard) => "api/v3 · 按量付费",
        (Provider::Bailian, ApiPlan::Coding) => "coding.dashscope.aliyuncs.com/v1",
        (Provider::Bailian, ApiPlan::Standard) => "按量付费",
        (Provider::Moonshot, ApiPlan::Coding) => "api.kimi.com/coding/v1 · Kimi Code 会员",
        (Provider::Moonshot, ApiPlan::Standard) => "api.moonshot.cn/v1 · 按量付费",
        (Provider::Zhipu, ApiPlan::Coding) => "api/coding/paas/v4 · GLM Coding Plan",
        (Provider::Zhipu, ApiPlan::Standard) => "api/paas/v4 · 按量付费",
        _ => "",
    }
}

pub async fn run_setup() -> Result<()> {
    let mut state = SetupState::new();

    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    loop {
        terminal.draw(|f| draw(f, &mut state))?;

        if let Event::Key(key) = event::read()? {
            handle_key(key, &mut state);
        }

        if state.is_done() {
            break;
        }
    }

    disable_raw_mode()?;
    execute!(
        stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;

    write_config(&state)?;
    Ok(())
}

fn handle_key(key: KeyEvent, state: &mut SetupState) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        state.phase = Phase::Done;
        return;
    }

    if state.selector.is_some() {
        handle_selector_key(key, state);
        return;
    }

    match key.code {
        KeyCode::Enter => handle_enter(state),
        KeyCode::Backspace => state.backspace(),
        KeyCode::Left => state.move_left(),
        KeyCode::Right => state.move_right(),
        KeyCode::Home => state.home(),
        KeyCode::End => state.end(),
        KeyCode::Char(c) => state.input_char(c),
        _ => {}
    }
}

fn handle_selector_key(key: KeyEvent, state: &mut SetupState) {
    let sel = state.selector.as_mut().unwrap();
    match key.code {
        KeyCode::Up => sel.move_cursor(-1),
        KeyCode::Down => sel.move_cursor(1),
        KeyCode::Backspace => sel.backspace(),
        KeyCode::Char(c) => sel.input_char(c),
        KeyCode::Enter => {
            let selected = state.selector.as_ref().and_then(|s| s.selection());
            state.selector = None;
            if let Some(item) = selected {
                match state.phase {
                    Phase::Provider => {
                        state.provider = PROVIDERS.iter().find(|p| p.slug == item.data.as_deref().unwrap_or(""));
                        if let Some(p) = state.provider {
                            if p.slug == "custom" {
                                state.start_custom_url();
                            } else {
                                state.start_plan_select();
                            }
                        }
                    }
                    Phase::Plan => {
                        if let Some(plan_slug) = item.data.as_deref() {
                            state.plan = ApiPlan::parse(plan_slug);
                        }
                        state.start_model_select();
                    }
                    Phase::Model => {
                        state.model = Some(item.label.clone());
                        state.start_api_key();
                    }
                    _ => {}
                }
            }
        }
        KeyCode::Esc => {
            if matches!(state.phase, Phase::Model) {
                state.model = None;
                state.start_api_key();
            }
        }
        _ => {}
    }
}

fn handle_enter(state: &mut SetupState) {
    match state.phase {
        Phase::CustomUrl => {
            state.base_url = Some(state.input.clone());
            state.input.clear();
            state.cursor = 0;
            state.phase = Phase::CustomModel;
        }
        Phase::CustomModel => {
            state.model = Some(state.input.clone());
            state.input.clear();
            state.cursor = 0;
            state.start_api_key();
        }
        Phase::ApiKey => {
            state.api_key = state.input.clone();
            state.input.clear();
            state.cursor = 0;
            state.phase = Phase::Done;
        }
        _ => {}
    }
}

fn write_config(state: &SetupState) -> Result<()> {
    let provider = state.provider.unwrap();
    let plan = state.plan;
    let model = state
        .model
        .as_deref()
        .unwrap_or_else(|| config::default_model_for_provider_plan(provider.slug, plan.slug()));
    let base_url = state.base_url.as_deref();
    let api_key_env = if provider.slug == "custom" {
        Some("MY_AGENT_API_KEY")
    } else {
        Some(provider.api_key_env)
    };
    let plan_str = if provider.slug != "custom" && plan != ApiPlan::Standard {
        Some(plan.slug())
    } else {
        None
    };

    let content = config::render_agent_toml(
        provider.slug,
        model,
        base_url,
        api_key_env,
        plan_str,
    );
    std::fs::write("agent.toml", &content)?;

    let mut env_lines = vec![format!("MY_AGENT_PROVIDER={}", provider.slug)];
    if plan != ApiPlan::Standard && provider.slug != "custom" {
        env_lines.push(format!("MY_AGENT_PLAN={}", plan.slug()));
    }
    if provider.slug == "custom" {
        if let Some(ref url) = state.base_url {
            env_lines.push(format!("MY_AGENT_BASE_URL={url}"));
        }
        env_lines.push(format!("MY_AGENT_API_KEY={}", state.api_key));
    } else {
        env_lines.push(format!("{}={}", provider.api_key_env, state.api_key));
    }
    std::fs::write(".env", env_lines.join("\n") + "\n")?;

    eprintln!("[setup] Configuration written: agent.toml + .env");
    eprintln!(
        "[setup] Provider: {} | Plan: {} | Model: {}",
        provider.label,
        plan.label(),
        model
    );
    Ok(())
}

fn draw(f: &mut Frame, state: &mut SetupState) {
    let area = centered(f.area(), 70, 70);
    f.render_widget(Clear, area);

    let (title, lines) = match &state.phase {
        Phase::Provider | Phase::Plan | Phase::Model => {
            if let Some(ref sel) = state.selector {
                draw_selector(f, area, sel);
                return;
            }
            return;
        }
        Phase::CustomUrl => (
            "Custom Base URL",
            vec![
                Line::from(Span::styled(
                    "Enter your OpenAI-compatible API base URL:",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "e.g. https://api.openai.com/v1",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                input_line(&state.input, state.cursor, "base_url> "),
            ],
        ),
        Phase::CustomModel => (
            "Custom Model ID",
            vec![
                Line::from(Span::styled(
                    "Enter the model ID to use:",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "e.g. gpt-4o, claude-sonnet-4-6, glm-5.2",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                input_line(&state.input, state.cursor, "model> "),
            ],
        ),
        Phase::ApiKey => {
            let env_var = state
                .provider
                .map(|p| {
                    if p.slug == "custom" {
                        "MY_AGENT_API_KEY"
                    } else {
                        p.api_key_env
                    }
                })
                .unwrap_or("API_KEY");
            (
                "API Key",
                vec![
                    Line::from(Span::styled(
                        format!("Enter your API key ({env_var}):"),
                        Style::default().fg(Color::Gray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "The key will be stored in .env (git-ignored).",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    input_line(&state.input, state.cursor, "key> "),
                ],
            )
        }
        Phase::Done => {
            return;
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" my-agent Setup · {title} "))
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn draw_selector(f: &mut Frame, area: Rect, sel: &SelectorState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" my-agent Setup · {} ", sel.title()))
        .border_style(Style::default().fg(Color::Cyan));

    let visible = sel.visible();
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in visible.iter().enumerate() {
        let is_cursor = i == sel.cursor();
        let style = if is_cursor {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let detail_style = if is_cursor {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", if is_cursor { ">" } else { " " }), style),
            Span::styled(item.label.clone(), style),
            Span::styled(format!("  · {}", item.detail), detail_style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  {}  | \u{2191}\u{2193} navigate, Enter select", sel.filter()),
        Style::default().fg(Color::DarkGray),
    )));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn input_line<'a>(input: &'a str, cursor: usize, prompt: &'a str) -> Line<'a> {
    let mut spans = vec![Span::styled(prompt, Style::default().fg(Color::Yellow))];
    spans.push(Span::raw(input));
    if cursor == input.len() {
        spans.push(Span::styled("_", Style::default().fg(Color::Gray).add_modifier(Modifier::SLOW_BLINK)));
    }
    Line::from(spans)
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let popup = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area)[1];

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup)[1]
}
