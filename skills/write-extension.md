---
name: write-extension
description: 给 zex 写一个应用（脚本面板/独立小应用），含模块划分、宿主接口、本地存储与 SQLite、剪贴板、图标规范和三条必踩的坑
---

# 给 zex 写应用

一个应用就是一段跑在 zex 进程内的 JavaScript，能画自己的界面、能调 agent 和工具。
不需要编译，改完重新打开面板即可生效。

界面上管它叫「应用」，代码和路径里还叫扩展（`~/.zex/extensions`）—— 同一个东西。

## 目录

```
~/.zex/extensions/<id>/
├── gpui-shell.json     # 清单
├── main.js             # 入口
├── icon.svg            # 图标（见下，每次都要给）
└── gpui-kit.d.ts       # 自动生成，别手写、别提交
```

清单：

```json
{ "id": "git-status", "name": "Git 状态", "entry": "main.js" }
```

`id` 决定模块名（见下），也决定库文件名 —— 只能用字母、数字、`-`、`_`、`.`。
`entry` 必须是目录内的相对路径。

**数据不要写进这个目录。** 应用目录装的是代码（会被 git、被拷贝、被分享），
运行期数据归宿主管：设置项走 `localStorage`，多行记录走 SQLite，
落在 `~/.zex/state/` 下（见「本地存储」「SQLite」两节）。

## 先查 API，再写代码

`gpui-kit.d.ts` 是这个运行时**唯一准确的** API 清单（两万多行），
就在扩展目录里。写代码前先查，别凭印象：

```bash
grep -n "v_flex\|h_flex" ~/.zex/extensions/<id>/gpui-kit.d.ts
```

**模块划分是死规矩**，一个名字只属于一个模块：

| 模块 | 有什么 |
|---|---|
| `gpui-kit` | `View`、`div`、`svg`、`image`、`list`、样式表、`cx` 上的能力 |
| `gpui-base` | `v_flex`、`h_flex`、组件（`Button`、`Input`…）、主题色 |
| `zex.<id>` | 本扩展的宿主接口（见下） |

最常见的错：`import { v_flex } from "gpui-kit"` —— **`v_flex` 在 `gpui-base`**。

## 图标：每个应用都要有一个

`icon.svg` 会显示在左侧栏和设置页的应用列表里。**每次建应用都顺手写一个**，
否则那里只有一个灰点，用户分不清自己装了什么。

它是靠**文件名**认的，清单里不写 —— 上游清单 `deny_unknown_fields`，
多一个键整个清单一解析就废，所以图标走约定文件。

### 三条硬约束

1. **单色**。渲染器只取 alpha 遮罩，颜色由主题决定：写 `fill="#ff0000"`
   不会出来红色，出来的是当前主题的前景色。所以**别用颜色表达信息**，
   要靠形状、疏密、`fill-opacity` / `stroke-opacity` 区分层次。
2. **`viewBox` 固定 `0 0 24 24`**，不要写 `width` / `height`。
   实际显示 14px（侧栏）和 28px（设置页），靠缩放。
3. **线条要够粗**。14px 下 1 单位 ≈ 0.58px，`stroke-width="1"` 会虚。
   统一用 `stroke-width="2"`、`fill="none"`、`stroke-linecap="round"`，
   和界面上其他图标（Lucide 风格）保持一致。

### 可直接改的模板

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
     stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
  <circle cx="12" cy="12" r="9"/>
  <path d="M12 7v5l3 2"/>
</svg>
```

（这是「时钟」。换成你的应用要画的形状就行；`currentColor` 才是对的写法，
它表示"用主题色"，虽然渲染器其实不看这个值。）

### 别做的事

- 别内嵌位图（`<image>`）、别用滤镜/渐变/`<text>` —— resvg 能解析，
  但在 14px 的 alpha 遮罩上只会糊成一团；
- 别把整块 24x24 涂满：会变成一个实心方块，和旁边的小圆点没区别；
- 写坏了不会报错，只是那行回到灰点 —— 所以写完用 `ext_check` 顺带确认
  目录里有这个文件，或者直接问用户看到的是不是他想要的形状。

## 本地存储：能用，但要按约定用

`localStorage` **是真的可用的**，重启进程后数据还在。
（`sessionStorage` 也能用，但只在本次进程内，别拿它存要留下来的东西。）

```js
const KEY = "myapp.v1.records";        // ← 必须带前缀，见下

// 读
let rows = [];
try { rows = JSON.parse(localStorage.getItem(KEY) || "[]"); } catch { rows = []; }

// 写
localStorage.setItem(KEY, JSON.stringify(rows));
```

### 三条必须知道的事

1. **键空间是所有应用共享的。** 所有应用的 `localStorage` 落在同一个文件里
   （`~/.zex/state/extensions-store.json`）。所以键名一律加
   `<你的id>.v<数字>.` 前缀 —— 不撞车是你自己的责任，宿主不会加。
2. **额度是全局算的**：总 8 MiB、最多 1024 个键、单个值 1 MiB。够存打卡记录、
   任务清单、设置项；不够存图片或日志（那该走 `fs`，而 `fs` 目前没开放）。
3. **写盘是异步的**，你要是需要"确定落盘了"，`await localStorage.flush()`。
   不加也能用 —— 退出前宿主会收尾，只是没有立刻落盘的保证。

### 还要不要 try/catch

要。宿主当前是**全局**开存储，所以正常情况不会抛；但这个能力是按版本给的，
将来如果收紧成"按应用授予"，没申请的应用就会抛
`storage is not granted`。捕获住、退到内存里，界面至少还能用：

```js
try {
  localStorage.setItem(KEY, v);
} catch (e) {
  this.note = "仅在本次运行内有效";   // 照实写出来，别假装存上了
  this.memory = v;
}
```

清单里要不要声明 `capabilities.storage`？**声明**，用户能在应用页看到你要什么。
但别指望它当开关：现在生效与否由宿主决定，不是清单决定。

## SQLite：要存"多行数据"就用它

`localStorage` 适合几个设置项。**打卡记录、任务、消息、流水这类越攒越多的
东西，用 SQLite** —— 它在同一个文件里直接按条件查，不用整份读进来再筛。

```js
import { dbExec, dbQuery, dbBatch } from "zex.<id>";

// 建表（幂等，每次启动都跑一遍没关系）
await dbExec("CREATE TABLE IF NOT EXISTS punches (id INTEGER PRIMARY KEY, at TEXT, kind TEXT)");

// 写：返回 { changes, lastInsertId }
const r = await dbExec("INSERT INTO punches (at, kind) VALUES (?, ?)", [now, "in"]);

// 查：返回对象数组，一列一个键
const today = await dbQuery("SELECT * FROM punches WHERE at >= ? ORDER BY id DESC", [dayStart]);

// 多条一次提交（同一个事务，任一失败整体回滚）
await dbBatch([
  { sql: "INSERT INTO punches (at, kind) VALUES (?, ?)", params: [t1, "in"] },
  { sql: "INSERT INTO punches (at, kind) VALUES (?, ?)", params: [t2, "out"] },
]);
```

### 规则

- **存表之前先想清楚主键和索引**。没人替你建，`ORDER BY at` 又没有索引，
  几千行之后每次渲染都是全表扫。
- **一次查询最多 5000 行**，超了直接报错。要"最近 20 条"就写 `LIMIT 20`，
  别 `SELECT *` 然后把结果丢掉。
- **库文件最大 64 MiB**。存不下就说明该做成文件（`fs` 目前没开放）。
- **数字只有一种**：JS 的数字是 f64，所以超过 2^53 的整数会掉精度。
  真要存雪花 id，按 TEXT 存。
- **对象和数组会存成 JSON 文本**，取出来是字符串，要自己 `JSON.parse`。
  （省得你每次手动 stringify，但别指望它自动还回来。）
- **参数用 `?` 占位**，不要把值拼进 SQL 字符串 —— 引号、换行、空格都在
  等着坑你。

### 一个应用一个库

库里只有你自己的表：每个应用在 `~/.zex/state/db/<id>.db` 有一个独立文件。
`ATTACH` 和 `PRAGMA` 会被**拒绝**（`not authorized`），所以碰不到 zex 自己的
库，也改不了上限。这不是建议，是连接级作者器，绕不过去。

### 什么时候别建表

只有一两个设置项（主题、上次选的城市）→ 用 `localStorage`，别为了两行数据
建一张表。

## 网络：HTTP / WebSocket / TCP

三个通道都能用，但**必须先申请、再让用户确认**。没确认时调用会直接抛错。

### 权限是两层的：清单申请 + 用户授权

清单里的 `capabilities.network.hosts` 只算**申请**。真正生效的是用户点
「允许」之后存进 `config.json` 的那份授权（`network_grants`），生效的是
两者的**交集**：

- 用户没点过 → 一条都不通；
- 用户点了允许 → 只通清单里那几条；
- 之后清单偷偷加一条 → 新的一条不通（用户重新确认才会放宽）。

所以看到 `not granted` 时的正确反应是**让用户去确认**，不是去改清单：

```
这个应用中请访问 <host>，需要你在它的面板上点「允许」（或 设置 → 应用 → 允许联网）
```

反复改清单里的 hosts 不会让请求通过 —— 那只是在改"申请单"。

### 先写清单

```json
{ "capabilities": { "network": { "hosts": ["api.example.com", "*.example.org:8443"] } } }
```

| 写法 | 含义 |
|---|---|
| `api.example.com` | 该域名，端口不限 |
| `*.example.org` | 子域（**不含** `example.org` 本身） |
| `api.example.com:8080` | 限定端口 |
| `*` | 任意主机（界面上标红，等于放弃白名单） |

打开这个应用时，面板顶部会弹一张确认卡：点「允许」立刻生效（应用会原地
重挂一次，`init` 里那通请求就能通），点「不允许」则记下"拒绝过"，不再追问。
设置页里也能改（应用行右侧的**允许联网 / 撤销联网**）。

### HTTP

```js
import { httpRequest } from "zex.<id>";

const r = await httpRequest({
  url: "https://api.example.com/v1/items",
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ q: "x" }),
  timeout_ms: 8000,
});
// r = { status, ok, headers: [[k,v]], body: 字符串, truncated }
```

- 只支持 `http(s)://`，**不自动跟随重定向**（跟过去就能绕过白名单）。
  收到 3xx 就自己决定要不要再发一次 —— 那时会重新过一遍白名单。
- 响应体最多 4 MiB，超了 `truncated` 为 true。
- `body` 是按 UTF-8 有损解码的字符串；二进制内容不要走这条路。

### WebSocket

```js
const id = await wsConnect("wss://example.com/socket");  // ws / wss
await wsSend(id, JSON.stringify({ type: "sub" }));

while (true) {
  const msg = await wsRecv(id, 30000);      // 30 秒没消息会抛错
  if (msg.closed) break;
  if (msg.type === "text") handle(JSON.parse(msg.data));
}

await wsClose(id);
```

`wsRecv` 也是"收一条"的语义，不是事件回调 —— 因为宿主模块不能反过来
调脚本（没有跨 await 的 cx）。要持续收就在 `cx.spawn` 里循环。

### TCP

```js
const id = await tcpConnect("127.0.0.1", 6379);
await tcpSend(id, "PING
");
const r = await tcpRecv(id, 2000);   // { type, data, closed }
await tcpClose(id);
```

`tcpRecv` 拿到的是**一段字节**（UTF-8 有损解码），不保证一次读满一条协议
报文 —— 要按分隔符/长度自己拼。

### 三个共用限制

- 每个应用最多同时开 **8 个连接**，用完记得 `close`。
- 单条消息上限 1 MiB。
- 白名单按**主机名**判，不做 IP 二次校验。所以别把"申请了 A 域名"当成
  "绝对不会连到别的 IP"；写清单时按最小范围写。

## 剪贴板：能读能写，但要用户点一次

剪贴板**不是**宿主模块的导出，是 `cx` 上的方法。别写
`import { read_from_clipboard } from "zex.<id>"` —— 那样整个模块都加载不了，
`ext_check` 报的是 `SyntaxError: Could not find export 'read_from_clipboard'
in module 'zex.<id>'`（这个错十有八九就是找错地方了）：

```js
const text = cx.read_from_clipboard();   // string | undefined
cx.write_to_clipboard("要复制的文本");    // 返回 undefined
```

两个都是**同步**的（不返回 Promise）。只申请了写、没申请读的应用，
`read_from_clipboard` 会抛错 —— 读写是两个独立的能力位。

### 清单里分开写

```json
{ "capabilities": { "clipboard": { "read": true, "write": true } } }
```

只做「复制」按钮就只写 `write`；只做「从剪贴板导入」就只写 `read`。
**别为了省事两个都写** —— 用户看到的确认卡片会写明"想读取你的剪贴板"还是
"想往你的剪贴板里写内容"，申请宽了等于让他更难判断该不该点。

### 同样是两层：清单申请 + 用户授权

和网络一个道理（清单是模型写的，不能自己生效）：

- 没点过「允许」→ 调用直接抛 `reading the clipboard is not granted;
  declare capabilities.clipboard.read in the manifest`；
- 点了允许 → 按清单申请的那一半放行：只申请写就只拿到写；
- 点了「不允许」→ 一直拒绝，不再追问。

所以看到 `not granted` 时的正确反应是**请用户去确认**，不是去改清单：

```
这个应用想读剪贴板，需要你在它的面板上点「允许」（或 设置 → 应用 → 允许剪贴板）
```

### 三个坑

1. **`undefined` 不是错误。** 空剪贴板、或者里面是图片/文件而不是文本时，
   读出来是 `undefined`。别把它当"授权失败"，两者要分开处理：

   ```js
   const t = cx.read_from_clipboard();
   if (t === undefined) return div().child("剪贴板里没有文字");
   ```

2. **别在 `render()` 里写剪贴板。** `render` 会被反复调用，写一次就是覆盖
   用户剪贴板一次。写只放在明确的用户动作里（按钮的 `on_click`）。

3. **只在你真要的时候读。** 没有长度上限，用户可能复制了一整份日志进来。
   别在每次渲染都读一遍再塞进 state —— 让"从剪贴板导入"是一个按钮。

### 一个能抄的写法

```js
import { View, div } from "gpui-kit";
import { h_flex, v_flex, Button } from "gpui-base";

export default class Notes extends View {
  init() { this.text = ""; }

  render(cx) {
    const c = cx.theme().colors;
    return v_flex().size_full().gap(10).p(16).bg(c.background)
      .child(div().text_sm().child(this.text || "（还没有内容）"))
      .child(h_flex().gap(8)
        .child(Button.new("paste").h(30).px(12).rounded(6).bg(c.primary)
          .on_click((_e, ecx) => {
            // 只在用户点的时候读，不在 render 里读
            this.text = ecx.read_from_clipboard() ?? "";
            ecx.notify();
          })
          .child(div().text_color(c.primary_foreground).child("粘贴")))
        .child(Button.new("copy").h(30).px(12).rounded(6).bg(c.secondary)
          .on_click((_e, ecx) => { ecx.write_to_clipboard(this.text); })
          .child(div().text_color(c.secondary_foreground).child("复制"))));
  }
}
```

## 宿主接口

```js
import { ask, reply, busy, name, session, dbExec, dbQuery, dbBatch } from "zex.<id>";
```

**全部可用接口就这些**，别去猜还有没有别的：

| 函数 | 说明 |
|---|---|
| `ask(prompt)` | 问 agent，返回 `Promise<string>` |
| `reply()` | 最近一次回复，**同步可读**（推荐这条） |
| `busy()` | 是否有 `ask` 在等回包 |
| `name()` / `session()` | 显示名 / 会话 id |
| `dbExec(sql, params?)` | 写语句 → `{ changes, lastInsertId }`（详见「SQLite」） |
| `dbQuery(sql, params?)` | 查询 → 对象数组（详见「SQLite」） |
| `dbBatch(statements)` | 多条写语句一个事务（详见「SQLite」） |
| `httpRequest(spec)` | HTTP 请求（详见「网络」） |
| `wsConnect/wsSend/wsRecv/wsClose` | WebSocket（详见「网络」） |
| `tcpConnect/tcpSend/tcpRecv/tcpClose` | 裸 TCP（详见「网络」） |

另外 `window.localStorage` / `window.sessionStorage` 可用，但它们不是模块导出，
是 `window` 上的（详见「本地存储」）。**剪贴板也不在这张表里** ——
它是 `cx.read_from_clipboard()` / `cx.write_to_clipboard()`（详见上一节）。

模块名带 id 不是可选：宿主模块按名唯一，共用名字会互相顶掉。

## 三条必踩的坑

1. **`cx` 只属于产出它的那一次调用。**
   用回调自己的参数，别用 render 的：

   ```js
   // 对
   .on_click((_e, cx) => { this.n += 1; cx.notify(); })
   // 错：TypeError: cx is no longer valid
   .on_click(() => cx.notify())
   ```

   跨 `await` 的 `cx` 同样失效。所以拿回包用 `reply()`，不要 `await` 之后再 notify。

2. **`render()` 会被反复调用。** 要跑一次的事用 flag 挡住：

   ```js
   render(cx) {
     if (!this.inited) { this.inited = true; ask("...").catch(() => {}); }
     ...
   }
   ```

3. **`setTimeout` 不存在。** 要定时用 `cx.timer.after(ms, cb)` / `cx.timer.every`。

## 最小骨架

```js
import { View, div } from "gpui-kit";
import { v_flex, h_flex, Button } from "gpui-base";
import { ask, reply, busy, name } from "zex.<id>";

export default class App extends View {
  init() { this.n = 0; this.asked = false; }

  render(cx) {
    const c = cx.theme().colors;
    return v_flex().size_full().gap(10).p(16).bg(c.background)
      .child(div().text_lg().text_color(c.foreground).child(name()))
      .child(div().text_sm().text_color(c.muted_foreground).child(`点击 ${this.n} 次`))
      .child(h_flex().gap(8)
        .child(Button.new("b").h(30).px(12).rounded(6).bg(c.primary)
          .on_click((_e, clickCx) => { this.n += 1; clickCx.notify(); })
          .child(div().text_color(c.primary_foreground).child("+1")))
        .child(Button.new("a").h(30).px(12).rounded(6).bg(c.secondary)
          .on_click(() => ask("说一句话").catch(() => {}))
          .child(div().text_color(c.secondary_foreground).child("问"))))
      .child(div().text_sm().text_color(c.muted_foreground)
        .child(busy() ? "等待中…" : reply() || "（还没问过）"));
  }
}
```

## 能力边界

| 能做 | 不能做 |
|---|---|
| 画界面（`gpui-kit` / `gpui-base`） | 读文件（`fs` 未开放） |
| 调 agent（`ask` / `reply`） | 改宿主界面（只能画自己的面板） |
| 本地存储（`localStorage`，全局共享一份） | 子进程（未开放） |
| 网络：HTTP / WS / TCP（清单申请 ∩ 用户允许） | 监听端口（只能做客户端） |
| 剪贴板：读 / 写（`cx.read_from_clipboard()` / `cx.write_to_clipboard()`，清单申请 ∩ 用户允许） | 读剪贴板里的图片和文件（只有文本） |
| SQLite（`dbExec` / `dbQuery` / `dbBatch`，一应用一库） | 碰 zex 自己的库（作者器挡 `ATTACH`） |

清单里的 `capabilities` **声明**可以写，用户能在应用页看到你要什么。
`storage` 直接生效；`network` 和 `clipboard` 要用户点「允许」（清单与授权取交集
才算数，剪贴板的读写还各自独立）；`fs` / `execute` 写了也不生效 ——
直接告诉用户"这个能力当前不支持"，别绕（比如用 `ask` 让 agent 去读文件、
再让它把内容写进剪贴板，那绕过了用户对文件权限的判断）。

## 写完怎么验证

1. **`ext_check`** —— 先跑这个。它真的求值一遍脚本，把
   `Could not find export 'v_flex'` 这类错误原文（带行号）返回给你。
   模块解析、导入名、默认导出错在这儿；它**不渲染**，所以拿不到元素树。
2. **`ext_logs`** —— 跑起来之后出的事在这儿：`console.*`、回调抛的异常、
   未处理的 Promise 拒绝，都带 `main.js:12:44` 这样的位置。
3. 让人点开面板看实际画面（设置 → 扩展 → 打开）。布局对不对、坐标偏没偏，
   只有人眼能确认 —— 别替用户断言"看起来没问题"。
