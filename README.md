# my-agent —— 基于 rig 框架的自我进化 AI Agent

基于 [rig](https://github.com/0xPlaygrounds/rig) (Rust LLM 框架) 构建的自主循环 AI Agent，支持多供应商、多模型、人在环（HITL）门控、技能系统与自我进化。

## 架构概览

```
my-agent
├── src/
│   ├── main.rs          # 程序入口（REPL，日志初始化）
│   ├── agent_loop.rs    # 自主循环 + HITL 门控（AgentHook）
│   ├── providers.rs     # 多供应商统一接入（OpenAI 兼容）
│   ├── registry.rs      # Agent 角色注册 + 工具权限分级
│   ├── tools.rs         # 内置工具（read_file / edit_file / run_bash / write_file）
│   ├── skills.rs        # 技能系统（运行时加载，无需重编译）
│   ├── reviewer.rs      # 审计门控（预留）
│   ├── memory.rs        # JSONL 对话记忆
│   └── evolution/       # 自我进化（提示词进化 + 代码进化 + 工具扩展）
├── agent.toml           # 运行时配置（供应商 / 模型 / 权限 / 记忆）
├── AGENTS.md            # 系统提示词（主 agent）
├── prompts/             # 各角色提示词（planner / builder / auditor）
├── skills/              # 技能清单与指令（运行时加载）
├── memory/              # 对话与经验记录
└── logs/                # 按时间命名的日志文件
```

### 核心概念

| 概念 | 说明 |
|------|------|
| **自主循环** | 用户输入目标 → Builder 自行规划并调用工具 → 达到 max_turns 或任务完成时停止 |
| **HITL（人在环）** | 工具调用按 `allow / ask / deny` 三级权限门控，危险操作暂停等待人工确认 |
| **多角色** | Orchestrator（编排）/ Planner（规划）/ Builder（构建）/ Auditor（审计） |
| **自我进化** | 提示词进化 + 代码进化 + 工具扩展，通过经验积累提升能力 |
| **技能系统** | 运行时加载 Markdown 技能文件，注入提示词，无需重编译 |

---

## 环境准备

### 1. 安装 Rust 工具链

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

安装完成后重启终端，验证：

```bash
rustc --version
cargo --version
```

### 2. 获取项目代码

```bash
git clone https://github.com/spmno/my-agent.git
cd my-agent
```

---

## 供应商配置

本项目通过环境变量 `MY_AGENT_PROVIDER` 选择供应商，API Key 从对应环境变量读取。所有供应商均通过 OpenAI 兼容接口接入。

### DeepSeek（默认）

无需设置 `MY_AGENT_PROVIDER`，默认即为 DeepSeek。

```bash
export DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 | 值 |
|------|-----|
| 环境变量 | `DEEPSEEK_API_KEY` |
| Base URL | `https://api.deepseek.com/v1` |
| 模型标识 | `deepseek-v4-pro` / `deepseek-v4-flash` |

API Key 获取：https://platform.deepseek.com/api_keys

---

### 百炼（阿里云 DashScope）

```bash
export MY_AGENT_PROVIDER=bailian
export DASHSCOPE_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 | 值 |
|------|-----|
| 环境变量 | `DASHSCOPE_API_KEY` |
| Base URL | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| 模型标识 | `kimi/kimi-k3` / `qwen-plus` / `qwen-max` |

> **注意**：百炼平台的模型标识格式为 `组织名/模型名`（如 `kimi/kimi-k3`），与其它供应商不同。

API Key 获取：https://dashscope.console.aliyun.com/apiKey

---

### Moonshot（Kimi 平台）

```bash
export MY_AGENT_PROVIDER=moonshot
export MOONSHOT_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 | 值 |
|------|-----|
| 环境变量 | `MOONSHOT_API_KEY` |
| Base URL | `https://api.moonshot.cn/v1` |
| 模型标识 | `kimi-k3` / `kimi-k2.7-code-highspeed` |

> **注意**：Kimi K3 仅接受 `temperature=1.0`，代码会自动限制，无需手动设置。

API Key 获取：https://platform.kimi.com/api-keys

---

### 快速切换供应商

通过 REPL 命令可在运行时切换模型（无需重启）：

```bash
# 启动时指定供应商
export MY_AGENT_PROVIDER=moonshot
export MOONSHOT_API_KEY=sk-xxx
cargo run

# 运行时切换模型（同供应商内）
> model kimi-k2.7-code-highspeed

# 查看当前模型
> model
```

---

## 使用方法

### 启动

```bash
cargo run
```

启动后显示：

```
my-agent ready (DeepSeek). model: kimi-k3
Commands: model <slug> | evolve | evolve-code | add-tool | add-skill | skills | quit
>
```

### 输入自然语言目标

直接输入你想让 Agent 完成的任务，它会自行规划并调用工具执行：

```
> 帮我看一下当前项目的目录结构
[调用工具] run_bash({"command":"find . -type f -not -path './target/*' -not -path './.git/*'"})
[工具结果] run_bash: ./src/main.rs ./src/tools.rs ...
[回复] 当前项目的文件结构如下...

> 读一下 src/main.rs 的前 20 行
[调用工具] read_file({"path":"src/main.rs"})
[回复] src/main.rs 的内容如下...

> 帮我创建一个 hello.rs 文件
[HITL] 允许执行 `write_file`？[y/N] y
[调用工具] write_file({"path":"hello.rs","content":"fn main() { println!(\"hello\"); }"})
[回复] 已创建 hello.rs。
```

### REPL 元命令

| 命令 | 说明 | 示例 |
|------|------|------|
| `model <slug>` | 切换当前会话的模型 | `model kimi-k3` |
| `model` | 查看当前模型 | `model` |
| `add-skill <name> <description>` | 添加新技能（运行时加载） | `add-skill "rust-expert" "Rust 代码审查技能"` |
| `skills` | 列出所有已注册技能 | `skills` |
| `history [n]` | 查看最近 n 轮对话记录（默认 10） | `history 5` |
| `lessons` | 查看已积累的经验教训 | `lessons` |
| `help` | 显示所有可用命令 | `help` |
| `evolve` | 触发提示词进化（根据经验优化提示词） | `evolve` |
| `evolve-code <file> <old> <new>` | 代码进化（替换代码片段） | `evolve-code src/main.rs "旧代码" "新代码"` |
| `add-tool <name> <desc>` | 扩展工具（运行时注册） | `add-tool "git-commit" "Git 提交工具"` |
| `quit` | 退出程序 | `quit` |

---

## 配置文件（agent.toml）

```toml
[provider]
# 由 MY_AGENT_PROVIDER 环境变量控制，无需手动配置

[agent]
# 默认模型（需与供应商匹配）
# DeepSeek: deepseek-v4-pro / deepseek-v4-flash
# Bailian:  kimi/kimi-k3 / qwen-plus / qwen-max
# Moonshot: kimi-k3 / kimi-k2.7-code-highspeed
default_model = "kimi-k3"
# 自主循环最大轮数
max_turns = 20

# 各角色配置（model 可被 REPL model 命令覆盖）
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
permissions.run_bash_mutating = "ask"     # 需人工确认
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

## 工具权限（HITL 门控）

每个角色的工具调用按三级权限处理：

| 分级 | 行为 | 典型场景 |
|------|------|----------|
| `allow` | 静默执行，不询问 | `read_file`、`run_bash`（只读命令如 `ls`、`git status`） |
| `ask` | 暂停终端，询问 yes/no | `run_bash`（危险命令如 `rm`）、Builder 的 `edit_file` |
| `deny` | 直接拦截，不执行 | Planner/Auditor 的 `edit_file`、`run_bash_mutating` |

> 工具分类由 `is_readonly_bash()` 自动判断。只读命令（`ls`、`cat`、`git log` 等）自动归为 `run_bash_readonly`，其余归为 `run_bash_mutating`。

---

## 技能系统

技能是运行时加载的 Markdown 文件，自动注入 Agent 提示词。

```bash
# 添加技能
> add-skill rust-expert "精通 Rust 惯用法、unsafe 安全、生命周期优化"
> add-skill code-reviewer "代码审查：安全性、正确性、可维护性"

# 查看已注册技能
> skills
- rust-expert
- code-reviewer
```

技能文件存储在 `skills/` 目录，格式为 `skills/<name>.md`，包含详细指令。Agent 启动时自动扫描并注入相关技能到提示词中。

---

## 日志

日志同时输出到终端和按时间命名的文件：

```
logs/2025-07-25_14-30-00.log
```

日志级别：`info`（默认），`rig_core=off`（关闭 rig 内部日志，避免 SSE span 字段洪流）。

如需调整日志级别，修改 `src/main.rs` 中的 `EnvFilter`：

```rust
// 只看 info 及以上
tracing_subscriber::EnvFilter::new("info")

// 包含 rig HTTP 调试（大量日志，仅调试时使用）
tracing_subscriber::EnvFilter::new("info,rig_core=trace")

// 关闭 rig 内部日志（默认，避免 SSE span 字段洪流）
tracing_subscriber::EnvFilter::new("info,rig_core=off")
```

---

## 项目结构

```
src/
├── main.rs              # 入口：日志初始化 + REPL 循环
├── agent_loop.rs        # 自主循环：AgentHook 实现 + HITL 门控
├── providers.rs         # 多供应商统一接入（OpenAI 兼容）
├── registry.rs          # 角色注册 + 工具权限分级
├── tools.rs             # 内置工具（read_file / edit_file / run_bash / write_file）
├── skills.rs            # 技能清单 + 注入逻辑
├── reviewer.rs          # 审计门控（预留）
├── memory.rs            # JSONL 对话记忆
└── evolution/
    ├── mod.rs           # 进化模块入口
    ├── prompt_evolve.rs # 提示词进化
    ├── self_modify.rs   # 代码进化（evolve-code）
    └── tool_ext.rs      # 工具扩展（add-tool）

prompts/
├── planner.md           # 规划者提示词
├── builder.md           # 构建者提示词
└── auditor.md           # 审计者提示词

skills/                  # 运行时技能文件（Markdown）
memory/                  # 对话与经验记录（JSONL / JSON）
logs/                    # 按时间命名的日志文件
```

---

## 技术栈

| 依赖 | 用途 |
|------|------|
| `rig-core 0.40` | LLM 框架（Agent / Tool / Hook / Runner） |
| `tokio` | 异步运行时 |
| `rustyline` | REPL 行编辑（支持中文 / 历史记录） |
| `tracing` + `tracing-subscriber` | 结构化日志（终端 + 文件双输出） |
| `chrono` | 本地时间戳 |
| `serde` + `serde_json` + `toml` | 序列化 / 配置解析 |
| `anyhow` + `thiserror` | 错误处理 |

---

## License

MIT
