// 两阶段评审门（ReviewGate）。被 Orchestrator 在 SDD 管线中调用：
// Builder 产出后，Auditor 先做规格符合性评审，再做代码质量评审。
// 两者都 APPROVE 才算通过，否则带反馈退回。

use crate::registry::{AgentRegistry, Role};

/// 评审结论：通过 / 驳回（附反馈）/ 需要澄清（附问题）。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reject(String), // 返回给构建者的反馈
    Clarify(String),
}

/// SDD 两阶段评审门。遵循 OMO 的纪律：在审计者通过两个阶段之前，任务不算完成：
///   1. 规格符合性 —— 是否实现了所要求的内容？
///   2. 代码质量   —— 安全性、正确性、可维护性。
/// 两者都必须 Approve，否则带着反馈退回。
pub struct ReviewGate {
    registry: AgentRegistry,
}

impl ReviewGate {
    /// 构造评审门（持有 registry 以构建审计者 Agent）。
    pub fn new(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    /// 对产物执行两阶段评审，返回最终结论。
    pub async fn review(&self, task: &str, produced: &str) -> anyhow::Result<Verdict> {
        let auditor = self.registry.build(Role::Auditor)?;

        let spec_prompt = format!(
            "你是规格符合性评审员（第 1/2 阶段）。\
             请求的任务：\n{task}\n\n产出的工作：\n{produced}\n\n\
             该工作是否实现了所要求的内容？恰好回复一行：\
             'APPROVE' 或 'REJECT: <缺失或错误之处>'。"
        );
        let spec_out = auditor.run(&spec_prompt).await?;

        if !spec_out.to_uppercase().contains("APPROVE") {
            let fb = spec_out
                .lines()
                .find(|l| l.to_uppercase().contains("REJECT"))
                .unwrap_or("spec compliance failed")
                .to_string();
            return Ok(Verdict::Reject(fb));
        }

        let qual_prompt = format!(
            "你是代码质量评审员（第 2/2 阶段）。\
             请求的任务：\n{task}\n\n产出的工作：\n{produced}\n\n\
             检查安全性、正确性与可维护性。恰好回复一行：\
             'APPROVE' 或 'REJECT: <问题>' 或 'CLARIFY: <疑问>'。"
        );
        let qual_out = auditor.run(&qual_prompt).await?;
        let up = qual_out.to_uppercase();
        if up.contains("APPROVE") {
            Ok(Verdict::Approve)
        } else if up.contains("CLARIFY") {
            Ok(Verdict::Clarify(
                qual_out.lines().find(|l| l.to_uppercase().contains("CLARIFY")).unwrap_or("").to_string(),
            ))
        } else {
            Ok(Verdict::Reject(
                qual_out.lines().find(|l| l.to_uppercase().contains("REJECT")).unwrap_or("quality failed").to_string(),
            ))
        }
    }
}
