//! Schema 迁移。
//!
//! 用 `PRAGMA user_version` 做版本号，顺序执行、幂等。
//! **从第一天就上**，不要等到有用户数据再补（ADR-005）。

use rusqlite::Connection;
use zex_core::{Error, Result};

/// 当前 schema 版本。新增迁移时 +1。
pub const LATEST: i64 = 1;

/// 跑完所有未应用的迁移。
pub fn run(conn: &Connection) -> Result<()> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| Error::Store(format!("读版本失败: {e}")))?;

    // 降级必须先拦住：旧版程序打开新版数据库，会按旧 schema 读写，
    // 轻则报错重则损坏数据。这里宁可不启动，也不能默默继续。
    if version > LATEST {
        return Err(Error::Store(format!(
            "数据库版本 {version} 高于本程序支持的 {LATEST}，请升级 Zex 后再打开"
        )));
    }

    while version < LATEST {
        match version {
            0 => v1_initial(conn)?,
            other => {
                // 正常情况下不可达：version 已在上面校验过 <= LATEST
                return Err(Error::Store(format!("迁移分支缺失: 版本 {other}")));
            }
        }
        version += 1;
        conn.pragma_update(None, "user_version", version)
            .map_err(|e| Error::Store(format!("写版本失败: {e}")))?;
        tracing::info!(version, "已应用迁移");
    }
    Ok(())
}

/// v1：会话、消息、审计、授权。
fn v1_initial(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- ─── 会话 ───
        CREATE TABLE IF NOT EXISTS sessions (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL DEFAULT '',
            model       TEXT NOT NULL DEFAULT '',
            cwd         TEXT NOT NULL DEFAULT '',
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );

        -- ─── 消息 ───
        -- payload 是 Message 的 JSON。用 JSON 而不是拆列：
        -- 消息结构会演进（thinking / images / tool_calls），
        -- 每变一次就改表结构是不可持续的。
        CREATE TABLE IF NOT EXISTS messages (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            role        TEXT NOT NULL,
            payload     TEXT NOT NULL,
            tokens      INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

        -- ─── 审计（只追加）───
        CREATE TABLE IF NOT EXISTS audit (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          INTEGER NOT NULL,
            kind        TEXT NOT NULL,
            risk        TEXT NOT NULL,
            tool        TEXT NOT NULL DEFAULT '',
            args_digest TEXT NOT NULL DEFAULT '',
            result      TEXT NOT NULL DEFAULT '',
            approved_by TEXT NOT NULL DEFAULT 'auto',
            session_id  TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit(ts);
        CREATE INDEX IF NOT EXISTS idx_audit_session ON audit(session_id);

        -- ★ 审计不可篡改：这是物理保证，不是"我们承诺不改"
        CREATE TRIGGER IF NOT EXISTS audit_no_update
        BEFORE UPDATE ON audit
        BEGIN
            SELECT RAISE(ABORT, 'audit log is append-only');
        END;

        CREATE TRIGGER IF NOT EXISTS audit_no_delete
        BEFORE DELETE ON audit
        BEGIN
            SELECT RAISE(ABORT, 'audit log is append-only');
        END;

        -- ─── 授权记忆 ───
        -- 用户说"以后别再问"就写一行；可列出、可撤销（SECURITY.md §3.2）
        CREATE TABLE IF NOT EXISTS grants (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            tool        TEXT NOT NULL,
            pattern     TEXT NOT NULL DEFAULT '',
            risk        TEXT NOT NULL,
            granted_at  INTEGER NOT NULL,
            expires_at  INTEGER,
            revoked_at  INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_grants_tool ON grants(tool);
        "#,
    )
    .map_err(|e| Error::Store(format!("v1 迁移失败: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        run(&c).unwrap();
        c
    }

    #[test]
    fn creates_expected_tables() {
        let c = mem();
        let mut names: Vec<String> = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["audit", "grants", "messages", "sessions"]);
    }

    #[test]
    fn audit_table_rejects_update() {
        let c = mem();
        c.execute(
            "INSERT INTO audit (ts, kind, risk) VALUES (1, 'tool_call', 'read_only')",
            [],
        )
        .unwrap();
        let err = c
            .execute("UPDATE audit SET risk = 'read_only' WHERE id = 1", [])
            .unwrap_err();
        assert!(err.to_string().contains("append-only"), "实际错误: {err}");
    }

    #[test]
    fn audit_table_rejects_delete() {
        let c = mem();
        c.execute(
            "INSERT INTO audit (ts, kind, risk) VALUES (1, 'tool_call', 'read_only')",
            [],
        )
        .unwrap();
        let err = c.execute("DELETE FROM audit WHERE id = 1", []).unwrap_err();
        assert!(err.to_string().contains("append-only"), "实际错误: {err}");
    }

    #[test]
    fn messages_cascade_on_session_delete() {
        let c = mem();
        c.execute(
            "INSERT INTO sessions (id, created_at, updated_at) VALUES ('s1', 0, 0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages (session_id, role, payload, created_at) VALUES ('s1', 'user', '{}', 0)",
            [],
        )
        .unwrap();
        c.execute("DELETE FROM sessions WHERE id = 's1'", [])
            .unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "删除会话应级联删除消息");
    }

    #[test]
    fn migration_refuses_future_version() {
        let c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "user_version", 999).unwrap();
        let err = run(&c).unwrap_err();
        assert!(matches!(err, Error::Store(_)));
    }
}
