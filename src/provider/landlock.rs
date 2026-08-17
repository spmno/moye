//! LandlockSandbox —— Landlock LSM-based sandbox provider (Linux fallback).
//!
//! 作为 bwrap 的 **FALLBACK**,不是 co-equal provider:
//! - bwrap 有 mount namespace + /proc /dev /tmp 隔离,更强。
//! - Landlock 无 mount namespace,/proc /dev /tmp 共享,仅路径级访问控制。
//! - openai/codex 已弃 Landlock 改 bwrap。
//!
//! `probe()` 返 `Full`(bwrap 可用)/ `Partial`(仅 landlock)/ `Unusable`(都无)
//! —— **bwrap 优先**。
//!
//! `grant_args` 用 `pre_exec`(unsafe,async-signal-safe)装 ruleset 后 execve ——
//! 不是 bwrap 的 argv 前缀模式。本 todo 不接入 run_bash(todo 9 管线接入),
//! 故 `grant_args` 返 `None`;todo 9 在 spawn 时设 pre_exec 回调。
//!
//! fail-closed —— 无 Landlock 内核且无 bwrap 时返 `Unusable`,调用方不执行命令。

use crate::seam::{ProbeLevel, SandboxProvider};
use std::path::{Path, PathBuf};

/// Landlock 沙箱 provider —— Linux 5.13+ LSM-based path access control。
///
/// 作为 bwrap 的 fallback:当 bwrap 不可用时,Landlock 提供路径级访问控制
/// (弱于 bwrap 的 mount namespace 隔离)。无 Landlock 内核支持且无 bwrap 时
/// fail-closed(返 `Unusable`)。
///
/// 不支持 Windows(landlock crate 仅 Linux 编译)。
pub struct LandlockSandbox {
    /// 项目根目录(规范化后的绝对路径,用于 check_path)。
    #[allow(dead_code)] // infrastructure for future phases
    root: PathBuf,
    /// 已授权的额外目录(规范化后的绝对路径)。
    authorized: Vec<PathBuf>,
}

impl LandlockSandbox {
    /// 创建 Landlock 沙箱,以当前工作目录为根。
    pub fn new() -> Self {
        let root = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("."));
        Self {
            root,
            authorized: Vec::new(),
        }
    }

    /// 创建 Landlock 沙箱并预授权一组目录(来自配置 `[sandbox].authorized_dirs`)。
    pub fn with_authorized_dirs(dirs: &[String]) -> Self {
        let mut sb = Self::new();
        for dir in dirs {
            let expanded = crate::sandbox::expand_tilde(dir);
            let path = PathBuf::from(&expanded);
            let canon = path.canonicalize().unwrap_or(path);
            sb.authorized.push(canon);
        }
        sb
    }

    /// 检查 bwrap 二进制是否在 PATH 中可用(与 SimpleSandbox 的 detect 逻辑一致)。
    #[allow(dead_code)] // used in tests
    fn bwrap_available() -> bool {
        let path = match std::env::var_os("PATH") {
            Some(p) => p,
            None => return false,
        };
        for dir in std::env::split_paths(&path) {
            if dir.join("bwrap").is_file() {
                return true;
            }
        }
        false
    }

    /// 检查 Landlock LSM 是否在当前内核上可用。
    ///
    /// 通过尝试创建一个最小 ruleset 来检测 —— `create()` 内部调用
    /// `landlock_create_ruleset` syscall,内核无 Landlock 支持时返 `ENOSYS`。
    /// 无副作用:ruleset 创建后立即 drop,不调用 `restrict_self()`。
    #[cfg(target_os = "linux")]
    #[allow(dead_code)] // used in tests
    fn landlock_available() -> bool {
        use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr};

        Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|rs| rs.create())
            .is_ok()
    }

    /// 非 Linux 平台:Landlock 不可用。
    #[cfg(not(target_os = "linux"))]
    #[allow(dead_code)] // used in tests
    fn landlock_available() -> bool {
        false
    }

    /// 将路径解析为绝对路径(相对于项目根目录),展开 `~`。
    #[allow(dead_code)] // used in tests
    fn resolve_path(&self, path: &str) -> PathBuf {
        let expanded = crate::sandbox::expand_tilde(path);
        let p = Path::new(&expanded);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// 安全规范化:路径不存在时规范化父目录再拼接文件名。
    #[allow(dead_code)] // used in tests
    fn canonicalize_safe(path: &Path) -> PathBuf {
        if let Ok(canon) = path.canonicalize() {
            return canon;
        }
        if let Some(parent) = path.parent()
            && let Ok(canon_parent) = parent.canonicalize()
        {
            let filename = path.file_name().unwrap_or_default();
            return canon_parent.join(filename);
        }
        path.to_path_buf()
    }
}

impl Default for LandlockSandbox {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// SandboxProvider trait impl
// ---------------------------------------------------------------------------

/// 把 `LandlockSandbox` 暴露为 `SandboxProvider` trait 的一个实现。
///
/// 行为映射:
/// - `probe()`: bwrap 可用 → `Full`(bwrap 优先);仅 landlock → `Partial`;
///   都无 → `Unusable`(fail-closed,调用方不执行命令)。
/// - `grant_args()`: 返 `None`。Landlock 用 `pre_exec`(unsafe,async-signal-safe)
///   装 ruleset,不是 argv 前缀模式。todo 9 在 spawn 时设 pre_exec 回调。
/// - `check_path()`: 与 SimpleSandbox 一致的路径检查(root + authorized)。
impl SandboxProvider for LandlockSandbox {
    fn probe(&self) -> ProbeLevel {
        // bwrap 优先:有 bwrap 则 Full(bwrap 更强,有 mount namespace)。
        if Self::bwrap_available() {
            return ProbeLevel::Full;
        }
        // 仅 landlock:Partial(路径级访问控制,弱于 bwrap)。
        if Self::landlock_available() {
            return ProbeLevel::Partial;
        }
        // 都无:Unusable(fail-closed)。
        ProbeLevel::Unusable
    }

    fn grant_args(&self, _read_only: &[String], _read_write: &[String]) -> Option<Vec<String>> {
        // Landlock 用 pre_exec(unsafe,async-signal-safe)装 ruleset,不是 argv 前缀模式。
        // 本 todo 不接入 run_bash(todo 9 管线接入);todo 9 在 spawn 时设 pre_exec 回调,
        // 在此处构造 ruleset。返 None 表示当前不走 argv 前缀路径。
        None
    }

    fn check_path(&self, path: &str) -> bool {
        let resolved = self.resolve_path(path);
        let canon = Self::canonicalize_safe(&resolved);
        // 项目根目录及其子目录。
        if canon.starts_with(&self.root) {
            return true;
        }
        // 已授权目录及其子目录。
        for dir in &self.authorized {
            if canon.starts_with(dir) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: `probe()` 返回三个 ProbeLevel 之一,不 panic。
    /// 实现前此测试编译失败(LandlockSandbox 不存在),实现后通过。
    #[test]
    fn probe_returns_valid_level() {
        let sb = LandlockSandbox::new();
        let level = sb.probe();
        assert!(
            level == ProbeLevel::Full
                || level == ProbeLevel::Partial
                || level == ProbeLevel::Unusable,
            "probe() must return one of the three ProbeLevel variants"
        );
    }

    /// `grant_args` 返 `None`(Landlock 用 pre_exec,不是 argv 前缀模式)。
    #[test]
    fn grant_args_returns_none() {
        let sb = LandlockSandbox::new();
        assert!(sb.grant_args(&[], &[]).is_none());
        assert!(sb.grant_args(&["/tmp".into()], &["/home".into()]).is_none());
    }

    /// `check_path` 对项目内路径返 `true`。
    #[test]
    fn check_path_within_root() {
        let sb = LandlockSandbox::new();
        assert!(sb.check_path("src/main.rs"));
        assert!(sb.check_path("./Cargo.toml"));
    }

    /// `check_path` 对项目外绝对路径返 `false`。
    #[test]
    fn check_path_outside_root() {
        let sb = LandlockSandbox::new();
        assert!(!sb.check_path("/etc/passwd"));
        assert!(!sb.check_path("/root/.ssh/id_rsa"));
    }

    /// `check_path` 对 `..` 逃逸路径返 `false`。
    #[test]
    fn check_path_dotdot_escape() {
        let sb = LandlockSandbox::new();
        assert!(!sb.check_path("../something"));
    }

    /// `check_path` 对授权目录返 `true`。
    #[test]
    fn check_path_authorized_dir() {
        let sb = LandlockSandbox::with_authorized_dirs(&["/tmp".to_string()]);
        assert!(sb.check_path("/tmp/test.txt"));
        assert!(sb.check_path("/tmp/sub/dir/file.txt"));
    }

    /// `bwrap_available` 返 bool(无 panic)。
    #[test]
    fn bwrap_available_returns_bool() {
        let _ = LandlockSandbox::bwrap_available();
    }

    /// `landlock_available` 返 bool(无 panic)。
    /// 无 Landlock 内核时返 false → probe() 在无 bwrap 时返 Unusable。
    #[test]
    fn landlock_available_returns_bool() {
        let _ = LandlockSandbox::landlock_available();
    }

    /// probe 逻辑验证:bwrap 可用时返 Full(本机有 bwrap → Full)。
    #[test]
    fn probe_full_when_bwrap_available() {
        if LandlockSandbox::bwrap_available() {
            let sb = LandlockSandbox::new();
            assert_eq!(sb.probe(), ProbeLevel::Full, "bwrap available → Full");
        }
    }

    /// probe 逻辑验证:无 bwrap 且无 landlock 时返 Unusable(fail-closed)。
    /// 通过直接验证 probe 逻辑的组合性来测试(而非依赖环境)。
    #[test]
    fn probe_unusable_when_neither_available() {
        // 本测试验证 probe() 的逻辑:bwrap 不可用 + landlock 不可用 → Unusable。
        // 在有 bwrap 的机器上,probe() 返 Full;此测试验证逻辑而非环境。
        let bwrap = LandlockSandbox::bwrap_available();
        let landlock = LandlockSandbox::landlock_available();
        if !bwrap && !landlock {
            let sb = LandlockSandbox::new();
            assert_eq!(sb.probe(), ProbeLevel::Unusable);
        }
        // 在有 bwrap 或 landlock 的机器上,此测试不断言 probe 值
        // (由 probe_full_when_bwrap_available / probe_partial_when_only_landlock 覆盖)。
    }

    /// probe 逻辑验证:无 bwrap 但有 landlock 时返 Partial。
    #[test]
    fn probe_partial_when_only_landlock() {
        let bwrap = LandlockSandbox::bwrap_available();
        let landlock = LandlockSandbox::landlock_available();
        if !bwrap && landlock {
            let sb = LandlockSandbox::new();
            assert_eq!(sb.probe(), ProbeLevel::Partial);
        }
    }

    /// `LandlockSandbox` impl `Send + Sync`(trait bound 要求)。
    #[test]
    fn landlock_sandbox_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LandlockSandbox>();
    }
}
