//! Capability Seam 模块 —— agent 与具体 provider 之间的边界。
//!
//! 本模块仅定义 trait 与辅助类型,**不实现任何具体 provider**。
//! 具体 provider 实现(迁移现有 `Sandbox` / `RunBash` / `ReadFile` 等)在
//! todo 4 完成;Landlock provider 在 todo 5 完成。
//!
//! 设计目标(来自 plan dsh-borrow-refactor.md todo 3):
//! - 单一 crate 内模块化(不拆 multi-crate)。
//! - 所有 trait `Send + Sync`,支持 `Box<dyn Trait>` 注入
//!   (todo 4 `AgentRegistry::build` 用 trait 对象替代具体类型)。
//! - 方法签名与 plan todo 3 字面一致(`-> Output` / `-> ApprovalVerdict` 等),
//!   同步 trait(如未来需要异步,在 todo 4 迁移时演进)。
//! - 不引入 `unwrap` / `expect` / `panic!`(`AGENTS.md` 安全规则)。
//!
//! 不在此 todo 实现 provider(只定义 trait);不改 src/sandbox.rs / src/tools.rs
//! (在 todo 4 迁移)。

pub mod traits;

// 重导出 trait 与辅助类型,使外部调用方可直接 `use crate::seam::SandboxProvider`
// 而非 `use crate::seam::traits::SandboxProvider`。
pub use traits::{
    ApprovalRequest, ApprovalVerdict, FileSystemProvider, ProbeLevel, SandboxProvider,
    ShellExecutor, ToolApproval,
};
