# my-agent —— 基于 rig 框架的自我进化 AI Agent / Self-Evolving AI Agent Built on rig

基于 [rig](https://github.com/0xPlaygrounds/rig) (Rust LLM 框架) 构建的自主循环 AI Agent，支持多供应商、多模型、人在环（HITL）门控、技能系统与自我进化。

A self-evolving autonomous-loop AI Agent built on [rig](https://github.com/0xPlaygrounds/rig) (Rust LLM framework). Features multi-provider, multi-model support, Human-in-the-Loop (HITL) gating, a skill system, and self-evolution.

---

## 架构概览 / Architecture Overview

```
my-agent
├── src/
│   ├── main.rs          # 程序入口（日志初始化 + TUI 启动）
│   │                    # Entry point (logging init + TUI launch)
│   ├── event.rs         # 共享事件类型（AgentEvent + EventSender）
│   │                    # Shared event types (AgentEvent + EventSender)
│   ├── agent_loop.rs    # 自主循环 + HITL 门控（AgentHook）
│   │                    # Autonomous loop + HITL gating (AgentHook)
│   ├── providers.rs     # 多供应商统一接入（OpenAI 兼容）
│   │                    # Multi-provider access (OpenAI-compatible)
│   ├── registry.rs      # Agent 角色注册 + 工具权限分级
│   │                    # Agent role registry + tool permission tiers
│   ├── tools.rs         # 内置工具（read_file / edit_file / run_bash / write_file）
│   │                    # Built-in tools (read_file / edit_file / run_bash / write_file)
│   ├── skills.rs        # 技能系统（运行时加载，无需重编译）
│   │                    # Skill system (runtime-loaded, no recompilation)
│   ├── reviewer.rs      # 审计门控（两阶段评审）
│   │                    # Review gate (two-phase review)
│   ├── memory.rs        # JSONL 对话记忆
│   │                    # JSONL conversation memory
│   ├── ui/              # TUI 表示层（ratatui + crossterm）
│   │                    # TUI presentation layer (ratatui + crossterm)
│   │   ├── tui.rs       #   事件循环 + 渲染 + 输入处理
│   │   ├── markdown.rs  #   Markdown → ratatui Text 渲染器
│   │   ├── theme.rs     #   颜色与样式
│   │   └── mod.rs       #   模块根
│   ├── cli/             # 命令行上下文
│   │   ├── context.rs   #   AppContext + 命令分发
│   │   └── repl.rs      #   命令解析
│   └── evolution/       # 自我进化（提示词进化 + 代码进化 + 工具扩展）
│       └── ...          # Self-evolution (prompt + code + tool extension)
├── agent.toml           # 运行时配置（供应商 / 模型 / 权限 / 记忆）
├── AGENTS.md            # 系统提示词（主 agent）
├── prompts/             # 各角色提示词（planner / builder / auditor）
├── skills/              # 技能清单与指令（运行时加载）
├── memory/              # 对话与经验记录
└── logs/                # 按时间命名的日志文件
```

### 依赖方向 / Dependency Direction

领域层（`agent_loop`、`registry`、`reviewer`、`prompt_evolve`）只依赖 `event` 模块，不依赖 `ui`。表示层（`ui::tui`）依赖 `event` 和自身的子模块。这保证了依赖倒置：未来加 CLI / API 消费者时，无需修改任何领域层代码。

The domain layer (`agent_loop`, `registry`, `reviewer`, `prompt_evolve`) depends only on the `event` module, never on `ui`. The presentation layer (`ui::tui`) depends on `event` and its own submodules. This ensures dependency inversion: adding a CLI/API consumer requires no domain-layer changes.

### 核心概念 / Core Concepts

| 概念 / Concept | 说明 / Description |
|------|------|
| **自主循环 / Autonomous Loop** | 用户输入目标 → Builder 自行规划并调用工具 → 达到 max_turns 或任务完成时停止。User inputs a goal → Builder plans and calls tools autonomously → stops at max_turns or task completion. |
| **HITL（人在环）/ Human-in-the-Loop** | 工具调用按 `allow / ask / deny` 三级权限门控，危险操作暂停等待人工确认。Tool calls gated by `allow / ask / deny` tiers; dangerous operations pause for human confirmation. |
| **多角色 / Multi-Role** | Orchestrator（编排）/ Planner（规划）/ Builder（构建）/ Auditor（审计）。Orchestrator / Planner / Builder / Auditor. |
| **自我进化 / Self-Evolution** | 提示词进化 + 代码进化 + 工具扩展，通过经验积累提升能力。Prompt evolution + code evolution + tool extension, improving through accumulated experience. |
| **技能系统 / Skill System** | 运行时加载 Markdown 技能文件，注入提示词，无需重编译。Runtime-loaded Markdown skill files injected into prompts, no recompilation needed. |
| **事件驱动 / Event-Driven** | Agent 输出通过 `AgentEvent` channel 传递给 TUI，解耦领域层与表示层。Agent output flows to TUI via `AgentEvent` channel, decoupling domain from presentation. |

---

## 环境准备 / Prerequisites

### 1. 安装 Rust 工具链 / Install Rust Toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

安装完成后重启终端，验证 / Restart terminal and verify:

```bash
rustc --version
cargo --version
```

### 2. 获取项目代码 / Clone the Repository

```bash
git clone https://github.com/spmno/my-agent.git
cd my-agent
```

---

## 供应商配置 / Provider Configuration

本项目通过环境变量 `MY_AGENT_PROVIDER` 选择供应商，API Key 从对应环境变量读取。所有供应商均通过 OpenAI 兼容接口接入。

Set `MY_AGENT_PROVIDER` to select a provider. API keys are read from provider-specific environment variables. All providers use the OpenAI-compatible interface.

### DeepSeek（默认 / Default）

无需设置 `MY_AGENT_PROVIDER`，默认即为 DeepSeek。/ No need to set `MY_AGENT_PROVIDER`; DeepSeek is the default.

```bash
export DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 / Item | 值 / Value |
|------|-----|
| 环境变量 / Env Var | `DEEPSEEK_API_KEY` |
| Base URL | `https://api.deepseek.com/v1` |
| 模型标识 / Model ID | `deepseek-v4-pro` / `deepseek-v4-flash` |

API Key 获取 / Get API Key: https://platform.deepseek.com/api_keys

---

### 百炼（阿里云 DashScope）/ Bailian (Alibaba Cloud DashScope)

```bash
export MY_AGENT_PROVIDER=bailian
export DASHSCOPE_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 / Item | 值 / Value |
|------|-----|
| 环境变量 / Env Var | `DASHSCOPE_API_KEY` |
| Base URL | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| 模型标识 / Model ID | `kimi/kimi-k3` / `qwen-plus` / `qwen-max` |

> **注意**：百炼平台的模型标识格式为 `组织名/模型名`（如 `kimi/kimi-k3`），与其它供应商不同。
> **Note**: Bailian uses `org/model` format (e.g., `kimi/kimi-k3`), unlike other providers.

API Key 获取 / Get API Key: https://dashscope.console.aliyun.com/apiKey

---

### Moonshot（Kimi 平台）/ Moonshot (Kimi Platform)

```bash
export MY_AGENT_PROVIDER=moonshot
export MOONSHOT_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 / Item | 值 / Value |
|------|-----|
| 环境变量 / Env Var | `MOONSHOT_API_KEY` |
| Base URL | `https://api.moonshot.cn/v1` |
| 模型标识 / Model ID | `kimi-k3` / `kimi-k2.7-code-highspeed` |

> **注意**：Kimi K3 仅接受 `temperature=1.0`，代码会自动限制，无需手动设置。
> **Note**: Kimi K3 only accepts `temperature=1.0`; the code clamps it automatically.

API Key 获取 / Get API Key: https://platform.kimi.com/api-keys

---

### 快速切换供应商 / Quick Provider Switching

通过 TUI 命令可在运行时切换模型（无需重启）/ Switch models at runtime via TUI commands (no restart needed):

```bash
# 启动时指定供应商 / Specify provider at startup
export MY_AGENT_PROVIDER=moonshot
export MOONSHOT_API_KEY=sk-xxx
cargo run

# 运行时切换模型（同供应商内）/ Switch model at runtime (within same provider)
> model kimi-k2.7-code-highspeed

# 查看当前模型 / Show current model
> model
```

---

## 使用方法 / Usage

### 启动 / Launch

```bash
cargo run
```

启动后进入全屏 TUI 界面 / Launches a fullscreen TUI interface:

```
 my-agent (DeepSeek) | model: kimi-k3 
─────────────────────────────────────────
Enter 发送任务 | /help 帮助 | Ctrl+C 退出
─────────────────────────────────────────
» _
```

### 输入自然语言目标 / Input a Natural-Language Goal

直接输入你想让 Agent 完成的任务，它会自行规划并调用工具执行。流式输出实时显示。

Type a task for the Agent to complete. It will plan and execute autonomously. Streaming output is shown in real-time.

### TUI 命令 / TUI Commands

| 命令 / Command | 说明 / Description | 示例 / Example |
|------|------|------|
| `model <slug>` | 切换当前会话的模型 / Switch session model | `model kimi-k3` |
| `model` | 查看当前模型 / Show current model | `model` |
| `add-skill <name> <desc>` | 添加新技能（运行时加载）/ Add a skill (runtime) | `add-skill "rust-expert" "Rust code review"` |
| `skills` | 列出所有已注册技能 / List registered skills | `skills` |
| `history [n]` | 查看最近 n 轮对话记录（默认 10）/ Show last n turns | `history 5` |
| `lessons` | 查看已积累的经验教训 / Show accumulated lessons | `lessons` |
| `help` | 显示所有可用命令 / Show all commands | `help` |
| `evolve` | 触发提示词进化 / Trigger prompt evolution | `evolve` |
| `evolve-code <file> <old> <new>` | 代码进化（替换代码片段）/ Code evolution (replace snippet) | `evolve-code src/main.rs "old" "new"` |
| `add-tool <name> <desc>` | 扩展工具（运行时注册）/ Extend tools (runtime) | `add-tool "git-commit" "Git commit tool"` |
| `quit` | 退出程序 / Quit | `quit` |

### TUI 快捷键 / TUI Keybindings

| 按键 / Key | 动作 / Action |
|------|------|
| `Enter` | 发送输入 / Submit input |
| `Ctrl+C` / `Ctrl+D` | 退出 / Quit |
| `Up` / `Down` | 浏览输入历史 / Browse input history |
| `PageUp` / `PageDown` | 滚动消息 / Scroll messages |
| `Home` / `End` | 光标移至行首/行尾 / Cursor to start/end of line |
| `y` / `n`（HITL 模式）/ (HITL mode) | 允许/拒绝工具执行 / Allow/deny tool execution |

---

## 配置文件（agent.toml）/ Configuration (agent.toml)

```toml
[provider]
# 由 MY_AGENT_PROVIDER 环境变量控制，无需手动配置
# Controlled by MY_AGENT_PROVIDER env var; no manual config needed

[agent]
# 默认模型（需与供应商匹配）/ Default model (must match provider)
# DeepSeek: deepseek-v4-pro / deepseek-v4-flash
# Bailian:  kimi/kimi-k3 / qwen-plus / qwen-max
# Moonshot: kimi-k3 / kimi-k2.7-code-highspeed
default_model = "kimi-k3"
# 自主循环最大轮数 / Max turns for autonomous loop
max_turns = 20

# 各角色配置（model 可被 TUI model 命令覆盖）
# Role configs (model can be overridden by TUI `model` command)
[agents.orchestrator]
model = "kimi-k3"
preamble = "AGENTS.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"

[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "ask"     # 需人工确认 / Requires confirmation
permissions.edit_file = "allow"

[memory]
dir = "memory"
conversation_file = "conversations.jsonl"
lessons_file = "lessons.jsonl"
rules_file = "rules.json"

[evolution]
rule_escalation_threshold = 3
```

---

## 工具权限（HITL 门控）/ Tool Permissions (HITL Gating)

每个角色的工具调用按三级权限处理 / Each role's tool calls are gated by three permission tiers:

| 分级 / Tier | 行为 / Behavior | 典型场景 / Typical Scenario |
|------|------|----------|
| `allow` | 静默执行，不询问 / Silent execution, no prompt | `read_file`、`run_bash`（只读命令 / read-only commands like `ls`, `git status`） |
| `ask` | 暂停终端，询问 yes/no / Pause, ask yes/no | `run_bash`（危险命令 / dangerous commands like `rm`）、Builder 的 `edit_file` |
| `deny` | 直接拦截，不执行 / Blocked, not executed | Planner/Auditor 的 `edit_file`、`run_bash_mutating` |

> 工具分类由 `is_readonly_bash()` 自动判断。只读命令（`ls`、`cat`、`git log` 等）自动归为 `run_bash_readonly`，其余归为 `run_bash_mutating`。
> Tool classification is automatic via `is_readonly_bash()`. Read-only commands (`ls`, `cat`, `git log`, etc.) are classified as `run_bash_readonly`; everything else as `run_bash_mutating`.

---

## 技能系统 / Skill System

技能是运行时加载的 Markdown 文件，自动注入 Agent 提示词。/ Skills are runtime-loaded Markdown files automatically injected into agent prompts.

```bash
# 添加技能 / Add a skill
> add-skill rust-expert "精通 Rust 惯用法、unsafe 安全、生命周期优化"
> add-skill code-reviewer "代码审查：安全性、正确性、可维护性"

# 查看已注册技能 / List registered skills
> skills
- rust-expert
- code-reviewer
```

技能文件存储在 `skills/` 目录，格式为 `skills/<name>.md`，包含详细指令。Agent 启动时自动扫描并注入相关技能到提示词中。

Skill files are stored in `skills/` as `skills/<name>.md` with detailed instructions. The Agent scans and injects relevant skills into prompts at startup.

---

## 日志 / Logging

日志仅输出到按时间命名的文件（TUI 模式下不输出到终端）：

Logging is file-only (no terminal output in TUI mode):

```
logs/2025-07-25_14-30-00.log
```

日志级别 / Log level: `info`（默认 / default），`rig_core=off`（关闭 rig 内部日志 / suppress rig internal logs to avoid SSE span flooding）。

如需调整日志级别，修改 `src/main.rs` 中的 `EnvFilter` / To adjust log level, modify `EnvFilter` in `src/main.rs`:

```rust
// 只看 info 及以上 / info and above only
tracing_subscriber::EnvFilter::new("info")

// 包含 rig HTTP 调试（大量日志，仅调试时使用）/ Include rig HTTP debug (verbose, debug only)
tracing_subscriber::EnvFilter::new("info,rig_core=trace")

// 关闭 rig 内部日志（默认）/ Suppress rig internal logs (default)
tracing_subscriber::EnvFilter::new("info,rig_core=off")
```

---

## 项目结构 / Project Structure

```
src/
├── main.rs              # 入口：日志初始化 + TUI 启动
│                        # Entry: logging init + TUI launch
├── event.rs             # 共享事件类型（AgentEvent + EventSender）
│                        # Shared event types (AgentEvent + EventSender)
├── agent_loop.rs        # 自主循环：AgentHook 实现 + HITL 门控
│                        # Autonomous loop: AgentHook impl + HITL gating
├── providers.rs         # 多供应商统一接入（OpenAI 兼容）
│                        # Multi-provider access (OpenAI-compatible)
├── registry.rs          # 角色注册 + 工具权限分级 + 意图分类
│                        # Role registry + tool tiers + intent classification
├── tools.rs             # 内置工具（read_file / edit_file / run_bash / write_file）
│                        # Built-in tools
├── skills.rs            # 技能清单 + 注入逻辑
│                        # Skill manifest + injection logic
├── reviewer.rs          # 两阶段审计门控（规格符合性 + 代码质量）
│                        # Two-phase review gate (spec compliance + code quality)
├── memory.rs            # JSONL 对话记忆
│                        # JSONL conversation memory
├── ui/                  # TUI 表示层（ratatui + crossterm）
│   ├── tui.rs           #   事件循环 + 渲染 + 输入处理 + TerminalGuard
│   ├── markdown.rs      #   Markdown → ratatui Text 渲染器
│   ├── theme.rs         #   颜色与样式定义
│   └── mod.rs           #   模块根
├── cli/                 # 命令行上下文
│   ├── context.rs       #   AppContext + 命令分发 + 记忆集成
│   └── repl.rs          #   命令解析（ReplCommand）
└── evolution/           # 自我进化
    ├── mod.rs           #   进化模块入口
    ├── prompt_evolve.rs #   提示词进化（benchmark 防漂移）
    ├── self_modify.rs   #   代码进化（evolve-code）
    └── tool_ext.rs      #   工具扩展（add-tool）

prompts/                  # 角色提示词 / Role prompt files
skills/                  # 运行时技能文件 / Runtime skill files (Markdown)
memory/                  # 对话与经验记录 / Conversation & experience records
logs/                    # 日志文件 / Log files
```

---

## 技术栈 / Tech Stack

| 依赖 / Dependency | 用途 / Purpose |
|------|------|
| `rig-core 0.40` | LLM 框架（Agent / Tool / Hook / Runner）/ LLM framework |
| `tokio` | 异步运行时 / Async runtime |
| `ratatui 0.29` | TUI 框架（全屏渲染、布局、部件）/ TUI framework |
| `crossterm 0.28` | 终端后端（事件流、原始模式、alt-screen）/ Terminal backend |
| `pulldown-cmark 0.12` | Markdown 解析 → ratatui Text 渲染 / Markdown parser → ratatui Text |
| `tracing` + `tracing-subscriber` | 结构化日志（仅文件）/ Structured logging (file-only) |
| `chrono` | 本地时间戳 / Local timestamps |
| `serde` + `serde_json` + `toml` | 序列化 / 配置解析 / Serialization / Config parsing |
| `anyhow` + `thiserror` | 错误处理 / Error handling |

---

## License

MIT
