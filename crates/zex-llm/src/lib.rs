//! Zex 的 LLM 客户端层。
//!
//! 只支持两种线格式（ADR-010）：
//!
//! | 变体 | 端点 | 流式 | 工具调用 |
//! |---|---|---|---|
//! | [`ProviderKind::OpenAiCompat`] | `POST {base}/v1/chat/completions` | SSE | `tool_calls` / `role:"tool"` |
//! | [`ProviderKind::Anthropic`] | `POST {base}/v1/messages` | SSE | `tool_use` / `tool_result` |
//!
//! 不引入 provider 抽象框架：只要两条路，自己写比适配框架便宜，
//! 而且 `base_url` 可配置已足以覆盖 Ollama / vLLM / one-api / OpenRouter 等。
//!
//! 本 crate **不依赖 gpui**（内核/UI 分层，ADR-002）。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod anthropic;
pub mod openai;
pub mod retry;
pub mod sse;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use zex_core::{Error, Message, Result, ToolSpec, Usage};

/// 支持的线格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// OpenAI 兼容格式（覆盖 OpenAI 官方、Ollama、vLLM、one-api、OpenRouter…）。
    OpenAiCompat,
    /// Anthropic Messages API 格式。
    Anthropic,
}

/// 一个 provider 的连接配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// 线格式。
    pub kind: ProviderKind,
    /// 基础 URL，如 `https://api.anthropic.com` 或 `http://127.0.0.1:11434/v1`。
    pub base_url: String,
    /// 环境变量名，密钥从它读取。`None` 表示不需要鉴权（本地 Ollama）。
    ///
    /// **刻意存变量名而不是密钥本身**：配置可被打印、可被提交，
    /// 而密钥永不进入模型上下文（见 `docs/SECURITY.md` §5.3）。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 额外请求头。
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl ProviderConfig {
    /// 解析出实际密钥。
    pub fn api_key(&self) -> Option<String> {
        let var = self.api_key_env.as_deref()?;
        std::env::var(var).ok().filter(|s| !s.is_empty())
    }

    /// 拼接完整请求 URL。
    ///
    /// 容忍 `base_url` 已经带 `/v1`（Ollama 的兼容端点就是这种形态）或带尾斜杠。
    pub fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        // 已经带版本段则不重复添加
        let has_version = base.rsplit('/').next().is_some_and(|seg| {
            seg.len() > 1 && seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit())
        });
        if has_version {
            format!("{base}/{path}")
        } else {
            format!("{base}/v1/{path}")
        }
    }
}

pub use zex_core::ThinkingEffort;

/// 一次对话请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 系统提示。
    pub system: Option<String>,
    /// 对话历史（不含 system）。
    pub messages: Vec<Message>,
    /// 可用工具。
    pub tools: Vec<ToolSpec>,
    /// 输出 token 上限。
    pub max_tokens: u32,
    /// 采样温度。
    pub temperature: Option<f32>,
    /// 思考强度。
    pub thinking: ThinkingEffort,
}

impl ChatRequest {
    /// 构造一个最小请求。
    pub fn new(messages: Vec<Message>) -> Self {
        ChatRequest {
            system: None,
            messages,
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: None,
            thinking: ThinkingEffort::Off,
        }
    }

    /// 设置系统提示。
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// 挂载可用工具。
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }
}

/// 流式过程中的增量事件。
///
/// 刻意与两种协议都解耦：适配层负责把各自的线格式翻译到这里。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 正文增量。
    Text(String),
    /// 推理过程增量。
    Thinking(String),
    /// 工具调用增量。
    ///
    /// 参数是**分片到达的 JSON 字符串**，上层需要按 `index` 拼接后再解析。
    ToolCallDelta {
        /// 该调用在本轮的序号，用于把分片归到同一个调用上。
        index: usize,
        /// provider 给出的调用 id（通常只在第一片出现）。
        id: Option<String>,
        /// 工具名（通常只在第一片出现）。
        name: Option<String>,
        /// 参数 JSON 的分片。
        args_delta: String,
    },
    /// 流正常结束。
    Done {
        /// token 用量。
        usage: Usage,
        /// provider 的 stop_reason，如 `end_turn` / `tool_use` / `stop`。
        stop_reason: Option<String>,
    },
}

/// 一个流式响应。
///
/// 内部是 channel：后台任务读 SSE 并投递事件，前端 `next()` 拉取。
/// Drop 掉 `ChatStream` 会让发送端失败，后台任务自动退出 —— 天然的取消语义。
pub struct ChatStream {
    rx: mpsc::Receiver<Result<StreamEvent>>,
}

impl ChatStream {
    /// 由后台任务构造。
    pub(crate) fn new(rx: mpsc::Receiver<Result<StreamEvent>>) -> Self {
        ChatStream { rx }
    }

    /// 取下一个事件。流结束返回 `None`。
    pub async fn next(&mut self) -> Option<Result<StreamEvent>> {
        self.rx.recv().await
    }

    /// 把流消费完，聚合出完整结果。
    ///
    /// 这是上层（`zex-agent`）最常用的入口：它不关心中间分片。
    pub async fn collect(mut self) -> Result<CollectedResponse> {
        let mut acc = Accumulator::default();
        while let Some(item) = self.next().await {
            acc.push(item?);
        }
        acc.finish()
    }
}

/// 聚合后的完整响应。
#[derive(Debug, Clone, Default)]
pub struct CollectedResponse {
    /// 正文。
    pub text: String,
    /// 推理过程。
    pub thinking: String,
    /// 完整的工具调用。
    pub tool_calls: Vec<zex_core::ToolCall>,
    /// token 用量。
    pub usage: Usage,
    /// stop_reason。
    pub stop_reason: Option<String>,
}

/// 错误消息里回显的原文最多留多少字符。
///
/// 400 个字符足够看出「断在哪个 token 上」：JSON 截断的现场通常就在开头
/// 或者紧邻截断处，两头各留一点即可，不必把整个文件搬进错误消息。
const ARGS_EXCERPT_CHARS: usize = 400;

/// 取一段原文做证据：超长时截断并标注原始长度。
fn excerpt(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…（原文共 {total} 字符，此处只留开头 {limit} 个）")
}

/// 把分片事件聚合成完整响应。
#[derive(Debug, Default)]
pub struct Accumulator {
    text: String,
    thinking: String,
    /// index → (id, name, args_buffer)
    calls: std::collections::BTreeMap<usize, (Option<String>, Option<String>, String)>,
    usage: Usage,
    stop_reason: Option<String>,
}

impl Accumulator {
    /// 喂入一个事件。
    pub fn push(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Text(t) => self.text.push_str(&t),
            StreamEvent::Thinking(t) => self.thinking.push_str(&t),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                args_delta,
            } => {
                let slot = self.calls.entry(index).or_default();
                if id.is_some() {
                    slot.0 = id;
                }
                if name.is_some() {
                    slot.1 = name;
                }
                slot.2.push_str(&args_delta);
            }
            StreamEvent::Done { usage, stop_reason } => {
                self.usage = usage;
                self.stop_reason = stop_reason;
            }
        }
    }

    /// 结束并产出结果。
    ///
    /// 工具参数分片拼接后统一解析；空字符串按 `{}` 处理（无参工具很常见）。
    pub fn finish(self) -> Result<CollectedResponse> {
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for (index, (id, name, args)) in self.calls {
            let name = name.ok_or_else(|| Error::Llm {
                message: format!("工具调用 #{index} 没有名字"),
                status: None,
                retryable: false,
            })?;
            let args = if args.trim().is_empty() {
                "{}".to_string()
            } else {
                args
            };
            let arguments = serde_json::from_str(&args).map_err(|e| Error::Llm {
                message: format!(
                    "工具 `{name}` 的参数不是合法 JSON: {e}（原文开头: {}）。\
                     最常见的原因是输出被 max_tokens 截断：把这次调用拆小\
                     （比如大文件分几次写，每次不超过 150 行）再试",
                    // 原文可能是一整个文件（写大文件时几十上百 KB）。
                    // 整段塞进错误消息有两个后果：界面上刷成一条读不完的长行，
                    // 下一轮还要把这坨载荷再送进上下文。所以只取开头做证据。
                    excerpt(&args, ARGS_EXCERPT_CHARS)
                ),
                status: None,
                retryable: false,
            })?;
            tool_calls.push(zex_core::ToolCall {
                id: zex_core::ToolCallId::new(id.unwrap_or_else(|| format!("call_{index}"))),
                name,
                arguments,
            });
        }
        Ok(CollectedResponse {
            text: self.text,
            thinking: self.thinking,
            tool_calls,
            usage: self.usage,
            stop_reason: self.stop_reason,
        })
    }
}

/// 发起流式对话。
///
/// 按 `cfg.kind` 分发到对应协议实现。
pub async fn stream_chat(
    http: &reqwest::Client,
    cfg: &ProviderConfig,
    model: &str,
    req: ChatRequest,
) -> Result<ChatStream> {
    match cfg.kind {
        ProviderKind::OpenAiCompat => openai::stream_chat(http, cfg, model, req).await,
        ProviderKind::Anthropic => anthropic::stream_chat(http, cfg, model, req).await,
    }
}

/// 流式响应两次数据之间的最长空闲时间。
///
/// `reqwest` 的 `read_timeout` 靠不住：服务端把连接一直挂着、就是不发
/// 数据时它不会触发（实测卡过 7 分钟）。这里在流上再设一道 —— 超过这个
/// 时间没有任何 chunk 到达，就判定这条流死了。
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// 建一个配置好超时的 HTTP 客户端。
///
/// 超时策略：连接 10s、整体不设上限（流式对话可能很久），
/// 但读超时 120s 兜住"连接活着但对端不说话"的情况。
pub fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| Error::Other(format!("无法创建 HTTP 客户端: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: ProviderKind, base: &str) -> ProviderConfig {
        ProviderConfig {
            kind,
            base_url: base.to_string(),
            api_key_env: None,
            headers: Default::default(),
        }
    }

    #[test]
    fn stream_idle_timeout_is_bounded() {
        // 太长会让用户干等，太短会误杀慢速生成长文本的模型
        assert!(STREAM_IDLE_TIMEOUT >= Duration::from_secs(30));
        assert!(STREAM_IDLE_TIMEOUT <= Duration::from_secs(300));
    }

    /// 真机验证 ASR 链路。需要 DEEPSEEK_API_KEY 和 /tmp/silence.wav。
    /// 手动跑：cargo test -p zex-llm asr_end_to_end -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn asr_end_to_end() {
        let cfg = ProviderConfig {
            kind: ProviderKind::OpenAiCompat,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            headers: Default::default(),
        };
        let http = build_http_client().unwrap();
        let wav = std::fs::read("/tmp/silence.wav").expect("先造一个 /tmp/silence.wav");
        let text = transcribe(&http, &cfg, wav).await.unwrap();
        println!("转写结果: {text:?}");
        assert!(text.is_empty(), "静音应该转出空文本");
    }

    #[test]
    fn endpoint_appends_v1_when_absent() {
        let c = cfg(ProviderKind::Anthropic, "https://api.anthropic.com");
        assert_eq!(
            c.endpoint("messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_respects_existing_v1() {
        // Ollama 的 OpenAI 兼容端点就是带 /v1 的
        let c = cfg(ProviderKind::OpenAiCompat, "http://127.0.0.1:11434/v1");
        assert_eq!(
            c.endpoint("chat/completions"),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_handles_trailing_slash() {
        let c = cfg(ProviderKind::OpenAiCompat, "https://api.openai.com/v1/");
        assert_eq!(
            c.endpoint("/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_does_not_mistake_vendor_for_version() {
        // "vllm" 以 v 开头但不是版本号
        let c = cfg(ProviderKind::OpenAiCompat, "http://host/vllm");
        assert_eq!(
            c.endpoint("chat/completions"),
            "http://host/vllm/v1/chat/completions"
        );
    }

    #[test]
    fn accumulator_joins_fragmented_tool_args() {
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fs_read".into()),
            args_delta: "{\"pa".into(),
        });
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            args_delta: "th\": \"a.txt\"}".into(),
        });
        let out = acc.finish().unwrap();
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "fs_read");
        assert_eq!(out.tool_calls[0].arguments["path"], "a.txt");
    }

    #[test]
    fn accumulator_handles_multiple_parallel_tool_calls() {
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("a".into()),
            name: Some("fs_read".into()),
            args_delta: "{}".into(),
        });
        acc.push(StreamEvent::ToolCallDelta {
            index: 1,
            id: Some("b".into()),
            name: Some("shell".into()),
            args_delta: "{\"cmd\":\"ls\"}".into(),
        });
        let out = acc.finish().unwrap();
        assert_eq!(out.tool_calls.len(), 2);
        assert_eq!(out.tool_calls[0].name, "fs_read");
        assert_eq!(out.tool_calls[1].arguments["cmd"], "ls");
    }

    #[test]
    fn broken_tool_args_error_keeps_only_an_excerpt() {
        // 写大文件时参数有一整个文件那么大。错误消息如果整段回显，
        // 界面上会是一条读不完的长行，下一轮还会把它再塞进上下文。
        let big = format!("{{\"text\": \"{}\"", "字".repeat(20_000));
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fs_write".into()),
            args_delta: big,
        });
        let err = acc.finish().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不是合法 JSON"), "{msg}");
        assert!(
            msg.chars().count() < 800,
            "错误消息不该带上整段参数，实际 {} 字符",
            msg.chars().count()
        );
        assert!(msg.contains("原文共 20011 字符"), "应当说明原始长度: {msg}");
    }

    #[test]
    fn accumulator_treats_empty_args_as_empty_object() {
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("x".into()),
            name: Some("noop".into()),
            args_delta: "".into(),
        });
        let out = acc.finish().unwrap();
        assert_eq!(out.tool_calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn accumulator_errors_on_invalid_json() {
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("x".into()),
            name: Some("bad".into()),
            args_delta: "{not json".into(),
        });
        assert!(acc.finish().is_err());
    }

    #[test]
    fn accumulator_collects_text() {
        let mut acc = Accumulator::default();
        acc.push(StreamEvent::Text("He".into()));
        acc.push(StreamEvent::Text("llo".into()));
        let out = acc.finish().unwrap();
        assert_eq!(out.text, "Hello");
    }
}

/// 语音识别（ASR）。
///
/// DeepSeek 专有端点：一次上传完整音频，返回整段转写文本，
/// 不支持流式。音频要求 16 位 PCM WAV 或 Ogg Opus，≤8 MiB、≤120 秒。
///
/// 不是所有 provider 都有这个能力 —— 调用方要么确认 base_url 指向
/// DeepSeek，要么接受 404。
pub async fn transcribe(
    http: &reqwest::Client,
    cfg: &ProviderConfig,
    wav: Vec<u8>,
) -> Result<String> {
    let url = cfg.endpoint("tools/asr");
    let mut form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(wav)
            .file_name("speech.wav")
            .mime_str("audio/wav")
            .map_err(|e| Error::Other(format!("构造上传体失败: {e}")))?,
    );

    let mut req = http.post(&url);
    if let Some(key) = cfg.api_key() {
        req = req.bearer_auth(key);
    }
    // 上传 + 识别可能比一次对话慢，但也不该无限等
    req = req.timeout(Duration::from_secs(90));

    let resp = req
        .multipart(std::mem::take(&mut form))
        .send()
        .await
        .map_err(|e| Error::Other(format!("语音识别请求失败: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Other(format!("读取语音识别响应失败: {e}")))?;

    if !status.is_success() {
        return Err(Error::Other(format!(
            "语音识别失败（HTTP {}）：{}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        )));
    }

    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::Other(format!("解析语音识别响应失败: {e}：{body}")))?;
    Ok(v.get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .trim()
        .to_string())
}

#[cfg(test)]
mod thinking_tests {
    use super::ThinkingEffort;

    #[test]
    fn off_sends_nothing() {
        // 选"不思考"时不该往请求里塞任何思考相关字段：
        // 不支持思考的模型收到 reasoning_effort 会直接报错
        assert_eq!(ThinkingEffort::Off.openai_effort(), None);
        assert_eq!(ThinkingEffort::Off.anthropic_budget(), None);
    }

    #[test]
    fn openai_effort_levels() {
        assert_eq!(ThinkingEffort::Low.openai_effort(), Some("low"));
        assert_eq!(ThinkingEffort::Medium.openai_effort(), Some("medium"));
        assert_eq!(ThinkingEffort::High.openai_effort(), Some("high"));
    }

    #[test]
    fn anthropic_budget_meets_minimum() {
        // 官方下限是 1024，低档也必须是这个数，
        // 给更低会被服务端拒掉
        assert_eq!(ThinkingEffort::Low.anthropic_budget(), Some(1024));
        assert!(ThinkingEffort::Medium.anthropic_budget().unwrap() > 1024);
        assert!(ThinkingEffort::High.anthropic_budget().unwrap()
            > ThinkingEffort::Medium.anthropic_budget().unwrap());
    }

    #[test]
    fn labels_are_distinct() {
        let all = ThinkingEffort::all();
        let mut names: Vec<&str> = all.iter().map(|t| t.label()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len(), "档位名不能重复");
    }
}
