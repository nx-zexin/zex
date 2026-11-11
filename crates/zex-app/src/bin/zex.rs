//! Zex GUI 二进制。
//!
//! 双世界桥接的 UI 侧（ADR-002）：gpui 主线程只做渲染与输入，
//! 内核在独立 tokio 线程里跑，两者用 channel 通信。
//!
//! M1 范围：流式对话 + 审批弹层 + 工具调用展示。
//! Markdown 渲染与消息虚拟滚动留到 M2。

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;
use tokio::sync::mpsc;
use zex_agent::ApprovalBroker;
use zex_app::{build_agent, CoreHandles};
use zex_core::{ApprovalId, ApprovalRequest, CoreEvent, RememberScope, UiCommand};

/// 一条展示用的消息。
#[derive(Clone)]
struct MsgItem {
    /// 角色标签。
    role: &'static str,
    /// 正文。
    text: String,
    /// 是否是错误。
    error: bool,
}

/// 主视图。
struct ChatView {
    input: Entity<InputState>,
    messages: Vec<MsgItem>,
    /// 正在流式输出的内容。
    streaming: Option<String>,
    /// 正在执行的工具。
    running_tool: Option<String>,
    /// 底部状态栏文本。
    status: String,
    /// 待审批请求。
    approval: Option<ApprovalRequest>,
    /// UI → 内核。
    cmd_tx: mpsc::Sender<UiCommand>,
    /// 回复审批用。
    broker: Arc<ApprovalBroker>,
}

impl ChatView {
    fn new(
        cmd_tx: mpsc::Sender<UiCommand>,
        broker: Arc<ApprovalBroker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("给 Zex 下达任务…"));

        ChatView {
            input,
            messages: Vec::new(),
            streaming: None,
            running_tool: None,
            status: "就绪".into(),
            approval: None,
            cmd_tx,
            broker,
        }
    }

    /// 提交输入框里的内容。
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.streaming.is_some() {
            self.status = "上一轮还在进行中".into();
            cx.notify();
            return;
        }

        self.input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.messages.push(MsgItem { role: "你", text: text.clone(), error: false });
        self.streaming = Some(String::new());
        self.status = "思考中…".into();

        let tx = self.cmd_tx.clone();
        cx.spawn(async move |_this, _cx| {
            let _ = tx.send(UiCommand::UserPrompt { text, attachments: vec![] }).await;
        })
        .detach();

        cx.notify();
    }

    /// 回复审批。
    fn respond(&mut self, approve: bool, remember: bool, cx: &mut Context<Self>) {
        let Some(req) = self.approval.take() else {
            return;
        };
        let broker = self.broker.clone();
        let cmd_tx = self.cmd_tx.clone();
        let id: ApprovalId = req.id.clone();
        let grantable = req.grantable;

        self.status = if approve { "已批准，执行中…".into() } else { "已拒绝".into() };

        cx.spawn(async move |_this, _cx| {
            let (cmd, decision_scope) = if approve {
                let scope = if remember && grantable {
                    Some(RememberScope::Pattern { pattern: None, days: None })
                } else {
                    None
                };
                (UiCommand::Approve { id: id.clone(), remember: scope.clone() }, scope)
            } else {
                (UiCommand::Reject { id: id.clone(), reason: "用户在界面上拒绝".into() }, None)
            };
            // 直接交给 broker 更快，也保证 UI 与内核看到同一个决定
            if let UiCommand::Approve { id, remember } = &cmd {
                let _ = broker
                    .resolve(
                        id,
                        zex_agent::ApprovalDecision::Approve { remember: remember.clone() },
                    )
                    .await;
            } else if let UiCommand::Reject { id, reason } = &cmd {
                let _ = broker
                    .resolve(id, zex_agent::ApprovalDecision::Reject { reason: reason.clone() })
                    .await;
            }
            let _ = decision_scope;
            let _ = cmd_tx;
        })
        .detach();

        cx.notify();
    }

    /// 处理一个内核事件。
    fn handle_event(&mut self, ev: CoreEvent, cx: &mut Context<Self>) {
        match ev {
            CoreEvent::TurnStarted { .. } => {
                self.streaming.get_or_insert_with(String::new);
                self.status = "思考中…".into();
            }
            CoreEvent::Token { text, .. } => {
                self.streaming.get_or_insert_with(String::new).push_str(&text);
            }
            CoreEvent::ToolCallBegin { name, arguments, risk, .. } => {
                // 把已流出的文本先落到消息列表，工具调用作为独立条目
                self.flush_streaming();
                self.running_tool = Some(name.clone());
                let brief = compact_args(&arguments);
                self.messages.push(MsgItem {
                    role: "工具",
                    text: format!("[{}] {name} {brief}", risk.label()),
                    error: false,
                });
            }
            CoreEvent::ToolCallEnd { ok, brief, .. } => {
                self.running_tool = None;
                self.messages.push(MsgItem {
                    role: if ok { "  结果" } else { "  失败" },
                    text: brief,
                    error: !ok,
                });
            }
            CoreEvent::ApprovalRequired { req } => {
                self.flush_streaming();
                self.status = "等待你的确认".into();
                self.approval = Some(req);
            }
            CoreEvent::ContextCompacted { before_tokens, after_tokens, strategy, .. } => {
                self.messages.push(MsgItem {
                    role: "系统",
                    text: format!(
                        "上下文已压缩（{strategy}）：{before_tokens} → {after_tokens} tokens"
                    ),
                    error: false,
                });
            }
            CoreEvent::TurnEnded { reason, usage, .. } => {
                self.flush_streaming();
                self.status = format!(
                    "{:?} · in {} / out {} tokens",
                    reason, usage.input_tokens, usage.output_tokens
                );
            }
            CoreEvent::Warning { message } => {
                self.messages.push(MsgItem { role: "警告", text: message, error: true });
            }
            CoreEvent::Fatal { message } => {
                self.flush_streaming();
                self.status = "出错".into();
                self.messages.push(MsgItem { role: "错误", text: message, error: true });
            }
            CoreEvent::Thinking { .. } => {}
        }
        cx.notify();
    }

    /// 把流式缓冲落成一条消息。
    fn flush_streaming(&mut self) {
        if let Some(text) = self.streaming.take() {
            if !text.trim().is_empty() {
                self.messages.push(MsgItem { role: "Zex", text, error: false });
            }
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_2().p_4();

        for m in &self.messages {
            let color = if m.error { rgb(0xd04040) } else { rgb(0x888888) };
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xs().text_color(color).child(m.role))
                    .child(div().text_sm().child(m.text.clone())),
            );
        }

        // 正在流式输出的内容
        if let Some(s) = &self.streaming {
            if !s.is_empty() {
                list = list.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_xs().text_color(rgb(0x888888)).child("Zex"))
                        .child(div().text_sm().child(s.clone())),
                );
            }
        }

        // 审批区
        let approval_bar = match &self.approval {
            Some(req) => {
                let grantable = req.grantable;
                let mut bar = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .bg(rgb(0x3a2f1a))
                    .child(
                        div()
                            .text_sm()
                            .child(format!("需要确认 [{}] {}", req.risk.label(), req.tool)),
                    )
                    .child(div().text_xs().child(req.summary.clone()))
                    .child(div().text_xs().child(compact_args(&req.arguments)));

                let mut buttons = div().flex().gap_2();
                buttons = buttons.child(
                    Button::new("ok").primary().small().label("批准").on_click(cx.listener(
                        |this, _: &ClickEvent, _window, cx| this.respond(true, false, cx),
                    )),
                );
                buttons = buttons.child(
                    Button::new("no").small().label("拒绝").on_click(cx.listener(
                        |this, _: &ClickEvent, _window, cx| this.respond(false, false, cx),
                    )),
                );
                if grantable {
                    buttons = buttons.child(
                        Button::new("always").small().label("批准并记住此类").on_click(
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.respond(true, true, cx)
                            }),
                        ),
                    );
                }
                bar = bar.child(buttons);
                Some(bar)
            }
            None => None,
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1a1a1a))
            .text_color(rgb(0xe0e0e0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .bg(rgb(0x242424))
                    .child(div().text_sm().child("Zex"))
                    .child(div().text_xs().text_color(rgb(0x888888)).child(self.status.clone())),
            )
            .child(div().flex_1().overflow_y_scrollbar().child(list));

        if let Some(bar) = approval_bar {
            root = root.child(bar);
        }

        root.child(
            div()
                .flex()
                .gap_2()
                .p_4()
                .bg(rgb(0x242424))
                .child(div().flex_1().child(Input::new(&self.input)))
                .child(
                    Button::new("send")
                        .primary()
                        .label("发送")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit(window, cx))),
                ),
        )
    }
}

/// 按平台选择 UI 字体。
///
/// 默认的虚拟字体 `.SystemUIFont` 在 Linux 上解析不到（会回退到 Segoe UI，
/// 那也是 Windows 字体），结果中文渲染成方框。这里显式指定一个装了的字体，
/// 并允许用 `ZEX_UI_FONT` 覆盖。
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
    let theme = cx.global_mut::<gpui_kit::component::Theme>();
    theme.font_family = candidate.into();
}

/// 把 JSON 参数压成一行短摘要。
fn compact_args(v: &serde_json::Value) -> String {
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s
    }
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
    let agent = match build_agent(&cfg, cwd) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("无法初始化内核: {e}");
            std::process::exit(1);
        }
    };

    let broker = agent.approvals();
    let CoreHandles { cmd_tx, mut event_rx } = zex_app::spawn_core(agent);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        apply_platform_font(cx);

        let (handle, view) = match gpui_kit::open_window(
            WindowOptions::default(),
            cx,
            |window, cx| {
                let tx = cmd_tx.clone();
                let broker = broker.clone();
                cx.new(|cx| ChatView::new(tx, broker, window, cx))
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
        let cmd_tx = cmd_tx.clone();
        cx.spawn(async move |cx| {
            while let Some(ev) = event_rx.recv().await {
                let alive = weak
                    .update(cx, |this, cx| this.handle_event(ev, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
            let _ = cmd_tx.send(UiCommand::Shutdown).await;
        })
        .detach();
    });
}
