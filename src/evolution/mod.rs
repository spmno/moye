// 自我进化模块：提示词进化（prompt_evolve）、代码自修改（self_modify）、
// Self-evolution module: prompt evolution (prompt_evolve), code self-modification (self_modify),
// 工具扩展（tool_ext）。这三支共同构成 agent "自我进化"的能力。
// and tool extension (tool_ext). These three together form the agent's "self-evolution" capability.
pub mod prompt_evolve;
pub mod self_modify;
pub mod tool_ext;
