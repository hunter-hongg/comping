//! `tools` —— 内置工具实现。
//!
//! 每个工具都是 [`agent_core::Tool`] 的实现，权限等级由
//! [`registry_tools`] 统一决定，不在工具内部散落。

pub mod bash;
pub mod list_dir;
pub mod read_file;
pub mod write_file;
pub mod workspace;

use agent_core::Registry;
use agent_core::tools_exec::Risk;
use std::sync::Arc;

pub use workspace::Workspace;

/// 构造带全部内置工具的注册表。
///
/// `bash_allowlist` 是命令**首个词**的白名单：`["cargo"]` 放行
/// `cargo test --all`，但不会顺带放行 `rm`。空前缀时所有命令都要确认。
pub fn default_registry(ws: Workspace, bash_allowlist: Vec<String>) -> Registry {
    let mut reg = Registry::new();

    // 只读：直接跑
    reg.register(Arc::new(read_file::ReadFile::new(ws.clone())), Risk::Safe);
    reg.register(Arc::new(list_dir::ListDir::new(ws.clone())), Risk::Safe);
    reg.register(Arc::new(list_dir::Grep::new(ws.clone())), Risk::Safe);

    // 写与执行：要确认
    reg.register(Arc::new(write_file::WriteFile::new(ws.clone())), Risk::Confirm);
    reg.register(Arc::new(write_file::EditFile::new(ws.clone())), Risk::Confirm);
    reg.register(
        Arc::new(bash::Bash::new(ws).with_allowlist(bash_allowlist)),
        Risk::Confirm,
    );

    reg
}

/// 工具白名单配置：把指定工具永久降级为 Safe（免确认）。
pub fn apply_allowlist(reg: &mut Registry, tools: &[String]) {
    for t in tools {
        reg.set_risk(t, Risk::Safe);
    }
}

/// 工具黑名单：把指定工具设为 Deny，模型再也看不到它。
pub fn apply_blocklist(reg: &mut Registry, tools: &[String]) {
    for t in tools {
        reg.set_risk(t, Risk::Deny);
    }
}
