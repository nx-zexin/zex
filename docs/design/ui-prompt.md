# Zex 界面设计 · 文生图提示词

> 用途：用文生图工具（Midjourney / DALL·E / Stable Diffusion / Flux）产出 Zex 桌面端的
> 界面设计稿，用于确定视觉方向，再据此改造 `crates/zex-app/src/bin/zex.rs`。
>
> ⚠️ **先说清楚能期待什么**：文生图工具对界面里的文字几乎一定会拼错（尤其中文）。
> 所以用它看**布局、配色、层次、气质**是可靠的，指望它输出可用的文案是妄想。
> 提示词里给出的中文文案是为了让构图"看起来像真的"，不是要求它写对。

---

## 一、视觉规格（这一版是"应该长成什么样"，不是当前实现）

当前实现只有六个灰阶，是能跑的原型。以下是设计目标：

### 配色

| 用途 | 值 | 说明 |
|---|---|---|
| 应用背景 | `#0E1014` | 近黑，带一点冷调 |
| 面板/栏 | `#16191F` | 与背景拉开一层 |
| 边框/分割 | `#252A33` | 极低调，靠间距而非线框分层 |
| 主文本 | `#E6E8EC` | 不用纯白，减少长时间阅读的刺眼感 |
| 次文本 | `#8B92A0` | 角色标签、元信息 |
| **品牌色** | `#E8A33D` | 琥珀金。刻意避开"AI 蓝"——助理该有温度，不该像仪表盘 |
| L0 只读 | `#6B7280` | 灰 |
| L1 可逆 | `#4A9EFF` | 蓝 |
| L2 有影响 | `#E8A33D` | 琥珀（同品牌色） |
| L3 高危 | `#E5484D` | 红 |

**风险四色是整个界面的视觉骨架**——用户扫一眼颜色就知道这次操作有多危险。

### 布局

```
┌──────────┬────────────────────────────────────────┐
│          │  状态栏  48px                          │
│  会话列表 ├────────────────────────────────────────┤
│  240px   │                                        │
│          │  消息区（文档流，非气泡）              │
│          │  flex:1                                │
│          │                                        │
│          ├────────────────────────────────────────┤
│          │  审批条（条件出现）                    │
│          ├────────────────────────────────────────┤
│          │  输入区 96px                           │
└──────────┴────────────────────────────────────────┘
```

**关键决策：消息不用气泡，用"文档流"。**
理由：Zex 的输出经常是一段带代码、带工具调用、带长文件路径的内容。
气泡的窄宽度会让这类内容反复折行，可读性远不如通栏文档流。这是 IDE/终端气质，
不是即时通讯气质——因为它是助理，不是聊天机器人。

### 字体

- 正文：无衬线（Noto Sans CJK / Inter）
- 路径、命令、代码：等宽（JetBrains Mono / SF Mono）
- 角色标签：字号小 2 档，全大写或小字距，用次文本色

---

## 二、主提示词（英文 · 推荐）

> 大多数工具对英文理解更稳。**直接整段复制**。

```
A professional dark-mode desktop application UI design, 16:10 landscape, 1x resolution, flat vector style, no photorealism.

This is "Zex", a personal AI digital assistant that runs locally on a developer's computer with full system control. The interface must feel like a serious developer tool — think modern IDE or terminal — not a consumer chat app.

LAYOUT: three-column structure. A narrow 240px left sidebar listing previous sessions. A main content area on the right, split vertically into: a 48px top status bar, a flexible scrollable message area, a conditional approval panel, and a 96px bottom input area.

LEFT SIDEBAR: dark panel #16191F, thin right border #252A33. At top a small amber indicator dot plus the wordmark "Zex" in medium weight. Below it a "New session" ghost button with a plus icon. Then a vertical list of 6 session items, each one line of muted #8B92A0 text at 13px, truncated with ellipsis. The active item has a subtle #1E232B background and a 2px amber left accent bar. Items show relative timestamps on the right in 11px dimmer text.

TOP STATUS BAR: background #16191F, 1px bottom border. Left side shows the current working directory path in monospace 12px muted gray, like "/home/adam/workspace/ChorusDev". Right side shows a small pill-shaped status chip with a soft green dot and text "Ready", plus a token counter in monospace 11px reading "3.4k / 108".

MESSAGE AREA: background #0E1014, generous 32px horizontal padding, messages flow as a continuous document rather than bubbles. Each message block has a small uppercase role label in 11px letter-spaced muted text (USER, ZEX, TOOL, SYSTEM), then the content below in 14px #E6E8EC with comfortable 1.6 line height.

Show five message blocks in sequence:
1. Role label "USER", content: a short two-line Chinese question about analyzing a codebase.
2. Role label "ZEX", a short paragraph of Chinese explanation.
3. A TOOL CALL block, visually distinct: slightly indented, left 2px border in amber #E8A33D, monospace 12px content showing a tool name "shell" and a command string in a subtle #12151A code background with rounded 6px corners. Include a small amber badge reading "L2" at the right end of the block's header line.
4. A tool RESULT block, similar indentation but left border in muted gray, monospace output lines, and a small green checkmark.
5. Role label "ZEX", streaming response with a blinking amber text cursor at the end.

APPROVAL PANEL: sits above the input area, full width, background #1A1420 with a 1px top border in amber at 40% opacity. Contains a warning triangle icon in amber, a bold 14px title "需要确认" plus a risk badge "L2 有影响", then a 13px muted description line, then a monospace parameter line in a slightly darker inset box. On the right side, three compact buttons: a solid amber primary button "批准", a ghost outlined button "拒绝", and a text-only button "批准并记住此类".

INPUT AREA: background #16191F, top border. A large rounded 10px input field with #1E232B background, placeholder text in muted gray, and a small monospace hint below the left edge reading the model name. On the right, a circular 40px solid amber send button with a white arrow icon pointing up.

STYLE DETAILS: 8px spacing grid everywhere. Border radius: 6px for code blocks, 8px for panels, 10px for input, fully round for pills and buttons. Extremely subtle elevation — no drop shadows heavier than 4px blur. Text rendering crisp and high contrast. No gradients, no glassmorphism, no glow effects. Restrained, dense, calm, professional.

TYPOGRAPHY: clean geometric sans-serif for UI text, monospace for all paths, commands and tool output. Small caps role labels. Tight but readable.

COLOR ACCENTS ONLY WHERE THEY MEAN SOMETHING: amber for the brand and for L2 risk, red only for L3 risk, blue only for L1, gray for L0. Everything else is neutral gray. The interface should read as mostly monochrome with precise color punctuation.

Rendered as a flat UI screenshot, straight-on, no perspective, no 3D mockup, no device frame, no reflections.
```

---

## 三、主提示词（中文版）

> 给对中文友好的工具（DALL·E 3 / 即梦 / 通义万相 等）用。

```
一个专业的深色桌面应用界面设计稿，16:10 横屏比例，扁平矢量风格，不要照片写实。

这是「Zex」，一个运行在开发者本机的个人 AI 数字助理，拥有完整的系统控制权。界面气质要对标现代 IDE 或终端这类专业开发工具，而不是消费级聊天软件。

【整体布局】三栏结构：左侧 240px 窄边栏是历史会话列表；右侧主内容区纵向分为四段——48px 顶部状态栏、自适应的可滚动消息区、按条件出现的审批面板、96px 底部输入区。

【左侧边栏】深色面板 #16191F，右侧一条极细的 #252A33 分隔线。顶部是一个小的琥珀色圆点加「Zex」字标。下面是一个带加号的「新建会话」幽灵按钮。再往下是 6 条会话条目，每条一行 13px 的灰蓝色文字，超出用省略号截断；当前选中项有极淡的 #1E232B 底色和左侧 2px 琥珀色竖条；每项右侧有 11px 更暗的相对时间。

【顶部状态栏】背景 #16191F，底部一条细线。左边用等宽字体 12px 灰色显示当前工作目录路径，例如 /home/adam/workspace/ChorusDev。右边是一个小胶囊状态标签，带柔和的绿点和「就绪」文字，旁边用等宽 11px 显示 token 计数。

【消息区】背景 #0E1014，左右留白 32px。消息以连续文档流的形式排列，不用聊天气泡。每条消息上方是一行 11px 大写字距的小标签（你 / ZEX / 工具 / 系统），标签下方是 14px 正文，行高 1.6。

依次展示五块内容：一条用户的中文提问；一段 ZEX 的中文回答；一个工具调用块（左侧 2px 琥珀色竖线、等宽字体、深色代码底色、右上角一个小的 L2 琥珀标签）；一条工具执行结果（左侧灰色竖线、等宽输出、绿色对勾）；最后一段 ZEX 的流式回答，结尾有一个闪烁的琥珀色光标。

【审批面板】位于输入区上方、占满整宽，背景 #1A1420，顶部一条半透明琥珀色边线。左侧有琥珀色警告三角图标、14px 加粗标题「需要确认」加一个「L2 有影响」风险标签、下面一行 13px 说明文字、再下面是一个更深的等宽参数框。右侧三个紧凑按钮：琥珀色实心「批准」、描边幽灵「拒绝」、纯文字「批准并记住此类」。

【输入区】背景 #16191F，顶部细线。一个大的 10px 圆角输入框，底色 #1E232B，占位文字为灰色；左下角下方用小号等宽字体显示模型名。右侧一个 40px 圆形琥珀色发送按钮，白色向上箭头。

【视觉细节】全局 8px 间距网格。圆角：代码块 6px、面板 8px、输入框 10px、胶囊与按钮全圆。投影极轻，不超过 4px 模糊。文字锐利高对比。不要渐变、不要毛玻璃、不要发光效果。整体克制、紧凑、平静、专业。

【配色原则】琥珀色只用于品牌和 L2 风险，红色只用于 L3，蓝色只用于 L1，灰色用于 L0。整体应该是近乎单色的中性灰，用精准的彩色点缀来传达风险等级。

输出为正面平视的扁平 UI 截图，不要透视、不要 3D 样机、不要设备外框、不要反光。
```

---

## 四、按工具适配

### Midjourney（v6 / v7）

主提示词 + 以下参数追加到末尾：

```
--ar 16:10 --style raw --stylize 100 --v 7
```

- `--stylize 100` 是刻意的低值：界面设计要的是准确，不是艺术性。
  默认的 100 已经偏"好看"，调到 0–100 之间最稳。
- 加 `--no` 参数排除常见污染：

```
--no 3d render, mockup, device frame, glow, neon, gradient, glassmorphism, drop shadow, text errors, watermark, logo
```

### DALL·E 3 / GPT-4o 图像

直接用中文或英文主提示词即可，但开头加一句约束：

> 请严格按照下面描述的布局和配色生成，不要自由发挥，不要添加描述中未提及的元素。

DALL·E 倾向于"美化"提示词，加这句能压住一部分。

### Stable Diffusion / Flux

用 tag 式串联，并把布局描述放前面（SD 对前 75 token 权重最高）：

```
ui design, desktop application, dark mode, flat vector, three column layout, left sidebar session list, top status bar, document flow messages, tool call block with amber accent, approval panel, bottom input area, amber accent color, monospace text, #0E1014 background,
professional, dense, restrained, developer tool aesthetic,
<negative: photorealistic, 3d, mockup, perspective, device frame, neon, glow, gradient, glassmorphism, blurry text, watermark, cluttered>
```

---

## 五、局部特写提示词

生成主界面后再出特写，用于检查细节。

### 审批面板特写

```
Extreme close-up of a single UI component: an approval confirmation panel from a dark-mode desktop app, filling the frame edge to edge.

Background #1A1420 with a 1px top border in amber at 40% opacity. Padding 24px. Left side: an amber warning triangle icon 20px, then on the same baseline a bold 14px white title in Chinese meaning "Needs confirmation", immediately followed by a small rounded pill badge with amber text on a translucent amber background reading "L2". Below, a 13px muted gray line of Chinese description text. Below that, an inset box with #12151A background and 6px radius containing a monospace parameter string in 12px.

Right side, vertically centered: three buttons in a row — a solid amber filled button with dark text, a ghost button with 1px gray outline and light text, and a plain text-only button in muted gray. Buttons are 32px tall, 8px apart, fully rounded corners.

Flat vector, crisp typography, no shadows beyond 4px, no gradients.
```

### 风险等级色卡

```
A minimal UI design reference sheet on a #0E1014 background: four horizontal badges stacked with 24px gaps, each a fully rounded pill, 28px tall, showing a risk level label and a short Chinese descriptor.

Badge 1: gray #6B7280 text and border, label "L0 只读".
Badge 2: blue #4A9EFF text and border, label "L1 可逆".
Badge 3: amber #E8A33D filled background with dark text, label "L2 有影响".
Badge 4: red #E5484D filled background with white text, label "L3 高危".

Flat vector, precise typography, generous negative space, no decoration.
```

### 浅色主题（可选）

把主提示词里的色值替换：

| 深色 | 浅色 |
|---|---|
| `#0E1014` 背景 | `#FAFAF9` |
| `#16191F` 面板 | `#FFFFFF` |
| `#252A33` 边框 | `#E6E4E0` |
| `#E6E8EC` 主文本 | `#1A1A18` |
| `#8B92A0` 次文本 | `#78766F` |
| `#E8A33D` 品牌 | `#C97F1E`（浅底上要压暗一档） |

---

## 六、使用建议

1. **一次只生成主界面**，别一上来就要"主界面 + 弹窗 + 设置页"拼在一起。多屏拼图会让每一屏的细节都糊掉。
2. **锁定一张满意的主界面后，用它的图做参考图（img2img / `--cref`）**再生成变体，能保持风格一致。
3. **别信文字**。看到 "Zex" 变成 "Zcx"、"批准"变成乱码是正常的。看布局和配色就好。
4. **优先检查三件事**：① 消息区是不是文档流而不是气泡；② 琥珀色是不是只出现在品牌和 L2；③ 审批面板是不是压在输入区上方而不是弹成模态框。
5. 拿到满意的图后，把**色值和间距**抄进 `zex.rs`，比抄长相重要——gpui 的布局能力完全够还原这个设计，缺的只是这些具体数值。

---

## 七、设计取舍说明

几个看起来"可以更炫"但我刻意没要的东西，理由是：

| 没要 | 原因 |
|---|---|
| 气泡式聊天气泡 | 助理输出常含代码、路径、长命令，窄气泡反复折行，可读性差 |
| 毛玻璃 / 发光 / 渐变色 | 长时间盯着会累，且和"专业工具"气质冲突 |
| "AI 蓝"主色 | 每个 AI 产品都是蓝的，没有辨识度；琥珀更暖，符合"助理"而非"仪表盘" |
| 大圆角、大留白 | 助理界面信息密度天然高，过度留白会让长任务需要不停滚动 |
| 模态审批弹窗 | 模态会打断对话上下文；内嵌审批条让用户能看到"为什么问"的来龙去脉 |
| 头像 / 拟人头像 | 它是工具不是人。加头像会制造错误的心理预期 |
