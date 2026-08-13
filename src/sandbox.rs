// 沙箱模块：限制 Agent 的文件系统访问到项目根目录及其子目录。
// Sandbox module: restricts the Agent's file-system access to the project root and its subdirectories.
//
// Agent 默认只能访问当前工作目录（项目根目录）及其子目录。
// The Agent can only access the current working directory (project root) and its subdirectories by default.
// 如果需要访问其它目录，会通过 HITL 机制向用户请求授权，用户确认后该目录被加入授权列表。
// If access to other directories is needed, the Agent prompts the user via the HITL mechanism;
// once the user confirms, that directory is added to the authorized list.
//
// 两层防护：
// Two layers of protection:
// 1. 路径检查（HITL 门控）——所有工具调用前检查路径是否在沙箱内。
//    Path check (HITL gate) — checks all paths before tool execution.
// 2. OS 级沙箱（进程隔离）——run_bash 命令在 bwrap（Linux）或 sandbox-exec（macOS）中执行。
//    OS-level sandbox (process isolation) — run_bash commands execute inside bwrap (Linux) or sandbox-exec (macOS).
//
// 可通过环境变量 AGENT_SANDBOX=off 禁用沙箱（不推荐）。
// The sandbox can be disabled via the AGENT_SANDBOX=off environment variable (not recommended).
// 可通过 AGENT_SANDBOX_BACKEND=auto|bwrap|seatbelt|path|off 选择后端。
// Select the backend via AGENT_SANDBOX_BACKEND=auto|bwrap|seatbelt|path|off.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 沙箱错误类型。
/// Sandbox error type.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// 路径在沙箱之外，需要用户授权后才能访问。
    /// The path is outside the sandbox; user authorization is required.
    #[error("路径 '{path}' 在沙箱之外，需要用户授权后才能访问")]
    OutsideSandbox { path: String },
}

/// OS 级沙箱后端。
/// OS-level sandbox backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// 自动检测可用后端（Linux 优先 bwrap，macOS 优先 Seatbelt）。
    /// Auto-detect the best available backend (bwrap on Linux, Seatbelt on macOS).
    Auto,
    /// Linux: Bubblewrap（bwrap）——通过命名空间隔离文件系统。
    /// Linux: Bubblewrap (bwrap) — filesystem isolation via namespaces.
    Bwrap,
    /// macOS: Seatbelt（sandbox-exec）——系统策略级沙箱。
    /// macOS: Seatbelt (sandbox-exec) — system policy-level sandbox.
    Seatbelt,
    /// 仅路径检查，不使用 OS 级沙箱（原有行为）。
    /// Path checking only, no OS-level sandbox (original behavior).
    Path,
    /// 完全禁用沙箱。
    /// Sandbox completely disabled.
    Off,
}

impl SandboxBackend {
    /// 从字符串解析后端选择。
    /// Parse a backend choice from a string.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "auto" => SandboxBackend::Auto,
            "bwrap" | "bubblewrap" => SandboxBackend::Bwrap,
            "seatbelt" | "sandbox-exec" => SandboxBackend::Seatbelt,
            "path" | "path-only" => SandboxBackend::Path,
            "off" | "false" | "0" => SandboxBackend::Off,
            _ => SandboxBackend::Auto,
        }
    }

    /// 在当前平台上检测可用的 OS 级沙箱后端。
    /// Detect the available OS-level sandbox backend on the current platform.
    pub fn detect() -> SandboxBackend {
        #[cfg(target_os = "linux")]
        {
            if which("bwrap").is_some() {
                SandboxBackend::Bwrap
            } else {
                SandboxBackend::Path
            }
        }
        #[cfg(target_os = "macos")]
        {
            // sandbox-exec is always present on macOS.
            SandboxBackend::Seatbelt
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            SandboxBackend::Path
        }
    }

    /// 解析为实际使用的后端（Auto → 检测结果）。
    /// Resolve to the actual backend in use (Auto → detection result).
    pub fn resolve(self) -> SandboxBackend {
        match self {
            SandboxBackend::Auto => Self::detect(),
            other => other,
        }
    }
}

/// 在 PATH 中查找可执行文件。
/// Find an executable in PATH.
fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(cmd);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

/// 沙箱：限制 Agent 的文件系统访问范围。
/// Sandbox: restricts the Agent's file-system access scope.
///
/// `root` 是项目根目录（当前工作目录的规范化绝对路径）。
/// `root` is the project root (canonicalized absolute path of the current working directory).
/// `authorized` 是用户已手动授权的额外目录集合（通过 Arc<Mutex> 共享，Clone 时共享同一份）。
/// `authorized` is the set of extra directories the user has manually authorized
/// (shared via Arc<Mutex>, so clones share the same underlying set).
#[derive(Clone)]
pub struct Sandbox {
    /// 项目根目录（规范化后的绝对路径）。
    /// Project root (canonicalized absolute path).
    root: PathBuf,
    /// 用户已授权访问的额外目录（规范化后的绝对路径）。
    /// Extra directories authorized by the user (canonicalized absolute paths).
    authorized: Arc<Mutex<HashSet<PathBuf>>>,
    /// 是否启用沙箱。
    /// Whether the sandbox is enabled.
    enabled: bool,
    /// OS 级沙箱后端（已解析，非 Auto）。
    /// OS-level sandbox backend (resolved, not Auto).
    backend: SandboxBackend,
}

impl Sandbox {
    /// 创建沙箱，以当前工作目录为根。
    /// Creates a sandbox with the current working directory as root.
    /// 通过 `AGENT_SANDBOX=off` 可禁用。
    /// Can be disabled via `AGENT_SANDBOX=off`.
    pub fn new() -> Self {
        let enabled = std::env::var("AGENT_SANDBOX")
            .map(|v| !matches!(v.as_str(), "off" | "false" | "0"))
            .unwrap_or(true);
        let backend_env = std::env::var("AGENT_SANDBOX_BACKEND")
            .map(|v| SandboxBackend::parse(&v))
            .unwrap_or(SandboxBackend::Auto);
        let backend = if !enabled {
            SandboxBackend::Off
        } else {
            backend_env.resolve()
        };
        let root = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("."));
        Self {
            root,
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled,
            backend,
        }
    }

    /// 创建沙箱并预授权一组目录（来自配置文件 `[sandbox]` 配置）。
    /// Creates a sandbox and pre-authorizes a set of directories
    /// (from the config file's `[sandbox]` section).
    ///
    /// 这些目录及其子目录可直接访问，无需弹窗确认。
    /// These directories and their subdirectories can be accessed without prompting.
    pub fn with_authorized_dirs(dirs: &[String]) -> Self {
        Self::with_backend(dirs, SandboxBackend::Auto)
    }

    /// 创建沙箱，指定后端和预授权目录。
    /// Creates a sandbox with an explicit backend and pre-authorized directories.
    pub fn with_backend(dirs: &[String], backend: SandboxBackend) -> Self {
        let mut sb = Self::new();
        if !sb.enabled {
            return sb;
        }
        sb.backend = if backend == SandboxBackend::Auto {
            SandboxBackend::detect()
        } else {
            backend
        };
        for dir in dirs {
            sb.authorize(dir);
        }
        sb
    }

    /// 返回项目根目录。
    /// Returns the project root.
    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 返回沙箱是否启用。
    /// Returns whether the sandbox is enabled.
    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 返回 OS 级沙箱后端（已解析）。
    /// Returns the resolved OS-level sandbox backend.
    pub fn backend(&self) -> SandboxBackend {
        self.backend
    }

    /// 构建 OS 级沙箱的命令前缀 argv。
    /// Builds the OS-level sandbox command prefix argv.
    ///
    /// 返回 `Some(argv)` 时，工具应执行 `argv + [--, sh, -c, <command>]`。
    /// 返回 `None` 时，工具直接执行 `sh -c <command>`（无 OS 级沙箱）。
    /// Returns `Some(argv)` → tool runs `argv + [--, sh, -c, <command>]`.
    /// Returns `None` → tool runs `sh -c <command>` directly (no OS sandbox).
    ///
    /// bwrap 策略：只读绑定根文件系统，读写绑定工作区 + 已授权目录，
    /// 创建独立的 /dev、/proc、/tmp。`--die-with-parent` 确保父进程退出时子进程一并退出。
    /// bwrap policy: read-only bind of root fs, read-write bind of workspace + authorized dirs,
    /// fresh /dev, /proc, /tmp. `--die-with-parent` kills children when the parent exits.
    pub fn wrap_command(&self) -> Option<Vec<String>> {
        match self.backend {
            SandboxBackend::Bwrap => {
                let mut argv = vec!["bwrap".to_string()];
                argv.push("--ro-bind".into());
                argv.push("/".into());
                argv.push("/".into());
                argv.push("--bind".into());
                argv.push(self.root.to_string_lossy().to_string());
                argv.push(self.root.to_string_lossy().to_string());
                let authorized = self.authorized.lock().unwrap();
                for dir in authorized.iter() {
                    argv.push("--bind".into());
                    argv.push(dir.to_string_lossy().to_string());
                    argv.push(dir.to_string_lossy().to_string());
                }
                drop(authorized);
                argv.push("--dev".into());
                argv.push("/dev".into());
                argv.push("--proc".into());
                argv.push("/proc".into());
                argv.push("--tmpfs".into());
                argv.push("/tmp".into());
                argv.push("--die-with-parent".into());
                Some(argv)
            }
            SandboxBackend::Seatbelt => {
                let policy = self.seatbelt_policy();
                let argv = vec![
                    "sandbox-exec".to_string(),
                    "-p".into(),
                    policy,
                ];
                Some(argv)
            }
            _ => None,
        }
    }

    /// 生成 macOS Seatbelt 策略字符串。
    /// Generates the macOS Seatbelt policy string.
    fn seatbelt_policy(&self) -> String {
        let mut policy = String::from("(version 1)(allow default)(deny file-write*)");
        policy.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))",
            self.root.to_string_lossy()
        ));
        let authorized = self.authorized.lock().unwrap();
        for dir in authorized.iter() {
            policy.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))",
                dir.to_string_lossy()
            ));
        }
        policy
    }

    /// 检查单个路径是否在沙箱内。
    /// Checks whether a single path is within the sandbox.
    ///
    /// 在沙箱内返回 `Ok(())`；在沙箱外返回 `Err(OutsideSandbox)`。
    /// Returns `Ok(())` if within the sandbox; `Err(OutsideSandbox)` otherwise.
    pub fn check_path(&self, path: &str) -> Result<(), SandboxError> {
        if !self.enabled {
            return Ok(());
        }
        // /dev/null, /dev/stdin, /dev/stdout, /dev/stderr 是安全特殊设备文件，无需授权。
        // /dev/null, /dev/stdin, /dev/stdout, /dev/stderr are safe special device files, no authorization needed.
        if matches!(
            self.resolve_path(path).to_str(),
            Some("/dev/null") | Some("/dev/stdin") | Some("/dev/stdout") | Some("/dev/stderr")
        ) {
            return Ok(());
        }
        let resolved = self.resolve_path(path);
        if self.is_within_sandbox(&resolved) {
            Ok(())
        } else {
            Err(SandboxError::OutsideSandbox {
                path: path.to_string(),
            })
        }
    }

    /// 授权一个目录——之后该目录及其子目录均可访问。
    /// Authorizes a directory — afterwards, it and its subdirectories are accessible.
    #[allow(dead_code)]
    pub fn authorize(&self, dir: &str) {
        if !self.enabled {
            return;
        }
        let resolved = self.resolve_path(dir);
        let canon = self.canonicalize_safe(&resolved);
        self.authorized.lock().unwrap().insert(canon);
    }

    /// 检查 bash 命令是否访问沙箱外的路径。
    /// Checks whether a bash command accesses paths outside the sandbox.
    ///
    /// 从命令中提取所有路径类 token，逐一检查。返回第一个越界路径的错误。
    /// Extracts all path-like tokens from the command and checks each one.
    /// Returns the error for the first out-of-bounds path found.
    pub fn check_bash(&self, command: &str) -> Result<(), SandboxError> {
        if !self.enabled {
            return Ok(());
        }
        for path in extract_paths_from_command(command) {
            if let Err(e) = self.check_path(&path) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// 检查工具调用是否访问沙箱外的路径。
    /// Checks whether a tool call accesses paths outside the sandbox.
    ///
    /// 从工具名和 JSON 参数中提取路径并检查。
    /// Extracts paths from the tool name and JSON args, then checks them.
    /// 返回 `Some(error)` 表示需要用户授权；`None` 表示在沙箱内或工具不涉及文件访问。
    /// Returns `Some(error)` if user authorization is needed;
    /// `None` if within the sandbox or the tool doesn't access files.
    pub fn check_tool(&self, tool_name: &str, args: &str) -> Option<SandboxError> {
        if !self.enabled {
            return None;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(args).ok()?;
        match tool_name {
            "read_file" | "edit_file" | "write_file" | "run_file" => {
                let path = parsed.get("path")?.as_str()?;
                self.check_path(path).err()
            }
            "run_bash" => {
                let command = parsed.get("command")?.as_str()?;
                self.check_bash(command).err()
            }
            // web_fetch / web_search 不涉及本地文件系统访问，无需沙箱检查。
            // web_fetch / web_search don't access the local filesystem; no sandbox check needed.
            _ => None,
        }
    }

    /// 从工具调用参数中提取所有路径并授权其父目录。
    /// Extracts all paths from the tool call args and authorizes their parent directories.
    ///
    /// 用户确认授权后调用此方法，将涉及的目录加入授权列表，
    /// Called after the user confirms authorization; adds the involved directories
    /// to the authorized list so subsequent accesses to the same area don't re-prompt.
    pub fn authorize_tool(&self, tool_name: &str, args: &str) {
        if !self.enabled {
            return;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(args).ok();
        match tool_name {
            "read_file" | "edit_file" | "write_file" | "run_file" => {
                if let Some(path) = parsed
                    .as_ref()
                    .and_then(|v| v.get("path"))
                    .and_then(|v| v.as_str())
                {
                    self.authorize_path(path);
                }
            }
            "run_bash" => {
                if let Some(command) = parsed
                    .as_ref()
                    .and_then(|v| v.get("command"))
                    .and_then(|v| v.as_str())
                {
                    for path in extract_paths_from_command(command) {
                        self.authorize_path(&path);
                    }
                }
            }
            _ => {}
        }
    }

    /// 授权单个路径的父目录。
    /// Authorizes the parent directory of a single path.
    fn authorize_path(&self, path: &str) {
        let resolved = self.resolve_path(path);
        if let Some(parent) = resolved.parent() {
            let canon = self.canonicalize_safe(parent);
            self.authorized.lock().unwrap().insert(canon);
        }
    }

    /// 将路径解析为绝对路径（相对于项目根目录）。
    /// Resolves a path to an absolute path (relative to the project root).
    fn resolve_path(&self, path: &str) -> PathBuf {
        let expanded = expand_tilde(path);
        let p = Path::new(&expanded);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        }
    }

    /// 检查路径是否在沙箱内（项目根目录或已授权目录之下）。
    /// Checks whether a path is within the sandbox (under root or an authorized directory).
    fn is_within_sandbox(&self, path: &Path) -> bool {
        let canon = self.canonicalize_safe(path);
        // 项目根目录及其子目录
        // Project root and its subdirectories
        if canon.starts_with(&self.root) {
            return true;
        }
        // 已授权目录及其子目录
        // Authorized directories and their subdirectories
        let authorized = self.authorized.lock().unwrap();
        for dir in authorized.iter() {
            if canon.starts_with(dir) {
                return true;
            }
        }
        false
    }

    /// 安全地规范化路径。如果路径不存在（如 write_file 创建新文件），
    /// Safely canonicalizes a path. If the path doesn't exist (e.g. write_file creating a new file),
    /// 则规范化其父目录再拼接文件名。
    /// canonicalizes the parent directory and appends the filename.
    fn canonicalize_safe(&self, path: &Path) -> PathBuf {
        // 直接规范化（路径存在时）
        // Direct canonicalize (when the path exists)
        if let Ok(canon) = path.canonicalize() {
            return canon;
        }
        // 路径不存在时，规范化父目录再拼接文件名
        // When the path doesn't exist, canonicalize the parent and append the filename
        if let Some(parent) = path.parent() {
            if let Ok(canon_parent) = parent.canonicalize() {
                let filename = path.file_name().unwrap_or_default();
                return canon_parent.join(filename);
            }
        }
        // 最后兜底：返回原始路径
        // Last resort: return the path as-is
        path.to_path_buf()
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// 展开 `~` 为用户主目录。
/// Expands `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// 从 bash 命令中提取所有路径类 token。
/// Extracts all path-like tokens from a bash command.
///
/// 先剥离命令替换（`$(...)` / 反引号），把内部命令当作独立命令递归检查，
/// 再检查外层命令的路径类 token。这能捕获 `ls $(rm -rf ~)` 这类绕过——
/// 旧实现只 tokenize 外层，`~)` 和 `$(rm` 都不被识别，导致沙箱放行。
/// First strips command substitutions ($(...) / backticks) and recursively checks
/// their inner commands as standalone commands, then checks path-like tokens of the
/// outer command. This catches bypasses like `ls $(rm -rf ~)` — the old
/// implementation only tokenized the outer layer, so neither `~)` nor `$(rm` was
/// recognized and the sandbox let it through.
///
/// 识别以下模式：
/// Recognizes the following patterns:
/// - 绝对路径（以 `/` 开头）/ Absolute paths (starting with `/`)
/// - 主目录路径（`~`、`~/`）/ Home-directory paths (`~`, `~/`)
/// - 含 `..` 的相对路径（可能逃逸沙箱）/ Relative paths containing `..` (may escape the sandbox)
/// - `cd` / `pushd` 的目标参数 / The target argument of `cd` / `pushd`
/// - 环境变量展开（`$HOME`、`${HOME}/x`、`$HOME/.zshrc`）/ Env-var expansion (`$HOME`, `${HOME}/x`, `$HOME/.zshrc`)
fn extract_paths_from_command(command: &str) -> Vec<String> {
    let mut paths = Vec::new();
    extract_paths_from_command_inner(command, &mut paths);
    paths
}

/// 从命令中剥离 heredoc 内容，只保留 shell 命令部分。
/// Heredoc 内容（`<< 'DELIM'` 到 `DELIM` 之间的行）是数据，不是命令，
/// 不应被路径提取扫描。
///
/// Strips heredoc bodies from a multi-line command, keeping only the shell
/// command parts. Heredoc content (lines between `<< 'DELIM'` and `DELIM`)
/// is data, not command, and must not be scanned for path-like tokens.
fn strip_heredocs(command: &str) -> String {
    let mut result = String::with_capacity(command.len());
    let mut heredoc_delim: Option<String> = None;

    for line in command.lines() {
        if let Some(ref delim) = heredoc_delim {
            if line.trim() == delim.as_str() {
                heredoc_delim = None;
            }
            continue;
        }

        match find_heredoc_start(line) {
            Some((prefix, delim)) => {
                result.push_str(&prefix);
                result.push('\n');
                heredoc_delim = Some(delim);
            }
            None => {
                result.push_str(line);
                result.push('\n');
            }
        }
    }

    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// 在单行中查找 heredoc 起始（`<< DELIM`），返回 `(<< 之前的部分, 定界符)`。
/// 排除 `<<<`（here-string）。处理 `<<-`、`<< 'DELIM'`、`<< "DELIM"`、`<< DELIM`。
///
/// Finds heredoc start (`<< DELIM`) in a single line. Returns `(prefix, delimiter)`
/// or `None`. Excludes `<<<` (here-string). Handles `<<-`, quoted/unquoted delimiters.
fn find_heredoc_start(line: &str) -> Option<(String, String)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            if i + 2 < bytes.len() && bytes[i + 2] == b'<' {
                i += 3;
                continue;
            }

            let prefix = line[..i].to_string();
            let rest = &line[i + 2..];
            let rest = rest.strip_prefix('-').unwrap_or(rest);
            let rest = rest.trim_start();

            if rest.is_empty() {
                return None;
            }

            let delim = if let Some(r) = rest.strip_prefix('\'') {
                r.split('\'').next().unwrap_or("").to_string()
            } else if let Some(r) = rest.strip_prefix('"') {
                r.split('"').next().unwrap_or("").to_string()
            } else {
                rest.split_whitespace().next().unwrap_or("").to_string()
            };

            if !delim.is_empty() {
                return Some((prefix, delim));
            }
        }
        i += 1;
    }
    None
}

fn extract_paths_from_command_inner(command: &str, paths: &mut Vec<String>) {
    let stripped = strip_heredocs(command);
    let outer = strip_command_substitutions(&stripped, paths);

    for segment in outer.split(['|', ';', '&', '\n']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        // Skip comment lines (first token starts with '#').
        // 跳过注释行（首个 token 以 '#' 开头）。
        if tokens[0].starts_with('#') {
            continue;
        }

        let mut i = 0;
        while i < tokens.len() {
            let token = tokens[i];

            // Inline comment: '#' at word boundary starts a comment to end of segment.
            // 行内注释： '#' 在词边界处开始注释，跳过本段剩余 token。
            if token.starts_with('#') {
                break;
            }

            // cd / pushd <dir> —— 检查目标目录
            // cd / pushd <dir> — check the target directory
            if (token == "cd" || token == "pushd") && i + 1 < tokens.len() {
                let target = clean_token(tokens[i + 1]);
                if !target.is_empty() && target != "-" {
                    paths.push(target.to_string());
                }
                i += 2;
                continue;
            }

            let cleaned = clean_token(token);

            let candidate = if (cleaned.starts_with('/') && cleaned != "/")
                || cleaned.starts_with("~/")
                || cleaned == "~"
                || cleaned.contains("..")
            {
                Some(cleaned.to_string())
            } else if cleaned.starts_with('$') {
                expand_env_var_token(cleaned)
            } else {
                None
            };
            if let Some(p) = candidate {
                paths.push(p);
            }

            i += 1;
        }
    }
}

/// 剥离命令替换并把内部命令递归送入路径检查。返回移除替换后的外层命令
/// （替换处留空格，避免 token 粘连误判）。不匹配闭合时按无替换处理。
/// Strips command substitutions, recursively feeding inner commands into path
/// checking. Returns the outer command with substitutions removed (replaced by a
/// space so tokens don't glue together). Unclosed substitutions are left as-is.
fn strip_command_substitutions(command: &str, paths: &mut Vec<String>) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
            let mut depth = 1usize;
            let mut j = i + 2;
            while j < chars.len() {
                if chars[j] == '$' && j + 1 < chars.len() && chars[j + 1] == '(' {
                    depth += 1;
                    j += 2;
                    continue;
                }
                if chars[j] == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            let inner: String = chars[i + 2..j].iter().collect();
            extract_paths_from_command_inner(&inner, paths);
            out.push(' ');
            i = j + 1;
            continue;
        }
        if chars[i] == '`' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '`' {
                j += 1;
            }
            let inner: String = chars[i + 1..j].iter().collect();
            extract_paths_from_command_inner(&inner, paths);
            out.push(' ');
            i = j + 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 展开环境变量 token（$HOME、${HOME}、$HOME/.zshrc、${HOME}/x）。
/// 已知变量用进程环境展开；PATH 类冒号分隔列表跳过以免误报；
/// 未知变量仅在带路径后缀时保守标记（触发沙箱询问）。
/// Expands an env-var token ($HOME, ${HOME}, $HOME/.zshrc, ${HOME}/x).
/// Known vars are expanded from the process environment; colon-separated lists
/// like $PATH are skipped to avoid false positives; unknown vars are conservatively
/// flagged only when followed by a path-like suffix.
fn expand_env_var_token(token: &str) -> Option<String> {
    if token == "$" {
        return None;
    }
    let rest = &token[1..];
    let (name, suffix) = if let Some(rest2) = rest.strip_prefix('{') {
        rest2.split_once('}')?
    } else {
        let idx = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        rest.split_at(idx)
    };
    if name.is_empty() {
        return None;
    }
    if let Ok(val) = std::env::var(name) {
        if val.is_empty() {
            return None;
        }
        let expanded = format!("{val}{suffix}");
        if expanded.contains(':') {
            return None;
        }
        Some(expanded)
    } else {
        if suffix.is_empty() {
            None
        } else {
            Some(format!("/${{{name}}}{suffix}"))
        }
    }
}

/// 清理 token：去除 shell 重定向操作符和引号。
/// Cleans a token: strips shell redirection operators and quotes.
fn clean_token(token: &str) -> &str {
    let mut s = token;
    // 去除前导重定向操作符：>, >>, <, 2>, &>
    // Strip leading redirection operators: >, >>, <, 2>, &>
    loop {
        if s.starts_with(">>") || s.starts_with("2>") || s.starts_with("&>") {
            s = &s[2..];
        } else if s.starts_with('>') || s.starts_with('<') {
            s = &s[1..];
        } else {
            break;
        }
    }
    // 去除首尾引号
    // Strip surrounding quotes
    s = s.trim_matches(|c| c == '"' || c == '\'');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 绝对路径应被提取。
    /// Absolute paths should be extracted.
    #[test]
    fn extract_absolute_path() {
        let paths = extract_paths_from_command("cat /etc/passwd");
        assert!(paths.contains(&"/etc/passwd".to_string()));
    }

    /// cd 目标应被提取。
    /// cd targets should be extracted.
    #[test]
    fn extract_cd_target() {
        let paths = extract_paths_from_command("cd /tmp && ls");
        assert!(paths.contains(&"/tmp".to_string()));
    }

    /// 含 .. 的相对路径应被提取。
    /// Relative paths with .. should be extracted.
    #[test]
    fn extract_dotdot_path() {
        let paths = extract_paths_from_command("ls ../parent");
        assert!(paths.contains(&"../parent".to_string()));
    }

    /// 重定向目标路径应被提取（去除操作符后）。
    /// Redirection target paths should be extracted (after stripping operators).
    #[test]
    fn extract_redirection_path() {
        let paths = extract_paths_from_command("echo data > /tmp/file");
        assert!(paths.contains(&"/tmp/file".to_string()));
    }

    /// clean_token 应去除重定向操作符和引号。
    /// clean_token should strip redirection operators and quotes.
    #[test]
    fn clean_token_strips_redirection() {
        assert_eq!(clean_token(">/tmp/file"), "/tmp/file");
        assert_eq!(clean_token(">>/tmp/file"), "/tmp/file");
        assert_eq!(clean_token("\"/tmp/file\""), "/tmp/file");
        assert_eq!(clean_token("'/tmp/file'"), "/tmp/file");
    }

    /// 管道中的多个路径应都被提取。
    /// Multiple paths in a pipeline should all be extracted.
    #[test]
    fn extract_multiple_paths() {
        let paths = extract_paths_from_command("cp /etc/passwd /tmp/copy");
        assert!(paths.contains(&"/etc/passwd".to_string()));
        assert!(paths.contains(&"/tmp/copy".to_string()));
    }

    /// 命令替换内的路径应被提取（`ls $(rm -rf ~)` 中的 `~`）。
    /// Paths inside command substitutions should be extracted (the `~` in
    /// `ls $(rm -rf ~)`).
    #[test]
    fn extract_paths_in_command_substitution() {
        let paths = extract_paths_from_command("ls $(rm -rf ~)");
        assert!(paths.contains(&"~".to_string()));
    }

    /// 嵌套命令替换内的路径应被提取。
    /// Paths inside nested command substitutions should be extracted.
    #[test]
    fn extract_paths_in_nested_substitution() {
        let paths = extract_paths_from_command("echo $(cat $(ls /etc))");
        assert!(paths.contains(&"/etc".to_string()));
    }

    /// 反引号命令替换内的路径应被提取。
    /// Paths inside backtick command substitutions should be extracted.
    #[test]
    fn extract_paths_in_backticks() {
        let paths = extract_paths_from_command("ls `cat /tmp/x`");
        assert!(paths.contains(&"/tmp/x".to_string()));
    }

    /// `$HOME` 环境变量展开的路径应被提取。
    /// Paths expanded from the $HOME env var should be extracted.
    #[test]
    fn extract_home_env_var_path() {
        let paths = extract_paths_from_command("cat $HOME/.zshrc");
        let home = std::env::var("HOME").expect("HOME must be set in test env");
        assert!(paths.iter().any(|p| p.starts_with(&format!("{home}/"))));
    }

    /// 裸 `$HOME` 应被提取。
    /// A bare $HOME should be extracted.
    #[test]
    fn extract_home_env_var_bare() {
        let paths = extract_paths_from_command("ls $HOME");
        let home = std::env::var("HOME").expect("HOME must be set in test env");
        assert!(paths.iter().any(|p| p == &home));
    }

    /// `${HOME}` 花括号形式应被提取。
    /// The ${HOME} brace form should be extracted.
    #[test]
    fn extract_braced_env_var_path() {
        let paths = extract_paths_from_command("cat ${HOME}/.config/app.toml");
        let home = std::env::var("HOME").expect("HOME must be set in test env");
        assert!(paths.iter().any(|p| p.starts_with(&format!("{home}/"))));
    }

    /// 项目内相对路径不应触发沙箱拒绝。
    /// Relative paths within the project should not trigger a sandbox rejection.
    #[test]
    fn within_sandbox_relative_path() {
        let sb = Sandbox::new();
        // src/ 是项目根目录的子目录，应在沙箱内
        // src/ is a subdirectory of the project root, should be within the sandbox
        assert!(sb.check_path("src/main.rs").is_ok());
        assert!(sb.check_path("./Cargo.toml").is_ok());
    }

    /// 绝对路径在项目根目录之外应被拒绝。
    /// Absolute paths outside the project root should be rejected.
    #[test]
    fn outside_sandbox_absolute_path() {
        let sb = Sandbox::new();
        // /etc 在项目根目录之外
        // /etc is outside the project root
        assert!(sb.check_path("/etc/passwd").is_err());
    }

    /// 授权后路径应可访问。
    /// After authorization, the path should be accessible.
    #[test]
    fn authorize_grants_access() {
        let sb = Sandbox::new();
        // 授权前应被拒绝
        // Should be rejected before authorization
        assert!(sb.check_path("/tmp/test.txt").is_err());
        // 授权 /tmp 目录
        // Authorize /tmp
        sb.authorize("/tmp");
        // 授权后应可访问
        // Should be accessible after authorization
        assert!(sb.check_path("/tmp/test.txt").is_ok());
        assert!(sb.check_path("/tmp/sub/dir/file.txt").is_ok());
    }

    /// 含 .. 的路径逃逸沙箱应被拒绝。
    /// Paths with .. that escape the sandbox should be rejected.
    #[test]
    fn dotdot_escape_rejected() {
        let sb = Sandbox::new();
        // ../something 应解析到项目根目录之外
        // ../something should resolve outside the project root
        assert!(sb.check_path("../something").is_err());
    }

    /// bash 命令中的沙箱外路径应被检测。
    /// Out-of-sandbox paths in bash commands should be detected.
    #[test]
    fn bash_outside_sandbox_detected() {
        let sb = Sandbox::new();
        assert!(sb.check_bash("cat /etc/passwd").is_err());
        assert!(sb.check_bash("cd /tmp && ls").is_err());
        assert!(sb.check_bash("ls src/").is_ok());
    }

    /// 命令替换与环境变量绕过应在沙箱层被检测（而非仅 HITL 层）。
    /// Command-substitution and env-var bypasses should be caught at the sandbox
    /// layer, not just at the HITL layer.
    #[test]
    fn bash_bypass_detected() {
        let sb = Sandbox::new();
        assert!(sb.check_bash("ls $(rm -rf ~)").is_err());
        assert!(sb.check_bash("cat $HOME/.zshrc").is_err());
        assert!(sb.check_bash("ls `cat /tmp/x`").is_err());
    }

    /// check_tool 应正确提取文件工具的路径参数。
    /// check_tool should correctly extract the path arg from file tools.
    #[test]
    fn check_tool_file_path() {
        let sb = Sandbox::new();
        assert!(sb
            .check_tool("read_file", r#"{"path":"/etc/passwd"}"#)
            .is_some());
        assert!(sb
            .check_tool("read_file", r#"{"path":"src/main.rs"}"#)
            .is_none());
    }

    /// check_tool 应正确检查 bash 命令。
    /// check_tool should correctly check bash commands.
    #[test]
    fn check_tool_bash() {
        let sb = Sandbox::new();
        assert!(sb
            .check_tool("run_bash", r#"{"command":"cat /etc/passwd"}"#)
            .is_some());
        assert!(sb
            .check_tool("run_bash", r#"{"command":"ls -la"}"#)
            .is_none());
    }

    /// check_tool 对不涉及文件的工具应返回 None。
    /// check_tool should return None for tools that don't access files.
    #[test]
    fn check_tool_web_tools() {
        let sb = Sandbox::new();
        assert!(sb
            .check_tool("web_fetch", r#"{"url":"https://example.com"}"#)
            .is_none());
        assert!(sb
            .check_tool("web_search", r#"{"query":"test"}"#)
            .is_none());
    }

    /// 禁用沙箱后所有检查应通过。
    /// When the sandbox is disabled, all checks should pass.
    #[test]
    fn disabled_sandbox_allows_all() {
        let sb = Sandbox {
            root: PathBuf::from("."),
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled: false,
            backend: SandboxBackend::Off,
        };
        assert!(sb.check_path("/etc/passwd").is_ok());
        assert!(sb.check_bash("cat /etc/passwd").is_ok());
        assert!(sb.check_tool("read_file", r#"{"path":"/etc/passwd"}"#).is_none());
    }

    #[test]
    fn backend_parse_strings() {
        assert_eq!(SandboxBackend::parse("auto"), SandboxBackend::Auto);
        assert_eq!(SandboxBackend::parse("BWRAP"), SandboxBackend::Bwrap);
        assert_eq!(SandboxBackend::parse("bubblewrap"), SandboxBackend::Bwrap);
        assert_eq!(SandboxBackend::parse("seatbelt"), SandboxBackend::Seatbelt);
        assert_eq!(SandboxBackend::parse("path"), SandboxBackend::Path);
        assert_eq!(SandboxBackend::parse("off"), SandboxBackend::Off);
        assert_eq!(SandboxBackend::parse("unknown"), SandboxBackend::Auto);
    }

    #[test]
    fn backend_resolve_auto() {
        let auto = SandboxBackend::Auto;
        let resolved = auto.resolve();
        assert_ne!(resolved, SandboxBackend::Auto);
    }

    #[test]
    fn wrap_command_path_backend_returns_none() {
        let sb = Sandbox {
            root: PathBuf::from("/tmp"),
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled: true,
            backend: SandboxBackend::Path,
        };
        assert!(sb.wrap_command().is_none());
    }

    #[test]
    fn wrap_command_off_returns_none() {
        let sb = Sandbox {
            root: PathBuf::from("/tmp"),
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled: false,
            backend: SandboxBackend::Off,
        };
        assert!(sb.wrap_command().is_none());
    }

    #[test]
    fn wrap_command_bwrap_includes_root_and_workspace() {
        let workspace = std::env::current_dir().unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let sb = Sandbox {
            root: workspace.clone(),
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled: true,
            backend: SandboxBackend::Bwrap,
        };
        let argv = sb.wrap_command().expect("bwrap should produce argv");
        assert_eq!(argv[0], "bwrap");
        assert!(argv.contains(&"--ro-bind".to_string()));
        assert!(argv.contains(&"--dev".to_string()));
        assert!(argv.contains(&"--proc".to_string()));
        assert!(argv.contains(&"--tmpfs".to_string()));
        assert!(argv.contains(&"--die-with-parent".to_string()));
        let ws_str = workspace.to_string_lossy().to_string();
        assert!(argv.contains(&ws_str), "workspace path must appear in bwrap argv");
    }

    #[test]
    fn wrap_command_bwrap_includes_authorized_dirs() {
        let workspace = std::env::current_dir().unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let sb = Sandbox {
            root: workspace,
            authorized: Arc::new(Mutex::new(HashSet::from([
                PathBuf::from("/tmp/authorized"),
            ]))),
            enabled: true,
            backend: SandboxBackend::Bwrap,
        };
        let argv = sb.wrap_command().expect("bwrap should produce argv");
        assert!(argv.contains(&"/tmp/authorized".to_string()));
    }

    #[test]
    fn seatbelt_policy_includes_workspace() {
        let sb = Sandbox {
            root: PathBuf::from("/home/user/project"),
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled: true,
            backend: SandboxBackend::Seatbelt,
        };
        let policy = sb.seatbelt_policy();
        assert!(policy.contains("deny file-write*"));
        assert!(policy.contains("/home/user/project"));
    }

    /// Comment lines starting with '#' should not produce paths.
    /// 以 '#' 开头的注释行不应产生路径。
    #[test]
    fn skip_comment_lines() {
        let paths = extract_paths_from_command("# Test 1: GET / should redirect to /log");
        assert!(!paths.contains(&"/".to_string()));
        assert!(!paths.contains(&"/log".to_string()));
    }

    /// Inline comments after '#' should not produce paths.
    /// 行内 '#' 之后的注释不应产生路径。
    #[test]
    fn skip_inline_comments() {
        let paths = extract_paths_from_command("echo hello # GET / should redirect");
        assert!(!paths.contains(&"/".to_string()));
    }

    /// Bare '/' token should not be treated as an absolute path.
    /// 裸 '/' token 不应被当作绝对路径。
    #[test]
    fn skip_bare_root_slash() {
        let paths = extract_paths_from_command("echo --- GET / (redirect) ---");
        assert!(!paths.contains(&"/".to_string()));
    }

    /// cd / should still extract '/' as a real directory change.
    /// cd / 仍应提取 '/' 作为真实的目录切换。
    #[test]
    fn cd_root_still_extracted() {
        let paths = extract_paths_from_command("cd / && ls");
        assert!(paths.contains(&"/".to_string()));
    }

    /// Real absolute paths should still be extracted even in echo strings.
    /// 真实的绝对路径即使在 echo 字符串中也应被提取。
    #[test]
    fn real_abs_path_still_extracted() {
        let paths = extract_paths_from_command("echo GET /etc/passwd");
        assert!(paths.contains(&"/etc/passwd".to_string()));
    }

    /// Heredoc content should not be scanned for paths.
    /// `//!` in a Rust heredoc should not be treated as a path.
    /// Heredoc 内容不应被扫描路径。Rust 文档注释 `//!` 不应被当作路径。
    #[test]
    fn heredoc_content_not_scanned() {
        let cmd = "cat > ~/project/src/main.rs << 'RUSTEOF'\n//! doc comment\n/// another comment\nRUSTEOF\necho done";
        let paths = extract_paths_from_command(cmd);
        assert!(!paths.iter().any(|p| p.contains("//!")));
        assert!(!paths.iter().any(|p| p.contains("///")));
    }

    /// Paths before the heredoc should still be extracted.
    /// Heredoc 之前的路径仍应被提取。
    #[test]
    fn heredoc_prefix_path_extracted() {
        let cmd = "cat > ~/project/src/main.rs << 'EOF'\nsome content\nEOF";
        let paths = extract_paths_from_command(cmd);
        assert!(paths.iter().any(|p| p.contains("src/main.rs")));
    }

    /// Commands after the heredoc closing delimiter should be processed.
    /// Heredoc 结束定界符之后的命令应被正常处理。
    #[test]
    fn heredoc_suffix_command_processed() {
        let cmd = "cat << 'EOF'\ncontent\nEOF\ncat /etc/hostname";
        let paths = extract_paths_from_command(cmd);
        assert!(paths.contains(&"/etc/hostname".to_string()));
    }

    /// `<<<` (here-string) should not trigger heredoc stripping.
    /// `<<<`（here-string）不应触发 heredoc 剥离。
    #[test]
    fn here_string_not_stripped() {
        let result = strip_heredocs("cat <<< /etc/passwd");
        assert!(result.contains("/etc/passwd"));
    }

    /// Unquoted heredoc delimiter should work.
    /// 无引号的 heredoc 定界符应正确工作。
    #[test]
    fn heredoc_unquoted_delim() {
        let cmd = "cat << EOF\n//! not a path\nEOF\necho done";
        let paths = extract_paths_from_command(cmd);
        assert!(!paths.iter().any(|p| p.contains("//!")));
    }

    /// `<<-` variant should work.
    /// `<<-` 变体应正确工作。
    #[test]
    fn heredoc_dash_variant() {
        let cmd = "cat <<- 'EOF'\n//! not a path\nEOF\necho done";
        let paths = extract_paths_from_command(cmd);
        assert!(!paths.iter().any(|p| p.contains("//!")));
    }

    /// No heredoc — normal command should be unaffected.
    /// 无 heredoc 时，普通命令不受影响。
    #[test]
    fn strip_heredocs_no_heredoc() {
        let result = strip_heredocs("ls /etc/passwd");
        assert_eq!(result, "ls /etc/passwd");
    }

    /// Basic heredoc stripping test.
    #[test]
    fn strip_heredocs_basic() {
        let cmd = "cat > file.rs << 'EOF'\n//! content\nEOF\necho done";
        let result = strip_heredocs(cmd);
        assert!(result.contains("cat > file.rs"));
        assert!(!result.contains("//! content"));
        assert!(result.contains("echo done"));
    }
}
