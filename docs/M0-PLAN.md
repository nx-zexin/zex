# M0 计划：骨架（1–2 周）

> **M0 的验收标准**：三平台都能跑，都能问它"帮我看看这个项目怎么跑起来"并得到可执行答案。
> **一条窄路走通，比十条宽路挖一半有价值。**

---

## 排序原则

**先验证外部风险，不先写最爽的部分。**

| 风险类型 | 例子 | 处理顺序 |
|---|---|---|
| **外部风险** | gpui 在 Linux/Wayland 能不能跑、系统库缺不缺、构建多久 | **第一件事**（失败会推翻技术选型） |
| **内部风险** | agent loop 怎么写、上下文怎么压缩 | 后面做（失败只是返工） |

---

## 任务清单

### T0. 三平台 gpui 冒烟 ★ 最高优先级

| 项 | 内容 |
|---|---|
| 做什么 | 最小 hello world，只验证「能弹窗」 |
| 验收 | 三平台各自弹出窗口并渲染一个按钮；记录首次构建耗时 |
| 阻塞 | 若 Linux/Wayland 不支持 → 整个技术选型作废，需重新评估（退到 Tauri / egui / Iced） |
| 状态 | 🔄 进行中（Linux 编译验证中） |

**已知风险**：当前开发机是 Manjaro + **Wayland** 会话。gpui 上游对 Linux 的支持路径需要实测确认。

---

### T1. Workspace 骨架 + CI

| 项 | 内容 |
|---|---|
| 做什么 | 12 个 crate 的目录与 `Cargo.toml`；`rust-toolchain.toml` 锁版本；GitHub Actions 三平台 matrix |
| 验收 | 三平台 `cargo check --workspace --all-targets` + `clippy -D warnings` 全绿 |
| 关键 | `fail-fast: false`，不允许只有一个平台绿 |
| 依赖 | T0 |

---

### T2. `zex-core`：纯类型层

| 项 | 内容 |
|---|---|
| 做什么 | `Message` / `Part` / `ToolCall` / `ToolSpec` / `Tool` trait / `Risk` / `CoreEvent` / `UiCommand` / `Error` |
| 验收 | 序列化往返测试通过；**`Cargo.toml` 里没有 tokio / reqwest / rusqlite** |
| 关键 | `Tool` trait 的 `RISK` 无默认值；`Args` 关联类型 `DeserializeOwned + JsonSchema` |
| 依赖 | T1 |

---

### T3. `zex-llm`：双协议客户端 ★ 核心难点

| 项 | 内容 |
|---|---|
| 做什么 | `openai-compatible` + `anthropic` 两种线格式，SSE 流式，工具调用 |
| 验收 | ① 对着真实 API 跑通流式文本；② 跑通流式工具调用（含 arguments 分片拼接）；③ 多轮工具调用序列正确；④ 单测用 mock SSE 覆盖边界 |
| 关键难点 | SSE chunk 分片、`data:` / `[DONE]` / keep-alive 注释行、tool_calls 增量拼接、工具调用序列修复 |
| 依赖 | T2 |

**必须处理的边界情况**（写进测试）：

```
- 一个 SSE chunk 被 TCP 切成两半
- 多个 `data:` 行在同一个 chunk 里
- keep-alive 注释行 `: ping`
- arguments 分片：{"pa → th": → "a.txt"}
- 空 arguments 字符串
- 模型返回 tool_call 但没有对应的 tool 结果就被再次调用
- 流中断（连接断开）后重试不应重复已产出的 token
```

---

### T4. 双世界桥接骨架

| 项 | 内容 |
|---|---|
| 做什么 | 独立 tokio runtime 线程；`mpsc<UiCommand>` / `mpsc<CoreEvent>`；UI 侧 `cx.spawn` 消费事件 |
| 验收 | tokio 线程每秒发一个心跳事件，gpui 窗口上的计数器逐秒递增 |
| 关键 | 验证 gpui 的 `cx.spawn` 与 tokio 线程的协作不出 panic |
| 依赖 | T2 |

---

### T5. `zex-store`：数据库骨架

| 项 | 内容 |
|---|---|
| 做什么 | `rusqlite` + 迁移系统 + `sessions` / `messages` 表 + WAL |
| 验收 | 建会话 → 追加消息 → 关进程 → 重开读回，数据完整 |
| 关键 | 迁移从第一天就上（version 表 + 顺序脚本），不要等有数据再补 |
| 依赖 | T2 |

---

### T6. `zex-agent`：最小 loop

| 项 | 内容 |
|---|---|
| 做什么 | `loop { build_context → llm.stream → dispatch tools → repeat }`，无压缩、无权限 |
| 验收 | 给它 `fs_read`，能完成"读这个文件并总结" |
| 关键 | 不做 planner、不做状态机。薄。 |
| 依赖 | T3, T4, T5 |

---

### T7. `zex-tools`：5 个基础工具

| 工具 | Risk | 说明 |
|---|---|---|
| `fs_read` | L0 | 带 offset/limit，大文件分段 |
| `fs_write` | L1 | 写入前打快照 |
| `shell` | L2 | 超时、输出截断、`CREATE_NO_WINDOW`（Win） |
| `web_fetch` | L0 | HTML → Markdown |
| `ask` | L0 | 向用户提问并等待回答（GUI 弹窗 / CLI stdin） |

| 验收 | 每个都有单测；schema 由 schemars 自动生成；加一个新工具只需一个文件 |
|---|---|
| 依赖 | T2, T6 |

---

### T8. `zex-app`：流式对话 UI

| 项 | 内容 |
|---|---|
| 做什么 | gpui 窗口 + 输入框 + 消息列表 + 流式渲染 + Markdown |
| 验收 | 打字 → 回车 → 看到逐字输出 → 代码块正确高亮 → 工具调用有折叠展示 |
| 依赖 | T4, T6 |

**UI 用 gpui-kit 现成组件**：`Input`（或 `TextareaState`）、`VirtualList`、`Markdown`。不自造轮子。

---

### T9. `zex-cli`：headless 入口

| 项 | 内容 |
|---|---|
| 做什么 | 同一内核，另一个 channel 端点；参数 `zex-cli "prompt"` |
| 验收 | `cargo run -p zex-app --bin zex-cli -- "读一下 README 并总结"` 输出到 stdout |
| 关键 | **不得有独立于 GUI 的能力分支**（ADR-009） |
| 依赖 | T6 |

---

### T10. 工具注册宏

| 项 | 内容 |
|---|---|
| 做什么 | `inventory` 或 declarative macro，自动收集所有 `Tool` 实现 |
| 验收 | 新增工具不需要改任何 registry 代码 |
| 依赖 | T7 |

---

## 里程碑检查点

| 检查点 | 判定 | 不通过怎么办 |
|---|---|---|
| **CP-1（T0 后）** | 三平台弹窗成功 | 若 Linux 不可行 → 技术选型重评估 |
| **CP-2（T1 后）** | CI 三平台绿 | 修到绿，不要跳过 |
| **CP-3（T3 后）** | 双协议跑通真实 API | 这是产品命脉，不能糊弄 |
| **CP-4（T6+T7 后）** | CLI 能完成一次真实任务 | M0 核心价值验证 |
| **CP-5（T8+T9 后）** | GUI 与 CLI 行为一致 | M0 完成 |

---

## M0 明确不做

| 不做 | 理由 |
|---|---|
| 上下文压缩 | M1 |
| 权限闸门 / 审批 UI | M1（M0 所有工具先强制直通，但审计接口预留） |
| 记忆 / 技能 | M2 |
| 浏览器 / OS 管理 | M3 |
| 知识库 / 自我进化 | M4 |
| 多模态（图片输入） | M1 之后 |
| 托盘 / 常驻 / 自动更新 | M2 / M5 |
| 子智能体 / 并行任务 | M2 |

---

## 进度跟踪

| # | 任务 | 状态 | 备注 |
|---|---|---|---|
| T0 | 三平台 gpui 冒烟 | ✅ Linux 已验证 | gpui-kit 0.9.1 编译 2m34s；Manjaro/Wayland(niri)+XWayland 下窗口创建成功、事件循环稳定、GPU 走 Vulkan(Intel Arc)。**Windows/macOS 待 CI 验证** |
| T1 | Workspace + CI | ✅ 完成 | 12 crate；三平台 matrix（fail-fast: false）+ 3 项分层约束检查 |
| T2 | `zex-core` | ✅ 完成 | 18 个测试全绿；schema 由 schemars 自动生成且包含文档注释 |
| T3 | `zex-llm` | ⬜ 待开始 | **下一个任务，核心难点** |
| T4 | 双世界桥接 | ✅ 完成 | tokio runtime 线程 + mpsc + WeakEntity 存活检测，实测稳定运行 |
| T5 | `zex-store` | ⬜ 待开始 | |
| T6 | `zex-agent` | ⬜ 待开始 | |
| T7 | `zex-tools` | ⬜ 待开始 | |
| T8 | GUI | ⬜ 待开始 | |
| T9 | CLI | 🔄 骨架已通 | 参数解析 + 帮助信息可用，内核待 T6/T7 接入 |
| T10 | 工具注册宏 | ⬜ 待开始 | |

图例：⬜ 待开始 / 🔄 进行中 / ✅ 完成 / ⚠️ 受阻 / ❌ 取消

---

## 实测记录

### T0：gpui-kit 0.9.1 平台验证（2026-11-11）

| 项 | 结果 |
|---|---|
| 版本 | **0.9.1**（文档站主力展示的是 0.6/0.7，注意以 crates.io 为准） |
| 依赖数量 | 525 个 crate |
| 首次构建 | 2 分 34 秒（22 核 / 30G 内存） |
| 环境 | Manjaro Linux + Wayland 会话（niri compositor）+ XWayland (`DISPLAY=:1`) |
| 窗口创建 | ✅ `PROBE_OK WindowId(1v1)` |
| 事件循环 | ✅ 持续运行不退出 |
| GPU | ✅ Vulkan（Intel Arc MTL），另有 GL 后备 |

**已发现的坑**：

1. **字体警告**：`failed to load font ".SystemUIFont"` → 回退到 `Segoe UI`，Linux 上通常不存在。
   需要在启动时按平台指定字体族（Linux 用 `Noto Sans CJK SC` / `Source Han Sans` 等），否则中文可能显示为方框。

2. **0.9 的 API 与旧文档不同**：
   - `gpui_kit::open_window(...)` 是顶层函数，**内部已包 `Root`**，不再需要手动 `cx.new(|cx| Root::new(...))`。
   - `Entity::update` 返回 `R`（不是 `Result`）；要检测窗口关闭必须用 `Entity::downgrade()` 拿到 `WeakEntity`，它的 `update` 才返回 `Result`。
