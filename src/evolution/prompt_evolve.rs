// 提示词进化模块：用一套固定的评估基准（benchmark）给"候选提示词"打分，
// Prompt evolution module: scores candidate prompts against a fixed benchmark,
// 只有分数不低于当前版本的提示词才会被采用，从而防止提示词越改越差（漂移）。
// only adopts a candidate if its score is >= the current version's, preventing prompt drift.
use crate::event::EventSender;
use crate::providers::{create_client, ChatAgent};
use crate::registry::{AgentRegistry, Role};
use anyhow::Result;
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use std::process::Command;

/// 固定的评估基准：一组内置任务，由审计者 Agent 判定通过/失败。提示词进化循环
/// Fixed evaluation benchmark: a set of built-in tasks judged pass/fail by the Auditor agent. The prompt evolution loop
/// 只有在"新提示词在本基准上的得分 >= 旧提示词"时才采用新版本。这一关正是
/// only adopts a new version when "new prompt's score on this benchmark >= old prompt's score". This gate is exactly
/// 防止提示词漂移（prompt drift）的机制。
/// the mechanism that prevents prompt drift.
const BENCHMARK_TASKS: &[&str] = &[
    "Write a Rust function that returns the nth Fibonacci number.",
    "Explain what a closure is in one sentence.",
    "List three ways to handle errors in Rust.",
];

/// 提示词进化器：持有注册表与被进化的 AGENTS.md 路径。
/// Prompt evolver: holds the registry and the path to the AGENTS.md being evolved.
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
    /// Read the current prompt (contents of AGENTS.md).
    pub fn current_preamble(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.agents_md_path)?)
    }

    /// 针对给定提示词跑一遍基准，返回通过的任务数量。
    /// Run the benchmark against the given prompt, returning the number of passed tasks.
    pub async fn eval_preamble(&self, preamble: &str, tx: &EventSender) -> Result<usize> {
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
            let v = judge.run(&verdict_prompt, tx).await?;
            if v.to_uppercase().contains("PASS") {
                passed += 1;
            }
        }
        Ok(passed)
    }

    /// 由一个元 Agent 提出新提示词，再用评估判定"起码不比旧的差"才采用。
    /// A meta Agent proposes a new prompt, then evaluation decides adoption only if "at least as good as the old one".
    /// 可回退：覆盖前先用 git tag 给旧提示词打点。
    /// Rollbackable: tags the old prompt with a git tag before overwriting.
    pub async fn evolve(&self, tx: &EventSender) -> Result<String> {
        let current = self.current_preamble()?;
        let meta = self.registry.build(Role::Orchestrator)?;
        let proposal_prompt = format!(
            "\u{4f60}\u{662f}\u{4e00}\u{4e2a}\u{5143} agent\u{ff0c}\u{8d1f}\u{8d23}\u{6539}\u{8fdb}\u{67d0}\u{4e2a} AI agent \u{7684}\u{7cfb}\u{7edf}\u{63d0}\u{793a}\u{8bcd}\u{3002}\
             \u{4e0b}\u{9762}\u{662f}\u{5f53}\u{524d}\u{7684}\u{63d0}\u{793a}\u{8bcd}\u{3002}\u{8bf7}\u{63d0}\u{51fa}\u{4e00}\u{4e2a}\u{6539}\u{8fdb}\u{7248}\u{672c}\u{ff0c}\u{8ba9}\u{8be5} agent \u{66f4}\u{6709}\u{5e2e}\u{52a9}\u{3001}\u{66f4}\u{51c6}\u{786e}\u{3001}\u{66f4}\u{5b89}\u{5168}\u{3002}\
             \u{53ea}\u{8f93}\u{51fa}\u{65b0}\u{7684}\u{63d0}\u{793a}\u{8bcd}\u{6587}\u{672c}\u{ff0c}\u{4e0d}\u{8981}\u{9644}\u{52a0}\u{4efb}\u{4f55}\u{8bc4}\u{8bba}\u{3002}\n\n\u{5f53}\u{524d}\u{63d0}\u{793a}\u{8bcd}\u{ff1a}\n{current}"
        );
        let proposed = meta.run(&proposal_prompt, tx).await?;

        let old_score = self.eval_preamble(&current, tx).await?;
        let new_score = self.eval_preamble(&proposed, tx).await?;

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
