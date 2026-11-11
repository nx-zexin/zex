//! 审批等待机制。
//!
//! agent 发出 `ApprovalRequired` 事件后挂起，等 UI / CLI 回一个决定。
//! 两条硬约束（`docs/SECURITY.md` §10）：
//!
//! 1. **无人应答默认拒绝**，不是放行，也不是无限等待。
//! 2. **改参数必重验** —— 重启一轮后会重新走 `policy.evaluate`，
//!    已批准的决定不会被复用到参数不同的调用上（每个 `ApprovalId` 一次性）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, oneshot};
use zex_core::{ApprovalId, RememberScope};

/// 审批的默认等待时长。超时即拒绝。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 用户对一次审批请求的决定。
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// 批准。`remember` 非空时写入授权记忆。
    Approve {
        /// 是否记住此类操作。
        remember: Option<RememberScope>,
    },
    /// 拒绝。
    Reject {
        /// 原因，会作为工具结果回给模型。
        reason: String,
    },
}

impl ApprovalDecision {
    /// 简单批准，不记忆。
    pub fn approve() -> Self {
        ApprovalDecision::Approve { remember: None }
    }

    /// 拒绝并给出原因。
    pub fn reject(reason: impl Into<String>) -> Self {
        ApprovalDecision::Reject {
            reason: reason.into(),
        }
    }

    /// 超时或通道断开时的默认决定。
    ///
    /// **必须是拒绝** —— 无人应答不能等于放行。
    pub fn timeout_decision() -> Self {
        ApprovalDecision::Reject {
            reason: "用户未在时限内响应，已按默认拒绝处理".into(),
        }
    }
}

/// 审批注册表。
///
/// agent 与 UI/CLI 各持一个 `Arc<ApprovalBroker>`。agent 侧 `register` 后
/// 发事件并 `wait`；UI 侧收到事件后用同一个 `ApprovalId` 调 `resolve`。
#[derive(Debug, Clone, Default)]
pub struct ApprovalBroker {
    pending: Arc<Mutex<HashMap<ApprovalId, oneshot::Sender<ApprovalDecision>>>>,
}

impl ApprovalBroker {
    /// 新建。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个待审批项，返回接收端。
    ///
    /// 同一个 id 重复注册会覆盖旧的 —— 但调用方应为每次调用生成新 id，
    /// 这正是"改参数必重验"的保障。
    pub async fn register(&self, id: ApprovalId) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        rx
    }

    /// 递交决定。返回 `true` 表示找到了对应的待审批项。
    pub async fn resolve(&self, id: &ApprovalId, decision: ApprovalDecision) -> bool {
        let tx = self.pending.lock().await.remove(id);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// 等待决定，超时按拒绝处理。
    pub async fn wait(
        &self,
        id: &ApprovalId,
        rx: oneshot::Receiver<ApprovalDecision>,
        timeout: Duration,
    ) -> ApprovalDecision {
        let decision = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(d)) => d,
            // 发送端被 drop（UI 关了）→ 拒绝
            Ok(Err(_)) => {
                tracing::warn!(id = %id, "审批通道断开，按拒绝处理");
                ApprovalDecision::timeout_decision()
            }
            // 超时 → 拒绝
            Err(_) => {
                tracing::warn!(id = %id, "审批超时，按拒绝处理");
                ApprovalDecision::timeout_decision()
            }
        };
        // 无论走哪条路都清掉残留注册，避免表无限增长
        self.pending.lock().await.remove(id);
        decision
    }

    /// 当前挂起的审批数（用于诊断）。
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approve_roundtrip() {
        let broker = ApprovalBroker::new();
        let id = ApprovalId::generate();
        let rx = broker.register(id.clone()).await;
        assert_eq!(broker.pending_count().await, 1);

        assert!(broker.resolve(&id, ApprovalDecision::approve()).await);
        let d = broker.wait(&id, rx, Duration::from_secs(1)).await;
        assert!(matches!(d, ApprovalDecision::Approve { remember: None }));
    }

    #[tokio::test]
    async fn timeout_is_reject_not_allow() {
        let broker = ApprovalBroker::new();
        let id = ApprovalId::generate();
        let rx = broker.register(id.clone()).await;
        // 不 resolve，等它超时
        let d = broker.wait(&id, rx, Duration::from_millis(20)).await;
        match d {
            ApprovalDecision::Reject { reason } => assert!(reason.contains("未在时限内")),
            other => panic!("超时必须拒绝，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropped_sender_is_reject() {
        let broker = ApprovalBroker::new();
        let id = ApprovalId::generate();
        let rx = broker.register(id.clone()).await;
        // 模拟 UI 关闭：直接清掉注册表（sender 被 drop）
        broker.pending.lock().await.clear();
        let d = broker.wait(&id, rx, Duration::from_secs(1)).await;
        assert!(matches!(d, ApprovalDecision::Reject { .. }));
    }

    #[tokio::test]
    async fn resolve_unknown_id_returns_false() {
        let broker = ApprovalBroker::new();
        assert!(
            !broker
                .resolve(&ApprovalId::generate(), ApprovalDecision::approve())
                .await
        );
    }

    #[tokio::test]
    async fn wait_cleans_up_registration() {
        let broker = ApprovalBroker::new();
        let id = ApprovalId::generate();
        let rx = broker.register(id.clone()).await;
        broker.resolve(&id, ApprovalDecision::approve()).await;
        let _ = broker.wait(&id, rx, Duration::from_secs(1)).await;
        assert_eq!(broker.pending_count().await, 0);
    }

    #[tokio::test]
    async fn register_new_replaces_old_for_same_id() {
        // 语义验证：同 id 再注册会覆盖，旧 receiver 收不到东西
        let broker = ApprovalBroker::new();
        let id = ApprovalId::generate();
        let _old = broker.register(id.clone()).await;
        let new_rx = broker.register(id.clone()).await;
        broker.resolve(&id, ApprovalDecision::approve()).await;
        let d = broker.wait(&id, new_rx, Duration::from_secs(1)).await;
        assert!(matches!(d, ApprovalDecision::Approve { .. }));
    }

    #[test]
    fn reject_carries_reason() {
        let d = ApprovalDecision::reject("user said no");
        match d {
            ApprovalDecision::Reject { reason } => assert_eq!(reason, "user said no"),
            _ => panic!(),
        }
    }

    #[test]
    fn timeout_decision_is_always_reject() {
        assert!(matches!(
            ApprovalDecision::timeout_decision(),
            ApprovalDecision::Reject { .. }
        ));
    }
}
