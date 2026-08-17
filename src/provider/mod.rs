//! Provider 模块 —— 具体的 SandboxProvider 实现。
//!
//! 当前实现:
//! - `LandlockSandbox` (todo 5): Landlock LSM-based 沙箱,作为 bwrap 的 **fallback**
//!   provider(非 co-equal)。bwrap 有 mount namespace + /proc /dev /tmp 隔离,更强;
//!   Landlock 无 mount namespace,仅路径级访问控制。openai/codex 已弃 Landlock 改 bwrap。
//!
//! 与 `SimpleSandbox` (src/sandbox.rs) 的关系:
//! - `SimpleSandbox` 是 bwrap/seatbelt/path 的 provider (todo 4 迁移)。
//! - `LandlockSandbox` 在 bwrap 不可用时兜底,并作 seccomp/network filtering 补充。
//! - `mode = "auto"` 时 bwrap 优先 landlock fallback (todo 8/9 接入选择逻辑)。

pub mod landlock;

pub use landlock::LandlockSandbox;
