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
    /// 用户批准过哪些应用（能力申请页上那个「批准申请」）。
    ///
    /// 共享一份 `Arc`：用户在设置页点批准之后，正在跑的应用**立刻**能联网，
    /// 不用重启、也不用重新挂载。
    approvals: Arc<Mutex<std::collections::HashSet<String>>>,
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
        gpui_shell::set_capabilities(Capabilities::new().storage(true));

        let runtime = ShellRuntime::new(cx)
            .map_err(|e| zex_core::Error::Other(format!("无法创建扩展运行时: {e}")))?;

        Ok(Self {
            runtime,
            bridge,
            replies: Arc::new(Mutex::new(HashMap::new())),
            dbs: Arc::new(Mutex::new(HashMap::new())),
            nets: Arc::new(Mutex::new(HashMap::new())),
            approvals: Arc::new(Mutex::new(std::collections::HashSet::new())),
        })
    }

    /// 把"用户批准了哪些应用"交给宿主。
    ///
    /// 不传也行：那样所有应用都算未批准，网络调用会被拒并提示去批准。
    /// UI 启动时和每次点「批准申请」之后各同步一次。
    pub fn set_approvals(&self, approved: std::collections::HashSet<String>) {
        if let Ok(mut guard) = self.approvals.lock() {
            *guard = approved;
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

        let app = self
            .runtime
            .load_application(&ext.dir, &ext.entry)
            .map_err(|e| zex_core::Error::Other(format!("加载失败: {e}")))?;
        self.runtime
            .mount_application(&app, window, cx)
            .map_err(|e| zex_core::Error::Other(format!("挂载失败: {e}")))
    }

    /// 记下某个会话的最后一次回复，并刷新对应脚本 View。
    ///
    /// 由 UI 线程在 `TurnEnded` 时调用。刷新是必需的：脚本的 render
    /// 只在"描述过期"时重跑，宿主改了脚本会读的状态就得明说一次。
    pub fn deliver(&self, session: &str, text: &str, view: &mut gpui::Entity<ScriptView>, cx: &mut gpui::App) {
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
            Err(poisoned) => poisoned.into_inner().get(session).cloned().unwrap_or_default(),
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
            self.approvals.clone(),
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
    approvals: Arc<Mutex<std::collections::HashSet<String>>>,
) -> HostModule {
    // 这个应用的网络世界：白名单来自它自己的清单。
    // 清单写错了（比如 `*.` 这种）不该让模块注册失败——那会让应用
    // 连界面都打不开，而错误其实只在联网时才该出现。所以退化成一个
    // 空白名单：任何连接都会被拒，并说明原因。
    let net = match crate::net::NetPolicy::parse(&ext.request.network_hosts) {
        Ok(policy) => crate::net::NetHub::new(policy).ok().map(Arc::new),
        Err(e) => {
            tracing::warn!(app = %ext.id, error = %e, "网络白名单写法有误，按空名单处理");
            None
        }
    };
    // 每个闭包各拿一份：`HostModule` 的闭包是 `Fn`，共享一个 moved 值是拿不到的
    let net_handle = net;
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
             ): Promise<Array<{{ changes: number; lastInsertId: number }}>>;\n",
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
        .function("session", move |_| Ok(HostValue::from(id_session.clone())))
        .function("name", {
            let t = ext.name.clone();
            move |_| Ok(HostValue::from(t.clone()))
        })
        .function("reply", move |_| {
            let text = match reply_map.lock() {
                Ok(g) => g.get(&reply_session).cloned().unwrap_or_default(),
                Err(p) => p.into_inner().get(&reply_session).cloned().unwrap_or_default(),
            };
            Ok(HostValue::from(text))
        })
        .function("busy", move |_| {
            Ok(HostValue::from(busy_bridge.is_waiting(&busy_session)))
        })
        .async_function("httpRequest", {
            let (approved, app, registry, ready) = (
                approvals.clone(),
                app.clone(),
                nets.clone(),
                net_handle.clone(),
            );
            move |args| {
                let spec = http_spec(args.get(0))?;
                gate(&approved, &app)?;
                let hub = net_hub(&registry, &app, ready.clone())?;
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
            let (approved, app, registry, ready) =
                (approvals.clone(), app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let host = args.string(0)?.to_string();
                let port = args.integer(1)? as u16;
                gate(&approved, &app)?;
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    let id = hub.tcp_connect(&host, port).await.map_err(HostError::from)?;
                    Ok(HostValue::from(id))
                })
            }
        })
        .async_function("tcpSend", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let data = args.string(1)?.to_string();
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    let n = hub.tcp_send(&id, &data).await.map_err(HostError::from)?;
                    Ok(HostValue::Number(n as f64))
                })
            }
        })
        .async_function("tcpRecv", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let wait = args.get(1).and_then(|v| v.as_number()).map(|n| n as u64);
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    let r = hub.tcp_recv(&id, wait).await.map_err(HostError::from)?;
                    Ok(recv_value(r))
                })
            }
        })
        .async_function("tcpClose", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move { Ok(HostValue::Bool(hub.tcp_close(&id).await)) })
            }
        })
        .async_function("wsConnect", {
            let (approved, app, registry, ready) =
                (approvals.clone(), app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let url = args.string(0)?.to_string();
                gate(&approved, &app)?;
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    let id = hub.ws_connect(&url).await.map_err(HostError::from)?;
                    Ok(HostValue::from(id))
                })
            }
        })
        .async_function("wsSend", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let text = args.string(1)?.to_string();
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    hub.ws_send(&id, &text).await.map_err(HostError::from)?;
                    Ok(HostValue::Null)
                })
            }
        })
        .async_function("wsRecv", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let wait = args.get(1).and_then(|v| v.as_number()).map(|n| n as u64);
                let hub = net_hub(&registry, &app, ready.clone())?;
                Ok(async move {
                    let r = hub.ws_recv(&id, wait).await.map_err(HostError::from)?;
                    Ok(recv_value(r))
                })
            }
        })
        .async_function("wsClose", {
            let (app, registry, ready) = (app.clone(), nets.clone(), net_handle.clone());
            move |args| {
                let id = args.string(0)?.to_string();
                let hub = net_hub(&registry, &app, ready.clone())?;
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
                    let out =
                        store_db::batch(&mut guard, &items).map_err(HostError::from)?;
                    Ok(HostValue::Array(out.into_iter().map(exec_result).collect()))
                })
            }
        })
}

/// 网络闸门：这个应用的能力申请被批准了吗。
///
/// 批准是**用户动作**，不是清单自说自话。模型能写清单，所以清单只能
/// 表达"我想要什么"；真正放行要人在设置页点一下。已经批准的集合放在
/// 共享表里，点完立刻生效，不用重启。
fn gate(
    approved: &Arc<Mutex<std::collections::HashSet<String>>>,
    app: &str,
) -> std::result::Result<(), HostError> {
    let ok = approved
        .lock()
        .map(|g| g.contains(app))
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(HostError::from(format!(
            "应用 `{app}` 的能力申请还没被批准，网络不可用。             请让用户在「设置 → 应用」里点这个应用的「批准申请」"
        )))
    }
}

/// 取（必要时建）某个应用的网络世界。
fn net_hub(
    nets: &NetRegistry,
    app: &str,
    ready: Option<Arc<crate::net::NetHub>>,
) -> std::result::Result<Arc<crate::net::NetHub>, HostError> {
    let mut map = nets
        .lock()
        .map_err(|_| HostError::from("网络表中毒"))?;
    if let Some(hub) = map.get(app) {
        return Ok(hub.clone());
    }
    let Some(hub) = ready else {
        return Err(HostError::from(
            "这个应用没有可用的网络（清单里的 network.hosts 写法有误，或运行时建不起来）",
        ));
    };
    map.insert(app.to_string(), hub.clone());
    Ok(hub)
}

/// `httpRequest({...})` 的参数。
fn http_spec(
    value: Option<&HostValue>,
) -> std::result::Result<crate::net::HttpRequest, HostError> {
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
fn open_db(dbs: &DbRegistry, id: &str) -> std::result::Result<Arc<Mutex<rusqlite::Connection>>, HostError> {
    let mut map = dbs
        .lock()
        .map_err(|_| HostError::from("库连接表中毒"))?;
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
        HostValue::Array(_) | HostValue::Object(_) => {
            JsValue::Json(to_json(v).to_string())
        }
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
fn batch_items(value: Option<&HostValue>) -> std::result::Result<Vec<(String, Vec<JsValue>)>, HostError> {
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
