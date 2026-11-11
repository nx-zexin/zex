//! 消息读写。
//!
//! `payload` 存 `Message` 的 JSON：消息结构会演进（thinking / images /
//! tool_calls），每变一次就改表结构不可持续。

use rusqlite::params;
use zex_core::{Error, Message, Result, SessionId};

use crate::{Store, now};

/// 一条落库的消息（含存储元信息）。
#[derive(Debug, Clone)]
pub struct StoredMessage {
    /// 自增主键，可用于断点续跑定位。
    pub row_id: i64,
    /// 消息本体。
    pub message: Message,
    /// token 估算（落库时的值，不是事后重算）。
    pub tokens: i64,
    /// 写入时间。
    pub created_at: i64,
}

impl Store {
    /// 追加一条消息。
    pub fn append_message(
        &self,
        session: &SessionId,
        message: &Message,
        tokens: usize,
    ) -> Result<i64> {
        let payload = serde_json::to_string(message)
            .map_err(|e| Error::Store(format!("序列化消息失败: {e}")))?;
        let role = format!("{:?}", message.role()).to_lowercase();
        let ts = now();

        self.with_conn(|c| {
            c.execute(
                "INSERT INTO messages (session_id, role, payload, tokens, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session.as_str(), role, payload, tokens as i64, ts],
            )
            .map_err(|e| Error::Store(format!("写入消息失败: {e}")))?;
            let id = c.last_insert_rowid();
            c.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![ts, session.as_str()],
            )
            .map_err(|e| Error::Store(format!("更新会话时间失败: {e}")))?;
            Ok(id)
        })
    }

    /// 读取一个会话的全部消息（按写入顺序）。
    pub fn load_messages(&self, session: &SessionId) -> Result<Vec<StoredMessage>> {
        self.with_conn(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT id, payload, tokens, created_at FROM messages
                     WHERE session_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| Error::Store(e.to_string()))?;
            let rows = stmt
                .query_map(params![session.as_str()], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map_err(|e| Error::Store(e.to_string()))?;

            let mut out = Vec::new();
            for row in rows {
                let (row_id, payload, tokens, created_at) =
                    row.map_err(|e| Error::Store(e.to_string()))?;
                // 单条坏数据不应让整个会话不可读：跳过并告警
                match serde_json::from_str::<Message>(&payload) {
                    Ok(message) => out.push(StoredMessage {
                        row_id,
                        message,
                        tokens,
                        created_at,
                    }),
                    Err(e) => {
                        tracing::warn!(row_id, error = %e, "消息反序列化失败，已跳过");
                    }
                }
            }
            Ok(out)
        })
    }

    /// 消息条数。
    pub fn count_messages(&self, session: &SessionId) -> Result<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params![session.as_str()],
                |r| r.get(0),
            )
            .map_err(|e| Error::Store(format!("计数失败: {e}")))
        })
    }

    /// 删除整个会话的消息（用于"清空对话"）。
    pub fn clear_messages(&self, session: &SessionId) -> Result<usize> {
        self.with_conn(|c| {
            let n = c
                .execute(
                    "DELETE FROM messages WHERE session_id = ?1",
                    params![session.as_str()],
                )
                .map_err(|e| Error::Store(format!("清空失败: {e}")))?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zex_core::{Part, ToolCall, ToolCallId};

    fn setup() -> (Store, SessionId) {
        let s = Store::in_memory().unwrap();
        let ses = s.create_session("t", "m", "/tmp").unwrap();
        (s, ses.id)
    }

    #[test]
    fn append_and_load_roundtrip() {
        let (s, sid) = setup();
        s.append_message(&sid, &Message::user("hello"), 10).unwrap();
        s.append_message(&sid, &Message::system("sys"), 5).unwrap();
        let msgs = s.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].message.text(), "hello");
        assert_eq!(msgs[0].tokens, 10);
    }

    #[test]
    fn preserves_tool_calls() {
        let (s, sid) = setup();
        let m = Message::Assistant {
            content: vec![Part::text("ok")],
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("c1"),
                name: "fs_read".into(),
                arguments: serde_json::json!({"path": "a"}),
            }],
        };
        s.append_message(&sid, &m, 20).unwrap();
        let back = &s.load_messages(&sid).unwrap()[0].message;
        assert_eq!(back, &m);
    }

    #[test]
    fn order_is_insertion_order() {
        let (s, sid) = setup();
        for i in 0..5 {
            s.append_message(&sid, &Message::user(format!("m{i}")), 1)
                .unwrap();
        }
        let msgs = s.load_messages(&sid).unwrap();
        let texts: Vec<String> = msgs.iter().map(|m| m.message.text()).collect();
        assert_eq!(texts, vec!["m0", "m1", "m2", "m3", "m4"]);
    }

    #[test]
    fn count_and_clear() {
        let (s, sid) = setup();
        s.append_message(&sid, &Message::user("a"), 1).unwrap();
        s.append_message(&sid, &Message::user("b"), 1).unwrap();
        assert_eq!(s.count_messages(&sid).unwrap(), 2);
        assert_eq!(s.clear_messages(&sid).unwrap(), 2);
        assert_eq!(s.count_messages(&sid).unwrap(), 0);
    }

    #[test]
    fn sessions_are_isolated() {
        let (s, a) = setup();
        let b = s.create_session("other", "m", "/tmp").unwrap().id;
        s.append_message(&a, &Message::user("in-a"), 1).unwrap();
        s.append_message(&b, &Message::user("in-b"), 1).unwrap();
        assert_eq!(s.load_messages(&a).unwrap().len(), 1);
        assert_eq!(s.load_messages(&b).unwrap()[0].message.text(), "in-b");
    }

    #[test]
    fn corrupt_payload_is_skipped_not_fatal() {
        let (s, sid) = setup();
        s.append_message(&sid, &Message::user("good"), 1).unwrap();
        // 手动插入一条坏数据
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO messages (session_id, role, payload, tokens, created_at)
                 VALUES (?1, 'user', '{not json', 0, 0)",
                params![sid.as_str()],
            )
            .map_err(|e| Error::Store(e.to_string()))?;
            Ok(())
        })
        .unwrap();
        let msgs = s.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 1, "坏数据应被跳过而不是让整个会话不可读");
    }
}
