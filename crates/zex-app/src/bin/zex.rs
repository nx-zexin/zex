//! Zex GUI 二进制。
//!
//! 视觉按参考设计稿实现：浅色主题、橙色品牌色、左侧会话栏、
//! 文档流消息、独立工具调用卡片、内嵌审批条。
//!
//! 双世界桥接的 UI 侧（ADR-002）：gpui 主线程只做渲染与输入，
//! 内核在独立 tokio 线程里跑，两者用 channel 通信。

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::text::markdown;
use gpui_kit::*;

use tokio::sync::mpsc;
use zex_agent::ApprovalBroker;
use zex_core::{ApprovalId, ApprovalRequest, CoreEvent, RememberScope, UiCommand, Usage};

// ─── 配色（浅色主题）───
// 取自设计稿：近白背景、极浅灰面板、橙色作为唯一强调色。
const BG_SIDEBAR: u32 = 0xF8F8F6;
const BG_MAIN: u32 = 0xFFFFFF;
const BG_HOVER: u32 = 0xEFEFEC;
const BG_TOOL: u32 = 0xF5F5F3;
const BG_RESULT: u32 = 0xFAFAF8;
const BG_INPUT: u32 = 0xFFFFFF;
const BORDER: u32 = 0xE5E5E2;
const BORDER_SOFT: u32 = 0xEDEDEA;
const TEXT: u32 = 0x1A1A1A;
const TEXT_MUTED: u32 = 0x8C8C88;
const TEXT_DIM: u32 = 0xA8A8A4;
const BRAND: u32 = 0xF59E0B;
const BRAND_SOFT: u32 = 0xFEF3E2;
const BRAND_BORDER: u32 = 0xF5C97A;
const OK: u32 = 0x22C55E;
const OK_SOFT: u32 = 0xDCFCE7;
const DANGER: u32 = 0xE5534B;
const DANGER_SOFT: u32 = 0xFDECEA;
const MONO: &str = "JetBrains Mono";

/// 一条展示用的消息。
#[derive(Clone)]
enum MsgKind {
    User,
    Assistant,
    ToolCall { name: String, args: String, risk: String },
    ToolResult {
        ok: bool,
        output: String,
        /// 产出这条结果的工具名。历史消息里可能缺失（老数据没存）。
        name: Option<String>,
    },
    System,
}

#[derive(Clone)]
struct MsgItem {
    kind: MsgKind,
    text: String,
}

/// 主区域显示什么。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// 工作台：离开期间发生了什么。
    Dashboard,
    /// 对话。
    Chat,
}

/// 会话列表项。
#[derive(Clone)]
struct SessionItem {
    id: String,
    title: String,
}

/// 主视图。
struct ChatView {
    input: Entity<InputState>,
    messages: Vec<MsgItem>,
    streaming: Option<String>,
    /// 任务快照（工作台展示用）。
    tasks: Vec<zex_store::tasks::TaskRecord>,
    /// 当前显示的是工作台还是对话。
    mode: ViewMode,
    /// 重命名会话用的输入框（侧栏内联编辑）。
    rename_input: Entity<InputState>,
    /// 正在就地重命名的会话 id（`None` 表示没有）。
    ///
    /// 用 id 而不是 bool：菜单可以对任意一行发起重命名，
    /// 而不只是当前会话。
    renaming: Option<String>,
    /// 消息区的虚拟列表状态。
    ///
    /// 之前用手写的 `ScrollHandle` + `overflow_y_scrollbar`，但实测
    /// `bounds` 与 `max_offset` 恒为 0（容器没拿到尺寸），滚动定位
    /// 从未生效过。改用组件库的 `MessageScroller`：它内置
    /// `FollowMode::Tail`，新内容到达时自动贴底，切会话时 `reset`
    /// 即可，且是虚拟列表，几百条消息也不会卡。
    scroller: Entity<MessageScrollerState>,
    running_tool: Option<String>,
    status: String,
    approval: Option<ApprovalRequest>,
    cmd_tx: mpsc::Sender<UiCommand>,
    broker: Arc<ApprovalBroker>,
    sessions: Vec<SessionItem>,
    active_session: String,
    /// 子智能体团队。非空时在对话里显示一块团队面板。
    team: Vec<zex_core::event::SubAgentView>,
    /// 别的会话发来的授权请求。切到那个会话时再摆出来，
    /// 免得后台任务把当前视图顶掉。
    pending_approvals: Vec<(String, ApprovalRequest)>,
    /// 用来刷新待办列表。后台会话跑完时当前视图不动，但要更新任务。
    store: Arc<zex_store::Store>,
    cwd: String,
    model: String,
    usage: Usage,
}

impl ChatView {
    fn new(
        cmd_tx: mpsc::Sender<UiCommand>,
        broker: Arc<ApprovalBroker>,
        sessions: Vec<SessionItem>,
        active_session: String,
        cwd: String,
        model: String,
        history: Vec<MsgItem>,
        tasks: Vec<zex_store::tasks::TaskRecord>,
        store: Arc<zex_store::Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("请输入您的指令，或选择一个快捷操作…"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));

        // Enter 发送，Shift+Enter 换行。输入框自己无法区分这两者的语义，
        // 由外层决定"按下回车意味着提交"。
        cx.subscribe_in(&input, window, |this, _input, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift, .. } = ev {
                if !shift {
                    this.submit(window, cx);
                }
            }
        })
        .detach();

        // 重命名输入框里按回车等于点"确定"
        cx.subscribe_in(
            &rename_input,
            window,
            |this, _input, ev: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.commit_rename(cx);
                }
            },
        )
        .detach();

        // 调试用：启动后自动发一条，方便在没有人手的情况下验证流式、
        // 停止按钮、错误提示这些只在"跑起来"时才出现的界面。
        if let Ok(prompt) = std::env::var("ZEX_DEBUG_PROMPT") {
            if !prompt.trim().is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1200))
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        this.input
                            .update(cx, |st, cx| st.set_value(prompt, window, cx));
                        this.submit(window, cx);
                    })
                    .ok();
                })
                .detach();
            }
        }

        let scroller = cx.new(|cx| MessageScrollerState::new(history.len(), cx));

        // 打开就先看工作台：用户想知道「我不在的时候发生了什么」
        let mode = ViewMode::Dashboard;

        ChatView {
            input,
            tasks,
            mode,
            messages: history,
            streaming: None,
            scroller,
            rename_input,
            renaming: None,
            running_tool: None,
            status: "Ready".into(),
            approval: None,
            cmd_tx,
            broker,
            sessions,
            active_session,
            team: Vec::new(),
            pending_approvals: Vec::new(),
            store,
            cwd,
            model,
            usage: Usage::default(),
        }
    }

    /// 就地重命名：把输入框里的标题发给内核。
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.renaming.take() else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_string();
        if !title.is_empty() {
            if let Ok(_) = self
                .cmd_tx
                .try_send(UiCommand::RenameSession {
                    session: zex_core::SessionId::from(session),
                    title,
                })
            {
                // 发出去了，等 SessionTitled 事件回来更新侧栏
            }
        }
        cx.notify();
    }

    /// 渲染虚拟列表的第 `ix` 行。
    ///
    /// `messages.len()` 那一行留给流式中的回复 —— 这样"正在输出"和
    /// "已落定的消息"共用同一套跟随逻辑，不必单独处理滚动。
    fn render_row(&self, ix: usize, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if ix < self.messages.len() {
            self.render_message(&self.messages[ix], ix, cx)
        } else {
            self.render_streaming(cx)
        }
    }

    /// 正在流式输出的那条回复。
    fn render_streaming(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(text) = self.streaming.as_ref() else {
            return div().into_any_element();
        };

        if text.is_empty() {
            // 请求已发出但还没吐字：界面上必须有东西，
            // 否则用户看到的是一个静默的空白，会以为卡死
            return div()
                .flex()
                .flex_col()
                .gap_2()
                .px_8()
                .py_3()
                .child(role_label("ZEX"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_lg().text_color(rgb(TEXT_DIM)).child("思考中"))
                        .child(div().size(px(6.0)).rounded_full().bg(rgb(BRAND))),
                )
                .into_any_element();
        }

        let content = markdown(text.clone()).into_any_element();
        let _ = cx;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_8()
            .py_3()
            .child(role_label("ZEX"))
            .child(content)
            .into_any_element()
    }

    /// 让虚拟列表的行数与当前实际内容对齐。
    ///
    /// 只在内容真的变化时动它，避免每帧 reset 把用户滚到一半的位置顶掉。
    fn sync_scroller(&mut self, cx: &mut Context<Self>) {
        let target = self.messages.len() + usize::from(self.streaming.is_some());
        let current = self.scroller.read(cx).item_count();
        if target == current {
            return;
        }
        if target > current {
            self.scroller.update(cx, |s, cx| {
                s.append(target - current, cx);
            });
        } else {
            self.scroller.update(cx, |s, cx| {
                s.reset(target, cx);
            });
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.streaming.is_some() {
            return;
        }

        self.input.update(cx, |state, cx| state.set_value("", window, cx));
        // 派了活就要看到它跑：从工作台自动进入对话视图
        self.mode = ViewMode::Chat;
        self.messages.push(MsgItem { kind: MsgKind::User, text: text.clone() });
        self.streaming = Some(String::new());
        self.status = "思考中…".into();

        let tx = self.cmd_tx.clone();
        let session = self.active_session.clone();
        cx.spawn(async move |_this, _cx| {
            let _ = tx
                .send(UiCommand::UserPrompt {
                    session: zex_core::SessionId::from(session),
                    text,
                    attachments: vec![],
                })
                .await;
        })
        .detach();

        self.sync_scroller(cx);
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        let tx = self.cmd_tx.clone();
        self.status = "正在停止…".into();
        cx.spawn(async move |_this, _cx| {
            let _ = tx
                .send(UiCommand::Cancel {
                    turn: zex_core::TurnId::generate(),
                })
                .await;
        })
        .detach();
        cx.notify();
    }

    fn respond(&mut self, approve: bool, remember: bool, cx: &mut Context<Self>) {
        let Some(req) = self.approval.take() else { return };
        let broker = self.broker.clone();
        let id: ApprovalId = req.id.clone();
        let grantable = req.grantable;

        self.status = if approve { "执行中…".into() } else { "已拒绝".into() };

        cx.spawn(async move |_this, _cx| {
            if approve {
                let scope = if remember && grantable {
                    Some(RememberScope::Pattern { pattern: None, days: None })
                } else {
                    None
                };
                let _ = broker
                    .resolve(&id, zex_agent::ApprovalDecision::Approve { remember: scope })
                    .await;
            } else {
                let _ = broker
                    .resolve(
                        &id,
                        zex_agent::ApprovalDecision::Reject { reason: "用户在界面上拒绝".into() },
                    )
                    .await;
            }
        })
        .detach();

        cx.notify();
    }

    /// 这个事件是否属于当前正在看的会话。
    ///
    /// 并行执行时内核会同时产出多个会话的事件；不属于当前会话的
    /// 一律不往视图里放，否则两个任务的内容会串在一起。
    fn is_active(&self, session: &zex_core::SessionId) -> bool {
        session.as_str() == self.active_session
    }

    fn handle_event(&mut self, ev: CoreEvent, cx: &mut Context<Self>) {
        match ev {
            CoreEvent::TeamUpdated { agents } => {
                self.team = agents;
                cx.notify();
            }
            CoreEvent::TurnStarted { session, turn: _ } => {
                if !self.is_active(&session) {
                    self.status = "其它会话在后台执行".into();
                    return;
                }
                self.streaming.get_or_insert_with(String::new);
                self.status = "思考中…".into();
            }
            CoreEvent::Token { session, turn: _, text } => {
                if self.is_active(&session) {
                    self.streaming.get_or_insert_with(String::new).push_str(&text);
                    self.sync_scroller(cx);
                }
            }
            CoreEvent::ToolCallBegin { session, name, arguments, risk, .. } => {
                if !self.is_active(&session) {
                    // 后台会话的工具调用不上台面，但它确实在跑
                    self.status = format!("其它会话执行 {name}");
                } else {
                self.flush_streaming(cx);
                self.running_tool = Some(name.clone());
                self.status = format!("执行 {name}");
                self.messages.push(MsgItem {
                    kind: MsgKind::ToolCall {
                        name: name.clone(),
                        args: compact_args(&arguments),
                        risk: risk.label().to_string(),
                    },
                    text: String::new(),
                });
                self.sync_scroller(cx);
                }
            }
            CoreEvent::ToolCallEnd { session, ok, brief, .. } => {
                if !self.is_active(&session) {
                    return;
                }
                // take 而不是 clean：结果卡要显示是哪个工具产出的
                let name = self.running_tool.take();
                self.status = "Ready".into();
                self.messages.push(MsgItem {
                    kind: MsgKind::ToolResult { ok, output: brief, name },
                    text: String::new(),
                });
                self.sync_scroller(cx);
            }
            CoreEvent::ApprovalRequired { session, req } => {
                // 别的会话要授权时不能顶掉当前视图：放队列里，切过去再看。
                if !self.is_active(&session) {
                    self.pending_approvals.push((session.to_string(), req));
                    return;
                }
                self.flush_streaming(cx);
                self.status = "等待确认".into();
                self.approval = Some(req);
            }
            CoreEvent::ContextCompacted { session, before_tokens, after_tokens, strategy, .. } => {
                if !self.is_active(&session) {
                    return;
                }
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: format!("上下文已压缩（{strategy}）：{before_tokens} → {after_tokens} tokens"),
                });
            }
            CoreEvent::TurnEnded { session, reason, usage, turn: _ } => {
                if !self.is_active(&session) {
                    // 后台会话跑完只是刷新待办，不动当前视图
                    self.tasks = load_tasks(&self.store);
                    return;
                }
                self.flush_streaming(cx);
                // 取消/失败路径不会有 flush 收尾，这里兜住，避免输入框锁死
                self.streaming = None;
                self.running_tool = None;
                self.usage.add(usage);
                self.status = "Ready".into();
                if !matches!(reason, zex_core::EndReason::Completed) {
                    self.messages.push(MsgItem {
                        kind: MsgKind::System,
                        text: format!("本轮结束：{reason:?}"),
                    });
                }
            }
            CoreEvent::Warning { message } => {
                self.messages.push(MsgItem { kind: MsgKind::System, text: message });
            }
            CoreEvent::SessionTitled { session, title } => {
                // 侧栏和当前标题一起更新：这个事件既可能来自自动起名，
                // 也可能来自用户手动重命名
                if let Some(item) = self.sessions.iter_mut().find(|s| s.id == session) {
                    item.title = title.clone();
                }
                self.status = "Ready".into();
            }
            CoreEvent::SessionSwitched {
                session,
                history,
                sessions,
            } => {
                // 整体替换而不是追加：切换会话是"换一个世界"，
                // 旧消息一条都不该留下。
                self.messages = history_to_items(&history);
                self.active_session = session.clone();
                self.sessions = summaries_to_items(&sessions);
                // 这个会话之前在后台等过授权？现在摆出来。
                if let Some(pos) = self
                    .pending_approvals
                    .iter()
                    .position(|(s, _)| *s == session)
                {
                    let (_, req) = self.pending_approvals.remove(pos);
                    self.approval = Some(req);
                }
                // 切了会话就是要聊，从工作台进对话
                self.mode = ViewMode::Chat;
                self.streaming = None;
                self.running_tool = None;
                self.status = "Ready".into();
                self.sync_scroller(cx);
            }
            CoreEvent::Fatal { message } => {
                self.flush_streaming(cx);
                // 必须清掉流式标记：submit() 用它判断"是否正在跑"，
                // 不清就会永久锁死输入框，出错后一句话都发不出去
                self.streaming = None;
                self.running_tool = None;
                self.status = "出错".into();
                self.messages.push(MsgItem { kind: MsgKind::System, text: format!("错误：{message}") });
            }
            CoreEvent::Thinking { .. } => {}
        }
        cx.notify();
    }

    fn flush_streaming(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.streaming.take() {
            if !text.trim().is_empty() {
                self.messages.push(MsgItem { kind: MsgKind::Assistant, text });
            }
        }
        self.sync_scroller(cx);
    }
}

// ─── 渲染辅助 ───

fn role_label(text: &str) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(BRAND))
        .child(text.to_string())
}

fn pill(label: String, bg: u32, fg: u32) -> impl IntoElement {
    div()
        .px_2()
        .py(px(2.0))
        .rounded_full()
        .bg(rgb(bg))
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .child(label)
}

impl ChatView {
    fn render_message(&self, m: &MsgItem, idx: usize, cx: &mut Context<Self>) -> AnyElement {
        match &m.kind {
            MsgKind::User => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(role_label("USER"))
                .child(div().text_lg().text_color(rgb(TEXT)).child(m.text.clone()))
                .into_any_element(),

            MsgKind::Assistant => {
                // 只有工具调用、没有说话内容的轮次会留下空 assistant 消息，
                // 渲染出来就是个光秃秃的 "ZEX" 标签，白占一行
                if m.text.trim().is_empty() {
                    return div().into_any_element();
                }
                let text = m.text.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(role_label("ZEX"))
                    // 模型回复是 markdown：代码块、列表、粗体都要按语义渲染，
                    // 而不是把 ** 和 - 原样吐给用户
                    .child(markdown(m.text.clone()).into_any_element())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new(ElementId::Name(format!("copy-{idx}").into()))
                                    .ghost()
                                    .compact()
                                    .label("复制")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                        // 复制原文而非渲染结果：贴到别处还能保留 markdown
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            text.clone(),
                                        ));
                                        this.status = "已复制".into();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element()
            }

            MsgKind::ToolCall { name, args, risk } => {
                // 风险等级决定胶囊配色
                let (bg, fg) = match risk.as_str() {
                    "高危" => (DANGER_SOFT, DANGER),
                    "有影响" => (BRAND_SOFT, BRAND),
                    _ => (BG_HOVER, TEXT_MUTED),
                };
                let head = div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(BRAND))
                                    .child("TOOL CALL"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(MONO)
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(name.clone()),
                            ),
                    );
                // 历史消息没有风险等级，此时不画空胶囊
                let head = if risk.is_empty() {
                    head
                } else {
                    head.child(pill(risk.clone(), bg, fg))
                };

                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(BG_TOOL))
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .child(head)
                    .child(
                        div()
                            .text_sm()
                            .font_family(MONO)
                            .text_color(rgb(TEXT))
                            .child(args.clone()),
                    )
                    .into_any_element()
            }

            MsgKind::ToolResult { ok, output, name } => {
                let (dot_bg, mark) = if *ok { (OK_SOFT, "✓") } else { (DANGER_SOFT, "✕") };
                let mark_color = if *ok { OK } else { DANGER };
                // 输出可能很长（日志、文件内容），先截断避免一屏全是工具输出
                let shown = truncate_for_display(output, 1200);
                let head_label = match name {
                    Some(n) if !n.is_empty() => format!("TOOL RESULT · {n}"),
                    _ => "TOOL RESULT".to_string(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(BG_RESULT))
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(head_label),
                            )
                            .child(
                                div()
                                    .size(px(16.0))
                                    .rounded_full()
                                    .bg(rgb(dot_bg))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(mark_color))
                                            .child(mark),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_family(MONO)
                            .text_color(rgb(TEXT_MUTED))
                            .child(shown),
                    )
                    .into_any_element()
            }

            MsgKind::System => div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_full()
                        .bg(rgb(BG_TOOL))
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(m.text.clone()),
                )
                .into_any_element(),
        }
    }

    /// 工作台：离开期间发生了什么。
    fn render_dashboard(&self, cx: &mut Context<Self>) -> AnyElement {
        use zex_store::tasks::TaskStatus;

        let running: Vec<_> = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued))
            .collect();
        let failed: Vec<_> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .collect();
        let finished: Vec<_> = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Completed | TaskStatus::Cancelled))
            .take(8)
            .collect();

        let mut col = div()
            .flex()
            .flex_col()
            .gap_6()
            .px_10()
            .py_8()
            .w_full();

        // ─── 标题 ───
        let headline = if running.is_empty() && failed.is_empty() && finished.is_empty() {
            "还没有任务。给它派点活试试。".to_string()
        } else {
            let mut parts = Vec::new();
            if !running.is_empty() {
                parts.push(format!("{} 个在跑", running.len()));
            }
            if !finished.is_empty() {
                parts.push(format!("{} 个已完成", finished.len()));
            }
            if !failed.is_empty() {
                parts.push(format!("{} 个需要处理", failed.len()));
            }
            parts.join(" · ")
        };

        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(TEXT))
                        .child("工作台"),
                )
                .child(div().text_sm().text_color(rgb(TEXT_MUTED)).child(headline)),
        );

        // ─── 新任务输入 ───
        // 放在最上方：打开工作台的第一件事通常是派活，
        // 而不是先翻旧任务。
        col = col.child(self.render_composer(cx));

        // ─── 需要处理（失败） ───
        if !failed.is_empty() {
            col = col.child(self.render_task_section("需要处理", failed.clone(), cx));
        }

        // ─── 运行中 ───
        if !running.is_empty() {
            col = col.child(self.render_task_section("正在执行", running.clone(), cx));
        }

        // ─── 最近完成 ───
        if !finished.is_empty() {
            col = col.child(self.render_task_section("最近完成", finished.clone(), cx));
        }

        // ─── 空状态引导 ───
        if running.is_empty() && failed.is_empty() && finished.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(BG_TOOL))
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT_MUTED))
                            .child("你可以让它读文件、跑命令、查资料。比如："),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child("「分析一下这个项目的依赖」「把 /tmp 里的日志整理成要点」"),
                    ),
            );
        }

        col.into_any_element()
    }

    /// 一组任务卡片。
    fn render_task_section(
        &self,
        title: &str,
        tasks: Vec<&zex_store::tasks::TaskRecord>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = div().flex().flex_col().gap_2().child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TEXT_MUTED))
                .child(title.to_uppercase()),
        );
        for t in tasks {
            section = section.child(self.render_task_card(t, cx));
        }
        section.into_any_element()
    }

    /// 单条任务卡片：点一下跳到它所在的会话。
    /// 团队面板：主对话里直接看到子智能体在干什么。
    ///
    /// 放在对话下方而不是侧栏，是因为它属于「这一轮正在发生的事」；
    /// 侧栏留给会话，两者是不同的层级。
    fn render_team(&self, _cx: &mut Context<Self>) -> AnyElement {
        let running = self.team.iter().filter(|a| a.status == "执行中").count();

        let mut panel = div()
            .flex_none()
            .mx_6()
            .mb_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_RESULT))
            .overflow_hidden();

        // 标题行
        panel = panel.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(rgb(BORDER_SOFT))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("团队"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(if running > 0 {
                            format!("{} 个执行中 · 共 {} 个", running, self.team.len())
                        } else {
                            format!("{} 个", self.team.len())
                        }),
                ),
        );

        for a in &self.team {
            let (dot, dot_bg) = match a.status.as_str() {
                "执行中" => (BRAND, BRAND_SOFT),
                "已完成" => (OK, OK_SOFT),
                "失败" => (DANGER, DANGER_SOFT),
                _ => (TEXT_DIM, BG_HOVER),
            };

            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_2()
                .child(div().size(px(6.0)).rounded_full().bg(rgb(dot)));

            row = row
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(TEXT))
                        .child(a.id.clone()),
                )
                .child(
                    div()
                        .px_1p5()
                        .rounded_sm()
                        .bg(rgb(dot_bg))
                        .text_color(rgb(dot))
                        .text_xs()
                        .child(a.status.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .overflow_hidden()
                        .child(a.description.clone()),
                );

            // 有结果就给一个能点进去看的会话
            if !a.session.is_empty() {
                let session = a.session.clone();
                let tx = self.cmd_tx.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("team-open-{}", a.id)))
                        .ghost()
                        .label("查看")
                        .on_click(move |_, _, _| {
                            // 直接投递即可：切换会话是内核侧的即时动作，
                            // 不需要在这里等一个往返。
                            let _ = tx.try_send(UiCommand::SwitchSession {
                                session: zex_core::SessionId::from(session.clone()),
                            });
                        }),
                );
            }

            panel = panel.child(row);
        }

        panel.into_any_element()
    }

    fn render_task_card(
        &self,
        t: &zex_store::tasks::TaskRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use zex_store::tasks::TaskStatus;

        let (dot, dot_bg) = match t.status {
            TaskStatus::Running => (BRAND, BRAND_SOFT),
            TaskStatus::Queued => (TEXT_MUTED, BG_HOVER),
            TaskStatus::Completed => (OK, OK_SOFT),
            TaskStatus::Failed => (DANGER, DANGER_SOFT),
            TaskStatus::Cancelled => (TEXT_DIM, BG_HOVER),
        };

        // 标题优先用任务自带的，没有就截 prompt 首行
        let label = if !t.title.trim().is_empty() {
            t.title.clone()
        } else {
            let first = t.prompt.lines().next().unwrap_or("").trim();
            let s: String = first.chars().take(40).collect();
            if s.is_empty() { "（空输入）".into() } else { s }
        };

        let detail = match t.status {
            TaskStatus::Failed => t.error.clone().unwrap_or_default(),
            _ => t.summary.clone().unwrap_or_default(),
        };
        let detail: String = detail.chars().take(80).collect();

        let when = relative_time(t.ended_at.unwrap_or(t.created_at));

        let sid = t.session_id.clone();
        let tx = self.cmd_tx.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_4()
            .rounded_lg()
            .bg(rgb(BG_TOOL))
            .border_1()
            .border_color(rgb(BORDER_SOFT))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    // 跳到那条任务所属的会话
                    this.mode = ViewMode::Chat;
                    let _ = tx.try_send(UiCommand::SwitchSession {
                        session: zex_core::SessionId::from(sid.clone()),
                    });
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(6.0)).rounded_full().bg(rgb(dot)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_2()
                            .py(px(1.0))
                            .rounded_full()
                            .bg(rgb(dot_bg))
                            .text_xs()
                            .text_color(rgb(dot))
                            .child(match t.status {
                                TaskStatus::Running => "运行中",
                                TaskStatus::Queued => "排队",
                                TaskStatus::Completed => "完成",
                                TaskStatus::Failed => "失败",
                                TaskStatus::Cancelled => "已取消",
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(when),
                    ),
            );

        if !detail.trim().is_empty() {
            card = card.child(
                div()
                    .pl(px(14.0))
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(detail),
            );
        }
        card.into_any_element()
    }

    /// 单行会话的操作菜单（重命名 / 清空 / 删除）。
    fn render_item_menu(&self, sid: &str, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let tx = self.cmd_tx.clone();
        let sid_owned = sid.to_string();

        // 直接用 Button 的 dropdown_menu：DropdownButton 会额外渲染一个
        // 带箭头的半按钮，在 240px 的侧栏里占掉两倍宽度。
        Button::new(ElementId::Name(format!("menu-{sid_owned}").into()))
            .ghost()
            .label("⋯")
            .dropdown_menu_with_anchor(Anchor::BottomLeft, {
                let sid = sid_owned.clone();
                let view = view.clone();
                let tx = tx.clone();
                move |menu, _window, _cx| {
                    let (sid_rename, view_rename) = (sid.clone(), view.clone());
                    let (sid_clear, tx_clear) = (sid.clone(), tx.clone());
                    let (sid_del, tx_del) = (sid.clone(), tx.clone());

                    menu.item(
                        PopupMenuItem::new("重命名").on_click(move |_, window, cx| {
                            let sid = sid_rename.clone();
                            view_rename
                                .update(cx, |this, cx| {
                                    // 预填当前标题，改起来省事
                                    let current = this
                                        .sessions
                                        .iter()
                                        .find(|s| s.id == sid)
                                        .map(|s| s.title.clone())
                                        .unwrap_or_default();
                                    this.rename_input
                                        .update(cx, |st, cx| st.set_value(current, window, cx));
                                    this.renaming = Some(sid.clone());
                                    cx.notify();
                                })
                                .ok();
                        }),
                    )
                    .separator()
                    .item(PopupMenuItem::new("清空会话").on_click(move |_, _w, _cx| {
                        let _ = tx_clear.try_send(UiCommand::ClearSession {
                            session: zex_core::SessionId::from(sid_clear.clone()),
                        });
                    }))
                    .item(PopupMenuItem::new("删除会话").on_click(move |_, _w, _cx| {
                        let _ = tx_del.try_send(UiCommand::DeleteSession {
                            session: zex_core::SessionId::from(sid_del.clone()),
                        });
                    }))
                }
            })
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div().flex().flex_col().gap_1().px_3();

        for s in &self.sessions {
            let active = s.id == self.active_session;
            let is_renaming = self.renaming.as_deref() == Some(s.id.as_str());

            // 每行一个独立的 group：同名 group 会让 hover 一行点亮全部行
            let group = format!("item-{}", s.id);
            let mut row = div()
                .w_full()
                .h(px(34.0))
                .px_2()
                .flex()
                .items_center()
                .gap_1()
                .rounded(px(6.0))
                .relative()
                .group(group.clone());

            if active {
                row = row.bg(rgb(BG_HOVER));
            }

            if is_renaming {
                // 就地重命名：复用这一行的位置，不弹窗
                row = row
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(Input::new(&self.rename_input)),
                    )
                    .child(
                        Button::new("rename-ok")
                            .primary()
                            .label("✓")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.commit_rename(cx)
                            })),
                    );
            } else {
                let color = if active { TEXT } else { TEXT_MUTED };
                let mut title = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_sm()
                    .text_color(rgb(color))
                    .child(s.title.clone());

                if !active {
                    let id = s.id.clone();
                    let tx = self.cmd_tx.clone();
                    title = title.cursor_pointer().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            let _ = tx.try_send(UiCommand::SwitchSession {
                                session: zex_core::SessionId::from(id.clone()),
                            });
                            this.status = "切换中…".into();
                            cx.notify();
                        }),
                    );
                }

                // 只显示标题，不再显示时间；⋯ 绝对定位在右端，
                // 平时不可见，hover 时才浮出来 —— 不占布局宽度，
                // 标题能用满整行（侧栏只有 240px）。
                let g_menu = group.clone();
                row = row
                    .child(title.pr(px(20.0)))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .invisible()
                            .group_hover(g_menu, |st| st.visible())
                            .child(self.render_item_menu(&s.id, cx)),
                    );
            }

            list = list.child(row);
        }

        div()
            .w(px(240.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(BG_SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            // 品牌区
            .child(
                div().flex().items_center().gap_2().px_4().pt_4().pb_3().child(
                    div()
                        .size(px(26.0))
                        .rounded(px(7.0))
                        .bg(rgb(BRAND))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(0xFFFFFF))
                                .child("Z"),
                        ),
                ).child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(TEXT))
                        .child("Zex"),
                ),
            )
            // 新建会话
            .child(
                div().px_3().pb_3().child(
                    div()
                        .w_full()
                        .h(px(38.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG_MAIN))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(div().text_base().child("＋"))
                        .child("新建会话")
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _window, cx| {
                            let tx = this.cmd_tx.clone();
                            this.status = "新建中…".into();
                            cx.spawn(async move |_this, _cx| {
                                let _ = tx.send(UiCommand::NewSession).await;
                            })
                            .detach();
                            cx.notify();
                        })),
                ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(list))
            // 底部图标
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(BORDER_SOFT))
                    .child(
                        div().text_lg().text_color(rgb(TEXT_MUTED)).child("⚙"),
                    )
                    .child(
                        div().text_lg().text_color(rgb(TEXT_MUTED)).child("？"),
                    ),
            )
            .into_any_element()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .rounded(px(24.0))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(BG_INPUT))
                            .px_4()
                            .py_2()
                            .child(Input::new(&self.input)),
                    )
                    .child(match self.streaming.is_some() {
                        // 正在跑就换成停止键：否则用户只能干等长任务跑完
                        true => Button::new("stop")
                            .danger()
                            .label("■")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.cancel(cx)
                            }))
                            .into_any_element(),
                        false => Button::new("send")
                            .primary()
                            .label("↑")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit(window, cx)
                            }))
                            .into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(format!("{} ⌄", self.model)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(format!(
                                "{} / {} tokens",
                                self.usage.input_tokens, self.usage.output_tokens
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(req) = &self.approval else {
            return div().into_any_element();
        };

        let mut actions = div().flex().items_center().gap_2();
        actions = actions.child(
            Button::new("approve").primary().label("批准").on_click(cx.listener(
                |this, _: &ClickEvent, _w, cx| this.respond(true, false, cx),
            )),
        );
        actions = actions.child(
            Button::new("reject").label("拒绝").on_click(cx.listener(
                |this, _: &ClickEvent, _w, cx| this.respond(false, false, cx),
            )),
        );
        if req.grantable {
            actions = actions.child(
                Button::new("always").label("批准并记住此类").on_click(cx.listener(
                    |this, _: &ClickEvent, _w, cx| this.respond(true, true, cx),
                )),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .rounded_lg()
            .bg(rgb(BRAND_SOFT))
            .border_1()
            .border_color(rgb(BRAND_BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(rgb(BRAND))
                                    .child("⚠"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(TEXT))
                                    .child("需要确认"),
                            )
                            .child(pill(
                                format!("{} {}", req.risk.label(), "").trim().to_string(),
                                BRAND_SOFT,
                                BRAND,
                            )),
                    )
                    .child(actions),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT_MUTED))
                    .child(req.summary.clone()),
            )
            .child(
                div()
                    .p_3()
                    .rounded(px(8.0))
                    .bg(rgb(0xEFEFEC))
                    .text_sm()
                    .font_family(MONO)
                    .text_color(rgb(TEXT))
                    .child(compact_args(&req.arguments)),
            )
            .into_any_element()
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ─── 顶部状态栏 ───
        let status_dot_color = if self.status == "Ready" { OK } else { BRAND };
        let status_bg = if self.status == "Ready" { OK_SOFT } else { BRAND_SOFT };
        let status_fg = if self.status == "Ready" { OK } else { BRAND };

        let header = div()
            .h(px(52.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px_6()
            .border_b_1()
            .border_color(rgb(BORDER_SOFT))
            .bg(rgb(BG_MAIN))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT_MUTED))
                    .child(self.cwd.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(status_bg))
                            .child(div().size(px(6.0)).rounded_full().bg(rgb(status_dot_color)))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(status_fg))
                                    .child(self.status.clone()),
                            ),
                    )
                    .child({
                        let total = self.usage.input_tokens + self.usage.output_tokens;
                        // 不足一千就给准确数字，"0.0k" 看着像统计坏了
                        let label = match total {
                            0..=999 => format!("{total} tokens"),
                            _ => format!("{:.1}k tokens", total as f64 / 1000.0),
                        };
                        let mut meta = div().flex().items_center().gap_2().child(
                            div().text_sm().text_color(rgb(TEXT)).child(label),
                        );
                        // 对话模式下给一个回工作台的入口
                        if self.mode == ViewMode::Chat {
                            meta = meta.child(
                                Button::new("back-to-dashboard")
                                    .ghost()
                                    .label("工作台")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                        this.mode = ViewMode::Dashboard;
                                        cx.notify();
                                    })),
                            );
                        }
                        meta
                    }),
            );

        // ─── 消息流 ───
        // 交给组件库的虚拟列表：它用 FollowMode::Tail 贴住最新内容，
        // 用户往上翻时不会被强行拽回，翻回底部后自动恢复跟随。
        let weak = cx.entity().downgrade();
        let feed = MessageScroller::new("feed", self.scroller.clone(), move |ix, window, cx| {
            weak.update(cx, |view, cx| view.render_row(ix, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h(px(0.0))
        .bg(rgb(BG_MAIN));

        // ─── 底部区域 ───
        let mut bottom = div().flex().flex_col().gap_3().px_8().pb_5();

        if self.approval.is_some() {
            bottom = bottom.child(self.render_approval(cx));
        }
        bottom = bottom.child(self.render_composer(cx));

        div()
            .size_full()
            .flex()
            .bg(rgb(BG_MAIN))
            .text_color(rgb(TEXT))
            .child(self.render_sidebar(cx))
            .child({
                let mut main = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(match self.mode {
                        ViewMode::Dashboard => self.render_dashboard(cx),
                        ViewMode::Chat => {
                            if self.team.is_empty() {
                                feed.into_any_element()
                            } else {
                                div()
                                    .flex_1()
                                    .min_h(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .child(feed)
                                    .child(self.render_team(cx))
                                    .into_any_element()
                            }
                        }
                    });
                // 工作台模式下不需要输入框：还没决定要干什么
                if self.mode == ViewMode::Chat {
                    main = main.child(bottom);
                }
                main
            })
    }
}

/// 把 JSON 参数压成一行短摘要。
fn compact_args(v: &serde_json::Value) -> String {
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

/// 截断过长的工具输出。
///
/// 保留开头和结尾 —— 结尾通常有错误信息，全砍掉反而最有用的一段没了。
fn truncate_for_display(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit * 2 / 3).collect();
    let tail: String = s.chars().skip(n - limit / 3).collect();
    format!("{head}\n\n… 省略 {} 字符 …\n\n{tail}", n - limit)
}

/// 按平台选择 UI 字体。
fn apply_platform_font(cx: &mut App) {
    let candidate = std::env::var("ZEX_UI_FONT").ok().unwrap_or_else(|| {
        if cfg!(target_os = "linux") {
            "Noto Sans CJK SC".to_string()
        } else if cfg!(target_os = "macos") {
            "PingFang SC".to_string()
        } else {
            "Microsoft YaHei".to_string()
        }
    });
    cx.global_mut::<gpui_kit::component::Theme>().font_family = candidate.into();
}

/// 把持久化的 Message 映射成展示项。
///
/// 带工具调用的 assistant 消息不单独展示 —— 它的内容由随后的
/// ToolResult 承载，重复展示会让历史看起来很吵。
fn history_to_items(msgs: &[zex_core::Message]) -> Vec<MsgItem> {
    use zex_core::Message;
    let mut out = Vec::new();
    for m in msgs {
        match m {
            Message::User { .. } => {
                out.push(MsgItem { kind: MsgKind::User, text: m.text() });
            }
            Message::Assistant { tool_calls, .. } => {
                let t = m.text();
                if !t.trim().is_empty() {
                    out.push(MsgItem { kind: MsgKind::Assistant, text: t });
                }
                // 历史里的工具调用也要还原成卡片，否则回看会话时
                // 只剩一堆结果、看不出中间发生了什么
                for call in tool_calls {
                    out.push(MsgItem {
                        kind: MsgKind::ToolCall {
                            name: call.name.clone(),
                            args: compact_args(&call.arguments),
                            // 历史里没存风险等级，留空即不画胶囊
                            risk: String::new(),
                        },
                        text: String::new(),
                    });
                }
            }
            Message::Tool { content, is_error, name, .. } => {
                out.push(MsgItem {
                    kind: MsgKind::ToolResult {
                        ok: !*is_error,
                        output: content.clone(),
                        name: name.clone(),
                    },
                    text: String::new(),
                });
            }
            Message::System { .. } => {}
        }
    }
    out
}

/// 从数据库读出最近的会话，供侧栏展示。
/// 把内核送来的会话摘要转成侧栏项（含相对时间）。
fn summaries_to_items(list: &[zex_core::event::SessionSummary]) -> Vec<SessionItem> {
    list.iter()
        .map(|s| {
            let title = if s.title.trim().is_empty() {
                let raw = s.id.as_str();
                let tail = if raw.len() > 6 { &raw[raw.len() - 6..] } else { raw };
                format!("会话 {tail}")
            } else {
                s.title.clone()
            };
            SessionItem { id: s.id.clone(), title }
        })
        .collect()
}

/// 把时间戳转成紧凑的相对时间（工作台用）。
fn relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = (now - ts).max(0);
    if d < 60 {
        "刚刚".to_string()
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86_400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86_400)
    }
}

/// 读工作台需要的任务快照：在跑的 + 最近结束的。
fn load_tasks(store: &zex_store::Store) -> Vec<zex_store::tasks::TaskRecord> {
    use zex_store::tasks::TaskStatus;
    let mut all = Vec::new();
    for group in [
        vec![TaskStatus::Running, TaskStatus::Queued],
        vec![TaskStatus::Completed, TaskStatus::Failed, TaskStatus::Cancelled],
    ] {
        if let Ok(mut list) = store.list_tasks_by_status(&group, 20) {
            all.append(&mut list);
        }
    }
    all
}

fn load_sessions(store: &zex_store::Store) -> Vec<SessionItem> {
    store
        .list_sessions(20)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let title = if s.title.trim().is_empty() {
                // session id 形如 ses_<uuidv7>，前缀是时间戳所以彼此相同；
                // 取尾部 6 位才能区分
                let raw = s.id.as_str();
                let tail = if raw.len() > 6 { &raw[raw.len() - 6..] } else { raw };
                format!("会话 {tail}")
            } else {
                s.title
            };
            SessionItem { id: s.id.as_str().to_string(), title }
        })
        .collect()
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cfg = match zex_app::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            std::process::exit(1);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let agent = match zex_app::build_agent_resuming(&cfg, cwd.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("无法初始化内核: {e}");
            std::process::exit(1);
        }
    };

    let broker = agent.approvals();
    let active_session = agent.session_id().as_str().to_string();
    let sessions = load_sessions(agent.store());
    let model = cfg.model.clone();
    let cwd_str = cwd.display().to_string();
    let history = history_to_items(agent.history());
    let tasks = load_tasks(agent.store());
    let store = Arc::new(agent.store().clone());
    let handles = zex_app::spawn_core(agent);
    let cmd_tx = handles.cmd_tx.clone();
    let mut event_rx = handles.event_rx;

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        // 设计稿是浅色主题
        gpui_kit::component::Theme::change(
            gpui_kit::component::ThemeMode::Light,
            None,
            cx,
        );
        apply_platform_font(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(80.0), px(60.0)),
                size(px(1280.0), px(820.0)),
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Zex".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (handle, view) = match gpui_kit::open_window(
            options,
            cx,
            |window, cx| {
                // niri/sway 这类平铺器会忽略 WindowOptions 里的初始尺寸，
                // 这里再显式要一次；被忽略也无害（浮动窗口自行决定）。
                window.resize(size(px(1280.0), px(820.0)));
                let tx = cmd_tx.clone();
                let broker = broker.clone();
                let sessions = sessions.clone();
                let active = active_session.clone();
                let cwd = cwd_str.clone();
                let model = model.clone();
                let history = history.clone();
                let tasks = tasks.clone();
                let store = store.clone();
                cx.new(|cx| {
                    ChatView::new(tx, broker, sessions, active, cwd, model, history, tasks, store, window, cx)
                })
            },
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("无法创建窗口: {e}");
                return;
            }
        };
        let _ = handle;

        let weak = view.downgrade();
        cx.spawn(async move |cx| {
            while let Some(ev) = event_rx.recv().await {
                let alive = weak.update(cx, |this, cx| this.handle_event(ev, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    });
}


