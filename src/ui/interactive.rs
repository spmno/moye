//! In-TUI interactive terminal: runs commands in a PTY and renders output
//! inside the TUI, so the user never leaves the alternate screen.
//! TUI 内交互式终端：在 PTY 中运行命令并在 TUI 内渲染输出，
//! 用户无需离开备用屏幕。
//!
//! Design:
//! - `openpty()` creates a master/slave PTY pair.
//! - The command is spawned with the slave as its controlling terminal
//!   (via `pre_exec`: `setsid` + `TIOCSCTTY` + `dup2`).
//! - The master fd is set to non-blocking; `poll_output()` drains it
//!   in the TUI tick handler (120ms cadence).
//! - User keystrokes are forwarded to the master fd via `write_input()`.
//! - ANSI escape sequences are stripped for display; `\r` (carriage return)
//!   is handled by truncating the current line (so progress bars update
//!   in-place rather than stacking).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

use tokio::sync::oneshot;

/// Maximum output buffer size (characters). Older content is trimmed.
const MAX_OUTPUT: usize = 1 << 16; // 64 KiB

/// State for an in-TUI interactive terminal session.
/// TUI 内交互式终端会话状态。
pub struct InteractiveState {
    /// PTY master fd (read output, write input).
    master: OwnedFd,
    /// The child process.
    child: Child,
    /// Accumulated output (ANSI-stripped, `\r`-aware plain text).
    pub output: String,
    /// The command being run (for the title bar).
    pub command: String,
    /// Responder to send output back to the agent when the command finishes.
    pub responder: Option<oneshot::Sender<String>>,
    /// Exit code — `Some(_)` once the child has been reaped.
    pub exit_code: Option<i32>,
}

impl InteractiveState {
    /// Create a PTY, spawn the command in it, and return the state.
    /// The master fd is non-blocking; call `poll_output()` periodically.
    pub fn spawn(command: &str) -> io::Result<Self> {
        let (master, slave) = open_pty()?;

        // Master: non-blocking so `poll_output` never blocks the TUI loop.
        set_nonblocking(master.as_raw_fd())?;

        let slave_raw = slave.as_raw_fd();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);

        // pre_exec runs in the child after fork, before exec.
        // Make the slave the controlling terminal + redirect stdio.
        // SAFETY: setsid, ioctl(TIOCSCTTY), dup2, close are all
        // async-signal-safe (signal-safety(7)). No heap allocation on the
        // success path. The closure captures only a Copy i32.
        unsafe {
            cmd.pre_exec(move || {
                // New session → detach from the TUI's process group so
                // signals and job control work within the PTY.
                let _ = libc::setsid();
                // Acquire the slave as the controlling terminal.
                let _ = libc::ioctl(slave_raw, libc::TIOCSCTTY as libc::c_ulong, 0i32);
                // Redirect stdin/stdout/stderr → slave.
                if libc::dup2(slave_raw, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_raw, 1) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_raw, 2) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Close the original slave fd if it's above stderr.
                if slave_raw > 2 {
                    let _ = libc::close(slave_raw);
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        // Close the parent's copy of the slave so the master gets EOF
        // when the child exits (otherwise read blocks forever).
        drop(slave);

        Ok(Self {
            master,
            child,
            output: String::new(),
            command: command.to_string(),
            responder: None,
            exit_code: None,
        })
    }

    /// Non-blocking read from the PTY master. Returns `true` if any new
    /// data was appended to `output`.
    pub fn poll_output(&mut self) -> bool {
        if self.exit_code.is_some() {
            return false;
        }
        let mut got = false;
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: reading from a valid open fd.
            let n = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n > 0 {
                self.append_raw(&buf[..n as usize]);
                got = true;
            } else if n == 0 {
                // EOF — child closed the slave.
                break;
            } else {
                // n < 0: EAGAIN/EWOULDBLOCK → no more data right now.
                let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                    break;
                }
                // EINTR — retry; anything else — stop.
                if errno != libc::EINTR {
                    break;
                }
            }
        }
        got
    }

    /// Append raw bytes: strip ANSI, handle `\r` (carriage return = line reset).
    fn append_raw(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let stripped = strip_ansi(&text);
        for ch in stripped.chars() {
            if ch == '\r' {
                // Carriage return → truncate current line (progress bars etc.).
                if let Some(pos) = self.output.rfind('\n') {
                    self.output.truncate(pos + 1);
                } else {
                    self.output.clear();
                }
            } else {
                self.output.push(ch);
            }
        }
        // Trim oldest content if over the cap.
        if self.output.len() > MAX_OUTPUT {
            let cut = self.output.len() - MAX_OUTPUT;
            if let Some(pos) = self.output[cut..].find('\n') {
                self.output.drain(..cut + pos + 1);
            } else {
                self.output.drain(..cut);
            }
        }
    }

    /// Check if the child has exited. If so, drain remaining output and
    /// record the exit code. Returns `true` if finished (this call or a
    /// previous one).
    pub fn check_exit(&mut self) -> bool {
        if self.exit_code.is_some() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = Some(status.code().unwrap_or(-1));
                // Final drain — the child may have written just before dying.
                let _ = self.poll_output();
                true
            }
            Ok(None) => false,
            Err(_) => {
                self.exit_code = Some(-1);
                true
            }
        }
    }

    /// Write user input bytes to the PTY master (keyboard → child stdin).
    pub fn write_input(&mut self, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            // SAFETY: writing to a valid open fd.
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    data[off..].as_ptr() as *const libc::c_void,
                    data.len() - off,
                )
            };
            if n < 0 {
                let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno == libc::EINTR {
                    continue;
                }
                // EAGAIN or real error — drop the remaining bytes.
                // Keyboard input is tiny; this is extremely unlikely.
                break;
            }
            off += n as usize;
        }
    }

    /// Kill the child process group (SIGKILL).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.exit_code = Some(-1);
    }
}

impl Drop for InteractiveState {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── PTY helpers ────────────────────────────────────────────────────────────

/// Open a PTY pair (master + slave). Returns `(master, slave)`.
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty writes to master and slave; returns 0 on success.
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded; both fds are valid and owned by us.
    unsafe { Ok((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))) }
}

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL / F_SETFL on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Strip ANSI/VT escape sequences from a string, preserving all visible text.
///
/// Handles:
/// - CSI: `ESC [ ... <final byte 0x40–0x7E>`
/// - OSC: `ESC ] ... BEL` or `ESC ] ... ESC \`
/// - Other: `ESC <single char>` (e.g. `ESC 7`, `ESC 8`)
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        // ESC — look at the next char to classify.
        match chars.peek() {
            Some('[') => {
                chars.next(); // consume '['
                // Skip until a final byte in 0x40–0x7E.
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if (c as u32) >= 0x40 && (c as u32) <= 0x7e {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next(); // consume ']'
                // OSC — skip until BEL (\x07) or ST (ESC \).
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        if let Some(&'\\') = chars.peek() {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next(); // ESC + one char → skip both.
            }
            None => {}
        }
    }
    out
}

// ── Key → terminal bytes ───────────────────────────────────────────────────

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Convert a crossterm key event into the raw bytes a terminal expects.
/// Used to forward TUI keystrokes to the PTY child.
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Ctrl+letter → control character (1–31).
    if ctrl {
        if let KeyCode::Char(c) = key.code {
            if let Some(byte) = ctrl_char(c) {
                return vec![byte];
            }
        }
    }

    let mut raw = match key.code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => Vec::new(),
    };

    // Alt prefix (ESC + key).
    if alt && !raw.is_empty() {
        let mut prefixed = vec![0x1b];
        prefixed.append(&mut raw);
        return prefixed;
    }
    raw
}

/// Map a character to its Ctrl control code, if it's a letter.
fn ctrl_char(c: char) -> Option<u8> {
    if c.is_ascii_lowercase() {
        Some((c as u8) - b'a' + 1)
    } else if c.is_ascii_uppercase() {
        Some((c as u8) - b'A' + 1)
    } else if c == '@' {
        Some(0x00)
    } else if c == '[' {
        Some(0x1b)
    } else if c == '\\' {
        Some(0x1c)
    } else if c == ']' {
        Some(0x1d)
    } else if c == '^' {
        Some(0x1e)
    } else if c == '_' {
        Some(0x1f)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_csi_color() {
        assert_eq!(strip_ansi("\x1b[32mgreen\x1b[0m"), "green");
    }

    #[test]
    fn strip_csi_cursor() {
        assert_eq!(strip_ansi("\x1b[2J\x1b[Hhello"), "hello");
    }

    #[test]
    fn strip_osc_title() {
        assert_eq!(strip_ansi("\x1b]0;title\x07rest"), "rest");
    }

    #[test]
    fn strip_osc_st() {
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\rest"), "rest");
    }

    #[test]
    fn strip_esc_single() {
        assert_eq!(strip_ansi("\x1b7save"), "save");
    }

    #[test]
    fn preserve_utf8() {
        assert_eq!(strip_ansi("你好\x1b[31m世界\x1b[0m"), "你好世界");
    }

    #[test]
    fn carriage_return_resets_line() {
        // Spawn a trivial command just to get a valid InteractiveState.
        let mut s = InteractiveState::spawn("true").unwrap();
        s.output.clear();
        s.append_raw(b"loading");
        s.append_raw(b"\rloaded!");
        assert_eq!(s.output, "loaded!");
    }

    #[test]
    fn ctrl_letter_mapping() {
        assert_eq!(ctrl_char('c'), Some(0x03));
        assert_eq!(ctrl_char('a'), Some(0x01));
        assert_eq!(ctrl_char('z'), Some(0x1a));
        assert_eq!(ctrl_char('C'), Some(0x03));
        assert_eq!(ctrl_char('1'), None);
    }
}
