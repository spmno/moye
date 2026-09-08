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
//! ## Enforcement (已接入)
//!
//! `pre_exec_landlock()` 在 spawn 时构造 ruleset 并返回一个闭包,
//! 闭包在 fork 后 execve 前调用 `prctl(PR_SET_NO_NEW_PRIVS)` +
//! `landlock_restrict_self(ruleset_fd, 0)` 两个系统调用,将预构造的
//! ruleset 装到子进程。ruleset 策略:
//! - 系统路径(`/usr` `/lib` `/lib64` `/bin` `/sbin` 等)只读 + 执行
//!   ——子进程必须能加载 libc 和 exec。
//! - 项目根 + 授权目录:全权读写。
//! - 其他路径:默认拒绝(fail-closed)。
//!
//! `grant_args()` 仍返 `None`(Landlock 不走 argv 前缀模式,走 pre_exec)。
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

    /// System base paths granted read+execute access — the minimal set a
    /// child needs to load libc, resolve dynamic linker paths, and exec
    /// binaries. Each entry justified inline.
    #[cfg(target_os = "linux")]
    fn system_base_paths() -> &'static [&'static str] {
        &[
            "/usr",             // system binaries + shared libs (libc, gcc, coreutils)
            "/lib",             // shared libraries (libc, ld-linux) on multi-arch
            "/lib64",           // 64-bit shared libraries on bi-arch systems
            "/bin",             // essential user binaries (sh, ls, cat, ...)
            "/sbin",            // system administration binaries
            "/etc/ld.so.cache", // dynamic linker cache (resolves lib paths at exec)
            "/etc/ld.so.conf",  // dynamic linker config
            "/etc/ld.so.conf.d", // dynamic linker include dir
            "/etc/resolv.conf", // DNS resolution (glibc resolver)
            "/etc/hosts",       // hostname resolution (fallback before DNS)
            "/etc/ssl",         // CA certificates for HTTPS (reqwest/TLS)
        ]
    }

    /// System paths that need full read+write+execute access.
    #[cfg(target_os = "linux")]
    fn system_full_paths() -> &'static [&'static str] {
        &[
            "/dev/null",    // common sink for shell redirects (2>/dev/null, >/dev/null)
            "/dev/urandom", // crypto RNG (TLS, UUID, hash, etc.)
            "/tmp",         // temp dir (shell pipelines, mktemp, cargo build, etc.)
        ]
    }

    /// Build the full Landlock ruleset (create + add all rules) and return
    /// the ruleset's `OwnedFd`. The fd is kept alive by the returned closure
    /// (moved into it); only the final `landlock_restrict_self` syscall
    /// happens inside the closure.
    /// 构造完整 ruleset(所有规则在此添加),返回 ruleset fd。
    #[cfg(target_os = "linux")]
    fn build_ruleset(&self) -> Option<std::os::unix::io::OwnedFd> {
        use landlock::{
            path_beneath_rules, ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset,
            RulesetAttr, RulesetCreatedAttr,
        };

        let abi = ABI::V1;
        let exec_only = AccessFs::Execute;
        let read_exec = AccessFs::from_read(abi);
        let full = AccessFs::from_all(abi);

        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(full)
            .ok()?;

        let created = ruleset.create().ok()?;

        // Grant Execute on "/" so the child can traverse the root directory
        // to reach allowed paths (e.g. /usr/bin/sh). Execute-only means the
        // process can walk through / but cannot read or write anything there
        // unless another rule explicitly grants it.
        let created = created
            .set_compatibility(CompatLevel::BestEffort)
            .add_rules(path_beneath_rules(std::iter::once("/"), exec_only))
            .ok()?;

        let created = created
            .set_compatibility(CompatLevel::BestEffort)
            .add_rules(path_beneath_rules(Self::system_full_paths(), full))
            .ok()?;

        let created = created
            .set_compatibility(CompatLevel::BestEffort)
            .add_rules(path_beneath_rules(Self::system_base_paths(), read_exec))
            .ok()?;

        let root_path = vec![self.root.as_path()];
        let created = created
            .set_compatibility(CompatLevel::BestEffort)
            .add_rules(path_beneath_rules(root_path.into_iter(), full))
            .ok()?;

        let auth_paths: Vec<&Path> = self.authorized.iter().map(|p| p.as_path()).collect();
        let created = created
            .set_compatibility(CompatLevel::BestEffort)
            .add_rules(path_beneath_rules(auth_paths.into_iter(), full))
            .ok()?;

        let owned_fd: Option<std::os::unix::io::OwnedFd> = created.into();
        owned_fd
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
/// - `grant_args()`: 返 `None`。Landlock 用 `pre_exec` 装 ruleset,
///   不是 argv 前缀模式;实际强制在 `pre_exec_landlock()` 中完成。
/// - `pre_exec_landlock()`: 构造 ruleset 并返回闭包,闭包在 fork 后
///   execve 前调用 `prctl` + `landlock_restrict_self`。
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
        // Landlock uses pre_exec (not argv prefix). Enforcement is wired in
        // pre_exec_landlock() which returns a closure calling prctl +
        // landlock_restrict_self in the forked child.
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

    /// Landlock 的 authorized 列表在启动时一次性构造(`with_authorized_dirs`),
    /// 不支持运行时动态扩展——因此版本恒为 0。后续 task 补齐 Landlock 的
    /// 动态授权能力时再实现递增逻辑。
    /// Landlock's authorized list is built once at startup
    /// (`with_authorized_dirs`); it does not support runtime widening —
    /// so the version is always 0. The follow-up task that adds Landlock's
    /// dynamic authorization enforcement will implement the increment here.
    fn auth_version(&self) -> u64 {
        0
    }

    #[cfg(target_os = "linux")]
    fn pre_exec_landlock(&self) -> Option<Box<dyn Fn() -> Result<(), String> + Send + Sync + 'static>> {
        use std::os::unix::io::AsRawFd;

        let owned_fd = self.build_ruleset()?;
        let raw_fd = owned_fd.as_raw_fd();

        Some(Box::new(move || {
            // Force the closure to capture owned_fd so the ruleset fd stays
            // open until the child calls landlock_restrict_self.
            let _ = &owned_fd;
            // SAFETY: runs in a forked child before execve. Success path
            // makes only two syscalls (prctl + landlock_restrict_self) — no
            // allocation, no locks. Error-path format! is fine (child won't exec).
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    let err = std::io::Error::last_os_error();
                    return Err(format!(
                        "landlock pre_exec: prctl(PR_SET_NO_NEW_PRIVS) failed: {err}"
                    ));
                }
                let ret = libc::syscall(libc::SYS_landlock_restrict_self, raw_fd, 0u32);
                if ret != 0 {
                    let err = std::io::Error::last_os_error();
                    return Err(format!(
                        "landlock pre_exec: landlock_restrict_self failed: {err}"
                    ));
                }
            }
            Ok(())
        }))
    }

    #[cfg(not(target_os = "linux"))]
    fn pre_exec_landlock(&self) -> Option<Box<dyn Fn() -> Result<(), String> + Send + Sync + 'static>> {
        None
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

    /// System base path list contains the essential entries needed for a
    /// child to exec binaries and load shared libraries.
    #[cfg(target_os = "linux")]
    #[test]
    fn system_base_paths_contains_expected_entries() {
        let paths = LandlockSandbox::system_base_paths();
        assert!(!paths.is_empty());
        assert!(paths.contains(&"/usr"));
        assert!(paths.contains(&"/lib"));
        assert!(paths.contains(&"/bin"));
        assert!(paths.contains(&"/etc/ld.so.cache"));
        assert!(paths.contains(&"/etc/ssl"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_full_paths_contains_expected_entries() {
        let paths = LandlockSandbox::system_full_paths();
        assert!(paths.contains(&"/dev/null"));
        assert!(paths.contains(&"/dev/urandom"));
        assert!(paths.contains(&"/tmp"));
    }

    /// `build_ruleset` returns `Some(fd)` on Landlock-capable kernels,
    /// `None` otherwise. Verifies that ruleset construction (create + all
    /// rules for system base + project root + authorized dirs) succeeds.
    #[cfg(target_os = "linux")]
    #[test]
    fn build_ruleset_succeeds_on_landlock_kernel() {
        if !LandlockSandbox::landlock_available() {
            eprintln!("[skip] Landlock not available on this kernel");
            return;
        }
        let sb = LandlockSandbox::new();
        assert!(
            sb.build_ruleset().is_some(),
            "build_ruleset should succeed on a Landlock-capable kernel"
        );
    }

    /// `build_ruleset` includes authorized dirs in the ruleset.
    #[cfg(target_os = "linux")]
    #[test]
    fn build_ruleset_with_authorized_dirs() {
        if !LandlockSandbox::landlock_available() {
            eprintln!("[skip] Landlock not available on this kernel");
            return;
        }
        let sb = LandlockSandbox::with_authorized_dirs(&["/tmp".to_string()]);
        assert!(
            sb.build_ruleset().is_some(),
            "build_ruleset should succeed with authorized dirs"
        );
    }

    /// `pre_exec_landlock` returns `Some(closure)` on Landlock-capable kernels.
    /// The closure's `Send + Sync` bound is enforced by the trait signature.
    #[cfg(target_os = "linux")]
    #[test]
    fn pre_exec_landlock_returns_closure_on_landlock_kernel() {
        if !LandlockSandbox::landlock_available() {
            eprintln!("[skip] Landlock not available on this kernel");
            return;
        }
        let sb = LandlockSandbox::new();
        let closure = sb.pre_exec_landlock();
        assert!(closure.is_some(), "pre_exec_landlock should return Some");
    }

    /// Integration test: a child spawned with the Landlock pre_exec hook
    /// can read files inside the project root (allowed) but cannot write
    /// to paths outside the allowed set (denied with EACCES).
    ///
    /// Skipped gracefully when Landlock is not available on the running kernel.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_enforcement_integration() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        if !LandlockSandbox::landlock_available() {
            eprintln!("[skip] Landlock not available on this kernel");
            return;
        }

        let sb = LandlockSandbox::new();
        let pre_exec_fn = match sb.pre_exec_landlock() {
            Some(f) => f,
            None => {
                eprintln!("[skip] pre_exec_landlock returned None (ruleset build failed)");
                return;
            }
        };

        let project_root = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("."));
        let cargo_toml = project_root.join("Cargo.toml");
        let denied_path = "/etc/moye_landlock_test";

        let script = format!(
            "cat '{}' >/dev/null 2>&1 && read_ok=1 || read_ok=0; \
             touch '{}' 2>/dev/null && write_ok=1 || write_ok=0; \
             printf 'read_ok=%s write_ok=%s' \"$read_ok\" \"$write_ok\"",
            cargo_toml.display(),
            denied_path
        );

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script);

        // SAFETY: The pre_exec closure makes only two syscalls on the
        // success path (prctl + landlock_restrict_self). No allocation,
        // no locks. The OwnedFd keeping the ruleset fd alive is moved
        // into the closure and remains valid until exec or exit.
        unsafe {
            cmd.pre_exec(move || pre_exec_fn().map_err(std::io::Error::other));
        }

        let output = cmd.output().expect("spawn child with landlock pre_exec");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let stdout_trim = stdout.trim();
        eprintln!("[landlock test] stdout: {stdout_trim}");
        if !stderr.is_empty() {
            eprintln!("[landlock test] stderr: {}", stderr.trim());
        }

        assert!(
            stdout_trim.contains("read_ok=1"),
            "reading Cargo.toml in project root should be allowed: {stdout_trim}"
        );
        assert!(
            stdout_trim.contains("write_ok=0"),
            "writing to {denied_path} should be denied by Landlock: {stdout_trim}"
        );
    }
}
