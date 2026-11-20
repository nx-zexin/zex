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
    /// 用户消息。
    User {
        /// 随消息一起提交的图片。
        ///
        /// 存已构造的 `gpui::Image` 而不是每帧重解 base64：
        /// 大图每帧解码会明显掉帧，而消息一旦加进来就不再变。
        images: Vec<std::sync::Arc<gpui::Image>>,
    },
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
    /// 扩展面板（gpui-shell 脚本画的界面）。
    Extension(ExtensionPanel),
}

/// 一个已挂载的扩展面板。
struct ExtensionPanel {
    /// 扩展 id。
    id: String,
    /// 展示名，用于标签。
    name: String,
    /// 它专用的会话 id（`ext-<id>`）。事件按它归位。
    session: String,
    /// 脚本 View。`None` = 还没挂上 / 挂失败。
    view: Option<gpui::Entity<zex_extension::ScriptView>>,
    /// 挂载失败的原因。
    error: Option<String>,
    /// 正在把这个扩展的回复攒起来。
    streaming: Option<String>,
    /// 状态栏文字。
    status: String,
    /// 还没得到确认的网络申请（主机清单）。`Some` = 该弹确认卡片。
    net_request: Option<Vec<String>>,
    /// 还没得到确认的剪贴板申请。`true` = 该弹确认卡片。
    clip_request: bool,
    /// 它申请了剪贴板的哪一半（卡片上的措辞要用）。
    clip_read: bool,
    clip_write: bool,
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
            PanelTab::Extension(e) => e.name.clone(),
        }
    }

    /// 去重用的键：同一个文件/同一个 agent 只占一个标签。
    fn key(&self) -> String {
        match self {
            PanelTab::SubAgent(p) => format!("agent:{}", p.session),
            PanelTab::File(f) => format!("file:{}", f.path),
            PanelTab::Extension(e) => format!("ext:{}", e.id),
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
    /// 绝对路径。粘贴进来的图片没有真实路径，用一个虚拟标识代替 ——
    /// 它只用于去重，不进模型上下文。
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

/// 设置页的分区。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    /// 模型与 provider。
    Model,
    /// 技能（预留）。
    Skill,
    /// MCP 服务（预留）。
    Mcp,
    /// 扩展（gpui-shell 脚本面板）。
    Extension,
    /// 版本、路径等信息。
    About,
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
    /// 设置页是否打开（None = 没开），以及停在哪个分区。
    settings: Option<SettingsTab>,
    /// `config.json` 的原始内容：设置页要展示、也要能改。
    config: zex_app::FileConfig,
    /// 已加载的技能库。
    skills: std::sync::Arc<zex_skill::SkillRegistry>,
    /// MCP 连接结果（启动后填充）。
    mcp_status: Vec<zex_mcp::ServerStatus>,
    /// provider 编辑表单：外层 Some 表示表单开着，内层 Some(name) 表示改现有的。
    provider_form: Option<Option<String>>,
    /// 等待确认删除的 provider 名。
    provider_pending_delete: Option<String>,
    /// 正在给哪个 provider 添加模型（provider 名）。
    adding_model_for: Option<String>,
    /// models.dev 目录（`None` = 还没加载出来）。
    ///
    /// 用 `Arc` 是因为渲染每帧都要读它，克隆整个目录太贵；
    /// 目录本身几 MB，也只该有一份。
    md_catalog: Option<std::sync::Arc<zex_app::models_dev::Catalog>>,
    /// 是否正在后台拉目录。
    md_loading: bool,
    /// 拉取失败的原因（`None` = 没失败过）。
    md_error: Option<String>,
    /// provider 表单里的目录搜索框。
    md_provider_q: Entity<InputState>,
    /// 添加模型面板里的搜索框。
    md_model_q: Entity<InputState>,
    /// 表单里选中的目录条目（provider id）。保存时据此带出模型清单。
    md_picked: Option<String>,
    /// provider 表单的输入框。
    pf_name: Entity<InputState>,
    pf_kind: Entity<InputState>,
    pf_base_url: Entity<InputState>,
    pf_key_env: Entity<InputState>,
    /// 新模型名。
    pf_model: Entity<InputState>,
    /// 正在编辑参数的模型：`(provider, model)`。
    editing_params_for: Option<(String, String)>,
    /// 参数表单：温度、最大输出 token。
    pf_temperature: Entity<InputState>,
    pf_max_tokens: Entity<InputState>,
    /// 当前会话的运行模式。
    run_mode: zex_core::RunMode,
    /// 思考强度（每轮随 prompt 下发，不落会话）。
    thinking: zex_llm::ThinkingEffort,
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
    /// 哪些 provider 的模型列表被展开了（按 provider 名记）。
    ///
    /// 从目录里选一家就带进十几个模型，anthropic 十八个、openrouter 四百多，
    /// 全铺出来会把其它 provider 顶出屏幕。
    models_expanded: std::collections::HashSet<String>,
    /// 已安装的扩展（目录扫描结果）。
    ///
    /// 只在设置页打开时刷新：扫描目录要读文件，没必要每帧做。
    extensions: Vec<zex_extension::Extension>,
    /// 已打开的扩展 → 脚本文件的最后修改时间（纳秒）。
    ///
    /// 用来做热重载：变了就重挂。`runtime.watch` 需要 `ShellRoot`，
    /// 而我们是面板挂载（View 直接塞进标签），用不了它。
    ext_mtimes: std::collections::HashMap<String, u128>,
    /// 扩展宿主（首次打开扩展时才建）。
    ///
    /// 建它就要起一个 QuickJS VM（十几 MiB 内存），所以不跟着启动走 ——
    /// 绝大多数会话用不到扩展。
    ext_host: Option<std::rc::Rc<zex_extension::ExtensionHost>>,
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
        // provider 表单的几个框：常驻，打开表单时填值
        let pf_name = cx.new(|cx| InputState::new(window, cx).placeholder("provider 名，如 deepseek"));
        let pf_kind = cx.new(|cx| InputState::new(window, cx).placeholder("openai-compat 或 anthropic"));
        let pf_base_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://api.example.com/v1"));
        let pf_key_env = cx.new(|cx| InputState::new(window, cx).placeholder("环境变量名，如 DEEPSEEK_API_KEY"));
        let pf_model = cx.new(|cx| InputState::new(window, cx).placeholder("模型名，如 gpt-4o"));
        let md_provider_q =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索 provider，如 deepseek / kimi"));
        let md_model_q =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型，如 flash / haiku"));
        let pf_temperature =
            cx.new(|cx| InputState::new(window, cx).placeholder("温度 0–2，留空用服务端默认"));
        let pf_max_tokens =
            cx.new(|cx| InputState::new(window, cx).placeholder("最大输出 token，留空用 8192"));

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

        // 目录搜索框：输入即重绘列表。
        // gpui 里 InputState 的变化不会自动让父视图重绘，
        // 不订阅的话筛选条件是变了、列表还是老的。
        for q in [&md_provider_q, &md_model_q] {
            cx.subscribe_in(q, window, |_this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }

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

        // 扩展热重载：每 1.2 秒看一次已打开扩展的脚本有没有变。
        // 没有打开的扩展时这个循环几乎不花时间（直接返回）。
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(1200))
                    .await;
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        this.poll_extension_changes(window, cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        // 测试钩子：ZEX_AUTO_EXTENSION=<id> 启动后直接把那个扩展挂进右侧面板。
        //
        // 扩展开、关、加载失败这几条路都得有人跑一遍。靠截图点鼠标验证
        // 在无头环境里会卡在"点不到"上，所以给它一个确定性的入口。
        if let Ok(id) = std::env::var("ZEX_AUTO_EXTENSION") {
            if !id.trim().is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(800))
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        this.settings = None;
                        this.open_extension(&id, window, cx);
                    })
                    .ok();
                })
                .detach();
            }
        }

        // 测试钩子：ZEX_AUTO_GRANT=<id> 启动后替用户点一下「允许」。
        //
        // 这个环境里 XTest 被挡着，合成鼠标点击拉不动指针 —— 但"点允许
        // 之后应用能不能立刻联网"是这次改动的核心，必须有人跑过。所以走
        // 和按钮**同一个函数**，验证的仍是配置写入 + 网关刷新 + 原地重挂。
        if let Ok(id) = std::env::var("ZEX_AUTO_GRANT") {
            if !id.trim().is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1600))
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        this.grant_network(&id, true, window, cx);
                    })
                    .ok();
                })
                .detach();
            }
        }

        // 测试钩子：ZEX_AUTO_GRANT_CLIP=<id> 启动后替用户点一下「允许剪贴板」。
        //
        // 和网络那个钩子同一个理由：合成点击在这个环境里拉不动指针，
        // 而"点了允许之后脚本立刻能读"正是这条能力的关键路径。
        // 走的是按钮调用的同一个函数，所以验证的仍是配置写入 + 授权表刷新 + 重挂。
        if let Ok(id) = std::env::var("ZEX_AUTO_GRANT_CLIP") {
            if !id.trim().is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1600))
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        this.grant_clipboard(&id, true, window, cx);
                    })
                    .ok();
                })
                .detach();
            }
        }

        // 测试钩子：ZEX_AUTO_CHECK=<id> 启动后静默跑一次扩展校验，
        // 把结论打到 stderr。
        //
        // 用途是验证「校验不弹窗」这条：`WindowOptions.show` 在 Linux 后端
        // 没有实现，"我自己没看见窗口"不能算证据，得有确定的入口 + 数窗口数。
        if let Ok(spec) = std::env::var("ZEX_AUTO_CHECK") {
            let app_id = spec.trim().to_string();
            if !app_id.is_empty() {
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(900))
                        .await;
                    this.update_in(cx, move |this, _window, cx| {
                        let (id, _) = zex_extension::check::request(&app_id);
                        let mut rx = zex_extension::check::register(&id);
                        this.run_extension_check(&id, &app_id, cx);
                        match rx.try_recv() {
                            Ok(r) => eprintln!(
                                "ZEX_AUTO_CHECK 结果：ok={} error={:?} note={:?} tree={:?}",
                                r.is_ok(),
                                r.error,
                                r.note,
                                r.tree.map(|t| t.chars().take(200).collect::<String>())
                            ),
                            Err(e) => eprintln!("ZEX_AUTO_CHECK 没有回音：{e}"),
                        }
                    })
                    .ok();
                })
                .detach();
            }
        }

        let scroller = cx.new(|cx| MessageScrollerState::new(history.len(), cx));

        // 打开就先看工作台：用户想知道「我不在的时候发生了什么」
        let mode = ViewMode::Dashboard;

        // 跟着恢复的会话走：硬编码成 Confirm 的话，
        // 用户上次设在"完全访问"的会话一重启就显示成"变更前确认"
        let initial_mode = sessions
            .iter()
            .find(|s| s.id == active_session)
            .map(|s| s.mode)
            .unwrap_or_default();

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
            settings: None,
            config: zex_app::read_file_config().unwrap_or_default(),
            skills: zex_app::load_skills(),
            mcp_status: Vec::new(),
            provider_form: None,
            provider_pending_delete: None,
            adding_model_for: None,
            md_catalog: None,
            md_loading: false,
            md_error: None,
            md_provider_q,
            md_model_q,
            md_picked: None,
            pf_name,
            pf_kind,
            pf_base_url,
            pf_key_env,
            pf_model,
            editing_params_for: None,
            pf_temperature,
            pf_max_tokens,
            run_mode: initial_mode,
            thinking: zex_llm::ThinkingEffort::Off,
            editing_model: false,
            model_input,
            input_history: restored_history,
            history_cursor: None,
            history_draft: String::new(),
            expanded_tools: std::collections::HashSet::new(),
            models_expanded: std::collections::HashSet::new(),
            extensions: zex_extension::scan_default(),
            ext_mtimes: std::collections::HashMap::new(),
            ext_host: None,
            subagent_panel_width: DEFAULT_PANEL_WIDTH,
            subagent_resizing: false,
            subagent_drag_origin: (0.0, 0.0),
            pending_approvals: Vec::new(),
            store,
            cwd,
            model,
            usage: Usage::default(),
        };

        // 启动后连 MCP 服务：连接要 spawn 子进程并等握手，异步做，
        // 不阻塞窗口显示。连上后把状态填进设置页。
        {
            let cfg = view.config.clone();
            // 连接会 spawn 子进程并等握手，放到后台线程：
            // 服务端启动慢的话不该让窗口卡住
            let task = cx
                .background_executor()
                .spawn(async move { zex_app::connect_mcp_servers(&cfg) });
            cx.spawn(async move |this, cx| {
                let (_tools, status, _hub) = task.await;
                if status.is_empty() {
                    return;
                }
                let _ = this.update(cx, |this, cx| {
                    this.mcp_status = status;
                    cx.notify();
                });
            })
            .detach();
        }

        // 测试钩子：ZEX_AUTO_SETTINGS=model|skill|mcp|about 直接打开设置页
        if let Ok(tab) = std::env::var("ZEX_AUTO_SETTINGS") {
            view.settings = Some(match tab.as_str() {
                "skill" => SettingsTab::Skill,
                "mcp" => SettingsTab::Mcp,
                "extension" | "ext" => SettingsTab::Extension,
                "about" => SettingsTab::About,
                _ => SettingsTab::Model,
            });
            view.ensure_catalog(cx);
        }

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

    /// 处理一份粘贴内容：图片/文件收成附件，纯文本交给输入框。
    ///
    /// 返回 true 表示已消费（不要再插文本）。
    ///
    /// 键盘 Ctrl+V 和右键菜单的 Paste 都汇集到这里 —— 组件库的
    /// `on_paste` 钩子对两条路径一致，分开处理只会漏掉其中一条。
    fn take_paste(&mut self, item: &gpui::ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut took = false;
        let mut pasted_paths: Vec<std::path::PathBuf> = Vec::new();

        for entry in &item.entries {
            match entry {
                gpui::ClipboardEntry::Image(img) => {
                    if self.push_pasted_image(&img.bytes, img.format) {
                        took = true;
                    }
                }
                gpui::ClipboardEntry::ExternalPaths(paths) => {
                    // 文件管理器里复制文件 → 粘贴，直接当附件收下
                    pasted_paths.extend(paths.paths().iter().cloned());
                    took = true;
                }
                gpui::ClipboardEntry::String(_) => {
                    // 文本留给输入框，不在这里抢
                }
            }
        }

        if !pasted_paths.is_empty() {
            self.add_attachments(&pasted_paths, cx);
        }
        if took {
            cx.notify();
        }
        took
    }

    /// 把一段图片字节收成附件。返回 false 表示同内容的已经在了。
    fn push_pasted_image(&mut self, bytes: &[u8], format: gpui::ImageFormat) -> bool {
        let (ext, media) = match format {
            gpui::ImageFormat::Png => ("png", "image/png"),
            gpui::ImageFormat::Jpeg => ("jpg", "image/jpeg"),
            gpui::ImageFormat::Webp => ("webp", "image/webp"),
            gpui::ImageFormat::Gif => ("gif", "image/gif"),
            gpui::ImageFormat::Bmp => ("bmp", "image/bmp"),
            // SVG 走不了多模态通道（模型看的是位图），明确跳过而不是塞进去
            _ => return false,
        };

        let b64 = base64_encode(bytes);
        // 同一张图连点两次粘贴很常见，按内容去重
        if self
            .attachments
            .iter()
            .any(|a| a.image_b64.as_deref() == Some(b64.as_str()))
        {
            return false;
        }

        // 序号取当前图片数，只是为了给个能看的名字
        let n = self.attachments.iter().filter(|a| a.image_b64.is_some()).count() + 1;
        let name = format!("粘贴的图片 {n}.{ext}");
        self.attachments.push(Attachment {
            // 虚拟路径：仅用于去重，不会出现在 prompt 里
            path: std::path::PathBuf::from(format!("paste://{n}.{ext}")),
            name,
            image_b64: Some(b64),
            media_type: media.to_string(),
        });
        true
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

        // 图片进信息流：用户要看到自己发了什么，
        // 而不是只剩一句"请查看附件"。必须在 clear 之前取。
        let shown_images: Vec<std::sync::Arc<gpui::Image>> = self
            .attachments
            .iter()
            .filter_map(|a| {
                let bytes = base64_decode(a.image_b64.as_ref()?)?;
                Some(std::sync::Arc::new(gpui::Image {
                    id: image_id(&bytes),
                    format: image_format_of(&a.media_type),
                    bytes,
                }))
            })
            .collect();
        self.attachments.clear();

        self.input.update(cx, |state, cx| state.set_value("", window, cx));
        // 派了活就要看到它跑：从工作台自动进入对话视图
        self.mode = ViewMode::Chat;
        self.messages.push(MsgItem {
            kind: MsgKind::User {
                images: shown_images,
            },
            text: text.clone(),
        });
        self.streaming = Some(String::new());
        self.status = "思考中…".into();

        let tx = self.cmd_tx.clone();
        let session = self.active_session.clone();
        let selected_model = self.selected_model.clone();
        let run_mode = self.run_mode;
        let thinking = self.thinking;
        cx.spawn(async move |_this, _cx| {
            let _ = tx
                .send(UiCommand::UserPrompt {
                    session: zex_core::SessionId::from(session),
                    text,
                    attachments: images,
                    model: selected_model,
                    mode: Some(run_mode),
                    thinking: Some(thinking),
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

    /// 把运行模式推给内核，并处理"正挂着的那条审批"。
    ///
    /// 只改界面状态是不够的：模式是**按轮**取快照的，任务跑一半时切到
    /// 完全访问，那一轮手里还是老模式，于是继续弹确认。用户看到的就是
    /// "我都切完全访问了，怎么还问我"。所以这里多走两步：
    ///
    /// 1. 发 `SetMode` 给内核，改掉那一轮共享的那个句柄 —— 下一句命令起效；
    /// 2. 已经弹出来的那条，按新模式重新判一次，能放行就直接放行，
    ///    不然用户还得为"切模式之前问的那一句"再点一次。
    ///
    /// 第 2 步**绝不碰不可逆操作**：L3 永远逐次确认，这是底线，
    /// 切什么模式都不豁免。
    fn push_mode_to_kernel(&mut self, mode: zex_core::RunMode, cx: &mut Context<Self>) {
        let session = zex_core::SessionId::from(self.active_session.clone());
        let _ = self
            .cmd_tx
            .try_send(UiCommand::SetMode { session, mode });

        // 判据不在这里手写：问策略层"这个模式会不会放行这一档风险"，
        // 免得矩阵抄两份、抄漏一格就退化成"切了还问"
        let Some(req) = self.approval.as_ref() else {
            return;
        };
        if zex_policy::auto_allows(mode, req.risk) {
            // 复用正常审批路径：一样的落库、一样的"处理下一个"
            self.respond(true, false, cx);
        }
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
        // 扩展自检请求：工具那边在等回音，必须由 UI 线程画一帧才答得出来
        if let CoreEvent::ExtensionCheck { id, app_id } = &ev {
            let (id, app_id) = (id.clone(), app_id.clone());
            self.run_extension_check(&id, &app_id, cx);
            return;
        }
        // 扩展的会话先认领：它的事件不该进主对话，也不该进子智能体面板
        if self.mirror_to_extension(&ev, cx) {
            cx.notify();
            return;
        }
        // 面板正在看的那个子智能体，它的事件直接喂给面板
        if self.mirror_to_panel(&ev) {
            cx.notify();
            return;
        }
        match ev {
            // 上面已经提前处理并 return，这里只是让 match 穷尽
            CoreEvent::ExtensionCheck { .. } => {}
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
            CoreEvent::TurnEnded { session, reason, usage, steps, elapsed_ms, turn: _ } => {
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
                // 结束一定要说清楚。以前只在非 Completed 时提示，且打的是
                // `{reason:?}` 调试串 —— 用户既看不懂"MaxSteps"，
                // 也不知道完成了没有（复杂任务跑几小时，没有落款就只能猜）。
                self.messages.push(MsgItem {
                    kind: MsgKind::System,
                    text: end_reason_text(&reason, steps, elapsed_ms),
                });
                // 必须同步条目数：对话流是 `MessageScroller`，它的条目数在
                // 初始化时就定了，`messages.push` 不会让它知道多了一条。
                // 少了这一行，结束提示会**永远渲染不出来** —— 这正是
                // "从来没告诉我任务完成了"的根因。
                self.sync_scroller(cx);
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

/// 从配置里取"每个应用被授权访问哪些主机"，交给扩展宿主做网络闸门。
fn grants_of(
    config: &zex_app::config::FileConfig,
) -> std::collections::BTreeMap<String, Vec<String>> {
    config.network_grants.clone()
}

/// 剪贴板授权表。
fn clip_grants_of(
    config: &zex_app::config::FileConfig,
) -> std::collections::BTreeMap<String, bool> {
    config.clipboard_grants.clone()
}

/// 两张授权表一起递给宿主。
///
/// 单开一个函数是因为之前踩过：三处 `set_grants` 各写一遍，加第二张表时
/// 漏一处就是"点了允许却还是读不到"。授权只有"全都传过去"这一种正确姿势。
fn sync_grants(host: &zex_extension::ExtensionHost, config: &zex_app::config::FileConfig) {
    host.set_grants(grants_of(config));
    host.set_clipboard_grants(clip_grants_of(config));
}

/// 要不要弹剪贴板确认？
///
/// 与网络不同，这里没有"清单长大了要重问"的分支：权限是一个布尔值，
/// 不是一张会增删的名单。问过就记住了，拒绝也算记住。
fn pending_clipboard(
    config: &zex_app::config::FileConfig,
    ext: &zex_extension::Extension,
) -> bool {
    let wants = ext.request.clipboard_read || ext.request.clipboard_write;
    wants && !config.clipboard_grants.contains_key(&ext.id)
}

/// 加载这个应用时要不要弹网络确认？
///
/// 三种情况要问：
/// - 没问过（配置里没这个应用）；
/// - 清单后来**新增**了主机（模型可能自己加了一条）——这时老授权只覆盖
///   旧的那几条，新的一条该由人来定；
/// - 用户已经明确拒绝过（空列表）的**不**再问：问过一次"不要"就是不要。
fn pending_net(
    config: &zex_app::config::FileConfig,
    ext: &zex_extension::Extension,
) -> Option<Vec<String>> {
    let declared = &ext.request.network_hosts;
    if declared.is_empty() {
        return None;
    }
    let Some(granted) = config.network_grants.get(&ext.id) else {
        return Some(declared.clone());
    };
    if granted.is_empty() {
        // 明确拒绝过：别追着问
        return None;
    }
    let covered = declared.iter().all(|want| {
        granted
            .iter()
            .any(|have| have.trim().eq_ignore_ascii_case(want.trim()))
    });
    if covered {
        None
    } else {
        // 清单里出现了没被授权过的主机：重新问一次
        Some(declared.clone())
    }
}

/// 一轮为什么停下来 —— 给用户看的一句话。
///
/// 每种结局都要回答两件事：**做完了没有**、**我该做什么**。
/// 复杂任务可能跑好几个小时，没有一句明确的落款，用户没法判断
/// "是完成了还是又停在那儿了"。
fn end_reason_text(reason: &zex_core::EndReason, steps: u32, elapsed_ms: u64) -> String {
    let took = zex_agent::human_elapsed(std::time::Duration::from_millis(elapsed_ms));
    let scale = format!("共 {steps} 步，用时 {took}");
    match reason {
        zex_core::EndReason::Completed => format!("✓ 任务完成 · {scale}"),
        zex_core::EndReason::Cancelled => format!("■ 已按你的要求停下 · {scale}"),
        zex_core::EndReason::MaxSteps { limit } => format!(
            "⏸ 连续跑了 {limit} 步（已达配置上限）后停下 · {scale}。\
             回复「继续」可以接着做；要彻底不设上限，把配置里的 \
             `max_steps` 设为 0"
        ),
        zex_core::EndReason::BudgetExhausted => {
            format!("⏸ 上下文预算用尽，已停下 · {scale}。建议新开一个会话，或先让它总结当前进度")
        }
        zex_core::EndReason::Failed { message } => {
            format!("✗ 出错了：{message} · {scale}")
        }
    }
}

/// 一个应用的图标：`icon.svg` 有就按主题色画出来，没有返回 `None`。
///
/// 为什么是 `Option`：回退方案在两个调用点不一样 —— 侧边栏退成一个小圆点
/// （保持原来的视觉节奏），设置页干脆不占位（那里有名字和路径，图标只是点缀）。
/// 所以"没有图标"这件事交给调用方决定，不在这里编一个通用占位。
///
/// SVG 是**单色**渲染的：resvg 只取 alpha 遮罩，颜色由 `text_color` 决定。
/// 所以图标里写什么 `fill` 都不作数，形状和 `fill-opacity` 才作数。
fn app_icon(icon: Option<&Arc<[u8]>>, size: f32, color: u32) -> Option<AnyElement> {
    let bytes = icon?;
    Some(
        svg()
            .data(bytes)
            .flex_shrink_0()
            .size(px(size))
            .text_color(rgb(color))
            .into_any_element(),
    )
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
            MsgKind::User { images } => {
                let mut col = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(role_label("USER"));

                // 图片排在文字上方：发图时那句话通常只是配文
                if !images.is_empty() {
                    let mut row = div().flex().flex_wrap().gap_2();
                    for (i, img) in images.iter().enumerate() {
                        row = row.child(
                            // 固定高度、限制最大宽度：原图尺寸各异，
                            // 按原始大小排会让信息流参差不齐
                            gpui::img(gpui::ImageSource::Image(img.clone()))
                                .id(ElementId::Name(
                                    format!("user-img-{idx}-{i}").into(),
                                ))
                                .h(px(120.0))
                                .max_w(px(240.0))
                                .rounded(px(8.0))
                                .border_1()
                                .border_color(rgb(BORDER)),
                        );
                    }
                    col = col.child(row);
                }

                // 只贴图不打字时 text 是空的，别留一段空白
                if !m.text.trim().is_empty() {
                    col = col.child(
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
                    );
                }
                col.context_menu(copy_context_menu(m.text.clone()))
                    .into_any_element()
            }

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

            MsgKind::System => {
                // 系统消息有两种长度：一行能说完的（"✓ 任务完成 · 共 3 步"）
                // 和说不完的（出错的那一大段原因）。后者塞进胶囊里会被排成
                // 一整行、然后两头各裁掉一截 —— 正好把"出错了"和后面的
                // 处理建议裁没，只剩中间一段谁也看不懂的原文。
                if zex_app::system_banner_is_long(&m.text) {
                    let danger = m.text.starts_with('✗');
                    let muted = if danger { DANGER } else { TEXT_MUTED };
                    div()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w_full()
                                .min_w(px(0.0))
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(if danger { DANGER_SOFT } else { BORDER_SOFT }))
                                .bg(rgb(if danger { DANGER_SOFT } else { BG_TOOL }))
                                .child(
                                    // 用 markdown 而不是裸文本：系统消息里
                                    // 常带反引号和路径，顺手渲染成码字更好读
                                    TextView::markdown(
                                        ElementId::Name(format!("md-sys-{idx}").into()),
                                        m.text.clone(),
                                    )
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(rgb(muted))
                                    .style(md_style_flat(scale)),
                                ),
                        )
                        .into_any_element()
                } else {
                    div()
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
                        .into_any_element()
                }
            }
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
                // 按前缀判断：状态后面可能带补充说明（"就绪 · 已重载"）
                PanelTab::Extension(e) if e.status.starts_with("运行中") => (BRAND, BRAND_SOFT),
                PanelTab::Extension(e) if e.status.starts_with("失败") => (DANGER, DANGER_SOFT),
                PanelTab::Extension(e) if e.status.starts_with("就绪") => (OK, OK_SOFT),
                PanelTab::Extension(_) => (TEXT_DIM, BG_HOVER),
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
            PanelTab::Extension(e) => self.render_extension_body(e, cx),
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

    /// 扩展页：脚本画的界面，本体直接占满。
    ///
    /// 挂载失败时不是白屏：错误信息摆在这里，其它面板照常工作 ——
    /// 一个写坏的扩展不该拖垮宿主。
    fn render_extension_body(&self, e: &ExtensionPanel, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex_1().min_h(px(0.0)).flex().flex_col();
        if let Some(hosts) = &e.net_request {
            col = col.child(self.render_net_request(e, hosts, cx));
        }
        if e.clip_request {
            col = col.child(self.render_clip_request(e, cx));
        }
        col.child(self.render_extension_inner(e, cx))
            .into_any_element()
    }

    /// 网络确认卡片：应用要联网，得由人点一下。
    ///
    /// 为什么要确认：清单是模型写的，它自己写一行 `network.hosts`
    /// 就能决定"我要连哪儿"。要是清单直接生效，等于模型给自己发授权。
    /// 所以清单只算**申请**，点这一下才是授权 —— 而且授权存进配置，
    /// 只放行清单里这几条，之后清单再偷偷加大也过不去。
    fn render_net_request(
        &self,
        e: &ExtensionPanel,
        hosts: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let allow_id = e.id.clone();
        let deny_id = e.id.clone();
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_3()
            .bg(rgb(BG_TOOL))
            .border_b_1()
            .border_color(rgb(BRAND))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(BRAND))
                            .child(format!("`{}` 想访问网络", e.id)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child("需你确认后才会生效"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(MONO)
                    .text_color(rgb(TEXT_MUTED))
                    .child(hosts.join("、")),
            )
            .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(
                "只放行上面这几条；随时可在「设置 → 应用」里撤销。",
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(OK_SOFT))
                            .text_xs()
                            .text_color(rgb(OK))
                            .hover(|st| st.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                    this.grant_network(&allow_id, true, w, cx);
                                }),
                            )
                            .child("允许"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .hover(|st| st.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                    this.grant_network(&deny_id, false, w, cx);
                                }),
                            )
                            .child("不允许"),
                    ),
            )
            .into_any_element()
    }

    /// 剪贴板确认卡片。
    ///
    /// 为什么单独一张而不是并进网络那张：两件事的风险不是一回事
    /// （网络是往外发，剪贴板是往里看），用户对它们的判断也不一样。
    /// 一张卡片上写两件事，最容易的结果是他只看懂一半就点了。
    fn render_clip_request(&self, e: &ExtensionPanel, cx: &mut Context<Self>) -> AnyElement {
        let allow_id = e.id.clone();
        let deny_id = e.id.clone();
        // 说清楚它到底要哪一半：只申请写的应用不该被当成"能看你剪贴板"
        let what = match (e.clip_read, e.clip_write) {
            (true, true) => "读取并修改你的剪贴板",
            (true, false) => "读取你的剪贴板",
            (false, true) => "往你的剪贴板里写内容",
            (false, false) => "剪贴板",
        };
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_3()
            .bg(rgb(BG_TOOL))
            .border_b_1()
            .border_color(rgb(BRAND))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(BRAND))
                            .child(format!("`{}` 想{what}", e.id)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child("需你确认后才会生效"),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("剪贴板里可能有密码、私钥、别处复制来的私事。准许后它随时可读，随时可在侧栏撤销。"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(OK_SOFT))
                            .text_xs()
                            .text_color(rgb(OK))
                            .hover(|st| st.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                    this.grant_clipboard(&allow_id, true, w, cx);
                                }),
                            )
                            .child("允许"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .hover(|st| st.bg(rgb(BG_HOVER)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                    this.grant_clipboard(&deny_id, false, w, cx);
                                }),
                            )
                            .child("不允许"),
                    ),
            )
            .into_any_element()
    }

    /// 面板主体：脚本 View 或错误信息。
    fn render_extension_inner(&self, e: &ExtensionPanel, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        if let Some(err) = &e.error {
            return div()
                .id(ElementId::Name(format!("ext-err-{}", e.id).into()))
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(DANGER))
                        .child(format!("应用 `{}` 未能加载", e.id)),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(err.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(format!(
                            "目录：{}",
                            zex_extension::extensions_dir().join(&e.id).display()
                        )),
                )
                .into_any_element();
        }

        let Some(view) = &e.view else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(TEXT_DIM))
                .child("正在加载…")
                .into_any_element();
        };

        // 脚本 View 就是一个普通元素，直接占满面板
        let mut body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(div().flex_1().min_h(px(0.0)).child(view.clone()));
        if !e.status.is_empty() {
            body = body.child(
                div()
                    .border_t_1()
                    .border_color(rgb(BORDER_SOFT))
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(e.status.clone()),
            );
        }
        body.into_any_element()
    }

    /// 用户对这个应用的网络申请表态：允许（= 授权清单里那些主机）或拒绝。
    ///
    /// 三种结果里只有"允许"会写非空列表；拒绝写空列表 —— 这样
    /// 界面就能区分"没问过"和"问过、说不要"，不会每次都追着问。
    ///
    /// `window` 用来在允许之后**原地重挂**：应用的 `init` 里那一通
    /// 网络调用当时是被拒的，不重挂它就得等用户自己关掉再打开。
    fn grant_network(&mut self, id: &str, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        let declared = self
            .extensions
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.request.network_hosts.clone())
            .unwrap_or_default();
        let list = if allow { declared.clone() } else { Vec::new() };
        self.config.network_grants.insert(id.to_string(), list);
        // 闸门读的就是这张表：点了立刻生效，不用重启
        if let Some(host) = &self.ext_host {
            sync_grants(&host, &self.config);
        }
        // 面板上的确认卡片收掉
        if let Some(rp) = &mut self.right_panel {
            if let Some(PanelTab::Extension(p)) = rp
                .tabs
                .iter_mut()
                .find(|t| t.key() == format!("ext:{id}"))
            {
                p.net_request = None;
            }
        }
        let msg = if allow {
            format!(
                "已允许 `{id}` 访问网络（{}）——只放行清单里这几条，随时可撤销",
                declared.join("、")
            )
        } else {
            format!("已拒绝 `{id}` 的网络申请，它再调用会被拦下（可随时在设置里改）")
        };
        self.persist_config(msg, cx);
        if allow {
            self.remount_extension(id, window, cx);
        }
    }

    /// 剪贴板授权的允许 / 拒绝。
    ///
    /// 与网络那条路一样：写进配置、立刻推给宿主、收掉确认卡片、重挂面板。
    /// 重挂是必须的 —— gpui-shell 的能力集是**建视图那一刻**读走的，
    /// 已挂载的视图里 `navigator.clipboard` 仍然按上一份判。
    fn grant_clipboard(&mut self, id: &str, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.config.clipboard_grants.insert(id.to_string(), allow);
        if let Some(host) = &self.ext_host {
            sync_grants(host, &self.config);
        }
        if let Some(rp) = &mut self.right_panel {
            if let Some(PanelTab::Extension(p)) = rp
                .tabs
                .iter_mut()
                .find(|t| t.key() == format!("ext:{id}"))
            {
                p.clip_request = false;
            }
        }
        let msg = if allow {
            format!("已允许 `{id}` 使用剪贴板 —— 只放行清单申请的那一半，随时可撤销")
        } else {
            format!("已拒绝 `{id}` 的剪贴板申请，它调用时会被拦下（可随时改）")
        };
        self.persist_config(msg, cx);
        self.remount_extension(id, window, cx);
    }

    /// 重新挂载一个已经打开的应用面板（改了授权/脚本之后用）。
    fn remount_extension(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.ext_host.clone() else {
            return;
        };
        self.extensions = zex_extension::scan_default();
        let Some(fresh) = self.extensions.iter().find(|e| e.id == id).cloned() else {
            return;
        };
        let mounted = host.mount(&fresh, window, cx);
        self.ext_mtimes.insert(fresh.id.clone(), Self::ext_stamp(&fresh));
        if let Some(rp) = &mut self.right_panel {
            if let Some(PanelTab::Extension(p)) = rp
                .tabs
                .iter_mut()
                .find(|t| t.key() == format!("ext:{id}"))
            {
                match mounted {
                    Ok(v) => {
                        p.view = Some(v);
                        p.error = None;
                        p.status = "就绪 · 已重载".into();
                    }
                    Err(e) => {
                        p.error = Some(e.to_string());
                        p.status = "重载失败".into();
                    }
                }
            }
        }
        cx.notify();
    }

    /// 固定 / 取消固定某个扩展到侧边栏，并写回配置。
    ///
    /// 只存 id：扩展叫什么、装在哪，都以目录扫描为准。
    /// 所以这里不校验 id 是否存在 —— 固定了但没装，侧栏不显示就是了。
    fn toggle_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        let pinned = zex_app::config::toggle_pinned(&mut self.config.pinned_extensions, id);
        let msg = if pinned {
            format!("已把 `{id}` 固定到侧边栏")
        } else {
            format!("已取消固定 `{id}`")
        };
        self.persist_config(msg, cx);
    }

    /// 脚本文件最后一次修改的时间（纳秒）。取不到时返回 0。
    ///
    /// 同时看 `main.js` 和清单：改清单里的 entry 也算改了东西。
    fn ext_stamp(ext: &zex_extension::Extension) -> u128 {
        fn mtime(p: &std::path::Path) -> u128 {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        }
        let entry = mtime(&ext.entry_path());
        let manifest = mtime(&ext.dir.join(zex_extension::catalog::MANIFEST));
        entry.max(manifest)
    }

    /// 检查已打开的扩展有没有被改动，变了就重新挂载。
    ///
    /// 轮询而不是监听文件系统：一次开发会话里最多同时开两三个扩展，
    /// 每 1.2 秒读几个 mtime 的成本远低于引入一个 watcher 依赖。
    /// 没有打开的扩展时这个函数什么都不做。
    fn poll_extension_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ext_mtimes.is_empty() {
            return;
        }
        let Some(_host) = self.ext_host.clone() else {
            return;
        };
        // 先算出哪些需要重挂，避免在借用 panel 的同时改 self
        let mut reload: Vec<zex_extension::Extension> = Vec::new();
        {
            let Some(rp) = &self.right_panel else {
                return;
            };
            for tab in &rp.tabs {
                let PanelTab::Extension(p) = tab else { continue };
                let Some(ext) = self.extensions.iter().find(|e| e.id == p.id) else {
                    continue;
                };
                let stamp = Self::ext_stamp(ext);
                let seen = self.ext_mtimes.get(&ext.id).copied().unwrap_or(0);
                if stamp != seen {
                    reload.push(ext.clone());
                }
            }
        }
        if reload.is_empty() {
            return;
        }

        let Some(host) = self.ext_host.clone() else {
            return;
        };
        for ext in reload {
            // 重新扫一遍：改动可能连清单也换了（比如换了 entry）
            self.extensions = zex_extension::scan_default();
            let Some(fresh) = self.extensions.iter().find(|e| e.id == ext.id).cloned() else {
                continue;
            };
            let mounted = host.mount(&fresh, window, cx);
            self.ext_mtimes
                .insert(fresh.id.clone(), Self::ext_stamp(&fresh));
            // 清单可能刚改过：还没确认过的网络申请要在重挂后重新摆出来
            let pending = pending_net(&self.config, &fresh);
            if let Some(rp) = &mut self.right_panel {
                if let Some(PanelTab::Extension(p)) =
                    rp.tabs.iter_mut().find(|t| t.key() == format!("ext:{}", fresh.id))
                {
                    match mounted {
                        Ok(v) => {
                            p.view = Some(v);
                            p.error = None;
                            p.status = "就绪 · 已重载".into();
                            p.net_request = pending;
                            p.clip_request = pending_clipboard(&self.config, &fresh);
                            p.clip_read = fresh.request.clipboard_read;
                            p.clip_write = fresh.request.clipboard_write;
                        }
                        Err(e) => {
                            // 重载失败保留上一版界面：改到一半语法错时，
                            // 屏幕上那块还在，比整块变红好收拾
                            p.error = Some(e.to_string());
                            p.status = "重载失败".into();
                        }
                    }
                }
            }
            self.status = format!("`{}` 已重载", fresh.id);
        }
        cx.notify();
    }

    /// 跑一次扩展自检，把结论交回给等着的工具。
    ///
    /// 为什么要开一个**隐藏窗口**：`ShellRuntime::check` 内部会
    /// `replace_root` 再 `draw` 一帧，而且用完不还原 —— 拿主窗口去调
    /// 会把用户界面换掉。所以给它一个用完就扔的窗口。
    fn run_extension_check(&mut self, req_id: &str, app_id: &str, cx: &mut Context<Self>) {
        let dir = zex_extension::extensions_dir().join(app_id);
        if !dir.is_dir() {
            zex_extension::check::deliver(
                req_id,
                zex_extension::CheckReport::failed(format!(
                    "目录不存在：{}（扩展 id 要和 ~/.zex/extensions 下的目录名一致）",
                    dir.display()
                )),
            );
            return;
        }
        // 校验可能比「打开」先发生（模型写完就想验），所以这里重新扫一遍：
        // 用启动时的快照会找不到刚生成出来的目录
        self.extensions = zex_extension::scan_default();
        let Some(ext) = self.extensions.iter().find(|e| e.id == app_id).cloned() else {
            zex_extension::check::deliver(
                req_id,
                zex_extension::CheckReport::failed(format!(
                    "目录存在但没有解析出扩展 `{app_id}`：检查 gpui-shell.json 是否存在且 id 一致"
                )),
            );
            return;
        };

        // 宿主没建就先建：校验也要有 VM 才能求值脚本
        if self.ext_host.is_none() {
            let bridge = zex_extension::AgentBridge::new(self.cmd_tx.clone());
            match zex_extension::ExtensionHost::new(cx, bridge) {
                Ok(h) => {
                    // 授权表随宿主一起交过去：网络闸门认的就是它
                    sync_grants(&h, &self.config);
                    self.ext_host = Some(std::rc::Rc::new(h));
                }
                Err(e) => {
                    zex_extension::check::deliver(
                        req_id,
                        zex_extension::CheckReport::failed(format!("无法初始化扩展运行时：{e}")),
                    );
                    return;
                }
            }
        }
        let Some(host) = self.ext_host.clone() else {
            zex_extension::check::deliver(
                req_id,
                zex_extension::CheckReport::failed("扩展运行时不可用"),
            );
            return;
        };

        // **不开窗口**。这里曾经建过一个 `show: false` 的临时窗口去物化一帧，
        // 但这条路在 Linux 上走不通，两个原因都实测过：
        //
        // 1. `WindowOptions.show` 在 X11/Wayland 后端根本没被读取 ——
        //    所谓"隐藏窗口"用户看到的就是一个真的空窗口；
        // 2. `window.remove_window()` 只是把窗口从 `cx.windows` 里摘掉，
        //    X 那边的窗口一直留着（`xdotool search --onlyvisible` 数得到），
        //    也就是说它会永久留在屏幕上。
        //
        // 所以校验退回到不依赖窗口的那一半：真求值一遍脚本。模块解析、
        // 导入名、默认导出都在这一步暴露 —— 手写扩展最常错的就是这些。
        // 渲染（元素树、坐标）只能在真的打开面板时才有，那需要用户在场，
        // 不该由一句工具调用偷偷替他开窗。
        let report = match host.validate_script(&ext) {
            Ok(()) => zex_extension::CheckReport::validated(
                "脚本求值通过（模块解析、导入名、默认导出都正常）。\
                 没有渲染，所以看不到元素树 —— 要确认实际画面请让用户点开这个扩展。",
            ),
            Err(e) => zex_extension::CheckReport::failed(e),
        };
        zex_extension::check::deliver(req_id, report);
    }

    /// 打开（或切到）一个扩展面板。
    ///
    /// 第一次调会建宿主、加载脚本；已存在的标签只切过去。
    fn open_extension(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ext) = self.extensions.iter().find(|e| e.id == id).cloned() else {
            self.status = format!("没有名为 `{id}` 的应用");
            cx.notify();
            return false;
        };

        let session = ext.session_id();
        let key = format!("ext:{id}");
        if let Some(rp) = &mut self.right_panel {
            if let Some(i) = rp.tabs.iter().position(|t| t.key() == key) {
                rp.active = i;
                cx.notify();
                return true;
            }
        }

        // 会话必须先落库：tasks 表对 sessions 有外键，
        // 没有这一行的话 create_task 会静默失败（`let _ =` 吞掉了）
        let _ = self.store.ensure_session(
            &zex_core::SessionId::from(session.as_str()),
            &format!("应用：{}", ext.name),
            &self.model,
            &self.cwd,
        );

        // 宿主按需建：一个 VM 是十几 MiB 常驻，不该跟着启动走
        if self.ext_host.is_none() {
            let bridge = zex_extension::AgentBridge::new(self.cmd_tx.clone());
            match zex_extension::ExtensionHost::new(cx, bridge) {
                Ok(h) => {
                    // 授权表随宿主一起交过去：网络闸门认的就是它
                    sync_grants(&h, &self.config);
                    self.ext_host = Some(std::rc::Rc::new(h));
                }
                Err(e) => {
                    self.status = format!("无法初始化扩展运行时：{e}");
                    cx.notify();
                    return false;
                }
            }
        }
        let Some(host) = self.ext_host.clone() else {
            return false;
        };

        // 挂载结果：成功就拿到 View，失败就把错误摆进面板
        let (view, error) = match host.mount(&ext, window, cx) {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let status = if error.is_none() {
            "就绪".to_string()
        } else {
            "失败".to_string()
        };

        let tab = PanelTab::Extension(ExtensionPanel {
            id: ext.id.clone(),
            name: ext.name.clone(),
            session,
            view,
            error,
            streaming: None,
            status,
            net_request: pending_net(&self.config, &ext),
            clip_request: pending_clipboard(&self.config, &ext),
            clip_read: ext.request.clipboard_read,
            clip_write: ext.request.clipboard_write,
        });
        self.ext_mtimes.insert(ext.id.clone(), Self::ext_stamp(&ext));
        self.activate_or_push_tab(tab, cx);
        true
    }

    /// 把扩展的流式输出/结束路由到它那个面板，并唤醒等回包的脚本。
    ///
    /// 返回 true 表示这个事件属于某个扩展，已经消费掉。
    fn mirror_to_extension(&mut self, ev: &CoreEvent, cx: &mut Context<Self>) -> bool {
        let (session, is_end) = match ev {
            CoreEvent::TurnStarted { session, .. } => (session, false),
            CoreEvent::Token { session, .. } => (session, false),
            CoreEvent::TurnEnded { session, .. } => (session, true),
            _ => return false,
        };
        let session = session.as_str().to_string();
        // 只接扩展自己的会话：前缀是契约，不认前缀的会话归别处
        if !session.starts_with("ext-") {
            return false;
        }
        let Some(host) = self.ext_host.clone() else {
            return false;
        };
        let Some(rp) = &mut self.right_panel else {
            return false;
        };
        let Some(panel) = rp.tabs.iter_mut().find_map(|t| match t {
            PanelTab::Extension(p) if p.session == session => Some(p),
            _ => None,
        }) else {
            return false;
        };

        match ev {
            CoreEvent::TurnStarted { .. } => {
                panel.status = "运行中".into();
                panel.streaming = Some(String::new());
            }
            CoreEvent::Token { text, .. } => {
                if let Some(buf) = panel.streaming.as_mut() {
                    buf.push_str(text);
                }
            }
            CoreEvent::TurnEnded { reason, .. } => {
                let text = panel.streaming.take().unwrap_or_default();
                panel.status = "就绪".into();
                let reply = match reason {
                    zex_core::event::EndReason::Completed => zex_extension::Reply::ok(text.clone()),
                    other => {
                        zex_extension::Reply::err(format!("这一轮没有正常结束：{other:?}"))
                    }
                };
                // 唤醒还在 await 的脚本；没人在等就什么也不做
                host.bridge().complete(&session, reply);
                // 同时把结果交给宿主持有并重跑脚本 render：
                // 脚本可能没在 await（比如只是轮询 reply()），
                // 而 render 只在"描述过期"时才重跑，宿主改了状态就得明说一次
                if let Some(view) = panel.view.clone() {
                    host.deliver(&session, &text, &mut view.clone(), cx);
                }
            }
            _ => {}
        }
        // `is_end` 目前只用于可读性：结束的分支已在 match 里处理
        let _ = is_end;
        let _ = cx;
        true
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
                    // 设置页是"盖在主区之上"的，不关掉的话切过去也看不见对话
                    this.mode = ViewMode::Chat;
                    this.settings = None;
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
                    // Windows 上必须走 window_ops：gpui 的
                    // start_window_move 在那边是空函数，zoom 不会还原。
                    if ev.click_count == 2 {
                        zex_app::window_ops::toggle_maximize(window);
                    } else {
                        zex_app::window_ops::start_drag(window);
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
                    zex_app::window_ops::toggle_maximize,
                    cx,
                ))
                .child(btn(IconName::Close, true, |w| w.remove_window(), cx));
        }

        bar.into_any_element()
    }

    /// 设置页：左侧分区导航，右侧内容。
    fn render_settings(&self, tab: SettingsTab, cx: &mut Context<Self>) -> AnyElement {
        let items: [(SettingsTab, &str, &str); 5] = [
            (SettingsTab::Model, "模型", "provider 与默认模型"),
            (SettingsTab::Skill, "技能", "可复用的能力包"),
            (SettingsTab::Mcp, "MCP", "外部工具服务"),
            (SettingsTab::Extension, "应用", "自己写的小应用"),
            (SettingsTab::About, "关于", "版本与路径"),
        ];

        let mut nav = div()
            .w(px(200.0))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_r_1()
            .border_color(rgb(BORDER_SOFT));

        for (t, label, hint) in items {
            let active = t == tab;
            let mut row = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .rounded(px(8.0))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(BG_HOVER)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                        this.settings = Some(t);
                        // 模型页要用 models.dev 目录：换到这里才开始拉，
                        // 不跟着启动走 —— 绝大多数会话用不上它
                        if t == SettingsTab::Model {
                            this.ensure_catalog(cx);
                        }
                        // 扩展页每次进来重扫：模型可能刚用 ext_install 装了一个
                        if t == SettingsTab::Extension {
                            this.extensions = zex_extension::scan_default();
                        }
                        cx.notify();
                    }),
                );
            if active {
                row = row.bg(rgb(BG_HOVER));
            }
            nav = nav.child(
                row.child(
                    div()
                        .text_sm()
                        .text_color(rgb(if active { TEXT } else { TEXT_MUTED }))
                        .child(label),
                )
                .child(div().text_xs().text_color(rgb(TEXT_DIM)).child(hint)),
            );
        }

        let content = match tab {
            SettingsTab::Model => self.render_settings_model(cx),
            SettingsTab::Skill => self.render_settings_skill(cx),
            SettingsTab::Mcp => self.render_settings_mcp(cx),
            SettingsTab::Extension => self.render_settings_extension(cx),
            SettingsTab::About => Self::render_settings_about(),
        };

        div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(nav)
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .overflow_y_scroll()
                    .p_8()
                    .child(content),
            )
            .into_any_element()
    }
    /// 模型页：provider 增删改 + 每个 provider 下的模型增删。
    fn render_settings_model(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg_path = zex_app::config_dir().join("config.json");
        let mut col = div()
            .flex()
            .flex_col()
            .gap_4()
            .max_w(px(820.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("模型"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("add-provider")
                            .primary()
                            .compact()
                            .label("添加 provider")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_provider_form(None, window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("配置文件：{}", cfg_path.display())),
            );

        // 表单开着就摆在最上面：新建时它就是主任务
        if self.provider_form.is_some() {
            col = col.child(self.render_provider_form(cx));
        }

        // 删除确认条
        if let Some(name) = &self.provider_pending_delete {
            let n = name.clone();
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(DANGER))
                    .bg(rgb(DANGER_SOFT))
                    .px_4()
                    .py_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(DANGER))
                            .child(format!("删除 provider `{n}`？它下面的模型也会一并消失。")),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("del-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.provider_pending_delete = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("del-ok")
                            .danger()
                            .compact()
                            .label("删除")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                                this.delete_provider(&n, cx);
                            })),
                    ),
            );
        }

        if self.config.providers.is_empty() && self.provider_form.is_none() {
            return col
                .child(
                    div()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG_TOOL))
                        .p_4()
                        .text_sm()
                        .text_color(rgb(TEXT_MUTED))
                        .child("还没有任何 provider。点上面的「添加 provider」开始。"),
                )
                .into_any_element();
        }

        for (name, entry) in &self.config.providers {
            let pname = name.clone();
            let card = div()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(BORDER))
                .bg(rgb(BG_MAIN))
                .p_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child(name.clone()),
                        )
                        .child(pill(entry.kind.clone(), BG_HOVER, TEXT_MUTED))
                        .child(div().flex_1())
                        .child(
                            Button::new(ElementId::Name(format!("edit-{name}").into()))
                                .ghost()
                                .compact()
                                .icon(IconName::Pencil)
                                .tooltip("编辑 provider")
                                .on_click(cx.listener({
                                    let n = pname.clone();
                                    move |this, _: &ClickEvent, window, cx| {
                                        this.open_provider_form(Some(n.clone()), window, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new(ElementId::Name(format!("del-{name}").into()))
                                .ghost()
                                .compact()
                                .icon(IconName::Trash)
                                .tooltip("删除 provider")
                                .on_click(cx.listener({
                                    let n = pname.clone();
                                    move |this, _: &ClickEvent, _w, cx| {
                                        this.provider_pending_delete = Some(n.clone());
                                        cx.notify();
                                    }
                                })),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(entry.base_url.clone()),
                )
                .child(match &entry.api_key_env {
                    Some(env) => {
                        // 只显示环境变量名在不在，绝不读值
                        let set = std::env::var(env).is_ok();
                        div()
                            .text_xs()
                            .text_color(rgb(if set { OK } else { DANGER }))
                            .child(format!(
                                "{} {}",
                                if set { "密钥已就绪：" } else { "缺少环境变量：" },
                                env
                            ))
                    }
                    None => div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child("未声明 api_key_env（适用于本地模型）"),
                });

            // 模型列表：以当前配置为准（加一个就该立刻多一个），
            // 配置为空时回退到启动时解析出的清单（含内置默认）
            let models: Vec<String> = if entry.models.is_empty() {
                self.models_by_provider.get(name).cloned().unwrap_or_default()
            } else {
                entry.models.clone()
            };
            let mut list = div().flex().flex_col().gap_1().pt_2();
            if models.is_empty() {
                list = list.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child("还没有配置模型"),
                );
            }
            // 从目录选一家就能带进十几个模型，全铺出来会把其它 provider
            // 顶出屏幕，所以默认只露前几条
            const MODEL_SHOWN: usize = 8;
            let expanded = self.models_expanded.contains(name);
            let total = models.len();
            let visible = if expanded || total <= MODEL_SHOWN {
                total
            } else {
                MODEL_SHOWN
            };
            for m in models.iter().take(visible) {
                let full = format!("{name}/{m}");
                let is_default = self.config.default_model.as_deref() == Some(full.as_str());
                let target = full.clone();
                let mp = pname.clone();
                let mm = m.clone();
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .font_family(MONO)
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child(m.clone()),
                    )
                    .child(div().flex_1());
                if is_default {
                    row = row.child(pill("默认".to_string(), OK_SOFT, OK));
                } else {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(rgb(BRAND))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                    this.set_default_model(&target, cx);
                                }),
                            )
                            .child("设为默认"),
                    );
                }
                // 已配的参数摘要：让人一眼看出改过什么
                if let Some(p) = entry.params.get(m) {
                    let mut bits = Vec::new();
                    if let Some(t) = p.temperature {
                        bits.push(format!("温度 {t:.2}"));
                    }
                    if let Some(mt) = p.max_tokens {
                        bits.push(format!("上限 {mt}"));
                    }
                    if !bits.is_empty() {
                        row = row.child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_DIM))
                                .child(bits.join(" · ")),
                        );
                    }
                }
                let params_p = pname.clone();
                let params_m = m.clone();
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(rgb(BRAND))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                this.open_params_form(&params_p, &params_m, window, cx);
                            }),
                        )
                        .child("参数"),
                );
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(DANGER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                this.remove_model(&mp, &mm, cx);
                            }),
                        )
                        .child("移除"),
                );
                list = list.child(row);

                // 参数表单就摆在它那一行下面，改哪个模型一目了然
                if self.editing_params_for.as_ref() == Some(&(pname.clone(), m.clone())) {
                    list = list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .mt_1()
                            .ml_6()
                            .mb_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BRAND))
                            .p_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(format!("{pname}/{m} 的参数（留空 = 用服务端默认）")),
                            )
                            .child(Input::new(&self.pf_temperature))
                            .child(Input::new(&self.pf_max_tokens))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().flex_1())
                                    .child(
                                        Button::new(ElementId::Name(
                                            format!("param-no-{m}").into(),
                                        ))
                                        .ghost()
                                        .compact()
                                        .label("取消")
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, _w, cx| {
                                                this.editing_params_for = None;
                                                cx.notify();
                                            },
                                        )),
                                    )
                                    .child(
                                        Button::new(ElementId::Name(
                                            format!("param-ok-{m}").into(),
                                        ))
                                        .primary()
                                        .compact()
                                        .label("保存")
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.save_params_form(window, cx);
                                            },
                                        )),
                                    ),
                            ),
                    );
                }
            }

            // 折叠开关：模型太多时只露前几条
            if total > MODEL_SHOWN {
                let pn = pname.clone();
                let now = expanded;
                list = list.child(
                    div()
                        .text_xs()
                        .text_color(rgb(BRAND))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .py_1()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                if now {
                                    this.models_expanded.remove(&pn);
                                } else {
                                    this.models_expanded.insert(pn.clone());
                                }
                                cx.notify();
                            }),
                        )
                        .child(if expanded {
                            "收起".to_string()
                        } else {
                            format!("展开其余 {} 个", total - MODEL_SHOWN)
                        }),
                );
            }

            // 添加模型：能匹配上目录就点选，匹配不上就手填
            if self.adding_model_for.as_deref() == Some(name.as_str()) {
                list = list.child(self.render_model_add_panel(name, cx));
            } else {
                let for_provider = pname.clone();
                let in_catalog = self
                    .md_catalog
                    .as_ref()
                    .map(|c| c.find(name).is_some())
                    .unwrap_or(false);
                list = list.child(
                    div()
                        .pt_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(BRAND))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        this.adding_model_for = Some(for_provider.clone());
                                        this.pf_model.update(cx, |st, cx| {
                                            st.set_value("", window, cx)
                                        });
                                        this.md_model_q.update(cx, |st, cx| {
                                            st.set_value("", window, cx)
                                        });
                                        cx.notify();
                                    }),
                                )
                                .child(if in_catalog {
                                    "+ 从目录添加模型"
                                } else {
                                    "+ 添加模型"
                                }),
                        ),
                );
            }

            col = col.child(card.child(list));
        }

        col.into_any_element()
    }

    /// provider 表单里的「从 models.dev 选择」块。
    ///
    /// 放在手填字段上方：能搜到的人就不该去抄 base_url，
    /// 搜不到的继续往下填，两条路并存。
    fn render_md_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div().flex().flex_col().gap_2();

        // 状态行：有目录就报数量，没有就说清楚是什么情况
        let mut head = div().flex().items_center().gap_2();
        match (&self.md_catalog, self.md_loading, &self.md_error) {
            (Some(cat), _, _) => {
                head = head.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(format!("models.dev · {} 个 provider", cat.providers.len())),
                );
            }
            (None, true, _) => {
                head = head.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child("正在拉取 models.dev 目录…"),
                );
            }
            (None, false, Some(e)) => {
                head = head.child(
                    div()
                        .text_xs()
                        .text_color(rgb(DANGER))
                        .child(format!("目录拉取失败：{e}")),
                );
            }
            (None, false, None) => {
                head = head.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child("目录未加载"),
                );
            }
        }
        head = head.child(div().flex_1());
        if !self.md_loading {
            head = head.child(
                div()
                    .text_xs()
                    .text_color(rgb(BRAND))
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                            this.refresh_catalog(cx);
                        }),
                    )
                    .child("重新拉取"),
            );
        }
        col = col.child(head);

        // 已选提示：把"接下来会发生什么"说在前头
        if let Some(id) = &self.md_picked {
            if self.md_catalog.is_some() {
                col = col.child(
                    div()
                        .text_xs()
                        .text_color(rgb(OK))
                        .child(format!("已选 `{id}`，保存时会连同它的模型一起加入")),
                );
            }
        }

        let Some(cat) = self.md_catalog.clone() else {
            return col.into_any_element();
        };
        col = col.child(Input::new(&self.md_provider_q));

        let q = self.md_provider_q.read(cx).value().to_string();
        // 两级搜索：先找 provider 名，一个都没有再看谁能提供这个模型。
        // 一上来就按模型名搜的话，搜 "deepseek" 会先铺满一屏聚合网关。
        let mut hits = cat.search(&q);
        let mut via_models = false;
        if hits.is_empty() && !q.trim().is_empty() {
            hits = cat.search_by_model(&q);
            via_models = true;
        }
        if hits.is_empty() {
            return col
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(format!("没有匹配 `{q}` 的 provider —— 继续手填下面的字段")),
                )
                .into_any_element();
        }

        // 搜索结果不展开到底：openrouter 一家就 421 个模型，
        // 把名字相近的一堆铺满屏幕并不会更好选
        const SHOWN: usize = 8;
        if via_models {
            col = col.child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("没有名为 `{q}` 的 provider，以下提供包含它的模型")),
            );
        }
        let mut list = div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER_SOFT))
            .overflow_hidden();
        for (i, p) in hits.iter().take(SHOWN).enumerate() {
            let pid = p.id.clone();
            let mut row = div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(BG_HOVER)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        this.pick_provider_from_catalog(&pid, window, cx);
                    }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(p.name.clone()),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(p.id.clone()),
                )
                .child(pill(p.kind().to_string(), BG_HOVER, TEXT_MUTED))
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(format!("{} 个模型", p.models.len())),
                );
            // 走原生 SDK 的 provider 用我们的兼容层调不通，先标出来
            if !p.natively_supported() {
                row = row.child(pill("需自备网关".to_string(), DANGER_SOFT, DANGER));
            }
            if i % 2 == 1 {
                row = row.bg(rgb(BG_TOOL));
            }
            list = list.child(row);
        }
        col = col.child(list);
        if hits.len() > SHOWN {
            col = col.child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!(
                        "还有 {} 个结果，继续输入以缩小范围",
                        hits.len() - SHOWN
                    )),
            );
        }
        col.into_any_element()
    }

    /// 选中目录里的 provider：把能填的都填上。
    ///
    /// 只填不存 —— 用户还得看一眼协议对不对、要不要改环境变量名，
    /// 直接落盘就把"选择"变成了"既成事实"。
    fn pick_provider_from_catalog(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cat) = self.md_catalog.clone() else {
            return;
        };
        let Some(p) = cat.find(id) else {
            return;
        };
        let (name, kind, base) = (p.id.clone(), p.kind().to_string(), p.base_url());
        let env = p.env.first().cloned().unwrap_or_default();
        let model_count = p.models.len();

        self.pf_name
            .update(cx, |st, cx| st.set_value(name.clone(), window, cx));
        self.pf_kind
            .update(cx, |st, cx| st.set_value(kind, window, cx));
        self.pf_base_url
            .update(cx, |st, cx| st.set_value(base, window, cx));
        self.pf_key_env
            .update(cx, |st, cx| st.set_value(env, window, cx));

        self.md_picked = Some(id.to_string());
        self.status = format!("已从目录填入 `{id}`（{model_count} 个模型）");
        cx.notify();
    }

    /// provider 新建/编辑表单。
    fn render_provider_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let editing = self.provider_form.clone().flatten();
        let title = match &editing {
            Some(n) => format!("编辑 provider · {n}"),
            None => "添加 provider".into(),
        };

        let field = |label: &str, input: Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child(label.to_string()))
                .child(Input::new(&input))
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BRAND))
            .bg(rgb(BG_MAIN))
            .p_4()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(title),
            )
            .child(self.render_md_picker(cx))
            .child(div().h(px(1.0)).bg(rgb(BORDER_SOFT)))
            .child(field("名称", self.pf_name.clone()))
            .child(field("协议（openai-compat / anthropic）", self.pf_kind.clone()))
            .child(field("base_url", self.pf_base_url.clone()))
            .child(field("api_key_env（可留空，本地模型）", self.pf_key_env.clone()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("pf-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.provider_form = None;
                                // 选中记录跟着表单走，否则下次打开会带着上次的“已选”提示
                                this.md_picked = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("pf-save")
                            .primary()
                            .compact()
                            .label("保存")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save_provider_form(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }


    /// 确保 models.dev 目录就绪。
    ///
    /// 策略：有缓存先上缓存（哪怕过期），过期了再后台静默刷新 ——
    /// 界面不该为了“最新”而白屏等网络。
    fn ensure_catalog(&mut self, cx: &mut Context<Self>) {
        if self.md_catalog.is_some() || self.md_loading {
            return;
        }
        if let Some((cat, stale)) = zex_app::models_dev::load_cache() {
            self.md_catalog = Some(std::sync::Arc::new(cat));
            cx.notify();
            if !stale {
                return;
            }
        }
        self.refresh_catalog(cx);
    }

    /// 从上游重新拉目录。
    ///
    /// 网络与解析都放在独立线程：gpui 的 executor 不跑 IO，
    /// 在那儿 await reqwest 会直接 panic（没有 reactor）。
    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        if self.md_loading {
            return;
        }
        self.md_loading = true;
        self.md_error = None;
        cx.notify();

        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("models-dev".into())
            .spawn(move || {
                let _ = tx.send(zex_app::models_dev::fetch_blocking());
            });
        if let Err(e) = spawned {
            self.md_loading = false;
            self.md_error = Some(format!("无法启动拉取线程: {e}"));
            cx.notify();
            return;
        }

        cx.spawn(async move |this, cx| {
            let res = rx.await;
            let _ = this.update(cx, |this, cx| {
                this.md_loading = false;
                match res {
                    Ok(Ok(cat)) => {
                        zex_app::models_dev::save_cache(&cat);
                        this.md_catalog = Some(std::sync::Arc::new(cat));
                        this.md_error = None;
                    }
                    Ok(Err(e)) => this.md_error = Some(e.to_string()),
                    Err(_) => this.md_error = Some("拉取线程异常退出".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开 provider 表单。`Some(name)` = 编辑现有，`None` = 新增。
    fn open_provider_form(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (kind, base, key) = match &name {
            Some(n) => match self.config.providers.get(n) {
                Some(e) => (
                    e.kind.clone(),
                    e.base_url.clone(),
                    e.api_key_env.clone().unwrap_or_default(),
                ),
                None => (String::new(), String::new(), String::new()),
            },
            None => ("openai-compat".to_string(), String::new(), String::new()),
        };
        let name_val = name.clone().unwrap_or_default();

        self.md_picked = None;
        self.pf_name
            .update(cx, |st, cx| st.set_value(name_val, window, cx));
        self.pf_kind
            .update(cx, |st, cx| st.set_value(kind, window, cx));
        self.pf_base_url
            .update(cx, |st, cx| st.set_value(base, window, cx));
        self.pf_key_env
            .update(cx, |st, cx| st.set_value(key, window, cx));

        self.provider_form = Some(name);
        cx.notify();
    }

    /// 保存 provider 表单。
    ///
    /// 改名会连带搬走它下面的模型列表和默认模型指针 ——
    /// 否则改个名字就把整组模型弄丢了。
    fn save_provider_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.provider_form.clone() else {
            return;
        };
        let name = self.pf_name.read(cx).value().trim().to_string();
        let kind = self.pf_kind.read(cx).value().trim().to_string();
        let base = self.pf_base_url.read(cx).value().trim().to_string();
        let key = self.pf_key_env.read(cx).value().trim().to_string();

        if kind != "openai-compat" && kind != "anthropic" {
            self.status = "kind 只能是 openai-compat 或 anthropic".into();
            cx.notify();
            return;
        }
        if base.is_empty() {
            self.status = "base_url 不能为空".into();
            cx.notify();
            return;
        }

        // 从目录选中的话，把它的对话模型一并带进来。
        // 只在“新建”或原配置本来就没模型时这么做 ——
        // 编辑一个已有清单的 provider 时擅自扩大到全量，会改掉用户显式删掉的项。
        let from_catalog: Vec<String> = match (&self.md_picked, &self.md_catalog) {
            (Some(id), Some(cat)) => cat
                .find(id)
                .map(|p| p.models.iter().map(|m| m.id.clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let existing: Vec<String> = editing
            .as_deref()
            .and_then(|n| self.config.providers.get(n))
            .map(|e| e.models.clone())
            .unwrap_or_default();

        let entry = zex_app::ProviderEntry {
            kind,
            base_url: base,
            api_key_env: if key.is_empty() { None } else { Some(key) },
            headers: Default::default(),
            // 有旧清单就継承（传空让 upsert_provider 接），否则用目录里的
            models: if existing.is_empty() {
                from_catalog.clone()
            } else {
                Vec::new()
            },
            params: Default::default(),
        };
        let added = if existing.is_empty() {
            from_catalog.len()
        } else {
            0
        };

        match zex_app::config::upsert_provider(
            &mut self.config,
            &name,
            entry,
            editing.as_deref(),
        ) {
            Ok(()) => {
                self.provider_form = None;
                self.md_picked = None;
                let msg = if added > 0 {
                    format!("已保存 provider `{name}`，带入 {added} 个模型")
                } else {
                    format!("已保存 provider `{name}`")
                };
                self.persist_config(msg, cx);
            }
            Err(e) => {
                self.status = e.to_string();
                cx.notify();
            }
        }
    }

    /// 删除 provider（连同它下面的模型）。
    fn delete_provider(&mut self, name: &str, cx: &mut Context<Self>) {
        zex_app::config::remove_provider(&mut self.config, name);
        self.provider_pending_delete = None;
        self.persist_config(
            if self.config.providers.is_empty() {
                format!("已删除 `{name}`，现在没有任何 provider")
            } else {
                format!("已删除 `{name}`")
            },
            cx,
        );
    }

    /// 打开某个模型的参数表单。
    fn open_params_form(
        &mut self,
        provider: &str,
        model: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = self
            .config
            .providers
            .get(provider)
            .and_then(|e| e.params.get(model))
            .cloned()
            .unwrap_or_default();
        // 用固定两位小数展示，避免 0.7000000001 这种浮点尾巴
        self.pf_temperature.update(cx, |st, cx| {
            st.set_value(
                p.temperature.map(|t| format!("{t:.2}")).unwrap_or_default(),
                window,
                cx,
            )
        });
        self.pf_max_tokens.update(cx, |st, cx| {
            st.set_value(
                p.max_tokens.map(|m| m.to_string()).unwrap_or_default(),
                window,
                cx,
            )
        });
        self.editing_params_for = Some((provider.to_string(), model.to_string()));
        cx.notify();
    }

    /// 保存模型参数。两个框都留空 = 清掉参数，回到服务端默认。
    fn save_params_form(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((provider, model)) = self.editing_params_for.clone() else {
            return;
        };
        let t_raw = self.pf_temperature.read(cx).value().trim().to_string();
        let m_raw = self.pf_max_tokens.read(cx).value().trim().to_string();

        let temperature = if t_raw.is_empty() {
            None
        } else {
            match t_raw.parse::<f32>() {
                // 支持的服务端范围普遍是 0–2，超出直接拒掉，
                // 免得发出去被 API 打回来还看不懂原因
                Ok(v) if (0.0..=2.0).contains(&v) => Some(v),
                Ok(v) => {
                    self.status = format!("温度 {v} 超出范围（0–2）");
                    cx.notify();
                    return;
                }
                Err(_) => {
                    self.status = "温度必须是数字".into();
                    cx.notify();
                    return;
                }
            }
        };

        let max_tokens = if m_raw.is_empty() {
            None
        } else {
            match m_raw.parse::<u32>() {
                Ok(v) if v > 0 => Some(v),
                Ok(_) => {
                    self.status = "最大输出 token 必须大于 0".into();
                    cx.notify();
                    return;
                }
                Err(_) => {
                    self.status = "最大输出 token 必须是整数".into();
                    cx.notify();
                    return;
                }
            }
        };

        if let Some(entry) = self.config.providers.get_mut(&provider) {
            if temperature.is_none() && max_tokens.is_none() {
                // 全空 = 清掉这条记录，别在配置里留一个 `{}`
                entry.params.remove(&model);
            } else {
                entry
                    .params
                    .insert(model.clone(), zex_app::config::ModelParams { temperature, max_tokens });
            }
        }

        self.editing_params_for = None;
        // 参数每轮从配置文件重读，所以明确说"下一轮"就生效 ——
        // 说"重启后生效"会让人白等一次重启
        self.status = match zex_app::save_file_config(&self.config) {
            Ok(()) => format!("已更新 `{provider}/{model}` 的参数（下一轮对话生效）"),
            Err(e) => format!("保存失败：{e}"),
        };
        cx.notify();
    }

    /// 把"内置回退"的模型清单固化进配置。
    ///
    /// 界面上显示的是解析后的清单（config 没写 models 时回退到内置表），
    /// 但增删改只能改 `entry.models`。不先固化的话，
    /// 用户对着内置的 `deepseek-flash` 点"移除"，配置里根本找不到它。
    fn materialize_models(&mut self, provider: &str) {
        let builtin = self.models_by_provider.get(provider).cloned();
        if let Some(entry) = self.config.providers.get_mut(provider) {
            if entry.models.is_empty() {
                if let Some(list) = builtin {
                    entry.models = list;
                }
            }
        }
    }

    /// 「添加模型」展开后的面板：上面是目录候选，下面是手填入口。
    fn render_model_add_panel(&self, provider: &str, cx: &mut Context<Self>) -> AnyElement {
        let for_provider = provider.to_string();
        // 以配置文件里的清单为准：加完一个得立刻反映到候选列表，
        // 而 models_by_provider 是启动时的快照，加完不会变。
        // 配置里为空时才回退到快照（那意味着用的是内置清单）。
        let configured: Vec<String> = self
            .config
            .providers
            .get(provider)
            .map(|e| e.models.clone())
            .unwrap_or_default();
        let existing: std::collections::HashSet<String> = if configured.is_empty() {
            self.models_by_provider
                .get(provider)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect()
        } else {
            configured.into_iter().collect()
        };

        let mut panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .mt_2()
            .ml_6()
            .mb_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(BRAND))
            .p_3();

        let catalog_hit = self.md_catalog.as_ref().and_then(|c| c.find(provider));
        if let Some(p) = catalog_hit {
            let q = self.md_model_q.read(cx).value().to_string();
            let q_lower = q.trim().to_ascii_lowercase();
            let candidates: Vec<_> = p
                .models
                .iter()
                .filter(|m| !existing.contains(&m.id))
                .filter(|m| {
                    q_lower.is_empty()
                        || m.id.to_ascii_lowercase().contains(&q_lower)
                        || m.name.to_ascii_lowercase().contains(&q_lower)
                })
                .collect();

            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(format!(
                        "models.dev 里 `{}` 有 {} 个可用模型（已加入 {} 个）",
                        p.id,
                        p.models.len(),
                        existing.len()
                    )),
            );
            panel = panel.child(Input::new(&self.md_model_q));

            // 同屏最多 12 条：一个 openrouter 就 421 个，铺满屏幕反而难找
            const SHOWN: usize = 12;
            if candidates.is_empty() {
                panel = panel.child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(if q_lower.is_empty() {
                            "目录里的模型都加过了".to_string()
                        } else {
                            format!("没有匹配 `{q}` 的模型")
                        }),
                );
            } else {
                let mut rows = div()
                    .flex()
                    .flex_col()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER_SOFT))
                    .overflow_hidden();
                for (i, m) in candidates.iter().take(SHOWN).enumerate() {
                    let pid = for_provider.clone();
                    let mid = m.id.clone();
                    let summary = m.summary();
                    // 名称 + id + 价格 一行放不下（1M 上下文 · 393k 输出 · $0.15/$0.6 …）：
                    // 名称与 id 排一行、摘要排第二行，不靠截断硬塞
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .py_1p5()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(BG_HOVER)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                this.add_model_direct(&pid, &mid, cx);
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .min_w(px(0.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .min_w(px(0.0))
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(rgb(TEXT))
                                                .truncate()
                                                .child(m.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .font_family(MONO)
                                                .text_xs()
                                                .text_color(rgb(TEXT_DIM))
                                                .truncate()
                                                .child(m.id.clone()),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(TEXT_MUTED))
                                        .truncate()
                                        .child(summary),
                                ),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(BRAND))
                                .flex_none()
                                .child("添加"),
                        );
                    if i % 2 == 1 {
                        row = row.bg(rgb(BG_TOOL));
                    }
                    rows = rows.child(row);
                }
                panel = panel.child(rows);
                if candidates.len() > SHOWN {
                    panel = panel.child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(format!(
                                "还有 {} 个，输入关键词缩小范围",
                                candidates.len() - SHOWN
                            )),
                    );
                }
            }
            panel = panel.child(div().h(px(1.0)).bg(rgb(BORDER_SOFT)));
        }

        // 手填入口永远在：自建网关、私有部署、刚上线的新模型
        // 都可能还没进目录
        panel
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(if catalog_hit.is_some() {
                        "或手动输入模型名"
                    } else {
                        "输入模型名（未在 models.dev 收录）"
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.pf_model)))
                    .child(
                        Button::new("newmodel-ok")
                            .primary()
                            .compact()
                            .label("添加")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.add_model(&for_provider, window, cx);
                            })),
                    )
                    .child(
                        Button::new("newmodel-no")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.adding_model_for = None;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    /// 从目录直接加一个模型（不经输入框）。
    fn add_model_direct(&mut self, provider: &str, model: &str, cx: &mut Context<Self>) {
        self.materialize_models(provider);
        if !zex_app::config::add_model(&mut self.config, provider, model) {
            self.status = format!("`{provider}` 下已经有 `{model}` 了");
            cx.notify();
            return;
        }
        self.persist_config(format!("已给 `{provider}` 添加模型 `{model}`"), cx);
    }

    /// 往某个 provider 下加一个模型。
    fn add_model(&mut self, provider: &str, window: &mut Window, cx: &mut Context<Self>) {
        let m = self.pf_model.read(cx).value().trim().to_string();
        if m.is_empty() {
            self.adding_model_for = None;
            cx.notify();
            return;
        }
        self.materialize_models(provider);
        if !zex_app::config::add_model(&mut self.config, provider, &m) {
            self.status = format!("`{provider}` 下已经有 `{m}` 了");
            cx.notify();
            return;
        }
        self.adding_model_for = None;
        self.pf_model
            .update(cx, |st, cx| st.set_value("", window, cx));
        self.persist_config(format!("已给 `{provider}` 添加模型 `{m}`"), cx);
    }

    /// 从某个 provider 下移除一个模型。
    fn remove_model(&mut self, provider: &str, model: &str, cx: &mut Context<Self>) {
        self.materialize_models(provider);
        zex_app::config::remove_model(&mut self.config, provider, model);
        self.persist_config(format!("已移除 `{provider}/{model}`"), cx);
    }

    /// 把配置写回磁盘并更新状态栏。
    fn persist_config(&mut self, ok_msg: String, cx: &mut Context<Self>) {
        self.status = match zex_app::save_file_config(&self.config) {
            Ok(()) => format!("{ok_msg}（重启后生效）"),
            Err(e) => format!("保存失败：{e}"),
        };
        cx.notify();
    }

    /// 把某个 `provider/model` 设成默认并写回配置文件。
    fn set_default_model(&mut self, full: &str, cx: &mut Context<Self>) {
        self.config.default_model = Some(full.to_string());
        self.status = match zex_app::save_file_config(&self.config) {
            Ok(()) => {
                self.model = full.to_string();
                self.selected_model = None;
                format!("默认模型已改为 {full}，重启后生效")
            }
            Err(e) => format!("保存失败：{e}"),
        };
        cx.notify();
    }

    /// 技能页：列出已加载的技能，说明怎么新增。
    /// 扩展页：列出已安装的扩展，点开就挂到右侧面板。
    fn render_settings_extension(&self, cx: &mut Context<Self>) -> AnyElement {
        let dir = zex_extension::extensions_dir();
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(px(820.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("应用"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("ext-rescan")
                            .ghost()
                            .compact()
                            .label("重新扫描")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.extensions = zex_extension::scan_default();
                                this.status =
                                    format!("已扫描到 {} 个应用", this.extensions.len());
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("目录：{}", dir.display())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_MUTED))
                    .child(
                        "每个应用是一个子目录，里面放 gpui-shell.json（id / name / entry）、\
                         脚本入口，以及可选的 icon.svg（侧边栏显示的那个图标）。\
                         脚本里用 `import { ask } from \"zex.<id>\"` 调用 agent；\
                         示例见仓库 examples/extensions/demo，文档见 docs/EXTENSIONS.md。",
                    ),
            );

        if self.extensions.is_empty() {
            return col
                .child(
                    div()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG_TOOL))
                        .p_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(TEXT_MUTED))
                                .child("还没有任何应用。"),
                        )
                        .child(
                            div()
                                .font_family(MONO)
                                .text_xs()
                                .text_color(rgb(TEXT_DIM))
                                .child(format!(
                                    "mkdir -p {}/demo && cd {} && \\\n  echo '{{\"id\":\"demo\",\"name\":\"示例\"}}' > gpui-shell.json && \\\n  touch main.js",
                                    dir.join("demo").display(),
                                    dir.join("demo").display()
                                )),
                        ),
                )
                .into_any_element();
        }

        for ext in &self.extensions {
            let id = ext.id.clone();
            let is_pinned = self.config.pinned_extensions.iter().any(|x| x == &ext.id);
            let open = self.right_panel.as_ref().is_some_and(|rp| {
                rp.tabs.iter().any(|t| t.key() == format!("ext:{}", ext.id))
            });
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG_MAIN))
                    .p_4()
                    // 图标放在名字左边、和名字同高对齐；没有就不占位
                    .children(app_icon(ext.icon.as_ref(), 28.0, TEXT_MUTED))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(TEXT))
                                            .child(ext.name.clone()),
                                    )
                                    .child(pill(
                                        ext.id.clone(),
                                        BG_HOVER,
                                        TEXT_MUTED,
                                    )),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_xs()
                                    .text_color(rgb(TEXT_DIM))
                                    .truncate()
                                    .child(ext.dir.display().to_string()),
                            )
                            // 能力申请：模型也能写清单，所以"它想要什么"
                            // 必须摆在用户眼前，而不是埋在 JSON 里
                            .child({
                                let req = &ext.request;
                                // 已生效的那部分先摆出来。存储是**全局**授予的
                                // （宿主只开这一个能力），所以不管清单要不要，
                                // 每个应用都能用 —— 不写清楚，用户会以为
                                // "没申请就等于用不了"。
                                let live = div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(format!(
                                        "已生效：本地存储（全局共享）+ SQLite（每个应用一份，{}）；网络按清单白名单，需批准",
                                        zex_extension::catalog::state_dir()
                                            .join(zex_extension::store_db::DB_DIR)
                                            .display()
                                    ));
                                if req.is_empty() {
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(TEXT_DIM))
                                                .child("未申请任何能力"),
                                        )
                                        .child(live)
                                } else {
                                    let broad = req.broad();
                                    // 网络三态：没问过 / 明确拒绝 / 允许了这几条
                                    let net_state = match self
                                        .config
                                        .network_grants
                                        .get(&ext.id)
                                    {
                                        None => None,
                                        Some(list) if list.is_empty() => Some(Vec::new()),
                                        Some(list) => Some(list.clone()),
                                    };
                                    let mut line = div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .flex_wrap()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(if broad.is_empty() {
                                                    TEXT_MUTED
                                                } else {
                                                    DANGER
                                                }))
                                                .child(format!("申请：{}", req.summary())),
                                        );
                                    if !broad.is_empty() {
                                        line = line.child(pill(
                                            format!("宽授权：{}", broad.join("、")),
                                            DANGER_SOFT,
                                            DANGER,
                                        ));
                                    }
                                    line.child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(match &net_state {
                                                Some(list) if !list.is_empty() => OK,
                                                Some(_) => DANGER,
                                                None => TEXT_DIM,
                                            }))
                                            .child(match &net_state {
                                                Some(list) if !list.is_empty() => format!(
                                                    "· 已允许联网：{}",
                                                    list.join("、")
                                                ),
                                                Some(_) => "· 已拒绝联网".to_string(),
                                                None => "· 联网未确认".to_string(),
                                            }),
                                    )
                                    .child(live)
                                }
                            }),
                    )
                    .child(div().flex_1())
                    // 网络的允许 / 撤销：只有真申请了网络才出现，否则是废按钮
                    .child({
                        let id = ext.id.clone();
                        // 没有网络申请的就不给按钮：批准一个用不到的权限没意义
                        let needs = !ext.request.network_hosts.is_empty();
                        let approved = self
                            .config
                            .network_grants
                            .get(&ext.id)
                            .map(|l| !l.is_empty())
                            .unwrap_or(false);
                        if needs {
                            div()
                                .text_xs()
                                .px_2()
                                .py_1()
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_color(rgb(if approved { OK } else { DANGER }))
                                .hover(|st| st.bg(rgb(BG_HOVER)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                        this.grant_network(&id, !approved, w, cx);
                                    }),
                                )
                                .child(if approved { "撤销联网" } else { "允许联网" })
                        } else {
                            div()
                        }
                    })
                    // 剪贴板的允许 / 撤销：同样只在真申请了才出现。
                    // 与网络并列而不是合成一个"权限"按钮：用户撤销某一个
                    // 权限时，不该顺手把另一个也关掉。
                    .child({
                        let id = ext.id.clone();
                        let needs = ext.request.clipboard_read || ext.request.clipboard_write;
                        let approved = self
                            .config
                            .clipboard_grants
                            .get(&ext.id)
                            .copied()
                            .unwrap_or(false);
                        if needs {
                            div()
                                .text_xs()
                                .px_2()
                                .py_1()
                                .rounded(px(5.0))
                                .cursor_pointer()
                                .text_color(rgb(if approved { OK } else { DANGER }))
                                .hover(|st| st.bg(rgb(BG_HOVER)))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, w, cx| {
                                        this.grant_clipboard(&id, !approved, w, cx);
                                    }),
                                )
                                .child(if approved { "撤销剪贴板" } else { "允许剪贴板" })
                        } else {
                            div()
                        }
                    })
                    // 侧边栏占位开关。放按钮左边、用文字链的样式：
                    // 它是个"设置项"，和右边的"打开"不是一类动作
                    .child({
                        let id = ext.id.clone();
                        let pinned = is_pinned;
                        div()
                            .text_xs()
                            .px_2()
                            .py_1()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_color(rgb(if pinned { OK } else { TEXT_DIM }))
                            .hover(|st| st.bg(rgb(BG_HOVER)).text_color(rgb(BRAND)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                    this.toggle_pin(&id, cx);
                                }),
                            )
                            .child(if pinned {
                                "✓ 已固定到边栏"
                            } else {
                                "固定到边栏"
                            })
                    })
                    .child(
                        Button::new(ElementId::Name(format!("ext-open-{id}").into()))
                            .primary()
                            .compact()
                            .label(if open { "切过去" } else { "打开" })
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.open_extension(&id, window, cx);
                            })),
                    ),
            );
        }

        col.into_any_element()
    }

    fn render_settings_skill(&self, cx: &mut Context<Self>) -> AnyElement {
        let dir = zex_app::skills_dir();
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(px(780.0))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child("技能"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(format!("技能目录：{}", dir.display())),
            );

        let skills = self.skills.all();
        if skills.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG_TOOL))
                    .p_4()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(TEXT))
                            .child("还没有技能。"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(20.0))
                            .text_color(rgb(TEXT_MUTED))
                            .child("在技能目录里放一个 .md 文件就会被加载，格式见下："),
                    )
                    .child(
                        TextView::markdown(
                            ElementId::Name("skill-example".into()),
                            as_code_block(
                                "---\nname: rust-refactor\ndescription: 重构 Rust 代码时的检查清单\n---\n\n1. 先跑 cargo check 建立基线\n2. 改完再跑一次，确认没有新增错误\n3. cargo fmt",
                                "markdown",
                            ),
                        )
                        .text_size(px(12.5))
                        .line_height(px(19.0))
                        .text_color(rgb(TEXT))
                        .style(md_style_flat(1.0)),
                    ),
            );
        } else {
            for sk in skills {
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG_MAIN))
                    .p_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(sk.name.clone()),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_DIM))
                                    .child(format!("{} 字", sk.body.chars().count())),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(sk.description.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_DIM))
                            .child(sk.path.display().to_string()),
                    );
                // 正文展开看
                let key = format!("skill-{}", sk.name);
                let open = self.expanded_tools.contains(&key);
                let toggle_key = key.clone();
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(rgb(BRAND))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                let k = toggle_key.clone();
                                if !this.expanded_tools.remove(&k) {
                                    this.expanded_tools.insert(k);
                                }
                                cx.notify();
                            }),
                        )
                        .child(if open { "收起正文" } else { "查看正文" }),
                );
                if open {
                    card = card.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .line_height(px(20.0))
                            .text_color(rgb(TEXT))
                            .child(sk.body.clone()),
                    );
                }
                col = col.child(card);
            }
        }

        col.into_any_element()
    }

    /// MCP 页：连接状态 + 配置示例。
    fn render_settings_mcp(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(px(780.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("MCP 服务"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("mcp-reconnect")
                            .ghost()
                            .compact()
                            .icon(IconName::RefreshCw)
                            .tooltip("重新连接")
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                                this.reconnect_mcp(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child("在 config.json 的 mcp_servers 里声明；每个服务是独立子进程"),
            );

        if self.config.mcp_servers.is_empty() {
            col = col
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG_TOOL))
                        .p_4()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(TEXT))
                                .child("还没有配置 MCP 服务。"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .line_height(px(20.0))
                                .text_color(rgb(TEXT_MUTED))
                                .child("在 ~/.zex/config.json 里加一段，重启后生效："),
                        )
                        .child(
                            TextView::markdown(
                                ElementId::Name("mcp-example".into()),
                                as_code_block(
                                    "{\n  \"mcp_servers\": {\n    \"filesystem\": {\n      \"command\": \"npx\",\n      \"args\": [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"]\n    }\n  }\n}",
                                    "json",
                                ),
                            )
                            .text_size(px(12.5))
                            .line_height(px(19.0))
                            .text_color(rgb(TEXT))
                            .style(md_style_flat(1.0)),
                        ),
                );
        } else {
            for (name, cfg) in &self.config.mcp_servers {
                let st = self.mcp_status.iter().find(|s| &s.name == name);
                let (dot, label, bg) = match st {
                    Some(s) if s.ok => (OK, s.detail.clone(), OK_SOFT),
                    Some(s) => (DANGER, s.detail.clone(), DANGER_SOFT),
                    None => (TEXT_DIM, "未连接".to_string(), BG_HOVER),
                };
                col = col.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(BG_MAIN))
                        .p_4()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().size(px(6.0)).rounded_full().bg(rgb(dot)))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT))
                                        .child(name.clone()),
                                )
                                .child(pill(label, bg, dot)),
                        )
                        .child(
                            div()
                                .font_family(MONO)
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child(format!("{} {}", cfg.command, cfg.args.join(" "))),
                        ),
                );
            }
        }

        col.into_any_element()
    }

    /// 重新连一遍 MCP 服务。
    fn reconnect_mcp(&mut self, cx: &mut Context<Self>) {
        let cfg = self.config.clone();
        self.status = "重连 MCP…".into();
        let task = cx
            .background_executor()
            .spawn(async move { zex_app::connect_mcp_servers(&cfg) });
        cx.spawn(async move |this, cx| {
            let (_tools, status, _hub) = task.await;
            let _ = this.update(cx, |this, cx| {
                this.mcp_status = status;
                this.status = "MCP 已更新".into();
                cx.notify();
            });
        })
        .detach();
    }

    fn render_settings_about() -> AnyElement {
        let rows: [(&str, String); 3] = [
            ("版本", env!("CARGO_PKG_VERSION").to_string()),
            ("配置目录", zex_app::config_dir().display().to_string()),
            (
                "工作目录",
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
        ];
        let mut col = div()
            .flex()
            .flex_col()
            .gap_3()
            .max_w(px(780.0))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child("关于"),
            );
        for (k, v) in rows {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(90.0)).text_xs().text_color(rgb(TEXT_DIM)).child(k))
                    .child(div().text_xs().text_color(rgb(TEXT)).child(v)),
            );
        }
        col.into_any_element()
    }

    /// 侧边栏底部的扩展入口：一个固定入口 + 已固定的扩展。
    ///
    /// 为什么放在会话列表下面而不是上面：会话是主线，扩展是工具。
    /// 工具压在底部、只露已固定那几个，才不至于把会话标题挤走。
    ///
    /// 没固定任何扩展时也保留"扩展"这一行 —— 它是这个功能唯一的常驻入口，
    /// 藏起来的话用户永远找不到第二次。
    fn render_sidebar_extensions(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .pt_2()
            .pb_2()
            .border_t_1()
            .border_color(rgb(BORDER_SOFT));

        // 入口行：去扩展设置页
        col = col.child(
            div()
                .w_full()
                .h(px(32.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .text_sm()
                .text_color(rgb(TEXT_MUTED))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(BG_HOVER)))
                .child(
                    Icon::new(IconName::Blocks)
                        .text_size(px(14.0))
                        .text_color(rgb(TEXT_DIM)),
                )
                .child("应用")
                .child(div().flex_1())
                // 已固定几个，就在右边标几个：一眼看出还有没有空位
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child(match self.config.pinned_extensions.len() {
                            0 => String::new(),
                            n => format!("{n} 个"),
                        }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.settings = Some(SettingsTab::Extension);
                        cx.notify();
                    }),
                ),
        );

        // 已固定的扩展。配置里有、目录里没了的一律跳过（卸载后残留）
        for ext in &self.extensions {
            if !self.config.pinned_extensions.iter().any(|x| x == &ext.id) {
                continue;
            }
            let id = ext.id.clone();
            let open = self.right_panel.as_ref().is_some_and(|rp| {
                rp.tabs.iter().any(|t| t.key() == format!("ext:{id}"))
            });
            let mut row = div()
                .w_full()
                .h(px(32.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .text_sm()
                .cursor_pointer()
                .text_color(rgb(if open { TEXT } else { TEXT_MUTED }));
            // 已经开着的那一页给个底色，和会话列表的选中态一致
            if open {
                row = row.bg(rgb(BG_HOVER));
            }
            col = col.child(
                row
                    .hover(|s| s.bg(rgb(BG_HOVER)))
                    .child(match app_icon(
                        ext.icon.as_ref(),
                        14.0,
                        if open { BRAND } else { TEXT_MUTED },
                    ) {
                        // 有 icon.svg 就画它；没有就退回原作那个小圆点，
                        // 让老应用（和手写的半成品）看起来不像是坏了
                        Some(icon) => icon,
                        None => div()
                            .size(px(6.0))
                            .rounded_full()
                            .flex_none()
                            .bg(rgb(if open { BRAND } else { TEXT_DIM }))
                            .into_any_element(),
                    })
                    .child(div().flex_1().min_w(px(0.0)).truncate().child(ext.name.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            // 设置页开着就先收起来，否则面板被盖住、点了像没反应
                            this.settings = None;
                            this.open_extension(&id, window, cx);
                        }),
                    ),
            );
        }

        col.into_any_element()
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
                            // 设置页开着的话先收起来：用户点会话就是想看对话
                            this.settings = None;
                            this.mode = ViewMode::Chat;
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
                            this.settings = None;
                            this.mode = ViewMode::Chat;
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
                            this.settings = None;
                            cx.notify();
                        })),
                ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(list))
            // 扩展入口 + 已固定的扩展
            .child(self.render_sidebar_extensions(cx))
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
                        div()
                            .cursor_pointer()
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .text_color(rgb(if self.settings.is_some() { TEXT } else { TEXT_MUTED }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                                    // 再点一次收起，省得去找返回键
                                    this.settings = match this.settings {
                                        Some(_) => None,
                                        None => Some(SettingsTab::Model),
                                    };
                                    if this.settings == Some(SettingsTab::Model) {
                                        this.ensure_catalog(cx);
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(Icon::new(IconName::Settings).text_size(px(15.0))),
                    )
                    .child(
                        div()
                            .cursor_pointer()
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .text_color(rgb(TEXT_MUTED))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                                    this.settings = Some(SettingsTab::About);
                                    cx.notify();
                                }),
                            )
                            .child(Icon::new(IconName::Info).text_size(px(15.0))),
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
        // 思考强度：和模式并排。它同样是"这一轮怎么干"的选择，
        // 而不是模型本身的属性
        let thinking_picker = {
            let cur = self.thinking;
            let view = cx.entity().downgrade();
            Button::new("thinking-effort")
                .ghost()
                .icon(IconName::Brain)
                .label(cur.label())
                .dropdown_caret(true)
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(220.0));
                    for t in zex_core::ThinkingEffort::all() {
                        let view = view.clone();
                        let selected = t == cur;
                        menu = menu.item(
                            PopupMenuItem::element(move |_w, _c| {
                                let mut row = div()
                                    .flex()
                                    .items_start()
                                    .gap_3()
                                    .w_full()
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .gap(px(2.0))
                                            .child(div().text_sm().child(t.label()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(TEXT_DIM))
                                                    .child(t.description()),
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
                                    this.thinking = t;
                                    cx.notify();
                                })
                                .ok();
                            }),
                        );
                    }
                    menu
                })
        };

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
                                    this.push_mode_to_kernel(m, cx);
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
                            // 图片/文件粘贴统一走这里：键盘 Ctrl+V 与
                            // 右键菜单的 Paste 是两条路径，只有这个钩子
                            // 能同时覆盖（输入框自己只认文本）
                            .on_paste({
                                let weak = cx.entity().downgrade();
                                move |item, _window, cx| {
                                    weak.update(cx, |this, cx| this.take_paste(item, cx))
                                        .unwrap_or(false)
                                }
                            })
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
                        .child(mode_picker)
                        .child(thinking_picker),
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
                // 设置页优先级高于"打断任务"：用户是在浏览，不是想取消干活
                if this.settings.is_some() {
                    this.settings = None;
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
                    .flex_col();
                if let Some(tab) = self.settings {
                    // 设置页盖住主区：它是个"离开对话"的动作，
                    // 和消息流并排没意义
                    main = main.child(self.render_settings(tab, cx));
                } else {
                    main = main.child(match self.mode {
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
/// base64 解码（只处理我们自己编码出来的：标准字母表、无换行）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let s = s.trim().trim_end_matches('=');
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// 按 MIME 类型定图片格式。认不出来按 PNG 处理。
fn image_format_of(media_type: &str) -> gpui::ImageFormat {
    match media_type {
        "image/jpeg" | "image/jpg" => gpui::ImageFormat::Jpeg,
        "image/gif" => gpui::ImageFormat::Gif,
        "image/webp" => gpui::ImageFormat::Webp,
        "image/bmp" => gpui::ImageFormat::Bmp,
        _ => gpui::ImageFormat::Png,
    }
}

/// 给图片算稳定 id，供 GPUI 做渲染缓存。
///
/// 同一张图必须给同一个 id、不同图必须不同 —— 用内容哈希天然满足。
fn image_id(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

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
            Message::User { content } => {
                // 历史里的图片也要回放：只留文字的话，
                // 回看时完全不知道自己当时发了什么图
                let images: Vec<std::sync::Arc<gpui::Image>> = content
                    .iter()
                    .filter_map(|p| match p {
                        zex_core::Part::Image(img) => {
                            let bytes = base64_decode(&img.blob_id)?;
                            Some(std::sync::Arc::new(gpui::Image {
                                id: image_id(&bytes),
                                format: image_format_of(&img.media_type),
                                bytes,
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                out.push(MsgItem {
                    kind: MsgKind::User { images },
                    text: m.text(),
                });
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

/// 应用目录之外，界面上的图片只有两个来源：
///
/// - `gpui-kit` 内置的 Lucide 图标库（`icons/*.svg`）；
/// - 剪贴板缩略图（`clip-thumbs/*`），落在 `~/.zex/state/clip-thumbs`。
///
/// 为什么要自己写一个：`gpui_kit::assets::AllAssets` 是一个编译进二进制的
/// 静态目录，`image()` 能画的只有里面那些图标。而剪贴板图片是运行期才出现的
/// 文件，必须先有一个能从磁盘读的资产源，脚本里的
/// `image("clip-thumbs/xxx.png")` 才有东西可取。
///
/// 前缀映射是这一个 AssetSource 的全部职责，所以校验只做一件必要的事：
/// 名字里不许出现路径分隔或 `..`，免得脚本借它读到缩略图目录以外的文件。
struct ZexAssets;

impl AssetSource for ZexAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(name) = path.strip_prefix("clip-thumbs/") {
            let plain = !name.is_empty()
                && !name.contains('/')
                && !name.contains('\\')
                && !name.contains("..")
                && !name.starts_with('.');
            if plain {
                let file = zex_extension::clip_image::dir().join(name);
                match std::fs::read(&file) {
                    Ok(bytes) => return Ok(Some(std::borrow::Cow::Owned(bytes))),
                    // 文件不在就是"没有这个资产"，交给下面的内置库再试一次：
                    // 返回 Err 会让整帧渲染失败，而一张缩略图被清理掉
                    // 不该有这个后果。
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(path = %file.display(), error = %e, "读取剪贴板缩略图失败");
                    }
                }
            }
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        gpui_kit::assets::AllAssets.list(path)
    }
}

fn main() {
    // 两层：一层给人看的终端输出（受 RUST_LOG 控制），
    // 一层给模型看的扩展日志缓冲（不受 RUST_LOG 控制）。
    //
    // 顺序很关键：捕获层装在过滤器**前面**。脚本的 console.log 是 info 级，
    // 默认 `warn` 的过滤器会把它挡掉；而"点按钮没反应"往往正是靠这几行
    // 才查得出来。缓冲自己有上限（200 条）和 target 白名单，不会失控。
    {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;

        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::registry()
            .with(zex_app::ext_logs::CaptureLayer)
            .with(tracing_subscriber::fmt::layer().with_filter(filter))
            .init();
    }

    let cfg = match zex_app::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            std::process::exit(1);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // MCP 工具必须在 agent 构造前连好：工具集是构造时定下的，
    // 之后再挂就得重建 agent（会丢当前会话状态）。
    let mcp_file = zex_app::read_file_config().unwrap_or_default();
    let (mcp_tools, mcp_status, _mcp_hub) = zex_app::connect_mcp_servers(&mcp_file);
    if !mcp_status.is_empty() {
        let ok = mcp_status.iter().filter(|s| s.ok).count();
        tracing::info!(total = mcp_status.len(), ok, "MCP 服务已连接");
    }

    let agent = match zex_app::build_agent_resuming_with(&cfg, cwd.clone(), mcp_tools) {
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
    let handles = zex_app::spawn_core(agent, cfg.all_providers.clone(), cfg.provider_name.clone());
    let cmd_tx = handles.cmd_tx.clone();
    let mut event_rx = handles.event_rx;

    gpui_kit::application().with_assets(ZexAssets).run(move |cx| {
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

                // 测试钩子：ZEX_AUTO_SWITCH=<mode>:<毫秒> —— 延迟切一次运行模式，
                // 走的是下拉菜单**同一条**路径（含"把挂着的审批按新模式结掉"）。
                //
                // 为什么需要它：这个 bug 的形态是"任务跑一半时切模式不生效"，
                // 而 Wayland 下没法注入鼠标去点下拉。不给一个确定性入口，
                // 这条路径就只能靠人肉点，回归了也发现不了。
                if let Ok(spec) = std::env::var("ZEX_AUTO_SWITCH") {
                    let (mode_txt, delay) = match spec.split_once(':') {
                        Some((m, d)) => (m.to_string(), d.parse::<u64>().unwrap_or(8000)),
                        None => (spec.clone(), 8000),
                    };
                    let mode = match mode_txt.as_str() {
                        "auto_edit" => zex_core::RunMode::AutoEdit,
                        "plan" => zex_core::RunMode::Plan,
                        "full_access" => zex_core::RunMode::FullAccess,
                        _ => zex_core::RunMode::Confirm,
                    };
                    let weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(delay))
                            .await;
                        let _ = weak.update(cx, |this, cx| {
                            this.run_mode = mode;
                            let sid = zex_core::SessionId::from(this.active_session.clone());
                            let _ = this.store.set_session_mode(&sid, mode);
                            this.push_mode_to_kernel(mode, cx);
                            cx.notify();
                            eprintln!("ZEX_AUTO_SWITCH 已切到 {}", mode.label());
                        });
                    })
                    .detach();
                }

                // 测试钩子：ZEX_AUTO_VIEW=chat 启动后进对话视图。
                //
                // 自动任务那条路不走 `submit()`，所以视图会停在「工作台」——
                // 想截「任务完成」那句话就必须能进对话视图，否则永远截到
                // 工作台的待办列表，看起来像是提示没生效。
                if std::env::var("ZEX_AUTO_VIEW").as_deref() == Ok("chat") {
                    let weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(700))
                            .await;
                        let _ = weak.update(cx, |this, cx| {
                            this.mode = ViewMode::Chat;
                            cx.notify();
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
                            let mode = std::env::var("ZEX_AUTO_MODE").ok().map(|m| match m.as_str() {
                                "auto_edit" => zex_core::RunMode::AutoEdit,
                                "plan" => zex_core::RunMode::Plan,
                                "full_access" => zex_core::RunMode::FullAccess,
                                _ => zex_core::RunMode::Confirm,
                            });
                            let _ = tx.try_send(UiCommand::UserPrompt {
                                session: zex_core::SessionId::from(session),
                                text: prompt,
                                attachments: vec![],
                                model: None,
                                mode,
                                thinking: None,
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


