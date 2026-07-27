// 动态工具模块：由 ToolManifest（evolution/tool_ext.rs）在 add_tool 时重新生成。
// 初始版本为空；每次 /add-tool 后 regenerate_mod_rs 会覆写本文件，
// 声明新的 pub mod 并在 load_all() 中注册。下次 cargo build 即编译进项目。
use rig_core::tool::ToolDyn;

/// 返回所有动态工具。工具为空时返回空 vec。
/// 本函数在编译期确定工具列表——add_tool 生成的新工具需要重新 cargo build 才生效。
pub fn load_all() -> Vec<Box<dyn ToolDyn>> {
    vec![]
}
