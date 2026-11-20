//! 给扩展用的 SQLite。
//!
//! # 为什么是 SQLite 而不是给 localStorage 加个索引
//!
//! `localStorage` 是整份 JSON 读进来、整份写回去：几百条记录就开始卡，
//! 而且没法按条件查。打卡记录、任务清单、对话草稿这类东西本来就是"表"，
//! 让它一开始就落在表里，比先存 JSON 再想办法迁移便宜得多。
//!
//! # 隔离靠文件，不靠 SQL
//!
//! **一个应用一个库文件**：`~/.zex/state/db/<id>.db`。
//!
//! 这件事只有宿主能做对，所以不放到脚本手里：脚本传给我们的只有 SQL 和
//! 参数，路径永远由宿主按应用 id 拼。否则一个应用 `ATTACH` 一下
//! `~/.zex/zex.db`，就能读写全部会话、审计记录和授权表 —— 那是提权。
//!
//! 同样的理由，`ATTACH` 和 `PRAGMA` 也要挡：前者能挂上任意路径的库，
//! 后者能改隔离级别和上限。挡法不是解析 SQL（解析不完），而是给连接开
//! `SQLITE_DBCONFIG_DEFENSIVE` 并只用作者器（authorizer）放行读写语句。
//!
//! # 上限
//!
//! - 库文件最大 64 MiB（`max_page_count`），超了写入失败而不是把磁盘写满；
//! - 一次 `query` 最多 5000 行：超了报错而不是把几万个对象塞进 JS 堆 ——
//!   脚本该自己写 `LIMIT`，那才是它想要的数据量。

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;

/// 库文件所在目录（`~/.zex/state/db`）。
pub const DB_DIR: &str = "db";

/// 库文件后缀。
pub const DB_SUFFIX: &str = ".db";

/// 单个库的页数上限。配合 4 KiB 页 = 64 MiB。
///
/// 选 64 MiB 是因为它比"一个应用该有的数据"大得多（打卡一年也就几百 KB），
/// 又远小于"把家目录写满"。超了 SQLite 自己会报 `database or disk is full`。
pub const MAX_PAGES: i64 = 16_384;

/// 一次查询最多返回多少行。
pub const MAX_ROWS: usize = 5_000;

/// 一个应用的库文件路径。
///
/// `id` 必须是已经过 [`valid_app_id`] 校验的：这个函数不负责消毒，
/// 因为拼路径的地方不止一处，消毒必须发生在入口（`catalog::scan`），
/// 而不是每个用到的地方各做一遍。
pub fn db_path(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join(DB_DIR).join(format!("{id}{DB_SUFFIX}"))
}

/// 应用 id 能不能直接当文件名用。
///
/// 只允许字母、数字、`-`、`_`、`.`，且不能是 `.` / `..`。
///
/// # 为什么在扫描时就拒
///
/// id 有三个去处：目录名、宿主模块名（`zex.<id>`）、库文件名。
/// 任何一处被注入 `../` 都会出事 —— 库文件名尤其危险，它能决定
/// **打开哪个文件**。所以入口就挡，而不是在每个下游各挡一次。
pub fn valid_app_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    if id == "." || id == ".." || id.contains("..") {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// 按标准设置打开一个库。
///
/// - `max_page_count`：把上限钉在 64 MiB。**每次打开都要设** ——
///   它是连接级设置，不是持久化在文件里的。
/// - `defensive` + 作者器：见模块文档。
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        // SQLite 自己也会建，但失败时报错难懂；这里显式建、显式失败
        let _ = std::fs::create_dir_all(dir);
    }
    let mut conn = Connection::open(path)?;
    configure(&mut conn)?;
    Ok(conn)
}

/// 打开一个内存库（只用于测试）。
pub fn open_in_memory() -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&mut conn)?;
    Ok(conn)
}

fn configure(conn: &mut Connection) -> rusqlite::Result<()> {
    apply_limits(conn)?;
    install_authorizer(conn)
}

/// 上限先设，作者器后装。
///
/// 顺序不能反：作者器会挡掉所有 `PRAGMA`，装完之后再设上限就设不进去了。
/// 分开成两个函数也是为了让测试能只设上限、不装作者器 —— 否则"上限到底
/// 生效没有"这件事自己就查不了（查它用的也是 PRAGMA）。
fn apply_limits(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "max_page_count", MAX_PAGES)?;
    // WAL：写不再整库加锁，读也不挡写。副作用是多两个 `-wal`/`-shm` 文件，
    // 都在同一个目录里，删库时一起删。
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    Ok(())
}

/// 只放行「读 / 写 / 建表」这类语句，挡掉 ATTACH、PRAGMA 以及任何
/// 不属于本库的操作。挡 ATTACH 是关键 —— 那是唯一的"跨库"通道。
fn install_authorizer(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.authorizer(Some(authorize))
}

/// 允许的 SQLite 动作。
///
/// 白名单而不是黑名单：上游以后加新动作时，默认是拒绝，而不是默认放行。
fn authorize(ctx: rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization {
    use rusqlite::hooks::{AuthAction, Authorization};
    match ctx.action {
        // 建表 / 索引 / 视图、增删改查、事务、改表结构
        AuthAction::CreateTable { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::CreateView { .. }
        | AuthAction::DropView { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::DropTempTable { .. }
        | AuthAction::CreateTempIndex { .. }
        | AuthAction::DropTempIndex { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::DropTempView { .. }
        | AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Transaction { .. }
        // `CREATE INDEX` 收尾时会自己触发一次 `REINDEX`；
        // 少了它，建索引就是 "not authorized"。
        | AuthAction::Reindex { .. }
        // `ALTER TABLE ... ADD COLUMN` 的动作。
        // 少了它，补列的迁移会失败 —— 而迁移失败的表现是"历史打不开"。
        | AuthAction::AlterTable { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        // 其余一律拒绝：ATTACH / DETACH（跨库）、PRAGMA、虚拟表、
        // 以及未来可能新增的任何动作
        _ => Authorization::Deny,
    }
}

/// 一条给脚本用的语句参数。
///
/// 直接用上游的 `HostValue` 不方便测试（它带着 `gpui-shell` 的类型），
/// 所以这里定义一份等价的内部表示，转换发生在 `host.rs` 那一层。
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
    /// `null` / `undefined`。
    Null,
    /// 布尔。存成 1/0 —— SQLite 没有布尔类型。
    Bool(bool),
    /// 数字。整数就存整数，带小数就存 REAL。
    Number(f64),
    /// 字符串。
    Str(String),
    /// 数组或对象。存成 JSON 文本（见模块文档）。
    Json(String),
}

impl JsValue {
    /// 转成 SQLite 参数。
    fn to_sql(&self) -> SqlValue {
        match self {
            JsValue::Null => SqlValue::Null,
            JsValue::Bool(b) => SqlValue::Integer(i64::from(*b)),
            JsValue::Number(n) => {
                // JS 的数字都是 f64；整数保持整数，别把 id 存成 7.0
                if n.fract() == 0.0 && n.abs() < 9e15 {
                    SqlValue::Integer(*n as i64)
                } else {
                    SqlValue::Real(*n)
                }
            }
            JsValue::Str(s) => SqlValue::Text(s.clone()),
            JsValue::Json(s) => SqlValue::Text(s.clone()),
        }
    }
}

/// 写语句的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// 受影响的行数。
    pub changes: usize,
    /// 最后插入的 rowid（没有就是 0）。
    pub last_insert_id: i64,
}

/// 执行一条写语句。
pub fn exec(conn: &Connection, sql: &str, params: &[JsValue]) -> Result<ExecResult, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    if stmt.column_count() > 0 {
        // SELECT 走 exec 是脚本写错了，得说清楚，别让它以为"执行成功但没数据"
        return Err("这条语句会返回结果，请用 query()".into());
    }
    let args = bindings(params);
    let changes = stmt
        .execute(rusqlite::params_from_iter(args))
        .map_err(|e| e.to_string())?;
    Ok(ExecResult {
        changes,
        last_insert_id: conn.last_insert_rowid(),
    })
}

/// 查询，返回按列名组织的对象。
///
/// 列名重复时后者覆盖前者（SQLite 允许 `SELECT a, a`），这是唯一合理的
/// 处理：对象只有一份键空间。
pub fn query(
    conn: &Connection,
    sql: &str,
    params: &[JsValue],
) -> Result<Vec<Vec<(String, JsValue)>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let args = bindings(params);
    let mut rows = stmt
        .query(rusqlite::params_from_iter(args))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if out.len() >= MAX_ROWS {
            return Err(format!(
                "一次查询最多返回 {MAX_ROWS} 行，请自己加 LIMIT / WHERE"
            ));
        }
        let mut record = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let value = row.get::<_, SqlValue>(i).map_err(|e| e.to_string())?;
            record.push((name.clone(), from_sql(value)));
        }
        out.push(record);
    }
    Ok(out)
}

/// 同一个事务里跑多条语句。
///
/// 任一条失败就整体回滚，并把那条的错误原样抛回去 —— 脚本需要知道
/// 是哪一条坏了，而不是笼统的"事务失败"。
pub fn batch(
    conn: &mut Connection,
    items: &[(String, Vec<JsValue>)],
) -> Result<Vec<ExecResult>, String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(items.len());
    for (sql, params) in items {
        let mut stmt = tx.prepare(sql).map_err(|e| e.to_string())?;
        if stmt.column_count() > 0 {
            return Err(format!("batch 里不能有查询语句：{}", first_line(sql)));
        }
        let args = bindings(params);
        let changes = stmt
            .execute(rusqlite::params_from_iter(args))
            .map_err(|e| format!("第 {} 条失败：{e}", out.len() + 1))?;
        out.push(ExecResult {
            changes,
            last_insert_id: tx.last_insert_rowid(),
        });
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(out)
}

fn first_line(sql: &str) -> String {
    sql.lines().next().unwrap_or("").trim().to_string()
}

/// 把 `JsValue` 转成 `rusqlite` 认的参数。
///
/// 用 `Box` 是为了避开 `params_from_iter` 的元素必须同类型这件事：
/// `Value` 本身就是统一后的表示。
fn bindings(params: &[JsValue]) -> Vec<SqlValue> {
    params.iter().map(JsValue::to_sql).collect()
}

/// SQLite 的值 → 脚本能看的值。
///
/// 大整数走 `f64` 会掉精度，但 JS 本来就只有 f64 一种数字类型；
/// 真要存 64 位 id 就当字符串存，这一点写在脚本文档里。
fn from_sql(value: SqlValue) -> JsValue {
    match value {
        SqlValue::Null => JsValue::Null,
        SqlValue::Integer(i) => JsValue::Number(i as f64),
        SqlValue::Real(f) => JsValue::Number(f),
        SqlValue::Text(s) => JsValue::Str(s),
        // 二进制不上桥：JS 侧没有对应的类型，转 base64 会让人误以为
        // 传的是字符串。要存图片就把文件放到应用目录，别塞进库。
        SqlValue::Blob(_) => JsValue::Str("<blob>".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        open_in_memory().unwrap()
    }

    #[test]
    fn rejects_ids_that_could_escape_the_directory() {
        // 这三个能拼出目录外的路径，必须在入口就拒
        assert!(!valid_app_id("../zex"));
        assert!(!valid_app_id("a/b"));
        assert!(!valid_app_id(".."));
        assert!(!valid_app_id("a\\b"));
        assert!(!valid_app_id(""));
        // 正常的照过
        assert!(valid_app_id("attendance"));
        assert!(valid_app_id("game-2048"));
        assert!(valid_app_id("a.b_c-1"));
    }

    #[test]
    fn db_path_is_per_app() {
        let base = Path::new("/state");
        let a = db_path(base, "attendance");
        let b = db_path(base, "clock");
        assert_ne!(a, b, "两个应用不能共用一个库");
        assert!(a.ends_with("db/attendance.db"));
    }

    #[test]
    fn exec_then_query_roundtrip() {
        let conn = db();
        exec(
            &conn,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)",
            &[],
        )
        .unwrap();
        let r = exec(
            &conn,
            "INSERT INTO t (name, score) VALUES (?, ?)",
            &[JsValue::Str("阿达".into()), JsValue::Number(9.5)],
        )
        .unwrap();
        assert_eq!(r.changes, 1);
        assert_eq!(r.last_insert_id, 1);

        let rows = query(&conn, "SELECT id, name, score FROM t", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][1].1, JsValue::Str("阿达".into()));
        assert_eq!(rows[0][2].1, JsValue::Number(9.5));
    }

    #[test]
    fn integers_stay_integers() {
        let conn = db();
        exec(&conn, "CREATE TABLE n (v)", &[]).unwrap();
        exec(&conn, "INSERT INTO n VALUES (?)", &[JsValue::Number(7.0)]).unwrap();
        exec(&conn, "INSERT INTO n VALUES (?)", &[JsValue::Number(7.5)]).unwrap();
        let rows = query(&conn, "SELECT typeof(v) FROM n", &[]).unwrap();
        assert_eq!(
            rows[0][0].1,
            JsValue::Str("integer".into()),
            "7.0 该存成整数"
        );
        assert_eq!(rows[1][0].1, JsValue::Str("real".into()), "7.5 才是实数");
    }

    #[test]
    fn objects_are_stored_as_json_text() {
        let conn = db();
        exec(&conn, "CREATE TABLE j (v)", &[]).unwrap();
        exec(
            &conn,
            "INSERT INTO j VALUES (?)",
            &[JsValue::Json(r#"{"a":1}"#.into())],
        )
        .unwrap();
        let rows = query(&conn, "SELECT v FROM j", &[]).unwrap();
        assert_eq!(rows[0][0].1, JsValue::Str(r#"{"a":1}"#.into()));
    }

    #[test]
    fn query_statement_through_exec_is_refused() {
        let conn = db();
        let e = exec(&conn, "SELECT 1", &[]).unwrap_err();
        assert!(e.contains("query()"), "要指出该用哪个函数：{e}");
    }

    /// 索引和 ALTER 必须能用。
    ///
    /// 这两个不是"顺手加的"：`CREATE INDEX` 收尾时会自己触发一次 `REINDEX`，
    /// `ALTER TABLE` 触发 `ALTER TABLE` 动作 —— 两者都曾不在白名单里，
    /// 结果是建索引和补列全部 `not authorized`，而 `CREATE TABLE` 却能成功，
    /// 于是表现为"表建了、索引没有、迁移失败、历史打不开"。
    #[test]
    fn index_and_alter_table_are_authorized() {
        let conn = db();
        exec(
            &conn,
            "CREATE TABLE clips (id INTEGER PRIMARY KEY, fp TEXT)",
            &[],
        )
        .unwrap();

        // 建索引（收尾会触发 REINDEX）
        exec(&conn, "CREATE UNIQUE INDEX clips_fp ON clips(fp)", &[])
            .expect("CREATE INDEX 必须被放行");

        // 补列
        exec(
            &conn,
            "ALTER TABLE clips ADD COLUMN w INTEGER NOT NULL DEFAULT 0",
            &[],
        )
        .expect("ALTER TABLE ADD COLUMN 必须被放行");
        exec(
            &conn,
            "ALTER TABLE clips ADD COLUMN h INTEGER NOT NULL DEFAULT 0",
            &[],
        )
        .expect("第二条 ALTER 也必须被放行");

        // 列真的加上了
        let rows = query(&conn, "SELECT w, h FROM clips", &[]).unwrap();
        assert!(rows.is_empty(), "空表应返回 0 行");
    }

    /// 放行索引和 ALTER 之后，跨库和 PRAGMA 仍然必须被挡。
    #[test]
    fn dangerous_actions_stay_denied() {
        let conn = db();
        for sql in [
            "ATTACH DATABASE '/tmp/other.db' AS other",
            "PRAGMA journal_mode",
            "DETACH other",
        ] {
            let e = exec(&conn, sql, &[]).unwrap_err();
            assert!(e.contains("not authorized"), "`{sql}` 应被拒，实际：{e}");
        }
    }

    #[test]
    fn batch_rolls_back_on_failure() {
        let mut conn = db();
        exec(&conn, "CREATE TABLE b (v INTEGER NOT NULL)", &[]).unwrap();
        let err = batch(
            &mut conn,
            &[
                ("INSERT INTO b VALUES (1)".to_string(), vec![]),
                // NOT NULL 违反 —— 第一条必须跟着回滚
                ("INSERT INTO b VALUES (NULL)".to_string(), vec![]),
            ],
        )
        .unwrap_err();
        assert!(err.contains("第 2 条"), "错误要指出是哪一条：{err}");
        let rows = query(&conn, "SELECT v FROM b", &[]).unwrap();
        assert!(rows.is_empty(), "失败的 batch 不能留下半条数据");
    }

    #[test]
    fn batch_commits_when_all_succeed() {
        let mut conn = db();
        exec(&conn, "CREATE TABLE c (v INTEGER)", &[]).unwrap();
        let out = batch(
            &mut conn,
            &[
                ("INSERT INTO c VALUES (1)".to_string(), vec![]),
                ("INSERT INTO c VALUES (2)".to_string(), vec![]),
            ],
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        let rows = query(&conn, "SELECT count(*) FROM c", &[]).unwrap();
        assert_eq!(rows[0][0].1, JsValue::Number(2.0));
    }

    #[test]
    fn attach_is_denied_by_the_authorizer() {
        // 这条是隔离的地基：能 ATTACH 就能挂上 zex 自己的库
        let conn = db();
        let e = conn
            .execute("ATTACH DATABASE '/tmp/evil.db' AS evil", [])
            .unwrap_err();
        assert!(
            e.to_string().to_lowercase().contains("not authorized"),
            "ATTACH 必须被作者器挡住，实际：{e}"
        );
    }

    #[test]
    fn pragma_is_denied_by_the_authorizer() {
        let conn = db();
        // 脚本改不了上限，也就没法把库写成无限大
        let e = conn.execute_batch("PRAGMA max_page_count = 1000000");
        assert!(e.is_err(), "PRAGMA 必须被挡住");
    }

    #[test]
    fn page_cap_is_applied() {
        // 不装作者器才查得到（查它用的就是 PRAGMA）
        let mut conn = Connection::open_in_memory().unwrap();
        apply_limits(&mut conn).unwrap();
        let pages: i64 = conn
            .pragma_query_value(None, "max_page_count", |r| r.get(0))
            .unwrap();
        assert_eq!(pages, MAX_PAGES);
    }

    #[test]
    fn row_cap_is_enforced() {
        let conn = db();
        exec(&conn, "CREATE TABLE many (v INTEGER)", &[]).unwrap();
        // 用递归 CTE 造 MAX_ROWS + 1 行，不写文件
        exec(
            &conn,
            &format!(
                "INSERT INTO many WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < {}) SELECT n FROM seq",
                MAX_ROWS + 1
            ),
            &[],
        )
        .unwrap();
        let e = query(&conn, "SELECT v FROM many", &[]).unwrap_err();
        assert!(e.contains("LIMIT"), "要告诉脚本怎么改：{e}");
    }
}
