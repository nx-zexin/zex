//! Zex 的持久化层。**唯一的数据入口。**
//!
//! 一个 SQLite 库承载全部状态：会话、消息、审计、授权（ADR-005）。
//! 理由：Zebra 把状态散在 `memory.db` + 6 个 JSON + 一个独立知识库服务里，
//! 导致备份要记 6 个地方、迁移没有统一版本号、跨模块事务不存在。
//!
//! ## 设计约束
//!
//! - **同步 API。** `rusqlite` 是同步的；本地 SQLite 单条读写通常 <1ms，
//!   对桌面应用完全可接受。调用方在 async 上下文里应包 `spawn_blocking`，
//!   但不要为每一条 SQL 都包 —— 那才是真的开销。
//! - **审计表物理不可改。** `audit` 上有 trigger 拒绝 UPDATE/DELETE，
//!   应用层拿不到修改能力（`docs/SECURITY.md` §4.1）。
//! - **迁移从第一天就上。** 用 `PRAGMA user_version` 做版本号。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod audit;
pub mod grants;
pub mod migrate;
pub mod msg;
pub mod session;
pub mod subagents;
pub mod tasks;

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use zex_core::{Error, Result};

/// 数据库句柄。
///
/// 内部是 `Arc<Mutex<Connection>>`：桌面应用是单用户低并发场景，
/// 连接池带来的复杂度远大于收益。SQLite 的写本身就是串行的。
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// 打开（或创建）数据库文件，并跑完迁移。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Store(format!("创建目录失败: {e}")))?;
            }
        }
        let conn =
            Connection::open(path).map_err(|e| Error::Store(format!("打开数据库失败: {e}")))?;
        Self::from_conn(conn)
    }

    /// 内存数据库（测试用）。
    pub fn in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().map_err(|e| Error::Store(format!("建库失败: {e}")))?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // WAL 提升并发读；busy_timeout 避免瞬时锁冲突直接报错
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| Error::Store(format!("设置 WAL 失败: {e}")))?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(|e| Error::Store(format!("设置 busy_timeout 失败: {e}")))?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(|e| Error::Store(format!("启用外键失败: {e}")))?;

        migrate::run(&conn)?;

        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 借出连接执行一段操作。
    ///
    /// 所有业务方法都走这里，保证锁的获取/释放成对出现。
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| Error::Store("数据库锁被污染（前一次操作 panic）".into()))?;
        f(&guard)
    }

    /// 当前 schema 版本。
    pub fn schema_version(&self) -> Result<i64> {
        self.with_conn(|c| {
            c.query_row("PRAGMA user_version", [], |r| r.get(0))
                .map_err(|e| Error::Store(format!("读取版本失败: {e}")))
        })
    }
}

/// 当前 unix 秒。
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_migrates() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), migrate::LATEST);
    }

    #[test]
    fn migration_is_idempotent() {
        let s = Store::in_memory().unwrap();
        let v1 = s.schema_version().unwrap();
        // 再跑一次迁移不应报错也不应改变版本
        s.with_conn(migrate::run).unwrap();
        assert_eq!(s.schema_version().unwrap(), v1);
    }

    #[test]
    fn wal_is_enabled_for_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let s = Store::open(&path).unwrap();
        let mode: String = s
            .with_conn(|c| {
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(|e| Error::Store(e.to_string()))
            })
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
