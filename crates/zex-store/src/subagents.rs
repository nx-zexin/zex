//! 子智能体名册。
//!
//! 团队要能活过进程重启：关掉窗口再打开，之前在干活的那些 agent
//! 至少还能看到、还能追问。至于"重启后自动接着跑"——不做，因为
//! 中断时它们手里的上下文已经没了，硬续只会胡来。重启时统一标成
//! 「已中断」，由用户决定要不要重新派活。

use rusqlite::params;
use serde::{Deserialize, Serialize};
use zex_core::{Error, Result};

use crate::{Store, now};

/// 子智能体在库里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentState {
    /// 正在跑（进程退出时留下的就是这种，恢复时改判为 Interrupted）。
    Running,
    /// 跑完了。
    Completed,
    /// 出错了。
    Failed,
    /// 被叫停。
    Terminated,
    /// 进程退出时还在跑，恢复后标记为中断。
    Interrupted,
}

impl SubAgentState {
    /// 转成数据库里存的字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Terminated => "terminated",
            Self::Interrupted => "interrupted",
        }
    }

    /// 从数据库字符串解析。未知值按中断处理 —— 宁可保守。
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "terminated" => Self::Terminated,
            _ => Self::Interrupted,
        }
    }

    /// 是否已经结束。
    pub fn is_done(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// 一条名册记录。
#[derive(Debug, Clone)]
pub struct SubAgentRecord {
    /// 标识。
    pub id: String,
    /// 角色。
    pub role: String,
    /// 职责描述。
    pub description: String,
    /// 它占用的会话。
    pub session_id: String,
    /// 派生它的主会话（团队列表按这个过滤）。
    pub parent_session: String,
    /// 状态。
    pub status: SubAgentState,
    /// 结果摘要。
    pub result: Option<String>,
    /// 创建时间（毫秒）。
    pub created_at: i64,
    /// 最近更新时间（毫秒）。
    pub updated_at: i64,
}

impl Store {
    /// 写入或更新一条名册记录。
    pub fn upsert_sub_agent(
        &self,
        id: &str,
        role: &str,
        description: &str,
        session_id: &str,
        parent_session: &str,
        status: SubAgentState,
        result: Option<&str>,
    ) -> Result<()> {
        let ts = now();
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sub_agents
                   (id, role, description, session_id, parent_session, status, result, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                   role = excluded.role,
                   description = excluded.description,
                   session_id = excluded.session_id,
                   parent_session = excluded.parent_session,
                   status = excluded.status,
                   result = excluded.result,
                   updated_at = excluded.updated_at",
                params![
                    id,
                    role,
                    description,
                    session_id,
                    parent_session,
                    status.as_str(),
                    result,
                    ts
                ],
            )
            .map_err(|e| Error::Store(format!("写子智能体失败: {e}")))?;
            Ok(())
        })
    }

    /// 读全部名册记录（最近的在前）。
    pub fn list_sub_agents(&self) -> Result<Vec<SubAgentRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, role, description, session_id, parent_session,
                            status, result, created_at, updated_at
                     FROM sub_agents ORDER BY updated_at DESC",
                )
                .map_err(|e| Error::Store(format!("查子智能体失败: {e}")))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(SubAgentRecord {
                        id: r.get(0)?,
                        role: r.get(1)?,
                        description: r.get(2)?,
                        session_id: r.get(3)?,
                        parent_session: r.get(4)?,
                        status: SubAgentState::parse(&r.get::<_, String>(5)?),
                        result: r.get(6)?,
                        created_at: r.get(7)?,
                        updated_at: r.get(8)?,
                    })
                })
                .map_err(|e| Error::Store(format!("读子智能体失败: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::Store(format!("解析子智能体失败: {e}")))?);
            }
            Ok(out)
        })
    }

    /// 删掉一条记录。
    pub fn delete_sub_agent(&self, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM sub_agents WHERE id = ?1", params![id])
                .map_err(|e| Error::Store(format!("删子智能体失败: {e}")))?;
            Ok(())
        })
    }

    /// 把上次退出时还在跑的一律改判为「已中断」。
    ///
    /// 启动时调用一次。进程都重启了，那些 agent 手里的上下文早没了，
    /// 继续标成「执行中」只会让界面撒谎。
    pub fn interrupt_orphan_sub_agents(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let n = conn
                .execute(
                    "UPDATE sub_agents SET status = 'interrupted', updated_at = ?1
                     WHERE status = 'running'",
                    params![now()],
                )
                .map_err(|e| Error::Store(format!("清理中断子智能体失败: {e}")))?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_list_roundtrip() {
        let s = Store::in_memory().unwrap();
        s.upsert_sub_agent(
            "a1",
            "coder",
            "写代码",
            "ses_1",
            "ses_parent",
            SubAgentState::Running,
            None,
        )
        .unwrap();
        let list = s.list_sub_agents().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "a1");
        assert_eq!(list[0].status, SubAgentState::Running);
    }

    #[test]
    fn upsert_updates_status_in_place() {
        let s = Store::in_memory().unwrap();
        s.upsert_sub_agent(
            "a1",
            "coder",
            "写代码",
            "ses_1",
            "ses_parent",
            SubAgentState::Running,
            None,
        )
        .unwrap();
        s.upsert_sub_agent(
            "a1",
            "coder",
            "写代码",
            "ses_1",
            "ses_parent",
            SubAgentState::Completed,
            Some("做完了"),
        )
        .unwrap();
        let list = s.list_sub_agents().unwrap();
        assert_eq!(list.len(), 1, "不该插成两条");
        assert_eq!(list[0].status, SubAgentState::Completed);
        assert_eq!(list[0].result.as_deref(), Some("做完了"));
    }

    #[test]
    fn orphans_become_interrupted() {
        let s = Store::in_memory().unwrap();
        s.upsert_sub_agent(
            "a1",
            "coder",
            "x",
            "ses_1",
            "ses_parent",
            SubAgentState::Running,
            None,
        )
        .unwrap();
        s.upsert_sub_agent(
            "a2",
            "coder",
            "y",
            "ses_2",
            "ses_parent",
            SubAgentState::Completed,
            Some("ok"),
        )
        .unwrap();
        assert_eq!(s.interrupt_orphan_sub_agents().unwrap(), 1);
        let list = s.list_sub_agents().unwrap();
        let a1 = list.iter().find(|r| r.id == "a1").unwrap();
        let a2 = list.iter().find(|r| r.id == "a2").unwrap();
        assert_eq!(a1.status, SubAgentState::Interrupted);
        assert_eq!(a2.status, SubAgentState::Completed, "已完成的不能被改写");
    }

    #[test]
    fn delete_removes_row() {
        let s = Store::in_memory().unwrap();
        s.upsert_sub_agent(
            "a1",
            "coder",
            "x",
            "ses_1",
            "ses_parent",
            SubAgentState::Running,
            None,
        )
        .unwrap();
        s.delete_sub_agent("a1").unwrap();
        assert!(s.list_sub_agents().unwrap().is_empty());
    }
}

#[cfg(test)]
mod restart_tests {
    use super::*;

    /// 关掉进程再打开，名册还在，且「执行中」的变成「已中断」。
    #[test]
    fn roster_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zex.db");

        {
            let s = Store::open(&path).unwrap();
            s.upsert_sub_agent(
                "coder-1",
                "coder",
                "查依赖",
                "ses_a",
                "ses_parent",
                SubAgentState::Running,
                None,
            )
            .unwrap();
            s.upsert_sub_agent(
                "reviewer-1",
                "reviewer",
                "审代码",
                "ses_b",
                "ses_parent",
                SubAgentState::Completed,
                Some("没问题"),
            )
            .unwrap();
        }

        {
            let s = Store::open(&path).unwrap();
            let changed = s.interrupt_orphan_sub_agents().unwrap();
            assert_eq!(changed, 1, "只有 running 的那条该被改判");

            let list = s.list_sub_agents().unwrap();
            assert_eq!(list.len(), 2, "重启后名册不该丢");
            let coder = list.iter().find(|r| r.id == "coder-1").unwrap();
            assert_eq!(coder.status, SubAgentState::Interrupted);
            assert_eq!(coder.session_id, "ses_a", "会话要留着，追问还得靠它");
        }
    }
}
