# 扩展

扩展是一段跑在 zex 进程内的 JavaScript，能画自己的界面（挂在右侧面板的一个标签里），
也能调 agent。它不需要重新编译，也不需要重新分发二进制。

```text
~/.zex/extensions/git-status/
├── gpui-shell.json     # id / name / entry
└── main.js
```

把 `examples/extensions/demo` 整个目录拷到 `~/.zex/extensions/` 就能看到它：

```bash
cp -r examples/extensions/demo ~/.zex/extensions/
```

然后在「设置 → 扩展」里点「打开」。

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

### 两种拿结果的写法

```js
// 一：await（适合按钮里的一次性动作）
Button.new("go").on_click(async () => {
  this.answer = await ask("解释这段代码");
  cx.notify();          // 这个 cx 已经跨了 await，可以用，但别存起来
});

// 二：不 await，下一帧从 reply() 读（推荐）
ask("解释这段代码").catch(() => {});
// render 里：reply() || "（还没问过）"
```

第二种更稳：跨 `await` 的 `cx` 会失效（gpui-shell 会明确报错），而宿主在回包到达时
会主动重跑 `render`，所以脚本只要同步读取就行。

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
