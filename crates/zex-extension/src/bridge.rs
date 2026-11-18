//! 扩展 → 内核的请求/回包桥。
//!
//! # 为什么需要它
//!
//! 两个世界的线程模型对不上：
//!
//! - `ShellRuntime` 是 `Rc`，`!Send`，只能待在 UI 线程。
//! - 内核跑在独立的 tokio 线程上，靠 `mpsc` 收命令、发事件。
//!
//! 所以扩展里 `zex.ask("...")` 这一句要跨过去再回来。gpui-shell 为此
//! 留了口子：`HostModule::async_function` 的同步半边跑主线程（能碰
//! `Entity`、能发命令），返回的 future 必须是 `Send`（能在别处等回包）。
//!
//! # 回包怎么认路
//!
//! zex 的事件里已经有 `session` 字段，内核本来就是多会话并行的。所以
//! 每个扩展分一个独立会话（`ext-<id>`），回包按会话号归位 ——
//! 不必再往 `CoreEvent` 里加"这是谁的请求"。
//!
//! 代价是**同一扩展的并发调用会串行**。这是刻意的：同一会话里的两轮
//! 对话本来就该排队（内核为每个会话加了锁），否则上下文会交错。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use zex_core::event::UiCommand;
use zex_core::SessionId;

/// 一次等待中的回包。
type Waiter = oneshot::Sender<Reply>;

/// 扩展拿到的一次回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// 模型输出（可能为空，比如整轮都失败了）。
    pub text: String,
    /// 失败原因。正常结束时为 `None`。
    pub error: Option<String>,
}

impl Reply {
    /// 成功且有内容。
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: None,
        }
    }

    /// 失败。
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            error: Some(message.into()),
        }
    }
}

/// 到内核的桥。可以自由克隆（内部是 `Arc`）。
#[derive(Clone)]
pub struct AgentBridge {
    cmd_tx: mpsc::Sender<UiCommand>,
    /// 会话 id → 等回包的人。
    ///
    /// 用 `std::sync::Mutex` 而不是 tokio 的：临界区只有几次 map 操作，
    /// 没有 await，用异步锁纯属多余，还会让同步的 `complete` 变成异步。
    pending: Arc<Mutex<HashMap<String, Waiter>>>,
}

impl AgentBridge {
    /// 用内核的命令通道建一个桥。
    pub fn new(cmd_tx: mpsc::Sender<UiCommand>) -> Self {
        Self {
            cmd_tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 向 `session` 这个会话提一个问题，等最终回复。
    ///
    /// 返回的 future 是 `Send` 的：`async_function` 要求如此，
    /// 它会在主线程之外的执行器上被 poll。
    pub async fn ask(&self, session: &str, prompt: String) -> Reply {
        let (tx, rx) = oneshot::channel();
        // 先挂上等待，再发命令 —— 反过来的话，回复可能比登记先到，
        // 那条回复就成了没人认领的孤儿
        {
            let mut held = match self.pending.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(stale) = held.insert(session.to_string(), tx) {
                // 同一会话已有等待者：把旧的放掉，免得它永远挂着
                let _ = stale.send(Reply::err("被同一扩展的新请求取代"));
            }
        }

        let cmd = UiCommand::UserPrompt {
            session: SessionId::from(session),
            // 三个 `None`（模型 / 模式 / 思考强度）都是有意为之：
            // 扩展不该悄悄替用户改本轮怎么跑，用内核默认即可
            model: None,
            mode: None,
            thinking: None,
            text: prompt,
            attachments: Vec::new(),
        };
        if self.cmd_tx.send(cmd).await.is_err() {
            self.take_pending(session);
            return Reply::err("内核已停止，无法执行");
        }

        match rx.await {
            Ok(reply) => reply,
            // 发送端被丢了（会话被清掉、应用退出）
            Err(_) => Reply::err("未收到回复"),
        }
    }

    /// 会话 `session` 的一轮结束了，把攒下的文本交给等它的人。
    ///
    /// 由 UI 线程在收到 `TurnEnded` 时调用。没有等待者时是空操作 ——
    /// 用户自己在这个会话里打字也会走到这里。
    pub fn complete(&self, session: &str, reply: Reply) {
        if let Some(waiter) = self.take_pending(session) {
            let _ = waiter.send(reply);
        }
    }

    /// 这个会话是否有扩展在等回包。
    pub fn is_waiting(&self, session: &str) -> bool {
        match self.pending.lock() {
            Ok(g) => g.contains_key(session),
            Err(poisoned) => poisoned.into_inner().contains_key(session),
        }
    }

    /// 丢掉某个会话的等待者，返回它。
    fn take_pending(&self, session: &str) -> Option<Waiter> {
        let mut held = match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.remove(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> (AgentBridge, mpsc::Receiver<UiCommand>) {
        let (tx, rx) = mpsc::channel(8);
        (AgentBridge::new(tx), rx)
    }

    #[tokio::test]
    async fn ask_sends_prompt_for_the_extension_session() {
        let (b, mut rx) = bridge();
        let task = tokio::spawn({
            let b = b.clone();
            async move { b.ask("ext-demo", "你好".into()).await }
        });
        let cmd = rx.recv().await.expect("命令应发出");
        match cmd {
            UiCommand::UserPrompt {
                session, text, model, ..
            } => {
                assert_eq!(session.as_str(), "ext-demo");
                assert_eq!(text, "你好");
                // 不覆盖用户的模型选择
                assert!(model.is_none());
            }
            _ => panic!("应该是 UserPrompt"),
        }
        b.complete("ext-demo", Reply::ok("回话"));
        let reply = task.await.unwrap();
        assert_eq!(reply.text, "回话");
        assert!(reply.error.is_none());
    }

    #[tokio::test]
    async fn complete_without_waiter_is_a_noop() {
        let (b, _rx) = bridge();
        // 用户在普通会话里打字时也会走到这里，不能 panic 也不能卡住
        b.complete("ses-user", Reply::ok("x"));
        assert!(!b.is_waiting("ses-user"));
    }

    #[tokio::test]
    async fn second_ask_supersedes_the_first() {
        let (b, mut rx) = bridge();
        let first = tokio::spawn({
            let b = b.clone();
            async move { b.ask("ext-x", "第一次".into()).await }
        });
        let _ = rx.recv().await;
        let second = tokio::spawn({
            let b = b.clone();
            async move { b.ask("ext-x", "第二次".into()).await }
        });
        let _ = rx.recv().await;

        // 先到的那次应该被判为被取代，而不是永远挂着
        let r1 = first.await.unwrap();
        assert!(r1.error.is_some(), "旧请求应收到明确的失败");

        b.complete("ext-x", Reply::ok("第二次的回复"));
        let r2 = second.await.unwrap();
        assert_eq!(r2.text, "第二次的回复");
    }

    #[tokio::test]
    async fn closed_core_reports_error_instead_of_hanging() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let b = AgentBridge::new(tx);
        let reply = b.ask("ext-gone", "在吗".into()).await;
        assert!(reply.error.is_some());
    }

    #[test]
    fn waiting_flag_tracks_registration() {
        let (b, _rx) = bridge();
        assert!(!b.is_waiting("ext-a"));
        // 直接塞一个等待者进去（不跨线程，只为验证查询）
        let (tx, _rx2) = oneshot::channel();
        b.pending.lock().unwrap().insert("ext-a".into(), tx);
        assert!(b.is_waiting("ext-a"));
        b.complete("ext-a", Reply::ok("done"));
        assert!(!b.is_waiting("ext-a"));
    }
}
