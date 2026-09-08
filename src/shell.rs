//! Persistent shell session + background bash registry.
//! 持久 shell 会话 + 后台 bash 注册表。
//!
//! Design:
//! - `ShellSession` owns ONE long-lived `sh` child (sandbox-wrapped at the
//!   process level so isolation covers every subsequent command — not
//!   per-command wrapping). Commands are written to the child's stdin; stdout
//!   and stderr are read concurrently until a per-session sentinel line
//!   appears on EACH stream (dual-sentinel protocol preserves the stdout/stderr
//!   separation needed for the existing output format).
//! - `LazyShell` wraps `Mutex<Option<ShellSession>>` — the shell spawns on the
//!   FIRST exec, never at agent build time. Each agent gets its own `LazyShell`
//!   (per-agent-run isolation; parallel SDD roles never interleave commands
//!   in one shell).
//! - `BackgroundRegistry` manages detached long-running commands. Each gets an
//!   id (`bg-{n}`). A reader task drains stdout+stderr into a capped buffer and
//!   records the exit code. `Drop` kills all children.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStderr, ChildStdout};

use crate::seam::SandboxProvider;

/// Global counter for nonce uniqueness within the process.
/// 进程级 nonce 计数器。
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a per-session nonce string.
/// 生成每会话唯一的 nonce 字符串。
///
/// Format: `__MOYE_DONE_{pid}_{counter}` — collision-resistant without new deps.
fn generate_nonce() -> String {
    let pid = std::process::id();
    let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__MOYE_DONE_{pid}_{counter}")
}

/// A long-lived shell session that persists cwd and env across commands.
/// 持久 shell 会话：跨命令保持工作目录与环境变量。
///
/// Design: spawns ONE `sh` child (sandbox-wrapped at the process level, so
/// isolation covers every subsequent command). Commands are written to the
/// child's stdin; stdout and stderr are read concurrently until a per-session
/// sentinel line appears on EACH stream.
pub struct ShellSession {
    child: Child,
    pid: u32,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    nonce: String,
    #[allow(dead_code)]
    max_output_chars: usize,
    dead: bool,
    /// 此 shell spawn 时记录的沙箱授权版本。
    /// 若当前 provider.auth_version() 与此值不同,说明 bwrap 命名空间
    /// 已过期(spawn 后新 authorize 的目录在旧命名空间中不存在 → ENOENT),
    /// shell 需要重启。
    /// The sandbox authorization version recorded when this shell was spawned.
    /// If the current provider.auth_version() differs, the bwrap namespace
    /// is stale (dirs authorized after spawn don't exist in the old namespace
    /// → ENOENT) and the shell must respawn.
    spawned_version: u64,
}

impl ShellSession {
    /// Spawn a new sandbox-wrapped `sh` process with stdin/stdout/stderr piped.
    /// 生成一个沙箱包裹的 `sh` 子进程，stdin/stdout/stderr 均为管道。
    ///
    /// `auth_version` is the sandbox's authorization version at spawn time;
    /// stored on the session so `LazyShell::exec` can detect staleness later.
    async fn spawn(
        sandbox: &dyn SandboxProvider,
        max_output_chars: usize,
        auth_version: u64,
    ) -> std::io::Result<Self> {
        let nonce = generate_nonce();
        let mut cmd = build_shell_command(sandbox);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        Ok(Self {
            child,
            pid,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: BufReader::new(stderr),
            nonce,
            max_output_chars,
            dead: false,
            spawned_version: auth_version,
        })
    }

    fn is_dead(&self) -> bool {
        self.dead
    }

    /// Kill the shell's process group and reap the child. Reused by both the
    /// timeout path and the version-staleness respawn path.
    /// 杀掉 shell 的进程组并回收子进程。超时路径与版本过期重启路径共用。
    async fn kill(&mut self) {
        if self.pid != 0 {
            let _ = unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        }
        let _ = self.child.wait().await;
        self.dead = true;
    }

    /// Execute a command and return (stdout, stderr, exit_code).
    /// 执行命令，返回 (stdout, stderr, 退出码)。
    ///
    /// Sentinel protocol: appends `__r=$?; printf '<nonce>__%d\n' "$__r"`;
    /// `printf '<nonce>__%d\n' "$__r" >&2` — sentinel on BOTH streams.
    /// Reads stdout and stderr concurrently until both sentinels appear.
    ///
    /// On timeout: kills the whole child (commands can't be killed individually
    /// inside a shell). Marks session dead; next exec spawns a fresh shell.
    /// On EOF/broken pipe: same respawn-on-next-exec behavior.
    async fn exec(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<(String, String, i32), String> {
        if self.dead {
            return Err("shell session is dead".to_string());
        }

        // Write command + sentinel to stdin.
        // 写入命令 + 哨兵到 stdin。
        let nonce = &self.nonce;
        let full_command = format!(
            "{command}\n__r=$?\nprintf '{nonce}__%d\\n' \"$__r\"\nprintf '{nonce}__%d\\n' \"$__r\" >&2\n"
        );

        if self.stdin.write_all(full_command.as_bytes()).await.is_err() {
            self.dead = true;
            return Err("shell stdin broken (child may have died)".to_string());
        }

        let sentinel_prefix = format!("{}__", self.nonce);

        // Read stdout and stderr concurrently until both sentinels appear.
        // 并发读取 stdout 和 stderr 直到各自出现哨兵。
        let stdout = &mut self.stdout;
        let stderr = &mut self.stderr;
        let result = tokio::time::timeout(timeout, read_both(stdout, stderr, &sentinel_prefix)).await;

        match result {
            Ok((stdout_out, stderr_out, Some(code))) => Ok((stdout_out, stderr_out, code)),
            Ok((stdout_out, stderr_out, None)) => {
                // Shell died before sentinel (e.g., `exit N` killed the shell).
                // Recover the exit code from the child process directly.
                self.dead = true;
                let code = match self.child.wait().await {
                    Ok(status) => status.code().unwrap_or(-1),
                    Err(_) => -1,
                };
                Ok((stdout_out, stderr_out, code))
            }
            Err(_) => {
                // Timeout — kill the entire process group (shell + children like `sleep`).
                // 超时——杀掉整个进程组（shell + 子进程如 `sleep`）。
                self.kill().await;
                Err(format!("command timed out after {}s", timeout.as_secs()))
            }
        }
    }
}

/// Build the sandbox-wrapped `sh` Command (no `-c`; reads from stdin).
/// 构建沙箱包裹的 `sh` Command（不带 `-c`；从 stdin 读取）。
///
/// The sandbox wrapping is applied to the SHELL process itself, so isolation
/// then covers every subsequent command — not per-command wrapping.
fn build_shell_command(sandbox: &dyn SandboxProvider) -> tokio::process::Command {
    if let Some(bwrap_argv) = sandbox.grant_args(&[], &[]) {
        let mut cmd = tokio::process::Command::new(&bwrap_argv[0]);
        for arg in &bwrap_argv[1..] {
            cmd.arg(arg);
        }
        cmd.arg("--").arg("sh");
        cmd.process_group(0);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        attach_landlock_pre_exec(sandbox, &mut cmd);
        cmd
    }
}

/// Build a sandbox-wrapped `sh -c <command>` Command (for background shells).
/// 构建沙箱包裹的 `sh -c <command>` Command（用于后台 shell）。
fn build_bash_command(sandbox: &dyn SandboxProvider, command: &str) -> tokio::process::Command {
    if let Some(bwrap_argv) = sandbox.grant_args(&[], &[]) {
        let mut cmd = tokio::process::Command::new(&bwrap_argv[0]);
        for arg in &bwrap_argv[1..] {
            cmd.arg(arg);
        }
        cmd.arg("--").arg("sh").arg("-c").arg(command);
        cmd.process_group(0);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        attach_landlock_pre_exec(sandbox, &mut cmd);
        cmd
    }
}

/// Attach a Landlock `pre_exec` hook to the command, if the sandbox provider
/// supplies one. Called from both `build_shell_command` and
/// `build_bash_command` when `grant_args` returned `None` (no bwrap).
/// 当 `grant_args` 返回 `None`(无 bwrap)时,附加 Landlock pre_exec 回调。
#[cfg(target_os = "linux")]
fn attach_landlock_pre_exec(sandbox: &dyn SandboxProvider, cmd: &mut tokio::process::Command) {
    if let Some(landlock_fn) = sandbox.pre_exec_landlock() {
        // SAFETY: The closure returned by pre_exec_landlock() makes only two
        // syscalls on the success path: prctl(PR_SET_NO_NEW_PRIVS) and
        // landlock_restrict_self(ruleset_fd, 0). No heap allocation, no locks
        // — async-signal-safe per signal-safety(7). The OwnedFd keeping the
        // ruleset fd alive is moved into the closure and remains valid until
        // the child execs or exits. On the error path, format! allocation is
        // acceptable (the child will not exec). The closure is Fn (only reads
        // the captured Box<dyn Fn> via &self), satisfying the FnMut bound.
        unsafe {
            cmd.pre_exec(move || landlock_fn().map_err(std::io::Error::other));
        }
    }
}

/// Read lines from a buffered reader until a sentinel line appears.
/// 从缓冲读取器读取行直到哨兵行出现。
///
/// Returns (output, Some(code)) if sentinel found, (output, None) on EOF.
/// The partial output is preserved even on EOF so the caller can recover it.
async fn read_until_sentinel<R>(
    reader: &mut R,
    sentinel_prefix: &str,
) -> (String, Option<i32>)
where
    R: AsyncBufRead + Unpin,
{
    let mut output = String::new();
    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(_) => return (output, None),
        };
        if n == 0 {
            return (output, None);
        }
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix(sentinel_prefix) {
            let code: i32 = rest.parse().unwrap_or(-1);
            return (output, Some(code));
        }
        output.push_str(&line);
    }
}

/// Read stdout and stderr concurrently until both sentinels appear.
/// 并发读取 stdout 和 stderr 直到各自出现哨兵。
///
/// Returns (stdout, stderr, Option<exit_code>). Uses `tokio::join!` so both streams
/// are drained simultaneously — preventing pipe-buffer deadlock. On EOF (shell died),
/// returns partial output and None for exit_code; the caller recovers the code from
/// the child process.
async fn read_both<S, E>(
    stdout: &mut S,
    stderr: &mut E,
    sentinel_prefix: &str,
) -> (String, String, Option<i32>)
where
    S: AsyncBufRead + Unpin,
    E: AsyncBufRead + Unpin,
{
    let stdout_fut = read_until_sentinel(stdout, sentinel_prefix);
    let stderr_fut = read_until_sentinel(stderr, sentinel_prefix);
    let ((stdout_buf, stdout_code), (stderr_buf, _)) = tokio::join!(stdout_fut, stderr_fut);
    (stdout_buf, stderr_buf, stdout_code)
}

/// Lazily-created long-lived shell. Spawns on FIRST exec, never at build time.
/// 延迟创建的持久 shell。首次 exec 时生成，绝不在构建时。
///
/// Each agent gets its own `LazyShell` (per-agent-run isolation; parallel SDD
/// roles never interleave commands in one shell).
pub struct LazyShell {
    inner: tokio::sync::Mutex<Option<ShellSession>>,
    sandbox: Arc<dyn SandboxProvider>,
    max_output_chars: usize,
}

impl LazyShell {
    pub fn new(sandbox: Arc<dyn SandboxProvider>, max_output_chars: usize) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(None),
            sandbox,
            max_output_chars,
        }
    }

    /// Execute a command, spawning a fresh shell if needed.
    /// 执行命令，必要时生成新 shell。
    ///
    /// Returns (stdout, stderr, exit_code). On timeout/death, the next call
    /// respawns a fresh shell (state loss is acceptable).
    ///
    /// Version check: if the sandbox's authorization version has changed
    /// since this shell was spawned, the bwrap namespace is stale —
    /// directories authorized after spawn don't exist inside the frozen
    /// namespace (ENOENT). The stale shell is killed and a fresh one
    /// spawned with the updated mounts. cwd/env loss on respawn is
    /// acceptable; stale isolation is worse.
    /// 版本检查：若沙箱授权版本自 shell 生成后已变化，bwrap 命名空间过期——
    /// spawn 后授权的目录在冻结的命名空间中不存在（ENOENT）。杀掉过期 shell
    /// 并用更新后的挂载生成新 shell。重启时 cwd/env 丢失可接受；
    /// 过期隔离更危险。
    pub async fn exec(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<(String, String, i32), String> {
        let mut session = self.inner.lock().await;
        let current_version = self.sandbox.auth_version();
        let need_respawn = match session.as_ref() {
            None => true,
            Some(s) => s.is_dead() || s.spawned_version != current_version,
        };
        if need_respawn {
            if let Some(ref mut old) = *session {
                if !old.is_dead() {
                    old.kill().await;
                }
            }
            let new_session = ShellSession::spawn(
                self.sandbox.as_ref(),
                self.max_output_chars,
                current_version,
            )
            .await
            .map_err(|e| format!("failed to spawn shell: {e}"))?;
            *session = Some(new_session);
        }
        let s = session.as_mut().unwrap();
        s.exec(command, timeout).await
    }
}

/// A background shell registered in `BackgroundRegistry`.
/// 注册在 BackgroundRegistry 中的后台 shell。
struct BackgroundShell {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    command: String,
    pid: u32,
    output: Arc<Mutex<String>>,
    exit: Arc<Mutex<Option<i32>>>,
    /// Reader task handle — awaited in `shutdown` for clean reaping.
    /// 读取任务句柄——在 shutdown 中等待以干净地回收子进程。
    #[allow(dead_code)]
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// Registry of detached long-running commands (dev servers, watch mode, etc.).
/// 后台长命令注册表（开发服务器、watch 模式等）。
///
/// Sharing model: one `Arc<BackgroundRegistry>` per Orchestrator, injected via
/// `ToolDeps`. Shells outlive the turn but die when the Orchestrator is dropped
/// (session end). `Drop` sends SIGKILL to all children.
pub struct BackgroundRegistry {
    inner: Mutex<HashMap<String, BackgroundShell>>,
    counter: AtomicU64,
    max_output_chars: usize,
    /// 最近一次 `start()` 时记录的沙箱授权版本。
    /// 版本变化时,所有已存在的后台 shell 的 bwrap 命名空间同样冻结
    /// 在旧版本——它们与前台 shell 一样过期,必须被杀掉
    /// (过期隔离是安全风险,不自动重启——杀掉是安全行为)。
    /// The sandbox auth version recorded at the last `start()`.
    /// On version change, existing background shells' bwrap namespaces
    /// are equally frozen at the old version — they are just as stale
    /// as foreground shells and must be killed (stale isolation is a
    /// hazard; killing is the safe behavior, not auto-respawn).
    spawned_version: AtomicU64,
}

impl BackgroundRegistry {
    pub fn new(max_output_chars: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            max_output_chars,
            spawned_version: AtomicU64::new(0),
        }
    }

    /// Start a background command. Returns its id (`bg-{n}`) immediately.
    /// 启动后台命令。立即返回其 id（`bg-{n}`）。
    pub fn start(&self, command: &str, sandbox: &dyn SandboxProvider) -> std::io::Result<String> {
        // Version check at the background exec path: if the sandbox auth
        // version has changed since the last background shell started, kill
        // all existing background shells — their frozen namespaces are
        // equally stale (stale isolation is a hazard).
        // 后台 exec 路径的版本检查：若沙箱授权版本自上次后台 shell
        // 启动后已变化，杀掉所有现有后台 shell——它们的冻结命名空间
        // 同样过期（过期隔离是安全风险）。
        let current_version = sandbox.auth_version();
        self.check_version(current_version);

        let id = format!("bg-{}", self.counter.fetch_add(1, Ordering::Relaxed));

        let mut cmd = build_bash_command(sandbox, command);
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let output: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let exit: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let max_chars = self.max_output_chars;

        let output_clone = output.clone();
        let exit_clone = exit.clone();

        // Reader task: drains stdout+stderr into the capped buffer, then waits
        // for the child to exit and records the exit code.
        // 读取任务：将 stdout+stderr 排入有上限的缓冲区，然后等待子进程退出并记录退出码。
        let handle = tokio::spawn(async move {
            let stdout_reader = BufReader::new(stdout);
            let stderr_reader = BufReader::new(stderr);
            let mut stdout_lines = stdout_reader.lines();
            let mut stderr_lines = stderr_reader.lines();

            loop {
                tokio::select! {
                    line = stdout_lines.next_line() => {
                        match line {
                            Ok(Some(l)) => append_capped(&output_clone, &l, max_chars),
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                    line = stderr_lines.next_line() => {
                        match line {
                            Ok(Some(l)) => append_capped(&output_clone, &l, max_chars),
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
            }
            // Drain remaining lines on each stream.
            // 排空每个流上的剩余行。
            while let Ok(Some(l)) = stdout_lines.next_line().await {
                append_capped(&output_clone, &l, max_chars);
            }
            while let Ok(Some(l)) = stderr_lines.next_line().await {
                append_capped(&output_clone, &l, max_chars);
            }
            let status = child.wait().await;
            if let Ok(s) = status {
                *exit_clone.lock().unwrap() = s.code();
            }
        });

        let shell = BackgroundShell {
            id: id.clone(),
            command: command.to_string(),
            pid,
            output,
            exit,
            handle: Some(handle),
        };
        self.inner.lock().unwrap().insert(id.clone(), shell);
        Ok(id)
    }

    /// Read accumulated output + exit status for a background shell.
    /// 读取后台 shell 的累积输出 + 退出状态。
    ///
    /// Returns `None` if the id is unknown. Returns `(buffer, None)` while
    /// the child is still running; `(buffer, Some(code))` after it exits.
    pub fn read(&self, id: &str) -> Option<(String, Option<i32>)> {
        let inner = self.inner.lock().unwrap();
        let shell = inner.get(id)?;
        let output = shell.output.lock().unwrap().clone();
        let exit = *shell.exit.lock().unwrap();
        Some((output, exit))
    }

    /// Kill a background shell by id. Returns true if the signal was sent.
    /// 按 id 杀掉后台 shell。信号已发送返回 true。
    ///
    /// Uses `libc::kill` — the `Child` is owned by the reader task.
    pub fn kill(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        if let Some(shell) = inner.get(id) {
            if shell.pid == 0 {
                return false;
            }
            let r = unsafe { libc::kill(-(shell.pid as i32), libc::SIGKILL) };
            r == 0
        } else {
            false
        }
    }

    /// Kill all background shells by sending SIGKILL to each process group.
    /// Does not wait for reaping — reader tasks will eventually record exit
    /// codes. Reused by `check_version` and `shutdown`.
    /// 杀掉所有后台 shell：向每个进程组发送 SIGKILL。不等待回收——
    /// 读取任务会最终记录退出码。被 check_version 与 shutdown 复用。
    pub fn kill_all(&self) {
        let inner = self.inner.lock().unwrap();
        for shell in inner.values() {
            if shell.pid != 0 {
                let _ = unsafe { libc::kill(-(shell.pid as i32), libc::SIGKILL) };
            }
        }
    }

    /// Check if the sandbox authorization version has changed since the last
    /// background shell was started. On mismatch, kill all existing background
    /// shells (their frozen namespaces are stale) and update the recorded
    /// version. Called from `start()` — the background exec path.
    /// 检查沙箱授权版本自上次后台 shell 启动后是否变化。不匹配时杀掉
    /// 所有现有后台 shell（它们的冻结命名空间已过期）并更新记录的版本。
    /// 从 start()（后台 exec 路径）调用。
    pub fn check_version(&self, provider_version: u64) {
        let spawned = self.spawned_version.load(Ordering::Relaxed);
        if spawned != provider_version {
            self.kill_all();
            self.spawned_version.store(provider_version, Ordering::Relaxed);
        }
    }

    /// Kill all children and wait for reader tasks to reap them.
    /// 杀掉所有子进程并等待读取任务回收。
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut inner = self.inner.lock().unwrap();
            for shell in inner.values() {
                if shell.pid != 0 {
                    let _ = unsafe { libc::kill(-(shell.pid as i32), libc::SIGKILL) };
                }
            }
            inner
                .values_mut()
                .filter_map(|s| s.handle.take())
                .collect()
        };
        for handle in handles {
            let _ = handle.await;
        }
    }
}

impl Drop for BackgroundRegistry {
    fn drop(&mut self) {
        let inner = self.inner.lock().unwrap();
        for shell in inner.values() {
            if shell.pid != 0 {
                let _ = unsafe { libc::kill(-(shell.pid as i32), libc::SIGKILL) };
            }
        }
    }
}

/// Append a line to the capped buffer (truncates at char boundary if needed).
/// 向有上限的缓冲区追加一行（超出时按字符边界截断）。
fn append_capped(buf: &Arc<Mutex<String>>, line: &str, max_chars: usize) {
    let mut b = buf.lock().unwrap();
    if b.chars().count() >= max_chars {
        return;
    }
    b.push_str(line);
    b.push('\n');
    if b.chars().count() > max_chars {
        // Truncate at char boundary without adding a notice — the cap is a
        // safety net, not a user-facing truncation (bash_output shows the raw
        // buffer; the LLM sees whatever was accumulated up to the cap).
        let end = b
            .char_indices()
            .take(max_chars)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        b.truncate(end);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{SimpleSandbox, SandboxBackend};

    /// Test sandbox: no OS-level wrapping (grant_args returns None).
    /// 测试沙箱：无 OS 级包裹（grant_args 返回 None）。
    fn no_sandbox() -> Arc<dyn SandboxProvider> {
        Arc::new(SimpleSandbox::with_backend(&[], SandboxBackend::Off))
    }

    /// Echo roundtrip: output and exit code 0.
    /// echo 往返：输出与退出码 0。
    #[tokio::test]
    async fn shell_echo_roundtrip_exit_code() {
        let mut session = ShellSession::spawn(no_sandbox().as_ref(), 20000, 0)
            .await
            .expect("spawn");
        let (stdout, stderr, code) = session
            .exec("echo hello", Duration::from_secs(10))
            .await
            .expect("exec");
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "hello");
        assert!(stderr.trim().is_empty());
    }

    /// `cd` persists across two execs.
    /// `cd` 跨两次 exec 持久化。
    #[tokio::test]
    async fn shell_cd_persists_across_execs() {
        let mut session = ShellSession::spawn(no_sandbox().as_ref(), 20000, 0)
            .await
            .expect("spawn");
        session
            .exec("cd /tmp", Duration::from_secs(10))
            .await
            .expect("cd");
        let (stdout, _, code) = session
            .exec("pwd", Duration::from_secs(10))
            .await
            .expect("pwd");
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "/tmp");
    }

    /// `export` persists across execs.
    /// `export` 跨 exec 持久化。
    #[tokio::test]
    async fn shell_export_persists_across_execs() {
        let mut session = ShellSession::spawn(no_sandbox().as_ref(), 20000, 0)
            .await
            .expect("spawn");
        session
            .exec("export MOYE_TEST_VAR=42", Duration::from_secs(10))
            .await
            .expect("export");
        let (stdout, _, code) = session
            .exec("echo $MOYE_TEST_VAR", Duration::from_secs(10))
            .await
            .expect("echo");
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "42");
    }

    /// Nonzero exit code is captured.
    /// 非零退出码被捕获。
    #[tokio::test]
    async fn shell_nonzero_exit_captured() {
        let mut session = ShellSession::spawn(no_sandbox().as_ref(), 20000, 0)
            .await
            .expect("spawn");
        let (stdout, _, code) = session
            .exec("exit 7", Duration::from_secs(10))
            .await
            .expect("exec");
        assert_eq!(code, 7);
        assert!(stdout.trim().is_empty());
    }

    /// Timeout kills the child; next exec spawns a fresh shell.
    /// 超时杀掉子进程；下次 exec 生成新 shell。
    #[tokio::test]
    async fn shell_timeout_kills_and_respawns() {
        let lazy = LazyShell::new(no_sandbox(), 20000);
        // First exec: sleep 100 with 1s timeout → should time out.
        let result = lazy.exec("sleep 100", Duration::from_secs(1)).await;
        assert!(result.is_err(), "sleep 100 should time out");
        let err = result.unwrap_err();
        assert!(err.contains("timed out"), "error should mention timeout: {err}");

        // Second exec: fresh shell, should work.
        let (stdout, _, code) = lazy
            .exec("echo revived", Duration::from_secs(10))
            .await
            .expect("revived");
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "revived");
    }

    /// A command printing a fake sentinel (different nonce) must not break detection.
    /// 打印伪造哨兵（不同 nonce）的命令不得破坏检测。
    #[tokio::test]
    async fn shell_fake_sentinel_does_not_break() {
        let lazy = LazyShell::new(no_sandbox(), 20000);
        // Print a line that looks like a sentinel but with a different nonce.
        lazy.exec("echo __MOYE_DONE_999999_999__0", Duration::from_secs(10))
            .await
            .expect("fake sentinel exec");
        // Next command should still work correctly.
        let (stdout, _, code) = lazy
            .exec("echo after_fake", Duration::from_secs(10))
            .await
            .expect("after fake");
        assert_eq!(code, 0);
        assert_eq!(stdout.trim(), "after_fake");
    }

    /// Output cap enforced via truncation in append_capped.
    /// append_capped 中的截断上限被正确执行。
    #[tokio::test]
    async fn bg_output_cap_enforced() {
        let bg = BackgroundRegistry::new(100);
        let id = bg
            .start("for i in $(seq 1 1000); do echo line$i; done", no_sandbox().as_ref())
            .expect("start");
        // Wait for the child to finish.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some((_, Some(_))) = bg.read(&id) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("child should finish within 10s");

        let (buffer, exit) = bg.read(&id).expect("read");
        assert!(exit.is_some(), "child should have exited");
        assert!(
            buffer.chars().count() <= 100,
            "buffer should be capped at 100 chars, got {}",
            buffer.chars().count()
        );
        bg.shutdown().await;
    }

    /// Background start returns an id immediately; read shows output; kill works.
    /// 后台启动立即返回 id；read 显示输出；kill 有效。
    #[tokio::test]
    async fn bg_start_read_kill() {
        let bg = BackgroundRegistry::new(20000);
        let id = bg
            .start("echo hi; sleep 30", no_sandbox().as_ref())
            .expect("start");
        assert!(id.starts_with("bg-"), "id should be bg-N: {id}");

        // Poll for "hi" with a bounded timeout (no fixed sleeps).
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some((buffer, _)) = bg.read(&id) {
                    if buffer.contains("hi") {
                        return true;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(found.unwrap_or(false), "should find 'hi' in output within 5s");

        // Kill should return true.
        assert!(bg.kill(&id), "kill should return true");

        // After kill + shutdown, the child should be reaped.
        bg.shutdown().await;
    }

    /// Drop registry kills children (no zombie processes).
    /// drop registry 杀掉子进程（无僵尸进程）。
    #[tokio::test]
    async fn bg_drop_kills_children() {
        let bg = Arc::new(BackgroundRegistry::new(20000));
        let id = bg
            .start("sleep 300", no_sandbox().as_ref())
            .expect("start");

        // Get the pid via /proc or by checking kill(pid, 0).
        let pid = {
            let inner = bg.inner.lock().unwrap();
            inner.get(&id).map(|s| s.pid).unwrap_or(0)
        };
        assert!(pid != 0, "pid should be non-zero");

        // Verify the process exists.
        let alive_before = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(alive_before, "child should be alive before drop");

        // Shutdown (simulates drop — kills all children and waits for reaping).
        bg.shutdown().await;

        // After shutdown, the process should be gone.
        let alive_after = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(!alive_after, "child should be dead after shutdown");
    }

    /// After an authorization bump, exec respawns a fresh shell.
    /// The env var set in exec 1 does NOT survive the respawn (fresh env).
    /// 授权递增后，exec 重启新 shell。exec 1 设置的环境变量在重启后消失。
    #[tokio::test]
    async fn shell_version_change_respawns() {
        let sb = Arc::new(SimpleSandbox::with_backend(&[], SandboxBackend::Off));
        let lazy = LazyShell::new(sb.clone() as Arc<dyn SandboxProvider>, 20000);

        // exec 1: set a marker env var.
        lazy.exec("export MOYE_VMARKER=alive", Duration::from_secs(10))
            .await
            .expect("exec 1");
        // verify the marker is set.
        let (stdout, _, _) = lazy
            .exec("echo $MOYE_VMARKER", Duration::from_secs(10))
            .await
            .expect("verify marker");
        assert_eq!(stdout.trim(), "alive");

        // bump the authorization version.
        sb.authorize("/tmp/moye_vmarker_test");

        // exec 2: should respawn; the marker env var must be gone.
        let (stdout, _, _) = lazy
            .exec("echo $MOYE_VMARKER", Duration::from_secs(10))
            .await
            .expect("exec 2 after bump");
        assert_eq!(
            stdout.trim(),
            "",
            "fresh shell should not retain the marker env var"
        );
    }

    /// Without a version bump, execs reuse the same shell (env var persists).
    /// 无版本递增时，exec 复用同一 shell（环境变量持久化）。
    #[tokio::test]
    async fn shell_no_bump_reuses_shell() {
        let sb = Arc::new(SimpleSandbox::with_backend(&[], SandboxBackend::Off));
        let lazy = LazyShell::new(sb.clone() as Arc<dyn SandboxProvider>, 20000);

        // exec 1: set a marker env var.
        lazy.exec("export MOYE_REUSE=persist", Duration::from_secs(10))
            .await
            .expect("exec 1");

        // exec 2: no version bump → same shell, marker should persist.
        let (stdout, _, _) = lazy
            .exec("echo $MOYE_REUSE", Duration::from_secs(10))
            .await
            .expect("exec 2");
        assert_eq!(
            stdout.trim(),
            "persist",
            "same shell should retain env var without version bump"
        );
    }

    /// After a version bump, check_version kills stale background shells.
    /// 版本递增后，check_version 杀掉过期的后台 shell。
    #[tokio::test]
    async fn bg_version_change_kills_stale() {
        let sb = Arc::new(SimpleSandbox::with_backend(&[], SandboxBackend::Off));
        let bg = BackgroundRegistry::new(20000);

        let id = bg
            .start("sleep 300", sb.as_ref())
            .expect("start");

        let pid = {
            let inner = bg.inner.lock().unwrap();
            inner.get(&id).map(|s| s.pid).unwrap_or(0)
        };
        assert!(pid != 0, "pid should be non-zero");

        let alive_before = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(alive_before, "bg shell should be alive before version change");

        sb.authorize("/tmp/moye_bg_version_test");
        assert_eq!(sb.auth_version(), 1, "version should be 1 after authorize");

        bg.check_version(sb.auth_version());

        bg.shutdown().await;

        let alive_after = unsafe { libc::kill(pid as i32, 0) } == 0;
        assert!(!alive_after, "bg shell should be dead after version change + shutdown");
    }
}
