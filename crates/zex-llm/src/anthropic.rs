//! Anthropic Messages API 协议的适配。
//!
//! 与 OpenAI 格式的三处关键差异：
//!
//! 1. `system` 是顶层字段，不在 `messages` 里。
//! 2. 工具结果（`tool_result`）必须放在 `user` 消息的 content block 里，
//!    且连续的多个结果应合并进同一条 user 消息。
//! 3. 流式事件的 `content_block_delta` 只带 `index`，`id`/`name` 只在
//!    `content_block_start` 出现 —— 由 [`crate::Accumulator`] 按 index 合并。

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use zex_core::{Error, Message, Part, Result, Usage};

use crate::sse::SseDecoder;
use crate::{ChatRequest, ChatStream, ProviderConfig, StreamEvent, retry};

/// Anthropic 要求的 API 版本头。
pub const API_VERSION: &str = "2023-06-01";

/// 构造 `POST /v1/messages` 的请求体。
pub fn build_body(model: &str, req: &ChatRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    // 待合并的 tool_result block：连续的 Tool 消息要收进同一条 user 消息
    let mut pending_results: Vec<Value> = Vec::new();

    let flush = |messages: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            messages.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    };

    for m in &req.messages {
        match m {
            Message::System { .. } => {
                // system 走顶层字段，由 ChatRequest.system 承担；此处忽略避免重复
            }
            Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id.as_str(),
                    "content": content,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                pending_results.push(block);
            }
            Message::User { content } => {
                flush(&mut messages, &mut pending_results);
                messages.push(json!({"role": "user", "content": blocks(content)}));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                flush(&mut messages, &mut pending_results);
                let mut blocks = blocks(content);
                for call in tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id.as_str(),
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
        }
    }
    flush(&mut messages, &mut pending_results);

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": req.max_tokens,
        "stream": true,
    });

    tracing::debug!(
        model,
        max_tokens = req.max_tokens,
        messages = req.messages.len(),
        thinking = ?req.thinking,
        "发起对话请求"
    );
    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }
    // 思考与温度互斥：官方要求开了 extended thinking 就不能指定 temperature，
    // 两者同时发会被拒。用户既然选了思考强度，温度让位。
    if let Some(budget) = req.thinking.anthropic_budget() {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        // 预算必须小于 max_tokens，否则整套请求被拒 —— 顶高一点留出输出余量
        let need = budget + 1024;
        if req.max_tokens < need {
            body["max_tokens"] = json!(need);
        }
    } else if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }

    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        // 注意字段名是 input_schema，不是 parameters
                        "input_schema": t.input_schema,
                    })
                })
                .collect(),
        );
    }

    body
}

/// 把内容片段转成 Anthropic 的 content block 数组。
fn blocks(parts: &[Part]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text { text } => Some(json!({"type": "text", "text": text})),
            Part::Image(img) => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": img.blob_id,
                }
            })),
            // 推理过程不回传
            Part::Thinking { .. } => None,
        })
        .collect()
}

/// 解析一个 SSE 事件的数据负载。
///
/// `event` 是 `event:` 行的值（Anthropic 同时放在 data 的 `type` 字段里，
/// 这里以 data 的 `type` 为准，`event` 仅作兜底）。
pub fn parse_chunk(data: &str) -> Result<Vec<StreamEvent>> {
    let v: Value = serde_json::from_str(data)
        .map_err(|e| Error::llm_fatal(format!("无法解析流式片段: {e}（原文: {data}）"), None))?;

    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut out = Vec::new();

    match kind {
        "content_block_start" => {
            let cb = v.get("content_block");
            // 工具调用的起点：这里才拿到 id 与 name。
            // 产出一个 args 为空的 delta，让 Accumulator 建立 index→调用 的映射。
            if let Some(cb) = cb {
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    out.push(StreamEvent::ToolCallDelta {
                        index,
                        id: cb.get("id").and_then(Value::as_str).map(str::to_owned),
                        name: cb.get("name").and_then(Value::as_str).map(str::to_owned),
                        args_delta: String::new(),
                    });
                } else if let Some(text) = cb.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        out.push(StreamEvent::Text(text.to_owned()));
                    }
                }
            }
        }
        "content_block_delta" => {
            let Some(delta) = v.get("delta") else {
                return Ok(out);
            };
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            out.push(StreamEvent::Text(text.to_owned()));
                        }
                    }
                }
                Some("thinking_delta") => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        if !text.is_empty() {
                            out.push(StreamEvent::Thinking(text.to_owned()));
                        }
                    }
                }
                Some("input_json_delta") => {
                    if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                        out.push(StreamEvent::ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            args_delta: partial.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            let stop_reason = v
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str);
            let usage = v.get("usage").map(|u| Usage {
                input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                cached_input_tokens: u
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
            if let Some(usage) = usage {
                out.push(StreamEvent::Done {
                    usage,
                    stop_reason: stop_reason.map(str::to_owned),
                });
            }
        }
        "message_start" => {
            // 输入 token 在 message_start 里给出，先记下来
            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                let usage = Usage {
                    input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                    cached_input_tokens: u
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                };
                // 用一个不含 stop_reason 的 Done 会误导上层，所以先不产出事件；
                // 输入 token 由随后的 message_delta 一并结算（Anthropic 的
                // message_delta usage 里只给 output_tokens，因此这里暂存到日志）。
                tracing::debug!(input_tokens = usage.input_tokens, "Anthropic 输入 token");
            }
        }
        "message_stop" => { /* 结束由 message_delta 的 Done 表达 */ }
        "error" => {
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(Error::llm_fatal(format!("Anthropic 流内错误: {msg}"), None));
        }
        "ping" => {}
        _ => {}
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
    let url = cfg.endpoint("messages");
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

    let resp = retry::with_retry(&url, || {
        let url = url.clone();
        let body = body.clone();
        let api_key = api_key.clone();
        let extra_headers = extra_headers.clone();
        async move {
            let mut builder = http
                .post(&url)
                .header("content-type", "application/json")
                .header("anthropic-version", API_VERSION);
            if let Some(key) = api_key {
                builder = builder.header("x-api-key", key);
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
        let mut saw_done = false;

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
                if ev.data.trim().is_empty() {
                    continue;
                }
                match parse_chunk(&ev.data) {
                    Ok(events) => {
                        for e in events {
                            if matches!(e, StreamEvent::Done { .. }) {
                                saw_done = true;
                            }
                            if tx.send(Ok(e)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        }

        if !saw_done {
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
            input_schema: json!({"type": "object"}),
            risk: Risk::ReadOnly,
        }
    }

    #[test]
    fn system_is_top_level_not_in_messages() {
        let req = ChatRequest::new(vec![Message::user("hi")]).with_system("be zex");
        let body = build_body("m", &req);
        assert_eq!(body["system"], "be zex");
        for m in body["messages"].as_array().unwrap() {
            assert_ne!(m["role"], "system", "system 不应出现在 messages 里");
        }
    }

    #[test]
    fn tools_use_input_schema_not_parameters() {
        let req = ChatRequest::new(vec![Message::user("x")]).with_tools(vec![tool()]);
        let body = build_body("m", &req);
        assert!(body["tools"][0].get("input_schema").is_some());
        assert!(body["tools"][0].get("parameters").is_none());
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let req = ChatRequest::new(vec![
            Message::user("do two things"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![
                    ToolCall {
                        id: ToolCallId::new("a"),
                        name: "fs_read".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: ToolCallId::new("b"),
                        name: "shell".into(),
                        arguments: json!({}),
                    },
                ],
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: None,
                content: "r1".into(),
                is_error: false,
            },
            Message::Tool {
                call_id: ToolCallId::new("b"),
                name: None,
                content: "r2".into(),
                is_error: false,
            },
        ]);
        let body = build_body("m", &req);
        let msgs = body["messages"].as_array().unwrap();
        // user, assistant, user(含两个 tool_result)
        assert_eq!(
            msgs.len(),
            3,
            "两个 tool_result 应合并成一条 user 消息: {msgs:#?}"
        );
        assert_eq!(msgs[2]["role"], "user");
        let blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn error_tool_result_is_flagged() {
        let req = ChatRequest::new(vec![Message::Tool {
            call_id: ToolCallId::new("a"),
            name: None,
            content: "boom".into(),
            is_error: true,
        }]);
        let body = build_body("m", &req);
        assert_eq!(body["messages"][0]["content"][0]["is_error"], true);
    }

    #[test]
    fn assistant_tool_use_is_content_block() {
        let req = ChatRequest::new(vec![Message::Assistant {
            content: vec![Part::text("checking")],
            tool_calls: vec![ToolCall {
                id: ToolCallId::new("t1"),
                name: "fs_read".into(),
                arguments: json!({"path": "a"}),
            }],
        }]);
        let body = build_body("m", &req);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["input"]["path"], "a");
    }

    #[test]
    fn parse_text_delta() {
        let evs = parse_chunk(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        )
        .unwrap();
        assert_eq!(evs, vec![StreamEvent::Text("Hi".into())]);
    }

    #[test]
    fn parse_thinking_delta() {
        let evs = parse_chunk(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )
        .unwrap();
        assert_eq!(evs, vec![StreamEvent::Thinking("hmm".into())]);
    }

    #[test]
    fn parse_tool_use_start_carries_id_and_name() {
        let evs = parse_chunk(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"fs_read","input":{}}}"#,
        )
        .unwrap();
        match &evs[0] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                assert_eq!(*index, 1);
                assert_eq!(id.as_deref(), Some("toolu_1"));
                assert_eq!(name.as_deref(), Some("fs_read"));
                assert!(args_delta.is_empty());
            }
            other => panic!("期望 ToolCallDelta，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_input_json_delta() {
        let evs = parse_chunk(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        )
        .unwrap();
        match &evs[0] {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                assert_eq!(*index, 1);
                assert!(id.is_none());
                assert!(name.is_none());
                assert_eq!(args_delta, "{\"pa");
            }
            other => panic!("期望 ToolCallDelta，得到 {other:?}"),
        }
    }

    #[test]
    fn tool_call_fragments_accumulate_end_to_end() {
        use crate::Accumulator;
        let mut acc = Accumulator::default();
        for data in [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"fs_read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}"#,
        ] {
            for ev in parse_chunk(data).unwrap() {
                acc.push(ev);
            }
        }
        let out = acc.finish().unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].id.as_str(), "t1");
        assert_eq!(out.tool_calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn parse_message_delta_emits_done_with_stop_reason() {
        let evs = parse_chunk(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        )
        .unwrap();
        match evs.last().unwrap() {
            StreamEvent::Done { usage, stop_reason } => {
                assert_eq!(usage.output_tokens, 9);
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
            }
            other => panic!("期望 Done，得到 {other:?}"),
        }
    }

    #[test]
    fn parse_error_event_is_fatal() {
        let e = parse_chunk(r#"{"type":"error","error":{"message":"overloaded"}}"#).unwrap_err();
        assert!(!e.is_retryable());
        assert!(e.to_string().contains("overloaded"));
    }

    #[test]
    fn parse_ping_produces_nothing() {
        assert!(parse_chunk(r#"{"type":"ping"}"#).unwrap().is_empty());
    }
}
