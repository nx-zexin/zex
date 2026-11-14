//! 会话读写。

use rusqlite::{OptionalExtension, params};
use zex_core::{Error, Result, SessionId};

use crate::{Store, now};

/// 一个会话的元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// 会话 id。
    pub id: SessionId,
    /// 标题（首条消息可自动生成，也可用户改）。
    pub title: String,
    /// 使用的模型。
    pub model: String,
    /// 工作目录。
    pub cwd: String,
    /// 创建时间（unix 秒）。
    pub created_at: i64,
    /// 最后活动时间。
    pub updated_at: i64,
    /// 这个会话的运行模式。
    pub mode: zex_core::RunMode,
}

impl Store {
    /// 新建会话。
    pub fn create_session(&self, title: &str, model: &str, cwd: &str) -> Result<Session> {
        let id = SessionId::generate();
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO sessions (id, title, model, cwd, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![id.as_str(), title, model, cwd, ts],
            )
            .map_err(|e| Error::Store(format!("创建会话失败: {e}")))?;
            Ok(())
        })?;
        Ok(Session {
            id,
            title: title.to_string(),
            model: model.to_string(),
            cwd: cwd.to_string(),
            created_at: ts,
            updated_at: ts,
            mode: zex_core::RunMode::default(),
        })
    }

    /// 读取会话。
    pub fn get_session(&self, id: &SessionId) -> Result<Option<Session>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, title, model, cwd, created_at, updated_at, mode FROM sessions WHERE id = ?1",
                params![id.as_str()],
                |r| {
                    Ok(Session {
                        id: SessionId::new(r.get::<_, String>(0)?),
                        title: r.get(1)?,
                        model: r.get(2)?,
                        cwd: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        mode: parse_mode(r.get::<_, String>(6)?),
                    })
                },
            )
            .optional()
            .map_err(|e| Error::Store(format!("查询会话失败: {e}")))
        })
    }

    /// 列出最近的会话（按最后活动时间倒序）。
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, title, model, cwd, created_at, updated_at, mode
                     FROM sessions ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(|e| Error::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![limit as i64], |r| {
                    Ok(Session {
                        id: SessionId::new(r.get::<_, String>(0)?),
                        title: r.get(1)?,
                        model: r.get(2)?,
                        cwd: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        mode: parse_mode(r.get::<_, String>(6)?),
                    })
                })
                .map_err(|e| Error::Store(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Store(e.to_string()))?);
            }
            Ok(out)
        })
    }

    /// 列会话，但排除子智能体的会话。
    ///
    /// 子 agent 各自占一个会话（那样能追查它读了什么、跑了什么），
    /// 但那是**实现细节**，不该出现在用户的会话列表里 —— 用户的侧栏
    /// 是他自己的对话，不是内部调度日志。
    ///
    /// 排除依据是 `sub_agents` 表里登记的 session_id，不靠标题前缀猜。
    pub fn list_visible_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, title, model, cwd, created_at, updated_at, mode
                     FROM sessions
                     WHERE id NOT IN (SELECT session_id FROM sub_agents)
                     ORDER BY updated_at DESC LIMIT ?1",
                )
                .map_err(|e| Error::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![limit as i64], |r| {
                    Ok(Session {
                        id: SessionId::new(r.get::<_, String>(0)?),
                        title: r.get(1)?,
                        model: r.get(2)?,
                        cwd: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        mode: parse_mode(r.get::<_, String>(6)?),
                    })
                })
                .map_err(|e| Error::Store(e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Store(e.to_string()))?);
            }
            Ok(out)
        })
    }

    /// 改标题。
    pub fn rename_session(&self, id: &SessionId, title: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![title, id.as_str()],
            )
            .map_err(|e| Error::Store(format!("改标题失败: {e}")))?;
            Ok(())
        })
    }

    /// 记住这个会话用哪个模型（`provider/model`）。
    ///
    /// 不碰 `updated_at`：切模型不是"有活动"，否则侧栏顺序会莫名其妙地跳。
    pub fn set_session_model(&self, id: &SessionId, model: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE sessions SET model = ?1 WHERE id = ?2",
                params![model, id.as_str()],
            )
            .map_err(|e| Error::Store(format!("更新会话模型失败: {e}")))?;
            Ok(())
        })
    }

    /// 记住这个会话的运行模式。
    pub fn set_session_mode(&self, id: &SessionId, mode: zex_core::RunMode) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE sessions SET mode = ?1 WHERE id = ?2",
                params![mode_str(mode), id.as_str()],
            )
            .map_err(|e| Error::Store(format!("更新会话模式失败: {e}")))?;
            Ok(())
        })
    }

    /// 触摸会话的最后活动时间。
    pub fn touch_session(&self, id: &SessionId) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![now(), id.as_str()],
            )
            .map_err(|e| Error::Store(format!("更新会话时间失败: {e}")))?;
            Ok(())
        })
    }

    /// 删除会话（级联删消息）。
    pub fn delete_session(&self, id: &SessionId) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM sessions WHERE id = ?1", params![id.as_str()])
                .map_err(|e| Error::Store(format!("删除会话失败: {e}")))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_mode_defaults_to_normal_and_roundtrips() {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("模式", "deepseek-chat", "/tmp").unwrap();
        // 新会话默认按风险确认，不是"啥都放行"
        assert_eq!(s.get_session(&ses.id).unwrap().unwrap().mode, zex_core::RunMode::Normal);

        s.set_session_mode(&ses.id, zex_core::RunMode::Plan).unwrap();
        assert_eq!(s.get_session(&ses.id).unwrap().unwrap().mode, zex_core::RunMode::Plan);

        // 列表查询也要带上，否则切会话时读不回来
        let listed = s.list_visible_sessions(10).unwrap();
        assert_eq!(listed[0].mode, zex_core::RunMode::Plan);
    }

    #[test]
    fn set_session_model_persists() {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("模型", "deepseek-chat", "/tmp").unwrap();
        s.set_session_model(&ses.id, "deepseek/deepseek-reasoner")
            .unwrap();
        assert_eq!(
            s.get_session(&ses.id).unwrap().unwrap().model,
            "deepseek/deepseek-reasoner"
        );
    }

    #[test]
    fn create_and_read_back() {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("测试", "deepseek-chat", "/tmp").unwrap();
        let got = s.get_session(&ses.id).unwrap().unwrap();
        assert_eq!(got.title, "测试");
        assert_eq!(got.model, "deepseek-chat");
    }

    /// 子智能体占的会话不该出现在用户的会话列表里。
    #[test]
    fn visible_sessions_hide_subagent_sessions() {
        let s = Store::in_memory().unwrap();
        let mine = s.create_session("我的对话", "m", "/tmp").unwrap();
        let child = s.create_session("子智能体 · coder-1", "m", "/tmp").unwrap();
        s.upsert_sub_agent(
            "coder-1",
            "coder",
            "干活",
            child.id.as_str(),
            crate::subagents::SubAgentState::Running,
            None,
        )
        .unwrap();

        let all = s.list_sessions(20).unwrap();
        assert_eq!(all.len(), 2, "底层还是要能查到全部");

        let visible = s.list_visible_sessions(20).unwrap();
        assert_eq!(visible.len(), 1, "侧栏只该看到自己的会话");
        assert_eq!(visible[0].id, mine.id);
    }

    #[test]
    fn missing_session_returns_none() {
        let s = Store::in_memory().unwrap();
        assert!(s.get_session(&SessionId::new("nope")).unwrap().is_none());
    }

    #[test]
    fn list_is_ordered_by_recency() {
        let s = Store::in_memory().unwrap();
        let a = s.create_session("a", "m", "/tmp").unwrap();
        let b = s.create_session("b", "m", "/tmp").unwrap();
        // 显式 touch a，让它的 updated_at 更新
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.touch_session(&a.id).unwrap();
        let list = s.list_sessions(10).unwrap();
        assert_eq!(list[0].id, a.id);
        assert_eq!(list[1].id, b.id);
    }

    #[test]
    fn rename_works() {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("old", "m", "/tmp").unwrap();
        s.rename_session(&ses.id, "new").unwrap();
        assert_eq!(s.get_session(&ses.id).unwrap().unwrap().title, "new");
    }

    #[test]
    fn delete_removes_session() {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("x", "m", "/tmp").unwrap();
        s.delete_session(&ses.id).unwrap();
        assert!(s.get_session(&ses.id).unwrap().is_none());
    }
}

/// 从库里读出来的模式字符串转回枚举。
///
/// 认不出来一律按 Normal：宁可多问一次，也不要因为一个坏值
/// 静默变成"少问一次"。
fn parse_mode(raw: String) -> zex_core::RunMode {
    match raw.as_str() {
        "plan" => zex_core::RunMode::Plan,
        _ => zex_core::RunMode::Normal,
    }
}

/// 枚举转存库字符串。
fn mode_str(mode: zex_core::RunMode) -> &'static str {
    match mode {
        zex_core::RunMode::Normal => "normal",
        zex_core::RunMode::Plan => "plan",
    }
}
