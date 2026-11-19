//! 错误类型。

use thiserror::Error;

/// Zex 的统一错误类型。
#[derive(Debug, Error)]
pub enum Error {
    /// 配置缺失或非法。
    #[error("配置错误: {0}")]
    Config(String),

    /// LLM 调用失败。
    #[error("模型调用失败: {message}")]
    Llm {
        /// 人类可读的错误描述。
        message: String,
        /// HTTP 状态码（若适用）。
        status: Option<u16>,
        /// 是否可重试（429 / 5xx / 超时为 true）。
        retryable: bool,
    },

    /// 模型输出撞到上限被截断。
    ///
    /// 单独一个变体而不是当成"参数不合法"：这两件事的处置完全不同 ——
    /// JSON 写错了要让模型修语法，被截断则是**预算不够**，改语法没用，
    /// 得把调用拆小，或者把思考关掉（思考同样花这份预算）。
    #[error("输出被截断: {detail}")]
    Truncated {
        /// 人话说明：撞了什么上限、什么没做完、接下来怎么办。
        detail: String,
    },

    /// 工具参数不合法。
    #[error("工具 `{tool}` 参数错误: {message}")]
    BadArgs {
        /// 工具名。
        tool: String,
        /// 具体哪个参数不对。
        message: String,
    },

    /// 工具执行失败。
    #[error("工具 `{tool}` 执行失败: {message}")]
    ToolFailed {
        /// 工具名。
        tool: String,
        /// 失败原因。
        message: String,
    },

    /// 权限被拒绝。
    #[error("操作被拒绝: {0}")]
    Denied(String),

    /// 等待用户审批时被取消。
    #[error("审批被取消")]
    ApprovalCancelled,

    /// 用户中断了任务。
    #[error("任务已中断")]
    Cancelled,

    /// 当前平台不支持该能力。
    ///
    /// 上层应据此隐藏入口，而不是弹错误（见 `docs/PLATFORM.md` §3）。
    #[error("当前平台不支持: {0}")]
    Unsupported(String),

    /// 存储层错误。
    #[error("存储错误: {0}")]
    Store(String),

    /// IO 错误。
    #[error("IO 错误: {0}")]
    Io(String),

    /// 其他。
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// 该错误是否值得重试。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Llm {
                retryable: true,
                ..
            }
        )
    }

    /// 构造一个可重试的 LLM 错误。
    pub fn llm_retryable(message: impl Into<String>, status: Option<u16>) -> Self {
        Error::Llm {
            message: message.into(),
            status,
            retryable: true,
        }
    }

    /// 构造一个不可重试的 LLM 错误。
    pub fn llm_fatal(message: impl Into<String>, status: Option<u16>) -> Self {
        Error::Llm {
            message: message.into(),
            status,
            retryable: false,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Other(format!("JSON: {e}"))
    }
}

/// Zex 的统一 Result 别名。
pub type Result<T> = std::result::Result<T, Error>;
