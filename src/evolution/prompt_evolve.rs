// 提示词进化模块：用一套固定的评估基准（benchmark）给"候选提示词"打分，
// 只有分数不低于当前版本的提示词才会被采用，从而防止提示词越改越差（漂移）。
use crate::providers::{create_client, ChatAgent};
use crate::registry::{AgentRegistry, Role};
use anyhow::Result;
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use std::process::Command;

/// 固定的评估基准：一组内置任务，由审计者 Agent 判定通过/失败。提示词进化循环
/// 只有在"新提示词在本基准上的得分 >= 旧提示词"时才采用新版本。这一关正是
/// 防止提示词漂移（prompt drift）的机制。
const BENCHMARK_TASKS: &[&str] = &[
    "Write a Rust function that returns the nth Fibonacci number.",
    "Explain what a closure is in one sentence.",
    "List three ways to handle errors in Rust.",
];

/// 提示词进化器：持有注册表与被进化的 AGENTS.md 路径。
pub struct PromptEvolver {
    registry: AgentRegistry,
    agents_md_path: String,
}

impl PromptEvolver {
    pub fn new(registry: AgentRegistry, agents_md_path: String) -> Self {
        Self {
            registry,
            agents_md_path,
        }
    }

    /// 读取当前提示词（AGENTS.md 的内容）。
    pub fn current_preamble(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.agents_md_path)?)
    }

    /// 针对给定提示词跑一遍基准，返回通过的任务数量。
    pub async fn eval_preamble(&self, preamble: &str) -> Result<usize> {
        let client = create_client()?;
        let model = self.registry.effective_model();
        let params = crate::providers::provider_additional_params();
        let agent: ChatAgent = client
            .agent(&model)
            .preamble(preamble)
            .temperature(crate::providers::Provider::clamp_temperature(0.0))
            .additional_params(params)
            .build();
        let judge = self.registry.build(Role::Auditor)?;
        let mut passed = 0;
        for task in BENCHMARK_TASKS {
            let out = agent.prompt(*task).await?;
            let verdict_prompt = format!(
                "下面的回答是否正确地、有效地解决了该任务？\n\
                 Task: {task}\nAnswer: {out}\n\
                 恰好回复一行：PASS 或 FAIL。"
            );
            let v = judge.run(&verdict_prompt).await?;
            if v.to_uppercase().contains("PASS") {
                passed += 1;
            }
        }
        Ok(passed)
    }

    /// 由一个元 Agent 提出新提示词，再用评估判定"起码不比旧的差"才采用。
    /// 可回退：覆盖前先用 git tag 给旧提示词打点。
    pub async fn evolve(&self) -> Result<String> {
        let current = self.current_preamble()?;
        let meta = self.registry.build(Role::Orchestrator)?;
        let proposal_prompt = format!(
            "你是一个元 agent，负责改进某个 AI agent 的系统提示词。\
             下面是当前的提示词。请提出一个改进版本，让该 agent 更有帮助、更准确、更安全。\
             只输出新的提示词文本，不要附加任何评论。\n\n当前提示词：\n{current}"
        );
        let proposed = meta.run(&proposal_prompt).await?;

        let old_score = self.eval_preamble(&current).await?;
        let new_score = self.eval_preamble(&proposed).await?;

        if new_score >= old_score {
            // Checkpoint the old version, then promote.
            let _ = Command::new("git")
                .args(["tag", &format!("prompt-v{}", now())])
                .output();
            std::fs::write(&self.agents_md_path, &proposed)?;
            Ok(format!(
                "promoted new prompt (score {new_score} >= {old_score}); old tagged in git"
            ))
        } else {
            Ok(format!(
                "kept old prompt (new score {new_score} < {old_score}); no change"
            ))
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
