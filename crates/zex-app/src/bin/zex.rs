//! Zex GUI 二进制。
//!
//! 视觉按参考设计稿实现：浅色主题、橙色品牌色、左侧会话栏、
//! 文档流消息、独立工具调用卡片、内嵌审批条。
//!
//! 双世界桥接的 UI 侧（ADR-002）：gpui 主线程只做渲染与输入，
//! 内核在独立 tokio 线程里跑，两者用 channel 通信。

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::text::{TextView, TextViewStyle, markdown};
use gpui_kit::*;

use tokio::sync::mpsc;
use zex_agent::ApprovalBroker;
use zex_core::{ApprovalId, ApprovalRequest, CoreEvent, RememberScope, UiCommand, Usage};

/// 界面快捷键动作。
///
/// `InterruptTask`：Esc 中断当前任务。
/// `HistoryPrev`/`HistoryNext`：输入框为空时 ↑↓ 回溯指令历史。
mod keys {
    #![allow(missing_docs)]
    gpui_kit::actions!(zex, [InterruptTask, HistorySearch]);
}
use keys::{HistorySearch, InterruptTask};

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
/// 消息正文字号与行高。
///
/// 界面里读得最多的字就是它，所以比全局正文小半档、行距放大一档：
/// 长段落（模型回复动辄两三屏）靠行距分隔比靠字号堆砌舒服得多。
const MSG_FONT: f32 = 14.5;
const MSG_LINE: f32 = 24.0;
/// 用户自己敲的那句话，比模型回复略大一点，便于回看时定位。
const USER_FONT: f32 = 15.0;

/// 右侧面板默认宽度，以及拖拽的上下限。
const DEFAULT_PANEL_WIDTH: f32 = 420.0;
const MIN_PANEL_WIDTH: f32 = 320.0;
const MAX_PANEL_WIDTH: f32 = 900.0;
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

/// 组里的一步：一次调用 + 它的结果。
#[derive(Clone)]
struct ToolStep {
    name: String,
    args: String,
    risk: String,
    /// `None` 表示还没出结果（正在跑，或这一轮被中断了）。
    result: Option<ToolOutcome>,
}

/// 工具执行结果。
#[derive(Clone)]
struct ToolOutcome {
    ok: bool,
    output: String,
}

impl MsgItem {
    /// 这条消息在信息流里的唯一标识，用于记住「哪个工具卡被展开了」。
    ///
    /// 用序号而不是消息下标：下标会随着新消息追加、会话切换而漂移，
    /// 展开状态会莫名其妙跳到别人身上。
    fn key(&self, idx: usize) -> String {
        format!("row-{idx}")
    }
}

#[derive(Clone)]
enum MsgKind {
    User,
    Assistant,
    /// 连续的一串工具调用。
    ///
    /// 一轮里模型常常连着调好几个工具（读文件、跑命令、再读…），
    /// 每次一张卡会把整屏刷满，真正要读的回复被挤到看不见的地方。
    /// 所以按"连续的调用"聚成一组，默认只占一行。
    ToolGroup {
        steps: Vec<ToolStep>,
    },
    /// 模型的思考过程（provider 支持时才有）。
    ///
    /// 默认折叠成一行：推理往往几百上千字，摊开会把正文顶下去；
    /// 但它解释「模型为什么这么做」，需要时得能翻出来。
    Thinking,
    System,
    /// 子智能体的一步动作。缩进显示在主对话里，
    /// 让用户不切视图也知道后台那些 agent 在干什么。
    SubAgentStep {
        /// 哪个 agent。
        agent: String,
        /// 工具名。
        name: String,
        /// 参数摘要。
        args: String,
    },
}

/// 右侧面板正在看哪个子智能体。
///
/// 单独一块而不是切主会话：用户想「看一眼它在干什么」，
/// 不是「离开当前对话」。
struct SubAgentPanel {
    /// agent 标识。
    id: String,
    /// 它占的会话 id。
    session: String,
    /// 职责描述，显示在面板标题下方。
    description: String,
    /// 状态标签。
    status: String,
    /// 已经发生的消息。
    messages: Vec<MsgItem>,
    /// 正在流式输出的文本。
    streaming: Option<String>,
}

/// 右侧面板的一个标签页。
enum PanelTab {
    /// 子智能体执行过程。
    SubAgent(SubAgentPanel),
    /// 文件预览。
    File(FilePreview),
}

impl PanelTab {
    /// 标签上显示的名字。
    fn title(&self) -> String {
        match self {
            PanelTab::SubAgent(p) => p.id.clone(),
            // 只取文件名：完整路径太长，三个标签一挤就全变成
            // "crates/zex..."。真实路径在内容区顶部还写着一份
            PanelTab::File(f) => f
                .display
                .rsplit('/')
                .next()
                .unwrap_or(f.display.as_str())
                .to_string(),
        }
    }

    /// 去重用的键：同一个文件/同一个 agent 只占一个标签。
    fn key(&self) -> String {
        match self {
            PanelTab::SubAgent(p) => format!("agent:{}", p.session),
            PanelTab::File(f) => format!("file:{}", f.path),
        }
    }
}

/// 文件预览。
struct FilePreview {
    /// 绝对路径：既是标签唯一键，也用于重新读取。
    path: String,
    /// 相对工作目录的显示名（短，适合放标签上）。
    display: String,
    /// 文件内容（超长会截断）。
    content: String,
    /// 语法高亮的语言名，空串表示不着色。
    lang: String,
    /// 截断/读取异常之类的说明。
    note: Option<String>,
}

/// 右侧面板：一组标签页，当前激活其中一页。
struct RightPanel {
    tabs: Vec<PanelTab>,
    active: usize,
}

/// 待发送的附件。
///
/// 图片走 `Part::Image`（provider 会转成 base64 data URL），
/// 其它文件以路径形式进上下文 —— 模型用 fs_read 自己读。
#[derive(Clone)]
struct Attachment {
    /// 绝对路径。
    path: std::path::PathBuf,
    /// 显示名（文件名）。
    name: String,
    /// 图片则已读成 base64，否则为 None。
    image_b64: Option<String>,
    /// 图片的 MIME 类型。
    media_type: String,
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
    /// 这个会话上次用的模型（`provider/model`），空=用配置默认。
    model: String,
    /// 这个会话上次的运行模式。
    mode: zex_core::RunMode,
}

/// 主视图。
struct ChatView {
    /// 多行输入。真实任务描述经常是分点的，单行框里滚动查看太难受。
    input: Entity<TextareaState>,
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
    /// 用户点了关闭、但还有任务在跑时，先停下来问一句。
    confirm_close: bool,
    /// 子智能体团队。非空时在对话里显示一块团队面板。
    team: Vec<zex_core::event::SubAgentView>,
    /// 右侧面板：正在看的子智能体。
    right_panel: Option<RightPanel>,
    /// 待发送的附件。
    attachments: Vec<Attachment>,
    /// 上一次输入的长度，用来发现"粘进来一大段"。
    last_input_len: usize,
    /// 正在录音。
    recording: Option<zex_voice::Recording>,
    /// 正在等转写结果。
    transcribing: bool,
    /// 斜杠命令菜单是否展开。
    slash_open: bool,
    /// `@` 引用的候选文件（懒加载，只扫一次）。
    file_index: Option<Vec<String>>,
    /// `@` 菜单是否展开。
    at_open: bool,
    /// Ctrl+R 的历史列表是否展开。
    history_search_open: bool,
    /// 弹出菜单里当前选中第几项。
    menu_cursor: usize,
    /// 本轮用哪个模型（`provider/model`）；None 用配置默认。
    selected_model: Option<String>,
    /// 可选的模型清单，按 provider 分组：一级机构，二级模型。
    models_by_provider: std::collections::BTreeMap<String, Vec<String>>,
    /// 默认模型所属的 provider，用于菜单里的选中标记。
    active_provider: String,
    /// `/`、`@` 菜单的方向键拦截器。
    ///
    /// 必须走 `intercept_keystrokes`：输入框把 up/down 绑成了移动光标，
    /// keybinding 先于 key listener 执行并停掉事件，
    /// `capture_key_down` 在这里根本收不到方向键。
    _key_interceptor: Option<gpui::Subscription>,
    /// 当前会话的运行模式。
    run_mode: zex_core::RunMode,
    /// 正在编辑模型名。
    editing_model: bool,
    model_input: Entity<InputState>,
    /// 本会话发过的指令，↑ 回溯用。
    input_history: Vec<String>,
    /// 当前回溯到第几条（None = 正在编辑新内容）。
    history_cursor: Option<usize>,
    /// 开始回溯前的草稿，回溯回来要还原。
    history_draft: String,
    /// 哪些工具卡被展开了（按消息序号记）。
    ///
    /// 默认全折叠：一轮对话里工具调用经常有七八条，
    /// 全摊开会把真正要读的回复挤出屏幕。
    expanded_tools: std::collections::HashSet<String>,
    /// 右侧子智能体面板的宽度（像素，可拖拽）。
    subagent_panel_width: f32,
    /// 正在拖面板边框。
    subagent_resizing: bool,
    /// 拖拽起点：按下时的鼠标 x 与当时的宽度。
    subagent_drag_origin: (f32, f32),
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
        models_by_provider: std::collections::BTreeMap<String, Vec<String>>,
        active_provider: String,
        history: Vec<MsgItem>,
        tasks: Vec<zex_store::tasks::TaskRecord>,
        store: Arc<zex_store::Store>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("请输入您的指令，或选择一个快捷操作…")
        });
        // 在 active_session 被移进结构体之前先把历史捞出来
        let restored_history = load_input_history(&store, &active_session);
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("会话标题"));
        let model_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("provider/model，例如 deepseek/deepseek-flash")
        });

        // Enter 发送，Shift+Enter 换行。输入框自己无法区分这两者的语义，
        // 由外层决定"按下回车意味着提交"。
        cx.subscribe_in(&input, window, |this, _input, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift, .. } = ev {
                if !shift {
                    this.submit(window, cx);
                }
            }
            // 焦点变化要重绘，否则高亮边框不跟手
            if matches!(ev, InputEvent::Focus | InputEvent::Blur) {
                cx.notify();
            }
            // 以 / 开头就亮出命令菜单
            if let InputEvent::Change = ev {
                let v = this.input.read(cx).value();
                // 一次增加几百字基本只可能是粘贴 —— 提一句，
                // 免得用户以为界面卡了
                let len = v.chars().count();
                if len > this.last_input_len + 500 {
                    this.status = format!("已插入 {} 字", len - this.last_input_len);
                }
                this.last_input_len = len;
                let v = v.trim_start();
                this.slash_open = v.starts_with('/') && !v.contains(' ');
                // 候选集变了，游标回到第一项
                this.menu_cursor = 0;
                // @ 提到最后一个 @ 之后的词
                let at = v.rfind('@').map(|i| &v[i + 1..]);
                this.at_open = at.map(|t| !t.contains(' ')).unwrap_or(false);
                if this.at_open {
                    this.ensure_file_index();
                }
            }
        })
        .detach();

        // 模型输入框：回车确认，Esc 取消
        cx.subscribe_in(
            &model_input,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.commit_model_edit(window, cx);
                }
            },
        )
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

        let mut view = ChatView {
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
            confirm_close: false,
            team: Vec::new(),
            right_panel: None,
            attachments: Vec::new(),
            last_input_len: 0,
            recording: None,
            transcribing: false,
            slash_open: false,
            file_index: None,
            at_open: false,
            history_search_open: false,
            menu_cursor: 0,
            selected_model: None,
            models_by_provider,
            active_provider,
            _key_interceptor: None,
            run_mode: zex_core::RunMode::Confirm,
            editing_model: false,
            model_input,
            input_history: restored_history,
            history_cursor: None,
            history_draft: String::new(),
            expanded_tools: std::collections::HashSet::new(),
            subagent_panel_width: DEFAULT_PANEL_WIDTH,
            subagent_resizing: false,
            subagent_drag_origin: (0.0, 0.0),
            pending_approvals: Vec::new(),
            store,
            cwd,
            model,
            usage: Usage::default(),
        };

        // 方向键：菜单导航与历史回溯
        //
        // 只能在 keystroke interceptor 里做。输入框把 up/down 绑成了
        // 「移动光标」，这一层跑在 keybinding 之前，能先一步把键吃掉；
        // 放在 capture_key_down 里收不到 —— 事件已被 keybinding
        // 标记消费，listener 阶段直接跳过了。
        let weak = cx.entity().downgrade();
        view._key_interceptor = Some(cx.intercept_keystrokes(move |event, window, cx| {
            let key = event.keystroke.key.as_str();

            // Enter：菜单开着时由这里确认选中项。
            // 输入框是多行 Textarea，它收到 Enter 会先往值里插一个换行，
            // 再发 PressEnter —— 等 submit 拿到时值已变成 "@/\n"，
            // 过滤条件带着换行符，候选直接变空。
            if key == "enter" {
                let mut picked = false;
                let _ = weak.update(cx, |this, cx| {
                    if this.menu_is_open() {
                        let cursor = this.menu_cursor;
                        this.menu_pick(cursor, &mut *window, cx);
                        picked = true;
                    }
                });
                if picked {
                    cx.stop_propagation();
                }
                return;
            }

            if key != "up" && key != "down" {
                return;
            }
            let is_up = key == "up";
            let handled = weak
                .update(cx, |this, cx| {
                    // 菜单优先：↑↓ 在候选里走，不动输入框光标
                    if this.menu_is_open() {
                        let n = this.menu_items(cx).len();
                        if is_up {
                            this.menu_cursor = this.menu_cursor.saturating_sub(1);
                        } else if this.menu_cursor + 1 < n {
                            this.menu_cursor += 1;
                        }
                        cx.notify();
                        return true;
                    }
                    // 空输入时回溯历史；有内容就把 ↑↓ 让给输入框
                    if is_up && this.input.read(cx).value().trim().is_empty() {
                        this.recall_history(true, &mut *window, cx);
                        return true;
                    }
                    false
                })
                .unwrap_or(false);
            if handled {
                cx.stop_propagation();
            }
        }));

        // 打开就能打字：否则用户要先点一下输入框，
        // 而这是整个应用唯一的入口
        window.focus(&view.input.focus_handle(cx), cx);
        view
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
        let inner = if ix < self.messages.len() {
            self.render_message(&self.messages[ix], ix, 1.0, cx)
        } else {
            self.render_streaming(cx)
        };
        // 内容和面板边缘之间要留出呼吸：不加这层，正文会直接贴到
        // 侧栏和窗口右边框上，读起来像没排版的纯文本
        div().px_8().py_2().child(inner).into_any_element()
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

    /// 弹出的菜单是否打开。
    fn menu_is_open(&self) -> bool {
        self.slash_open || self.at_open || self.history_search_open
    }

    /// 渲染弹出菜单。
    ///
    /// 键盘（↑↓ + Enter）和鼠标（点击）走同一份候选与同一个游标，
    /// 不会出现"看到 A、执行 B"。
    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items = self.menu_items(cx);
        if items.is_empty() {
            return None;
        }
        let hint = if self.slash_open {
            "命令"
        } else if self.at_open {
            "引用文件"
        } else {
            "历史指令"
        };

        let mut menu = div()
            .rounded(px(10.0))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(BG_MAIN))
            .p_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("{hint}  ↑↓ 选择 · Enter 确认 · Esc 取消")),
            );

        for (i, item) in items.iter().enumerate() {
            let picked = i == self.menu_cursor;
            let label = if self.slash_open {
                SLASH_COMMANDS
                    .iter()
                    .find(|(n, _)| n == item)
                    .map(|(n, d)| format!("{n}  —  {d}"))
                    .unwrap_or_else(|| item.clone())
            } else {
                item.clone()
            };
            let idx = i;
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(6.0));
            if picked {
                row = row.bg(rgb(BG_HOVER));
            }
            menu = menu.child(
                row
                    .child(
                        div()
                            .text_xs()
                            .child(if self.slash_open {
                                "›"
                            } else if self.at_open {
                                "📄"
                            } else {
                                "↺"
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(if picked { rgb(TEXT) } else { rgb(TEXT_MUTED) })
                            .overflow_hidden()
                            .child(label),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                        this.menu_pick(idx, window, cx);
                    })),
            );
        }
        Some(menu.into_any_element())
    }

    /// 选中菜单里第 `i` 项。
    ///
    /// `/` 只做补全，不执行 —— 命令多带参数（`/model deepseek/…`），
    /// 选中就发出去的话用户没有补参数的余地。`@` 是插入引用，同样不改内容。
    fn menu_pick(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.menu_items(cx);
        let Some(item) = items.get(i).cloned() else {
            return;
        };
        if self.slash_open {
            self.complete_slash(&item, window, cx);
        } else if self.at_open {
            self.insert_at_reference(&item, window, cx);
        } else if self.history_search_open {
            self.input
                .update(cx, |st, cx| st.set_value(item, window, cx));
            self.history_search_open = false;
            cx.notify();
        }
    }

    /// 把选中的命令补全进输入框，光标停在末尾，等用户自己回车执行。
    fn complete_slash(&mut self, cmd: &str, window: &mut Window, cx: &mut Context<Self>) {
        // 带一个尾随空格：多数命令后面要跟参数，直接接着打字即可
        let text = format!("{cmd} ");
        let end = text.len();
        self.input.update(cx, |st, cx| {
            st.set_value(text, window, cx);
            // set_value 内部会 reset_selection（多行输入回到 0），
            // 不显式挪的话下一个字会插到命令前面
            st.set_selected_range(end..end, cx);
        });
        self.menu_cursor = 0;
        self.slash_open = false;
        cx.notify();
    }

    /// 当前菜单的候选。
    ///
    /// 渲染和键盘导航共用这一份：两边各算一次的话，筛选条件一改就会
    /// 出现「看到的是 A、回车执行的是 B」。
    fn menu_items(&self, cx: &Context<Self>) -> Vec<String> {
        if self.slash_open {
            let typed = self.input.read(cx).value().trim_start().to_string();
            SLASH_COMMANDS
                .iter()
                .filter(|(name, _)| name.starts_with(&typed))
                .map(|(name, _)| (*name).to_string())
                .collect()
        } else if self.at_open {
            let typed = self.input.read(cx).value().to_string();
            // trim：多行输入框可能在尾部留下换行
            let q = typed
                .rfind('@')
                .map(|i| typed[i + 1..].trim().to_lowercase())
                .unwrap_or_default();
            let mut hits: Vec<String> = self
                .file_index
                .as_ref()
                .map(|all| {
                    all.iter()
                        .filter(|f| f.to_lowercase().contains(&q))
                        .take(8)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            hits.sort_by_key(|f| f.len());
            hits
        } else if self.history_search_open {
            self.input_history.iter().rev().take(8).cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// 扫一遍工作目录，给 `@` 引用用。
    ///
    /// 只在第一次用到时扫，跳过大目录 —— 用户等不了几百毫秒的索引。
    fn ensure_file_index(&mut self) {
        if self.file_index.is_some() {
            return;
        }
        const SKIP: &[&str] = &[
            ".git", "target", "node_modules", "dist", "build", ".venv", "__pycache__",
            ".next", ".cache", "vendor",
        ];
        const MAX: usize = 4000;
        let mut out = Vec::new();
        let root = std::path::PathBuf::from(&self.cwd);
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > 4 || out.len() >= MAX {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                    continue;
                }
                let path = entry.path();
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        stack.push((path, depth + 1));
                    } else if let Ok(rel) = path.strip_prefix(&root) {
                        out.push(rel.display().to_string());
                    }
                }
            }
        }
        out.sort();
        self.file_index = Some(out);
    }

    /// 把 `@片段` 换成 `@完整路径`。
    fn insert_at_reference(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.input.read(cx).value().trim_end().to_string();
        let new = match v.rfind('@') {
            Some(i) => format!("{}@{} ", &v[..i], path),
            None => format!("{v}@{path} "),
        };
        let end = new.len();
        self.input.update(cx, |st, cx| {
            st.set_value(new, window, cx);
            st.set_selected_range(end..end, cx);
        });
        self.at_open = false;
        cx.notify();
    }

    /// 执行一条斜杠命令。
    ///
    /// 返回 true 表示这次输入被当作命令消费掉了，不该发给模型。
    fn run_slash(&mut self, cmd: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let cmd = cmd.trim();
        let (name, rest) = match cmd.split_once(' ') {
            Some((n, r)) => (n, r.trim()),
            None => (cmd, ""),
        };
        match name {
            "/new" => {
                let tx = self.cmd_tx.clone();
                cx.spawn(async move |_this, _cx| {
                    let _ = tx.send(UiCommand::NewSession).await;
                })
                .detach();
            }
            "/clear" => {
                let tx = self.cmd_tx.clone();
                let session = self.active_session.clone();
                cx.spawn(async move |_this, _cx| {
                    let _ = tx
                        .send(UiCommand::ClearSession {
                            session: zex_core::SessionId::from(session),
                        })
                        .await;
                })
                .detach();
                self.messages.clear();
            }
            "/model" => {
                if rest.is_empty() {
                    self.begin_model_edit(window, cx);
                } else {
                    self.model_input
                        .update(cx, |st, cx| st.set_value(rest, window, cx));
                    self.commit_model_edit(window, cx);
                }
            }
            "/help" | "/?" => {
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: "/new 新会话　/clear 清空当前会话　/model [名称] 切换模型　/help 帮助"
                        .into(),
                });
            }
            _ => {
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: format!("没有 `{name}` 这个命令。试试 /help"),
                });
            }
        }
        self.input.update(cx, |st, cx| st.set_value("", window, cx));
        self.slash_open = false;
        cx.notify();
        true
    }

    /// 点麦克风：开始录音，再点停止并转写。
    fn toggle_recording(&mut self, cx: &mut Context<Self>) {
        if self.transcribing {
            return;
        }
        if self.recording.is_some() {
            let Some(rec) = self.recording.take() else {
                return;
            };
            let secs = rec.elapsed().as_secs();
            if secs == 0 {
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: "录得太短了，按住多说一会儿".into(),
                });
                cx.notify();
                return;
            }
            self.status = "识别中…".into();
            self.transcribing = true;
            let tx = self.cmd_tx.clone();
            // 停止录音要等后台线程收尾，别卡住界面
            cx.spawn(async move |this, cx| {
                let wav = rec.stop();
                let _ = this.update(cx, |this, cx| {
                    match wav {
                        Ok(bytes) => {
                            let _ = tx.try_send(UiCommand::Transcribe { audio: bytes });
                        }
                        Err(e) => {
                            this.transcribing = false;
                            this.status = "Ready".into();
                            this.messages.push(MsgItem {
                                kind: MsgKind::System,
                                text: format!("录音失败：{e}"),
                            });
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }

        match zex_voice::Recording::start() {
            Ok(rec) => {
                self.recording = Some(rec);
                self.status = "录音中…（再点一次结束）".into();
                // 每秒刷新时长；超过 110 秒自动停 —— ASR 上限 120 秒，
                // 撞上限会直接 400，不如提前收尾
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(1))
                            .await;
                        let Some(secs) = this
                            .update(cx, |this, cx| {
                                let s = this.recording.as_ref().map(|r| r.elapsed().as_secs());
                                if let Some(s) = s {
                                    this.status = format!("录音中 {s}s（再点一次结束）");
                                    if s >= 110 {
                                        this.toggle_recording(cx);
                                    }
                                    cx.notify();
                                }
                                s
                            })
                            .ok()
                            .flatten()
                        else {
                            break;
                        };
                        if secs == 0 || secs >= 110 {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => {
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: format!("无法开始录音：{e}"),
                });
            }
        }
        cx.notify();
    }

    /// 点模型名进入编辑：预填当前值，回车生效。
    ///
    /// 做成可编辑而不是固定下拉：配置里的 provider 组合是用户自己写的，
    /// 界面猜不准他想配哪个模型名。
    fn begin_model_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self
            .selected_model
            .clone()
            .unwrap_or_else(|| self.model.clone());
        self.model_input
            .update(cx, |st, cx| st.set_value(cur, window, cx));
        self.editing_model = true;
        cx.notify();
    }

    /// 提交模型编辑。空输入等于取消。
    fn commit_model_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v = self.model_input.read(cx).value().trim().to_string();
        self.editing_model = false;
        if v.is_empty() {
            cx.notify();
            return;
        }
        // 只接受配置里声明过的 provider，否则那一轮会静默用默认模型
        // 认「provider/model」和「光写模型名」两种写法：
        // 光写模型名时，只要它属于任一声明过的 provider 就放行
        let ok = self
            .models_by_provider
            .values()
            .any(|list| list.iter().any(|m| m == &v))
            || self
                .models_by_provider
                .contains_key(v.split('/').next().unwrap_or(""));
        if ok {
            self.selected_model = if v == self.model { None } else { Some(v) };
        } else {
            self.messages.push(MsgItem {
                kind: MsgKind::System,
                text: format!("模型 `{v}` 不在配置里，仍用 {}", self.model),
            });
        }
        let _ = window;
        cx.notify();
    }

    /// 把拖进来的文件收成附件。
    ///
    /// 图片读成 base64（模型能直接看），其它类型只记路径 ——
    /// 让模型按需用 fs_read 读，比一股脑塞进上下文省 token。
    fn add_attachments(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        for p in paths {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string());

            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let media_type = match ext.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "",
            };

            let image_b64 = if media_type.is_empty() {
                None
            } else {
                match std::fs::read(p) {
                    Ok(bytes) => Some(base64_encode(&bytes)),
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e, "读图片失败");
                        None
                    }
                }
            };

            // 同一个文件别重复加
            if self.attachments.iter().any(|a| a.path == *p) {
                continue;
            }
            self.attachments.push(Attachment {
                path: p.clone(),
                name,
                image_b64,
                media_type: media_type.to_string(),
            });
        }
        cx.notify();
    }

    /// ↑↓ 在历史里走。
    ///
    /// 只在输入框为空时触发（见根元素的 `ZexEmpty` context），
    /// 所以不会抢掉多行编辑里的光标上下移动。
    fn recall_history(&mut self, prev: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_history.is_empty() {
            return;
        }

        if prev {
            let next = match self.history_cursor {
                None => {
                    // 第一次回溯：先记住正在打的内容，回来时还原
                    self.history_draft = self.input.read(cx).value().to_string();
                    self.input_history.len() - 1
                }
                Some(0) => 0, // 已经在最旧的一条
                Some(i) => i - 1,
            };
            self.history_cursor = Some(next);
            let text = self.input_history[next].clone();
            self.input
                .update(cx, |st, cx| st.set_value(text, window, cx));
        } else {
            match self.history_cursor {
                None => return,
                Some(i) if i + 1 < self.input_history.len() => {
                    self.history_cursor = Some(i + 1);
                    let text = self.input_history[i + 1].clone();
                    self.input
                        .update(cx, |st, cx| st.set_value(text, window, cx));
                }
                Some(_) => {
                    // 走到最新一条之后再按 ↓ 就回到草稿
                    self.history_cursor = None;
                    let draft = std::mem::take(&mut self.history_draft);
                    self.input
                        .update(cx, |st, cx| st.set_value(draft, window, cx));
                }
            }
        }
        cx.notify();
    }

    /// 输入框高度：跟着内容长，三行起步、六行封顶。
    ///
    /// 起步三行是刻意的：一句话的指令一行就够，但真正有价值的需求
    /// 往往要分点写，给足初始空间比让用户先看见一条缝强。
    fn composer_height(&self, cx: &Context<Self>) -> f32 {
        const LINE: f32 = 22.0;
        const MIN_LINES: usize = 3;
        const MAX_LINES: usize = 6;
        let text = self.input.read(cx).value();
        // 显式换行 + 视觉折行都算；按 60 字估一行，粗一点够用
        let explicit = text.split('\n').count();
        let wrapped = text.chars().count() / 60 + 1;
        let lines = explicit.max(wrapped).clamp(MIN_LINES, MAX_LINES);
        LINE * lines as f32 + 8.0
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 菜单开着的话，回车是"确认选中项"而不是发送
        if self.menu_is_open() {
            let cursor = self.menu_cursor;
            self.menu_pick(cursor, window, cx);
            return;
        }

        let mut text = self.input.read(cx).value().trim().to_string();
        if (text.is_empty() && self.attachments.is_empty()) || self.streaming.is_some() {
            return;
        }

        // 斜杠命令不发给模型
        if text.starts_with('/') {
            self.run_slash(&text, window, cx);
            return;
        }

        // 图片走多模态通道，其它文件只在正文里留个路径让模型自己读
        let mut images = Vec::new();
        for a in &self.attachments {
            match &a.image_b64 {
                Some(b64) => images.push(zex_core::message::ImageRef {
                    media_type: a.media_type.clone(),
                    blob_id: b64.clone(),
                }),
                None => {
                    text.push_str(&format!("\n\n[附带文件] {}", a.path.display()));
                }
            }
        }
        if text.trim().is_empty() {
            text = "请查看附件。".to_string();
        }

        // 记历史：重复的相邻指令不重复记
        if self.input_history.last() != Some(&text) {
            self.input_history.push(text.clone());
        }
        self.history_cursor = None;
        self.history_draft.clear();
        self.attachments.clear();

        self.input.update(cx, |state, cx| state.set_value("", window, cx));
        // 派了活就要看到它跑：从工作台自动进入对话视图
        self.mode = ViewMode::Chat;
        self.messages.push(MsgItem { kind: MsgKind::User, text: text.clone() });
        self.streaming = Some(String::new());
        self.status = "思考中…".into();

        let tx = self.cmd_tx.clone();
        let session = self.active_session.clone();
        let selected_model = self.selected_model.clone();
        let run_mode = self.run_mode;
        cx.spawn(async move |_this, _cx| {
            let _ = tx
                .send(UiCommand::UserPrompt {
                    session: zex_core::SessionId::from(session),
                    text,
                    attachments: images,
                    model: selected_model,
                    mode: Some(run_mode),
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

    /// 打开右侧面板，看某个子智能体在干什么。
    ///
    /// 从库里读一份快照 —— 切会话那套会丢掉当前对话，而用户这里
    /// 只是想「瞄一眼」。
    /// 当前会话派生的子智能体。
    ///
    /// 团队是**按会话**的：registry 里存着所有会话派出去的 agent，
    /// 但界面上只该看到自己这边的 —— 否则切到别的对话，
    /// 门口还杵着一队不相干的人在报进度。
    fn visible_team(&self) -> Vec<&zex_core::event::SubAgentView> {
        self.team
            .iter()
            .filter(|a| a.parent_session == self.active_session)
            .collect()
    }

    fn open_subagent_panel(&mut self, id: &str, cx: &mut Context<Self>) {
        let info = self.team.iter().find(|a| a.id == id).cloned();
        let (session, description, status, id) = match info {
            Some(a) => (a.session, a.description, a.status, a.id),
            None => {
                // 团队快照里没有（可能被 try_send 丢了），查库
                match self
                    .store
                    .list_sub_agents()
                    .ok()
                    .and_then(|list| list.into_iter().find(|r| r.id == id))
                {
                    Some(r) => (
                        r.session_id,
                        r.description,
                        format!("{:?}", r.status),
                        r.id,
                    ),
                    None => return,
                }
            }
        };

        let messages = load_session_items(&self.store, &session);
        let tab = PanelTab::SubAgent(SubAgentPanel {
            id,
            session,
            description,
            status,
            messages,
            streaming: None,
        });
        self.activate_or_push_tab(tab, cx);
    }

    /// 把标签放进来：已有同键的就切过去，没有才新建一页。
    fn activate_or_push_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        let key = tab.key();
        let panel = self
            .right_panel
            .get_or_insert_with(|| RightPanel { tabs: Vec::new(), active: 0 });
        match panel.tabs.iter().position(|t| t.key() == key) {
            Some(i) => panel.active = i,
            None => {
                panel.tabs.push(tab);
                panel.active = panel.tabs.len() - 1;
            }
        }
        cx.notify();
    }

    /// 在右侧面板打开一个文件。
    ///
    /// 已经开着就切过去，不重复开标签 —— 反复点同一个文件
    /// 会攒出一排一模一样的页。
    fn open_file_tab(&mut self, path: &str, cx: &mut Context<Self>) {
        let abs = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{}/{}", self.cwd.trim_end_matches('/'), path)
        };
        let tab = PanelTab::File(load_file_preview(&abs, &self.cwd));
        self.activate_or_push_tab(tab, cx);
    }

    /// 关掉第 `i` 个标签；全关完就把面板收起来。
    fn close_panel_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(panel) = &mut self.right_panel else {
            return;
        };
        if i >= panel.tabs.len() {
            return;
        }
        panel.tabs.remove(i);
        if panel.tabs.is_empty() {
            self.right_panel = None;
        } else if panel.active >= panel.tabs.len() {
            panel.active = panel.tabs.len() - 1;
        } else if panel.active > i {
            panel.active -= 1;
        }
        cx.notify();
    }

    /// 从队列里挑一个审批摆出来。
    ///
    /// 优先当前会话 —— 用户正看着这个，替他串行处理最符合直觉。
    /// 别的会话的请求会一直排着，直到切过去。
    fn show_next_approval(&mut self, cx: &mut Context<Self>) {
        if self.approval.is_some() {
            return;
        }
        let pos = self
            .pending_approvals
            .iter()
            .position(|(s, _)| *s == self.active_session);
        if let Some(pos) = pos {
            let (_, req) = self.pending_approvals.remove(pos);
            self.flush_streaming(cx);
            self.status = "等待确认".into();
            self.approval = Some(req);
        } else if !self.pending_approvals.is_empty() {
            self.status = format!(
                "其它会话有 {} 个待确认",
                self.pending_approvals.len()
            );
        }
        cx.notify();
    }

    fn respond(&mut self, approve: bool, remember: bool, cx: &mut Context<Self>) {
        let Some(req) = self.approval.take() else { return };
        let broker = self.broker.clone();
        let id: ApprovalId = req.id.clone();
        let grantable = req.grantable;

        self.status = if approve { "执行中…".into() } else { "已拒绝".into() };
        // 处理完一个就摆出下一个，别让排队的一直等着
        self.show_next_approval(cx);

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

    /// 把事件镜像到右侧面板。
    ///
    /// 返回 true 表示这个事件属于面板正在看的那个子智能体，已经消费掉，
    /// 不该再走主对话的逻辑。
    fn mirror_to_panel(&mut self, ev: &CoreEvent) -> bool {
        let Some(rp) = &mut self.right_panel else {
            return false;
        };
        let session = match ev {
            CoreEvent::TurnStarted { session, .. }
            | CoreEvent::Token { session, .. }
            | CoreEvent::Thinking { session, .. }
            | CoreEvent::ToolCallBegin { session, .. }
            | CoreEvent::ToolCallEnd { session, .. }
            | CoreEvent::TurnEnded { session, .. }
            | CoreEvent::ContextCompacted { session, .. } => session,
            _ => return false,
        };
        // 面板可能是文件标签在前、agent 在后，按 session 找那个 agent 页
        let Some(panel) = rp.tabs.iter_mut().find_map(|t| match t {
            PanelTab::SubAgent(p) if p.session == session.as_str() => Some(p),
            _ => None,
        }) else {
            return false;
        };

        match ev {
            CoreEvent::TurnStarted { .. } => {
                panel.streaming.get_or_insert_with(String::new);
                panel.status = "执行中".into();
            }
            CoreEvent::Token { text, .. } => {
                panel.streaming.get_or_insert_with(String::new).push_str(text);
            }
            CoreEvent::ToolCallBegin { name, arguments, risk, .. } => {
                // 先把攒着的文本落成一条消息，保持顺序
                if let Some(buf) = panel.streaming.take() {
                    if !buf.trim().is_empty() {
                        panel.messages.push(MsgItem {
                            kind: MsgKind::Assistant,
                            text: buf,
                        });
                    }
                }
                push_tool_step(
                    &mut panel.messages,
                    ToolStep {
                        name: name.clone(),
                        args: compact_args(arguments),
                        risk: risk.label().to_string(),
                        result: None,
                    },
                );
            }
            CoreEvent::ToolCallEnd { ok, brief, .. } => {
                // 并进前面那条调用；配不上就单独摆一条
                if !merge_tool_result(&mut panel.messages, *ok, brief.clone()) {
                    push_tool_step(
                        &mut panel.messages,
                        ToolStep {
                            name: String::new(),
                            args: String::new(),
                            risk: String::new(),
                            result: Some(ToolOutcome {
                                ok: *ok,
                                output: brief.clone(),
                            }),
                        },
                    );
                }
            }
            CoreEvent::TurnEnded { reason, .. } => {
                if let Some(buf) = panel.streaming.take() {
                    if !buf.trim().is_empty() {
                        panel.messages.push(MsgItem {
                            kind: MsgKind::Assistant,
                            text: buf,
                        });
                    }
                }
                panel.status = if matches!(reason, zex_core::EndReason::Completed) {
                    "已完成".into()
                } else {
                    "已结束".into()
                };
            }
            _ => {}
        }
        true
    }

    fn handle_event(&mut self, ev: CoreEvent, cx: &mut Context<Self>) {
        // 面板正在看的那个子智能体，它的事件直接喂给面板
        if self.mirror_to_panel(&ev) {
            cx.notify();
            return;
        }
        match ev {
            CoreEvent::Transcribed { text, error } => {
                self.transcribing = false;
                match (text, error) {
                    (Some(t), _) => {
                        // 接到已有内容后面，别把用户打的字冲掉
                        let cur = self.input.read(cx).value().to_string();
                        let joined = if cur.trim().is_empty() {
                            t
                        } else {
                            format!("{cur} {t}")
                        };
                        // set_value 要 window，handle_event 这里拿不到，
                        // 走 update_in 借一帧
                        cx.spawn(async move |this, cx| {
                            this.update_in(cx, |this, window, cx| {
                                this.input
                                    .update(cx, |st, cx| st.set_value(joined, window, cx));
                                cx.notify();
                            })
                            .ok();
                        })
                        .detach();
                        self.status = "Ready".into();
                    }
                    (None, Some(e)) => {
                        self.messages.push(MsgItem {
                            kind: MsgKind::System,
                            text: format!("语音识别失败：{e}"),
                        });
                        self.status = "Ready".into();
                    }
                    _ => {}
                }
                cx.notify();
            }
            CoreEvent::PanelControl { open, agent } => {
                if open {
                    // 指定了就看那个；没指定就挑一个能看的 ——
                    // 优先正在跑的，其次最近的
                    let target = agent.or_else(|| {
                        let team = self.visible_team();
                        team.iter()
                            .find(|a| a.status == "执行中")
                            .or_else(|| team.first())
                            .map(|a| a.id.clone())
                    });
                    match target {
                        Some(id) => self.open_subagent_panel(&id, cx),
                        None => {
                            // 没子智能体可看：说清楚，而不是开了个空面板
                            self.messages.push(MsgItem {
                                kind: MsgKind::System,
                                text: "当前会话没有子智能体，右侧面板没有内容可显示".into(),
                            });
                            cx.notify();
                        }
                    }
                } else if self.right_panel.is_some() {
                    self.right_panel = None;
                    cx.notify();
                }
            }
            CoreEvent::TeamUpdated { agents } => {
                // 面板里开着的 agent 状态要跟着团队一起变，
                // 否则会出现「面板说已完成、团队说执行中」这种自相矛盾。
                if let Some(rp) = &mut self.right_panel {
                    for tab in &mut rp.tabs {
                        if let PanelTab::SubAgent(panel) = tab {
                            if let Some(a) = agents.iter().find(|a| a.id == panel.id) {
                                panel.status = a.status.clone();
                                panel.description = a.description.clone();
                            }
                        }
                    }
                }
                self.team = agents;
                self.tasks = load_tasks(&self.store);
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
                    // 是我的子智能体吗？是的话把动作回流到主对话，
                    // 用户不用点进去也知道后台在干什么。
                    let owner = self
                        .team
                        .iter()
                        .find(|a| a.session == session.as_str())
                        .map(|a| a.id.clone())
                        // 兜底：TeamUpdated 走的是 try_send，主对话流式输出
                        // 时通道可能被 token 挤满，团队事件就丢了。查一次库
                        // 比让子智能体的动作凭空消失强。
                        .or_else(|| {
                            self.store
                                .list_sub_agents()
                                .ok()?
                                .into_iter()
                                .find(|r| r.session_id == session.as_str())
                                .map(|r| r.id)
                        });
                    if let Some(agent) = owner {
                        self.messages.push(MsgItem {
                            kind: MsgKind::SubAgentStep {
                                agent,
                                name: name.clone(),
                                args: compact_args(&arguments),
                            },
                            text: String::new(),
                        });
                        self.sync_scroller(cx);
                    }
                    // 其它会话的工具调用不上台面，但它确实在跑
                    self.status = format!("其它会话执行 {name}");
                } else {
                self.flush_streaming(cx);
                self.running_tool = Some(name.clone());
                self.status = format!("执行 {name}");
                push_tool_step(
                    &mut self.messages,
                    ToolStep {
                        name: name.clone(),
                        args: compact_args(&arguments),
                        risk: risk.label().to_string(),
                        result: None,
                    },
                );
                self.sync_scroller(cx);
                }
            }
            CoreEvent::ToolCallEnd { session, ok, brief, .. } => {
                if !self.is_active(&session) {
                    return;
                }
                self.running_tool = None;
                self.status = "Ready".into();
                // 并进前面那条调用，而不是再追加一张卡
                if !merge_tool_result(&mut self.messages, ok, brief.clone()) {
                    push_tool_step(
                        &mut self.messages,
                        ToolStep {
                            name: String::new(),
                            args: String::new(),
                            risk: String::new(),
                            result: Some(ToolOutcome { ok, output: brief }),
                        },
                    );
                }
                self.sync_scroller(cx);
            }
            CoreEvent::ApprovalRequired { session, req } => {
                let key = session.to_string();
                // 同一时刻只摆一个授权给用户看，其余排队。
                // 之前是直接覆盖：三个子智能体同时要授权时，前面两个
                // 会被静默挤掉，等它们超时失败，用户还不知道发生过。
                if self.approval.is_none() && key == self.active_session {
                    self.flush_streaming(cx);
                    self.status = "等待确认".into();
                    self.approval = Some(req);
                } else {
                    self.pending_approvals.push((key, req));
                    let n = self.pending_approvals.len();
                    self.status = if self.approval.is_some() {
                        format!("等待确认（还有 {n} 个排队）")
                    } else {
                        "其它会话有待确认的请求".into()
                    };
                }
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
                // 一轮跑完，工作台的待办列表得跟着变 —— 否则用户
                // 不切换视图就永远看到旧的"执行中"。
                self.tasks = load_tasks(&self.store);
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
                // 模型和模式是会话自己的属性：切过去就该是它原来的样子，
                // 而不是把上一个会话的选择带过来
                if let Some(item) = self.sessions.iter().find(|s| s.id == *session) {
                    self.selected_model = if item.model.is_empty() {
                        None
                    } else if item.model.contains('/') {
                        Some(item.model.clone())
                    } else {
                        // 老数据只存了裸模型名，用默认 provider 补全
                        Some(format!("{}/{}", self.active_provider, item.model))
                    };
                    self.run_mode = item.mode;
                }
                // 每个会话有自己的指令史，切过去要换一份
                self.input_history = load_input_history(&self.store, &session);
                self.history_cursor = None;
                self.history_draft.clear();
                // 这个会话之前在后台等过授权？现在摆出来。
                self.show_next_approval(cx);
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
            CoreEvent::Thinking { session, text, .. } => {
                if !self.is_active(&session) {
                    return;
                }
                // 同一次思考逐 token 追加到末尾那条上；
                // 中间隔着工具调用时另起一条，保留先后顺序
                match self.messages.last_mut() {
                    Some(m) if matches!(m.kind, MsgKind::Thinking) => m.text.push_str(&text),
                    _ => self.messages.push(MsgItem {
                        kind: MsgKind::Thinking,
                        text,
                    }),
                }
                self.status = "思考中…".into();
                self.sync_scroller(cx);
            }
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
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
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

/// 往当前组里追加一步调用，没有组就新开一个。
fn push_tool_step(items: &mut Vec<MsgItem>, step: ToolStep) {
    if let Some(MsgItem { kind: MsgKind::ToolGroup { steps }, .. }) = items.last_mut() {
        steps.push(step);
        return;
    }
    items.push(MsgItem {
        kind: MsgKind::ToolGroup { steps: vec![step] },
        text: String::new(),
    });
}

/// 把一条工具结果回填到组里最后一个尚未配对的步骤。
///
/// 返回 `false` 表示这是一条**孤立结果**（调用记录丢了，比如老数据），
/// 由调用方决定怎么摆。
fn merge_tool_result(items: &mut [MsgItem], ok: bool, output: String) -> bool {
    for item in items.iter_mut().rev() {
        if let MsgKind::ToolGroup { steps } = &mut item.kind {
            // 从后往前找第一个没结果的步骤
            for step in steps.iter_mut().rev() {
                if step.result.is_none() {
                    step.result = Some(ToolOutcome { ok, output });
                    return true;
                }
            }
            // 这一组都配完了，继续往前找上一组
            continue;
        }
        // 遇到非工具消息就停：结果不该跨过对话内容去认领调用
        return false;
    }
    false
}

/// 把纯文本包成 markdown 代码块。
///
/// 用四个反引号而不是三个：工具输出里常有 ``` （diff、markdown 文件），
/// 三个反引号会被提前闭合，后面的内容全变成正文。
fn as_code_block(text: &str, lang: &str) -> String {
    // 语言名决定高亮器用哪套规则；空串表示不做高亮
    format!("````{lang}\n{text}\n````")
}

/// 工具卡片里代码块的样式：无边框、和主信息流同色。
///
/// 工具输出不是"代码片段"，套上卡片式的底和边只会让人以为
/// 那是另一层可以点开的东西 —— 它就该是连成一片的正文。
fn md_style_flat(scale: f32) -> TextViewStyle {
    let mut s = StyleRefinement::default();
    // 白底托在灰卡上，再用一道浅边勾出范围：
    // 纯靠底色差分层在浅色主题下几乎看不出来
    s.background = Some(rgb(BG_MAIN).into());
    s.border_widths = EdgesRefinement {
        top: Some(AbsoluteLength::from(px(1.))),
        right: Some(AbsoluteLength::from(px(1.))),
        bottom: Some(AbsoluteLength::from(px(1.))),
        left: Some(AbsoluteLength::from(px(1.))),
    };
    s.border_color = Some(rgb(BORDER_SOFT).into());
    md_style(scale).code_block(s)
}

/// 代码块高亮用的主题。
///
/// 必须是**另一个 Arc**：组件库里拿 `Arc::ptr_eq` 和默认主题比对，
/// 相同就认为"调用方没设置"，直接不挂高亮器。所以这里克隆一份
/// 内容相同但地址不同的实例，且只建一次 —— 每帧新建会让代码块
/// 每帧重新解析。
fn hl_theme() -> std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme> {
    static HL: std::sync::OnceLock<std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>> =
        std::sync::OnceLock::new();
    HL.get_or_init(|| {
        std::sync::Arc::new((*gpui_kit::component::highlighter::HighlightTheme::default_light()).clone())
    })
    .clone()
}

/// 从工具参数里猜语言，用于高亮结果块。
///
/// 只做后缀映射：猜错比猜对更容易误导（把 shell 输出标成 bash 会让
/// 大段文字被当成注释染色），所以认不出来就不标。
fn lang_from_args(args: &str) -> &'static str {
    const MAP: &[(&str, &str)] = &[
        (".rs", "rust"),
        (".toml", "toml"),
        (".json", "json"),
        (".py", "python"),
        (".js", "javascript"),
        (".ts", "typescript"),
        (".tsx", "tsx"),
        (".go", "go"),
        (".md", "markdown"),
        (".yml", "yaml"),
        (".yaml", "yaml"),
        (".sh", "bash"),
        (".html", "html"),
        (".css", "css"),
    ];
    MAP.iter()
        .find(|(ext, _)| args.contains(ext))
        .map(|(_, lang)| *lang)
        .unwrap_or("")
}

/// 消息里 markdown 的统一样式：标题压层级、代码块不夺目。
fn md_style(scale: f32) -> TextViewStyle {
    let mut st = TextViewStyle::default()
        .heading_font_size(move |level, _base| {
            let size = match level {
                1 => 19.0,
                2 => 17.0,
                3 => 15.5,
                _ => 14.5,
            };
            px(size * scale)
        })
        // 工具输出常整段包在代码块里：去掉默认的重底色，
        // 让它读起来还是"一段输出"，不是"一块代码"
        .code_block({
            let mut s = StyleRefinement::default();
            s.background = Some(gpui_kit::Hsla::transparent_black().into());
            s
        });
    // 不设这个的话代码块永远是纯色文本：组件库拿 Arc 指针判断
    // 「调用方有没有换过主题」，指针相同就当没设置
    st.highlight_theme = hl_theme();
    st
}

/// 一条消息的右键菜单。
///
/// 目前的动作只有「复制整条」：跨元素拖选（消息正文 + 工具输出）
/// 在 gpui 里还做不到，先给一个能一次拿走全文的出口。
fn copy_context_menu(
    text: String,
) -> impl Fn(
    gpui_kit::component::menu::PopupMenu,
    &mut Window,
    &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu
       + 'static {
    move |menu, _w, _cx| {
        let t = text.clone();
        menu.item(PopupMenuItem::new("复制").on_click(move |_, _w, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(t.clone()));
        }))
    }
}

impl ChatView {
    /// 渲染一条消息。
    ///
    /// `scale` 让子智能体面板（420px 窄栏）用略小一号的字，
    /// 否则同样的行宽在窄栏里显得拥挤。
    fn render_message(
        &self,
        m: &MsgItem,
        idx: usize,
        scale: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match &m.kind {
            MsgKind::User => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(role_label("USER"))
                .child(
                    // 用 TextView 而不是裸 div：gpui 里只有 TextView 的文字
                    // 能用鼠标选中，div 里的字符串选不了
                    TextView::markdown(
                        ElementId::Name(format!("md-user-{idx}").into()),
                        m.text.clone(),
                    )
                    .text_size(px(USER_FONT * scale))
                    .line_height(px(MSG_LINE * scale))
                    .text_color(rgb(TEXT))
                    .style(md_style(scale)),
                )
                .context_menu(copy_context_menu(m.text.clone()))
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
                    .child(
                        TextView::markdown(
                            ElementId::Name(format!("md-ai-{idx}").into()),
                            m.text.clone(),
                        )
                            .text_size(px(MSG_FONT * scale))
                            .line_height(px(MSG_LINE * scale))
                            // 标题默认按 14px 基准放大，h2 能到 24px——
                            // 在消息流里比正文抢眼太多。压到这个比例后，
                            // 层级还在，但不再喧宾夺主。
                            .style(md_style(scale))
                            .into_any_element(),
                    )
                    .context_menu(copy_context_menu(m.text.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            // 靠右收尾：文字按钮占一整块太抢眼，
                            // 一个图标悬在段落末尾就够
                            .justify_end()
                            .child(
                                Button::new(ElementId::Name(format!("copy-{idx}").into()))
                                    .ghost()
                                    .compact()
                                    .icon(IconName::Copy)
                                    .tooltip("复制原文")
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

            MsgKind::ToolGroup { steps } => {
                // 默认整组收起，只占一行：一轮里连着七八次调用时，
                // 一条一卡会把真正要读的回复顶出屏幕
                // key 带会话前缀：否则切到另一个会话时，
                // 同一行号上的工具组会继承这边的展开状态
                let key = format!("{}/{}", self.active_session, m.key(idx));
                let expanded = self.expanded_tools.contains(&key);
                let step_prefix = key.clone();

                let total = steps.len();
                let done = steps.iter().filter(|s| s.result.is_some()).count();
                let failed = steps
                    .iter()
                    .filter(|s| matches!(&s.result, Some(r) if !r.ok))
                    .count();

                let (dot_bg, dot_fg) = if failed > 0 {
                    (DANGER_SOFT, DANGER)
                } else if done == total {
                    (OK_SOFT, OK)
                } else {
                    (BRAND_SOFT, BRAND)
                };

                let head = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                            let key = key.clone();
                            if !this.expanded_tools.remove(&key) {
                                this.expanded_tools.insert(key);
                            }
                            cx.notify();
                        }),
                    )
                    .child(
                        div().text_color(rgb(TEXT_DIM)).child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .text_size(px(12.0)),
                        ),
                    )
                    .child(
                        Icon::default()
                            .path("icons/wrench.svg")
                            .text_size(px(13.0))
                            .text_color(rgb(TEXT_MUTED)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT))
                            .child(if total == 1 {
                                "1 个工具调用".to_string()
                            } else {
                                format!("{total} 个工具调用")
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(if done < total {
                                format!("进行中 {done}/{total}")
                            } else if failed > 0 {
                                format!("{failed} 个失败")
                            } else {
                                "全部完成".to_string()
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .size(px(16.0))
                            .rounded_full()
                            .bg(rgb(dot_bg))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(if done < total {
                                div().size(px(6.0)).rounded_full().bg(rgb(dot_fg)).into_any_element()
                            } else {
                                Icon::new(if failed > 0 {
                                    IconName::Close
                                } else {
                                    IconName::Check
                                })
                                .text_size(px(10.0))
                                .text_color(rgb(dot_fg))
                                .into_any_element()
                            }),
                    );

                let mut card = div()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .bg(rgb(BG_RESULT))
                    .overflow_hidden()
                    .child(head);

                if expanded {
                    // 组内每一步仍是独立小卡片，各自折叠 ——
                    // 一个组里常有七八次调用，全铺开等于没分组
                    let mut body = div().flex().flex_col().gap_2().px_3().pb_3();
                    for (i, step) in steps.iter().enumerate() {
                        let step_key = format!("{step_prefix}-step-{i}");
                        let step_open = self.expanded_tools.contains(&step_key);

                        let (s_dot_bg, s_dot_fg) = match &step.result {
                            None => (BRAND_SOFT, BRAND),
                            Some(r) if r.ok => (OK_SOFT, OK),
                            Some(_) => (DANGER_SOFT, DANGER),
                        };

                        let mut head = div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let step_key = step_key.clone();
                                    move |this, _: &MouseDownEvent, _w, cx| {
                                        let k = step_key.clone();
                                        if !this.expanded_tools.remove(&k) {
                                            this.expanded_tools.insert(k);
                                        }
                                        cx.notify();
                                    }
                                }),
                            )
                            .child(
                                div().text_color(rgb(TEXT_DIM)).child(
                                    Icon::new(if step_open {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .text_size(px(12.0)),
                                ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(TEXT))
                                    .child(if step.name.is_empty() {
                                        "工具".to_string()
                                    } else {
                                        tool_display_name(&step.name).to_string()
                                    }),
                            );

                        if !step.risk.is_empty() {
                            let (bg, fg) = match step.risk.as_str() {
                                "高危" => (DANGER_SOFT, DANGER),
                                "有影响" => (BRAND_SOFT, BRAND),
                                _ => (BG_HOVER, TEXT_MUTED),
                            };
                            head = head.child(pill(step.risk.clone(), bg, fg));
                        }

                        // 参数里带 path 的（读写文件的工具）给一个可点的文件名：
                        // 点开就在右侧面板里预览，不用去终端翻
                        if let Some(full) = path_from_args(&step.args) {
                            let short = full
                                .rsplit('/')
                                .next()
                                .unwrap_or(full.as_str())
                                .to_string();
                            head = head.child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(BRAND)))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                            // 别让点击冒到分组头上把整组展开/收起
                                            cx.stop_propagation();
                                            this.open_file_tab(&full, cx);
                                        }),
                                    )
                                    .child(short),
                            );
                        }

                        head = head
                            .child(
                                div()
                                    .size(px(16.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(rgb(s_dot_bg))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(match &step.result {
                                        None => div()
                                            .size(px(6.0))
                                            .rounded_full()
                                            .bg(rgb(s_dot_fg))
                                            .into_any_element(),
                                        Some(r) => Icon::new(if r.ok {
                                            IconName::Check
                                        } else {
                                            IconName::Close
                                        })
                                        .text_size(px(10.0))
                                        .text_color(rgb(s_dot_fg))
                                        .into_any_element(),
                                    }),
                            )
                            .child(div().flex_1());

                        let mut step_card = div()
                            .flex()
                            .flex_col()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER_SOFT))
                            .bg(rgb(BG_MAIN))
                            .overflow_hidden()
                            .child(head);

                        if step_open {
                            let mut sb = div().flex().flex_col().gap_2().px_3().pb_2();
                            if !step.args.trim().is_empty() {
                                sb = sb.child(
                                    TextView::markdown(
                                        ElementId::Name(format!("md-call-{idx}-{i}").into()),
                                        as_code_block(&step.args, "json"),
                                    )
                                    .text_size(px(12.5))
                                    .line_height(px(19.0))
                                    .text_color(rgb(TEXT))
                                    .style(md_style_flat(1.0)),
                                );
                            }
                            if let Some(r) = &step.result {
                                if !r.output.trim().is_empty() {
                                    sb = sb.child(
                                        TextView::markdown(
                                            ElementId::Name(format!("md-out-{idx}-{i}").into()),
                                            as_code_block(
                                                &truncate_for_display(&r.output, 2000),
                                                lang_from_args(&step.args),
                                            ),
                                        )
                                        .text_size(px(12.5))
                                        .line_height(px(19.0))
                                        .text_color(rgb(TEXT_MUTED))
                                        .style(md_style_flat(1.0)),
                                    );
                                }
                            }
                            step_card = step_card.child(sb);
                        }

                        body = body.child(
                            step_card.context_menu(copy_context_menu(format!(
                                "{}({}) → {}",
                                step.name,
                                step.args,
                                step.result
                                    .as_ref()
                                    .map(|r| r.output.as_str())
                                    .unwrap_or("（未完成）")
                            ))),
                        );
                    }
                    card = card.child(body);
                }

                let copy_text = steps
                    .iter()
                    .map(|s| {
                        format!(
                            "{}({}) → {}",
                            s.name,
                            s.args,
                            s.result.as_ref().map(|r| r.output.as_str()).unwrap_or("（未完成）")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                card.context_menu(copy_context_menu(copy_text))
                    .into_any_element()
            }

            MsgKind::SubAgentStep { agent, name, args } => {
                // 缩进 + 竖线：视觉上明确属于「后台那几个」，不抢主线注意力
                div()
                    .ml_10()
                    .pl_3()
                    .border_l_2()
                    .border_color(rgb(BORDER))
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(TEXT_MUTED))
                            .child(format!("⟡ {agent}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family(MONO)
                            .text_color(rgb(TEXT_DIM))
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .overflow_hidden()
                            .child(args.clone()),
                    )
                    .into_any_element()
            }

            MsgKind::Thinking => {
                // 展开状态和工具卡片共用一套 key（会话+行号），
                // 否则切到别的会话会继承这边的开合状态
                let key = format!("{}/thinking-{idx}", self.active_session);
                let expanded = self.expanded_tools.contains(&key);
                let chars = m.text.chars().count();

                let mut card = div()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .bg(rgb(BG_TOOL))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let key = key.clone();
                                    move |this, _: &MouseDownEvent, _w, cx| {
                                        let k = key.clone();
                                        if !this.expanded_tools.remove(&k) {
                                            this.expanded_tools.insert(k);
                                        }
                                        cx.notify();
                                    }
                                }),
                            )
                            .child(
                                div().text_color(rgb(TEXT_DIM)).child(
                                    Icon::new(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .text_size(px(12.0)),
                                ),
                            )
                            .child(
                                Icon::new(IconName::Brain)
                                    .text_size(px(12.0))
                                    .text_color(rgb(TEXT_DIM)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child("思考过程"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_DIM))
                                    .child(format!("· {chars} 字")),
                            )
                            .child(div().flex_1()),
                    );

                if expanded {
                    card = card.child(
                        div().px_3().pb_3().child(
                            TextView::markdown(
                                ElementId::Name(format!("md-think-{idx}").into()),
                                m.text.clone(),
                            )
                            .text_size(px(12.5))
                            .line_height(px(19.0))
                            .text_color(rgb(TEXT_MUTED))
                            .style(md_style_flat(1.0)),
                        ),
                    );
                }

                card.into_any_element()
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
    fn render_dashboard(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
        col = col.child(self.render_composer(window, cx));

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
    /// 关窗确认层。
    ///
    /// 用户按了关闭但后台还有任务 —— 直接退会把活儿掐了，
    /// 所以停下来问一句，把"会中断什么"说清楚。
    fn render_close_confirm(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.team.iter().filter(|a| a.status == "执行中").count();

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x1A1A1A).opacity(0.35))
            .child(
                div()
                    .w(px(400.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG_MAIN))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("还有任务在跑"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT_MUTED))
                            .child(if running > 0 {
                                format!("{running} 个子智能体正在执行。现在关闭会中断它们，已完成的进度会保留。")
                            } else {
                                "有任务正在执行。现在关闭会中断它，已完成的进度会保留。".to_string()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-close")
                                    .ghost()
                                    .label("继续运行")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                        this.confirm_close = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-close")
                                    .label("仍然关闭")
                                    .on_click(cx.listener(|_this, _: &ClickEvent, _w, cx| {
                                        cx.quit();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
    /// 团队面板：主对话里直接看到子智能体在干什么。
    ///
    /// 放在对话下方而不是侧栏，是因为它属于「这一轮正在发生的事」；
    /// 侧栏留给会话，两者是不同的层级。
    /// 右侧面板：子智能体的完整过程。
    ///
    /// 独立于主对话：用户想「看一眼它在干什么」，不是「离开当前对话」。
    /// 右侧面板：标签栏 + 当前页。
    fn render_right_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(rp) = &self.right_panel else {
            return div().into_any_element();
        };
        if rp.tabs.is_empty() {
            return div().into_any_element();
        }
        let active = rp.active.min(rp.tabs.len() - 1);

        // ─── 标签栏 ───
        let mut tabbar = div()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(36.0))
            .flex_none()
            .border_b_1()
            .border_color(rgb(BORDER_SOFT))
            .overflow_hidden();

        for (i, tab) in rp.tabs.iter().enumerate() {
            let is_active = i == active;
            let (dot, dot_bg) = match tab {
                PanelTab::SubAgent(p) => match p.status.as_str() {
                    "执行中" => (BRAND, BRAND_SOFT),
                    "已完成" => (OK, OK_SOFT),
                    "失败" => (DANGER, DANGER_SOFT),
                    _ => (TEXT_DIM, BG_HOVER),
                },
                PanelTab::File(_) => (TEXT_DIM, BG_HOVER),
            };

            let chip = div()
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .h(px(26.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .min_w(px(0.0))
                // 激活页常亮，其余 hover 才亮
                .bg(rgb(if is_active { BG_HOVER } else { BG_MAIN }))
                .hover(|s| s.bg(rgb(BG_HOVER)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                        if let Some(p) = &mut this.right_panel {
                            p.active = i;
                        }
                        cx.notify();
                    }),
                )
                // 每页的状态点：红色=失败，橙色=跑着，绿色=完成
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(rgb(dot)))
                .child(
                    div()
                        .max_w(px(170.0))
                        .truncate()
                        .text_xs()
                        .text_color(rgb(if is_active { TEXT } else { TEXT_MUTED }))
                        .child(tab.title()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.0))
                        .rounded(px(4.0))
                        .flex_none()
                        .hover(|s| s.bg(rgb(dot_bg)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                // 别让关闭的按下冒到 chip 上切成这一页
                                cx.stop_propagation();
                                this.close_panel_tab(i, cx);
                            }),
                        )
                        .child(
                            Icon::new(IconName::Close)
                                .text_size(px(10.0))
                                .text_color(rgb(TEXT_DIM)),
                        ),
                );

            tabbar = tabbar.child(chip);
        }
        tabbar = tabbar.child(div().flex_1());

        // ─── 当前页内容 ───
        let body = match &rp.tabs[active] {
            PanelTab::SubAgent(p) => self.render_agent_body(p, cx),
            PanelTab::File(f) => self.render_file_body(f),
        };

        // 左边缘的拖拽手柄：5px 宽，够点中又不占地方。
        // 真正的位置计算在根容器的 on_mouse_move 里 ——
        // 只在手柄上听 move 的话，鼠标一滑出手柄就丢事件。
        let handle = div()
            .id("subagent-resize")
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            // 常态和面板同色（看着就是一条边），
            // 悬停才亮出来 —— 否则一条 5px 的色带天天杵在那儿太吵
            .hover(|s| s.bg(rgb(BRAND_BORDER)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _w, cx| {
                    this.subagent_resizing = true;
                    this.subagent_drag_origin = (f32::from(e.position.x), this.subagent_panel_width);
                    cx.notify();
                }),
            );

        div()
            .w(px(self.subagent_panel_width))
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            // 和主对话区同色：靠左边框划界就够了，
            // 再叠一层底色反而显得是两个不相干的窗口
            .bg(rgb(BG_MAIN))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(handle)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(tabbar)
                    .child(body),
            )
            .into_any_element()
    }

    /// 子智能体页：职责说明 + 消息流。
    fn render_agent_body(&self, panel: &SubAgentPanel, cx: &mut Context<Self>) -> AnyElement {
        let mut body = div()
            // id 带上会话：每个标签各自记住滚动位置。
            // 共用一个 id 的话，切标签时滚动量会互相覆盖，
            // 表现就是"切回来内容又从头开始了"
            .id(ElementId::Name(format!("agent-scroll-{}", panel.session).into()))
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4();

        for (i, m) in panel.messages.iter().enumerate() {
            // 窄栏用 0.93 倍：字号小一点，行宽才不至于挤
            body = body.child(self.render_message(m, i + 10_000, 0.93, cx));
        }
        // 流式中的文本：单独一块
        if let Some(buf) = &panel.streaming {
            if !buf.trim().is_empty() {
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(role_label("THINKING"))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .line_height(px(21.0))
                                .text_color(rgb(TEXT_MUTED))
                                .child(buf.clone()),
                        ),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(panel.description.clone()),
            )
            .child(body)
            .into_any_element()
    }

    /// 文件页：路径 + 内容（超长已截断）。
    fn render_file_body(&self, f: &FilePreview) -> AnyElement {
        let mut body = div()
            // 同上：按文件路径区分滚动状态
            .id(ElementId::Name(format!("file-scroll-{}", f.path).into()))
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(f.path.clone()),
            );

        if let Some(note) = &f.note {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(rgb(DANGER))
                    .child(note.clone()),
            );
        }

        if !f.content.trim().is_empty() {
            body = body.child(
                TextView::markdown(
                    ElementId::Name(format!("file-{}", f.path).into()),
                    as_code_block(&f.content, &f.lang),
                )
                .text_size(px(12.5))
                .line_height(px(19.0))
                .text_color(rgb(TEXT))
                .style(md_style_flat(1.0)),
            );
        }

        body.into_any_element()
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

    /// 自绘标题栏。
    ///
    /// 系统标题栏已经关掉（Linux 靠 `WindowDecorations::Client`，
    /// macOS 靠 `appears_transparent`），拖动、双击最大化、
    /// 最小化/关闭都得自己实现。
    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let maxed = window.is_maximized();

        let mut bar = div()
            .flex()
            .items_center()
            .flex_none()
            .h(px(38.0))
            .w_full()
            .bg(rgb(BG_SIDEBAR))
            .border_b_1()
            .border_color(rgb(BORDER))
            // 整条都是拖动区：双击最大化，单击跟手拖走
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, ev: &MouseDownEvent, window, cx| {
                    if ev.click_count == 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                    cx.stop_propagation();
                }),
            );

        // macOS 的系统红绿灯还在左上角，给它让出位置
        bar = if cfg!(target_os = "macos") {
            bar.pl(px(78.0))
        } else {
            bar.pl(px(16.0))
        };

        // 品牌跟着标题栏走：窗口最上沿统一是标题栏，
        // 侧栏从那下面才开始
        bar = bar
            .child(
                div()
                    .size(px(22.0))
                    .rounded(px(6.0))
                    .bg(rgb(BRAND))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xFFFFFF))
                            .child("Z"),
                    ),
            )
            .child(div().w(px(8.0)))
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(TEXT))
                    .child("Zex"),
            )
            .child(div().flex_1());

        // macOS 用系统红绿灯，不重复画按钮
        if !cfg!(target_os = "macos") {
            let btn = |icon: IconName,
                       danger: bool,
                       action: fn(&mut Window),
                       cx: &mut Context<Self>| {
                let base = div()
                    .w(px(34.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    // 按下就吃掉：否则事件冒到标题栏，点按钮会顺带把窗口拖起来
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _e: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |_this, _e: &MouseUpEvent, window, _cx| {
                            action(window);
                        }),
                    );
                // 关闭键 hover 变红，警告意味明确
                let base = if danger {
                    base.hover(|s| s.bg(rgb(0xE0383E)))
                } else {
                    base.hover(|s| s.bg(rgb(BG_HOVER)))
                };
                base.child(
                    Icon::new(icon)
                        .text_size(px(13.0))
                        .text_color(rgb(TEXT_MUTED)),
                )
            };

            bar = bar
                .child(btn(IconName::Minus, false, |w| w.minimize_window(), cx))
                .child(btn(
                    if maxed {
                        IconName::Minimize
                    } else {
                        IconName::Square
                    },
                    false,
                    |w| w.zoom_window(),
                    cx,
                ))
                .child(btn(IconName::Close, true, |w| w.remove_window(), cx));
        }

        bar.into_any_element()
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
            // 品牌挪到标题栏了，这里直接从"新建会话"开始
            .child(
                div().px_3().pt_2().pb_3().child(
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
                        // 「＋ 新建会话」下面补一个回工作台的入口：
                        // 顶栏删掉之后，看进度/进度的首页就没地方进了
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
            // 工作台入口：顶栏去掉之后，首页没有别的路可走了
            .child(
                div().px_3().pb_2().child(
                    div()
                        .w_full()
                        .h(px(34.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .text_sm()
                        .text_color(rgb(TEXT_MUTED))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(BG_HOVER)))
                        .child(Icon::default().path("icons/layout-dashboard.svg").text_size(px(14.0)))
                        .child("工作台")
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _ev, _window, cx| {
                            this.mode = ViewMode::Dashboard;
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

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // 附件条：有东西才占位
        let mut attach_row = div().flex().items_center().gap_2().flex_wrap();
        for (i, a) in self.attachments.iter().enumerate() {
            let is_img = a.image_b64.is_some();
            attach_row = attach_row.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG_TOOL))
                    .child(
                        div()
                            .text_xs()
                            .child(if is_img { "🖼" } else { "📄" }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(if a.name.chars().count() > 24 {
                                let n: String = a.name.chars().take(24).collect();
                                format!("{n}…")
                            } else {
                                a.name.clone()
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("drop-attach-{i}")))
                            .ghost()
                            .icon(Icon::new(IconName::Close))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.attachments.remove(i);
                                cx.notify();
                            })),
                    ),
            );
        }

        // 聚焦时边框变品牌色：输入框是主交互面，得让人一眼看出
        // 现在敲的字会去哪儿
        let focused = self
            .input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let border = if focused { BRAND } else { BORDER };

        let mut composer = div().flex().flex_col().gap_2();

        // 弹出菜单（命令 / 文件 / 历史共用一个渲染）
        if let Some(menu) = self.render_menu(cx) {
            composer = composer.child(menu);
        }

        if !self.attachments.is_empty() {
            composer = composer.child(attach_row);
        }
        // 语音和提交按钮放进输入框内部右下角：它们属于"这条消息"
        // 的一部分，摆在外面会让人以为和输入内容无关。
        let voice_btn = {
            let (icon, danger) = match (&self.recording, self.transcribing) {
                (Some(_), _) => (IconName::Square, true),
                (None, true) => (IconName::LoaderCircle, false),
                _ => (IconName::Mic, false),
            };
            let btn = Button::new("voice")
                .icon(Icon::new(icon))
                .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                    this.toggle_recording(cx);
                }));
            if danger {
                btn.danger().into_any_element()
            } else {
                btn.into_any_element()
            }
        };
        let send_btn = match self.streaming.is_some() {
            // 正在跑就换成停止键：否则用户只能干等长任务跑完
            true => Button::new("stop")
                .danger()
                .label("■")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.cancel(cx)))
                .into_any_element(),
            false => Button::new("send")
                .primary()
                .icon(Icon::new(IconName::ArrowUp))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.submit(window, cx)
                }))
                .into_any_element(),
        };
        // 模型切换放在输入框内部左下角：它和"这条消息"一起生效，
        // 摆到框外会被当成全局设置。
        let model_picker = if self.editing_model {
            div()
                .w(px(180.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(rgb(BRAND))
                .px_2()
                .py_1()
                .child(Input::new(&self.model_input))
                .into_any_element()
        } else {
            // selected_model 是 `provider/model`；按钮上只留模型名，
            // provider 在菜单里当分组标题
            let cur_full = self
                .selected_model
                .clone()
                .unwrap_or_else(|| format!("{}/{}", self.active_provider, self.model));
            let cur_label = cur_full
                .split('/')
                .nth(1)
                .unwrap_or(&cur_full)
                .to_string();
            let groups: Vec<(String, Vec<String>)> = self
                .models_by_provider
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let view = cx.entity().downgrade();

            Button::new("switch-model")
                .ghost()
                .label(cur_label)
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, window, cx| {
                    let mut menu = menu;
                    // 一级：provider；二级：这个 provider 下的模型
                    for (provider, models) in groups.iter() {
                        let provider_name = provider.clone();
                        let models = models.clone();
                        let view = view.clone();
                        let cur_full = cur_full.clone();
                        menu = menu.submenu(
                            provider_name.clone(),
                            window,
                            cx,
                            move |submenu, _w, _c| {
                                let mut submenu = submenu;
                                for m in models.iter() {
                                    let full = format!("{provider_name}/{m}");
                                    let selected = full == cur_full;
                                    let view = view.clone();
                                    submenu = submenu.item(
                                        PopupMenuItem::new(m.clone())
                                            .checked(selected)
                                            .on_click(move |_, _w, cx| {
                                                let full = full.clone();
                                                view.update(cx, |this, cx| {
                                                    this.selected_model = Some(full.clone());
                                                    // 落到会话上：切走再切回来还是这个模型
                                                    let sid = zex_core::SessionId::from(
                                                        this.active_session.clone(),
                                                    );
                                                    let _ =
                                                        this.store.set_session_model(&sid, &full);
                                                    if let Some(item) = this
                                                        .sessions
                                                        .iter_mut()
                                                        .find(|s| s.id == this.active_session)
                                                    {
                                                        item.model = full.clone();
                                                    }
                                                    cx.notify();
                                                })
                                                .ok();
                                            }),
                                    );
                                }
                                submenu
                            },
                        );
                    }
                    // 配置里没声明的模型也允许手填：provider 组合是用户自己写的
                    menu.separator().item(
                        PopupMenuItem::new("手动输入模型名…").on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                view.update(cx, |this, cx| this.begin_model_edit(window, cx))
                                    .ok();
                            }
                        }),
                    )
                })
                .into_any_element()
        };

        // 运行模式：决定这一轮工具调用的放行尺度。
        // 放在模型旁边，因为两者都是"这条消息怎么跑"的开关。
        let mode_picker = {
            let cur = self.run_mode;
            let view = cx.entity().downgrade();
            Button::new("run-mode")
                .ghost()
                .icon(Icon::default().path(mode_icon_path(cur)))
                .label(cur.label())
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
                    // 每项两行：名字 + 一句话。光看名字，
                    // 「自动编辑」和「完全访问」差在哪没人猜得出来。
                    let mut menu = menu.min_w(px(240.0));
                    for m in zex_core::RunMode::all() {
                        let view = view.clone();
                        let selected = m == cur;
                        menu = menu.item(
                            PopupMenuItem::element(move |_w, _c| {
                                let mut row = div()
                                    .flex()
                                    .items_start()
                                    .gap_3()
                                    .w_full()
                                    .child(
                                        Icon::default().path(mode_icon_path(m))
                                            .text_size(px(18.0))
                                            .text_color(rgb(TEXT_MUTED)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .child(div().text_sm().child(m.label()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(TEXT_DIM))
                                                    .child(m.description()),
                                            ),
                                    );
                                if selected {
                                    row = row.child(
                                        Icon::new(IconName::Check)
                                            .text_size(px(14.0))
                                            .text_color(rgb(TEXT_MUTED)),
                                    );
                                }
                                row
                            })
                            .on_click(move |_, _w, cx| {
                                view.update(cx, |this, cx| {
                                    this.run_mode = m;
                                    let sid =
                                        zex_core::SessionId::from(this.active_session.clone());
                                    let _ = this.store.set_session_mode(&sid, m);
                                    if let Some(item) = this
                                        .sessions
                                        .iter_mut()
                                        .find(|s| s.id == this.active_session)
                                    {
                                        item.mode = m;
                                    }
                                    cx.notify();
                                })
                                .ok();
                            }),
                        );
                    }
                    menu
                })
                .into_any_element()
        };

        // 团队入口：只在当前会话真的派过子智能体时出现。
        // 它是「查看」而不是「设置」，所以和模型/模式同一行摆着，
        // 但不带下拉箭头 —— 点开是名单，不是一个可选项。
        let team_entry = {
            let members: Vec<(String, String, String)> = self
                .visible_team()
                .iter()
                .map(|a| (a.id.clone(), a.status.clone(), a.description.clone()))
                .collect();
            if members.is_empty() {
                None
            } else {
                let running = members.iter().filter(|(_, s, _)| s == "执行中").count();
                let label = if running > 0 {
                    format!("团队 {} · {} 进行中", members.len(), running)
                } else {
                    format!("团队 {}", members.len())
                };
                let view = cx.entity().downgrade();
                Some(
                    Button::new("team-menu")
                        .ghost()
                        .icon(Icon::default().path("icons/users.svg"))
                        .label(label)
                        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _w, _cx| {
                            let mut menu = menu.min_w(px(260.0));
                            for (id, status, desc) in members.iter() {
                                let view = view.clone();
                                let open_id = id.clone();
                                let (id, status, desc) =
                                    (id.clone(), status.clone(), desc.clone());
                                menu = menu.item(
                                    PopupMenuItem::element(move |_w, _c| {
                                        let (dot, dot_bg) = match status.as_str() {
                                            "执行中" => (BRAND, BRAND_SOFT),
                                            "已完成" => (OK, OK_SOFT),
                                            "失败" => (DANGER, DANGER_SOFT),
                                            _ => (TEXT_DIM, BG_HOVER),
                                        };
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .w_full()
                                            .child(
                                                div()
                                                    .size(px(6.0))
                                                    .rounded_full()
                                                    .bg(rgb(dot)),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(id.clone()),
                                            )
                                            .child(
                                                div()
                                                    .px_1p5()
                                                    .rounded_sm()
                                                    .bg(rgb(dot_bg))
                                                    .text_color(rgb(dot))
                                                    .text_xs()
                                                    .child(status.clone()),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.0))
                                                    .text_xs()
                                                    .text_color(rgb(TEXT_MUTED))
                                                    .child(desc.clone()),
                                            )
                                    })
                                    .on_click(move |_, _w, cx| {
                                        let id = open_id.clone();
                                        view.update(cx, |this, cx| {
                                            this.open_subagent_panel(&id, cx)
                                        })
                                        .ok();
                                    }),
                                );
                            }
                            menu
                        })
                        .into_any_element(),
                )
            }
        };

        // 选文件/图片：拖拽之外的第二条入口。
        // 拖拽对触控板和远程桌面都不友好，没有按钮等于没有这个功能。
        let attach_btn = Button::new("pick-attachments")
            .ghost()
            .icon(Icon::default().path("icons/paperclip.svg"))
            .tooltip("添加附件")
            .on_click(cx.listener(|_this, _: &ClickEvent, _w, cx| {
                let rx = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: true,
                    prompt: Some("选择要添加的文件".into()),
                });
                cx.spawn(async move |this, cx| {
                    match rx.await {
                        Ok(Ok(Some(paths))) => {
                            this.update(cx, |this, cx| this.add_attachments(&paths, cx))
                                .ok();
                        }
                        // 用户取消：什么都不做
                        Ok(Ok(None)) => {}
                        // Linux 上没有 xdg-desktop-portal 就会走到这里，
                        // 静默失败的话用户只会觉得「按钮是坏的」
                        Ok(Err(e)) => {
                            this.update(cx, |this, cx| {
                                this.status = format!("打开文件选择器失败：{e}");
                                cx.notify();
                            })
                            .ok();
                        }
                        Err(_) => {}
                    }
                })
                .detach();
            }))
            .into_any_element();

        composer.child(
            div()
                .relative()
                .rounded(px(16.0))
                .border_1()
                .border_color(rgb(border))
                .bg(rgb(BG_INPUT))
                // 底部给按钮留出空间，文字不会钻到按钮底下
                .pb(px(44.0))
                .child(
                    div().px_4().pt_3().child(
                        Textarea::new(&self.input)
                            .appearance(false)
                            .bordered(false)
                            .h(px(self.composer_height(cx))),
                    ),
                )
                // 按钮浮在右下角：它们属于这条消息的一部分，
                // 摆在框外会让人以为是独立控件
                .child(
                    div()
                        .absolute()
                        .right(px(10.0))
                        .bottom(px(8.0))
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(team_entry)
                        .child(model_picker)
                        .child(voice_btn)
                        .child(send_btn),
                )
                // 左下角：模型切换。和右下角的发送键同处框内，
                // 用户一眼能看出"模型 + 内容"是同一条消息的属性
                .child(
                    div()
                        .absolute()
                        .left(px(10.0))
                        .bottom(px(8.0))
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(attach_btn)
                        .child(mode_picker),
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
                            .child("Enter 发送 · Shift+Enter 换行 · @ 引文件 · / 命令"),
                    )
                    .child({
                        // 输入预估 + 已消耗，两件事分开显示
                        let typed = estimate_tokens(&self.input.read(cx).value());
                        let used = self.usage.input_tokens + self.usage.output_tokens;
                        let mut label = String::new();
                        if typed > 0 {
                            label.push_str(&format!("输入约 {typed} · "));
                        }
                        label.push_str(&format!(
                            "已用 {}",
                            if used >= 1000 {
                                format!("{:.1}k", used as f64 / 1000.0)
                            } else {
                                format!("{used}")
                            }
                        ));
                        div()
                            .text_xs()
                            .text_color(if typed > 2000 { rgb(DANGER) } else { rgb(TEXT_DIM) })
                            .child(label)
                    }),
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
        // 后面还压着几个？写出来，免得用户以为处理完就没事了
        let queued = self.pending_approvals.len();
        let actions = if queued == 0 {
            actions
        } else {
            actions.child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(format!("还有 {queued} 个排队")),
            )
        };

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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        bottom = bottom.child(self.render_composer(window, cx));

        let mut root = div()
            .size_full()
            .relative()
            // 拖文件进来：图片进上下文，其它按路径引用
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.add_attachments(paths.paths(), cx);
            }))
            .key_context(if self.menu_is_open() {
                // 菜单优先：上下键在候选里走
                "Zex ZexMenu"
            } else if self.input.read(cx).value().trim().is_empty() {
                "Zex ZexEmpty"
            } else {
                "Zex"
            })
            .on_action(cx.listener(|this, _: &InterruptTask, _window, cx| {
                // 菜单开着时 Esc 先关菜单，不打断任务
                if this.menu_is_open() {
                    this.slash_open = false;
                    this.at_open = false;
                    this.history_search_open = false;
                    cx.notify();
                    return;
                }
                if this.streaming.is_some() {
                    this.cancel(cx);
                } else if this.right_panel.is_some() {
                    this.right_panel = None;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &HistorySearch, _window, cx| {
                this.history_search_open = !this.history_search_open;
                cx.notify();
            }))
            // 面板拖拽的移动与松手挂在整窗根容器上：
            // 手柄只有 5px 宽，鼠标一快就滑出去了，听手柄自己的
            // on_mouse_move 会在中途断掉。
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _w, cx| {
                if !this.subagent_resizing {
                    return;
                }
                let (origin_x, origin_w) = this.subagent_drag_origin;
                let delta = f32::from(e.position.x) - origin_x;
                // 手柄在左边缘：鼠标往左拖，面板变宽
                let w = (origin_w - delta).clamp(MIN_PANEL_WIDTH, MAX_PANEL_WIDTH);
                if (w - this.subagent_panel_width).abs() > 0.5 {
                    this.subagent_panel_width = w;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseUpEvent, _w, cx| {
                    if this.subagent_resizing {
                        this.subagent_resizing = false;
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .bg(rgb(BG_MAIN))
            .text_color(rgb(TEXT))
            .child(self.render_titlebar(window, cx))
            .child({
                let mut main = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(match self.mode {
                        ViewMode::Dashboard => self.render_dashboard(window, cx),
                        ViewMode::Chat => {
                            // 团队入口已经收进编辑器，底部不再挂卡片：
                            // 它挡在消息流和输入框之间，长对话时很碍事
                            feed.into_any_element()
                        }
                    });
                // 工作台模式下不需要输入框：还没决定要干什么
                if self.mode == ViewMode::Chat {
                    main = main.child(bottom);
                }
                let mut body = div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(main);
                // 面板是横排里的第三列。挂在 root 上会跑到标题栏下面去 ——
                // root 现在是竖排（标题栏 + 内容），面板会被当成第二行
                if self.right_panel.is_some() {
                    body = body.child(self.render_right_panel(cx));
                }
                body.into_any_element()
            });

        // 关窗确认：压在整界面之上，挡住所有交互
        if self.confirm_close {
            root = root.child(self.render_close_confirm(cx));
        }
        root
    }

}

/// 斜杠命令表。菜单渲染和键盘导航都用它。
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/new", "新会话"),
    ("/clear", "清空当前会话"),
    ("/model", "切换模型，可跟模型名"),
    ("/help", "命令说明"),
];

/// 粗略估算 token 数。
///
/// 不引 tiktoken 那种大依赖：这里只需要让用户知道"这一大段贴进去
/// 大概什么量级"。CJK 按 1 字 1 token，拉丁按 4 字符 1 token，
/// 误差 ±20% 不影响判断。
fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut ascii = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            cjk += 1;
        }
    }
    cjk + ascii.div_ceil(4)
}

/// 标准 base64 编码（带 = 填充）。
///
/// 只为了把图片塞进 data URL，不需要 URL-safe 变体。
fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
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

/// 工具名 → 界面显示名。未知工具原样返回。
fn tool_display_name(name: &str) -> &str {
    match name {
        "fs_read" => "读取文件",
        "fs_write" => "写入文件",
        "fs_edit" => "编辑文件",
        "fs_list" => "列出目录",
        "fs_grep" => "搜索内容",
        "shell" => "执行命令",
        "web_fetch" => "抓取网页",
        "web_search" => "搜索网页",
        "control_panel" => "控制面板",
        "echo" => "回声",
        "spawn_agent" => "创建子智能体",
        "send_message" => "发送消息",
        "wait_for_agents" => "等待子智能体",
        "team_status" => "查看团队",
        "terminate_agent" => "终止子智能体",
        "todo_write" => "更新待办",
        "ask_user" => "询问用户",
        other => other,
    }
}

/// 读一个文件给右侧面板预览。
///
/// 大文件截断：面板是"扫一眼"的地方，不是编辑器，
/// 把几十兆的日志整个塞进 TextView 只会把界面卡住。
fn load_file_preview(abs: &str, cwd: &str) -> FilePreview {
    const MAX_BYTES: usize = 256 * 1024;
    let prefix = format!("{}/", cwd.trim_end_matches('/'));
    let display = abs.strip_prefix(&prefix).unwrap_or(abs).to_string();
    let lang = lang_from_path(abs);
    let path = std::path::Path::new(abs);

    let mut note = None;
    let content = match std::fs::read(path) {
        Err(e) => {
            note = Some(format!("无法打开：{e}"));
            String::new()
        }
        Ok(bytes) => {
            let truncated = bytes.len() > MAX_BYTES;
            let slice = &bytes[..bytes.len().min(MAX_BYTES)];
            let text = String::from_utf8_lossy(slice).to_string();
            if truncated {
                note = Some(format!(
                    "文件较大（{} KB），只显示前 {} KB",
                    bytes.len() / 1024,
                    MAX_BYTES / 1024
                ));
            } else if text.trim().is_empty() {
                note = Some("（空文件）".to_string());
            }
            text
        }
    };

    FilePreview {
        path: abs.to_string(),
        display,
        content,
        lang,
        note,
    }
}

/// 按扩展名猜代码块的高亮语言，猜不出就返回空串（不着色）。
fn lang_from_path(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "go" => "go",
        "py" => "python",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "sh" | "bash" => "bash",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "ex" | "exs" => "elixir",
        "rb" => "ruby",
        "php" => "php",
        "xml" => "xml",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "java" => "java",
        _ => "",
    }
    .to_string()
}

/// 从工具参数里取文件路径。
///
/// README 里所有文件类工具（fs_read / fs_write / fs_edit）都用 `path`，
/// 取不到就返回 None —— 调用方据此决定要不要显示"打开文件"的入口。
fn path_from_args(args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    v.get("path").and_then(|x| x.as_str()).map(|s| s.to_string())
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
    let theme = cx.global_mut::<gpui_kit::component::Theme>();
    theme.font_family = candidate.into();
    // 组件默认 16px，对桌面端偏大：按钮、输入框、列表全都跟着涨。
    // 降到 15px，各处 text_sm/base 按比例落回正常观感。
    // 等宽保持 13px：代码块本来就靠小字号挤进窄栏，再降会糊。
    theme.font_size = px(15.0);
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
            Message::Assistant { content, tool_calls, .. } => {
                // 思考过程也要还原。它已经存在历史里（Part::Thinking），
                // 只是 m.text() 只拼 Text 片段 —— 不回放的话，
                // 重开会话就只剩结论，看不出当时是怎么想的
                let think: String = content
                    .iter()
                    .filter_map(|p| match p {
                        zex_core::Part::Thinking { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !think.trim().is_empty() {
                    out.push(MsgItem {
                        kind: MsgKind::Thinking,
                        text: think,
                    });
                }
                let t = m.text();
                if !t.trim().is_empty() {
                    out.push(MsgItem { kind: MsgKind::Assistant, text: t });
                }
                // 历史里的工具调用也要还原成卡片，否则回看会话时
                // 只剩一堆结果、看不出中间发生了什么
                for call in tool_calls {
                    push_tool_step(
                        &mut out,
                        ToolStep {
                            name: call.name.clone(),
                            args: compact_args(&call.arguments),
                            // 历史里没存风险等级，留空即不画胶囊
                            risk: String::new(),
                            result: None,
                        },
                    );
                }
            }
            Message::Tool { content, is_error, .. } => {
                // 结果的归属靠"前面那条还没配对的调用"，不靠工具名 ——
                // 同一轮里连着调两个 shell 时名字是重复的
                if !merge_tool_result(&mut out, !*is_error, content.clone()) {
                    push_tool_step(
                        &mut out,
                        ToolStep {
                            name: String::new(),
                            args: String::new(),
                            risk: String::new(),
                            result: Some(ToolOutcome {
                                ok: !*is_error,
                                output: content.clone(),
                            }),
                        },
                    );
                }
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
            SessionItem {
                id: s.id.clone(),
                title,
                model: s.model.clone(),
                mode: s.mode,
            }
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

/// 读某个会话的历史，转成可渲染的条目。
///
/// 和主对话用的是同一套 `history_to_items`，保证子智能体的面板里
/// 工具卡片、结果卡片长得和主对话一致。
/// 从库里捞本会话发过的指令，给 ↑ 回溯用。
///
/// 只留在内存里的话，重启就没了 —— 而"上周我让它干过什么"
/// 恰恰是最想翻回来重跑的东西。
fn load_input_history(store: &zex_store::Store, session: &str) -> Vec<String> {
    let sid = zex_core::SessionId::from(session);
    let stored = store.load_messages(&sid).unwrap_or_default();
    stored
        .into_iter()
        .filter_map(|s| match s.message {
            zex_core::Message::User { content } => {
                let text: String = content
                    .iter()
                    .filter_map(|p| match p {
                        zex_core::message::Part::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let t = text.trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            }
            _ => None,
        })
        .collect()
}

fn load_session_items(store: &zex_store::Store, session: &str) -> Vec<MsgItem> {
    let sid = zex_core::SessionId::from(session);
    let stored = store.load_messages(&sid).unwrap_or_default();
    let msgs: Vec<zex_core::Message> = stored.into_iter().map(|s| s.message).collect();
    history_to_items(&msgs)
}

/// 运行模式对应的图标。
///
/// 引用 `AllAssets` 里的完整 Lucide 目录：`IconName` 枚举只覆盖组件默认的
/// 106 个，手掌/盾牌这些不在其中，但资源文件在，直接给路径即可。
fn mode_icon_path(mode: zex_core::RunMode) -> &'static str {
    match mode {
        zex_core::RunMode::Confirm => "icons/hand.svg",
        zex_core::RunMode::AutoEdit => "icons/shield-check.svg",
        zex_core::RunMode::Plan => "icons/clipboard-list.svg",
        zex_core::RunMode::FullAccess => "icons/shield-alert.svg",
    }
}

fn load_sessions(store: &zex_store::Store) -> Vec<SessionItem> {
    store
        .list_visible_sessions(20)
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
            SessionItem {
                id: s.id.as_str().to_string(),
                title,
                model: s.model,
                mode: s.mode,
            }
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
    let provider_name = cfg.provider_name.clone();
    // 模型菜单：每个 provider 配当前模型名的后缀。配置里没写模型清单，
    // 用「provider/当前模型」的组合列出最务实 —— 切 provider 时
    // 模型名不变，用户可以在 config.json 里改。
    let models_by_provider = cfg.models.clone();
    let cwd_str = cwd.display().to_string();
    let history = history_to_items(agent.history());
    let tasks = load_tasks(agent.store());
    let store = Arc::new(agent.store().clone());
    let handles = zex_app::spawn_core(agent, cfg.all_providers.clone());
    let cmd_tx = handles.cmd_tx.clone();
    let mut event_rx = handles.event_rx;

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx| {
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
                // 系统标题栏让位给自绘的那一条：
                // macOS/Windows 靠 appears_transparent，Linux 靠 window_decorations
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.0), px(12.0))),
            }),
            window_decorations: Some(WindowDecorations::Client),
            ..Default::default()
        };
        let (handle, view) = match gpui_kit::open_window(
            options,
            cx,
            |window, cx| {
                // niri/sway 这类平铺器会忽略 WindowOptions 里的初始尺寸，
                // 这里再显式要一次；被忽略也无害（浮动窗口自行决定）。
                window.resize(size(px(1280.0), px(820.0)));

                // Esc 中断当前任务。用户等长任务时手在键盘上，
                // 去够鼠标点 ■ 那一下很别扭。
                cx.bind_keys([
                    KeyBinding::new("escape", InterruptTask, Some("Zex")),
                    KeyBinding::new("ctrl-r", HistorySearch, Some("Zex")),
                ]);

                let tx = cmd_tx.clone();
                let broker = broker.clone();
                let sessions = sessions.clone();
                let active = active_session.clone();
                let cwd = cwd_str.clone();
                let model = model.clone();
                let history = history.clone();
                let tasks = tasks.clone();
                let store_for_view = store.clone();
                let view = cx.new(|cx| {
                    ChatView::new(
                        tx,
                        broker,
                        sessions,
                        active,
                        cwd,
                        model,
                        models_by_provider,
                        provider_name.clone(),
                        history,
                        tasks,
                        store_for_view,
                        window,
                        cx,
                    )
                });

                // 关窗拦截：还有任务在跑就停下来问一句，而不是直接掐掉。
                // 不做托盘 —— Linux 的托盘在 Wayland 下不可靠，而且用户
                // 需要的是"明确知道我在后台跑着"，不是一个看不见的图标。
                {
                    let weak = view.downgrade();
                    let store = store.clone();
                    window.on_window_should_close(cx, move |_window, cx| {
                        let busy = store.has_running_work().unwrap_or(false);
                        if !busy {
                            return true;
                        }
                        let _ = weak.update(cx, |this, cx| {
                            this.confirm_close = true;
                            cx.notify();
                        });
                        // 先不关：等用户在确认层里做决定
                        false
                    });
                }

                // 测试钩子：ZEX_AUTO_VOICE=<秒数> 自动录一段并转写。
                // Wayland 下既没法模拟点击、也没法对着麦克风说话，
                // 这是唯一能自动跑通录音→ASR 全链路的方式。
                if let Ok(secs) = std::env::var("ZEX_AUTO_VOICE") {
                    if let Ok(secs) = secs.parse::<u64>() {
                        let weak = view.downgrade();
                        cx.spawn(async move |cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(2))
                                .await;
                            let _ = weak.update(cx, |this, cx| {
                                this.toggle_recording(cx);
                                eprintln!("[voice] 开始录音 {secs}s");
                            });
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(secs))
                                .await;
                            let _ = weak.update(cx, |this, cx| {
                                eprintln!("[voice] 停止录音");
                                this.toggle_recording(cx);
                            });
                        })
                        .detach();
                    }
                }

                // 测试钩子：ZEX_AUTO_PANEL=1 时，等团队出现后自动打开
                // 第一个子智能体的右侧面板 —— Wayland 下没法模拟点击。
                if std::env::var("ZEX_AUTO_PANEL").is_ok() {
                    let weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        for _ in 0..40 {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(2))
                                .await;
                            let done = weak
                                .update(cx, |this, cx| {
                                    if let Some(a) = this.team.first().cloned() {
                                        this.open_subagent_panel(&a.id, cx);
                                        return true;
                                    }
                                    // team 只在有事件推过来时才有内容；
                                    // 冷启动查看历史时退一步查库，别干等
                                    let id = this
                                        .store
                                        .list_sub_agents()
                                        .ok()
                                        .and_then(|l| l.first().map(|r| r.id.clone()));
                                    match id {
                                        Some(id) => {
                                            this.open_subagent_panel(&id, cx);
                                            true
                                        }
                                        None => false,
                                    }
                                })
                                .unwrap_or(true);
                            if done {
                                break;
                            }
                        }
                    })
                    .detach();
                }

                // 测试钩子：ZEX_AUTO_FILE=<路径> 时自动在右侧面板开一个文件页，
                // 用于无人值守验证文件预览的渲染。
                if let Ok(path) = std::env::var("ZEX_AUTO_FILE") {
                    let weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(900))
                            .await;
                        let _ = weak.update(cx, |this, cx| {
                            // 分号分隔可以一次开多个，用来验证标签栏
                            for p in path.split(';').filter(|p| !p.is_empty()) {
                                this.open_file_tab(p, cx);
                            }
                        });
                    })
                    .detach();
                }

                // 测试钩子：设了 ZEX_AUTO_PROMPT 就自动提交一条任务。
                // 只用于无人值守地跑通 GUI 全链路（Wayland 下没法模拟输入）。
                if let Ok(prompt) = std::env::var("ZEX_AUTO_PROMPT") {
                    if !prompt.trim().is_empty() {
                        let tx = view.read_with(cx, |v, _| v.cmd_tx.clone());
                        let session = view.read_with(cx, |v, _| v.active_session.clone());
                        {
                            // 同步投递即可：内核那边是独立线程
                            let _ = tx.try_send(UiCommand::UserPrompt {
                                session: zex_core::SessionId::from(session),
                                text: prompt,
                                attachments: vec![],
                                model: None,
                                mode: None,
                            });
                        }
                    }
                }

                view
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


