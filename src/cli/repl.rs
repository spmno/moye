//! REPL 命令解析模块：把用户在 TUI 中输入的 `/` 命令或自然语言目标
//! 解析为 [`ReplCommand`] 枚举，供 `AppContext` 分发执行。
//! REPL command parsing module: parses the `/` commands or natural-language goals
//! entered by the user in the TUI into the [`ReplCommand`] enum for `AppContext` to dispatch.

/// REPL 命令枚举。TUI 输入处理复用此 parse 逻辑。
/// REPL command enum. The TUI input handler reuses this parse logic.
pub enum ReplCommand {
    /// `/model [slug]`：查看或切换当前会话模型。
    /// `/model [slug]`: show or switch the current session model.
    Model { slug: Option<String> },
    /// `/models`：打开交互式模型选择器（opencode 风格）。
    /// `/models`: open the interactive model selector (opencode-style).
    Models,
    /// `/plan [standard|coding|agent]`：查看或切换 API 套餐。
    /// `/plan [standard|coding|agent]`: show or switch the API plan.
    Plan { plan: Option<String> },
    /// `/evolve`：触发提示词进化。
    /// `/evolve`: trigger prompt evolution.
    Evolve,
    /// `/evolve-code <file> <old> <new>`：代码自修改（编译验证 + 回退）。
    /// `/evolve-code <file> <old> <new>`: code self-modification (compile-verified + rollback).
    EvolveCode {
        file: String,
        old: String,
        new: String,
    },
    /// `/add-tool <name> <desc>`：生成新工具脚手架（需重新编译生效）。
    /// `/add-tool <name> <desc>`: scaffold a new tool (requires recompile to take effect).
    AddTool { name: String, description: String },
    /// `/add-skill <name> <desc>`：添加运行时技能（无需重编译）。
    /// `/add-skill <name> <desc>`: add a runtime skill (no recompile needed).
    AddSkill { name: String, description: String },
    /// `/skills`：列出已注册技能。
    /// `/skills`: list registered skills.
    Skills,
    /// `/context`：查看当前上下文（模型、token 用量、消息历史等）。
    /// `/context`: show the current context (model, token usage, message history, etc.).
    Context,
    /// `/help`：显示帮助信息。
    /// `/help`: show help text.
    Help,
    /// `/history [n]`：查看最近 n 轮对话记录（默认 10）。
    /// `/history [n]`: show the last n turns of conversation (default 10).
    History { limit: Option<usize> },
    /// `/lessons`：查看已积累的经验教训。
    /// `/lessons`: show accumulated lessons.
    Lessons,
    /// `/quit`：退出程序。
    /// `/quit`: quit the program.
    Quit,
    /// `/trust`：切换沙箱信任模式（开启后沙箱外访问自动授权，不再弹窗确认）。
    /// `/trust`: toggle sandbox trust mode (when enabled, out-of-sandbox access is
    /// auto-authorized without prompting the user).
    Trust,
    /// 非 `/` 开头的自然语言输入：作为任务目标交给 Orchestrator 执行。
    /// Natural-language input not starting with `/`: handed to the Orchestrator as a task goal.
    Goal(String),
    /// 解析失败（空输入 / 引号不配对 / 未知命令 / 参数缺失）。
    /// Parse failure (empty input / unbalanced quotes / unknown command / missing args).
    InvalidUsage(&'static str),
}

impl ReplCommand {
    /// 把一行原始输入解析为 `ReplCommand`。
    /// 空输入返回 `InvalidUsage("empty input")`；
    /// 不以 `/` 开头的输入视为自然语言目标 `Goal`。
    /// Parse a single raw input line into a `ReplCommand`.
    /// Empty input returns `InvalidUsage("empty input")`;
    /// input not starting with `/` is treated as a natural-language `Goal`.
    pub fn parse(line: &str) -> Self {
        let line = line.trim();
        if line.is_empty() {
            return Self::InvalidUsage("empty input");
        }

        match line {
            "quit" | "exit" | "q" => return Self::Quit,
            _ => {}
        }

        if !line.starts_with('/') {
            return Self::Goal(line.to_owned());
        }

        let tokens = match shlex::split(line) {
            Some(t) => t,
            None => return Self::InvalidUsage("unbalanced quotes"),
        };
        let cmd = tokens.first().map(|s| s.as_str()).unwrap_or("");
        let rest = &tokens[1..];

        match cmd {
            "/model" | "/m" => Self::Model {
                slug: rest.first().map(|s| s.as_str()).map(str::to_owned),
            },
            "/models" => Self::Models,
            "/plan" => Self::Plan {
                plan: rest.first().map(|s| s.as_str()).map(str::to_owned),
            },
            "/evolve" | "/e" => Self::Evolve,
            "/evolve-code" | "/ec" => match rest {
                [file, old, new, ..] if !file.is_empty() => Self::EvolveCode {
                    file: file.clone(),
                    old: old.clone(),
                    new: new.clone(),
                },
                _ => Self::InvalidUsage("usage: /evolve-code <file> <old> <new>"),
            },
            "/add-tool" => match rest {
                [name, desc, ..] => Self::AddTool {
                    name: name.clone(),
                    description: desc.clone(),
                },
                _ => Self::InvalidUsage("usage: /add-tool <name> <description>"),
            },
            "/add-skill" => match rest {
                [name, desc, ..] => Self::AddSkill {
                    name: name.clone(),
                    description: desc.clone(),
                },
                _ => Self::InvalidUsage("usage: /add-skill <name> <description>"),
            },
            "/skills" => Self::Skills,
            "/context" | "/ctx" => Self::Context,
            "/help" | "/h" | "/?" => Self::Help,
            "/history" | "/hist" => Self::History {
                limit: rest.first().and_then(|s| s.parse().ok()),
            },
            "/lessons" => Self::Lessons,
            "/quit" | "/q" | "/exit" => Self::Quit,
            "/trust" => Self::Trust,
            _ => Self::InvalidUsage("unknown command; type /help for usage"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_command_parses() {
        assert!(matches!(ReplCommand::parse("/models"), ReplCommand::Models));
    }

    #[test]
    fn model_slug_still_works() {
        assert!(matches!(
            ReplCommand::parse("/model kimi-k3"),
            ReplCommand::Model { slug: Some(s) } if s == "kimi-k3"
        ));
    }

    #[test]
    fn non_slash_is_goal() {
        assert!(matches!(
            ReplCommand::parse("帮我修复 bug"),
            ReplCommand::Goal(_)
        ));
    }
}
