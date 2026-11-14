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
        /// 所属会话。并行执行时 UI 据此丢弃别的会话的输出。
        session: SessionId,
        /// 本轮。
        turn: TurnId,
        /// 增量文本（可能为空串，表示心跳）。
        text: String,
    },
    /// 模型的推理过程增量（若 provider 支持）。
    Thinking {
        /// 所属会话。并行执行时据它判断该往哪个视图投递。
        session: SessionId,
        /// 本轮。
        turn: TurnId,
        /// 增量文本。
        text: String,
    },
    /// 即将执行工具。
    ToolCallBegin {
        /// 所属会话。并行执行时据它判断该往哪个视图投递。
        session: SessionId,
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
        /// 所属会话。并行执行时据它判断该往哪个视图投递。
        session: SessionId,
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
        /// 谁在等确认。并行时可能有多个任务同时要授权。
        session: SessionId,
        /// 审批请求。
        req: ApprovalRequest,
    },
    /// 发生了上下文压缩。
    ContextCompacted {
        /// 所属会话。并行执行时据它判断该往哪个视图投递。
        session: SessionId,
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
        /// 所属会话。并行执行时据它判断该往哪个视图投递。
        session: SessionId,
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
    /// 会话标题已更新（自动生成或用户重命名）。
    SessionTitled {
        /// 会话 id。
        session: String,
        /// 新标题。
        title: String,
    },
    /// 语音转写完成。
    Transcribed {
        /// 成功时的文本。
        text: Option<String>,
        /// 失败原因。
        error: Option<String>,
    },

    /// 子智能体团队的整体状态变了。
    ///
    /// 一次推全量而不是增量：团队通常只有几个成员，全量最省心，
    /// UI 也不用维护差分逻辑。
    TeamUpdated {
        /// 当前所有子智能体。
        agents: Vec<SubAgentView>,
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
        /// 最新的会话列表（新建/清空/切换后侧栏要跟着变）。
        sessions: Vec<SessionSummary>,
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

/// 侧栏用的会话摘要。
///
/// `zex-core` 不依赖 `zex-store`（分层：内核不认识 SQLite），所以这里
/// 用一个最简 DTO 把必要字段带过去。
#[derive(Debug, Clone)]
pub struct SessionSummary {
    /// 会话 id。
    pub id: String,
    /// 标题（可能为空，由 UI 决定怎么显示）。
    pub title: String,
    /// 最后更新时间（unix 秒）。
    pub updated_at: i64,
    /// 这个会话用的模型（`provider/model`；空串表示用配置默认）。
    pub model: String,
    /// 这个会话的运行模式。
    pub mode: RunMode,
}

/// 运行模式：这一轮允许多"放得开"。
///
/// 定义在 core 而不是 policy，是因为它要跨层走：
/// UI 选择 → 命令 → 内核执行。放在 policy 里会让 core 反向依赖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// 改文件之前先问。
    #[default]
    Confirm,
    /// 文件改动直接做（带快照），其它照旧。
    AutoEdit,
    /// 只读：出计划可以，动手不行。
    Plan,
    /// 减少确认次数：命令也不再逐条问，只剩不可逆的事必问。
    FullAccess,
}

impl RunMode {
    /// 给人看的名字（下拉菜单里显示的就是它）。
    pub fn label(self) -> &'static str {
        match self {
            RunMode::Confirm => "变更前确认",
            RunMode::AutoEdit => "自动编辑",
            RunMode::Plan => "计划模式",
            RunMode::FullAccess => "完全访问",
        }
    }

    /// 一句话说明这个模式会怎么对待你的文件。
    ///
    /// 菜单里显示在名字下面 —— 光看名字，"自动编辑"和"完全访问"
    /// 差在哪没人猜得出来。
    pub fn description(self) -> &'static str {
        match self {
            RunMode::Confirm => "改文件前先问我。",
            RunMode::AutoEdit => "自动编辑文件。",
            RunMode::Plan => "编辑前先出计划。",
            RunMode::FullAccess => "减少确认次数。",
        }
    }

    /// 菜单里的候选，顺序即展示顺序（由严到松）。
    pub fn all() -> [RunMode; 4] {
        [
            RunMode::Confirm,
            RunMode::AutoEdit,
            RunMode::Plan,
            RunMode::FullAccess,
        ]
    }
}

/// UI → 内核的命令。
#[derive(Debug, Clone)]
pub enum UiCommand {
    /// 用户提交了一条输入。
    UserPrompt {
        /// 目标会话。带上它，不同会话的任务才能并行 ——
        /// 内核不再假设"当前会话"就是唯一在执行的那个。
        session: SessionId,
        /// 覆盖本轮的模型（`provider/model`）。`None` 用内核默认。
        ///
        /// 放请求里而不是改内核状态：切模型不必重启，也不必影响
        /// 正在跑的其它会话。
        model: Option<String>,
        /// 覆盖本轮的运行模式。`None` 用内核默认。
        ///
        /// 和 `model` 一样走请求：模式是**每个会话**的选择，
        /// 存在进程状态里会让并行会话互相污染。
        mode: Option<RunMode>,
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
    /// 转写一段录音（语音输入）。
    Transcribe {
        /// 16 位 PCM 单声道 WAV。
        audio: Vec<u8>,
    },

    /// 开一个新会话。
    NewSession,
    /// 清空指定会话的消息（保留会话本身）。
    ClearSession {
        /// 目标会话。
        session: SessionId,
    },
    /// 删除会话。
    DeleteSession {
        /// 目标会话。
        session: SessionId,
    },
    /// 给会话改名。
    RenameSession {
        /// 目标会话。
        session: SessionId,
        /// 新标题。
        title: String,
    },
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
            session: SessionId::generate(),
            turn: TurnId::generate(),
            text: "hi".into(),
        };
        let cloned = ev.clone();
        assert!(matches!(cloned, CoreEvent::Token { .. }));
    }
}

/// 给界面看的子智能体快照。
///
/// 独立于内核里的状态机：UI 只关心怎么显示，不关心内部字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgentView {
    /// 标识。
    pub id: String,
    /// 角色。
    pub role: String,
    /// 状态标签（执行中 / 已完成 / 失败 / 已停止）。
    pub status: String,
    /// 负责什么。
    pub description: String,
    /// 它占用的会话，点进去能看过程。
    pub session: String,
    /// 派生它的主会话。团队列表只显示属于当前会话的那些。
    pub parent_session: String,
    /// 结果摘要（跑完之后才有）。
    pub result: Option<String>,
}
