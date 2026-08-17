//! 剪贴板复制：分层尝试，任一成功即返回 `true`。
//!
//! 1. Wayland：内置 Wayland 剪贴板（`wl-clipboard-rs`，纯 Rust 编译进二进制，
//!    不依赖 `wl-copy`/`xclip`/`xsel` 等外部工具，也不依赖终端支持 OSC52）。
//! 2. 系统剪贴板工具：macOS `pbcopy`、Windows `clip.exe`、Linux `wl-copy`/`xclip`/`xsel`。
//! 3. OSC52：转义序列写 `/dev/tty`，穿透 alternate screen + SSH + tmux
//!    （alacritty / kitty / ghostty / wezterm 默认支持；tmux 需
//!    `set -g set-clipboard on`；GNOME Terminal / Konsole / xterm 默认关）。
//!
//! Clipboard copy: layered attempts, the first success wins.
//!
//! 1. Wayland: built-in Wayland clipboard (`wl-clipboard-rs`, pure Rust compiled
//!    into the binary; no dependency on external tools like `wl-copy`/`xclip`/
//!    `xsel`, nor on terminal OSC52 support).
//! 2. System clipboard tool: macOS `pbcopy`, Windows `clip.exe`, Linux
//!    `wl-copy`/`xclip`/`xsel`.
//! 3. OSC52: escape sequence written to `/dev/tty`, passing through alternate
//!    screen + SSH + tmux (alacritty / kitty / ghostty / wezterm by default;
//!    tmux needs `set -g set-clipboard on`; GNOME Terminal / Konsole / xterm off).

use std::io::Write;
use std::process::{Command, Stdio};

/// 把文本复制到剪贴板。分层尝试：Wayland 内置后端 → 系统剪贴板工具 → OSC52，
/// 任一成功即返回 `true`。
///
/// OSC52 写 `/dev/tty` 即使成功，终端也可能不处理该序列（不支持 OSC52、
/// 被嵌套会话吞掉等），因此系统工具也必须尝试。在 SSH 会话中系统工具
/// 可能不存在，此时仅靠 OSC52。
///
/// Copy text to the clipboard. Tries Wayland built-in backend, then a system
/// clipboard tool, then OSC52; returns `true` if any succeeded.
///
/// Even if OSC52 writes to `/dev/tty` successfully, the terminal may not
/// process the sequence (no OSC52 support, nested session swallowed it, etc.),
/// so the system tool must also be tried. In SSH sessions the system tool
/// may not exist, leaving OSC52 as the only path.
pub fn copy_to_clipboard(text: &str) -> bool {
    if copy_via_wayland(text) {
        return true;
    }
    let system_ok = copy_via_system_tool(text);
    let osc52_ok = copy_via_osc52(text);
    system_ok || osc52_ok
}

/// Wayland 内置剪贴板后端：仅当 `WAYLAND_DISPLAY` 已设置时启用。
/// 通过 `wl-clipboard-rs`（纯 Rust Wayland client）直接与 compositor 的
/// `ext-data-control`/`wlr-data-control` 协议通信，把数据 offer 为
/// `text/plain`（并附带 `UTF8_STRING` 等别名以便 XWayland 客户端粘贴）。
///
/// `Options::copy` 默认在后台线程持续服务粘贴请求（`serve_requests` 默认
/// Unlimited），调用即刻返回；data source 在其他应用接管剪贴板时自动销毁。
///
/// Built-in Wayland clipboard backend: active only when `WAYLAND_DISPLAY` is
/// set. Talks directly to the compositor's `ext-data-control`/`wlr-data-control`
/// protocol via `wl-clipboard-rs` (a pure-Rust Wayland client), offering the
/// data as `text/plain` (plus `UTF8_STRING` etc. aliases for XWayland clients).
///
/// `Options::copy` serves paste requests on a background thread by default
/// (`serve_requests` defaults to Unlimited) and returns immediately; the data
/// source is destroyed automatically when another app takes over the clipboard.
#[cfg(target_os = "linux")]
fn copy_via_wayland(text: &str) -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    let opts = wl_clipboard_rs::copy::Options::new();
    let bytes: Box<[u8]> = text.as_bytes().to_vec().into_boxed_slice();
    opts.copy(
        wl_clipboard_rs::copy::Source::Bytes(bytes),
        wl_clipboard_rs::copy::MimeType::Text,
    )
    .is_ok()
}

#[cfg(not(target_os = "linux"))]
fn copy_via_wayland(_text: &str) -> bool {
    false
}

/// OSC52：`\x1b]52;c;<base64>\x07` 写 `/dev/tty`。
/// 用 `/dev/tty` 而非 stdout/stderr，以穿透 alternate screen（stdout 被
/// ratatui 后端接管）和 SSH 透传。
///
/// OSC52: `\x1b]52;c;<base64>\x07` written to `/dev/tty`.
/// `/dev/tty` (not stdout/stderr) is used so the sequence passes through the
/// alternate screen (stdout is owned by the ratatui backend) and over SSH.
fn copy_via_osc52(text: &str) -> bool {
    let b64 = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x07");
    match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(mut f) => f.write_all(seq.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// 回退：调用系统剪贴板工具，通过 stdin 管道喂入文本。
/// Fallback: invoke a system clipboard tool, piping text via stdin.
fn copy_via_system_tool(text: &str) -> bool {
    let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("pbcopy", vec![])
    } else if cfg!(target_os = "windows") {
        ("clip.exe", vec![])
    } else {
        // Linux / BSD：Wayland 优先 wl-copy，否则 xclip，最后 xsel
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && command_exists("wl-copy") {
            ("wl-copy", vec![])
        } else if command_exists("xclip") {
            ("xclip", vec!["-selection", "clipboard"])
        } else {
            ("xsel", vec!["--clipboard", "--input"])
        }
    };

    Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()?;
            Ok(())
        })
        .map(|()| true)
        .unwrap_or(false)
}

/// 用 `sh -c 'command -v X'` 探测可执行文件是否存在。
/// Detect whether an executable is on PATH via `sh -c 'command -v X'`.
fn command_exists(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 最小 base64 编码器（RFC 4648 标准变体），避免引入 `base64` crate 依赖。
/// Minimal base64 encoder (RFC 4648 standard variant) to avoid pulling in the
/// `base64` crate as a dependency.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_one_byte() {
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_two_bytes() {
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn base64_three_bytes() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_known_vector_man() {
        // "Man" -> "TWFu"（RFC 4648 示例向量）
        assert_eq!(base64_encode(b"Man"), "TWFu");
    }

    #[test]
    fn base64_cjk_text() {
        // "你好" UTF-8 = E4 BD A0 E5 A5 BD -> base64
        assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn base64_padding_two_equals() {
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn base64_padding_one_equal() {
        assert_eq!(base64_encode(b"Ma"), "TWE=");
    }

    #[test]
    fn copy_to_clipboard_returns_bool_without_panic() {
        // 不断言成功（CI 沙箱无 tty/无剪贴板工具），只确保不 panic 且返回 bool。
        let _ = copy_to_clipboard("test");
    }
}
