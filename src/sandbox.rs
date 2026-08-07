// 沙箱模块：限制 Agent 的文件系统访问到项目根目录及其子目录。
// Sandbox module: restricts the Agent's file-system access to the project root and its subdirectories.
//
// Agent 默认只能访问当前工作目录（项目根目录）及其子目录。
// The Agent can only access the current working directory (project root) and its subdirectories by default.
// 如果需要访问其它目录，会通过 HITL 机制向用户请求授权，用户确认后该目录被加入授权列表。
// If access to other directories is needed, the Agent prompts the user via the HITL mechanism;
// once the user confirms, that directory is added to the authorized list.
//
// 可通过环境变量 MY_AGENT_SANDBOX=off 禁用沙箱（不推荐）。
// The sandbox can be disabled via the MY_AGENT_SANDBOX=off environment variable (not recommended).

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
}

impl Sandbox {
    /// 创建沙箱，以当前工作目录为根。
    /// Creates a sandbox with the current working directory as root.
    /// 通过 `MY_AGENT_SANDBOX=off` 可禁用。
    /// Can be disabled via `MY_AGENT_SANDBOX=off`.
    pub fn new() -> Self {
        let enabled = std::env::var("MY_AGENT_SANDBOX")
            .map(|v| !matches!(v.as_str(), "off" | "false" | "0"))
            .unwrap_or(true);
        let root = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("."));
        Self {
            root,
            authorized: Arc::new(Mutex::new(HashSet::new())),
            enabled,
        }
    }

    /// 创建沙箱并预授权一组目录（来自配置文件 `[sandbox].authorized_dirs`）。
    /// Creates a sandbox and pre-authorizes a set of directories
    /// (from the config file's `[sandbox].authorized_dirs`).
    ///
    /// 这些目录及其子目录可直接访问，无需弹窗确认。
    /// These directories and their subdirectories can be accessed without prompting.
    pub fn with_authorized_dirs(dirs: &[String]) -> Self {
        let sb = Self::new();
        if !sb.enabled {
            return sb;
        }
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

    /// 检查单个路径是否在沙箱内。
    /// Checks whether a single path is within the sandbox.
    ///
    /// 在沙箱内返回 `Ok(())`；在沙箱外返回 `Err(OutsideSandbox)`。
    /// Returns `Ok(())` if within the sandbox; `Err(OutsideSandbox)` otherwise.
    pub fn check_path(&self, path: &str) -> Result<(), SandboxError> {
        if !self.enabled {
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
            "read_file" | "edit_file" | "write_file" => {
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
            "read_file" | "edit_file" | "write_file" => {
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
fn expand_tilde(path: &str) -> String {
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

fn extract_paths_from_command_inner(command: &str, paths: &mut Vec<String>) {
    let outer = strip_command_substitutions(command, paths);

    for segment in outer.split(['|', ';', '&', '\n']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        let mut i = 0;
        while i < tokens.len() {
            let token = tokens[i];

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

            let candidate = if cleaned.starts_with('/')
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
        };
        assert!(sb.check_path("/etc/passwd").is_ok());
        assert!(sb.check_bash("cat /etc/passwd").is_ok());
        assert!(sb.check_tool("read_file", r#"{"path":"/etc/passwd"}"#).is_none());
    }
}
