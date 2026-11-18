---
name: write-extension
description: 给 zex 写一个扩展（脚本面板/独立小应用），含模块划分、宿主接口和三条必踩的坑
---

# 给 zex 写扩展

扩展是一段跑在 zex 进程内的 JavaScript，能画自己的界面、能调 agent 和工具。
不需要编译，改完重新打开面板即可生效。

## 目录

```
~/.zex/extensions/<id>/
├── gpui-shell.json     # 清单
├── main.js             # 入口
└── gpui-kit.d.ts       # 自动生成，别手写、别提交
```

清单：

```json
{ "id": "git-status", "name": "Git 状态", "entry": "main.js" }
```

`id` 决定模块名（见下），`entry` 必须是目录内的相对路径。

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

## 宿主接口

```js
import { ask, reply, busy, name, session } from "zex.<id>";
```

| 函数 | 说明 |
|---|---|
| `ask(prompt)` | 问 agent，返回 `Promise<string>` |
| `reply()` | 最近一次回复，**同步可读**（推荐这条） |
| `busy()` | 是否有 `ask` 在等回包 |
| `name()` / `session()` | 显示名 / 会话 id |

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

扩展默认**什么都碰不到**：没有文件、没有网络、没有剪贴板、没有子进程。
要读文件、要联网，走宿主接口（由 zex 代劳，并受权限与审计约束），
不要在清单里堆 `capabilities` 指望它生效。

## 写完怎么验证

1. **`ext_check`** —— 先跑这个。它真的求值一遍脚本，把
   `Could not find export 'v_flex'` 这类错误原文（带行号）返回给你。
   模块解析、导入名、默认导出错在这儿；它**不渲染**，所以拿不到元素树。
2. **`ext_logs`** —— 跑起来之后出的事在这儿：`console.*`、回调抛的异常、
   未处理的 Promise 拒绝，都带 `main.js:12:44` 这样的位置。
3. 让人点开面板看实际画面（设置 → 扩展 → 打开）。布局对不对、坐标偏没偏，
   只有人眼能确认 —— 别替用户断言"看起来没问题"。
