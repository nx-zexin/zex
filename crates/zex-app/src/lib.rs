//! Zex 组合根：GUI 与 headless 共享的内核装配。
//!
//! 本 crate 是唯一的「组合根」——只有这里可以同时看到所有 crate。
//! 它持有 gpui（UI）与 tokio（内核）两个世界的桥接（见 docs/ARCHITECTURE.md §2）。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod config;

use std::path::PathBuf;

pub use config::{ResolvedConfig, config_dir, load};
use zex_store::Store;

/// 主数据库路径 `~/.zex/zex.db`。
pub fn db_path() -> PathBuf {
    config_dir().join("zex.db")
}

/// 打开（或创建）主数据库。
pub fn open_store() -> zex_core::Result<Store> {
    Store::open(db_path())
}

/// 把已解析的配置装配成内核。
pub fn build_agent(cfg: &ResolvedConfig, cwd: PathBuf) -> zex_core::Result<zex_agent::Agent> {
    let store = open_store()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
}

/// 装配一个用内存数据库的内核（测试与 --ephemeral 模式）。
pub fn build_ephemeral_agent(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
) -> zex_core::Result<zex_agent::Agent> {
    let store = Store::in_memory()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
}
