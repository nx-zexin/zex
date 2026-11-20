//! OpenAI 兼容协议的适配。
//!
//! 构造请求体与解析响应片段都是**纯函数**，可以完全离线测试；
//! `stream_chat` 只负责把它们接到网络上。

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zex_core::{Error, Message, Part, Result, Usage};

use crate::sse::SseDecoder;
use crate::{ChatRequest, ChatStream, ProviderConfig, StreamEvent, retry};

/// 构造 `POST /v1/chat/completions` 的请求体。
pub fn build_body(model: &str, req: &ChatRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = &req.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in &req.messages {
        messages.push(message_to_wire(m));
    }

    // 实际发出去的上限记一条 debug：截断问题排查全靠它，
    // 不然只能靠猜"到底发的是 4096 还是 8192"
    tracing::debug!(
        model,
        max_tokens = req.max_tokens,
        messages = req.messages.len(),
        tools = req.tools.len(),
        thinking = ?req.thinking,
        "发起对话请求"
    );
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        // 没有这个字段，OpenAI 兼容端点不会在流里返回 usage，
        // 我们就永远算不出成本。
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_tokens,
    });

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    // 思考强度：只对支持它的端点有效（o 系列、部分兼容实现）。
    // 不支持的端点会直接忽略这个字段，不会报错。
    if let Some(effort) = req.thinking.openai_effort() {
        body["reasoning_effort"] = json!(effort);
    }

    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect(),
        );
    }

    body
}

/// 单条消息 → 线格式。
fn message_to_wire(m: &Message) -> Value {
    match m {
        Message::System { content } => json!({"role": "system", "content": content}),
        Message::User { content } => {
            // 纯文本走字符串形式（兼容性最好），含图片才用数组形式
            if content.iter().all(|p| matches!(p, Part::Text { .. })) {
                json!({"role": "user", "content": join_text(content)})
            } else {
                let parts: Vec<Value> = content.iter().map(part_to_wire).collect();
                json!({"role": "user", "content": parts})
            }
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let text = join_text(content);
            if tool_calls.is_empty() {
                json!({"role": "assistant", "content": text})
            } else {
                json!({
                    "role": "assistant",
                    // 有工具调用时 content 允许为 null
                    "content": if text.is_empty() { Value::Null } else { json!(text) },
                    "tool_calls": tool_calls.iter().map(|c| json!({
                        "id": c.id.as_str(),
                        "type": "function",
                        "function": {
                            "name": c.name,
                            // arguments 必须是 JSON *字符串*，不是对象
                            "arguments": c.arguments.to_string(),
                        }
                    })).collect::<Vec<_>>(),
                })
            }
        }
        Message::Tool {
            call_id, content, ..
        } => {
            json!({"role": "tool", "tool_call_id": call_id.as_str(), "content": content})
        }
    }
}

fn part_to_wire(p: &Part) -> Value {
    match p {
        Part::Text { text } => json!({"type": "text", "text": text}),
        Part::Image(img) => json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{};base64,{}", img.media_type, img.blob_id)}
        }),
        // 推理过程不回传给模型
        Part::Thinking { .. } => json!({"type": "text", "text": ""}),
    }
}

fn join_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(Part::as_text)
        .collect::<Vec<_>>()
        .join("")
}

/// 解析一个 SSE `data:` 负载。
///
/// 返回该片段产生的增量事件。`[DONE]` 标记由 [`stream_chat`] 处理。
pub fn parse_chunk(data: &str) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(data)
        .map_err(|e| Error::llm_fatal(format!("无法解析流式片段: {e}（原文: {data}）"), None))?;

    let mut out = Vec::new();

    // usage 可能出现在任意 chunk（通常在最后一个）
    let usage = v.get("usage").filter(|u| !u.is_null()).map(|u| Usage {
        input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    });

    let mut stop_reason = None;

    if let Some(choices) = v.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let delta = choice.get("delta");
            if let Some(delta) = delta {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(StreamEvent::Text(text.to_owned()));
                    }
                }
                // 部分兼容端点用 reasoning_content 承载推理
                if let Some(think) = delta.get("reasoning_content").and_then(Value::as_str) {
                    if !think.is_empty() {
                        out.push(StreamEvent::Thinking(think.to_owned()));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (i, c) in calls.iter().enumerate() {
                        let index = c
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize)
                            .unwrap_or(i);
                        let function = c.get("function");
                        out.push(StreamEvent::ToolCallDelta {
                            index,
                            id: c.get("id").and_then(Value::as_str).map(str::to_owned),
                            name: function
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            args_delta: function
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                        });
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                stop_reason = Some(fr.to_owned());
            }
        }
    }

    // 只有真正拿到终止原因或 usage 才算结束
    if stop_reason.is_some() {
        out.push(StreamEvent::Done {
            usage: usage.unwrap_or_default(),
            stop_reason,
        });
    }

    Ok(out)
}

/// 发起流式请求。
pub async fn stream_chat(
    http: &reqwest::Client,
    cfg: &ProviderConfig,
    model: &str,
    req: ChatRequest,
) -> Result<ChatStream> {
    let url = cfg.endpoint("chat/completions");
    let body = build_body(model, &req);
    let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(64);

    // 每次重试都要重新构造请求：builder 和 body 都必须 move 进 async block，
    // 否则闭包返回的 future 会借用局部变量。
    let api_key = cfg.api_key();
    let extra_headers: Vec<(String, String)> = cfg
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // 重试只覆盖"拿到 2xx 之前"的失败。流已经开始了就不能重试 ——
    // 否则会重复输出已经发给用户的 token。
    let resp = retry::with_retry(&url, || {
        let url = url.clone();
        let body = body.clone();
        let api_key = api_key.clone();
        let extra_headers = extra_headers.clone();
        async move {
            let mut builder = http.post(&url).header("content-type", "application/json");
            if let Some(key) = api_key {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }
            for (k, v) in &extra_headers {
                builder = builder.header(k, v);
            }
            let resp = builder
                .json(&body)
                .send()
                .await
                .map_err(|e| Error::llm_retryable(format!("请求 {url} 失败: {e}"), None))?;
            retry::check_status(&url, resp).await
        }
    })
    .await?;

    tokio::spawn(async move {
        let mut decoder = SseDecoder::new();
        let mut stream = resp.bytes_stream();
        let mut finished = false;

        loop {
            // 空闲超时：服务端挂住连接不发数据时，光靠 read_timeout 兜不住
            let next = match tokio::time::timeout(crate::STREAM_IDLE_TIMEOUT, stream.next()).await {
                Ok(v) => v,
                Err(_) => {
                    let _ = tx
                        .send(Err(Error::llm_retryable(
                            format!("服务端 {:?} 没有任何响应", crate::STREAM_IDLE_TIMEOUT),
                            None,
                        )))
                        .await;
                    return;
                }
            };
            let Some(chunk) = next else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx
                        .send(Err(Error::llm_retryable(format!("读流失败: {e}"), None)))
                        .await;
                    return;
                }
            };

            for ev in decoder.push(&chunk) {
                let data = ev.data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    finished = true;
                    // [DONE] 之后可能还有内容，但按协议流已结束
                    break;
                }
                match parse_chunk(data) {
                    Ok(events) => {
                        for e in events {
                            if tx.send(Ok(e)).await.is_err() {
                                return; // 消费端已 drop
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
            if finished {
                break;
            }
        }

        // 流自然结束但没有收到 Done：补一个，避免上层永远等不到结束信号
        if !finished {
            let _ = tx
                .send(Ok(StreamEvent::Done {
                    usage: Usage::default(),
                    stop_reason: None,
                }))
                .await;
        }
    });

    Ok(ChatStream::new(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zex_core::{Risk, ToolCall, ToolCallId, ToolSpec};

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "fs_read",
            description: "读文件",
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            risk: Risk::ReadOnly,
        }
    }

    #[test]
    fn body_puts_system_first() {
        let req = ChatRequest::new(vec![Message::user("hi")]).with_system("you are zex");
        let body = build_body("m", &req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "you are zex");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn body_maps_tools_to_function_schema() {
        let req = ChatRequest::new(vec![Message::user("x")]).with_tools(vec![tool()]);
        let body = build_body("m", &req);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn body_includes_stream_options_for_usage() {
        // 没有 stream_options，兼容端点不返回 usage，成本就无从统计
        let body = build_body("m", &ChatRequest::new(vec![Message::user("x")]));
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn assistant_tool_call_arguments_must_be_string() {
        let msg = Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("call_1"),
                name: "fs_read".into(),
                arguments: json!({"path": "a.txt"}),
            }],
        };
        let wire = message_to_wire(&msg);
        // OpenAI 要求 arguments 是 JSON 字符串而非对象
        let args = &wire["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string(), "arguments 必须是字符串，实际: {args}");
        assert!(wire["content"].is_null());
    }

    #[test]
    fn tool_message_maps_call_id() {
        let msg = Message::Tool {
            call_id: ToolCallId::new("call_1"),
            name: Some("fs_read".into()),
            content: "data".into(),
            is_error: false,
        };
        let wire = message_to_wire(&msg);
        assert_eq!(wire["role"], "tool");
        assert_eq!(wire["tool_call_id"], "call_1");
    }

    #[test]
    fn parse_text_delta() {
        let evs = parse_chunk(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).unwrap();
        assert_eq!(evs, vec![StreamEvent::Text("Hi".into())]);
    }

    #[test]
    fn parse_empty_content_is_skipped() {
        // 很多端点在第一个 chunk 发 content: "" —— 不应产生空 Text 事件
        let evs = parse_chunk(r#"{"choices":[{"delta":{"content":""}}]}"#).unwrap();
        assert!(evs.is_empty());
    }

    #[test]
    fn parse_tool_call_delta() {
        let evs = parse_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"fs_read","arguments":"{\"pa"}}]}}]}"#,
        )
        .unwrap();
        match &evs[0] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("call_1"));
                assert_eq!(name.as_deref(), Some("fs_read"));
                assert_eq!(args_delta, "{\"pa");
            }
            other => panic!("期望 ToolCallDelta，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_tool_call_delta_without_index_uses_position() {
        let evs = parse_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"c","function":{"name":"n","arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        assert!(matches!(
            evs[0],
            StreamEvent::ToolCallDelta { index: 0, .. }
        ));
    }

    #[test]
    fn parse_finish_reason_emits_done() {
        let evs =
            parse_chunk(r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#)
                .unwrap();
        match evs.last().unwrap() {
            StreamEvent::Done { usage, stop_reason } => {
                assert_eq!(usage.input_tokens, 7);
                assert_eq!(usage.output_tokens, 3);
                assert_eq!(stop_reason.as_deref(), Some("stop"));
            }
            other => panic!("期望 Done，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_invalid_json_is_fatal_not_retryable() {
        let e = parse_chunk("{oops").unwrap_err();
        assert!(!e.is_retryable());
    }

    #[test]
    fn parse_reasoning_content() {
        let evs =
            parse_chunk(r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#).unwrap();
        assert_eq!(evs, vec![StreamEvent::Thinking("thinking...".into())]);
    }
}

#[cfg(test)]
mod thinking_body_tests {
    use super::*;
    use crate::{ChatRequest, ThinkingEffort};
    use zex_core::Message;

    fn body(thinking: ThinkingEffort) -> serde_json::Value {
        let mut req = ChatRequest::new(vec![Message::user("hi")]);
        req.thinking = thinking;
        // build_body 是内部函数，这里通过它验证字段确实写进去了
        build_body("m", &req)
    }

    #[test]
    fn high_effort_adds_reasoning_field() {
        let b = body(ThinkingEffort::High);
        assert_eq!(b["reasoning_effort"], "high");
    }

    #[test]
    fn off_omits_reasoning_field() {
        let b = body(ThinkingEffort::Off);
        assert!(
            b.get("reasoning_effort").is_none(),
            "选不思考时不应发该字段"
        );
    }
}
