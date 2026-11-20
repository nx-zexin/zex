//! Zex 的工具实现。
//!
//! 每个工具只声明 `NAME` / `RISK` / `Args` / `description` / `call`，
//! Schema 由 `schemars` 从 `Args` 自动生成（见 `zex_core::tool`）。
//!
//! **工具不自己判权限。** 风险等级由 `Tool::RISK` 声明，实际放行/审批由
//! `zex-policy` 决定。在工具里写 `if allowed { ... }` 是分层破坏。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod clipboard;
pub mod fs;
pub mod shell;
pub mod web;

use std::sync::Arc;

use zex_core::tool::ErasedTool;

/// 构建 M0 的工具集。
///
/// 能读、能写、能跑命令、能上网、能看剪贴板。
/// 这已经足够完成"帮我看看这个项目怎么跑起来"这类真实任务 —— 先窄后宽。
pub fn default_tools() -> Vec<Arc<dyn ErasedTool>> {
    vec![
        Arc::new(fs::FsRead),
        Arc::new(fs::FsWrite),
        Arc::new(shell::Shell),
        Arc::new(web::WebFetch),
        // 剪贴板：用户说「我复制了一段…」时，比让他再贴一遍靠谱。
        // 读是 L0（静默），写是 L2（覆盖掉的东西找不回来）。
        Arc::new(clipboard::ClipboardRead),
        Arc::new(clipboard::ClipboardWrite),
    ]
}
