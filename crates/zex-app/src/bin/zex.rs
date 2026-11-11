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
use gpui_kit::component::text::markdown;
use gpui_kit::component::scroll::ScrollableElement as _;
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

/// 会话列表项。
#[derive(Clone)]
struct SessionItem {
    id: String,
    title: String,
    when: String,
}

/// 主视图。
struct ChatView {
    input: Entity<InputState>,
    messages: Vec<MsgItem>,
    streaming: Option<String>,
    /// 消息区滚动句柄：新内容到达时跟到底部。
    feed_scroll: gpui_kit::ScrollHandle,
    running_tool: Option<String>,
    status: String,
    approval: Option<ApprovalRequest>,
    cmd_tx: mpsc::Sender<UiCommand>,
    broker: Arc<ApprovalBroker>,
    sessions: Vec<SessionItem>,
    active_session: String,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("请输入您的指令，或选择一个快捷操作…"));

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

        ChatView {
            input,
            messages: history,
            streaming: None,
            feed_scroll: gpui_kit::ScrollHandle::new(),
            running_tool: None,
            status: "Ready".into(),
            approval: None,
            cmd_tx,
            broker,
            sessions,
            active_session,
            cwd,
            model,
            usage: Usage::default(),
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.streaming.is_some() {
            return;
        }

        self.input.update(cx, |state, cx| state.set_value("", window, cx));
        self.messages.push(MsgItem { kind: MsgKind::User, text: text.clone() });
        self.streaming = Some(String::new());
        self.status = "思考中…".into();

        let tx = self.cmd_tx.clone();
        cx.spawn(async move |_this, _cx| {
            let _ = tx.send(UiCommand::UserPrompt { text, attachments: vec![] }).await;
        })
        .detach();

        self.feed_scroll.scroll_to_bottom();
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

    fn handle_event(&mut self, ev: CoreEvent, cx: &mut Context<Self>) {
        match ev {
            CoreEvent::TurnStarted { .. } => {
                self.streaming.get_or_insert_with(String::new);
                self.status = "思考中…".into();
            }
            CoreEvent::Token { text, .. } => {
                self.streaming.get_or_insert_with(String::new).push_str(&text);
                self.feed_scroll.scroll_to_bottom();
            }
            CoreEvent::ToolCallBegin { name, arguments, risk, .. } => {
                self.flush_streaming();
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
                self.feed_scroll.scroll_to_bottom();
            }
            CoreEvent::ToolCallEnd { ok, brief, .. } => {
                // take 而不是 clean：结果卡要显示是哪个工具产出的
                let name = self.running_tool.take();
                self.status = "Ready".into();
                self.messages.push(MsgItem {
                    kind: MsgKind::ToolResult { ok, output: brief, name },
                    text: String::new(),
                });
                self.feed_scroll.scroll_to_bottom();
            }
            CoreEvent::ApprovalRequired { req } => {
                self.flush_streaming();
                self.status = "等待确认".into();
                self.approval = Some(req);
            }
            CoreEvent::ContextCompacted { before_tokens, after_tokens, strategy, .. } => {
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: format!("上下文已压缩（{strategy}）：{before_tokens} → {after_tokens} tokens"),
                });
            }
            CoreEvent::TurnEnded { reason, usage, .. } => {
                self.flush_streaming();
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
            CoreEvent::SessionSwitched { session, history } => {
                // 整体替换而不是追加：切换会话是"换一个世界"，
                // 旧消息一条都不该留下。
                self.messages = history_to_items(&history);
                self.active_session = session;
                self.streaming = None;
                self.running_tool = None;
                self.status = "Ready".into();
                self.feed_scroll.scroll_to_bottom();
            }
            CoreEvent::Fatal { message } => {
                self.flush_streaming();
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

    fn flush_streaming(&mut self) {
        if let Some(text) = self.streaming.take() {
            if !text.trim().is_empty() {
                self.messages.push(MsgItem { kind: MsgKind::Assistant, text });
                self.feed_scroll.scroll_to_bottom();
            }
        }
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
    fn render_message(&self, m: &MsgItem) -> AnyElement {
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
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(role_label("ZEX"))
                    // 模型回复是 markdown：代码块、列表、粗体都要按语义渲染，
                    // 而不是把 ** 和 - 原样吐给用户
                    .child(markdown(m.text.clone()).into_any_element())
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div().flex().flex_col().gap_1().px_3();

        for s in &self.sessions {
            let active = s.id == self.active_session;
            // Button 的 on_click 是固有方法；Div 在这个 kit 里没有暴露点击能力，
            // 所以可点行直接用 ghost 按钮。
            let mut btn = Button::new(s.id.clone())
                .ghost()
                .w_full()
                .label(format!("{}  ·  {}", s.title, s.when));

            if active {
                btn = btn.toggled(true);
            } else {
                let id = s.id.clone();
                let tx = self.cmd_tx.clone();
                btn = btn.on_click(cx.listener(move |this, _ev: &ClickEvent, _window, cx| {
                    let id = id.clone();
                    let tx = tx.clone();
                    this.status = "切换中…".into();
                    cx.spawn(async move |_this, _cx| {
                        let _ = tx
                            .send(UiCommand::SwitchSession {
                                session: zex_core::SessionId::from(id),
                            })
                            .await;
                    })
                    .detach();
                    cx.notify();
                }));
            }

            list = list.child(btn);
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
                    .child(
                        Button::new("send")
                            .primary()
                            .label("↑")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit(window, cx)
                            })),
                    ),
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(format!(
                                "{:.1}k / {}",
                                self.usage.input_tokens as f64 / 1000.0,
                                self.usage.output_tokens
                            )),
                    ),
            );

        // ─── 消息流 ───
        let mut feed = div().flex().flex_col().gap_6().px_8().py_8();
        for m in &self.messages {
            feed = feed.child(self.render_message(m));
        }
        if let Some(s) = &self.streaming {
            if s.is_empty() {
                // 请求已发出但还没吐字：界面上必须有东西，
                // 否则用户看到的是一个静默的空白，会以为卡死
                feed = feed.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(role_label("ZEX"))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().text_lg().text_color(rgb(TEXT_DIM)).child("思考中"))
                                .child(
                                    div()
                                        .size(px(6.0))
                                        .rounded_full()
                                        .bg(rgb(BRAND)),
                                ),
                        ),
                );
            } else {
                feed = feed.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(role_label("ZEX"))
                        .child(
                            div()
                                .flex()
                                .items_end()
                                .gap_1()
                                .child(div().text_lg().text_color(rgb(TEXT)).child(s.clone()))
                                // 流式光标
                                .child(
                                    div()
                                        .w(px(3.0))
                                        .h(px(18.0))
                                        .bg(rgb(BRAND))
                                        .mb(px(3.0)),
                                ),
                        ),
                );
            }
        }

        let feed_scroll = div()
            .flex_1()
            .min_h(px(0.0))
            .bg(rgb(BG_MAIN))
            .overflow_y_scrollbar()
            .child(feed);

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
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(feed_scroll)
                    .child(bottom),
            )
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
fn load_sessions(store: &zex_store::Store, active: &str) -> Vec<SessionItem> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    store
        .list_sessions(20)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let delta = now - s.updated_at;
            let when = if delta < 3600 {
                format!("{} 分钟前", (delta / 60).max(1))
            } else if delta < 86_400 {
                format!("{} 小时前", delta / 3600)
            } else if delta < 172_800 {
                "昨天".to_string()
            } else {
                // 简化：只显示天数差，避免引入时间格式化依赖
                format!("{} 天前", delta / 86_400)
            };
            let title = if s.title.trim().is_empty() {
                // session id 形如 ses_<uuidv7>，前缀是时间戳所以彼此相同；
                // 取尾部 6 位才能区分
                let raw = s.id.as_str();
                let tail = if raw.len() > 6 { &raw[raw.len() - 6..] } else { raw };
                format!("会话 {tail}")
            } else {
                s.title
            };
            SessionItem { id: s.id.as_str().to_string(), title, when }
        })
        .map(|mut s| {
            s.id = if s.id == active { active.to_string() } else { s.id };
            s
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
    let sessions = load_sessions(agent.store(), &active_session);
    let model = cfg.model.clone();
    let cwd_str = cwd.display().to_string();
    let history = history_to_items(agent.history());
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
                cx.new(|cx| ChatView::new(tx, broker, sessions, active, cwd, model, history, window, cx))
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


