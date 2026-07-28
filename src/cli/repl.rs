use rustyline::DefaultEditor;
use tracing::info;

use crate::cli::context::AppContext;

/// REPL 命令——显式枚举，编译器强制 match 穷尽。
enum ReplCommand {
    Model { slug: Option<String> },
    Evolve,
    EvolveCode { file: String, old: String, new: String },
    AddTool { name: String, description: String },
    AddSkill { name: String, description: String },
    Skills,
    Help,
    History { limit: Option<usize> },
    Lessons,
    Quit,
    Goal(String),
    InvalidUsage(&'static str),
}

impl ReplCommand {
    fn parse(line: &str) -> Self {
        let line = line.trim();
        if line.is_empty() {
            return Self::InvalidUsage("empty input");
        }

        match line {
            "quit" | "exit" | "q" => return Self::Quit,
            _ => {}
        }

        // `/` 前缀 = 命令；否则 = goal。消除 evolve/evolve-code 顺序依赖。
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
            "/help" | "/h" | "/?" => Self::Help,
            "/history" | "/hist" => Self::History {
                limit: rest.first().and_then(|s| s.parse().ok()),
            },
            "/lessons" => Self::Lessons,
            "/quit" | "/q" | "/exit" => Self::Quit,
            _ => Self::InvalidUsage("unknown command; type /help for usage"),
        }
    }
}

pub async fn run_repl(ctx: &AppContext) -> anyhow::Result<()> {
    let mut rl = DefaultEditor::new()?;
    loop {
        let line = match rl.readline("» ") {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let _ = rl.add_history_entry(line.as_str());

        match ReplCommand::parse(&line) {
            ReplCommand::Quit => break,
            ReplCommand::Goal(goal) => ctx.run_goal(&goal).await,
            ReplCommand::Model { slug } => ctx.cmd_model(slug),
            ReplCommand::Evolve => ctx.cmd_evolve().await,
            ReplCommand::EvolveCode { file, old, new } => ctx.cmd_evolve_code(&file, &old, &new),
            ReplCommand::AddTool { name, description } => ctx.cmd_add_tool(&name, &description),
            ReplCommand::AddSkill { name, description } => ctx.cmd_add_skill(&name, &description),
            ReplCommand::Skills => ctx.cmd_list_skills(),
            ReplCommand::Help => ctx.cmd_help(),
            ReplCommand::History { limit } => ctx.cmd_history(limit),
            ReplCommand::Lessons => ctx.cmd_list_lessons(),
            ReplCommand::InvalidUsage(msg) => info!("{msg}"),
        }
    }
    Ok(())
}
