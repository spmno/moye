// OS 级心跳调度：把"每分钟唤醒一次 moye"注册到操作系统自带的调度器，
// 使定时任务在 moye 进程退出后依然能被触发。
// OS-level heartbeat scheduling: registers "wake moye once per minute" with the
// operating system's own scheduler, so scheduled tasks fire even when no moye
// process is running.
//
// 平台实现 / Platform backends:
//   - Linux/macOS: 用户 crontab 中的标记块（每项目一条，key = 工作目录哈希）。
//     Linux/macOS: a marked block in the user's crontab (one per project).
//   - Windows:     任务计划程序（schtasks），每分钟运行一个生成的 .cmd 包装脚本。
//     Windows 没有 crontab，等价物是 Task Scheduler；schtasks 是其命令行前端，
//     原生支持分钟级周期（/SC MINUTE /MO 1），与 cron 的 * * * * * 对齐。
//     Windows: Task Scheduler via schtasks (no crontab exists on Windows);
//     a generated .cmd wrapper script is run every minute.
//
// 心跳只做一件事：执行 `moye --scheduler-tick`。到期扫描 / 派发 / 记账全部
// 复用进程内调度器逻辑（任务 JSON 存储是唯一事实源，增删改任务无需动 OS 侧）。
// The heartbeat does exactly one thing: run `moye --scheduler-tick`. Due-scan /
// dispatch / bookkeeping all reuse the in-process scheduler logic, and the task
// JSON store remains the single source of truth (task edits never touch the OS
// side).

use std::path::{Path, PathBuf};

use anyhow::Result;

/// 一次心跳注册所需的全部信息。
/// Everything needed to (un)register one heartbeat.
pub struct Heartbeat {
    /// 项目工作目录（心跳触发后先 cd 到这里，保证 agent.toml / .env 可解析）。
    /// Project working directory (heartbeat cd's here first so agent.toml / .env
    /// resolve exactly like an interactive run).
    pub workdir: PathBuf,
    /// moye 可执行文件的绝对路径。
    /// Absolute path of the moye executable.
    pub binary: PathBuf,
    /// 心跳自身 stdout/stderr 追加写入的日志文件。
    /// Log file the heartbeat's stdout/stderr are appended to.
    pub log_path: PathBuf,
}

impl Heartbeat {
    /// 项目级唯一 key（工作目录哈希）：同一台机器上多个 moye 项目的心跳互不干扰。
    /// Per-project unique key (workdir hash): multiple moye projects on one
    /// machine get independent heartbeats.
    pub fn key(&self) -> String {
        project_key(&self.workdir)
    }

    /// 注册心跳（幂等：内容未变化时不改写 OS 调度器）。
    /// Register the heartbeat (idempotent: no rewrite when nothing changed).
    pub fn install(&self) -> Result<String> {
        platform::install(self)
    }

    /// 注销本项目的心跳（不影响其他项目 / 其他用户条目）。
    /// Unregister this project's heartbeat (other entries are left untouched).
    pub fn uninstall(&self) -> Result<String> {
        platform::uninstall(self)
    }

    /// 本项目心跳当前是否已注册。
    /// Whether this project's heartbeat is currently registered.
    #[allow(dead_code)]
    pub fn is_installed(&self) -> bool {
        platform::is_installed(self)
    }
}

/// 由工作目录派生短哈希 key。
/// Derive a short hash key from the working directory.
pub fn project_key(workdir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    workdir.to_string_lossy().hash(&mut h);
    format!("{:08x}", h.finish() & 0xFFFF_FFFF)
}

// ── Linux / macOS：crontab 标记块 ──
// ── Linux / macOS: marked crontab block ──

#[cfg(unix)]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::process::Stdio;

    fn begin_marker(key: &str) -> String {
        format!("# moye-heartbeat-begin:{key}")
    }

    fn end_marker(key: &str) -> String {
        format!("# moye-heartbeat-end:{key}")
    }

    /// crontab 命令行。cron 以 /bin/sh 执行命令，允许 shell 语法；
    /// `%` 在 crontab 命令中有特殊含义（换行），必须转义。
    /// The crontab entry (per-minute schedule + command). cron runs the
    /// command via /bin/sh so shell syntax is fine; `%` is special in
    /// crontab commands (newline) and must be escaped.
    pub(super) fn cron_line(hb: &Heartbeat) -> String {
        let cmd = format!(
            "* * * * * cd {} && {} --scheduler-tick >> {} 2>&1",
            sh_quote(&hb.workdir.to_string_lossy()),
            sh_quote(&hb.binary.to_string_lossy()),
            sh_quote(&hb.log_path.to_string_lossy()),
        );
        cmd.replace('%', r"\%")
    }

    /// POSIX 单引号引用：foo'bar → 'foo'\''bar'
    pub(super) fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn read_crontab() -> String {
        match std::process::Command::new("crontab").arg("-l").output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            // "no crontab for user" 等失败一律视为空 crontab。
            // Any failure (e.g. "no crontab for user") is treated as empty.
            _ => String::new(),
        }
    }

    fn write_crontab(content: &str) -> Result<()> {
        use std::io::Write;
        let mut child = std::process::Command::new("crontab")
            .arg("-")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn `crontab -` (is a cron daemon installed?)")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(content.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("`crontab -` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// 移除指定 key 的标记块，返回剩余内容（块不存在时原样返回）。
    /// Remove the marked block for `key`; returns the remaining content
    /// unchanged when the block doesn't exist.
    pub(super) fn strip_block(content: &str, key: &str) -> String {
        let begin = begin_marker(key);
        let end = end_marker(key);
        let mut out: Vec<&str> = Vec::new();
        let mut skip = false;
        for line in content.lines() {
            let t = line.trim();
            if t == begin {
                skip = true;
                continue;
            }
            if t == end {
                skip = false;
                continue;
            }
            if !skip {
                out.push(line);
            }
        }
        let trimmed = out.join("\n").trim_end().to_string();
        if trimmed.is_empty() { String::new() } else { format!("{trimmed}\n") }
    }

    pub fn install(hb: &Heartbeat) -> Result<String> {
        // 日志重定向要求父目录存在（cron 不会帮忙创建）。
        // The >> redirect needs the parent dir to exist (cron won't create it).
        if let Some(dir) = hb.log_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let key = hb.key();
        let old = read_crontab();
        let mut next = strip_block(&old, &key);
        next.push_str(&format!(
            "{}\n{}\n{}\n",
            begin_marker(&key),
            cron_line(hb),
            end_marker(&key)
        ));
        if next == old {
            return Ok(format!("crontab heartbeat unchanged (key={key})"));
        }
        write_crontab(&next)?;
        Ok(format!("installed crontab heartbeat (key={key}): {}", cron_line(hb)))
    }

    pub fn uninstall(hb: &Heartbeat) -> Result<String> {
        let key = hb.key();
        let old = read_crontab();
        let next = strip_block(&old, &key);
        if next == old {
            return Ok(format!("no crontab heartbeat found (key={key})"));
        }
        write_crontab(&next)?;
        Ok(format!("removed crontab heartbeat (key={key})"))
    }

    pub fn is_installed(hb: &Heartbeat) -> bool {
        read_crontab().contains(&begin_marker(&hb.key()))
    }
}

// ── Windows：任务计划程序（schtasks）+ .cmd 包装脚本 ──
// ── Windows: Task Scheduler (schtasks) + .cmd wrapper script ──
//
// 为什么不直接把命令写进 /TR：schtasks 无法设置任务的工作目录，
// 而 moye 依赖 cwd 解析 agent.toml/.env；包装脚本里 `cd /d` 是最稳妥的方案，
// 也顺带解决输出重定向与引号转义问题。
// Why a wrapper script instead of putting the command in /TR directly:
// schtasks cannot set a task's working directory, and moye resolves
// agent.toml/.env from cwd; `cd /d` inside a wrapper is the robust fix and
// also sidesteps output-redirection and quoting issues.

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::process::Stdio;

    fn task_name(key: &str) -> String {
        format!("moye-scheduler-{key}")
    }

    /// 包装脚本路径：与心跳日志同目录。
    /// Wrapper script path: next to the heartbeat log.
    fn wrapper_path(hb: &Heartbeat) -> PathBuf {
        hb.log_path
            .with_file_name(format!("moye_heartbeat_{}.cmd", hb.key()))
    }

    fn write_wrapper(hb: &Heartbeat) -> Result<PathBuf> {
        let path = wrapper_path(hb);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let content = format!(
            "@echo off\r\ncd /d \"{}\"\r\n\"{}\" --scheduler-tick >> \"{}\" 2>&1\r\n",
            hb.workdir.display(),
            hb.binary.display(),
            hb.log_path.display()
        );
        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn install(hb: &Heartbeat) -> Result<String> {
        let wrapper = write_wrapper(hb)?;
        let name = task_name(&hb.key());
        // /SC MINUTE /MO 1：每分钟一次（Task Scheduler 最细粒度，对齐 cron 的 * * * * *）。
        // /F：同名任务已存在时覆盖。默认仅当前用户登录期间运行（注销后不触发，
        // 需要"未登录也运行"请改用 /RU+/RP 或组策略，见文档说明）。
        // /SC MINUTE /MO 1: every minute (finest Task Scheduler granularity,
        // matching cron's * * * * *). /F: overwrite an existing task. Runs only
        // while the user is logged on by default (see docs for /RU + /RP).
        let out = std::process::Command::new("schtasks")
            .args([
                "/Create",
                "/TN", &name,
                "/SC", "MINUTE",
                "/MO", "1",
                "/TR", &format!("\"{}\"", wrapper.display()),
                "/F",
            ])
            .output()
            .context("failed to run schtasks (is this Windows?)")?;
        if !out.status.success() {
            // schtasks 的错误信息可能输出在 stdout。
            // schtasks may print its error on stdout.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            bail!("schtasks /Create failed: {stderr} {stdout}");
        }
        Ok(format!(
            "installed Windows scheduled task '{name}' -> {}",
            wrapper.display()
        ))
    }

    pub fn uninstall(hb: &Heartbeat) -> Result<String> {
        let name = task_name(&hb.key());
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", &name, "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        std::fs::remove_file(wrapper_path(hb)).ok();
        match out {
            Ok(o) if o.status.success() => Ok(format!("removed scheduled task '{name}'")),
            _ => Ok(format!("scheduled task '{name}' not present")),
        }
    }

    pub fn is_installed(hb: &Heartbeat) -> bool {
        std::process::Command::new("schtasks")
            .args(["/Query", "/TN", &task_name(&hb.key())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ── 其他平台：明确报错，不静默失败 ──
// ── Other platforms: fail loudly ──

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    use anyhow::bail;

    pub fn install(_hb: &Heartbeat) -> Result<String> {
        bail!("OS heartbeat scheduling is not supported on this platform; use [scheduler] mode = \"process\"")
    }

    pub fn uninstall(_hb: &Heartbeat) -> Result<String> {
        bail!("OS heartbeat scheduling is not supported on this platform")
    }

    pub fn is_installed(_hb: &Heartbeat) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_is_stable_and_distinct() {
        let a = project_key(Path::new("/tmp/project-a"));
        let b = project_key(Path::new("/tmp/project-a"));
        let c = project_key(Path::new("/tmp/project-b"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }

    #[cfg(unix)]
    #[test]
    fn strip_block_removes_only_own_block() {
        let content = "# other job\n0 1 * * * /bin/true\n# moye-heartbeat-begin:abc\n* * * * * x\n# moye-heartbeat-end:abc\n# moye-heartbeat-begin:def\n* * * * * y\n# moye-heartbeat-end:def\n";
        let stripped = platform::strip_block(content, "abc");
        assert!(!stripped.contains("moye-heartbeat-begin:abc"));
        assert!(stripped.contains("# other job"));
        assert!(stripped.contains("moye-heartbeat-begin:def"));
        // 再 strip 一次应为幂等。
        assert_eq!(platform::strip_block(&stripped, "abc"), stripped);
    }

    #[cfg(unix)]
    #[test]
    fn sh_quote_handles_single_quotes() {
        assert_eq!(platform::sh_quote("a'b"), "'a'\\''b'");
    }

    #[cfg(unix)]
    #[test]
    fn cron_line_has_schedule_prefix() {
        // 回归测试：cron 行必须以计划字段开头（曾经漏掉 `* * * * *` 导致
        // crontab 报 "bad minute"）。
        let hb = Heartbeat {
            workdir: PathBuf::from("/tmp/proj"),
            binary: PathBuf::from("/usr/local/bin/moye"),
            log_path: PathBuf::from("/tmp/hb.log"),
        };
        let line = platform::cron_line(&hb);
        assert!(line.starts_with("* * * * * cd "), "line = {line}");
        assert!(line.contains("--scheduler-tick"));
        assert!(line.contains("2>&1"));
    }
}
