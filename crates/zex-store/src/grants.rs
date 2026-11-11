//! 授权记忆。
//!
//! 用户说"以后这个操作不用问了"就写一行。三条硬要求（`docs/SECURITY.md` §3）：
//! 可列出、可撤销、**L3 永不写入**。
//!
//! 匹配逻辑不在本模块 —— 那是 `zex-policy` 的职责。这里只负责存取。

use rusqlite::params;
use zex_core::{Error, Result, Risk};

use crate::{Store, now};

/// 一条授权记忆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// 自增 id。
    pub id: i64,
    /// 工具名。
    pub tool: String,
    /// 参数 glob 模式。空串表示"该工具全部参数"。
    pub pattern: String,
    /// 授权时的风险等级。
    pub risk: Risk,
    /// 授予时间。
    pub granted_at: i64,
    /// 过期时间（`None` = 永久）。
    pub expires_at: Option<i64>,
    /// 撤销时间（`None` = 未撤销）。
    pub revoked_at: Option<i64>,
}

impl Grant {
    /// 当前是否有效（未撤销且未过期）。
    pub fn is_active(&self, now: i64) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        match self.expires_at {
            Some(exp) => exp > now,
            None => true,
        }
    }
}

impl Store {
    /// 新增一条授权。
    ///
    /// L3（Destructive）**永远拒绝写入** —— 高危操作必须每次确认。
    pub fn add_grant(
        &self,
        tool: &str,
        pattern: &str,
        risk: Risk,
        days: Option<u32>,
    ) -> Result<i64> {
        if !risk.grantable() {
            return Err(Error::Denied(format!(
                "风险等级 {risk} 不允许被记住（高危操作必须每次确认）"
            )));
        }
        let ts = now();
        let expires = days.map(|d| ts + i64::from(d) * 86_400);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO grants (tool, pattern, risk, granted_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    tool,
                    pattern,
                    format!("{risk:?}").to_lowercase(),
                    ts,
                    expires
                ],
            )
            .map_err(|e| Error::Store(format!("写授权失败: {e}")))?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 查某个工具的全部有效授权。
    pub fn active_grants_for(&self, tool: &str) -> Result<Vec<Grant>> {
        let ts = now();
        let all = self.list_grants_inner(Some(tool))?;
        Ok(all.into_iter().filter(|g| g.is_active(ts)).collect())
    }

    /// 列出全部授权（含已撤销，供 UI 展示历史）。
    pub fn list_grants(&self) -> Result<Vec<Grant>> {
        self.list_grants_inner(None)
    }

    fn list_grants_inner(&self, tool: Option<&str>) -> Result<Vec<Grant>> {
        self.with_conn(|c| {
            let sql = match tool {
                Some(_) => {
                    "SELECT id, tool, pattern, risk, granted_at, expires_at, revoked_at
                     FROM grants WHERE tool = ?1 ORDER BY id DESC"
                }
                None => {
                    "SELECT id, tool, pattern, risk, granted_at, expires_at, revoked_at
                     FROM grants ORDER BY id DESC"
                }
            };
            let mut stmt = c.prepare(sql).map_err(|e| Error::Store(e.to_string()))?;
            let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Grant> {
                let risk_str: String = r.get(3)?;
                Ok(Grant {
                    id: r.get(0)?,
                    tool: r.get(1)?,
                    pattern: r.get(2)?,
                    risk: parse_risk(&risk_str),
                    granted_at: r.get(4)?,
                    expires_at: r.get(5)?,
                    revoked_at: r.get(6)?,
                })
            };
            let mut out = Vec::new();
            match tool {
                Some(t) => {
                    let rows = stmt
                        .query_map(params![t], map_row)
                        .map_err(|e| Error::Store(e.to_string()))?;
                    for row in rows {
                        out.push(row.map_err(|e| Error::Store(e.to_string()))?);
                    }
                }
                None => {
                    let rows = stmt
                        .query_map([], map_row)
                        .map_err(|e| Error::Store(e.to_string()))?;
                    for row in rows {
                        out.push(row.map_err(|e| Error::Store(e.to_string()))?);
                    }
                }
            }
            Ok(out)
        })
    }

    /// 撤销一条授权。
    pub fn revoke_grant(&self, id: i64) -> Result<bool> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "UPDATE grants SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                    params![now(), id],
                )
                .map_err(|e| Error::Store(format!("撤销失败: {e}")))?;
            Ok(n > 0)
        })
    }

    /// 清掉过期授权（可定期调用）。
    pub fn purge_expired_grants(&self) -> Result<usize> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "DELETE FROM grants WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                    params![now()],
                )
                .map_err(|e| Error::Store(format!("清理失败: {e}")))?;
            Ok(n)
        })
    }
}

fn parse_risk(s: &str) -> Risk {
    match s {
        "reversible" => Risk::Reversible,
        "impactful" => Risk::Impactful,
        "destructive" => Risk::Destructive,
        _ => Risk::ReadOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_find() {
        let s = Store::in_memory().unwrap();
        s.add_grant("shell", "git *", Risk::Impactful, None)
            .unwrap();
        let g = s.active_grants_for("shell").unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].pattern, "git *");
    }

    #[test]
    fn l3_cannot_be_granted() {
        let s = Store::in_memory().unwrap();
        let e = s
            .add_grant("shell", "rm -rf /", Risk::Destructive, None)
            .unwrap_err();
        assert!(matches!(e, Error::Denied(_)));
        assert!(s.list_grants().unwrap().is_empty());
    }

    #[test]
    fn revoke_makes_inactive() {
        let s = Store::in_memory().unwrap();
        let id = s.add_grant("shell", "ls", Risk::Impactful, None).unwrap();
        assert_eq!(s.active_grants_for("shell").unwrap().len(), 1);
        assert!(s.revoke_grant(id).unwrap());
        assert!(s.active_grants_for("shell").unwrap().is_empty());
        // 撤销后仍在列表里（UI 要能看见历史）
        assert_eq!(s.list_grants().unwrap().len(), 1);
    }

    #[test]
    fn revoking_twice_returns_false() {
        let s = Store::in_memory().unwrap();
        let id = s.add_grant("shell", "ls", Risk::Impactful, None).unwrap();
        assert!(s.revoke_grant(id).unwrap());
        assert!(!s.revoke_grant(id).unwrap());
    }

    #[test]
    fn expired_grant_is_inactive() {
        let s = Store::in_memory().unwrap();
        s.add_grant("shell", "ls", Risk::Impactful, Some(0))
            .unwrap();
        // days=0 → 立即过期
        assert!(s.active_grants_for("shell").unwrap().is_empty());
    }

    #[test]
    fn purge_removes_expired() {
        let s = Store::in_memory().unwrap();
        s.add_grant("shell", "ls", Risk::Impactful, Some(0))
            .unwrap();
        s.add_grant("fs_write", "", Risk::Reversible, None).unwrap();
        assert_eq!(s.purge_expired_grants().unwrap(), 1);
        assert_eq!(s.list_grants().unwrap().len(), 1);
    }

    #[test]
    fn grants_are_per_tool() {
        let s = Store::in_memory().unwrap();
        s.add_grant("shell", "", Risk::Impactful, None).unwrap();
        s.add_grant("fs_write", "", Risk::Reversible, None).unwrap();
        assert_eq!(s.active_grants_for("shell").unwrap().len(), 1);
        assert_eq!(s.active_grants_for("web_fetch").unwrap().len(), 0);
    }
}
