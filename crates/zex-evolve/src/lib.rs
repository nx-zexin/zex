//! 自我进化：distill / 提案 / 验证 / 落盘
//!
//! 状态：M0 骨架占位，实现见 `docs/M0-PLAN.md`。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]
