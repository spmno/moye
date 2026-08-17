//! Capability Seam traits — the boundary between the agent and concrete providers.
//!
//! 本模块仅定义 trait 与辅助类型,**不实现任何具体 provider**。
//! 具体实现(迁移现有 `Sandbox` / `RunBash` / `ReadFile` 等)在 todo 4 完成;
//! Landlock provider 在 todo 5 完成。
//!
//! 设计原则:
//! - 所有 trait 同步(sync),方法签名与 plan todo 3 字面一致(`-> Output`,
//!   `-> ApprovalVerdict` 等)。如未来需要异步,在 todo 4 迁移时演进。
//! - 所有 trait `Send + Sync` bound,支持 `Box<dyn Trait>` 注入
//!   (todo 4 的 `AgentRegistry::build` 将用 trait 对象替代具体类型)。
//! - 辅助类型遵循 branded-id / value-object 模式:enum 表达封闭变体,
//!   struct 表达领域载荷,字段为已解析的强类型而非裸字符串。
//! - 不引入 `unwrap` / `expect` / `panic!`(`AGENTS.md` 安全规则)。

use std::path::Path;
use std::process::Output;

use serde_json::Value;

// ---------------------------------------------------------------------------
// 辅助类型(Auxiliary types)
// ---------------------------------------------------------------------------

/// 沙箱探测等级——OS 级沙箱可用性的自检结果。
///
/// 由 `SandboxProvider::probe()` 返回,用于:
/// - 在启动时选择后端(`auto` 模式:Full 优先 bwrap,Partial 退到 landlock,
///   Unusable 拒绝执行)。
/// - 在工具调用前向用户暴露当前隔离强度(避免"看起来安全实际无沙箱")。
///
/// 注意:这是**能力等级**而非"是否启用"——`Unusable` 表示没有任何 OS 级
/// 隔离可用,此时调用方应 fail-closed(拒绝执行命令),而非"无沙箱执行"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // infrastructure for future phases
pub enum ProbeLevel {
    /// 全功能 OS 级沙箱可用(bwrap + mount namespace / 完整 Seatbelt 策略)。
    Full,
    /// 部分可用(仅 Landlock / 仅路径检查)——有隔离但弱于 Full。
    Partial,
    /// 无任何 OS 级沙箱可用——调用方应 fail-closed,不执行命令。
    Unusable,
}

/// 工具调用审批请求——传给 `ToolApproval::request()` 的载荷。
///
/// 由三个已解析的强类型字段构成,不传裸 JSON 字符串(符合 parse-don't-validate)。
/// 调用方(工具管线 todo 9)在工具执行前构造此请求,送给注册的审批 listener。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// 工具名(如 `read_file` / `run_bash`)。决定审批策略的分支。
    pub tool_name: String,
    /// 工具参数的已解析 JSON 值。审批方按需取字段,不重复解析。
    pub args: Value,
    /// 发起调用的角色(orchestrator / investigator / planner / builder / auditor)。
    /// 不同角色的同一工具可有不同审批门槛(如 builder 的 edit_file=allow,
    /// auditor 的 edit_file=deny)。
    #[allow(dead_code)] // infrastructure for future phases
    pub role: String,
}

/// 工具调用审批裁决——`ToolApproval::request()` 的返回值。
///
/// 三值封闭变体,所有调用路径必须 exhaustive match(用 `match` 而非 `if/else`)。
/// `Ask` 触发 HITL TUI 提示(todo 10 复用现有 y/n 流程);`Allow` 静默通过;
/// `Deny` 静默拒绝并将原因返回给模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// 允许执行,无需用户介入。
    Allow,
    /// 拒绝执行。模型收到拒绝原因(由调用方包装)。
    Deny,
    /// 需要用户确认。触发 HITL 提示(原有 y/n TUI 流程)。
    Ask,
}

// ---------------------------------------------------------------------------
// Trait 定义
// ---------------------------------------------------------------------------

/// Sandbox seam —— OS 级沙箱能力探测 + 命令前缀构造 + 路径校验。
///
/// 一个 provider 封装一种沙箱后端(bwrap / seatbelt / landlock / path-only)。
/// `AgentRegistry::build()` (todo 4) 将用 `Box<dyn SandboxProvider>` 注入替代
/// 当前的具体 `Sandbox` 类型,使沙箱后端可在配置层切换。
///
/// 方法语义:
/// - `probe()`: 同步自检,**无 I/O**(不真的去 spawn bwrap)。返回能力等级。
/// - `grant_args()`: 给定只读 / 读写路径列表,构造 OS 级沙箱命令前缀 argv。
///   返回 `Some(argv)` 时,工具执行 `argv + [--, sh, -c, <cmd>]`;
///   返回 `None` 时,工具直接执行 `sh -c <cmd>`(无 OS 级沙箱)。
///   `None` 语义与现有 `Sandbox::wrap_command()` 一致(todo 4 一行迁移)。
/// - `check_path()`: 同步路径校验,返回 `true` 表示在沙箱允许范围内。
///   与现有 `Sandbox::check_path()` 的 `Result<(), SandboxError>` 等价:
///   `Ok(())` → `true`,`Err(_)` → `false`(todo 4 用 `.is_ok()` 包装)。
pub trait SandboxProvider: Send + Sync {
    /// 探测当前平台可用的沙箱能力等级(无 I/O,纯环境/能力检查)。
    #[allow(dead_code)] // infrastructure for future phases
    fn probe(&self) -> ProbeLevel;

    /// 给定只读 / 读写路径,构造 OS 级沙箱命令前缀 argv。
    /// 返回 `None` 表示当前后端不需要 / 不支持 OS 级沙箱(直接执行)。
    fn grant_args(&self, read_only: &[String], read_write: &[String]) -> Option<Vec<String>>;

    /// 检查路径是否在沙箱允许范围内。`true` = 允许,`false` = 拒绝。
    #[allow(dead_code)] // infrastructure for future phases
    fn check_path(&self, path: &str) -> bool;
}

/// Shell seam —— shell 命令执行能力。
///
/// 封装 `run_bash` 工具的进程执行逻辑。todo 4 迁移时,现有 `RunBash` 的
/// `tokio::process::Command` 逻辑搬进 `impl ShellExecutor for ...`。
///
/// 方法签名同步(`-> Result<Output>`),与 plan todo 3 字面一致。
/// 内部实现可阻塞(用 `tokio::runtime::Handle::current().block_on(...)`
/// 包装现有 async 逻辑)或重构为同步;todo 4 决定。
#[allow(dead_code)] // infrastructure for future phases
pub trait ShellExecutor: Send + Sync {
    /// 执行 shell 命令,返回进程输出(stdout / stderr / status)。
    /// 失败(超时、spawn 失败、退出码非 0 由调用方判断)返回 `Err`。
    fn run(&self, command: &str) -> anyhow::Result<Output>;
}

/// FileSystem seam —— 文件系统读写能力。
///
/// 封装 `read_file` / `write_file` / `edit_file` 工具的文件操作逻辑。
/// todo 4 迁移时,现有 `ReadFile` / `WriteFile` / `EditFile` 的 `std::fs`
/// 调用搬进 `impl FileSystemProvider for ...`。
///
/// 路径用 `&Path` 而非 `&str`,与新类型在边界处解析的语义一致
/// (调用方负责把 `&str` 转 `Path`,trait 内部不再做字符串解析)。
#[allow(dead_code)] // infrastructure for future phases
pub trait FileSystemProvider: Send + Sync {
    /// 读取文件全部内容。失败(不存在、权限拒绝、不是 UTF-8)返回 `Err`。
    fn read(&self, path: &Path) -> anyhow::Result<String>;

    /// 写入文件(覆盖已存在内容)。失败返回 `Err`。
    fn write(&self, path: &Path, content: &str) -> anyhow::Result<()>;

    /// 编辑文件:将 `old` 片段替换为 `new`。失败(文件不存在、`old` 不匹配、
    /// 多处匹配)返回 `Err`。
    fn edit(&self, path: &Path, old: &str, new: &str) -> anyhow::Result<()>;
}

/// Approval seam —— 工具调用审批门控。
///
/// 封装 `ToolPerms` (registry.rs:46-86) 的 allow/ask/deny 决策。
/// todo 10 升级为可注册 listener 机制;此处先定义 trait 契约。
/// todo 4 迁移时,现有 `ToolPerms` 包装成 `DefaultApproval` impl。
pub trait ToolApproval: Send + Sync {
    /// 请求对一次工具调用的审批裁决。
    /// 调用方(工具管线 todo 9 的 approval 阶段)在工具执行前调用此方法。
    /// 返回 `Deny` 时工具不执行,模型收到拒绝原因(由调用方包装)。
    /// 返回 `Ask` 时触发 HITL TUI 提示(原有 y/n 流程)。
    /// 返回 `Allow` 时静默通过,继续执行。
    fn request(&self, req: &ApprovalRequest) -> ApprovalVerdict;
}

/// LLM seam —— LLM 调用 + 流式响应能力。
///
/// 封装 `providers.rs` 的 client 构造与 SSE 流式调用逻辑。
/// todo 9+ 接入工具管线时使用。本 todo 仅定义契约。
///
/// 方法签名同步,与 plan todo 3 字面一致。todo 9 演进时若需真正流式
/// 返回 `BoxStream`,在 trait 上加方法或修改返回类型即可(本 todo 不实现,
/// 签名演进风险可控)。
#[allow(dead_code)] // infrastructure for future phases
pub trait LlmAdapter: Send + Sync {
    /// 流式调用 LLM。`prompt` 为用户/系统输入。
    /// 当前签名返回 `Result<()>`(触发流后由内部状态接收 token);
    /// todo 9 演进为返回流式句柄。
    fn stream(&self, prompt: &str) -> anyhow::Result<()>;

    /// 准备 LLM 调用(设置 model / params / provider 切换)。
    /// 在 `stream()` 之前调用,确保 client 就绪。
    fn prepare_call(&self) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Mock 实现 —— 编译验证(每个 trait 至少一个 impl,在 #[cfg(test)] 下编译)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mocks {
    //! 每个 trait 一个 mock impl,仅用于证明 trait 定义可被实现(`cargo test --no-run`
    //! 编译这些 impl)。无运行时行为;返回值是 stub。
    //!
    //! **Failure proof**:在此 mod 内删除任一 mock 的任一方法,`cargo test --no-run`
    //! 会因 "not all trait items implemented" 编译失败——证明 trait 真的被 enforced。

    use super::*;
    use std::path::Path;
    use std::process::Output;

    /// `SandboxProvider` 的 mock——所有方法返回最弱结果。
    pub struct MockSandbox;

    impl SandboxProvider for MockSandbox {
        fn probe(&self) -> ProbeLevel {
            ProbeLevel::Unusable
        }

        fn grant_args(&self, _read_only: &[String], _read_write: &[String]) -> Option<Vec<String>> {
            None
        }

        fn check_path(&self, _path: &str) -> bool {
            true
        }
    }

    /// `ShellExecutor` 的 mock——`run` 返回空输出。
    pub struct MockShell;

    impl ShellExecutor for MockShell {
        fn run(&self, _command: &str) -> anyhow::Result<Output> {
            Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    /// `FileSystemProvider` 的 mock——`read` 返空串,`write`/`edit` 空操作。
    pub struct MockFs;

    impl FileSystemProvider for MockFs {
        fn read(&self, _path: &Path) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn write(&self, _path: &Path, _content: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn edit(&self, _path: &Path, _old: &str, _new: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// `ToolApproval` 的 mock——全部 `Allow`(不阻塞任何调用)。
    pub struct MockApproval;

    impl ToolApproval for MockApproval {
        fn request(&self, _req: &ApprovalRequest) -> ApprovalVerdict {
            ApprovalVerdict::Allow
        }
    }

    /// `LlmAdapter` 的 mock——`stream` / `prepare_call` 空操作。
    pub struct MockLlm;

    impl LlmAdapter for MockLlm {
        fn stream(&self, _prompt: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn prepare_call(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // --- 编译验证测试:每个 mock 实例化 + 调用一次,确保 trait 可被实现且可被调用 ---

    #[test]
    fn mock_sandbox_compiles_and_runs() {
        let sb = MockSandbox;
        assert_eq!(sb.probe(), ProbeLevel::Unusable);
        assert!(sb.grant_args(&[], &[]).is_none());
        assert!(sb.check_path("anywhere"));
    }

    #[test]
    fn mock_shell_compiles_and_runs() {
        let sh = MockShell;
        let out = sh.run("echo hi").expect("mock run never fails");
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn mock_fs_compiles_and_runs() {
        let fs = MockFs;
        assert_eq!(fs.read(Path::new("/dev/null")).expect("mock read"), "");
        fs.write(Path::new("/dev/null"), "").expect("mock write");
        fs.edit(Path::new("/dev/null"), "a", "b")
            .expect("mock edit");
    }

    #[test]
    fn mock_approval_compiles_and_runs() {
        let ap = MockApproval;
        let req = ApprovalRequest {
            tool_name: "read_file".into(),
            args: serde_json::json!({"path": "src/main.rs"}),
            role: "builder".into(),
        };
        assert_eq!(ap.request(&req), ApprovalVerdict::Allow);
    }

    #[test]
    fn mock_llm_compiles_and_runs() {
        let llm = MockLlm;
        llm.prepare_call().expect("mock prepare_call");
        llm.stream("hello").expect("mock stream");
    }
}

// ---------------------------------------------------------------------------
// 辅助类型单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod aux_tests {
    use super::*;

    /// `ProbeLevel` 的三个变体互不相等,且可 Copy(避免所有权传递开销)。
    #[test]
    fn probe_level_variants_distinct_and_copy() {
        let full = ProbeLevel::Full;
        let partial = ProbeLevel::Partial;
        let unusable = ProbeLevel::Unusable;
        assert_ne!(full, partial);
        assert_ne!(partial, unusable);
        assert_ne!(full, unusable);
        // Copy 检查:赋值后原值仍可用
        let copied = full;
        assert_eq!(full, copied);
    }

    /// `ApprovalVerdict` 的三个变体互不相等,且可 Copy。
    #[test]
    fn approval_verdict_variants_distinct_and_copy() {
        let allow = ApprovalVerdict::Allow;
        let deny = ApprovalVerdict::Deny;
        let ask = ApprovalVerdict::Ask;
        assert_ne!(allow, deny);
        assert_ne!(deny, ask);
        assert_ne!(allow, ask);
        let copied = allow;
        assert_eq!(allow, copied);
    }

    /// `ApprovalRequest` 可 Clone,字段可读。
    #[test]
    fn approval_request_clone_and_fields() {
        let req = ApprovalRequest {
            tool_name: "run_bash".into(),
            args: serde_json::json!({"command": "ls"}),
            role: "orchestrator".into(),
        };
        let cloned = req.clone();
        assert_eq!(req.tool_name, cloned.tool_name);
        assert_eq!(req.role, cloned.role);
        assert_eq!(req.args, cloned.args);
    }
}
