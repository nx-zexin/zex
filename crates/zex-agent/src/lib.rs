//! Zex 的 agent 内核。
//!
//! 刻意做薄：一个 while 循环 + 一次 LLM 调用 + 工具分发。**没有状态机。**
//! 不实现 planner / critic / orchestrator / workflow —— 见 ADR-006。
//!
//! 本 crate **不依赖 gpui**（内核/UI 分层，ADR-002）。它只认识
//! [`zex_core::CoreEvent`]，UI 与 CLI 各自消费。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod context;
pub mod prompt;
pub mod toolctx;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use zex_core::tool::ErasedTool;
use zex_core::{
    CoreEvent, EndReason, Error, Message, Result, SessionId, ToolCallId, TurnId, Usage,
};
use zex_llm::{ChatRequest, ProviderConfig, StreamEvent};

use crate::context::ContextBudget;
use crate::toolctx::AgentCtx;

/// 内核配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// provider 连接配置。
    pub provider: ProviderConfig,
    /// 模型名。
    pub model: String,
    /// 单轮内最多允许的"模型调用"次数（防止工具循环失控）。
    pub max_steps: u32,
    /// 单次输出 token 上限。
    pub max_tokens: u32,
    /// 工作目录。
    pub cwd: PathBuf,
    /// 上下文预算。
    pub budget: ContextBudget,
}

impl AgentConfig {
    /// 用最小必填项构造。
    pub fn new(provider: ProviderConfig, model: impl Into<String>, cwd: PathBuf) -> Self {
        AgentConfig {
            provider,
            model: model.into(),
            max_steps: 25,
            max_tokens: 8192,
            cwd,
            budget: ContextBudget::default(),
        }
    }
}

/// agent 内核。
pub struct Agent {
    cfg: AgentConfig,
    http: reqwest::Client,
    tools: Vec<Arc<dyn ErasedTool>>,
    history: Vec<Message>,
    session: SessionId,
    /// 累计 token 用量（整个会话）。
    usage: Usage,
}

impl Agent {
    /// 新建 agent。
    pub fn new(cfg: AgentConfig, tools: Vec<Arc<dyn ErasedTool>>) -> Result<Self> {
        let http = zex_llm::build_http_client()?;
        Ok(Agent {
            cfg,
            http,
            tools,
            history: Vec::new(),
            session: SessionId::generate(),
            usage: Usage::default(),
        })
    }

    /// 当前会话 id。
    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    /// 会话累计用量。
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// 历史消息（只读）。
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// 清空上下文（保留会话 id）。
    pub fn clear(&mut self) {
        self.history.clear();
    }

    /// 执行一轮：接收用户输入，跑完整 loop，返回最终文本。
    ///
    /// `events` 用于把过程推给 UI/CLI。传 `None` 表示不需要过程事件
    /// （测试与非交互场景）。
    pub async fn run_turn(
        &mut self,
        input: impl Into<String>,
        events: Option<&mpsc::Sender<CoreEvent>>,
    ) -> Result<String> {
        let turn = TurnId::generate();
        self.checkpoint(
            CoreEvent::TurnStarted {
                session: self.session.clone(),
                turn: turn.clone(),
            },
            events,
        );

        self.history.push(Message::user(input));

        let mut final_text = String::new();
        let mut end_reason = EndReason::Completed;
        let mut step = 0u32;

        loop {
            if step >= self.cfg.max_steps {
                end_reason = EndReason::MaxSteps {
                    limit: self.cfg.max_steps,
                };
                break;
            }
            step += 1;

            let req = self.build_request();
            let (text, thinking, tool_calls, usage) =
                match self.stream_once(req, &turn, events).await {
                    Ok(v) => v,
                    Err(e) => {
                        // 把错误也作为一轮结束上报，UI 才能把 loading 收掉
                        self.checkpoint(
                            CoreEvent::TurnEnded {
                                turn: turn.clone(),
                                reason: EndReason::Failed {
                                    message: e.to_string(),
                                },
                                usage: self.usage,
                            },
                            events,
                        );
                        return Err(e);
                    }
                };

            self.usage.add(usage);

            // 记录 assistant 消息（含工具调用），这是后续 tool_result 配对的依据
            self.history.push(Message::Assistant {
                content: if thinking.is_empty() {
                    vec![zex_core::Part::text(text.clone())]
                } else {
                    vec![
                        zex_core::Part::Thinking {
                            text: thinking.clone(),
                        },
                        zex_core::Part::text(text.clone()),
                    ]
                },
                tool_calls: tool_calls.clone(),
            });

            if tool_calls.is_empty() {
                final_text = text;
                break;
            }

            // 串行执行工具。并行留到 M2 —— 串行的可观测性好得多，
            // 而且审批必须逐个过，并行会让审批顺序混乱。
            for call in &tool_calls {
                self.checkpoint(
                    CoreEvent::ToolCallBegin {
                        turn: turn.clone(),
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        risk: self.risk_of(&call.name),
                    },
                    events,
                );

                let ctx = AgentCtx::new(self.cfg.cwd.clone(), self.session.clone());
                let (content, is_error) = self.dispatch(&ctx, call).await;

                self.checkpoint(
                    CoreEvent::ToolCallEnd {
                        turn: turn.clone(),
                        call_id: call.id.clone(),
                        ok: !is_error,
                        brief: brief(&content),
                    },
                    events,
                );

                self.history.push(Message::Tool {
                    call_id: call.id.clone(),
                    name: Some(call.name.clone()),
                    content,
                    is_error,
                });
            }
        }

        self.checkpoint(
            CoreEvent::TurnEnded {
                turn,
                reason: end_reason,
                usage: self.usage,
            },
            events,
        );
        Ok(final_text)
    }

    /// 组装一次请求。上下文压缩在 M1 接入，这里先只做系统提示。
    fn build_request(&self) -> ChatRequest {
        let system = prompt::system_prompt(&self.cfg.cwd);
        let req = ChatRequest::new(self.history.clone())
            .with_system(system)
            .with_tools(self.tools.iter().map(|t| t.spec()).collect());
        ChatRequest {
            max_tokens: self.cfg.max_tokens,
            ..req
        }
    }

    /// 发起一次流式请求，把增量推成事件，返回聚合结果。
    async fn stream_once(
        &self,
        req: ChatRequest,
        turn: &TurnId,
        events: Option<&mpsc::Sender<CoreEvent>>,
    ) -> Result<(String, String, Vec<zex_core::ToolCall>, Usage)> {
        let mut stream =
            zex_llm::stream_chat(&self.http, &self.cfg.provider, &self.cfg.model, req).await?;

        let mut acc = zex_llm::Accumulator::default();
        while let Some(item) = stream.next().await {
            let ev = item?;
            // 先推事件再聚合：用户应该看到 token 落在屏幕上，
            // 而不是等整段结束才看到（流式的全部意义在这里）。
            match &ev {
                StreamEvent::Text(t) => self.checkpoint(
                    CoreEvent::Token {
                        turn: turn.clone(),
                        text: t.clone(),
                    },
                    events,
                ),
                StreamEvent::Thinking(t) => self.checkpoint(
                    CoreEvent::Thinking {
                        turn: turn.clone(),
                        text: t.clone(),
                    },
                    events,
                ),
                _ => {}
            }
            acc.push(ev);
        }

        let out = acc.finish()?;
        Ok((out.text, out.thinking, out.tool_calls, out.usage))
    }

    /// 查一个工具的风险等级（未知工具按 L2 处理 —— 保守）。
    fn risk_of(&self, name: &str) -> zex_core::Risk {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.spec().risk)
            .unwrap_or(zex_core::Risk::Impactful)
    }

    /// 分发一次工具调用。
    ///
    /// M0 阶段**不做权限闸门**（按 M0-PLAN 的约定），所有调用直通。
    /// M1 在此处插入 `zex_policy::gate`：Allow 则执行、Ask 则挂起等审批、
    /// Deny 则返回拒绝原因给模型。这个函数的签名已经为此留好位（返回 `(content, is_error)`）。
    async fn dispatch(&self, ctx: &AgentCtx, call: &zex_core::ToolCall) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|t| t.spec().name == call.name) else {
            return (
                format!("没有名为 `{}` 的工具，请从可用工具中选择。", call.name),
                true,
            );
        };
        match tool.call_json(ctx, call.arguments.clone()).await {
            Ok(out) => (out.content, out.is_error),
            Err(Error::BadArgs { tool, message }) => {
                // 参数错误要让模型看见并自行纠正，属于"可恢复失败"
                (
                    format!("调用 `{tool}` 的参数不合法：{message}。请检查参数后重试。"),
                    true,
                )
            }
            Err(e) => (format!("工具 `{}` 执行失败：{e}", call.name), true),
        }
    }

    /// 事件投递。接收端已关闭时静默忽略 —— UI 关掉不代表内核必须崩。
    fn checkpoint(&self, ev: CoreEvent, events: Option<&mpsc::Sender<CoreEvent>>) {
        if let Some(tx) = events {
            if tx.try_send(ev).is_err() {
                // 队列满或已关闭：丢弃事件而不是阻塞内核。
                // UI 跟不上时，正确行为是掉帧，不是卡死 agent。
            }
        }
    }
}

/// 工具结果的一句话摘要，供 UI 展示（不塞全文，避免 UI 卡顿）。
fn brief(content: &str) -> String {
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let s: String = first.chars().take(120).collect();
    // 只有真正的多行内容才加省略号：开头的空行不构成"还有更多"
    let non_blank = content.lines().filter(|l| !l.trim().is_empty()).count();
    if non_blank > 1 || first.chars().count() > 120 {
        format!("{s}…")
    } else {
        s
    }
}

/// 便捷构造：工具调用 id 的兼容处理。
///
/// 部分 provider 会给出空 id，这里补一个稳定的兜底。
pub fn ensure_call_id(id: &ToolCallId, index: usize) -> ToolCallId {
    if id.as_str().is_empty() {
        ToolCallId::new(format!("call_{index}"))
    } else {
        id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_truncates_long_first_line() {
        let long = "x".repeat(200);
        let b = brief(&long);
        assert!(b.ends_with('…'));
        assert!(b.chars().count() <= 121);
    }

    #[test]
    fn brief_marks_multiline() {
        assert!(brief("line1\nline2").ends_with('…'));
    }

    #[test]
    fn brief_leaves_short_single_line() {
        assert_eq!(brief("done"), "done");
    }

    #[test]
    fn brief_skips_leading_blank_lines() {
        assert_eq!(brief("\n\nactual"), "actual");
    }
}
