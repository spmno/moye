/// REPL 命令枚举。TUI 输入处理复用此 parse 逻辑。
pub enum ReplCommand {
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
