//! 统一的消息表示。
//!
//! 协议适配层负责 `Message` ⇄ 各 provider 线格式的双向映射。
//! 内部表示刻意比任何单一协议都更中性。

use serde::{Deserialize, Serialize};

use crate::ids::ToolCallId;

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统提示。
    System,
    /// 用户。
    User,
    /// 模型。
    Assistant,
    /// 工具结果。
    Tool,
}

/// 图片引用。实际字节由存储层管理，这里只存定位信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// 媒体类型，如 `image/png`。
    pub media_type: String,
    /// 存储键（`zex-store` 里的 blob id）。
    pub blob_id: String,
}

/// 消息内容的一部分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// 纯文本。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图片。
    Image(ImageRef),
    /// 模型的推理过程（Anthropic thinking / OpenAI reasoning）。
    ///
    /// 是否回传给模型取决于 provider —— 大部分情况不应回传。
    Thinking {
        /// 推理文本。
        text: String,
    },
}

impl Part {
    /// 便捷构造文本片段。
    pub fn text(s: impl Into<String>) -> Self {
        Part::Text { text: s.into() }
    }

    /// 提取文本内容，非文本返回 `None`。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Part::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// 模型请求的一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// provider 给出的调用 id，用于配对工具结果。
    pub id: ToolCallId,
    /// 工具名。
    pub name: String,
    /// 参数（已解析为 JSON）。
    ///
    /// 流式期间是分片到达的 JSON 字符串，由适配层拼接后解析。
    pub arguments: serde_json::Value,
}

/// 一条完整的消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// 系统提示。
    System {
        /// 提示内容。
        content: String,
    },
    /// 用户消息。
    User {
        /// 内容片段。
        content: Vec<Part>,
    },
    /// 模型消息。
    Assistant {
        /// 内容片段。
        content: Vec<Part>,
        /// 本轮的请求的工具调用（可能为空）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// 工具执行结果。
    Tool {
        /// 对应的调用 id。
        call_id: ToolCallId,
        /// 工具名（部分 provider 需要）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 结果文本。
        content: String,
        /// 是否执行失败。模型可据此决定是否换方法。
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    /// 便捷构造用户文本消息。
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            content: vec![Part::text(text)],
        }
    }

    /// 便捷构造系统消息。
    pub fn system(text: impl Into<String>) -> Self {
        Message::System {
            content: text.into(),
        }
    }

    /// 该消息的角色。
    pub fn role(&self) -> Role {
        match self {
            Message::System { .. } => Role::System,
            Message::User { .. } => Role::User,
            Message::Assistant { .. } => Role::Assistant,
            Message::Tool { .. } => Role::Tool,
        }
    }

    /// 拼接该消息的全部文本内容。
    pub fn text(&self) -> String {
        match self {
            Message::System { content } => content.clone(),
            Message::User { content } | Message::Assistant { content, .. } => content
                .iter()
                .filter_map(Part::as_text)
                .collect::<Vec<_>>()
                .join(""),
            Message::Tool { content, .. } => content.clone(),
        }
    }
}

/// token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 缓存命中的输入 token（若 provider 提供）。
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl Usage {
    /// 总 token 数。
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// 累加。
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip() {
        let m = Message::user("hello");
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn assistant_with_tool_calls_roundtrip() {
        let m = Message::Assistant {
            content: vec![Part::text("let me check")],
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call_1"),
                name: "fs_read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn tool_message_roundtrip() {
        let m = Message::Tool {
            call_id: ToolCallId::new("call_1"),
            name: Some("fs_read".into()),
            content: "file body".into(),
            is_error: false,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn role_tagging_is_correct() {
        assert_eq!(Message::user("x").role(), Role::User);
        assert_eq!(Message::system("x").role(), Role::System);
    }

    #[test]
    fn usage_accumulates() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 2,
        };
        a.add(Usage {
            input_tokens: 1,
            output_tokens: 2,
            cached_input_tokens: 0,
        });
        assert_eq!(a.total(), 18);
        assert_eq!(a.cached_input_tokens, 2);
    }
}
