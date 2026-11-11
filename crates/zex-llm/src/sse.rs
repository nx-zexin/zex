//! Server-Sent Events 增量解码器。
//!
//! 这是整个 LLM 层最容易出 bug 的地方，因此单独成模块并**完全脱离网络**测试。
//!
//! 必须处理的现实情况（每一条都有对应测试）：
//!
//! - 一个 TCP chunk 里包含多个事件
//! - 一个事件被 TCP 切成多个 chunk
//! - 行结束符是 `\n`、`\r\n` 或 `\r`
//! - `:` 开头的注释行（keep-alive ping），必须忽略而不是当成数据
//! - `data:` 后有 0 或 1 个空格
//! - 多行 `data:` 需要按规范用 `\n` 拼接
//! - **一个 UTF-8 多字节字符被 chunk 边界切开**（中文场景必现）
//! - 流在没有空行的情况下结束（残留事件）
//!
//! 实现要点：buffer 存 `Vec<u8>` 而不是 `String`，否则 UTF-8 分片会直接 panic。

/// 一个完整的 SSE 事件。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// `event:` 字段。未出现时为 `None`。
    pub event: Option<String>,
    /// `data:` 字段拼接后的内容（多行用 `\n` 连接）。
    pub data: String,
    /// `id:` 字段。
    pub id: Option<String>,
}

/// 增量 SSE 解码器。
///
/// 用法：每次收到网络 chunk 就 `push`，拿到已完整的事件列表。
/// 流结束时调用 [`SseDecoder::finish`] 取出可能残留的最后一个事件。
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// 尚未构成完整行的字节。必须是字节而非字符串。
    buf: Vec<u8>,
    /// 正在累积的事件。
    event_type: Option<String>,
    data: Option<String>,
    id: Option<String>,
}

impl SseDecoder {
    /// 新建解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回其中已经完整的事件。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();

        // 反复取出完整行。行结束符可能是 \n 或 \r\n 或单独的 \r。
        while let Some((line_end, next_start)) = find_line_end(&self.buf) {
            // 到这一步，buf[..line_end] 一定是合法 UTF-8 边界（\n / \r 都是 ASCII）
            let line = String::from_utf8_lossy(&self.buf[..line_end]).into_owned();
            self.buf.drain(..next_start);

            if let Some(ev) = self.consume_line(&line) {
                out.push(ev);
            }
        }

        out
    }

    /// 流结束：把残留内容当作最后一个事件（若其为合法事件）。
    pub fn finish(mut self) -> Option<SseEvent> {
        // 残留的不完整行：SSE 规范允许最后一个事件没有结尾空行
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Some(ev) = self.consume_line(&line) {
                return Some(ev);
            }
        }
        self.take_event()
    }

    /// 处理一行。若这一行是事件终止符，返回组装好的事件。
    fn consume_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            // 空行 = 派发事件
            return self.take_event();
        }
        // 注释行（keep-alive）。SSE 规范里以 ':' 开头的行是注释，不产生任何数据。
        if line.starts_with(':') {
            return None;
        }

        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // 无冒号的行：字段名是整行，值为空
            None => (line, ""),
        };

        match field {
            "event" => self.event_type = Some(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            // `retry` 我们不用（自己控制退避），忽略
            "data" => match &mut self.data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
        None
    }

    /// 取出当前累积的事件。没有 `data` 则视为空事件（规范：只有 `data` 才派发）。
    fn take_event(&mut self) -> Option<SseEvent> {
        let data = self.data.take()?;
        let ev = SseEvent {
            event: self.event_type.take(),
            data,
            id: self.id.take(),
        };
        Some(ev)
    }
}

/// 在 buffer 中寻找第一个行结束符，返回 (行内容结束位置, 下一行起始位置)。
fn find_line_end(buf: &[u8]) -> Option<(usize, usize)> {
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            return Some((i, i + 1));
        }
        if b == b'\r' {
            // \r\n 整体算一个结束符；若 \r 是最后一个字节，可能 \n 还在下一个 chunk，
            // 此时先按 \r 处理（SSE 允许 CR 单独作为行结束符）。
            let next = if buf.get(i + 1) == Some(&b'\n') {
                i + 2
            } else {
                i + 1
            };
            return Some((i, next));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_owned),
            data: data.to_owned(),
            id: None,
        }
    }

    #[test]
    fn single_event() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: hello\n\n");
        assert_eq!(out, vec![ev(None, "hello")]);
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: a\n\ndata: b\n\n");
        assert_eq!(out, vec![ev(None, "a"), ev(None, "b")]);
    }

    #[test]
    fn event_split_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: he").is_empty());
        assert!(d.push(b"ll").is_empty());
        let out = d.push(b"o\n\n");
        assert_eq!(out, vec![ev(None, "hello")]);
    }

    #[test]
    fn split_exactly_at_newline_boundary() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: hi\n").is_empty()); // 有行结束但没空行
        let out = d.push(b"\n");
        assert_eq!(out, vec![ev(None, "hi")]);
    }

    #[test]
    fn crlf_line_endings() {
        let mut d = SseDecoder::new();
        let out = d.push(b"event: delta\r\ndata: x\r\n\r\n");
        assert_eq!(out, vec![ev(Some("delta"), "x")]);
    }

    #[test]
    fn cr_only_line_endings() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: x\r\r");
        assert_eq!(out, vec![ev(None, "x")]);
    }

    #[test]
    fn comment_lines_are_ignored() {
        let mut d = SseDecoder::new();
        // keep-alive ping 不应被当成数据，也不应终止事件
        let out = d.push(b": ping\n\ndata: real\n\n");
        assert_eq!(out, vec![ev(None, "real")]);
    }

    #[test]
    fn comment_inside_event_does_not_dispatch() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: a\n: ping\n").is_empty());
        let out = d.push(b"\n");
        assert_eq!(out, vec![ev(None, "a")]);
    }

    #[test]
    fn data_without_space_after_colon() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data:nospace\n\n");
        assert_eq!(out, vec![ev(None, "nospace")]);
    }

    #[test]
    fn data_with_leading_space_is_trimmed_once() {
        let mut d = SseDecoder::new();
        // 只去掉一个空格，其余是数据的一部分
        let out = d.push(b"data:  two spaces\n\n");
        assert_eq!(out, vec![ev(None, " two spaces")]);
    }

    #[test]
    fn multiline_data_is_joined_with_newline() {
        let mut d = SseDecoder::new();
        let out = d.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(out, vec![ev(None, "line1\nline2")]);
    }

    #[test]
    fn event_without_data_is_not_dispatched() {
        let mut d = SseDecoder::new();
        // 只有 event 字段没有 data：按规范不派发
        assert!(d.push(b"event: ping\n\n").is_empty());
    }

    #[test]
    fn utf8_split_across_chunks_does_not_panic() {
        // 中文"你"= E4 BD A0，故意在中间切开
        let mut d = SseDecoder::new();
        let full = "data: 你好\n\n".as_bytes();
        let cut = 8; // "data: " + 首个字节
        assert!(d.push(&full[..cut]).is_empty());
        // 此时 buf 里是半截 UTF-8，绝不能在这里做 from_utf8().unwrap()
        assert!(d.push(&full[cut..cut + 1]).is_empty());
        let out = d.push(&full[cut + 1..]);
        assert_eq!(out, vec![ev(None, "你好")]);
    }

    #[test]
    fn finish_flushes_trailing_event_without_blank_line() {
        let mut d = SseDecoder::new();
        assert!(d.push(b"data: tail\n").is_empty());
        let ev = d.finish();
        assert_eq!(ev, Some(ev_none("tail")));
    }

    #[test]
    fn finish_on_empty_returns_none() {
        let d = SseDecoder::new();
        assert_eq!(d.finish(), None);
    }

    fn ev_none(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_owned(),
            id: None,
        }
    }

    #[test]
    fn id_field_is_captured() {
        let mut d = SseDecoder::new();
        let out = d.push(b"id: 42\ndata: x\n\n");
        assert_eq!(out[0].id.as_deref(), Some("42"));
    }

    #[test]
    fn real_openai_sse_shape() {
        // 贴近真实的 OpenAI 流式片段
        let mut d = SseDecoder::new();
        let payload = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let out = d.push(payload.as_bytes());
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].data,
            "{\"choices\":[{\"delta\":{\"content\":\"He\"}}]}"
        );
        assert_eq!(out[2].data, "[DONE]");
    }

    #[test]
    fn real_anthropic_sse_shape() {
        let mut d = SseDecoder::new();
        let payload = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\"}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hi\"}}\n\n"
        );
        let out = d.push(payload.as_bytes());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some("message_start"));
        assert_eq!(out[1].event.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn byte_by_byte_feeding_is_stable() {
        // 最严苛的测试：一个字节一个字节喂
        let payload = "event: delta\r\ndata: 测试\r\n\r\ndata: tail\n\n";
        let mut d = SseDecoder::new();
        let mut all = Vec::new();
        for b in payload.as_bytes() {
            all.extend(d.push(&[*b]));
        }
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].data, "测试");
        assert_eq!(all[1].data, "tail");
    }
}
