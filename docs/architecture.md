# moye 整体架构

> 本文档是架构设计参考与 Phase 执行依据。代码实现以本文件为准；如有偏差，改代码不改文档。

## 设计目标

1. **通用任务 agent**——不只是编码，也能做研究、规划、审计、数据处理。
2. **可自进化**——保留代码自修改 + 提示词进化 + 经验沉淀能力。
3. **生产可用基座**——上下文管理、会话持久化、跨会话搜索。
4. **Rust 类型安全**——所有原语用 trait + enum + 强类型 struct 表达，编译期保证一致性。

## 设计原则

- **不写预留代码**。trait + impl + 调用点三位一体，缺一不进代码库。预留设计写进本文档，不写进 `src/`。
- **禁止 `#[allow(dead_code)]`**。编译器报 dead_code 是信号——要么接线，要么删掉。
- **演进式实现**。要 Phase N 再写 Phase N。
- **提示词统一中文**。给 LLM 的自然语言（system prompt、role preamble、tool description）统一中文；JSON Schema 参数名、代码标识符保持英文。

## 分层架构

```
┌─────────────────────────────────────────────────────────────┐
│  Interface Layer（接口层）                                    │
│  clap CLI · rustyline REPL · （未来：ratatui TUI · axum HTTP）│
├─────────────────────────────────────────────────────────────┤
│  Evolution Layer（进化层）                                    │
│  PromptEvolver · CodeModifier · LessonExtractor · RulePromoter│
├─────────────────────────────────────────────────────────────┤
│  Orchestration Layer（编排层）                                │
│  Orchestrator · IntentClassifier · Investigator · ReviewGate│
├─────────────────────────────────────────────────────────────┤
│  Session & Context Layer（会话与上下文层）—— Phase 1          │
│  SessionStore · Message/Part · ContextManager · Compactor    │
├─────────────────────────────────────────────────────────────┤
│  Subagent Layer（子 Agent 层）—— Phase 2                      │
│  SubagentSpawner · BackgroundJob · PermissionDerivation      │
├─────────────────────────────────────────────────────────────┤
│  Memory & Knowledge Layer（记忆与知识层）                     │
│  MemoryStore · LessonStore · RuleStore · （未来：VectorMemory）│
├─────────────────────────────────────────────────────────────┤
│  Tool & Skill Layer（工具与技能层）                           │
│  ToolRegistry · builtin tools · tools_ext · SkillLoader      │
├─────────────────────────────────────────────────────────────┤
│  Provider & Permission Layer（Provider 与权限层）              │
│  Provider · CompletionModel · PermissionGate · HitlHook      │
├─────────────────────────────────────────────────────────────┤
│  Config & Storage Layer（配置与存储层）                        │
│  agent.toml · JSONL/SQLite · memory/ · skills/ · logs/      │
└─────────────────────────────────────────────────────────────┘
```

## 模块树

```
src/
├── main.rs                    # 入口：日志初始化 + 委托 cli::repl
├── cli/
│   ├── mod.rs                 # 模块声明
│   ├── repl.rs                # ReplCommand enum + match 分发
│   └── context.rs             # AppContext：持有 registry/orchestrator/memory/evolver
├── config.rs                  # 统一配置：agent.toml 一次解析（provider/agent/context/roles/memory/evolution）+ OnceLock 缓存
├── tools_ext/
│   └── mod.rs                 # 动态工具：load_all() 由 ToolManifest 重新生成
├── tool/                      # （未来：从 tools.rs 拆分）
├── skill/                     # （未来：从 skills.rs 拆分）
├── session/                  # （未来 Phase 1）
├── subagent/                  # （未来 Phase 2）
├── memory/
│   └── mod.rs                 # MemoryStore：Turn/Lesson/Rule + JSONL 持久化
├── providers/
│   └── mod.rs                 # Provider enum + OpenAI 兼容客户端
├── permission/                # （未来：从 registry.rs 拆分）
├── orchestrator/
│   └── mod.rs                 # （未来：从 registry.rs 拆分）
├── registry/
│   └── mod.rs                 # AgentRegistry + Role + Orchestrator + Intent
├── reviewer/
│   └── mod.rs                 # ReviewGate（两轮审计门）
├── agent_loop/
│   └── mod.rs                 # run_autonomous + HitlHook
├── evolution/
│   ├── mod.rs
│   ├── prompt_evolve.rs       # 提示词进化（基准门控 + git tag）
│   ├── self_modify.rs         # 代码自修改（cargo build 验证 + 回退）
│   └── tool_ext.rs            # 工具扩展（可编译模板 + 清单）
└── skills.rs                  # 技能系统（运行时 markdown 注入）

# 提示词文件（prompts/）
#   investigator.md  # 调查者：判断是否需要调查 → 探索代码库 → 产出调查报告
#   planner.md       # 规划者：拆解任务为可执行步骤
#   builder.md       # 构建者：执行编辑与 bash 命令
#   auditor.md       # 审计者：两轮评审（规格符合性 + 代码质量）
```

## Phase 路线图

### Phase 0：接线死代码 + REPL 重构（当前）

目标：消除所有 `#[allow(dead_code)]`，让 AGENTS.md 描述的 SDD 纪律真正落地。

| 工作 | 文件 |
|---|---|
| 接 ReviewGate 进 SDD 管线 | `registry.rs` Orchestrator + `reviewer.rs` |
| 接 Orchestrator + Intent 进 REPL | `registry.rs` + `cli/repl.rs` + `cli/context.rs` |
| 接 record_lesson + observe_rule + promote_rule | `cli/context.rs` 调 `memory.rs` |
| 修复 tool_ext：可编译模板 + mod 声明 + 动态加载 | `evolution/tool_ext.rs` + `tools_ext/mod.rs` + `tools.rs` |
| REPL if 链 → enum + match | `cli/repl.rs` |
| 工具描述注释矛盾修复 | `tools.rs` |
| `agent_loop::run_autonomous` 接受 Role 参数 | `agent_loop.rs` |

### Phase 1：上下文管理 + 会话持久化

| 工作 | 模块 |
|---|---|
| Message/Part 模型替换 Turn | `session/mod.rs` |
| ContextManager：token 计数 + pruning + compaction | `session/context.rs` |
| SQLite 替换 JSONL | `storage/mod.rs` |
| 接 rig-memory VectorStore | `memory/mod.rs` |

### Phase 2：子 Agent + 工具扩展

| 工作 | 模块 |
|---|---|
| SubagentSpawner + 上下文隔离 | `subagent/mod.rs` |
| BackgroundJob + 异步通知 | `subagent/background.rs` |
| Task 工具（spawn 子 agent） | `tool/task.rs` |
| 新增工具：Grep/Glob/WebFetch/WebSearch | `tool/builtin.rs` |

### Phase 3：MCP + LSP + 配置分层

### Phase 4：可观测性 + TUI

## 死代码规避规则（写进 AGENTS.md）

1. trait + impl + 调用点三位一体，否则不进代码库。
2. 禁止 `#[allow(dead_code)]` 和 `#[allow(unused)]`。
3. `todo!()`/`unimplemented!()` 只允许在自动生成的脚手架中，且必须返回错误而非 panic。
4. CI 加 `cargo machete`（未用依赖）+ `cargo +nightly udeps`（未用代码）。
