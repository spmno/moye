// 两阶段评审门（ReviewGate）。被 Orchestrator 在 SDD 管线中调用：
// Two-stage review gate (ReviewGate). Called by the Orchestrator in the SDD pipeline:
// Builder 产出后，Auditor 先做规格符合性评审，再做代码质量评审。
// After Builder produces, the Auditor first does spec compliance review, then code quality review.
// 两者都 APPROVE 才算通过，否则带反馈退回。
// Both must APPROVE to pass; otherwise returned with feedback.

use crate::event::EventSender;
use crate::registry::{AgentRegistry, Role};

/// 评审结论：通过 / 驳回（附反馈）/ 需要澄清（附问题）。
/// Review verdict: Approve / Reject (with feedback) / Clarify (with question).
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Reject(String), // 返回给构建者的反馈
    // Feedback returned to the builder
    Clarify(String),
}

/// SDD 两阶段评审门。遵循 OMO 的纪律：在审计者通过两个阶段之前，任务不算完成：
/// SDD two-stage review gate. Follows OMO discipline: a task is not complete until the Auditor passes both stages:
///   1. 规格符合性 —— 是否实现了所要求的内容？
///   1. Spec compliance — does it implement what was required?
///   2. 代码质量   —— 安全性、正确性、可维护性。
///   2. Code quality   — security, correctness, maintainability.
/// 两者都必须 Approve，否则带着反馈退回。
/// Both must Approve; otherwise returned with feedback.
pub struct ReviewGate {
    registry: AgentRegistry,
}

impl ReviewGate {
    /// 构造评审门（持有 registry 以构建审计者 Agent）。
    /// Constructs the review gate (holds registry to build the Auditor Agent).
    pub fn new(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    /// 对产物执行两阶段评审，返回最终结论。
    /// Executes two-stage review on the produced work, returns the final verdict.
    pub async fn review(&self, task: &str, produced: &str, tx: &EventSender) -> anyhow::Result<Verdict> {
        let auditor = self.registry.build(Role::Auditor)?;

        let spec_prompt = format!(
            "\u{4f60}\u{662f}\u{89c4}\u{683c}\u{7b26}\u{5408}\u{6027}\u{8bc4}\u{5ba1}\u{5458}\u{ff08}\u{7b2c} 1/2 \u{9636}\u{6bb5}\u{ff09}\u{3002}\
             \u{8bf7}\u{6c42}\u{7684}\u{4efb}\u{52a1}\u{ff1a}\n{task}\n\n\u{4ea7}\u{51fa}\u{7684}\u{5de5}\u{4f5c}\u{ff1a}\n{produced}\n\n\
             \u{8be5}\u{5de5}\u{4f5c}\u{662f}\u{5426}\u{5b9e}\u{73b0}\u{4e86}\u{6240}\u{8981}\u{6c42}\u{7684}\u{5185}\u{5bb9}\u{ff1f}\u{6070}\u{597d}\u{56de}\u{590d}\u{4e00}\u{884c}\u{ff1a}\
             'APPROVE' \u{6216} 'REJECT: <\u{7f3a}\u{5931}\u{6216}\u{9519}\u{8bef}\u{4e4b}\u{5904}>'\u{3002}"
        );
        let spec_out = auditor.run(&spec_prompt, tx).await?;

        if !spec_out.to_uppercase().contains("APPROVE") {
            let fb = spec_out
                .lines()
                .find(|l| l.to_uppercase().contains("REJECT"))
                .unwrap_or("spec compliance failed")
                .to_string();
            return Ok(Verdict::Reject(fb));
        }

        let qual_prompt = format!(
            "\u{4f60}\u{662f}\u{4ee3}\u{7801}\u{8d28}\u{91cf}\u{8bc4}\u{5ba1}\u{5458}\u{ff08}\u{7b2c} 2/2 \u{9636}\u{6bb5}\u{ff09}\u{3002}\
             \u{8bf7}\u{6c42}\u{7684}\u{4efb}\u{52a1}\u{ff1a}\n{task}\n\n\u{4ea7}\u{51fa}\u{7684}\u{5de5}\u{4f5c}\u{ff1a}\n{produced}\n\n\
             \u{68c0}\u{67e5}\u{5b89}\u{5168}\u{6027}\u{3001}\u{6b63}\u{786e}\u{6027}\u{4e0e}\u{53ef}\u{7ef4}\u{62a4}\u{6027}\u{3002}\u{6070}\u{597d}\u{56de}\u{590d}\u{4e00}\u{884c}\u{ff1a}\
             'APPROVE' \u{6216} 'REJECT: <\u{95ee}\u{9898}>' \u{6216} 'CLARIFY: <\u{7591}\u{95ee}>'\u{3002}"
        );
        let qual_out = auditor.run(&qual_prompt, tx).await?;
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
