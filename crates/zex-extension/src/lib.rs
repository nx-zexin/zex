//! 扩展宿主：把 gpui-shell 的脚本面板接进 Zex。
//!
//! # 分工
//!
//! | 这一层 | gpui-shell |
//! |---|---|
//! | 目录约定、清单解析（[`catalog`]） | 脚本求值、元素描述、样式、能力 |
//! | 到内核的请求/回包（[`bridge`]） | 宿主模块注册、`HostModule` |
//! | 面板实体、错误呈现 | `ScriptView`、`ShellRoot` |
//!
//! # 为什么单独一个 crate
//!
//! gpui-shell 自述 M0："脚本 API 预计会变"。把它关在 zex-app 之外的
//! 一层里，升级或替换上游时改动不会渗进 7000 行的 `bin/zex.rs`。
//!
//! # 能力默认全拒
//!
//! 上游的 `Capabilities::default()` 是空集，必须显式授予。这里同样不
//! 预设任何权限：装在 `~/.zex/extensions` 下的脚本默认碰不到文件、网络
//! 和剪贴板，只能画界面和调 [`bridge`] 里我们主动开放的那几个函数。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bridge;
pub mod catalog;

#[cfg(feature = "ui")]
pub mod host;

pub use bridge::{AgentBridge, Reply};
pub use catalog::{Extension, extensions_dir, scan, scan_default};

/// 给脚本看的宿主模块名。
///
/// 脚本里写 `import { ask } from "zex"`。
pub const HOST_MODULE: &str = "zex";
