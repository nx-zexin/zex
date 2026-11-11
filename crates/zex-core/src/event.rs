//! 双世界之间传递的事件与命令。
//!
//! 这是 **gpui 世界** 与 **tokio 内核** 的唯一接口（见 `docs/ARCHITECTURE.md` §2）。
//!
//! 硬性约束：本模块的类型必须是**可 `Clone` 的纯数据**，
//! 不得包含 `Entity` / `Window` / 任何 gpui 类型 —— 否则内核就被迫感知 UI。

use serde::{Deserialize, Serialize};

use crate::ids::{ApprovalId, SessionId, ToolCallId, TurnId};
use crate::message::Usage;
use crate::risk::Risk;

/// 一轮对话为何结束。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndReason {
    /// 模型给出了最终回复。
    Completed,
    /// 用户中断。
    Cancelled,
    /// 达到最大轮次（工具调用循环上限）。
    MaxSteps {
        /// 上限值。
        limit: u32,
    },
    /// 出错终止。
    Failed {
        /// 错误描述。
        message: String,
    },
    /// token / 金额预算耗尽。
    BudgetExhausted,
}

/// 内核 → UI 的事件。
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// 一轮开始。
    TurnStarted {
        /// 会话。
        session: SessionId,
        /// 本轮。
        turn: TurnId,
    },
    /// 模型输出的文本增量。
    Token {
        /// 本轮。
        turn: TurnId,
        /// 增量文本（可能为空串，表示心跳）。
        text: String,
    },
    /// 模型的推理过程增量（若 provider 支持）。
    Thinking {
        /// 本轮。
        turn: TurnId,
        /// 增量文本。
        text: String,
    },
    /// 即将执行工具。
    ToolCallBegin {
        /// 本轮。
        turn: TurnId,
        /// 调用 id。
        call_id: ToolCallId,
        /// 工具名。
        name: String,
        /// 参数（已解析）。
        arguments: serde_json::Value,
        /// 风险等级，UI 据此着色。
        risk: Risk,
    },
    /// 工具执行完毕。
    ToolCallEnd {
        /// 本轮。
        turn: TurnId,
        /// 调用 id。
        call_id: ToolCallId,
        /// 是否成功。
        ok: bool,
        /// 一句话摘要（不塞全文，避免 UI 卡顿）。
        brief: String,
    },
    /// 需要用户审批。
    ///
    /// UI 必须回一个 [`UiCommand::Approve`] 或 [`UiCommand::Reject`]。
    ApprovalRequired {
        /// 审批请求。
        req: ApprovalRequest,
    },
    /// 发生了上下文压缩。
    ContextCompacted {
        /// 本轮。
        turn: TurnId,
        /// 压缩前 token 估算。
        before_tokens: u64,
        /// 压缩后 token 估算。
        after_tokens: u64,
        /// 采用了哪种策略。
        strategy: String,
    },
    /// 一轮结束。
    TurnEnded {
        /// 本轮。
        turn: TurnId,
        /// 为什么结束。
        reason: EndReason,
        /// token 用量。
        usage: Usage,
    },
    /// 非致命错误（展示给用户，不终止会话）。
    Warning {
        /// 描述。
        message: String,
    },
    /// 致命错误。
    Fatal {
        /// 描述。
        message: String,
    },
    /// 已切换到另一个会话，附带该会话的完整历史。
    ///
    /// 历史随事件一起送来，UI 不必再去查库 —— 否则容易和内核的
    /// 状态不一致（比如切换过程中又追加了一条消息）。
    SessionSwitched {
        /// 新会话 id。
        session: String,
        /// 历史消息。
        history: Vec<crate::message::Message>,
    },
}

/// 一条待审批的请求。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    /// 请求 id，回传时用它配对。
    pub id: ApprovalId,
    /// 涉及的轮次。
    pub turn: TurnId,
    /// 工具名。
    pub tool: String,
    /// 风险等级。
    pub risk: Risk,
    /// 参数（敏感值已脱敏）。
    pub arguments: serde_json::Value,
    /// 人类可读的意图说明，供用户判断。
    pub summary: String,
    /// 能否被"记住"（L3 永远为 `false`）。
    pub grantable: bool,
    /// 参数是否含有来自不可信来源的内容（提示注入防线，见 SECURITY.md §5.2）。
    pub tainted: bool,
}

/// UI → 内核的命令。
#[derive(Debug, Clone)]
pub enum UiCommand {
    /// 用户提交了一条输入。
    UserPrompt {
        /// 文本。
        text: String,
        /// 附件（图片等）。
        attachments: Vec<crate::message::ImageRef>,
    },
    /// 批准一次审批请求。
    Approve {
        /// 请求 id。
        id: ApprovalId,
        /// 是否记住此类操作。
        remember: Option<RememberScope>,
    },
    /// 拒绝一次审批请求。
    Reject {
        /// 请求 id。
        id: ApprovalId,
        /// 拒绝原因，会作为工具结果回给模型。
        reason: String,
    },
    /// 中断当前轮。
    Cancel {
        /// 轮次。
        turn: TurnId,
    },
    /// 开一个新会话。
    NewSession,
    /// 清空当前会话的消息（保留会话本身）。
    ClearSession,
    /// 切换到另一个会话。
    SwitchSession {
        /// 目标会话。
        session: SessionId,
    },
    /// 关闭内核（退出应用）。
    Shutdown,
}

/// "记住这个授权"的范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RememberScope {
    /// 只记住这次完全相同参数。
    Exact,
    /// 记住这个工具（+ 可选参数模式），在一定期限内。
    Pattern {
        /// glob 模式，如 `git *`。`None` 表示该工具全部参数。
        pattern: Option<String>,
        /// 有效天数。`None` 表示永久。
        days: Option<u32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_reason_serde() {
        let r = EndReason::MaxSteps { limit: 25 };
        let back: EndReason = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn remember_scope_serde() {
        let s = RememberScope::Pattern {
            pattern: Some("git *".into()),
            days: Some(7),
        };
        let back: RememberScope =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn core_event_is_clone_and_pure_data() {
        // 能 clone 即说明不含 Entity/Window 这类不可 clone 的 gpui 类型
        let ev = CoreEvent::Token {
            turn: TurnId::generate(),
            text: "hi".into(),
        };
        let cloned = ev.clone();
        assert!(matches!(cloned, CoreEvent::Token { .. }));
    }
}
