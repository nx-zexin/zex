//! Zex 核心类型层。
//!
//! 本 crate **只包含类型定义与纯函数**：不依赖 tokio、不依赖任何 IO、
//! 不依赖 gpui。它是所有其他 crate 的公共词汇表。
//!
//! 若本 crate 的 `Cargo.toml` 里出现 `tokio` / `reqwest` / `rusqlite` /
//! `gpui-kit`，说明分层已被破坏（见 `docs/ARCHITECTURE.md` §3）。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod risk;
pub mod tool;

pub use error::{Error, Result};
pub use event::{ApprovalRequest, CoreEvent, EndReason, RememberScope, UiCommand};
pub use ids::{ApprovalId, SessionId, TaskId, ToolCallId, TurnId};
pub use message::{ImageRef, Message, Part, Role, ToolCall, Usage};
pub use risk::{Decision, Risk};
pub use tool::{Tool, ToolCtx, ToolOutput, ToolSpec};
