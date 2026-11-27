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
pub mod subagent;
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
    /// 单段内最多允许的"模型调用"次数。**`0` = 不限**（默认）。
    ///
    /// # 为什么默认不限
    ///
    /// 步数上限对"跑飞"没什么用，对"长任务"伤害很大：实测 119 个任务里
    /// 6 个恰好停在 25 步，用户看到的是"干到一半不说话"。而真正复杂的活
    /// （重构、批量迁移、逐文件核对）本来就该跑几百步甚至跨天。
    ///
    /// 该拦住的是**卡住**（同一个调用反复失败），不是**推进**。
    /// 所以默认不设上限，靠这几个东西兜底：用户按停止、上下文压缩、
    /// token 预算，以及每 50 步一次的进度提示。
    ///
    /// 显式设成正数仍然有效（撞上会自动续段，见 `max_continue_rounds`）。
    pub max_steps: u32,
    /// 撞到 `max_steps` 之后自动续几段。
    ///
    /// # 为什么需要这个
    ///
    /// 25 步听起来很多，实际上"读几个文件 → 改 → 自查 → 再改"这种活
    /// 很容易用掉：实测 119 个任务里有 6 个恰好停在 25 步，用户看到的是
    /// "干到一半不说话了"，得手动打字说「继续」。
    ///
    /// 步数上限的本意是**防跑飞**，不是"任务边界"。模型这一轮还没给出
    /// 最终回复就说明它没做完，续上就是了 —— 但要有天花板，所以是
    /// 有限段数而不是无限续。
    pub max_continue_rounds: u32,
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
    /// 采样温度。None = 用服务端默认（通常是 1.0）。
    pub temperature: Option<f32>,
    /// 思考强度。
    pub thinking: zex_llm::ThinkingEffort,
    /// 追加到系统提示末尾的内容（技能清单等）。
    ///
    /// 单独一个字段而不是改 `prompt::system_prompt`：后者是产品人格，
    /// 这里装的是运行时环境，两者的变更节奏完全不同。
    pub extra_prompt: String,
}

/// 单次输出的默认上限（token）。
///
/// 配置里没写 `max_tokens` 时用它。单独提成常量是因为不止一处要"回到默认"：
/// 换了模型、配置被删掉，都得有一个明确的落点，而不是沿用上一个模型的值。
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

impl AgentConfig {
    /// 用最小必填项构造。
    pub fn new(provider: ProviderConfig, model: impl Into<String>, cwd: PathBuf) -> Self {
        let model = model.into();
        AgentConfig {
            provider,
            // 预算跟着模型走：1M 窗口的模型按 128K 压缩，
            // 等于每轮都在白扔上下文
            budget: ContextBudget::for_model(&model),
            model,
            max_steps: 0,
            max_continue_rounds: 3,
            max_tokens: DEFAULT_MAX_TOKENS,
            cwd,
            mode: Mode::Confirm,
            approval_timeout: approval::DEFAULT_TIMEOUT,
            temperature: None,
            thinking: zex_llm::ThinkingEffort::Off,
            extra_prompt: String::new(),
        }
    }

    /// 追加一段系统提示（技能清单等）。
    pub fn with_extra_prompt(mut self, extra: impl Into<String>) -> Self {
        self.extra_prompt = extra.into();
        self
    }

    /// 套用一组采样参数；`None` 的项保持不变。
    pub fn with_model_params(mut self, temperature: Option<f32>, max_tokens: Option<u32>) -> Self {
        if let Some(t) = temperature {
            self.temperature = Some(t);
        }
        if let Some(m) = max_tokens {
            self.max_tokens = m;
        }
        self
    }
}

/// 把时长说成人话。
///
/// 长任务可能跑几小时，`3700 秒` 没人愿意换算。UI 报"完成"时也用它，
/// 所以是 `pub` 而不是内部小工具。
pub fn human_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
    }
}

/// 到了步数上限该干什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepBudget {
    /// 没设上限（`max_steps == 0`），想跑多久跑多久。
    Unlimited,
    /// 再开一段，`round` 是第几次续段（从 1 开始）。
    Continue {
        /// 第几次续段。
        round: u32,
    },
    /// 到顶了，收工。`total` 是这一轮总共跑过多少步。
    Stop {
        /// 总步数上限（含续段）。
        total: u32,
    },
}

/// 到了 `max_steps` 时，续段还是停。
///
/// 单独抽出来是为了能测：这段逻辑决定"任务会不会被悄悄掐断"，
/// 而真正的循环里带着网络和存储，测不动。
fn step_budget(step: u32, cfg: &AgentConfig, round: u32) -> StepBudget {
    // `0` = 不限：这是默认值，也是"长任务不该被步数掐断"的落实点
    if cfg.max_steps == 0 {
        return StepBudget::Unlimited;
    }
    if step < cfg.max_steps {
        return StepBudget::Unlimited; // 还没到，调用方也不会问
    }
    if round < cfg.max_continue_rounds {
        StepBudget::Continue { round: round + 1 }
    } else {
        StepBudget::Stop {
            total: cfg.max_steps.saturating_mul(cfg.max_continue_rounds + 1),
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
    /// 当前运行模式。**每次判定前现读**，不是快照 —— 用户在任务跑一半
    /// 时切到「完全访问」，下一句命令就不该再问（见 `zex_core::ModeCell`）。
    mode: zex_core::ModeCell,
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
        // 上一次进程被杀 / 用户点停止，都会在库里留下「有调用没结果」的
        // 半截历史。带这种历史发第一次请求，接口直接 400：
        //   an assistant message with 'tool_calls' must be followed by
        //   tool messages responding to each 'tool_call_id'
        // 所以装载完先补一遍，再让它参与任何请求。
        let mut history = history;
        // 中断留下的半截历史：补出来的结果顺手写回库里。
        //
        // 不写回也能跑（下一轮发请求前还会再修），但那样**每次启动都会
        // 再修一遍**，日志里就永远挂着一行"历史不完整，已补齐" —— 看着
        // 像每次都在坏。只在安全时写：被修的那条 assistant 必须是库里
        // 最后一条（中断的常态），这样补出来的结果就是新尾巴。
        let before_len = history.len();
        let tail_is_orphan = matches!(
            history.last(),
            Some(Message::Assistant { tool_calls, .. }) if !tool_calls.is_empty()
        );
        let healed = crate::compress::heal_tool_pairs(&mut history);
        if healed > 0 {
            if tail_is_orphan {
                // 缺口在末尾：追加就能真正修好，下次启动不用再修
                for msg in &history[before_len..] {
                    let tokens = context::estimate_message_tokens(msg);
                    if let Err(e) = store.append_message(&session, msg, tokens) {
                        tracing::warn!(error = %e, "补出来的工具结果没能写回库");
                        break;
                    }
                }
                tracing::info!(healed, "历史末尾缺工具结果，已补齐并写回库");
            } else {
                // 缺口在历史中段：库里只有追加，补不到中间去，所以每次启动
                // 都要重算一遍。功能上没问题（每轮发请求前还会再补一次），
                // 因此不是 warn —— 它不是异常，是一次幂等的修补。
                tracing::debug!(healed, "历史中段缺工具结果，已在内存里补齐");
            }
        }
        // 工具结果预算跟着模型的输出上限走。放在这个**唯一的**装配点上，
        // 而不是各个 builder 里：`cfg.max_tokens` 有四个写入点（构造默认值、
        // 配置里的 params、请求级换模型、每轮从配置文件重读），
        // 漏掉任何一处就是“改了参数却不生效”。
        // 回写进 cfg，是为了与 `cfg.budget` 保持单一真相 —— 否则摘要长度等
        // 地方读到的是另一个数。
        let mut cfg = cfg;
        cfg.budget = cfg.budget.clone().with_output_tokens(cfg.max_tokens);
        let compressor = Compressor::new(cfg.budget.clone());
        Ok(Agent {
            mode: zex_core::ModeCell::new(cfg.mode),
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

    /// 接上一个外部的取消开关。
    ///
    /// 子智能体跑完一轮后实例就被丢弃了，但 `terminate_agent` 仍然
    /// 要能叫停它后续的轮次 —— 所以取消开关跟着会话走，不跟着实例走。
    pub fn with_cancel_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancelled = flag;
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
    /// 先用第一句话立刻起个标题，不等模型。
    ///
    /// 自动命名原本只看模型返回，而模型调用发生在整轮结束之后 ——
    /// 一个跑十分钟的任务，侧栏就空白十分钟。先给个粗标题，
    /// 之后 [`Self::maybe_autotitle`] 再用模型结果覆盖它。
    ///
    /// 返回 `Some(title)` 表示这次确实写了标题（供 UI 立即刷新）。
    pub fn quick_title(&self, first_input: &str) -> Result<Option<String>> {
        let Some(s) = self.store.get_session(&self.session)? else {
            return Ok(None);
        };
        if !s.title.trim().is_empty() {
            return Ok(None);
        }
        if first_input.trim().is_empty() {
            return Ok(None);
        }
        let title = truncate_chars(first_input.trim(), 20);
        if title.is_empty() {
            return Ok(None);
        }
        self.store.rename_session(&self.session, &title)?;
        Ok(Some(title))
    }

    /// 用模型给会话起个更好的标题。
    ///
    /// 只替换「首句截断」那个占位；用户手动改过的不动。
    pub async fn maybe_autotitle(&self) -> Result<Option<String>> {
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

        // 已有标题就不覆盖 —— 但「首句截断」那个兜底标题除外，
        // 它本来就是占位，模型给出的更好就该换掉。
        let fallback = truncate_chars(user_text.trim(), 20);
        if let Some(s) = self.store.get_session(&self.session)? {
            let existing = s.title.trim();
            if !existing.is_empty() && existing != fallback {
                return Ok(None);
            }
        }

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
            // 推理模型（deepseek-flash 之类）会先花 token 思考再输出，
            // 48 会被 thinking 吃光、正文为空。留够余量。
            max_tokens: 512,
            temperature: Some(0.2),
            // 起标题不值得思考：给个标题而已，多想只会更慢更贵
            thinking: zex_llm::ThinkingEffort::Off,
        };

        let mut stream = match zex_llm::stream_chat(
            &self.http,
            &self.cfg.provider,
            &self.cfg.model,
            req,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "autotitle", error = %e, "起名请求失败");
                return Err(e);
            }
        };
        let mut raw = String::new();
        while let Some(item) = stream.next().await {
            if let StreamEvent::Text(t) = item? {
                raw.push_str(&t);
            }
        }

        let mut title = clean_title(&raw);
        if title.is_empty() {
            // 模型没给出可用标题（空回复、网络抖动、推理模型把 token
            // 花在了 thinking 上…）。退回到用户第一句话的截断 ——
            // 有标题总比侧栏一片「会话 3f8a2c」强。
            tracing::warn!(target: "autotitle", "模型未返回可用标题，改用首句截断");
            title = truncate_chars(user_text.trim(), 20);
            if title.is_empty() {
                return Ok(None);
            }
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
        let session =
            self.store
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

    /// provider 连接配置（语音转写这类非对话调用要用）。
    pub fn provider(&self) -> &zex_llm::ProviderConfig {
        &self.cfg.provider
    }

    /// 复用的 HTTP 客户端。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
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
        self.mode.set(mode);
    }

    /// 接入外部的模式句柄。
    ///
    /// 内核给每个会话准备一个，`UiCommand::SetMode` 改的是同一个句柄 ——
    /// 这样"用户切了模式"和"下一句命令要不要问"之间没有延迟。
    pub fn with_mode_cell(mut self, cell: zex_core::ModeCell) -> Self {
        self.cfg.mode = cell.get();
        self.mode = cell;
        self
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
        self.run_turn_with_images(input, Vec::new(), events).await
    }

    /// 带图片的一轮。
    ///
    /// 图片以 `Part::Image` 混进用户消息，provider 侧会转成 base64
    /// data URL —— 模型能直接看到截图，不用先落盘再让它 fs_read。
    pub async fn run_turn_with_images(
        &mut self,
        input: impl Into<String>,
        images: Vec<zex_core::message::ImageRef>,
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

        let mut content = vec![zex_core::message::Part::text(input)];
        for img in images {
            content.push(zex_core::message::Part::Image(img));
        }
        let user_msg = Message::User { content };
        self.record(&user_msg)?;
        self.history.push(user_msg);

        let mut final_text = String::new();
        let mut end_reason = EndReason::Completed;
        let mut step = 0u32;
        // 已经自动续过几段
        let mut round = 0u32;
        // 整轮累计（续段也算进去）—— 结束时报给用户的就是它
        let mut total_steps = 0u32;
        // 因输出截断自动重试过没有。只给一次：第二次还撞上限，
        // 说明模型没按提醒拆分，继续重试只是烧钱。
        let mut truncation_retried = false;
        let started = std::time::Instant::now();

        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                end_reason = EndReason::Cancelled;
                break;
            }
            // 步数上限是**防跑飞**的闸，不是任务边界：这一轮还没给出
            // 最终回复，就说明没做完，续一段接着跑（上限见 max_continue_rounds）。
            // 以前这里直接 break，用户看到的是"干到一半不说话"。
            //
            // 判定只有 `step_budget` 一处：早先这里还额外写了一遍
            // `max_steps != 0 &&`，结果改了默认值之后两边对不上 ——
            // 0（不限）被守卫判成"已到上限"，第一轮就停。
            match step_budget(step, &self.cfg, round) {
                StepBudget::Unlimited => {}
                StepBudget::Continue { round: next } => {
                    round = next;
                    step = 0;
                    // 让用户知道这不是结束，也解释了为什么还在动
                    self.emit(
                        CoreEvent::Warning {
                            message: format!(
                                "已达单轮 {} 步上限，自动继续（{}/{}）",
                                self.cfg.max_steps, round, self.cfg.max_continue_rounds
                            ),
                        },
                        events,
                    );
                    continue;
                }
                StepBudget::Stop { total } => {
                    end_reason = EndReason::MaxSteps { limit: total };
                    break;
                }
            }
            step += 1;
            total_steps += 1;
            // 跨天跑也让人看得见它在动：不是限制，是心跳
            if total_steps % 50 == 0 {
                self.emit(
                    CoreEvent::Warning {
                        message: format!(
                            "已跑 {total_steps} 步，用时 {}",
                            human_elapsed(started.elapsed())
                        ),
                    },
                    events,
                );
            }

            // 压缩判断放在每次调用模型之前。
            //
            // token 判断**只用本地全量估算**，不混用 provider 回报的 usage：
            // 后者代表的是"上一次请求"的用量，再叠加增量就是双重计数 ——
            // 那正是 Zebra 提前触发压缩的原因。估算成本随消息数线性增长，
            // 几百条消息的规模完全可忽略。
            self.compressor.tick_turn();
            self.maybe_compress(&turn, events).await;
            // 压缩、中断、上一步的工具执行都可能让配对缺口重新出现；
            // 这里是**发请求前的最后一道闸**，幂等且 O(n)，代价可忽略。
            if crate::compress::heal_tool_pairs(&mut self.history) > 0 {
                tracing::debug!("发请求前补齐了工具调用配对");
            }

            let req = self.build_request();
            let streamed = match self.stream_once(req, &turn, events).await {
                Ok(v) => Ok(v),
                // 输出被上限截断：这一轮的半截工具调用不能进历史（协议要求
                // 调用与结果成对），只能丢掉重来。重来之前做两件事：
                // 关掉思考（它和正文抢同一份 max_tokens），并在系统提示里
                // 明确要求拆分写入 —— 否则模型会原样再写一遍，再撞一次。
                Err(zex_core::Error::Truncated { detail }) if !truncation_retried => {
                    truncation_retried = true;
                    self.emit(
                        CoreEvent::Warning {
                            message: format!("{detail}（正在重试一次：关闭思考并要求分块写入）"),
                        },
                        events,
                    );
                    self.stream_once(
                        truncation_retry_request(&self.build_request()),
                        &turn,
                        events,
                    )
                    .await
                }
                Err(e) => Err(e),
            };

            let (text, thinking, tool_calls, usage) = match streamed {
                Ok(v) => v,
                Err(e) => {
                    self.emit(
                        CoreEvent::TurnEnded {
                            session: self.session.clone(),
                            turn: turn.clone(),
                            reason: EndReason::Failed {
                                message: e.to_string(),
                            },
                            usage: self.usage,
                            steps: total_steps,
                            elapsed_ms: started.elapsed().as_millis() as u64,
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
            let mut executed = 0usize;
            for call in &tool_calls {
                // 中断后剩下的工具不再执行 —— 用户按停止就是不想让它继续动
                if self.cancelled.load(Ordering::SeqCst) {
                    end_reason = EndReason::Cancelled;
                    break;
                }
                let risk = self.risk_of(&call.name, &call.arguments);

                self.emit(
                    CoreEvent::ToolCallBegin {
                        session: self.session.clone(),
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
                        session: self.session.clone(),
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
                executed += 1;
            }

            // 中断发生在半路时，剩下的调用一条结果都不会有。用户看到的是
            // "停下了"，但下一轮请求会因为配对不全被接口拒掉 —— 所以在
            // 这里就把没跑的那些补成明确的结果，别等下一轮再去猜。
            if self.cancelled.load(Ordering::SeqCst) {
                for call in &tool_calls[executed..] {
                    let msg = Message::Tool {
                        call_id: call.id.clone(),
                        name: Some(call.name.clone()),
                        content: format!(
                            "未执行：用户在这一步之前中断了本轮，`{}` 没有运行。",
                            call.name
                        ),
                        is_error: true,
                    };
                    self.record(&msg)?;
                    self.history.push(msg);
                }
            }
        }

        self.emit(
            CoreEvent::TurnEnded {
                session: self.session.clone(),
                turn,
                reason: end_reason,
                usage: self.usage,
                steps: total_steps,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
            events,
        );
        Ok(final_text)
    }

    /// 按预算判断是否需要压缩，需要则执行。
    ///
    /// 两档策略：先截断过长的工具结果（不惊动 LLM），再走摘要。
    async fn maybe_compress(&mut self, turn: &TurnId, events: Option<&mpsc::Sender<CoreEvent>>) {
        let before = context::estimate_history_tokens(&self.history);
        let plan = self.compressor.plan(&self.history, None);

        match plan {
            CompressionPlan::None => {}
            CompressionPlan::ShrinkToolResults { indices } => {
                let max = self.compressor.budget().tool_result_max_tokens;
                self.history = Compressor::apply_shrink(&self.history, &indices, max);
                let after = context::estimate_history_tokens(&self.history);
                tracing::info!(before, after, count = indices.len(), "已截断过长的工具结果");
                // 只报有意义的节省：省下的还不如一条提示本身占的地方，
                // 说出来只是噪音。阈值取窗口的 1% —— 早先是 32 token，
                // 于是「193458 → 193328」这种 0.07% 的截断也会刷一张卡片。
                let saved = before.saturating_sub(after);
                if saved * 100 >= self.compressor.budget().window {
                    self.emit(
                        CoreEvent::ContextCompacted {
                            session: self.session.clone(),
                            turn: turn.clone(),
                            before_tokens: before as u64,
                            after_tokens: after as u64,
                            strategy: "shrink_tool_results".into(),
                        },
                        events,
                    );
                }
            }
            CompressionPlan::Summarize { drop_before, .. } => {
                let dropped: Vec<Message> = self.history[..drop_before].to_vec();
                let summary = self.summarize(&dropped).await;

                // 先取出 history，避免与 compressor 的可变借用冲突
                let history = std::mem::take(&mut self.history);
                self.history = self
                    .compressor
                    .apply_summary(&history, drop_before, summary);
                let after = context::estimate_history_tokens(&self.history);

                tracing::info!(before, after, dropped = drop_before, "已压缩上下文");
                self.emit(
                    CoreEvent::ContextCompacted {
                        session: self.session.clone(),
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
        let specs: Vec<_> = self.tools.iter().map(|t| t.spec()).collect();
        let mut system = prompt::system_prompt(&self.cfg.cwd);
        // 工具规则跟着实际工具集走，不写在人格里
        system.push_str(&prompt::tool_rules(&specs));
        if !self.cfg.extra_prompt.trim().is_empty() {
            system.push_str(&self.cfg.extra_prompt);
        }
        let req = ChatRequest::new(self.history.clone())
            .with_system(system)
            .with_tools(specs);
        ChatRequest {
            max_tokens: self.cfg.max_tokens,
            temperature: self.cfg.temperature,
            thinking: self.cfg.thinking,
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
        // 记住这一轮的输出上限：截断报错要把它写进去，否则用户和模型
        // 只知道"被截断了"，不知道顶在哪儿、该往哪调
        let max_tokens = req.max_tokens;
        let mut stream = match zex_llm::stream_chat(
            &self.http,
            &self.cfg.provider,
            &self.cfg.model,
            req,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "autotitle", error = %e, "起名请求失败");
                return Err(e);
            }
        };

        let mut acc = zex_llm::Accumulator::default();
        while let Some(item) = stream.next().await {
            let ev = item?;
            // 先推事件再聚合：用户应该看到 token 落在屏幕上，
            // 而不是等整段结束才看到（流式的全部意义在这里）。
            match &ev {
                StreamEvent::Text(t) => self.emit(
                    CoreEvent::Token {
                        session: self.session.clone(),
                        turn: turn.clone(),
                        text: t.clone(),
                    },
                    events,
                ),
                StreamEvent::Thinking(t) => self.emit(
                    CoreEvent::Thinking {
                        session: self.session.clone(),
                        turn: turn.clone(),
                        text: t.clone(),
                    },
                    events,
                ),
                _ => {}
            }
            acc.push(ev);
        }

        let out = acc.finish().map_err(|e| match e {
            zex_core::Error::Truncated { detail } => zex_core::Error::Truncated {
                detail: format!("{detail}（本轮 max_tokens={max_tokens}）"),
            },
            other => other,
        })?;
        // 没有工具调用却被截断：模型是把**正文**写到一半没词了。
        // 这种情况不该悄悄结束 —— 用户看到的会是一句戛然而止的话，
        // 只能猜"是它不想说了还是被砍了"。
        if out.tool_calls.is_empty() && zex_llm::is_truncated(out.stop_reason.as_deref()) {
            self.emit(
                CoreEvent::Warning {
                    message: format!(
                        "这一轮回复被 max_tokens={max_tokens} 截断，内容没有说完。\
                         可以回复「继续」接着写，或把这个模型的 max_tokens 调大"
                    ),
                },
                events,
            );
        }
        Ok((out.text, out.thinking, out.tool_calls, out.usage))
    }

    /// 查一次工具调用的风险等级（未知工具按 L2 处理 —— 保守）。
    ///
    /// 带上参数：`shell` 的风险取决于具体命令，`ls` 和 `rm -rf` 不该同一档。
    fn risk_of(&self, name: &str, arguments: &serde_json::Value) -> Risk {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.risk_for(arguments))
            .unwrap_or(Risk::Impactful)
    }

    /// 现在这个模式会怎么处理这次调用。**模式是现读的**，不是快照。
    ///
    /// 直接问"会不会拦我"的入口，给 `dispatch_gated` 和测试共用 ——
    /// 两边各抄一份判定顺序，迟早会走岔。
    fn judge_for(&self, spec: &zex_core::tool::ToolSpec, args: &serde_json::Value) -> Verdict {
        // cwd 必须传下去：模型给的路径常常是相对的（`src/main.rs`）或
        // `~/x`，不带上工作目录，权限层就只能当成"哪都不在" → 误判成
        // 越界 → 连「完全访问」都要弹审批。
        self.policy
            .evaluate(spec, args, self.mode.get(), &self.cfg.cwd)
    }

    /// 按工具名与参数问一次判定结论（不改任何状态）。
    ///
    /// 工具的静态 risk 只作默认值：`shell` 真正的等级取决于命令内容，
    /// 所以这里和真正执行时一样，用 `risk_for` 覆盖后再判。
    pub fn judge(&self, name: &str, args: &serde_json::Value) -> Verdict {
        let mut spec = self
            .tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.spec())
            .unwrap_or_else(|| zex_core::tool::ToolSpec {
                name: "unknown",
                description: "",
                risk: Risk::Impactful,
                input_schema: serde_json::json!({}),
                hint: None,
            });
        spec.risk = self.risk_of(name, args);
        self.judge_for(&spec, args)
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
        // spec 里的 risk 是静态默认值。shell 这类工具的风险取决于具体命令，
        // 所以用判定过的等级覆盖它再交给策略层 —— 否则 `ls` 也会被要求审批。
        let mut spec = tool.spec();
        spec.risk = risk;
        let verdict = self.judge_for(&spec, &call.arguments);

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
                        session: self.session.clone(),
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
        let risk = self.risk_of(tool, args);
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
    fn truncation_retry_turns_thinking_off_and_asks_for_chunks() {
        // 重试请求必须做两件事：关掉思考（它和正文抢同一份 max_tokens）、
        // 明确要求分块写（否则模型会原样重写整个文件）
        let mut req = ChatRequest::new(Vec::new()).with_system("系统提示");
        req.thinking = zex_llm::ThinkingEffort::High;
        let next = truncation_retry_request(&req);
        assert_eq!(next.thinking, zex_llm::ThinkingEffort::Off);
        let sys = next.system.unwrap_or_default();
        assert!(sys.starts_with("系统提示"), "原提示要保留");
        assert!(sys.contains("append"), "得告诉它怎么续写: {sys}");
        assert!(sys.contains("max_tokens"), "得说明为什么被截断: {sys}");
        // 原请求不能被就地改掉：克隆语义
        assert_eq!(req.thinking, zex_llm::ThinkingEffort::High);
    }

    #[test]
    fn truncation_retry_works_without_a_system_prompt() {
        let req = ChatRequest::new(Vec::new());
        let next = truncation_retry_request(&req);
        assert!(next.system.unwrap_or_default().contains("被截断"));
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
    fn compressor_budget_follows_configured_max_tokens() {
        // 回归：`max_tokens` 有四个写入点，早先预算只在构造时算一次，
        // 所以配置里写的上限只管输出、不管工具结果。
        let store = Store::in_memory().unwrap();
        let cfg = test_cfg().with_model_params(None, Some(2048));
        let agent = Agent::new(cfg, vec![], store).unwrap();
        assert_eq!(agent.compressor.budget().tool_result_max_tokens, 2048);
        // 回写后两个字段必须一致，否则摘要长度等处读到的是旧值
        assert_eq!(agent.cfg.budget.tool_result_max_tokens, 2048);
    }

    #[test]
    fn default_has_no_step_cap() {
        // 用户的原话：复杂任务可能跑好几天，别用步数掐断它
        let cfg = test_cfg();
        assert_eq!(cfg.max_steps, 0, "默认不设上限");
        assert_eq!(
            step_budget(100_000, &cfg, 0),
            StepBudget::Unlimited,
            "再长也不该因为步数停下"
        );
    }

    #[test]
    fn explicit_cap_auto_continues_before_giving_up() {
        // 显式设了上限时，撞上要续段而不是直接哑掉
        let mut cfg = test_cfg();
        cfg.max_steps = 25;
        cfg.max_continue_rounds = 3;

        assert_eq!(
            step_budget(10, &cfg, 0),
            StepBudget::Unlimited,
            "没到上限不该触发任何动作"
        );
        assert_eq!(step_budget(25, &cfg, 0), StepBudget::Continue { round: 1 });
        assert_eq!(step_budget(25, &cfg, 2), StepBudget::Continue { round: 3 });
        assert_eq!(
            step_budget(25, &cfg, 3),
            StepBudget::Stop { total: 100 },
            "续满要停，且报出真实总步数（25 × 4 = 100）"
        );
    }

    #[test]
    fn zero_continue_rounds_stops_at_the_cap() {
        let mut cfg = test_cfg();
        cfg.max_steps = 25;
        cfg.max_continue_rounds = 0;
        assert_eq!(step_budget(25, &cfg, 0), StepBudget::Stop { total: 25 });
    }

    #[test]
    fn elapsed_is_human_readable() {
        use std::time::Duration;
        assert_eq!(human_elapsed(Duration::from_secs(9)), "9 秒");
        assert_eq!(human_elapsed(Duration::from_secs(80)), "1 分 20 秒");
        // 长任务就是这个量级，得让人一眼看懂
        assert_eq!(
            human_elapsed(Duration::from_secs(3 * 3600 + 25 * 60)),
            "3 小时 25 分"
        );
    }

    #[test]
    fn switching_mode_mid_turn_stops_asking() {
        // 这条锁的是一个真实的用户路径：任务跑一半，用户把下拉切到
        // 「完全访问」，后面那些命令就不该再弹确认。以前模式是提交时的
        // 快照，切了也还是问 —— 用户连着点了十几次"批准"。
        let store = Store::in_memory().unwrap();
        let mut cfg = test_cfg();
        cfg.mode = Mode::Confirm;
        let cell = zex_core::ModeCell::new(Mode::Confirm);
        let agent = Agent::new(cfg, vec![], store)
            .unwrap()
            .with_mode_cell(cell.clone());

        let args = serde_json::json!({"command": "ls"});
        assert!(
            matches!(agent.judge("shell", &args), Verdict::Ask { .. }),
            "确认模式下跑命令应先问"
        );

        // 用户切了模式 —— 不重启、不重开一轮
        cell.set(Mode::FullAccess);
        assert!(
            matches!(agent.judge("shell", &args), Verdict::Allow { .. }),
            "切到完全访问后，同一轮里的下一句命令必须直接放行"
        );
    }

    #[test]
    fn full_access_still_asks_for_destructive() {
        let store = Store::in_memory().unwrap();
        let cell = zex_core::ModeCell::new(Mode::FullAccess);
        let agent = Agent::new(test_cfg(), vec![], store)
            .unwrap()
            .with_mode_cell(cell);
        // 认不出的工具按最保守的等级处理，不可逆操作永远要问
        let spec = zex_core::tool::ToolSpec {
            name: "unknown",
            description: "",
            risk: Risk::Destructive,
            input_schema: serde_json::json!({}),
            hint: None,
        };
        assert!(
            matches!(
                agent.judge_for(&spec, &serde_json::json!({})),
                Verdict::Ask { .. }
            ),
            "完全访问也不该自动放行不可逆操作"
        );
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
    fn resume_heals_half_finished_tool_calls() {
        // 复现用户报的那个 400：进程在工具执行到一半时被杀，
        // 库里留着「assistant 有 tool_calls，但没有对应结果」的半截历史。
        // 直接 resume 再发请求会被接口拒掉，所以装载时就得补齐。
        let store = Store::in_memory().unwrap();
        let agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let sid = agent.session_id().clone();
        store
            .append_message(&sid, &Message::user("读一下"), 1)
            .unwrap();
        store
            .append_message(
                &sid,
                &Message::Assistant {
                    content: vec![zex_core::Part::text("我看看")],
                    tool_calls: vec![zex_core::ToolCall {
                        id: zex_core::ToolCallId::new("call-1"),
                        name: "fs_read".into(),
                        arguments: r#"{"path":"a"}"#.into(),
                    }],
                },
                1,
            )
            .unwrap();
        // 结果没来得及写

        let resumed = Agent::resume(test_cfg(), vec![], store.clone(), sid.clone()).unwrap();
        let h = resumed.history();
        assert_eq!(h.len(), 3, "缺的那条结果应该被补上：{h:?}");
        // 补出来的那条要落库，否则每次启动都要再修一遍
        let persisted = store.load_messages(&sid).unwrap();
        assert_eq!(persisted.len(), 3, "库里也该是三条：{persisted:?}");
        assert!(matches!(
            &persisted[2].message,
            Message::Tool { is_error: true, .. }
        ));
        match &h[2] {
            Message::Tool {
                call_id,
                is_error,
                content,
                ..
            } => {
                assert_eq!(call_id.to_string(), "call-1");
                assert!(is_error);
                assert!(content.contains("未执行"), "{content}");
            }
            other => panic!("第三条应该是补出来的工具结果，得到 {other:?}"),
        }
    }

    #[test]
    fn switch_session_swaps_history_and_target() {
        let store = Store::in_memory().unwrap();
        let mut agent = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let a = agent.session_id().clone();
        store
            .append_message(&a, &Message::user("在 A 里"), 1)
            .unwrap();
        agent.history.push(Message::user("在 A 里"));

        // 另起一个会话，写两条
        let other = Agent::new(test_cfg(), vec![], store.clone()).unwrap();
        let b = other.session_id().clone();
        assert_ne!(a, b, "两次 new 应得到不同会话");
        store
            .append_message(&b, &Message::user("在 B 里"), 1)
            .unwrap();
        store
            .append_message(&b, &Message::user("B 的第二条"), 1)
            .unwrap();

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

#[cfg(test)]
mod autotitle_tests {
    use super::*;

    /// 模型回空时不该让会话永远没有标题 —— 用户第一句的截断就是兜底。
    #[test]
    fn empty_model_reply_falls_back_to_first_line() {
        let raw = "";
        let user_text = "帮我分析一下这个项目的功能，看看有没有明显的架构问题";
        let mut title = clean_title(raw);
        assert!(title.is_empty(), "前提：模型没给东西");
        if title.is_empty() {
            title = truncate_chars(user_text.trim(), 20);
        }
        assert!(!title.is_empty());
        // truncate_chars 会补一个省略号
        assert!(title.chars().count() <= 21, "标题要短: {title}");
    }

    #[test]
    fn model_reply_wins_over_fallback() {
        let title = clean_title("Rust语言一句话简介");
        assert_eq!(title, "Rust语言一句话简介");
        assert!(!title.is_empty());
    }
}

/// 因输出截断而重试时，改一版请求再发。
///
/// 两处改动，都是冲着"为什么会撞上限"去的：
///
/// - **关掉思考**：推理内容同样计入 `max_tokens`。开着思考的模型可能把
///   大半预算花在推理上，正文刚开始写就被切断 —— 这正是实测里
///   `fs_write` 只写出半截的原因。
/// - **明确要求分块**：模型的默认行为是"一次写完整个文件"。光说
///   "被截断了"它多半原样再来一遍，得告诉它具体怎么拆。
fn truncation_retry_request(req: &ChatRequest) -> ChatRequest {
    let mut next = req.clone();
    next.thinking = zex_llm::ThinkingEffort::Off;
    let note = "\n\n## 上一次尝试被截断\n\n\
        你上一轮的输出达到了 max_tokens 上限，工具调用只写出了一半，已作废。\n\
        这次请把大块内容拆开：第一次用 `fs_write` 写开头，之后用 `append: true` 续写，\
        每次不要超过 150 行；不要试图一次写完整个文件。\n";
    match next.system.as_mut() {
        Some(sys) => sys.push_str(note),
        None => next.system = Some(note.to_string()),
    }
    next
}
