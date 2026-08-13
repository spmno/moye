# moye —— 基于 rig 框架的自我进化 AI Agent

[English](./README.md) | [简体中文](./README.zh-CN.md)

基于 [rig](https://github.com/0xPlaygrounds/rig) (Rust LLM 框架) 构建的自主循环 AI Agent，支持多供应商、多模型、人在环（HITL）门控、技能系统与自我进化。

---

## 架构概览

```
moye
├── src/
│   ├── main.rs          # 程序入口（日志初始化 + 配置加载 + TUI 启动）
│   ├── config.rs        # 统一配置模块（agent.toml 一次解析，全 crate 共享）
│   ├── event.rs         # 共享事件类型（AgentEvent + EventSender）
│   ├── agent_loop.rs    # 自主循环 + HITL 门控（AgentHook）
│   ├── providers.rs     # 多供应商统一接入（OpenAI 兼容）
│   ├── registry.rs      # Agent 角色注册 + 工具权限分级 + SDD 管线编排
│   ├── tools.rs         # 内置工具（read_file / edit_file / run_bash / write_file / web_fetch / web_search）
│   ├── tools_ext/       # 动态工具模块（由 /add-tool 生成，需重新编译）
│   │   └── mod.rs       # 动态工具模块（由 /add-tool 生成，需重新编译）
│   ├── sandbox.rs       # 文件系统沙箱（限制访问到项目根目录及子目录）
│   ├── context.rs       # 上下文管理（token 估算 + 两层压缩 + 工具输出截断）
│   ├── skills.rs        # 技能系统（运行时加载，无需重编译）
│   ├── reviewer.rs      # 审计门控（两阶段评审）
│   ├── memory.rs        # JSONL 对话记忆
│   ├── model_history.rs # 模型使用历史持久化（~/.config/moye/models.json）
│   ├── ui/              # TUI 表示层（ratatui + crossterm）
│   │   ├── tui.rs       #   事件循环 + 渲染 + 输入处理
│   │   ├── markdown.rs  #   Markdown → ratatui Text 渲染器
│   │   ├── theme.rs     #   颜色与样式
│   │   ├── clipboard.rs #   OSC52 剪贴板复制（穿透 SSH / tmux）
│   │   ├── selection.rs #   字符级文本选择状态机（鼠标拖拽选区）
│   │   ├── selector.rs  #   交互式列表选择器（opencode 风格）
│   │   └── mod.rs       #   模块根
│   ├── cli/             # 命令行上下文
│   │   ├── context.rs   #   AppContext + 命令分发
│   │   ├── repl.rs      #   命令解析
│   │   └── mod.rs       #   模块根
│   └── evolution/       # 自我进化（提示词进化 + 代码进化 + 工具扩展）
│       ├── mod.rs       #   模块根
│       ├── prompt_evolve.rs  # 提示词进化
│       ├── self_modify.rs    # 代码自修改（编译验证 + 回退）
│       └── tool_ext.rs       # 工具扩展（脚手架生成）
├── agent.toml           # 运行时配置（供应商 / 模型 / 权限 / 记忆）
├── AGENTS.md            # 系统提示词（主 agent）
├── prompts/             # 各角色提示词（investigator / planner / builder / auditor）
├── docs/                # 文档（架构 / 上下文管理 / 对比 CrewAI）
├── memory/              # 对话与经验记录
└── logs/                # 按时间命名的日志文件
```

### 依赖方向

领域层（`agent_loop`、`registry`、`reviewer`、`prompt_evolve`）只依赖 `event` 和 `config` 模块，不依赖 `ui`。配置模块（`config`）统一解析 `agent.toml`，全 crate 通过 `Arc<Config>` 或 `config::config()` 共享。表示层（`ui::tui`）依赖 `event` 和自身的子模块。这保证了依赖倒置：未来加 CLI / API 消费者时，无需修改任何领域层代码。

### 核心概念

| 概念 | 说明 |
|------|------|
| **自主循环** | 用户输入目标 → Builder 自行规划并调用工具 → 达到 max_turns 或任务完成时停止。 |
| **SDD 管线** | 实现意图自动走调查→规划→构建→审计流水线：Investigator 探索代码 → Planner 拆解步骤 → Builder 执行（工具循环 + HITL）→ Auditor 两轮评审。 |
| **HITL（人在环）** | 工具调用按 `allow / ask / deny` 三级权限门控，危险操作暂停等待人工确认。 |
| **多角色** | Orchestrator（编排）/ Investigator（调查）/ Planner（规划）/ Builder（构建）/ Auditor（审计）。 |
| **自我进化** | 提示词进化 + 代码进化 + 工具扩展，通过经验积累提升能力。 |
| **技能系统** | 运行时加载 Markdown 技能文件，注入提示词，无需重编译。 |
| **事件驱动** | Agent 输出通过 `AgentEvent` channel 传递给 TUI，解耦领域层与表示层。 |
| **文件系统沙箱** | Agent 默认只能访问项目根目录及子目录；访问其它目录需用户手动授权。 |
| **联网工具** | 内置 `web_fetch`（抓取网页转纯文本）与 `web_search`（DuckDuckGo 搜索），权限按角色分级。 |
| **模型历史** | 用过的模型持久化到 `~/.config/moye/models.json`，`/models` 选择器列出最近使用，一键切回。 |

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
git clone https://github.com/spmno/moye.git
cd moye
```

---

## 供应商配置

本项目通过环境变量 `AGENT_PROVIDER` 选择供应商，API Key 从对应环境变量读取。所有供应商均通过 OpenAI 兼容接口接入。支持 5 种供应商：DeepSeek、百炼（DashScope）、Moonshot（Kimi）、火山引擎（Ark）、自定义（Custom）。

### 推荐：`.env` 文件（写一次，无需每次 export）

程序启动时会自动加载项目根目录的 `.env` 文件（若存在），把配置写入进程环境——只需配置一次，之后每次启动自动生效，无需手动 export。显式 export 的变量优先，不会被 `.env` 覆盖。

```bash
# 首次配置：复制模板并填写你的 key
cp .env.example .env
# 编辑 .env：设置 AGENT_PROVIDER 与对应 API Key
# 可选供应商：deepseek / bailian / moonshot / volcengine / custom

cargo run   # 之后每次启动自动生效
```

`.env` 已被 `.gitignore` 忽略，密钥不会进入 git。

### DeepSeek（默认）

无需设置 `AGENT_PROVIDER`，默认即为 DeepSeek。

```bash
export DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

或在 `.env` 中配置：

```bash
AGENT_PROVIDER=deepseek
DEEPSEEK_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
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
export AGENT_PROVIDER=bailian
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
export AGENT_PROVIDER=moonshot
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

### 火山引擎 Ark

```bash
export AGENT_PROVIDER=volcengine
export ARK_API_KEY=xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
cargo run
```

| 项目 | 值 |
|------|-----|
| 环境变量 | `ARK_API_KEY` |
| Base URL | `https://ark.cn-beijing.volces.com/api/plan/v3` |
| 模型标识 | `doubao-1-5-pro-256k` / `doubao-1-5-lite-32k` / `deepseek-r1-250120` |

API Key 获取：https://console.volcengine.com/ark/

---

### 自定义 OpenAI 兼容供应商

适用于任何兼容 OpenAPI 的网关（如 OpenRouter、智谱 GLM、自建网关等）。通过 `AGENT_BASE_URL` + `AGENT_API_KEY` 配置，也可在 `agent.toml` 的 `[provider]` 小节中设置。

```bash
export AGENT_PROVIDER=custom
export AGENT_BASE_URL=https://ai-gateway.example.com/v1
export AGENT_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxx
cargo run
```

| 项目 | 值 |
|------|-----|
| 环境变量 | `AGENT_API_KEY` |
| Base URL | 自定义（`AGENT_BASE_URL` 或 `agent.toml`） |
| 模型标识 | 任意 OpenAI 兼容模型 ID |

> **注意**：Custom 供应商无内置模型目录，`/models` 选择器允许直接输入任意模型 ID。

---

### 全局配置回退

除项目根的 `agent.toml` 外，程序启动时还会读取 `~/.config/moye/config.toml`（若存在），将其中的 `[provider]` 字段和 `[keys]` 条目作为项目缺失项的回退。优先级：环境变量 > 项目 `agent.toml` > 全局 `config.toml` > 供应商默认。

---

### 快速切换供应商

通过 TUI 命令可在运行时切换模型（无需重启）：

```bash
# 启动时指定供应商
export AGENT_PROVIDER=moonshot
export MOONSHOT_API_KEY=sk-xxx
cargo run

# 运行时切换模型（同供应商内）
> /model kimi-k2.7-code-highspeed

# 查看当前模型
> /model

# 打开交互式模型选择器
> /models

# 启动时通过环境变量覆盖默认模型
export AGENT_MODEL=glm-latest
cargo run
```

> **模型历史**：用过的模型会持久化到 `~/.config/moye/models.json`，`/models` 选择器列出"最近使用"分区，一键切回，无需重新输入 slug。

---

## 使用方法

### 启动

```bash
cargo run
```

启动后进入全屏 TUI 界面：

```
 moye (DeepSeek) | model: kimi-k3                    │ Provider
Enter 发送任务 | /help 帮助 | Ctrl+C 退出                │   DeepSeek
                                                         │
» _                                                      │ Model
                                                         │   kimi-k3
                                                         │
                                                         │ Context
                                                         │   0 tok
                                                         │
                                                         │ Progress
                                                         │   [░░░░░░░░░░] 0/20
                                                         │   ✓ ready
                                                         │
                                                         │ Tools (6)
                                                         │   • read_file
                                                         │   • edit_file
                                                         │   • write_file
                                                         │   • run_bash
                                                         │   • web_fetch
                                                         │   • web_search
```

右侧侧边栏实时显示：供应商、模型、累计 token 用量及上次消耗明细、回合进度条、运行状态（thinking / HITL / ready）、全部工具名称列表、已注册技能列表。无背景色，以左边框与主区域分隔。

### 输入自然语言目标

直接输入你想让 Agent 完成的任务，它会自行规划并调用工具执行。流式输出实时显示。

### TUI 命令

| 命令 | 别名 | 说明 | 示例 |
|------|------|------|------|
| `/model [slug]` | `/m` | 切换或查看当前会话模型 | `/model kimi-k3` |
| `/models` | — | 打开交互式模型选择器（opencode 风格，可输入过滤/自定义 ID） | `/models` |
| `/add-skill <name> <desc>` | — | 添加新技能（运行时加载） | `/add-skill "rust-expert" "Rust code review"` |
| `/skills` | — | 列出所有已注册技能 | `/skills` |
| `/history [n]` | `/hist` | 查看最近 n 轮对话记录（默认 10） | `/history 5` |
| `/lessons` | — | 查看已积累的经验教训 | `/lessons` |
| `/help` | `/h` `/?` | 显示所有可用命令 | `/help` |
| `/evolve` | `/e` | 触发提示词进化 | `/evolve` |
| `/evolve-code <file> <old> <new>` | `/ec` | 代码进化（替换代码片段，编译验证 + 回退） | `/evolve-code src/main.rs "old" "new"` |
| `/add-tool <name> <desc>` | — | 扩展工具（运行时注册，需重新编译） | `/add-tool "git-commit" "Git commit tool"` |
| `/quit` | `/q` `/exit` | 退出程序 | `/quit` |
| 非 `/` 开头的输入 | — | 作为自然语言目标交给 Orchestrator | `帮我修复 bug` |

### TUI 快捷键

| 按键 | 动作 |
|------|------|
| `Enter` | 发送输入 |
| `Ctrl+C` / `Ctrl+D` | 退出 |
| `Up` / `Down` | 浏览输入历史 |
| `PageUp` / `PageDown` | 滚动消息（每次 5 行） |
| `鼠标滚轮` | 滚动消息（每次 3 行） |
| `鼠标拖拽` | 选中文本，松开自动复制到剪贴板（OSC52） |
| `Home` / `End` | 光标移至行首/行尾 |
| `y` / `n`（HITL 模式） | 允许/拒绝工具执行 |

> **智能上翻**：手动上翻后（PageUp / 鼠标滚轮），新事件不会自动跳到底部；右侧滚动条显示当前位置，按 PageDown 或滚轮回到底部后恢复自动滚动。

---

## 配置文件（agent.toml）

```toml
[provider]
# 默认供应商（deepseek / bailian / moonshot / volcengine / custom）
# 优先级：AGENT_PROVIDER 环境变量（或 .env）> 此处配置 > 全局 config.toml > deepseek
provider = "deepseek"
# 可选：全局覆盖 OpenAI 兼容 base URL
# 优先级：AGENT_BASE_URL 环境变量 > 此处配置 > 供应商默认
# base_url = "https://api.example.com/v1"
# 可选：自定义 API key 环境变量名（默认按供应商自动选择）
# api_key_env = "MY_CUSTOM_KEY"

[agent]
# 默认模型（需与供应商匹配）
# DeepSeek: deepseek-v4-pro / deepseek-v4-flash
# Bailian:  kimi/kimi-k3 / qwen-plus / qwen-max
# Moonshot: kimi-k3 / kimi-k2.7-code-highspeed
# Volcengine: doubao-1-5-pro-256k / doubao-1-5-lite-32k / deepseek-r1-250120
# Custom: 任意 OpenAI 兼容模型 ID
default_model = "kimi-k3"
# 自主循环最大轮数
max_turns = 30

# 各角色配置（model 可被 /model 命令覆盖）
# 权限字段：read_file / run_bash_readonly / run_bash_mutating / edit_file / web_fetch / web_search
[agents.orchestrator]
model = "kimi-k3"
preamble = "AGENTS.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.investigator]
model = "kimi-k3"
preamble = "prompts/investigator.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.planner]
model = "kimi-k3"
preamble = "prompts/planner.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.builder]
model = "kimi-k3"
preamble = "prompts/builder.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "ask"     # 需人工确认
permissions.edit_file = "allow"
permissions.web_fetch = "allow"
permissions.web_search = "allow"

[agents.auditor]
model = "kimi-k3"
preamble = "prompts/auditor.md"
permissions.read_file = "allow"
permissions.run_bash_readonly = "allow"
permissions.run_bash_mutating = "deny"
permissions.edit_file = "deny"
permissions.web_fetch = "deny"
permissions.web_search = "deny"

[context]
# 上下文管理配置
max_output_tokens = 4096           # 预留输出 token
compaction_threshold = 0.5         # 触发 LLM 摘要的比例
keep_recent_turns = 2              # 压缩时保留的最近轮数
max_bash_output_chars = 20000      # run_bash 输出截断
max_read_lines = 500               # read_file 输出截断
microcompact_threshold = 20000     # Tier 1 触发 token 阈值
microcompact_protected_results = 3 # Tier 1 保护最近 N 个工具结果

[memory]
dir = "memory"
conversation_file = "conversations.jsonl"
lessons_file = "lessons.jsonl"
rules_file = "rules.json"

[evolution]
rule_escalation_threshold = 3

# 可选：全局 API key 存储（项目 .env / export 优先，缺失时回退此处）
# [keys]
# DEEPSEEK_API_KEY = "sk-xxx"
# MOONSHOT_API_KEY = "sk-yyy"
```

---

## 文件系统沙箱

Agent 默认只能访问**项目根目录（当前工作目录）及其子目录**。访问其它目录时，会暂停并请求用户授权。

### 工作原理

| 步骤 | 行为 |
|------|------|
| 1. 工具调用 | Agent 调用 `read_file` / `edit_file` / `write_file` / `run_bash` 时，沙箱先检查路径 |
| 2. 路径在沙箱内 | 静默放行，继续执行 |
| 3. 路径在沙箱外 | 弹出 HITL 确认框，询问用户是否授权 |
| 4. 用户确认 | 该目录加入授权列表，后续访问不再询问 |
| 5. 用户拒绝 | 工具调用被跳过，Agent 收到拒绝原因 |

### 检测的路径模式

沙箱从 `run_bash` 命令中提取以下路径模式进行检查：

- 绝对路径（`/etc/passwd`）
- 主目录路径（`~/.config`）
- 含 `..` 的相对路径（可能逃逸沙箱）
- `cd` / `pushd` 的目标参数
- 重定向目标（`> /tmp/file`）

### 禁用沙箱

```bash
# 通过环境变量禁用（不推荐）
export AGENT_SANDBOX=off
cargo run
```

> **安全提示**：禁用沙箱后，Agent 可访问文件系统上的任意路径，请仅在受信任的环境中使用。

---

## 工具权限（HITL 门控）

每个角色的工具调用按三级权限处理：

| 分级 | 行为 | 典型场景 |
|------|------|----------|
| `allow` | 静默执行，不询问 | `read_file`、`run_bash`（只读命令如 `ls`、`git status`）、`web_fetch`、`web_search` |
| `ask` | 暂停终端，询问 yes/no | `run_bash`（危险命令如 `rm`）、Builder 的 `edit_file` |
| `deny` | 直接拦截，不执行 | Planner/Auditor 的 `edit_file`、`run_bash_mutating`、Auditor 的 `web_fetch`/`web_search` |

> 工具分类由 `is_readonly_bash()` 自动判断。只读命令（`ls`、`cat`、`git log` 等）自动归为 `run_bash_readonly`，其余归为 `run_bash_mutating`。`web_fetch` 和 `web_search` 也有独立的权限字段，默认为 `ask`。

---

## 上下文管理

长时间运行的 Agent 会话会积累大量对话历史（用户消息、Assistant 回复、工具调用结果），最终超出模型的上下文窗口。moye 采用**两层渐进式压缩策略**，在不丢失关键信息的前提下自动管理上下文。

### 策略概览

```
                    估算 token 数
                         │
           ┌─────────────┴─────────────┐
           │                           │
     ≤ microcompact_threshold    > microcompact_threshold
           │                           │
     不做处理                      Tier 1: 微压缩（无 LLM）
                                    │
                           ┌────────┴────────┐
                           │                  │
                     仍超阈值            已降至阈值以下
                           │                  │
                     Tier 2: LLM 摘要     返回微压缩历史
                           │
                     返回摘要 + 近期历史
```

| 层级 | 机制 | 触发条件 | LLM 调用 | 耗时 |
|------|------|------|------|------|
| **Tier 1 — 微压缩** | 扫描历史中的旧 `ToolResult`，保护最近 N 个，将更早的工具结果替换为 `[工具结果已清除]` 标记 | 估算 token > `microcompact_threshold`（默认 20000） | 否 | 极低（纯内存操作） |
| **Tier 2 — LLM 摘要** | 将旧对话历史发送给摘要 LLM，生成 5 段锚定摘要，替换为单条 System 消息 | Tier 1 后仍超过 `compaction_threshold`（默认 0.5 × 有效预算）| 是 | 较高（一次 LLM 调用） |

### Tier 1：微压缩

**无需 LLM 调用**的轻量压缩。工具调用结果（`ToolResult`）通常占据大量 token（如 `read_file` 返回数百行代码、`run_bash` 返回长输出），但在后续轮次中往往不再需要完整内容。

**工作流程：**

1. 扫描历史中所有 `ToolResult` 的位置
2. 保护最近 `microcompact_protected_results` 个（默认 3）
3. 将更早的 `ToolResult` 内容替换为 `[工具结果已清除]`
4. 保留 `ToolResult` 的 `id` 和 `call_id`（API 需要它们关联工具调用）

**示例：**

```
压缩前：
  User: do task 1
  Assistant: [tool_call: read_file]
  User: [ToolResult: fn main() { ... 200 lines of code ... }]   ← 清除
  Assistant: [tool_call: run_bash]
  User: [ToolResult: cargo build output ... 50 lines ...]       ← 清除
  Assistant: [tool_call: edit_file]
  User: [ToolResult: file edited successfully]                  ← 保留（最近 3 个之一）
  Assistant: [tool_call: run_bash]
  User: [ToolResult: cargo test output ... 30 lines ...]        ← 保留
  Assistant: [tool_call: read_file]
  User: [ToolResult: fn helper() { ... }]                       ← 保留

压缩后：
  User: do task 1
  Assistant: [tool_call: read_file]
  User: [ToolResult: [工具结果已清除]]     ← 替换
  Assistant: [tool_call: run_bash]
  User: [ToolResult: [工具结果已清除]]     ← 替换
  Assistant: [tool_call: edit_file]
  User: [ToolResult: file edited successfully]                  ← 原样保留
  ...
```

### Tier 2：LLM 摘要

当 Tier 1 微压缩后仍超出 `compaction_threshold` 时触发。将旧对话历史发送给摘要 LLM，生成** 5 段锚定摘要**（受 OpenCode 启发），确保关键信息不丢失。如果历史中已存在上一次摘要，LLM 将增量更新而非从头创建。

**5 段摘要模板：**

| # | 小节 | 内容 |
|---|------|------|
| 1 | Objective | 用户目标（1-2 句） |
| 2 | Important Details | 约束/偏好、决策原因、重要事实 |
| 3 | Work State | Completed / Active / Blocked — 工作状态 |
| 4 | Next Move | 紧接着需要做什么 |
| 5 | Relevant Files | 文件/目录路径及重要性 |

**压缩后历史结构：**

```
[System: [对话历史摘要]
  ## 1. Objective ...
  ## 2. Important Details ...
  ...
  ## 5. Relevant Files ...]
[System: Continue if you have next steps ...]
[User: 最近第 1 轮消息]          ← keep_recent_turns 保留
[Assistant: 最近第 1 轮回复]
[User: 最近第 2 轮消息]
...
```

### rig PatchRequest 机制

压缩通过 rig 的 `Flow::PatchRequest` + `RequestPatch::history()` 实现。这是**每轮非粘性**的替换：只修改当轮发送给 API 的历史，不影响 rig 内部持久化的真实 transcript。这意味着 Agent 始终能访问完整的真实历史，只是在发送给 API 时用压缩版替换。

### 配置参数

所有参数在 `agent.toml` 的 `[context]` 节配置：

| 参数 | 默认值 | 说明 |
|------|------|------|
| `max_output_tokens` | `4096` | 预留给模型输出的 token 数 |
| `compaction_threshold` | `0.5` | Tier 2 触发比例（占有效预算的比例，0.0–1.0） |
| `keep_recent_turns` | `2` | Tier 2 压缩时保留的最近对话轮数 |
| `max_bash_output_chars` | `20000` | `run_bash` 工具输出截断字符数 |
| `max_read_lines` | `500` | `read_file` 工具输出截断行数 |
| `microcompact_threshold` | `20000` | Tier 1 触发的 token 阈值 |
| `microcompact_protected_results` | `3` | Tier 1 微压缩时保护的最近工具结果数量 |

> **有效预算** = `上下文窗口大小 - max_output_tokens`。例如 128K 窗口、4096 预留 → 有效预算 123,904 tokens，Tier 2 触发线 = 123,904 × 0.5 ≈ 61,952 tokens。

### 缓存机制

压缩结果会被缓存（`compaction_cache`）。当历史长度未变时（例如 Agent 在等待用户输入期间多次进入 `handle_completion_call`），直接复用上次的压缩结果，避免重复计算。

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

日志仅输出到按时间命名的文件（TUI 模式下不输出到终端）：

```
logs/2025-07-25_14-30-00.log
```

日志级别：`info`（默认），`rig_core=off`（关闭 rig 内部日志，避免 SSE span 泛滥）。

如需调整日志级别，修改 `src/main.rs` 中的 `EnvFilter`：

```rust
// 只看 info 及以上
tracing_subscriber::EnvFilter::new("info")

// 包含 rig HTTP 调试（大量日志，仅调试时使用）
tracing_subscriber::EnvFilter::new("info,rig_core=trace")

// 关闭 rig 内部日志（默认）
tracing_subscriber::EnvFilter::new("info,rig_core=off")
```

---

## 项目结构

```
src/
├── main.rs              # 入口：日志初始化 + 配置加载 + TUI 启动
├── config.rs            # 统一配置模块（agent.toml 一次解析，全 crate 共享）
├── event.rs             # 共享事件类型（AgentEvent + EventSender）
├── agent_loop.rs        # 自主循环：AgentHook 实现 + HITL 门控
├── providers.rs         # 多供应商统一接入（OpenAI 兼容）
├── registry.rs          # 角色注册 + 工具权限分级 + SDD 管线编排
├── tools.rs             # 内置工具（read_file / edit_file / run_bash / write_file / web_fetch / web_search）
├── tools_ext/           # 动态工具模块（由 /add-tool 生成，需重新编译）
│   └── mod.rs           #   动态工具模块（由 /add-tool 生成，需重新编译）
├── sandbox.rs           # 文件系统沙箱（限制访问到项目根目录及子目录）
├── context.rs           # 上下文管理（token 估算 + 两层压缩 + 工具输出截断）
├── skills.rs            # 技能清单 + 注入逻辑
├── reviewer.rs          # 两阶段审计门控（规格符合性 + 代码质量）
├── memory.rs            # JSONL 对话记忆
├── model_history.rs     # 模型使用历史持久化（~/.config/moye/models.json）
├── ui/                  # TUI 表示层（ratatui + crossterm）
│   ├── tui.rs           #   事件循环 + 渲染 + 输入处理 + TerminalGuard
│   ├── markdown.rs      #   Markdown → ratatui Text 渲染器
│   ├── theme.rs         #   颜色与样式定义
│   ├── clipboard.rs     #   OSC52 剪贴板复制（穿透 SSH / tmux）
│   ├── selection.rs     #   字符级文本选择状态机（鼠标拖拽选区）
│   ├── selector.rs      #   交互式列表选择器（opencode 风格）
│   └── mod.rs           #   模块根
├── cli/                 # 命令行上下文
│   ├── context.rs       #   AppContext + 命令分发 + 记忆集成
│   ├── repl.rs          #   命令解析（ReplCommand）
│   └── mod.rs           #   模块根
└── evolution/           # 自我进化
    ├── mod.rs           #   进化模块入口
    ├── prompt_evolve.rs #   提示词进化（benchmark 防漂移）
    ├── self_modify.rs   #   代码进化（evolve-code）
    └── tool_ext.rs      #   工具扩展（add-tool）

prompts/                  # 角色提示词文件
skills/                  # 运行时技能文件（Markdown）
memory/                  # 对话与经验记录
logs/                    # 日志文件
```

---

## 技术栈

| 依赖 | 用途 |
|------|------|
| `rig-core 0.40` | LLM 框架（Agent / Tool / Hook / Runner） |
| `rig-memory 0.40` | rig 记忆扩展 |
| `tokio` | 异步运行时 |
| `ratatui 0.29` | TUI 框架（全屏渲染、布局、部件） |
| `crossterm 0.28` | 终端后端（事件流、原始模式、alt-screen） |
| `pulldown-cmark 0.12` | Markdown 解析 → ratatui Text 渲染 |
| `reqwest 0.13` | HTTP 客户端（web_fetch / web_search） |
| `tracing` + `tracing-subscriber` | 结构化日志（仅文件） |
| `chrono` | 本地时间戳 |
| `serde` + `serde_json` + `toml` | 序列化 / 配置解析 |
| `anyhow` + `thiserror` | 错误处理 |
| `shlex 2` | Shell 命令分词（run_bash 解析） |
| `futures 0.3` | 异步流处理（事件流） |
| `dotenvy 0.15` | `.env` 文件自动加载 |

---

## License

MIT
