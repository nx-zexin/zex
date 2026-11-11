//! 审计日志。
//!
//! **只追加。** 表上有 trigger 拒绝 UPDATE / DELETE（见 `migrate::v1_initial`），
//! 应用层拿不到修改能力 —— 这是审计可信的物理保证，不是"我们承诺不改"。

use rusqlite::params;
use serde::{Deserialize, Serialize};
use zex_core::{Error, Result, Risk};

use crate::{Store, now};

/// 一条审计记录的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// 工具调用。
    ToolCall,
    /// 审批决定。
    Approval,
    /// 配置变更。
    ConfigChange,
    /// 自我修改（M4）。
    SelfModify,
    /// 会话生命周期。
    Session,
}

impl AuditKind {
    /// 线格式字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            AuditKind::ToolCall => "tool_call",
            AuditKind::Approval => "approval",
            AuditKind::ConfigChange => "config_change",
            AuditKind::SelfModify => "self_modify",
            AuditKind::Session => "session",
        }
    }
}

/// 一条审计记录。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// 类别。
    pub kind: AuditKind,
    /// 风险等级。
    pub risk: Risk,
    /// 工具名（非工具调用时为空）。
    pub tool: String,
    /// 参数摘要（**已脱敏**）。
    pub args_digest: String,
    /// 结果摘要。
    pub result: String,
    /// 谁批准的：`auto` / `user` / `grant:12`。
    pub approved_by: String,
    /// 关联会话。
    pub session_id: String,
}

impl AuditEntry {
    /// 构造一条工具调用记录。
    pub fn tool_call(
        risk: Risk,
        tool: impl Into<String>,
        args_digest: impl Into<String>,
        approved_by: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        AuditEntry {
            kind: AuditKind::ToolCall,
            risk,
            tool: tool.into(),
            args_digest: args_digest.into(),
            result: String::new(),
            approved_by: approved_by.into(),
            session_id: session.into(),
        }
    }

    /// 补充结果摘要。
    pub fn with_result(mut self, result: impl Into<String>) -> Self {
        self.result = result.into();
        self
    }
}

/// 审计查询结果。
#[derive(Debug, Clone)]
pub struct AuditRow {
    /// 自增 id。
    pub id: i64,
    /// 时间。
    pub ts: i64,
    /// 原始记录。
    pub entry: AuditEntry,
}

impl Store {
    /// 写入一条审计。
    pub fn write_audit(&self, entry: &AuditEntry) -> Result<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO audit (ts, kind, risk, tool, args_digest, result, approved_by, session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    now(),
                    entry.kind.as_str(),
                    format!("{:?}", entry.risk).to_lowercase(),
                    entry.tool,
                    entry.args_digest,
                    entry.result,
                    entry.approved_by,
                    entry.session_id,
                ],
            )
            .map_err(|e| Error::Store(format!("写审计失败: {e}")))?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 最近 N 条审计（按时间倒序）。
    pub fn list_audit(&self, limit: usize) -> Result<Vec<AuditRow>> {
        self.list_audit_filtered(limit, None)
    }

    /// 按会话过滤的审计。
    pub fn list_audit_filtered(
        &self,
        limit: usize,
        session: Option<&str>,
    ) -> Result<Vec<AuditRow>> {
        self.with_conn(|c| {
            let (sql, args): (&str, Vec<String>) = match session {
                Some(s) => (
                    "SELECT id, ts, kind, risk, tool, args_digest, result, approved_by, session_id
                     FROM audit WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
                    vec![s.to_string(), limit.to_string()],
                ),
                None => (
                    "SELECT id, ts, kind, risk, tool, args_digest, result, approved_by, session_id
                     FROM audit ORDER BY id DESC LIMIT ?1",
                    vec![limit.to_string()],
                ),
            };
            let mut stmt = c.prepare(sql).map_err(|e| Error::Store(e.to_string()))?;
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                args.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(params_ref.as_slice(), |r| {
                    let risk_str: String = r.get(3)?;
                    Ok(AuditRow {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        entry: AuditEntry {
                            kind: parse_kind(&r.get::<_, String>(2)?),
                            risk: parse_risk(&risk_str),
                            tool: r.get(4)?,
                            args_digest: r.get(5)?,
                            result: r.get(6)?,
                            approved_by: r.get(7)?,
                            session_id: r.get(8)?,
                        },
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

    /// 审计总条数。
    pub fn count_audit(&self) -> Result<i64> {
        self.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))
                .map_err(|e| Error::Store(e.to_string()))
        })
    }
}

fn parse_kind(s: &str) -> AuditKind {
    match s {
        "approval" => AuditKind::Approval,
        "config_change" => AuditKind::ConfigChange,
        "self_modify" => AuditKind::SelfModify,
        "session" => AuditKind::Session,
        _ => AuditKind::ToolCall,
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

/// 把工具参数压成可长期保存的摘要。
///
/// 原则：**能定位，不能泄露**。
/// - 保留键名与结构，便于事后复盘"它当时想做什么"
/// - 值超过阈值就截断并标注长度，避免把整段文件内容写进审计
pub fn digest_args(args: &serde_json::Value) -> String {
    const MAX_VALUE_LEN: usize = 200;
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => {
                if s.chars().count() > MAX_VALUE_LEN {
                    let head: String = s.chars().take(MAX_VALUE_LEN).collect();
                    out.push_str(&format!("\"{head}…[{} 字符]\"", s.chars().count()));
                } else {
                    out.push_str(&serde_json::Value::String(s.clone()).to_string());
                }
            }
            serde_json::Value::Object(map) => {
                out.push('{');
                for (i, (k, val)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("\"{k}\":"));
                    walk(val, out);
                }
                out.push('}');
            }
            serde_json::Value::Array(arr) => {
                out.push('[');
                for (i, val) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    walk(val, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    walk(args, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_list() {
        let s = Store::in_memory().unwrap();
        let e = AuditEntry::tool_call(
            Risk::Impactful,
            "shell",
            "{\"cmd\":\"ls\"}",
            "user",
            "ses_1",
        )
        .with_result("ok");
        s.write_audit(&e).unwrap();
        let rows = s.list_audit(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.tool, "shell");
        assert_eq!(rows[0].entry.risk, Risk::Impactful);
        assert_eq!(rows[0].entry.approved_by, "user");
    }

    #[test]
    fn newest_first() {
        let s = Store::in_memory().unwrap();
        for i in 0..3 {
            s.write_audit(&AuditEntry::tool_call(
                Risk::ReadOnly,
                format!("t{i}"),
                "",
                "auto",
                "s",
            ))
            .unwrap();
        }
        let rows = s.list_audit(10).unwrap();
        assert_eq!(rows[0].entry.tool, "t2");
    }

    #[test]
    fn filter_by_session() {
        let s = Store::in_memory().unwrap();
        s.write_audit(&AuditEntry::tool_call(
            Risk::ReadOnly,
            "a",
            "",
            "auto",
            "s1",
        ))
        .unwrap();
        s.write_audit(&AuditEntry::tool_call(
            Risk::ReadOnly,
            "b",
            "",
            "auto",
            "s2",
        ))
        .unwrap();
        let rows = s.list_audit_filtered(10, Some("s1")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.tool, "a");
    }

    #[test]
    fn risk_roundtrips_through_db() {
        let s = Store::in_memory().unwrap();
        for risk in [
            Risk::ReadOnly,
            Risk::Reversible,
            Risk::Impactful,
            Risk::Destructive,
        ] {
            s.write_audit(&AuditEntry::tool_call(risk, "x", "", "auto", "s"))
                .unwrap();
        }
        let risks: Vec<Risk> = s
            .list_audit(10)
            .unwrap()
            .iter()
            .map(|r| r.entry.risk)
            .collect();
        assert!(risks.contains(&Risk::Destructive));
        assert!(risks.contains(&Risk::Reversible));
    }

    #[test]
    fn digest_truncates_long_values() {
        let v = serde_json::json!({"content": "x".repeat(1000)});
        let d = digest_args(&v);
        assert!(d.len() < 300, "摘要应被截断，实际 {} 字符", d.len());
        assert!(d.contains("1000 字符"));
    }

    #[test]
    fn digest_keeps_structure_and_keys() {
        let v = serde_json::json!({"path": "a.txt", "n": 3, "ok": true});
        let d = digest_args(&v);
        assert!(d.contains("path"));
        assert!(d.contains("a.txt"));
        assert!(d.contains('3'));
    }

    #[test]
    fn digest_handles_nested() {
        let v = serde_json::json!({"a": {"b": [1, 2, {"c": "d"}]}});
        let d = digest_args(&v);
        assert!(d.contains("\"a\""));
        assert!(d.contains("\"c\""));
    }
}
