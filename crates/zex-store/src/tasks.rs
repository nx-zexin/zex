//! 任务表：异步执行的底座。
//!
//! 一轮对话在这里落成一条有状态的任务记录，而不是只留在内存里。
//! 这样用户关掉窗口再回来，才能回答「我不在的时候它做了什么」——
//! 这是「数字助理」和「聊天框」的分界线。

use rusqlite::params;
use serde::{Deserialize, Serialize};
use zex_core::{Error, Result};

use crate::{now, Store};

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 已入队，等待执行。
    Queued,
    /// 正在执行。
    Running,
    /// 正常结束。
    Completed,
    /// 出错结束。
    Failed,
    /// 被用户中断。
    Cancelled,
}

impl TaskStatus {
    /// 转成数据库里存的字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否已结束（不再变化）。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            // 认不出的状态按排队处理：宁可当它还没跑，
            // 也不要误判成已完成而把它从界面上抹掉
            _ => Self::Queued,
        }
    }
}

/// 一条任务记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// 任务 id。
    pub id: String,
    /// 所属会话。
    pub session_id: String,
    /// 原始输入。
    pub prompt: String,
    /// 展示用标题。
    pub title: String,
    /// 状态。
    pub status: TaskStatus,
    /// 入队时间。
    pub created_at: i64,
    /// 开始执行时间。
    pub started_at: Option<i64>,
    /// 结束时间。
    pub ended_at: Option<i64>,
    /// 结果摘要。
    pub summary: Option<String>,
    /// 失败原因。
    pub error: Option<String>,
    /// 用户是否已看过。
    pub seen: bool,
}

impl Store {
    /// 新建一条排队中的任务。
    pub fn create_task(
        &self,
        id: &str,
        session_id: &str,
        prompt: &str,
        title: &str,
    ) -> Result<TaskRecord> {
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks (id, session_id, prompt, title, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
                params![id, session_id, prompt, title, ts],
            )
            .map_err(|e| Error::Store(format!("建任务失败: {e}")))?;
            Ok(())
        })?;
        Ok(TaskRecord {
            id: id.to_string(),
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
            title: title.to_string(),
            status: TaskStatus::Queued,
            created_at: ts,
            started_at: None,
            ended_at: None,
            summary: None,
            error: None,
            seen: false,
        })
    }

    /// 标记任务开始执行。
    pub fn start_task(&self, id: &str) -> Result<()> {
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET status = 'running', started_at = ?2 WHERE id = ?1",
                params![id, ts],
            )
            .map_err(|e| Error::Store(format!("更新任务失败: {e}")))?;
            Ok(())
        })
    }

    /// 结束一条任务。
    pub fn finish_task(
        &self,
        id: &str,
        status: TaskStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks
                    SET status = ?2, ended_at = ?3, summary = ?4, error = ?5
                  WHERE id = ?1",
                params![id, status.as_str(), ts, summary, error],
            )
            .map_err(|e| Error::Store(format!("结束任务失败: {e}")))?;
            Ok(())
        })
    }

    /// 把某个任务标记为已读。
    pub fn mark_task_seen(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("UPDATE tasks SET seen = 1 WHERE id = ?1", params![id])
                .map_err(|e| Error::Store(format!("标记已读失败: {e}")))?;
            Ok(())
        })
    }

    /// 把所有已结束的任务标记为已读（打开 dashboard 时调用）。
    pub fn mark_all_finished_seen(&self) -> Result<usize> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET seen = 1
                  WHERE seen = 0 AND status IN ('completed', 'failed', 'cancelled')",
                [],
            )
            .map_err(|e| Error::Store(format!("批量标记已读失败: {e}")))
        })
    }

    /// 启动时清理：上次进程退出时还在 running 的任务已经不可能再跑了，
    /// 标成失败并说明原因，否则它们会永远挂在「运行中」。
    pub fn fail_orphaned_running_tasks(&self) -> Result<usize> {
        let ts = now();
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks
                    SET status = 'failed', ended_at = ?1,
                        error = '进程退出时该任务仍在运行'
                  WHERE status = 'running'",
                params![ts],
            )
            .map_err(|e| Error::Store(format!("清理孤儿任务失败: {e}")))
        })
    }

    /// 按状态列出任务（新的在前）。
    pub fn list_tasks_by_status(&self, statuses: &[TaskStatus], limit: usize) -> Result<Vec<TaskRecord>> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        // 状态是枚举转字符串，不来自用户输入，但这里仍用参数绑定
        let placeholders = (1..=statuses.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, session_id, prompt, title, status, created_at, started_at,
                    ended_at, summary, error, seen
               FROM tasks
              WHERE status IN ({placeholders})
              ORDER BY created_at DESC
              LIMIT {}",
            limit as i64
        );
        self.with_conn(|c| {
            let mut stmt = c
                .prepare(&sql)
                .map_err(|e| Error::Store(format!("查询任务失败: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(
                    statuses.iter().map(|s| s.as_str()),
                ), |r| {
                    Ok(TaskRecord {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        prompt: r.get(2)?,
                        title: r.get(3)?,
                        status: TaskStatus::parse(&r.get::<_, String>(4)?),
                        created_at: r.get(5)?,
                        started_at: r.get(6)?,
                        ended_at: r.get(7)?,
                        summary: r.get(8)?,
                        error: r.get(9)?,
                        seen: r.get::<_, i64>(10)? != 0,
                    })
                })
                .map_err(|e| Error::Store(format!("读取任务失败: {e}")))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| Error::Store(format!("读取任务失败: {e}")))
        })
    }

    /// 未读的已结束任务数（dashboard 角标用）。
    pub fn unseen_finished_count(&self) -> Result<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM tasks
                  WHERE seen = 0 AND status IN ('completed', 'failed', 'cancelled')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| Error::Store(format!("统计未读任务失败: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, String) {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("t", "m", "/tmp").unwrap();
        (s, ses.id.to_string())
    }

    #[test]
    fn create_and_finish_task() {
        let (s, sid) = store();
        s.create_task("t1", &sid, "帮我看看依赖", "").unwrap();

        let queued = s.list_tasks_by_status(&[TaskStatus::Queued], 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, TaskStatus::Queued);

        s.start_task("t1").unwrap();
        let running = s.list_tasks_by_status(&[TaskStatus::Running], 10).unwrap();
        assert_eq!(running.len(), 1);
        assert!(running[0].started_at.is_some());

        s.finish_task("t1", TaskStatus::Completed, Some("完成"), None)
            .unwrap();
        let done = s
            .list_tasks_by_status(&[TaskStatus::Completed], 10)
            .unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].summary.as_deref(), Some("完成"));
        assert!(done[0].ended_at.is_some());
        assert!(!done[0].seen, "刚结束的任务应是未读");
    }

    #[test]
    fn orphaned_running_tasks_are_failed_on_startup() {
        let (s, sid) = store();
        s.create_task("t1", &sid, "半路退出", "").unwrap();
        s.start_task("t1").unwrap();

        let n = s.fail_orphaned_running_tasks().unwrap();
        assert_eq!(n, 1);

        let failed = s.list_tasks_by_status(&[TaskStatus::Failed], 10).unwrap();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].error.as_deref().unwrap_or("").contains("进程退出"));
    }

    #[test]
    fn unseen_count_tracks_finished_only() {
        let (s, sid) = store();
        s.create_task("t1", &sid, "a", "").unwrap();
        s.create_task("t2", &sid, "b", "").unwrap();
        s.finish_task("t1", TaskStatus::Completed, Some("ok"), None)
            .unwrap();
        // t2 还在排队，不算未读
        assert_eq!(s.unseen_finished_count().unwrap(), 1);

        assert_eq!(s.mark_all_finished_seen().unwrap(), 1);
        assert_eq!(s.unseen_finished_count().unwrap(), 0);
    }
}
