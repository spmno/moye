// 内嵌默认提示词模块：把各角色的 prompt 文件在编译期打进二进制，
// Embedded default prompts: role prompt files are baked into the binary at compile time,
// 发布时不再依赖外部的 AGENTS.md / prompts/*.md 文件。
// so distribution no longer depends on external AGENTS.md / prompts/*.md files.
//
// 运行时优先从配置的路径加载本地提示词（便于用户自定义）；加载失败时回退到
// 这里内嵌的默认提示词，保证 Agent 始终带着完整纪律运行，而不是静默降级成
// 一句「你是 X agent。」。
// At runtime, local prompts at the configured path are loaded first (for customization);
// on failure these embedded defaults are used, so agents always run with full discipline
// instead of silently degrading to a bare "You are X agent".

use crate::registry::Role;

/// Orchestrator / 系统提示词（AGENTS.md）。
/// Orchestrator / system preamble (AGENTS.md).
pub const ORCHESTRATOR: &str = include_str!("../AGENTS.md");
/// 调查者 / Investigator.
pub const INVESTIGATOR: &str = include_str!("../prompts/investigator.md");
/// 规划者 / Planner.
pub const PLANNER: &str = include_str!("../prompts/planner.md");
/// 构建者 / Builder.
pub const BUILDER: &str = include_str!("../prompts/builder.md");
/// 审计者 / Auditor.
pub const AUDITOR: &str = include_str!("../prompts/auditor.md");

/// 返回某角色的内嵌默认提示词。
/// Returns the embedded default preamble for a role.
pub fn default_for(role: Role) -> &'static str {
    match role {
        Role::Orchestrator => ORCHESTRATOR,
        Role::Investigator => INVESTIGATOR,
        Role::Planner => PLANNER,
        Role::Builder => BUILDER,
        Role::Auditor => AUDITOR,
    }
}

/// 加载角色提示词：优先读本地文件，失败时回退到内嵌默认值并记一条警告。
/// Loads a role's preamble: local file first, embedded default on failure (with a warning).
pub fn load(role: Role, path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!(
                role = ?role,
                path = path,
                error = %e,
                "preamble file not found, using embedded default"
            );
            default_for(role).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_prompts_are_non_empty() {
        for role in [
            Role::Orchestrator,
            Role::Investigator,
            Role::Planner,
            Role::Builder,
            Role::Auditor,
        ] {
            assert!(
                !default_for(role).trim().is_empty(),
                "{role:?} embedded prompt is empty"
            );
        }
    }

    #[test]
    fn embedded_prompts_carry_role_discipline() {
        assert!(default_for(Role::Builder).contains("write_file"));
        assert!(default_for(Role::Auditor).contains("评审"));
        assert!(default_for(Role::Investigator).contains("调查报告"));
    }

    #[test]
    fn load_returns_embedded_default_when_file_missing() {
        let out = load(Role::Builder, "/nonexistent/moye-builder.md");
        assert_eq!(out, BUILDER);
    }

    #[test]
    fn load_prefers_local_file_when_present() {
        let dir = std::env::temp_dir();
        let path = dir.join("moye-test-preamble-4242.md");
        std::fs::write(&path, "CUSTOM-PREAMBLE").unwrap();
        let out = load(Role::Builder, path.to_str().unwrap());
        assert_eq!(out, "CUSTOM-PREAMBLE");
        std::fs::remove_file(&path).ok();
    }
}