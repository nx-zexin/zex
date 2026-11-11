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

pub mod approval;
pub mod compress;
pub mod context;
pub mod prompt;
pub mod toolctx;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use zex_core::tool::ErasedTool;
use zex_core::{
    ApprovalId, CoreEvent, EndReason, Error, Message, Result, Risk, SessionId, ToolCallId, TurnId,
    Usage,
};
use zex_llm::{ChatRequest, ProviderConfig, StreamEvent};
use zex_policy::{ApprovedBy, Mode, Policy, Verdict};
use zex_store::Store;
use zex_store::audit::{AuditEntry, digest_args};

pub use approval::{ApprovalBroker, ApprovalDecision};
use compress::{CompressionPlan, Compressor};
use context::ContextBudget;
use toolctx::AgentCtx;

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
    /// 运行模式。
    pub mode: Mode,
    /// 审批等待时长。
    pub approval_timeout: Duration,
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
            mode: Mode::Normal,
            approval_timeout: approval::DEFAULT_TIMEOUT,
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
    store: Store,
    policy: Policy,
    approvals: Arc<ApprovalBroker>,
    compressor: Compressor,
    /// 累计 token 用量（整个会话）。只用于成本统计，**不用于压缩判断**。
    usage: Usage,
    /// 用户中断标志。
    ///
    /// 用 AtomicBool 而不是 channel：`run_turn` 的检查点是同步的，
    /// 而且取消要在 `await` 之间也能看到 —— channel 得额外轮询。
    cancelled: Arc<AtomicBool>,
}

impl Agent {
    /// 装配一个 agent，**不创建会话**。
    ///
    /// `new` / `resume` 都走这里。分开的原因是 `resume` 曾经复用了
    /// `new`，而 `new` 会往库里插一条空会话 —— 结果每启动一次就多出
    /// 一个空壳，侧栏里堆满「会话 xxxxx」。
    fn with_session(
        cfg: AgentConfig,
        tools: Vec<Arc<dyn ErasedTool>>,
        store: Store,
        session: SessionId,
        history: Vec<Message>,
    ) -> Result<Self> {
        let http = zex_llm::build_http_client()?;
        let policy = Policy::new(store.clone());
        let compressor = Compressor::new(cfg.budget.clone());
        Ok(Agent {
            cfg,
            http,
            tools,
            history,
            session,
            store,
            policy,
            approvals: Arc::new(ApprovalBroker::new()),
            compressor,
            usage: Usage::default(),
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 新建 agent（带持久化与权限闸门）。
    pub fn new(cfg: AgentConfig, tools: Vec<Arc<dyn ErasedTool>>, store: Store) -> Result<Self> {
        let session = store.create_session("", &cfg.model, &cfg.cwd.display().to_string())?;
        Self::with_session(cfg, tools, store, session.id, Vec::new())
    }

    /// 恢复一个已有会话。
    pub fn resume(
        cfg: AgentConfig,
        tools: Vec<Arc<dyn ErasedTool>>,
        store: Store,
        session: SessionId,
    ) -> Result<Self> {
        let stored = store.load_messages(&session)?;
        let history: Vec<Message> = stored
            .into_iter()
            .map(|s| {
                let mut m = s.message;
                // 旧数据里可能留着防护上线前的明文凭据
                zex_policy::secret::redact_message(&mut m);
                m
            })
            .collect();
        let mut agent = Self::with_session(cfg, tools, store, session.clone(), history)?;
        // 历史消息里存了各自的 token 数，恢复后带上，界面才不会一片 0
        let total = agent.store.session_tokens(&agent.session).unwrap_or(0);
        agent.usage = Usage {
            input_tokens: total.max(0) as u64,
            output_tokens: 0,
            cached_input_tokens: 0,
        };
        tracing::info!(session = %session, messages = agent.history.len(), tokens = total, "已恢复会话");
        Ok(agent)
    }

    /// 换成共享的审批通道。
    ///
    /// 并行执行时，每个任务都有自己的 `Agent` 实例；如果各自持有独立
    /// 的 `ApprovalBroker`，用户在界面上点的「批准」就投递不到真正在等
    /// 待的那个任务。构造后接上同一个 broker 即可。
    pub fn with_shared_approvals(mut self, broker: Arc<ApprovalBroker>) -> Self {
        self.approvals = broker;
        self
    }

    /// 取一个取消句柄，可跨线程触发当前轮的中断。
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// 首轮对话后自动给会话起标题。
    ///
    /// 用一次独立的小请求（不带工具、输出上限很小）。返回 `Some(标题)`
    /// 表示已在库里写好；任何一步失败都返回 `Ok(None)` —— 起标题是锦上
    /// 添花，不该让主流程报错。
    pub async fn maybe_autotitle(&self) -> Result<Option<String>> {
        // 已有标题就不覆盖：用户手动改过的名字更该留住
        if let Some(s) = self.store.get_session(&self.session)? {
            if !s.title.trim().is_empty() {
                return Ok(None);
            }
        }

        // 只给"刚开始"的会话起名。历史一长，第一条往往是「你好」，
        // 拿它当标题既不贴切也没意义 —— 那种情况留给用户手动改。
        let user_turns = self
            .history
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .count();
        if user_turns > 2 {
            return Ok(None);
        }

        let Some(user_text) = self
            .history
            .iter()
            .find_map(|m| match m {
                Message::User { .. } => Some(m.text()),
                _ => None,
            })
            .filter(|t| !t.trim().is_empty())
        else {
            return Ok(None);
        };

        let assistant_text = self
            .history
            .iter()
            .find_map(|m| match m {
                Message::Assistant { tool_calls, .. } if tool_calls.is_empty() => Some(m.text()),
                _ => None,
            })
            .unwrap_or_default();

        let excerpt = format!(
            "用户：{}\n助手：{}",
            truncate_chars(&user_text, 400),
            truncate_chars(&assistant_text, 200)
        );

        let req = ChatRequest {
            system: Some(
                "你负责给对话起标题。只输出标题本身，不要引号、不要解释、不要以标点结尾。"
                    .to_string(),
            ),
            messages: vec![Message::user(format!(
                "用不超过 12 个汉字概括下面这段对话的主题：\n\n{excerpt}"
            ))],
            tools: Vec::new(),
            max_tokens: 48,
            temperature: Some(0.2),
        };

        let mut stream =
            zex_llm::stream_chat(&self.http, &self.cfg.provider, &self.cfg.model, req).await?;
        let mut raw = String::new();
        while let Some(item) = stream.next().await {
            if let StreamEvent::Text(t) = item? {
                raw.push_str(&t);
            }
        }

        let title = clean_title(&raw);
        if title.is_empty() {
            return Ok(None);
        }
        self.store.rename_session(&self.session, &title)?;
        tracing::info!(session = %self.session, title = %title, "自动生成会话标题");
        Ok(Some(title))
    }

    /// 请求中断当前轮。已经结束的轮次不受影响。
    pub fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 开一个新会话，`Agent::new` 时那样留一个空壳。
    pub fn new_session(&mut self) -> Result<()> {
        let session = self
            .store
            .create_session("", &self.cfg.model, &self.cfg.cwd.display().to_string())?;
        self.session = session.id;
        self.history.clear();
        self.usage = Usage::default();
        tracing::info!(session = %self.session, "已新建会话");
        Ok(())
    }

    /// 切换当前会话，并把历史换成目标会话的历史。
    ///
    /// 调用方负责保证此时没有正在进行的轮次 —— 中途切换会让
    /// 本轮的写入落到新会话上。
    pub fn switch_session(&mut self, session: SessionId) -> Result<()> {
        let stored = self.store.load_messages(&session)?;
        self.session = session.clone();
        self.history = stored.into_iter().map(|s| s.message).collect();
        self.usage = Usage::default();
        tracing::info!(session = %session, messages = self.history.len(), "已切换会话");
        Ok(())
    }

    /// 审批注册表。UI/CLI 需要 clone 它来回复审批。
    pub fn approvals(&self) -> Arc<ApprovalBroker> {
        self.approvals.clone()
    }

    /// 内核配置（克隆给任务执行器用）。
    pub fn config(&self) -> &AgentConfig {
        &self.cfg
    }

    /// 工具集。元素是 `Arc`，克隆成本很低。
    pub fn tools(&self) -> &[Arc<dyn ErasedTool>] {
        &self.tools
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

    /// 存储句柄（UI 读历史/审计用）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 切换运行模式。
    pub fn set_mode(&mut self, mode: Mode) {
        self.cfg.mode = mode;
    }

    /// 清空上下文（同时清库）。
    pub fn clear(&mut self) -> Result<()> {
        self.history.clear();
        self.store.clear_messages(&self.session)?;
        Ok(())
    }

    /// 执行一轮：接收用户输入，跑完整 loop，返回最终文本。
    pub async fn run_turn(
        &mut self,
        input: impl Into<String>,
        events: Option<&mpsc::Sender<CoreEvent>>,
    ) -> Result<String> {
        let turn = TurnId::generate();
        // 新一轮开始，清掉上一轮遗留的中断请求
        self.cancelled.store(false, Ordering::SeqCst);
        self.emit(
            CoreEvent::TurnStarted {
                session: self.session.clone(),
                turn: turn.clone(),
            },
            events,
        );

        let user_msg = Message::user(input);
        self.record(&user_msg)?;
        self.history.push(user_msg);

        let mut final_text = String::new();
        let mut end_reason = EndReason::Completed;
        let mut step = 0u32;

        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                end_reason = EndReason::Cancelled;
                break;
            }
            if step >= self.cfg.max_steps {
                end_reason = EndReason::MaxSteps {
                    limit: self.cfg.max_steps,
                };
                break;
            }
            step += 1;

            // 压缩判断放在每次调用模型之前。
            //
            // token 判断**只用本地全量估算**，不混用 provider 回报的 usage：
            // 后者代表的是"上一次请求"的用量，再叠加增量就是双重计数 ——
            // 那正是 Zebra 提前触发压缩的原因。估算成本随消息数线性增长，
            // 几百条消息的规模完全可忽略。
            self.compressor.tick_turn();
            self.maybe_compress(&turn, events).await;

            let req = self.build_request();
            let (text, thinking, tool_calls, usage) =
                match self.stream_once(req, &turn, events).await {
                    Ok(v) => v,
                    Err(e) => {
                        self.emit(
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

            let assistant = Message::Assistant {
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
            };
            self.record(&assistant)?;
            self.history.push(assistant);

            if tool_calls.is_empty() {
                final_text = text;
                break;
            }

            // 串行执行：审批必须逐个过，并行会让审批顺序混乱。
            for call in &tool_calls {
                // 中断后剩下的工具不再执行 —— 用户按停止就是不想让它继续动
                if self.cancelled.load(Ordering::SeqCst) {
                    end_reason = EndReason::Cancelled;
                    break;
                }
                let risk = self.risk_of(&call.name);

                self.emit(
                    CoreEvent::ToolCallBegin {
                        turn: turn.clone(),
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        risk,
                    },
                    events,
                );

                let ctx = AgentCtx::new(self.cfg.cwd.clone(), self.session.clone());
                let (content, is_error) =
                    self.dispatch_gated(&ctx, call, risk, &turn, events).await;

                self.emit(
                    CoreEvent::ToolCallEnd {
                        turn: turn.clone(),
                        call_id: call.id.clone(),
                        ok: !is_error,
                        brief: brief(&content),
                    },
                    events,
                );

                let tool_msg = Message::Tool {
                    call_id: call.id.clone(),
                    name: Some(call.name.clone()),
                    content,
                    is_error,
                };
                self.record(&tool_msg)?;
                self.history.push(tool_msg);
            }
        }

        self.emit(
            CoreEvent::TurnEnded {
                turn,
                reason: end_reason,
                usage: self.usage,
            },
            events,
        );
        Ok(final_text)
    }

    /// 按预算判断是否需要压缩，需要则执行。
    ///
    /// 两档策略：先截断过长的工具结果（不惊动 LLM），再走摘要。
    async fn maybe_compress(
        &mut self,
        turn: &TurnId,
        events: Option<&mpsc::Sender<CoreEvent>>,
    ) {
        let before = context::estimate_history_tokens(&self.history);
        let plan = self.compressor.plan(&self.history, None);

        match plan {
            CompressionPlan::None => {}
            CompressionPlan::ShrinkToolResults { indices } => {
                self.history = Compressor::apply_shrink(&self.history, &indices);
                let after = context::estimate_history_tokens(&self.history);
                tracing::info!(before, after, count = indices.len(), "已截断过长的工具结果");
                self.emit(
                    CoreEvent::ContextCompacted {
                        turn: turn.clone(),
                        before_tokens: before as u64,
                        after_tokens: after as u64,
                        strategy: "shrink_tool_results".into(),
                    },
                    events,
                );
            }
            CompressionPlan::Summarize { drop_before, .. } => {
                let dropped: Vec<Message> = self.history[..drop_before].to_vec();
                let summary = self.summarize(&dropped).await;

                // 先取出 history，避免与 compressor 的可变借用冲突
                let history = std::mem::take(&mut self.history);
                self.history = self.compressor.apply_summary(&history, drop_before, summary);
                let after = context::estimate_history_tokens(&self.history);

                tracing::info!(before, after, dropped = drop_before, "已压缩上下文");
                self.emit(
                    CoreEvent::ContextCompacted {
                        turn: turn.clone(),
                        before_tokens: before as u64,
                        after_tokens: after as u64,
                        strategy: "summarize".into(),
                    },
                    events,
                );
            }
        }
    }

    /// 让模型生成本次摘要。
    ///
    /// 失败或返回空时走降级方案 —— **摘要永远不返回空**，
    /// 空摘要等于让模型丢失全部前情。
    async fn summarize(&self, dropped: &[Message]) -> String {
        let prompt = compress::summary_prompt(
            self.compressor.summary(),
            dropped,
            self.cfg.budget.summary_max_tokens,
        );
        let req = ChatRequest::new(vec![Message::user(prompt)])
            .with_system("你是一个精确的对话摘要器。只输出摘要正文，不要任何前言或解释。");

        let turn = TurnId::generate();
        match self.stream_once(req, &turn, None).await {
            Ok((text, _, _, _)) if !text.trim().is_empty() => text,
            Ok(_) => {
                tracing::warn!("摘要模型返回空内容，使用降级摘要");
                self.compressor.fallback_summary(dropped)
            }
            Err(e) => {
                tracing::warn!(error = %e, "摘要生成失败，使用降级摘要");
                self.compressor.fallback_summary(dropped)
            }
        }
    }

    /// 组装一次请求。
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
                StreamEvent::Text(t) => self.emit(
                    CoreEvent::Token {
                        turn: turn.clone(),
                        text: t.clone(),
                    },
                    events,
                ),
                StreamEvent::Thinking(t) => self.emit(
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
    fn risk_of(&self, name: &str) -> Risk {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.spec().risk)
            .unwrap_or(Risk::Impactful)
    }

    /// 过闸门后执行一次工具调用。
    ///
    /// 这是 `docs/SECURITY.md` §10 四步叠加的落地点。三条不变量：
    /// - Deny 不可被任何配置绕过
    /// - 审批超时 = 拒绝
    /// - 每一次调用都写审计（含被拒绝的）
    async fn dispatch_gated(
        &self,
        ctx: &AgentCtx,
        call: &zex_core::ToolCall,
        risk: Risk,
        turn: &TurnId,
        events: Option<&mpsc::Sender<CoreEvent>>,
    ) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|t| t.spec().name == call.name) else {
            return (
                format!("没有名为 `{}` 的工具，请从可用工具中选择。", call.name),
                true,
            );
        };
        let spec = tool.spec();
        let verdict = self.policy.evaluate(&spec, &call.arguments, self.cfg.mode);

        let approved_by = match verdict {
            Verdict::Deny { reason } => {
                self.audit(risk, &call.name, &call.arguments, "denied", &reason);
                return (format!("操作被拒绝：{reason}"), true);
            }
            Verdict::Allow { by, snapshot } => {
                if snapshot {
                    // M1 只记录意图；真正的文件级快照在 M2（见 docs/SECURITY.md §9）
                    tracing::debug!(tool = %call.name, "该调用声明需要快照");
                }
                by
            }
            Verdict::Ask {
                reason,
                grantable,
                double_confirm,
            } => {
                let id = ApprovalId::generate();
                let rx = self.approvals.register(id.clone()).await;
                self.emit(
                    CoreEvent::ApprovalRequired {
                        req: zex_core::event::ApprovalRequest {
                            id: id.clone(),
                            turn: turn.clone(),
                            tool: call.name.clone(),
                            risk,
                            arguments: call.arguments.clone(),
                            summary: reason,
                            grantable,
                            // 污点追踪在 M2（SECURITY.md §5.2）
                            tainted: false,
                        },
                    },
                    events,
                );

                let decision = self
                    .approvals
                    .wait(&id, rx, self.cfg.approval_timeout)
                    .await;

                match decision {
                    ApprovalDecision::Reject { reason: why } => {
                        self.audit(risk, &call.name, &call.arguments, "rejected", &why);
                        return (format!("用户拒绝了该操作：{why}"), true);
                    }
                    ApprovalDecision::Approve { remember } => {
                        // 需要二次确认的高危操作，不接受"记住"
                        if let Some(scope) = remember {
                            if double_confirm {
                                tracing::warn!(tool = %call.name, "高危操作不接受授权记忆，已忽略");
                            } else if let Err(e) =
                                self.persist_grant(&call.name, &scope, &call.arguments)
                            {
                                self.emit(
                                    CoreEvent::Warning {
                                        message: format!("授权记忆保存失败: {e}"),
                                    },
                                    events,
                                );
                            }
                        }
                        ApprovedBy::User
                    }
                }
            }
        };

        // 执行
        let (content, is_error) = match tool.call_json(ctx, call.arguments.clone()).await {
            Ok(out) => (out.content, out.is_error),
            Err(Error::BadArgs { tool, message }) => (
                format!("调用 `{tool}` 的参数不合法：{message}。请检查参数后重试。"),
                true,
            ),
            Err(e) => (format!("工具 `{}` 执行失败：{e}", call.name), true),
        };

        // 凭据脱敏放在最外一层：不管密钥是从 env、文件还是网络响应里来的，
        // 只要形态可疑就不进上下文。路径黑名单挡得住 `cat .env`，
        // 挡不住 `env | grep KEY` 或 `curl` 回来的配置。
        let content = zex_policy::secret::redact(&content);

        let result_brief = if is_error { "失败" } else { "成功" };
        self.audit(
            risk,
            &call.name,
            &call.arguments,
            &approved_by.as_audit_str(),
            result_brief,
        );
        (content, is_error)
    }

    /// 把"记住这个授权"落库。
    fn persist_grant(
        &self,
        tool: &str,
        scope: &zex_core::event::RememberScope,
        args: &serde_json::Value,
    ) -> Result<()> {
        use zex_core::event::RememberScope;
        let risk = self.risk_of(tool);
        match scope {
            RememberScope::Exact => {
                // 只记住这一次的参数
                if let Some(p) = zex_policy::glob::literal_pattern(args) {
                    self.store.add_grant(tool, &p, risk, None)?;
                }
            }
            RememberScope::Pattern {
                pattern: Some(p),
                days,
            } => {
                // 调用方显式给了模式（如 `git *`），照用
                self.store.add_grant(tool, p, risk, *days)?;
            }
            RememberScope::Pattern {
                pattern: None,
                days,
            } => {
                // "该工具全部参数" 的口子太宽：用户在某个文件上点一次记住，
                // 就放行了这个工具对任何路径的调用。退化成这次的具体参数。
                if let Some(p) = zex_policy::glob::literal_pattern(args) {
                    self.store.add_grant(tool, &p, risk, *days)?;
                }
            }
        }
        Ok(())
    }

    /// 写一条审计。
    fn audit(&self, risk: Risk, tool: &str, args: &serde_json::Value, by: &str, result: &str) {
        let entry = AuditEntry::tool_call(risk, tool, digest_args(args), by, self.session.as_str())
            .with_result(result);
        if let Err(e) = self.store.write_audit(&entry) {
            // 审计写不进去是严重问题，但不能因此让整个任务崩掉；
            // 用 error 级别日志留下痕迹。
            tracing::error!(error = %e, "写审计失败");
        }
    }

    /// 落库一条消息。
    fn record(&self, msg: &Message) -> Result<()> {
        let tokens = context::estimate_message_tokens(msg);
        self.store.append_message(&self.session, msg, tokens)?;
        Ok(())
    }

    /// 事件投递。接收端已关闭时静默忽略 —— UI 关掉不代表内核必须崩。
    fn emit(&self, ev: CoreEvent, events: Option<&mpsc::Sender<CoreEvent>>) {
        if let Some(tx) = events {
            if tx.try_send(ev).is_err() {
                // 队列满或已关闭：丢弃事件而不是阻塞内核。
                // UI 跟不上时，正确行为是掉帧，不是卡死 agent。
            }
        }
    }
}

/// 按字符数截断（不是字节），避免切碎多字节字符。
fn truncate_chars(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(limit).collect::<String>())
    }
}

/// 清理模型返回的标题：去引号、去末尾标点、压到一行。
fn clean_title(raw: &str) -> String {
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line
        .trim()
        .trim_matches(['"', '\'', '「', '」', '『', '』', '《', '》', '*', '#', ' '])
        .trim_end_matches(['。', '.', '，', ',', '！', '!', '？', '?', '：', ':'])
        .trim();
    // 再兜一层长度上限：模型偶尔不听话，会写一整句
    truncate_chars(trimmed, 20)
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
    fn clean_title_strips_quotes_and_punctuation() {
        assert_eq!(clean_title("「关于贪吃蛇」"), "关于贪吃蛇");
        assert_eq!(clean_title("\"调试滚动问题。\""), "调试滚动问题");
        assert_eq!(clean_title("  **密钥保护**  "), "密钥保护");
        // 模型偶尔不听话写一整句，要截断
        assert!(clean_title(&"标".repeat(50)).chars().count() <= 21);
    }

    #[test]
    fn clean_title_skips_leading_blank_lines() {
        assert_eq!(clean_title("\n\n真正的标题\n第二行"), "真正的标题");
    }

    #[test]
    fn truncate_chars_counts_chars_not_bytes() {
        // 中文按字符算，不能按字节切（否则会切碎 UTF-8）
        assert_eq!(truncate_chars("中文测试", 2).chars().count(), 3); // "中文…"
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

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

    fn test_cfg() -> AgentConfig {
        use zex_llm::ProviderKind;
        AgentConfig::new(
            ProviderConfig {
                kind: ProviderKind::OpenAiCompat,
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: Default::default(),
            },
            "test-model",
            PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn new_agent_creates_persisted_session() {
        let store = Store::in_memory().unwrap();
        let agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let sid = agent.session_id().clone();
        assert!(store.get_session(&sid).unwrap().is_some(), "会话应已落库");
    }

    #[test]
    fn resume_does_not_create_a_session() {
        // 回归：resume 曾经复用 new 的构造，而 new 会插一条空会话，
        // 结果每启动一次就多一个空壳
        let store = Store::in_memory().unwrap();
        let agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let sid = agent.session_id().clone();
        let before = store.list_sessions(100).unwrap().len();

        let resumed = Agent::resume(test_cfg(), vec![], store.clone(), sid.clone()).unwrap();

        assert_eq!(resumed.session_id(), &sid);
        assert_eq!(
            store.list_sessions(100).unwrap().len(),
            before,
            "resume 不应新增会话"
        );
    }

    #[test]
    fn resume_restores_history() {
        let store = Store::in_memory().unwrap();
        let agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let sid = agent.session_id().clone();
        // 直接往库里写两条
        store.append_message(&sid, &Message::user("hi"), 1).unwrap();
        store
            .append_message(&sid, &Message::system("sys"), 1)
            .unwrap();

        let resumed = Agent::resume(test_cfg(), vec![], store, sid).unwrap();
        assert_eq!(resumed.history().len(), 2);
    }

    #[test]
    fn switch_session_swaps_history_and_target() {
        let store = Store::in_memory().unwrap();
        let mut agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let a = agent.session_id().clone();
        store.append_message(&a, &Message::user("在 A 里"), 1).unwrap();
        agent.history.push(Message::user("在 A 里"));

        // 另起一个会话，写两条
        let other = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let b = other.session_id().clone();
        assert_ne!(a, b, "两次 new 应得到不同会话");
        store.append_message(&b, &Message::user("在 B 里"), 1).unwrap();
        store.append_message(&b, &Message::user("B 的第二条"), 1).unwrap();

        agent.switch_session(b.clone()).unwrap();

        assert_eq!(agent.session_id(), &b, "会话 id 应已切换");
        assert_eq!(agent.history().len(), 2, "历史应换成目标会话的");
        assert_eq!(agent.history()[1].text(), "B 的第二条");
    }

    #[test]
    fn switch_to_unknown_session_loads_empty_history() {
        let store = Store::in_memory().unwrap();
        let mut agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        store
            .append_message(agent.session_id(), &Message::user("旧"), 1)
            .unwrap();
        agent.history.push(Message::user("旧"));

        // 一个库里没有的会话 id：不该 panic，历史清空即可
        let ghost = SessionId::from("ses_does_not_exist".to_string());
        agent.switch_session(ghost.clone()).unwrap();
        assert_eq!(agent.session_id(), &ghost);
        assert!(agent.history().is_empty());
    }

    #[test]
    fn clear_empties_both_memory_and_db() {
        let store = Store::in_memory().unwrap();
        let mut agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let sid = agent.session_id().clone();
        store.append_message(&sid, &Message::user("a"), 1).unwrap();
        agent.history.push(Message::user("a"));

        agent.clear().unwrap();
        assert!(agent.history().is_empty());
        assert_eq!(store.count_messages(&sid).unwrap(), 0);
    }
}
