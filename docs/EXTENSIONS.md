# 扩展（界面上叫「应用」）

一个扩展就是一段跑在 zex 进程内的 JavaScript，能画自己的界面（挂在右侧面板的一个标签里），
也能调 agent。它不需要重新编译，也不需要重新分发二进制。

**界面上管它叫「应用」**：说"写个应用"比"写个扩展"更像用户想干的事。
代码、路径、工具名里仍是 `extension` / `ext_*` —— 改名会牵动插件清单格式和
所有已装目录，收益不够，所以保留内部叫法，只改用户看到的那一层。

```text
~/.zex/extensions/git-status/
├── gpui-shell.json     # id / name / entry
├── main.js
└── icon.svg            # 可选，侧边栏显示的那个图标
```

把 `examples/extensions/demo` 整个目录拷到 `~/.zex/extensions/` 就能看到它：

```bash
cp -r examples/extensions/demo ~/.zex/extensions/
```

然后在「设置 → 应用」里点「打开」。

## 数据：localStorage 与 SQLite

两套都用，按数据形状选：

| 用途 | 用什么 | 落盘 |
|---|---|---|
| 几个设置项 | `localStorage`（全局共享一份） | `~/.zex/state/extensions-store.json` |
| 多行记录、要按条件查 | `dbExec` / `dbQuery` / `dbBatch` | `~/.zex/state/db/<id>.db` |

**SQLite 是一个应用一个库文件。** 这条隔离是宿主做的，不是靠 SQL 过滤：脚本
只能传 SQL 和参数，路径永远由宿主按应用 id 拼，所以它打不开 `~/.zex/zex.db`
（那里面有全部会话、审计记录和授权表）。

连接上装了作者器，`ATTACH` 和 `PRAGMA` 一律 `not authorized` —— 这是唯一的
"跨库"通道，也是"改上限把磁盘写满"的入口，所以从底层挡，而不是解析 SQL 猜意图。

限额：库 64 MiB（`max_page_count`）、单次查询 5000 行。库连接按应用缓存，
首次用到才建文件 —— 不用数据库的应用不会凭空多出个 `.db`。

实现见 `crates/zex-extension/src/store_db.rs`（纯 SQLite 逻辑，12 个测试）
和 `host.rs`（接到 `HostModule`）。

## 网络：为什么自己做，而不是打开上游能力

上游 gpui-shell **已经实现了** `fetch` 和 `WebSocket`，甚至还有按 scheme /
method / path 限定的细粒度授权。看起来只要 `set_capabilities(network_hosts(...))`
就完事了 —— 但它的能力集是**进程级**的：`set_capabilities` 改的是全局默认
Policy，而 zex 挂应用走 `load_application`，所有应用共用同一份。

也就是说一旦全局放开 `network_hosts`，A 应用声明的域名 B 应用照样能连，
能力申请页那句"按应用授予"就成了摆设。**存储可以接受共享**（风险低、
坏了就是脏数据），**网络不行**：它能把用户硬盘上的东西发出去。

所以 `crates/zex-extension/src/net.rs` 自己实现三个通道，白名单按应用从
各自清单里取 —— 我们的宿主模块本来就是 `zex.<id>` 一应用一个，天生知道
"这次是谁在调"。

| 通道 | 实现 | 说明 |
|---|---|---|
| HTTP | `reqwest` | 不跟随重定向（否则 302 就是白名单旁路），响应体 4 MiB 上限 |
| WebSocket | `tokio-tungstenite` | `wsConnect/wsSend/wsRecv/wsClose` |
| TCP | `tokio::net` | 客户端 only，`tcpConnect/tcpSend/tcpRecv/tcpClose` |

每个应用一个独立的 tokio runtime：宿主模块的回调跑在 gpui 的执行器上，
那里没有 reactor，直接 `TcpStream::connect` 会 panic（"no reactor running"）。

这里踩过一次：一开始 `NetHub` 只是**存着**一个 runtime，但方法体直接
`await`，于是 `tokio::spawn` / `TcpStream::connect` 用的还是环境里的运行时
（gpui 的），结果炸出 `Task polled after completion`。单元测试没发现，
因为测试外面套了一层 `block_on` —— 环境里刚好有运行时。
现在所有网络方法都是薄壳：`self.runtime.spawn(async move { ... })`，
真活儿全在 `*_on(inner, ...)` 里跑。

**闸门是"清单 ∩ 授权"**。清单（`capabilities.network.hosts`）只是申请：
模型能写清单，所以它自己说"我要连哪儿"不算数。真正放行的是用户确认后存进
`config.json` 的 `network_grants`，生效的是两者的交集 —— 清单偷偷加一条不会
扩权，用户撤销则立刻失效。

### 三态，靠"键在不在"区分

| `network_grants[id]` | 意思 | 界面 |
|---|---|---|
| 键不存在 | 还没问过 | 打开应用时弹确认卡 |
| `[]` | 明确拒绝过 | 不弹了；设置页可改成允许 |
| `["api.x.com"]` | 允许这几条 | 生效中；清单后来又加主机时会重新问 |

### 两层各管什么

脚本里两条路都通，但门在两层上关：

| 通道 | 代码 | 认的白名单 |
|---|---|---|
| `fetch` / `WebSocket`（上游内建） | gpui-shell | 挂载时装进去的 `Capabilities` |
| `httpRequest` / `wsConnect` / `tcpConnect`（我们加的） | `net.rs` | 每次调用现算的 `NetPolicy` |

上游那层只认精确主机名（`network_hosts` 不认通配、端口另走
`HttpRequestGrant`），所以 `upstream_grants()` 把交集翻译成它的语言：精确
主机直给，带端口的转成 `HttpRequestGrant`，`*.` 与 `*` 翻译不过去 —— 那两条
只留给自己的 `httpRequest`。宁可让 `fetch` 连不上，也不能为了"能用"把 `*`
当成放行一切。

### 能力集怎么"按应用"

上游的 `set_capabilities` 改的是**默认** Policy，而 `mount_application` 建视图
时读的就是这份默认 —— 视图建好后自己持有那一份（`ScriptView::with_policy`）。
所以挂载时"装 → 加载 → 挂载 → 立刻换回基础集"就够：这份通行证只属于这个
应用，换回是为了别让下一个应用白捡上一份授权。同一线程、挂载即返回，
中间没有别的应用在跑。

边界（说清楚，不假装安全）：白名单按主机名判，DNS 返回私有地址也照连
（应用连本机开发服务器是正常需求）；只能做客户端，不能监听。

实现 18 个测试，含 HTTP / TCP / WS 三个**真实往返**（本地起服务，
不是 mock）。

## 图标

`icon.svg` 出现在侧边栏和设置页的应用列表里；没有就退回一个小灰点。

它是**按文件名认的**，清单里不写 —— 上游的 `gpui-shell.json` 开了
`deny_unknown_fields`，加一个 `icon` 键会让整份清单解析失败：那样 zex 能显示图标，
`gpui-shell dev` 却跑不起来。约定文件名对上游完全透明。

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
     stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
  <circle cx="12" cy="12" r="9"/>
  <path d="M12 7v5l3 2"/>
</svg>
```

三条约束：

- **单色**。`Window::paint_svg` 只取 alpha 遮罩，颜色来自 `text_color`，
  所以图标里的 `fill` 写什么都不作数 —— 层次只能靠形状和 `fill-opacity`。
- **`viewBox 0 0 24 24`**，不写 `width`/`height`：实际按 14px（侧栏）和 28px（设置页）缩放。
- **线要够粗**。14px 下 1 个用户单位约 0.58px，`stroke-width="1"` 会发虚，
  和界面上其他 Lucide 图标一样用 2。

扫描时就把字节读进 `Extension::icon`，不是每帧读盘：侧边栏一行一个图标，
每帧 open/read 一遍太亏。读不出来就当没有 —— 图标是装饰，缺了不该让应用装不上。

## 让 agent 来写

zex 的定位不只是运行扩展，还包括**按你的要求现场写一个**。这条闭环需要的四件东西都在了：

| 环节 | 靠什么 | 状态 |
|---|---|---|
| 生成 | 内置技能 `write-extension`（`~/.zex/skills/`）+ 应用目录里的 `gpui-kit.d.ts` | ✅ |
| 验证 | `ext_check` 工具：真求值一遍脚本，返回脚本原文错误（不渲染、不开窗口） | ✅ |
| 登记 | `ext_install` 工具：补清单 + 重扫；`ext_uninstall` 删目录（L3，逐次审批） | ✅ |
| 观察 | `ext_logs` 工具：`console.*`、回调异常、未处理拒绝，带文件与行号 | ✅ |
| 迭代 | 改完 1.2 秒内自动重载（轮询 mtime） | ✅ |

用法就是一句话：

> 给我做一个能显示今天完成任务的扩展

agent 会写 `main.js` → `ext_install` 登记 → `ext_check` 自检 → 报给你。写错了它自己看得见错误（含 `main.js:12:44` 这样的位置），改完再验。

### 为什么 `ext_check` 是关键

导出的名字对不对、模块解析得通不通，光看代码判断不了。`ext_check` 让模型**真的求值一遍**，所以「`v_flex` 到底属于哪个模块」这种问题在写的时候就暴露了，而不是等你点开面板看到白屏。

它**不渲染**，所以你拿不到元素树。这不是偷懒，是被平台堵死的：

- `WindowOptions.show` 在 gpui 的 X11/Wayland 后端**根本没被读取** —— 设成 `false` 也照样弹出一个真的空窗口；
- `window.remove_window()` 只是把窗口从 `cx.windows` 里摘掉，X 那边的窗口一直留着（`xdotool search --onlyvisible` 数得到），也就是永久挂在屏幕上。

两条都实测过，结论是「校验时顺手开个窗口画一帧」在 Linux 上做不了。要确认布局/坐标，只能有人点开面板 —— 这也符合「由用户发起」：不该因为一句工具调用就替用户开窗。

### 能力申请：看得见，但不生效

模型能写扩展，也就能写清单。所以清单里的 `capabilities` 会被解析并显示在扩展页：

```
申请：读 1 处 · 写 1 处 · 执行任意命令 · 网络 1 个主机
宽授权：可执行任意命令、整个家目录：${homeDir}、任意网络主机
· 未审阅
```

**这一版真正生效的是 `storage` 和 `network`**（后者按应用各自授权）：

| 能力 | 状态 |
|---|---|
| `storage` → `localStorage` | ✅ 生效，落盘在 `~/.zex/state/extensions-store.json`（全局共享一份） |
| `network` → `fetch` / `httpRequest` / WS / TCP | ✅ 生效，按应用：清单 ∩ 用户允许（见上节） |
| `fs` / `clipboard` / `execute` | ❌ 只解析、展示、留痕 |

存储之所以能给，是因为它不需要"按应用隔离才有意义"：宿主建运行时调 `set_storage_path` + `set_capabilities(storage)`，两行成对就有——少任一行脚本一碰 `localStorage` 就抛错，报错还看不出另一边没配。

**代价是它被所有应用共享**：存储仍走全局默认 Policy（网络已经按应用分开了，
见上节）。所以键空间共享（8 MiB / 1024 键 / 单值 1 MiB 也是全局算的）、
没申请存储的应用一样能用。要每应用一份文件得换成上游 `plugin::Plugin::load`，
那是一整套插件生命周期，先不值当。

`fs` / `clipboard` / `execute` 仍然只是「申请 → 展示 → 留痕」：它们比网络更
难按应用切（一个路径授权要落到 `Grant` 句柄上，剪贴板根本没法按应用隔离），
等 per-app 策略铺开再逐个接。`config.json` 里还留着 `approved_extensions`
（早期"审阅过"的痕迹），网络早已改由 `network_grants` 判定。

## 侧边栏入口

侧边栏底部常驻一行「扩展」，点它进扩展设置页。想让某个扩展直接露在侧边栏上，
在设置页点那一行的 `固定到边栏`：

```
（会话列表）
─────────────
🧩 扩展          1 个
• 示例扩展        ← 点它直接开面板
─────────────
⚙  ⓘ
```

固定的结果写进 `~/.zex/config.json`：

```json
{ "pinned_extensions": ["demo"] }
```

存的是 id，不是完整清单 —— 扩展叫什么、装在哪，都以目录扫描为准。
所以扩展被卸掉之后，配置里残留的 id 会被安静忽略，不会在侧边栏留一个死入口。

没固定任何扩展时这一行也在。它是这个功能唯一的常驻入口，
藏起来的话用户永远找不到第二次。

## 清单

`gpui-shell.json` 复用上游 [gpui-shell](https://gpui-kit.com/shell/) 的格式，
不自造一套 —— 这样同一个目录既能被 zex 挂载，也能用 `gpui-shell dev` 单独跑起来调试。

```json
{
  "id": "demo",
  "name": "示例扩展",
  "entry": "main.js"
}
```

`entry` 必须是目录内的相对路径（`../` 会被拒）。缺省是 `main.js`。
上游的 `version` / `capabilities` / `dependencies` 原样保留，zex 不读它们。

## 示例扩展的自检面板

`examples/extensions/demo` 不是"最小示例"，是一张**自检表**：每行一个能力，
右边是此刻的真实返回值。哪一环断了，哪一行就是空的或灰的。

| 行 | 右边显示 | 断在这里说明 |
|---|---|---|
| 脚本渲染 | `这一屏就是` | 面板空白 —— 脚本没跑起来，看面板上的错误原文 |
| 本地状态 | `N 次` | 灰点表示还没点过；点了不动是 `cx.notify()` 没生效 |
| 宿主 `name()` | 扩展显示名 | 宿主模块没注册上（多半是模块名和 id 不一致） |
| 宿主 `busy()` | `空闲` / `等待回包` | 桥没连到内核 |
| agent 回包 | 模型返回的文本 | 内核跑了但回包没回来（查 `TurnEnded` 的会话号） |

两个按钮的分工是刻意的，它们测的东西完全不同：

| 按钮 | 碰不碰宿主 | 验证的是 |
|---|---|---|
| `本地 +1（不碰宿主）` | 不碰 | 脚本能持有状态、`cx.notify()` 能触发重绘 |
| `问 agent（走内核）` | 全程贯穿 | 脚本 → 宿主模块 → 桥 → 内核 → 模型 → 事件回流 → 重绘 |

第一个是纯前端的事，第二个是整条链路。分开摆是为了**故障定位**：
如果 `+1` 能加而回复出不来，问题在内核那一侧，不用去查渲染。

## 宿主接口

模块名是 **`zex.<扩展 id>`**：

```js
import { ask, name, reply, busy, session } from "zex.demo";
```

| 函数 | 说明 |
|---|---|
| `ask(prompt)` | 向 agent 提问，返回 `Promise<string>` |
| `reply()` | 最近一次回复的文本，**同步可读** |
| `busy()` | 是否有 `ask()` 还在等回包 |
| `name()` | 本扩展的显示名 |
| `session()` | 本扩展的会话 id（`ext-<id>`） |

模块名带 id 不是美化问题：宿主模块注册表**按名唯一**，所有扩展共用一个 `zex`
的话，后挂的会顶掉先挂的，第一个扩展的 `ask` 就会发进第二个的会话。

### cx 只属于产出它的那一次调用

这条规矩踩一次头就疼一次，所以单独说：

```js
// 错：render 的 cx 不属于点击那次调用
render(cx) {
  return Button.new("x").on_click(() => {
    this.n += 1;
    cx.notify();     // TypeError: cx is no longer valid
  });
}

// 对：回调自己的第二个参数才是这次的 cx
render(cx) {
  return Button.new("x").on_click((_event, clickCx) => {
    this.n += 1;
    clickCx.notify();
  });
}
```

跨 `await` 的 `cx` 同样失效。所以拿回包推荐不 await、下一帧读 `reply()`：

```js
// 推荐：一问一答，不碰 cx
ask("解释这段代码").catch((e) => { this.lastError = String(e); });
// render 里：reply() || "（还没问过）"
```

宿主在回包到达时会主动重跑 `render`，脚本只要同步读取就行。

注意这个失效是**静默的失败**：`this.n` 确实加了，只是界面不重绘。
所以自检面板里那行 `本地状态` 点了不动，就说明 `notify()` 没生效 ——
错误只写进 `tracing`（默认 `warn` 级别以上会打印），界面本身不会弹提示。

## 会话

每个扩展有自己的会话 `ext-<id>`，历史持久化在 `~/.zex/zex.db`。
它**不出现在侧栏** —— 扩展自己有界面，不必再占一行。

同一扩展的并发 `ask` 会串行（内核按会话加锁）；后来的会取代先前的，
被取代的那次立刻收到一个明确的失败，而不是永远挂着。

## 能力：默认全拒

扩展的沙箱**默认什么都不给**：没有文件、没有网络、没有剪贴板、没有子进程。

```rust
gpui_shell::set_capabilities(Capabilities::new())   // 空集
```

要读文件、发 HTTP 这类事，走宿主模块（也就是我们主动开放、能审的那几个函数），
而不是给脚本开权限。加能力之前先问：这个能力真需要给脚本吗，还是宿主代劳更好。

除此之外沙箱还砍了 `eval`、冻结了内置原型、限制堆 256 MiB / 单次调用 50 ms。
一个扩展抛异常不会带走 zex：界面停在上一帧，错误显示在面板里。

## 三点约束

**不套 ShellRoot。** 上游的对话框/toast 要求挂在 `ShellRoot` 下，而那会接管整个窗口
的焦点恢复和 Tab 导航 —— 和 zex 自己的 Esc 中断、Ctrl+R 历史搜索、输入框回车都会打架。
所以扩展只能画内联界面。真需要弹窗时再解决，代价是要重新验证那几个快捷键。

**debug 版慢 3 倍。** 根 `Cargo.toml` 里的 `[profile.dev.package]` 必须保留：

```toml
[profile.dev.package.quickjs-jit-sys]
opt-level = 3
[profile.dev.package.quickjs-jit-core]
opt-level = 3
```

不优化这两个包，debug 构建的脚本渲染是 31.5 ms（release 是 11.0 ms）。

**体积。** 链接 QuickJS 是 +13.5 MiB 二进制、+14 MiB 常驻内存。
`zex-extension` 的 `ui` feature 关掉就不链接它，扩展页只剩目录列表。

## 上游还是实验期

gpui-shell 自述处于 M0：「脚本 API 预计会变」。所以：

- 依赖钉死 `=0.9.1`，升级是一次显式动作，不是 `cargo update` 的副作用；
- 集成代码全部关在 `crates/zex-extension` 里，`bin/zex.rs` 只碰它的公开接口。

升级到 0.10 需要连带升 `gpui-kit`（0.9 → 0.10），届时 `ScriptView` 的挂载方式
会变简单（上游公开了 `instantiate` / `ScriptView::new`），可以顺手把 per-plugin
`Policy` 用起来，把模块名里的 id 去掉。

## 排查

脚本报错进 `tracing`，target 是 `gpui_kit::shell::script`：

```bash
RUST_LOG=gpui_kit::shell::script=debug zex
```

面板里显示「扩展未能加载」时，错误原文就在面板上，包含脚本自己的栈。
目录结构问题（清单缺失、JSON 语法错、`entry` 逃出目录）在扫描阶段就会被
`tracing::warn` 报出来，并在扩展页里跳过这一个（其余照常显示）。
