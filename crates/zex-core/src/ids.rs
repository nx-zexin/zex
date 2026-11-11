//! 各种 id 的强类型包装。
//!
//! 用 newtype 而不是裸 `String`：防止把 `SessionId` 传给要 `ToolCallId` 的地方。
//! 这类 bug 在 agent 里后果很严重（工具结果配对错乱）。

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 用已有字符串构造（不校验前缀）。
            pub fn new(s: impl Into<String>) -> Self {
                $name(s.into())
            }

            /// 生成一个新的唯一 id，带类型前缀。
            pub fn generate() -> Self {
                $name(format!("{}_{}", $prefix, uuid::Uuid::now_v7().simple()))
            }

            /// 取内部字符串。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_owned())
            }
        }
    };
}

define_id! {
    /// 会话 id。
    SessionId, "ses"
}

define_id! {
    /// 单轮对话 id（一次用户输入到最终回复）。
    TurnId, "turn"
}

define_id! {
    /// 工具调用 id，由 provider 给出或本地生成，用于配对请求与结果。
    ToolCallId, "call"
}

define_id! {
    /// 审批请求 id。
    ApprovalId, "appr"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_prefixed_and_unique() {
        let a = SessionId::generate();
        let b = SessionId::generate();
        assert!(a.as_str().starts_with("ses_"));
        assert_ne!(a, b);
    }

    #[test]
    fn ids_are_type_safe() {
        // 这行若取消注释应当编译失败 —— newtype 的意义
        // let _: ToolCallId = SessionId::generate();
        let x: SessionId = "abc".into();
        assert_eq!(x.as_str(), "abc");
    }

    #[test]
    fn serde_is_transparent() {
        let id = ToolCallId::new("call_1");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"call_1\"");
    }
}
