//! Schema 迁移。
//!
//! 用 `PRAGMA user_version` 做版本号，顺序执行、幂等。
//! **从第一天就上**，不要等到有用户数据再补（ADR-005）。

use rusqlite::Connection;
use zex_core::{Error, Result};

/// 当前 schema 版本。新增迁移时 +1。
pub const LATEST: i64 = 5;

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
            1 => v2_tasks(conn)?,
            2 => v3_sub_agents(conn)?,
            3 => v4_session_mode(conn)?,
            4 => v5_subagent_parent(conn)?,
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

/// v2：任务表。
///
/// 这是「异步执行」的底座：一轮对话不再只是一串消息，而是一条有状态的
/// 任务。有了它，用户关掉窗口再回来才能回答「我不在的时候它做了什么」。
fn v2_tasks(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS tasks (
            id          TEXT PRIMARY KEY,
            session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            -- 原始输入：重跑、审计、展示都用得上
            prompt      TEXT NOT NULL,
            -- 展示用标题（沿用会话标题或首行摘要）
            title       TEXT NOT NULL DEFAULT '',
            status      TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            started_at  INTEGER,
            ended_at    INTEGER,
            -- 成功时的结果摘要
            summary     TEXT,
            -- 失败原因
            error       TEXT,
            -- 用户是否已在界面上看过（dashboard 的未读标记）
            seen        INTEGER NOT NULL DEFAULT 0
        );

        -- dashboard 的主查询：按状态+时间捞
        CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
        "#,
    )
    .map_err(|e| Error::Store(format!("v2 迁移失败: {e}")))?;
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
        assert_eq!(
            names,
            vec![
                "audit",
                "grants",
                "messages",
                "sessions",
                "sub_agents",
                "tasks"
            ]
        );
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

/// v3：子智能体名册。
///
/// 进程退出后团队是要恢复的 —— 否则用户辛苦派出的一队人，
/// 关一次窗口就全没了。
fn v3_sub_agents(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sub_agents (
            id          TEXT PRIMARY KEY,
            role        TEXT NOT NULL,
            description TEXT NOT NULL,
            -- 每个子 agent 独占的会话，点进去能看它干了什么
            session_id  TEXT NOT NULL,
            status      TEXT NOT NULL,
            -- 跑完的结果摘要
            result      TEXT,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_sub_agents_updated ON sub_agents(updated_at DESC);
        "#,
    )
    .map_err(|e| Error::Store(format!("v3 迁移失败: {e}")))?;
    Ok(())
}

/// v4：会话自己的运行模式。
///
/// 模式是**每个会话**的选择：一个会话在只读地读代码，另一个在放开改，
/// 两者并存完全是合理用法。存在进程状态里会互相污染。
fn v4_session_mode(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'normal';
        "#,
    )
    .map_err(|e| Error::Store(format!("v4 迁移失败: {e}")))?;
    Ok(())
}

/// v5：子智能体记住它由哪个会话派生。
///
/// 在此之前团队列表是全局的：不管当前在看哪个会话，都会显示所有
/// 子智能体。加这一列之后，团队只属于派出它的那个会话。
/// 老数据留空串 —— 那些 agent 的归属已经无法追溯，宁可不显示，
/// 也不要塞进当前会话假装是它的。
fn v5_subagent_parent(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        ALTER TABLE sub_agents ADD COLUMN parent_session TEXT NOT NULL DEFAULT '';
        CREATE INDEX IF NOT EXISTS idx_sub_agents_parent ON sub_agents(parent_session);
        "#,
    )
    .map_err(|e| Error::Store(format!("v5 迁移失败: {e}")))?;
    Ok(())
}
