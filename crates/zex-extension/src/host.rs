//! gpui-shell 集成：建运行时、注册宿主模块、挂载脚本 View。
//!
//! # 三步
//!
//! ```text
//! ExtensionHost::new(cx, bridge)   建一个 VM（全应用共用一个）
//!   .mount(&ext, window, cx)       注册 zex.<id> 模块 → 加载脚本 → Entity<ScriptView>
//! ```
//!
//! 挂出来的东西就是一个普通 GPUI 元素，塞进哪个容器都行 —— 这一点是
//! 整个方案成立的前提（zex 的右侧面板是自绘的标签容器，不是上游的 dock）。
//!
//! # 为什么全应用只用一个 VM
//!
//! 一个 VM 就是 +13.5 MiB 二进制、+14 MiB 常驻内存。每个扩展各起一个
//! 会把这两笔开销乘上去，而扩展之间本来也不需要隔离到这个程度：
//! gpui-shell 自己按插件分发 `Policy`，一个插件崩了不外溢。
//!
//! # 为什么模块名带扩展 id
//!
//! 宿主模块注册表是**按模块名唯一**的（`modules.insert(name, module)`），
//! 同名后注册的会顶掉先注册的。所以不能让所有扩展共用一个 `zex` 模块 ——
//! 那样第二个扩展一挂上，第一个的 `ask` 就会把问题发进第二个的会话。
//!
//! 于是模块名是 `zex.<id>`，脚本里写：
//!
//! ```js
//! import { ask } from "zex.demo";   // demo 是这个扩展的 id
//! ```
//!
//! 代价是脚本和 id 绑定（改 id 要同步改 import）。换来的是每个扩展的
//! 调用天然落到自己的会话上，不需要在运行时猜"这次是谁在问"。
//!
//! # 不套 ShellRoot
//!
//! 上游的对话框 / toast 要求 View 挂在 `ShellRoot` 下，而那会接管整个
//! 窗口的焦点恢复和 Tab 导航 —— 和 zex 自己的 Esc 中断、Ctrl+R 历史搜索、
//! 输入框回车都会打架。所以这里不套：扩展只能画内联界面。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gpui_kit::*;
use gpui_shell::{Capabilities, HostError, HostModule, HostValue, ScriptView, ShellRuntime};
use zex_core::Result;

use crate::bridge::AgentBridge;
use crate::catalog::Extension;
use crate::store_db::{self, JsValue};

/// 扩展宿主：一个 VM 加一套宿主模块。
pub struct ExtensionHost {
    runtime: Rc<ShellRuntime>,
    bridge: AgentBridge,
    /// 会话 id → 最后一次回复。
    ///
    /// 为什么宿主也要存一份：脚本 await 之后拿 `cx.notify()` 会撞上
    /// "cx 属于那一次调用"的约束（跨 await 的 cx 是失效的）。
    /// 让宿主把结果存下来、由宿主触发重绘，脚本在 render 里同步读取 ——
    /// 这条路不需要跨 await 的 cx。
    replies: Arc<Mutex<HashMap<String, String>>>,
    /// 应用 id → 已打开的库连接。
    ///
    /// 缓存而不是每次现开：打开 SQLite 要读文件头、跑一遍 pragma，
    /// 而一次渲染里脚本可能连着查好几次。连接带 `Mutex` 是因为
    /// `rusqlite::Connection` 不是 `Sync`，而宿主模块的回调可能并发。
    dbs: DbRegistry,
    /// 应用 id → 网络世界（白名单 + 连接 + 独立运行时）。
    ///
    /// 按应用分开而不是共用：白名单来自各自的清单，共用就等于
    /// 把 A 申请的主机递给 B。
    nets: NetRegistry,
    /// 用户对每个应用的网络授权：应用 id → 允许的主机。
    ///
    /// 共享一份 `Arc`：用户点「允许」之后，正在跑的应用**立刻**能联网，
    /// 不用重启、也不用重新挂载（每次取 hub 时现算交集）。
    grants: Arc<Mutex<std::collections::BTreeMap<String, Vec<String>>>>,
    /// 用户对每个应用的**剪贴板授权**：应用 id → 是否允许。
    ///
    /// 与网络分开一张表：两者的授予理由不同（一个防外发，一个防偷看），
    /// 合成一张表就会出现"允许联网顺带给了剪贴板"这种谁也没想过的结果。
    /// 一个布尔值就够：清单里申请读还是写由 `CapabilityRequest` 记着，
    /// 这里是"这个人准不准这个应用碰剪贴板"。
    clip_grants: Arc<Mutex<std::collections::BTreeMap<String, bool>>>,
}

/// 已打开的库。
type DbRegistry = Arc<Mutex<HashMap<String, Arc<Mutex<rusqlite::Connection>>>>>;

/// 应用 id → 网络世界。
type NetRegistry = Arc<Mutex<HashMap<String, Arc<crate::net::NetHub>>>>;

impl ExtensionHost {
    /// 建运行时。
    ///
    /// 能力集里只开 `storage`：不给文件、不给网络、不给剪贴板、不给子进程。
    /// 画界面不需要这些，要读文件的事由宿主模块代劳（那才是我们能审的地方）。
    ///
    /// # `set_storage_path` 和 `set_capabilities` 必须成对
    ///
    /// 这两行少一个都会在脚本第一次碰 `localStorage` 时抛错，而且报错
    /// 看不出来"另一边没配"：
    ///
    /// - 只给能力：`localStorage has no backing file; the host must call
    ///   set_storage_path before the application runs`
    /// - 只给路径：`storage is not granted; set capabilities.storage to true`
    ///
    /// 顺带一个坑：`set_storage_path` **不建目录**。父目录不存在时它不报错，
    /// 写盘时才 warn —— 于是脚本能用、重启就丢，而且看起来一切正常。
    /// 所以这里自己 `create_dir_all`，失败也只 warn（存储用不了不该让
    /// 整个扩展宿主起不来）。
    pub fn new(cx: &mut App, bridge: AgentBridge) -> Result<Self> {
        gpui_shell::init(cx);
        let store = crate::catalog::storage_path();
        if let Some(dir) = store.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "无法创建扩展存储目录，本地存储将在进程退出后丢失"
                );
            }
        }
        gpui_shell::set_storage_path(store);
        gpui_shell::set_capabilities(base_capabilities());

        // 剪贴板监视线程：整个进程一个。它不碰 GPUI，所以放在这里启动
        // 也不会和 App 的借用打架；重复调用是空操作。
        crate::clip_watch::start();

        let runtime = ShellRuntime::new(cx)
            .map_err(|e| zex_core::Error::Other(format!("无法创建扩展运行时: {e}")))?;

        Ok(Self {
            runtime,
            bridge,
            replies: Arc::new(Mutex::new(HashMap::new())),
            dbs: Arc::new(Mutex::new(HashMap::new())),
            nets: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            clip_grants: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        })
    }

    /// 这个应用这次挂载时的能力集：存储照旧，网络按「清单 ∩ 授权」。
    ///
    /// 为什么要在这里算，而不是挂载时读全局：gpui-shell 的 `fetch` /
    /// `WebSocket` 读的是**建视图那一刻**的默认策略，视图建好后自己持有
    /// 那一份。所以把交集装成默认、挂载、再换回来，就等于给这个应用发了
    /// 一张只属于它的通行证 —— 别的应用拿不到（它们各自挂载时各算各的）。
    fn capabilities_for(&self, ext: &Extension) -> Capabilities {
        let mut caps = base_capabilities();

        // 剪贴板：清单申请 ∩ 用户授权，且读、写分开判。
        // 上游把它做成脚本里的 `cx.read_from_clipboard()` /
        // `cx.write_to_clipboard(text)`（同步，只能从 host call 里调），
        // 读写各有各的能力位，所以这里能精确到"准它写、不准它读"。
        let clip_allowed = self
            .clip_grants
            .lock()
            .ok()
            .and_then(|g| g.get(&ext.id).copied())
            .unwrap_or(false);
        if clip_allowed {
            caps = caps
                .clipboard_read(ext.request.clipboard_read)
                .clipboard_write(ext.request.clipboard_write);
        }

        let declared = &ext.request.network_hosts;
        if declared.is_empty() {
            return caps;
        }
        let granted = self
            .grants
            .lock()
            .ok()
            .and_then(|g| g.get(&ext.id).cloned());
        let Some(granted) = granted else {
            // 还没问过用户：一条都不给，脚本会拿到"没被授予"的拒绝
            return caps;
        };
        let effective = crate::net::intersect(declared, &granted);
        let (hosts, ports, unsupported) = crate::net::upstream_grants(&effective);
        if !unsupported.is_empty() {
            tracing::debug!(
                app = %ext.id,
                entries = ?unsupported,
                "通配主机不能翻译成上游能力，只有 httpRequest 能用它们"
            );
        }
        let mut requests = Vec::new();
        for (host, port) in ports {
            // 上游按 scheme 分别判端口，http 和 https 各发一张
            for scheme in ["http", "https"] {
                requests.push(
                    gpui_shell::HttpRequestGrant::new(
                        host.clone(),
                        ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
                        Vec::<String>::new(),
                        // 空前缀 = 任何路径
                        vec![String::new()],
                    )
                    .scheme(scheme)
                    .port(port),
                );
            }
        }
        caps = caps.network_hosts(hosts).http_requests(requests);
        caps
    }

    /// 把"每个应用被授权访问哪些主机"交给宿主。
    ///
    /// 不传也行：那样所有应用的网络调用都会被拒，并提示去确认。
    /// UI 启动时、以及每次用户点「允许 / 撤销」之后各同步一次。
    pub fn set_grants(&self, grants: std::collections::BTreeMap<String, Vec<String>>) {
        if let Ok(mut guard) = self.grants.lock() {
            *guard = grants;
        }
    }

    /// 把"每个应用准不准碰剪贴板"交给宿主。
    ///
    /// 与 `set_grants` 分开传：网络是"允许连哪些主机"，剪贴板是"准不准碰"，
    /// 一个列表一个布尔，硬塞进同一个类型只会让两边都变难读。
    /// 不传等于全拒 —— 授权表没到之前，任何应用都读不到剪贴板。
    pub fn set_clipboard_grants(&self, grants: std::collections::BTreeMap<String, bool>) {
        if let Ok(mut guard) = self.clip_grants.lock() {
            *guard = grants;
        }
    }

    /// 到内核的桥（UI 侧要在 `TurnEnded` 时回调它）。
    pub fn bridge(&self) -> &AgentBridge {
        &self.bridge
    }

    /// 加载并挂载一个扩展。
    ///
    /// 失败时返回带脚本自身栈的错误，调用方把这句话显示在面板里就行 ——
    /// 一个写坏的扩展不该让宿主 panic。
    pub fn mount(
        &self,
        ext: &Extension,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<ScriptView>> {
        // 必须先注册模块再加载脚本：导入名集合在 import 那一刻就定死了，
        // 之后再注册对已求值的模块无效
        self.install_module(ext)?;

        // 能力集临时装成"这个应用的"，挂完立刻换回基础集。
        // 换回是必须的：默认策略是**下一位**挂载者的起点，留着上一个
        // 应用的网络授权，后面那个应用白捡一份。
        let caps = self.capabilities_for(ext);
        gpui_shell::set_capabilities(caps);
        let mounted = match self.runtime.load_application(&ext.dir, &ext.entry) {
            Ok(app) => {
                let view = self.runtime.mount_application(&app, window, cx);
                view.map_err(|e| zex_core::Error::Other(format!("挂载失败: {e}")))
            }
            Err(e) => Err(zex_core::Error::Other(format!("加载失败: {e}"))),
        };
        gpui_shell::set_capabilities(base_capabilities());

        mounted
    }

    /// 记下某个会话的最后一次回复，并刷新对应脚本 View。
    ///
    /// 由 UI 线程在 `TurnEnded` 时调用。刷新是必需的：脚本的 render
    /// 只在"描述过期"时重跑，宿主改了脚本会读的状态就得明说一次。
    pub fn deliver(
        &self,
        session: &str,
        text: &str,
        view: &mut gpui::Entity<ScriptView>,
        cx: &mut gpui::App,
    ) {
        {
            let mut held = match self.replies.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            held.insert(session.to_string(), text.to_string());
        }
        view.update(cx, |v, cx| v.refresh(cx));
    }

    /// 某个会话的最后一次回复。
    pub fn reply_for(&self, session: &str) -> String {
        match self.replies.lock() {
            Ok(g) => g.get(session).cloned().unwrap_or_default(),
            Err(poisoned) => poisoned
                .into_inner()
                .get(session)
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// 只求值、不渲染的校验。**不需要窗口。**
    ///
    /// 覆盖脚本写得对不对这件事里最常见的一类：模块解析、导入名、默认导出。
    /// 例如 `import { v_flex } from "gpui-kit"` 会在这里就报
    /// `Could not find export 'v_flex'`，而不是等用户点开面板看到白屏。
    ///
    /// 不做的事：不跑 `render`，也不物化元素树 —— 那两步必须有窗口，
    /// 而 Linux 后端的 `WindowOptions.show` 没有实现，开窗口就是真的开窗口。
    pub fn validate_script(&self, ext: &Extension) -> std::result::Result<(), String> {
        if let Err(e) = self.install_module(ext) {
            return Err(e.to_string());
        }
        // 拿到就丢：只要「能不能求值成功」这个结论。留着反而会让后续
        // mount 撞上「已挂载」。
        self.runtime
            .load_application(&ext.dir, &ext.entry)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 校验一个扩展能否加载并物化出第一帧。
    ///
    /// **必须传一个隐藏窗口的 `Window`。** 上游的 `check` 内部会
    /// `replace_root` 再 `draw` 一帧，用完不还原 —— 拿主窗口去调
    /// 会把用户界面换成一棵探针树。
    pub fn check(
        &self,
        ext: &Extension,
        window: &mut Window,
        cx: &mut App,
    ) -> crate::check::CheckReport {
        // 先把 `zex.<id>` 模块装好再求值。
        //
        // 少了这一步，任何 `import { ask } from "zex.demo"` 的扩展都会
        // 被误判成 "cannot resolve module" —— 错误看起来像扩展写坏了，
        // 实际是校验环境缺了一块。校验要和真实加载走同一套准备。
        if let Err(e) = self.install_module(ext) {
            return crate::check::CheckReport::failed(e.to_string());
        }
        match self.runtime.check(&ext.dir, window, cx) {
            Ok(tree) => crate::check::CheckReport::ok(tree),
            Err(e) => crate::check::CheckReport::failed(e.to_string()),
        }
    }

    /// 给这个扩展注册 `zex.<id>` 模块。
    fn install_module(&self, ext: &Extension) -> Result<()> {
        let module = zex_module(
            ext,
            self.bridge.clone(),
            self.replies.clone(),
            self.dbs.clone(),
            self.nets.clone(),
            self.grants.clone(),
            self.clip_grants.clone(),
        );
        module
            .validate()
            .map_err(|e| zex_core::Error::Other(format!("宿主模块声明与实现不一致: {e}")))?;
        gpui_shell::export_module(module)
            .map_err(|e| zex_core::Error::Other(format!("无法注册宿主模块: {e}")))
    }
}

/// 脚本里 import 的模块名。
pub fn module_name(ext_id: &str) -> String {
    format!("zex.{ext_id}")
}

/// 剪贴板的哪一半。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipHalf {
    Read,
    Write,
}

/// 剪贴板权限没给时，把话说清楚 —— 用户要做的是"去点一下允许"，
/// 不是去读代码。
///
/// 判据和 `capabilities_for` 用的是同一张表：用户点过允许，这里就通；
/// 没点过或拒绝过，这里就拒。图片读写走的是同一份授权，不额外开口子。
fn ensure_clipboard(
    grants: &Mutex<std::collections::BTreeMap<String, bool>>,
    app: &str,
    half: ClipHalf,
) -> std::result::Result<(), HostError> {
    let allowed = grants
        .lock()
        .map_err(|_| HostError::from("剪贴板授权表中毒"))?
        .get(app)
        .copied()
        .unwrap_or(false);

    if allowed {
        return Ok(());
    }
    let which = match half {
        ClipHalf::Read => "读取",
        ClipHalf::Write => "写入",
    };
    Err(HostError::from(format!(
        "应用 `{app}` 没有剪贴板{which}权限。请在它的面板上点「允许剪贴板」，\
         或在「设置 → 应用」里允许"
    )))
}

/// 给脚本用的宿主模块。
///
/// 边界是**纯数据**：进出都只有 null/布尔/数字/字符串/数组/对象，
/// 不传任何脚本句柄，也不传 `Entity`。这不是洁癖 —— 句柄一旦被宿主留住，
/// 就可能在下一次脚本调用时还活着，而它所属的调用栈早就没了。
fn zex_module(
    ext: &Extension,
    bridge: AgentBridge,
    replies: Arc<Mutex<HashMap<String, String>>>,
    dbs: DbRegistry,
    nets: NetRegistry,
    grants: Arc<Mutex<std::collections::BTreeMap<String, Vec<String>>>>,
    clip_grants: Arc<Mutex<std::collections::BTreeMap<String, bool>>>,
) -> HostModule {
    // 这个应用的网络世界。**建的时候白名单是空的**：真正生效的那份在
    // 每次取 hub 时按「清单申请 ∩ 用户授权」现算（见 `net_hub`）——
    // 用户在面板上点「允许」之后，下一次调用就该通，不用重开。
    let net = crate::net::NetHub::new(crate::net::NetPolicy::default())
        .ok()
        .map(std::sync::Arc::new);
    if net.is_none() {
        tracing::warn!(app = %ext.id, "无法为应用建立网络运行时，网络功能不可用");
    }
    // 每个闭包各拿一份：`HostModule` 的闭包是 `Fn`，共享一个 moved 值是拿不到的
    let net_handle = net;
    // 清单里申请的主机（只是申请，见 `net_hub`）
    let declared = ext.request.network_hosts.clone();
    let app = ext.id.clone();
    let ask_bridge = bridge.clone();
    let reply_map = replies;
    let reply_session = ext.session_id();
    let busy_bridge = bridge.clone();
    let busy_session = ext.session_id();
    // 会话在注册时就固定下来：这个模块只属于这一个扩展，
    // 谁调它都只可能代表这一个扩展说话
    let ask_session = ext.session_id();
    let id_session = ask_session.clone();
    let title = ext.name.clone();

    HostModule::new(module_name(&ext.id))
        .declarations(format!(
            "/** {title} 的宿主接口。 */\n\
             /** 向 agent 提一个问题，等这一轮说完再返回。 */\n\
             export function ask(prompt: string): Promise<string>;\n\
             /** 本扩展的会话 id。 */\n\
             export function session(): string;\n\
             /** 本扩展的显示名。 */\n\
             export function name(): string;\n\
             /** 最近一次 ask() 的回复（可同步读）。 */\n\
             export function reply(): string;\n\
             /** 是否有 ask() 还在等回包。 */\n\
             /**\n\
              * 发一个 HTTP 请求。只支持 http/https，不自动跟随重定向。\n\
              * 域名必须在清单的 capabilities.network.hosts 里，且经用户批准。\n\
              */\n\
             export function httpRequest(spec: {{\n\
               url: string;\n\
               method?: string;\n\
               headers?: [string, string][] | Record<string, string>;\n\
               body?: string;\n\
               timeout_ms?: number;\n\
             }}): Promise<{{\n\
               status: number;\n\
               ok: boolean;\n\
               headers: [string, string][];\n\
               body: string;\n\
               truncated: boolean;\n\
             }}>;\n\
             /** 连一个 TCP 端口，返回连接编号。 */\n\
             export function tcpConnect(host: string, port: number): Promise<string>;\n\
             /** 往 TCP 连接写字符串，返回写入字节数。 */\n\
             export function tcpSend(id: string, data: string): Promise<number>;\n\
             /** 读一条：{{ type, data, closed }}。超时会 reject。 */\n\
             export function tcpRecv(\n\
               id: string,\n\
               timeout_ms?: number,\n\
             ): Promise<{{ type: string; data: string; closed: boolean }}>;\n\
             /** 关闭 TCP 连接。 */\n\
             export function tcpClose(id: string): Promise<boolean>;\n\
             /** 连一个 WebSocket（ws/wss），返回连接编号。 */\n\
             export function wsConnect(url: string): Promise<string>;\n\
             /** 发一条文本。 */\n\
             export function wsSend(id: string, text: string): Promise<void>;\n\
             /** 收一条：{{ type, data, closed }}，type 为 text/binary/close。 */\n\
             export function wsRecv(\n\
               id: string,\n\
               timeout_ms?: number,\n\
             ): Promise<{{ type: string; data: string; closed: boolean }}>;\n\
             /** 关闭 WebSocket。 */\n\
             export function wsClose(id: string): Promise<boolean>;\n\
             export function busy(): boolean;\n\
             /**\n\
              * 执行一条写语句（INSERT / UPDATE / DELETE / DDL）。\n\
              * 返回受影响行数与最后插入的 rowid。\n\
              */\n\
             export function dbExec(\n\
               sql: string,\n\
               params?: unknown[],\n\
             ): Promise<{{ changes: number; lastInsertId: number }}>;\n\
             /**\n\
              * 查询，返回按列名组织的对象数组。一次最多 5000 行。\n\
              */\n\
             export function dbQuery(\n\
               sql: string,\n\
               params?: unknown[],\n\
             ): Promise<Array<Record<string, unknown>>>;\n\
             /**\n\
              * 同一个事务里跑多条写语句；任一条失败整体回滚。\n\
              */\n\
             export function dbBatch(\n\
               statements: Array<{{ sql: string; params?: unknown[] }}>,\n\
             ): Promise<Array<{{ changes: number; lastInsertId: number }}>>;\n\
             /**\n\
              * 剪贴板里如果有**图片**，把它存成本机缩略文件并返回句柄；没有就是 null。\n\
              * 返回的 path 可以直接交给 image() 画出来。\n\
              */\n\
             export function captureClipboardImage(): {{\n\
               path: string;\n\
               width: number;\n\
               height: number;\n\
               bytes: number;\n\
             }} | null;\n\
             /** 把 captureClipboardImage 存下的图放回剪贴板。 */\n\
             export function putClipboardImage(path: string): boolean;\n\
             /**\n\
              * 取走后台线程捕获到的全部剪贴板变化（取一次就清空）。\n\
              * 窗口不可见时主线程不跑定时器，靠这个接口才不会漏掉中间的复制。\n\
              */\n\
             export function drainClipboard(): Array<{{\n\
               kind: string;\n\
               body: string;\n\
               width: number;\n\
               height: number;\n\
               atMs: number;\n\
             }}>;\n\
             /** 后台监视线程是否在跑（只有 Linux/Wayland 会起）。 */\n\
             export function clipboardWatching(): boolean;\n",
        ))
        .async_function("ask", move |args| {
            let prompt = args.string(0)?.to_string();
            // 同步半边：只抓走要用的东西，不 await（主线程上不该等网络）
            let session = ask_session.clone();
            let bridge = ask_bridge.clone();
            // 异步半边：Send，会在主线程之外被 poll
            Ok(async move {
                let reply = bridge.ask(&session, prompt).await;
                match reply.error {
                    None => Ok(HostValue::from(reply.text)),
                    Some(e) => Err(gpui_shell::HostError::from(e)),
                }
            })
        })
        // ---- 剪贴板图片 ----
        //
        // 这两件事脚本自己做不到：`cx.read_from_clipboard()` 只回答文本
        // （图片条目被 `text()` 跳过），`image()` 又只认资产根内的相对路径。
        // 所以由宿主在中间搭一座桥：图存成资产根下的文件，脚本拿到的就是
        // 一个能直接画出来的路径。详见 `clip_image` 模块头。
        //
        // 权限沿用剪贴板的两个布尔位：读图要 read，写回要 write，
        // 与用户已经点过的那一次授权是同一份判断。
        .function("captureClipboardImage", {
            let (grants, app_id) = (clip_grants.clone(), app.clone());
            move |_| {
                ensure_clipboard(grants.as_ref(), &app_id, ClipHalf::Read)?;
                let item = gpui_shell::with_current_app(|app| app.read_from_clipboard()).flatten();
                let Some(shot) = crate::clip_image::capture(item) else {
                    return Ok(HostValue::Null);
                };
                Ok(HostValue::Object(vec![
                    ("path".into(), HostValue::from(shot.rel_path)),
                    ("width".into(), HostValue::Number(f64::from(shot.width))),
                    ("height".into(), HostValue::Number(f64::from(shot.height))),
                    ("bytes".into(), HostValue::Number(shot.bytes as f64)),
                ]))
            }
        })
        // 放回剪贴板。
        //
        // **不走 GPUI 的 `write_to_clipboard`**：它在 Wayland 下只 offer 文本
        // MIME 类型，图片会被写成空字符串（界面上"已复制"，粘出来什么都没有）。
        // 这里交给 `clip_watch` 自己的服务线程，它直接和 compositor 打交道。
        // 详见 `clip_watch::put` 的注释。
        .function("putClipboardImage", {
            let (grants, app_id) = (clip_grants.clone(), app.clone());
            move |args| {
                ensure_clipboard(grants.as_ref(), &app_id, ClipHalf::Write)?;
                let path = args.string(0)?.to_string();
                let Some(bytes) = crate::clip_image::bytes_for(&path) else {
                    return Ok(HostValue::Bool(false));
                };
                let mime = crate::clip_image::format_of_name(&path)
                    .mime_type()
                    .to_string();

                // 同时给一份文本：有些程序（编辑器、聊天框）只认 text/plain，
                // 没有它就当作"剪贴板里没有可粘贴的东西"。
                let sources = vec![
                    (mime, bytes),
                    ("text/plain".to_string(), "\u{fffc}".as_bytes().to_vec()),
                ];
                Ok(HostValue::Bool(crate::clip_watch::put(
                    crate::clip_watch::PutContent::Bytes(sources),
                )))
            }
        })
        // 后台线程捕获到的变化。这里是"取走"语义：一次调用把队列清空，
        // 脚本负责处理每一条 —— 它才知道哪些已经记过。
        .function("drainClipboard", move |_| {
            let items = crate::clip_watch::drain();
            let rows: Vec<HostValue> = items
                .into_iter()
                .map(|e| {
                    HostValue::Object(vec![
                        ("kind".into(), HostValue::from(e.kind)),
                        ("body".into(), HostValue::from(e.body)),
                        ("width".into(), HostValue::Number(f64::from(e.width))),
                        ("height".into(), HostValue::Number(f64::from(e.height))),
                        // 捕获时刻：脚本用它排序，而不是自己取当前时间
                        ("atMs".into(), HostValue::Number(e.at_ms as f64)),
                    ])
                })
                .collect();
            Ok(HostValue::Array(rows))
        })
        .function("clipboardWatching", |_| {
            Ok(HostValue::Bool(crate::clip_watch::watching()))
        })
        .function("session", move |_| Ok(HostValue::from(id_session.clone())))
        .function("name", {
            let t = ext.name.clone();
            move |_| Ok(HostValue::from(t.clone()))
        })
        .function("reply", move |_| {
            let text = match reply_map.lock() {
                Ok(g) => g.get(&reply_session).cloned().unwrap_or_default(),
                Err(p) => p
                    .into_inner()
                    .get(&reply_session)
                    .cloned()
                    .unwrap_or_default(),
            };
            Ok(HostValue::from(text))
        })
        .function("busy", move |_| {
            Ok(HostValue::from(busy_bridge.is_waiting(&busy_session)))
        })
        .async_function("httpRequest", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let spec = http_spec(args.get(0))?;
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let out = hub.http(spec).await.map_err(HostError::from)?;
                    Ok(HostValue::Object(vec![
                        ("status".into(), HostValue::Number(f64::from(out.status))),
                        ("ok".into(), HostValue::Bool(out.is_ok())),
                        ("headers".into(), header_pairs(out.headers)),
                        ("body".into(), HostValue::from(out.body)),
                        ("truncated".into(), HostValue::Bool(out.truncated)),
                    ]))
                })
            }
        })
        .async_function("tcpConnect", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let host = args.string(0)?.to_string();
                let port = args.integer(1)? as u16;
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let id = hub
                        .tcp_connect(&host, port)
                        .await
                        .map_err(HostError::from)?;
                    Ok(HostValue::from(id))
                })
            }
        })
        .async_function("tcpSend", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let data = args.string(1)?.to_string();
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let n = hub.tcp_send(&id, &data).await.map_err(HostError::from)?;
                    Ok(HostValue::Number(n as f64))
                })
            }
        })
        .async_function("tcpRecv", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let wait = args.get(1).and_then(|v| v.as_number()).map(|n| n as u64);
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let r = hub.tcp_recv(&id, wait).await.map_err(HostError::from)?;
                    Ok(recv_value(r))
                })
            }
        })
        .async_function("tcpClose", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move { Ok(HostValue::Bool(hub.tcp_close(&id).await)) })
            }
        })
        .async_function("wsConnect", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let url = args.string(0)?.to_string();
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let id = hub.ws_connect(&url).await.map_err(HostError::from)?;
                    Ok(HostValue::from(id))
                })
            }
        })
        .async_function("wsSend", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let text = args.string(1)?.to_string();
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    hub.ws_send(&id, &text).await.map_err(HostError::from)?;
                    Ok(HostValue::Null)
                })
            }
        })
        .async_function("wsRecv", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let wait = args.get(1).and_then(|v| v.as_number()).map(|n| n as u64);
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move {
                    let r = hub.ws_recv(&id, wait).await.map_err(HostError::from)?;
                    Ok(recv_value(r))
                })
            }
        })
        .async_function("wsClose", {
            let (grants, declared, app, registry, ready) = (
                grants.clone(),
                declared.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let id = args.string(0)?.to_string();
                let hub = net_hub(&registry, &app, &declared, &grants, ready.clone())?;
                Ok(async move { Ok(HostValue::Bool(hub.ws_close(&id).await)) })
            }
        })
        .async_function("dbExec", {
            let dbs = dbs.clone();
            let id = ext.id.clone();
            move |args| {
                let sql = args.string(0)?.to_string();
                let params = sql_params(args.get(1))?;
                let conn = open_db(&dbs, &id)?;
                Ok(async move {
                    let guard = lock_conn(&conn)?;
                    let r = store_db::exec(&guard, &sql, &params).map_err(HostError::from)?;
                    Ok(exec_result(r))
                })
            }
        })
        .async_function("dbQuery", {
            let dbs = dbs.clone();
            let id = ext.id.clone();
            move |args| {
                let sql = args.string(0)?.to_string();
                let params = sql_params(args.get(1))?;
                let conn = open_db(&dbs, &id)?;
                Ok(async move {
                    let guard = lock_conn(&conn)?;
                    let rows = store_db::query(&guard, &sql, &params).map_err(HostError::from)?;
                    Ok(HostValue::Array(
                        rows.into_iter()
                            .map(|record| {
                                HostValue::Object(
                                    record
                                        .into_iter()
                                        .map(|(k, v)| (k, host_value(v)))
                                        .collect(),
                                )
                            })
                            .collect(),
                    ))
                })
            }
        })
        .async_function("dbBatch", {
            let dbs = dbs.clone();
            let id = ext.id.clone();
            move |args| {
                let items = batch_items(args.get(0))?;
                let conn = open_db(&dbs, &id)?;
                Ok(async move {
                    let mut guard = lock_conn(&conn)?;
                    let out = store_db::batch(&mut guard, &items).map_err(HostError::from)?;
                    Ok(HostValue::Array(out.into_iter().map(exec_result).collect()))
                })
            }
        })
}

/// 宿主给所有应用的基础能力：只有本地存储。
///
/// 网络不在这里：它必须按应用分别授予，见 `capabilities_for`。
fn base_capabilities() -> Capabilities {
    Capabilities::new().storage(true)
}

/// 网络闸门 + 取 hub：把「清单申请」和「用户授权」合成当前白名单。
///
/// 三种状态各有各的说法，别让用户/模型猜：
/// - **没问过**（配置里没有这个应用）→ 拒绝，并告诉他去面板上确认；
/// - **拒绝过**（授权是空列表）→ 拒绝，并说"你拒绝过，可在设置里改"；
/// - **授权过** → 只放行「清单 ∩ 授权」，清单偷偷加大也不会自动生效。
fn net_hub(
    nets: &NetRegistry,
    app: &str,
    declared: &[String],
    grants: &Arc<Mutex<std::collections::BTreeMap<String, Vec<String>>>>,
    ready: Option<Arc<crate::net::NetHub>>,
) -> std::result::Result<Arc<crate::net::NetHub>, HostError> {
    if declared.is_empty() {
        return Err(HostError::from(format!(
            "应用 `{app}` 没有申请任何网络主机：清单里 capabilities.network.hosts 是空的"
        )));
    }
    let granted = grants
        .lock()
        .map_err(|_| HostError::from("网络授权表中毒"))?
        .get(app)
        .cloned();

    let effective = match granted {
        None => {
            return Err(HostError::from(format!(
                "应用 `{app}` 申请访问网络（申请：{}），但还没得到确认。请在它的面板上点「允许」",
                declared.join("、")
            )));
        }
        Some(list) if list.is_empty() => {
            return Err(HostError::from(format!(
                "你拒绝过应用 `{app}` 的网络访问（申请：{}）。要改的话在「设置 → 应用」里允许",
                declared.join("、")
            )));
        }
        Some(list) => crate::net::intersect(declared, &list),
    };

    if effective.is_empty() {
        return Err(HostError::from(format!(
            "应用 `{app}` 申请的主机（{}）没有一条被授权；以清单为准，授权只减不增",
            declared.join("、")
        )));
    }

    let policy = crate::net::NetPolicy::parse(&effective)
        .map_err(|e| HostError::from(format!("白名单写法有误：{e}")))?;

    let mut map = nets.lock().map_err(|_| HostError::from("网络表中毒"))?;
    let hub = match map.get(app) {
        Some(hub) => hub.clone(),
        None => {
            let Some(hub) = ready else {
                return Err(HostError::from("这个应用没有可用的网络运行时"));
            };
            map.insert(app.to_string(), hub.clone());
            hub
        }
    };
    // 每次调用都刷新一次：用户刚点过「允许」的话，这一通就该通
    hub.set_policy(policy);
    Ok(hub)
}

/// `httpRequest({...})` 的参数。
fn http_spec(value: Option<&HostValue>) -> std::result::Result<crate::net::HttpRequest, HostError> {
    let Some(HostValue::Object(entries)) = value else {
        return Err(HostError::from("httpRequest 需要一个对象参数"));
    };
    let mut spec = crate::net::HttpRequest {
        method: "GET".into(),
        url: String::new(),
        headers: Vec::new(),
        body: None,
        timeout_ms: None,
    };
    for (k, v) in entries {
        match k.as_str() {
            "url" => spec.url = v.as_str().unwrap_or_default().to_string(),
            "method" => {
                spec.method = v.as_str().unwrap_or("GET").to_string();
            }
            "body" => spec.body = v.as_str().map(str::to_string),
            "timeout_ms" => spec.timeout_ms = v.as_number().map(|n| n as u64),
            "headers" => {
                if let HostValue::Array(items) = v {
                    // 两种写法都收：[["k","v"]] 和 [{k,v}]
                    for item in items {
                        if let HostValue::Array(pair) = item {
                            if let (Some(k), Some(val)) = (
                                pair.first().and_then(|x| x.as_str()),
                                pair.get(1).and_then(|x| x.as_str()),
                            ) {
                                spec.headers.push((k.to_string(), val.to_string()));
                            }
                        }
                    }
                } else if let HostValue::Object(pairs) = v {
                    for (k, val) in pairs {
                        if let Some(s) = val.as_str() {
                            spec.headers.push((k.clone(), s.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if spec.url.trim().is_empty() {
        return Err(HostError::from("httpRequest 缺少 url"));
    }
    Ok(spec)
}

fn header_pairs(headers: Vec<(String, String)>) -> HostValue {
    HostValue::Array(
        headers
            .into_iter()
            .map(|(k, v)| HostValue::Array(vec![HostValue::from(k), HostValue::from(v)]))
            .collect(),
    )
}

fn recv_value(r: crate::net::Recv) -> HostValue {
    HostValue::Object(vec![
        ("type".into(), HostValue::from(r.kind)),
        ("data".into(), HostValue::from(r.data)),
        ("closed".into(), HostValue::Bool(r.closed)),
    ])
}

/// 取（必要时打开）某个应用的库。
///
/// 这里只接受应用 id，路径由 [`Extension::db_file`] 拼 —— 脚本给不出
/// 第二个参数，也就没有"打开别的文件"这条路。
fn open_db(
    dbs: &DbRegistry,
    id: &str,
) -> std::result::Result<Arc<Mutex<rusqlite::Connection>>, HostError> {
    let mut map = dbs.lock().map_err(|_| HostError::from("库连接表中毒"))?;
    if let Some(conn) = map.get(id) {
        return Ok(conn.clone());
    }
    // 目录在 ExtensionHost::new 里已经建过；这里失败只有一个原因：
    // 磁盘/权限出问题，值得原样报给脚本
    let conn = store_db::open(&state_db_path(id))
        .map_err(|e| HostError::from(format!("无法打开本应用的库: {e}")))?;
    let shared = Arc::new(Mutex::new(conn));
    map.insert(id.to_string(), shared.clone());
    Ok(shared)
}

/// 库文件路径。和 [`Extension::db_file`] 同一来源，但这里没有 `Extension`。
fn state_db_path(id: &str) -> std::path::PathBuf {
    crate::store_db::db_path(&crate::catalog::state_dir(), id)
}

fn lock_conn(
    conn: &Arc<Mutex<rusqlite::Connection>>,
) -> std::result::Result<std::sync::MutexGuard<'_, rusqlite::Connection>, HostError> {
    conn.lock().map_err(|_| HostError::from("库连接已损坏"))
}

fn exec_result(r: store_db::ExecResult) -> HostValue {
    HostValue::Object(vec![
        ("changes".to_string(), HostValue::Number(r.changes as f64)),
        (
            "lastInsertId".to_string(),
            HostValue::Number(r.last_insert_id as f64),
        ),
    ])
}

/// 参数数组 → 内部表示。
///
/// 第二个参数可以省略，也可以给 `null`/`undefined`。
fn sql_params(value: Option<&HostValue>) -> std::result::Result<Vec<JsValue>, HostError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        HostValue::Null => Ok(Vec::new()),
        HostValue::Array(items) => Ok(items.iter().map(to_js_value).collect()),
        other => Err(HostError::from(format!(
            "params 必须是数组，收到的是 {}",
            other.describe()
        ))),
    }
}

fn to_js_value(v: &HostValue) -> JsValue {
    match v {
        HostValue::Null => JsValue::Null,
        HostValue::Bool(b) => JsValue::Bool(*b),
        HostValue::Number(n) => JsValue::Number(*n),
        HostValue::Str(s) => JsValue::Str(s.clone()),
        // 数组和对象存成 JSON 文本：SQLite 没有嵌套类型，而"存一坨结构化
        // 数据"是脚本最常干的事，让它自己 JSON.stringify 一遍太啰嗦
        HostValue::Array(_) | HostValue::Object(_) => JsValue::Json(to_json(v).to_string()),
    }
}

fn to_json(v: &HostValue) -> serde_json::Value {
    match v {
        HostValue::Null => serde_json::Value::Null,
        HostValue::Bool(b) => serde_json::Value::Bool(*b),
        HostValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        HostValue::Str(s) => serde_json::Value::String(s.clone()),
        HostValue::Array(items) => serde_json::Value::Array(items.iter().map(to_json).collect()),
        HostValue::Object(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect(),
        ),
    }
}

fn host_value(v: JsValue) -> HostValue {
    match v {
        JsValue::Null => HostValue::Null,
        JsValue::Bool(b) => HostValue::Bool(b),
        JsValue::Number(n) => HostValue::Number(n),
        JsValue::Str(s) => HostValue::Str(s),
        JsValue::Json(s) => HostValue::Str(s),
    }
}

/// `dbBatch` 的参数：`[{sql, params?}, ...]`。
fn batch_items(
    value: Option<&HostValue>,
) -> std::result::Result<Vec<(String, Vec<JsValue>)>, HostError> {
    let Some(HostValue::Array(items)) = value else {
        return Err(HostError::from("dbBatch 需要语句数组"));
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let HostValue::Object(entries) = item else {
            return Err(HostError::from(format!("第 {} 条不是对象", i + 1)));
        };
        let mut sql = None;
        let mut params = Vec::new();
        for (k, v) in entries {
            match k.as_str() {
                "sql" => {
                    sql = v.as_str().map(str::to_string);
                }
                "params" => params = sql_params(Some(v))?,
                _ => {}
            }
        }
        let Some(sql) = sql else {
            return Err(HostError::from(format!("第 {} 条缺少 sql", i + 1)));
        };
        out.push((sql, params));
    }
    Ok(out)
}
