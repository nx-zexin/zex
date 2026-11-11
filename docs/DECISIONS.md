# 架构决策记录（ADR）

> 每条记录：决定了什么、为什么、否决了什么、什么时候该重新考虑。
> **被否决的方案必须留着** —— 否则三个月后会有人重新提一遍。

---

## ADR-001：采用纯 Rust 单体架构，不保留 Go 内核

**状态**：已采纳（2026-11）

**决定**：Zex 是一棵 Rust cargo workspace，UI 与内核同进程。旧项目 Zebra（Go）冻结为参考实现，不作为运行时依赖。

**背景**：Zebra 有 13.7 万行 Go，工具、记忆、自我进化都已成熟。理论上最快的路是"Rust 写 UI + Go 做 sidecar"。

**否决的方案**：

| 方案 | 否决理由 |
|---|---|
| **B：Rust UI + Go 内核（stdio JSON-RPC）** | ① 这本质是 Mews 的老路（Wails 包 Zebra），换掉 Wails 却把 Go 焊死进架构；② schema 双写，Rust/Go 各一份靠人肉同步，是长期 bug 温床；③ 跨进程流式延迟，把 gpui 最大的优势（同进程零拷贝流式）废掉；④ 两套构建链、跨语言调试 |
| **C：Rust 单核 + Go sidecar 渐进迁移** | 中间态复杂度最高，而中间态会活很久 |

**依据的核心论断**：

> 该继承的是**设计资产**（文件格式、协议、阈值、踩坑教训），不是**代码资产**。

13.7 万行里，约 4 万行是编排层（workflow 5.6k + ralphloop 4.2k + multiagent 3.9k + planner/critic/orchestrator），本来就要扔。真正的工具层只有 7 类（fs/shell/search/web/browser/knowledge/memory），Rust 重写约 2–3 周。

**隐藏红利**：`schemars` derive 让"工具定义 = Rust 类型"，Schema 自动生成。Go 方案里这是双写痛点。见 ADR-004。

**何时重新考虑**：如果 M2 结束时发现某个能力（如向量检索）Rust 侧实现质量显著低于 Go 侧，可以临时引入该单一能力的 sidecar，但**不得引入通用内核 sidecar**。

---

## ADR-002：双世界桥接 —— tokio 内核 + gpui UI，用 channel 连接

**状态**：已采纳

**决定**：内核跑在独立 tokio runtime 线程，UI 在 gpui 主线程，两者用 `mpsc<UiCommand>` / `mpsc<CoreEvent>` 通信。

**背景**：gpui 的 async executor 是 smol 系；`reqwest`、`tokio::fs`、CDP 客户端全要 tokio。直接在 gpui task 里 poll tokio 的 future 会产生"在错误 runtime 里 poll"的运行时恐慌，这类 bug 极难调试。

**否决的方案**：

- **全部用 smol / async-std**：生态不匹配，`reqwest`、`rusqlite` 异步封装、绝大多数 CDP crate 都绑 tokio。会陷入无休止的适配。
- **直接用 gpui executor 跑一切**，靠 `tokio::runtime::Handle::enter()` 兜：能跑，但错误处理边界模糊，一处漏 enter 就是线上崩溃。

**收益**（不只是规避问题）：

1. 内核完全不知道 gpui 存在 → 可在 `#[tokio::test]` 里独立测试，无需启动 UI。
2. 同一内核直接产出 `zex-cli`（headless），供脚本和 cron 调用 —— 这是产品需求，不是副产品。
3. 未来若换 UI 框架（不排除），内核零改动。

**硬性约束**：`zex-agent` 及其下游的 `Cargo.toml` 出现 `gpui-kit` 即为分层破坏，CI 拦截。

---

## ADR-003：不内嵌浏览器，走 CDP 控制外部浏览器

**状态**：已采纳

**决定**：`zex-browser` 通过 Chrome DevTools Protocol 控制用户已有的 Chrome / Edge / Chromium 进程（或按需拉起一个带调试端口的实例）。不内嵌 WebView。

**背景与证据**：

`gpui-webview`（原 `gpui-wry`）的官方平台支持表：

| 平台 | 引擎 | 状态 |
|---|---|---|
| Linux (X11) | WebKitGTK | **实验性** |
| Linux (Wayland) | — | **不支持**，必须走 XWayland |
| macOS / Windows | — | 实验性 |

当前开发机是 Manjaro + Wayland 会话，内嵌 WebView 直接不可用。

**但更重要的是产品理由**：数字助理要操作的是**用户真实的浏览器** —— 带着登录态、cookie、已安装的插件、已保存的密码。一个内嵌的无状态 WebView 需要用户重新登录每一个网站，这在真实工作里是不可接受的。

**否决的方案**：

- **内嵌 WebView（gpui-webview / wry）**：平台受限 + 无用户登录态 + 三平台行为不一致。
- **Playwright / Puppeteer（需 Node 运行时）**：引入 Node 依赖，违背单二进制目标，且同样是无状态浏览器。
- **CEF**：体积巨大（>150MB），集成复杂度远超收益。

**实现要点**：

- CDP 用裸 WebSocket（`tokio-tungstenite`）或 `chromiumoxide`，走 `Page` / `Runtime` / `DOM` / `Input` 域。
- 浏览器生命周期：优先"接管"用户已开的调试实例（`--remote-debugging-port`），其次"拉起"独立实例，最后"引导"用户授权。
- 多标签页管理、导航等待、网络空闲判定都要包装成稳定语义。

---

## ADR-004：工具定义用 schemars 自动生成 Schema，RISK 编译期强制

**状态**：已采纳

**决定**：

```rust
pub trait Tool: Send + Sync + 'static {
    const NAME: &'static str;
    const RISK: Risk;                    // 编译期强制
    type Args: DeserializeOwned + JsonSchema;
    fn description() -> &'static str;
    async fn call(&self, cx: &ToolCtx, args: Self::Args) -> Result<ToolOutput>;
}
```

**决定 1：Schema 由 `schemars::schema_for!(Args)` 生成。**

工具参数结构体的字段文档注释直接成为给模型的参数说明。三条收益：
- 不可能与实现漂移（手写 schema 的经典死法是"改了参数忘了改 schema"）
- **写注释就是在写给模型看的提示词** —— 让文档和提示词合成一件事
- 新增工具只需写一个文件，不用改任何注册表

**决定 2：`RISK` 是关联常量，不给默认值。**

漏标 → 编译失败。这是有意为之：默认值意味着"忘记标注的工具会被静默当作安全工具放行"，而那正是权限事故的典型形态。

**否决的方案**：

- **运行时注册 + JSON 描述文件**：灵活但弱类型，参数错误在运行时才暴露。
- **`RISK` 给默认值 `ReadOnly`**：见上，危险。
- **手写 `serde_json::json!({...})` schema**：Zebra 的做法，44 个工具文件里维护了上千行手写 schema，漂移不可避免。

---

## ADR-005：单一 SQLite 库承载全部状态

**状态**：已采纳

**决定**：会话、记忆、知识库、审计、授权、定时任务全部在一个 `zex.db` 里。

**背景**：Zebra 的状态分散在 `memory.db`、`config.json`、`memories/*.md`、`.curator-state.json`、`tasks.json`、`checkpoint.json`，外加一个独立的 ZebraKnowledge HTTP 服务。结果是：备份要记得备份 6 个地方，迁移没有统一版本号，跨模块事务不存在。

**收益**：一个库、一套迁移、一个备份点、真正的跨模块事务。

**代价**：单文件锁竞争。缓解：WAL 模式 + 单写多读 + 长任务不持有事务。

**关于 Markdown 记忆文件**：`~/.zex/memories/*.md` 保留为**人类可读的镜像**，DB 是真相来源。双向同步（写穿）。理由：用户要能看见、能编辑、能用 git 管理自己的记忆 —— 这是长期信任的基础。但**不做**"文件是主、DB 是索引"的反向设计，那会导致一致性噩梦。

**何时重新考虑**：如果单库超过 10GB 或写入 QPS 成为瓶颈。预计不会。

---

## ADR-006：不做编排层

**状态**：已采纳（本项目的核心判断）

**决定**：不实现 workflow DSL、多角色议会（planner / critic / clarifier / orchestrator）、DAG 任务图、Ralph 循环。

**只保留一种编排形态**：派生（subtask）→ 并行 → 汇总。

**理由**：

这些机制是**模型不会自己分解任务那个时代的补丁** —— 用外部状态机模拟推理。它们的代价是真实的：

1. **抢走控制权**：外部 planner 一旦规划错，模型没有机会纠正，只能在错误框架里执行。
2. **增加失败面**：每个额外阶段都是一次额外的 LLM 调用，都有解析失败的可能。Zebra 的 `planner` / `critic` 模块大量代码在处理"模型没按格式输出怎么办"。
3. **上下文浪费**：议会式讨论消耗大量 token 换取边际收益，在长任务里加速触发压缩。
4. **不可观测**：多层嵌套的编排让"它到底在干什么"变得难以解释，而这恰恰是用户最需要的。

现代模型能自己规划、自己纠错、自己决定要不要拆任务。Zex 要做的是**别挡路**。

**如果模型表现得不会拆任务怎么办**：那是提示词和工具设计的问题，不是加一个 planner 能解决的。

**这条最容易被重新提议**，因为"多智能体协作"听起来很先进。当有人提的时候，请先回答：**它解决的具体问题是什么，为什么模型自己做不到？**

---

## ADR-007：权限分级是产品的一部分，不是安全补丁

**状态**：已采纳

**决定**：四档风险模型（L0 只读 / L1 可逆写 / L2 有影响 / L3 不可逆），默认档位为 L1。审批可记忆、可撤销、可审计。

**核心区分**：

> **能力上限** ≠ **默认权限**

用户要求"拿到整机全部控制权"，这个诉求本身是对的 —— 但它的正确解读是**能力不设天花板**，而不是**默认无阻执行**。

**否决的方案**：

- **全放行（真正的"全部控制权"）**：第一次 `rm -rf` 就结束。而且更现实的威胁不是模型犯错，是**提示注入** —— 一个读网页、读邮件的助理，被网页里的一段文字操纵去执行命令，这是当前攻击面最大的软件形态。
- **全面确认（每个操作都问）**：用户会在第二天关掉确认，然后授权全部、永不回顾。这比全放行更危险，因为它制造了虚假的安全感。
- **纯沙箱（容器 / VM）**：安全但丧失了"管理操作系统"这个核心能力。且用户的数据在主系统上。

**关键设计**：审批必须是**可记忆的**（否则疲劳）且**可撤销的**（否则等于永久放权）。见 `docs/SECURITY.md`。

---

## ADR-008：M0 先验证不确定性，不先写最爽的部分

**状态**：已采纳

**决定**：M0 第一件事是三平台 gpui hello world 冒烟，而不是写 agent loop。

**理由**：gpui-kit 是外部依赖，其 Linux/Wayland 支持、系统库依赖、构建时长都是**外部风险**，无法通过设计规避。而 agent loop 是**内部风险**，一定能写成。

先验证外部风险，因为它的失败会推翻整个技术选型；内部风险晚一点发现只是返工。

**推论**：M0 的验收标准是"三平台都能弹窗 + 一条窄路走通"，不是"功能齐全"。

---

## ADR-009：CLI 是与 GUI 平等的产物，不是附属品

**状态**：已采纳

**决定**：`zex-app` 同时产出 GUI（`zex`）和 headless（`zex-cli`）两个二进制，共享同一内核。

**理由**：

1. **常驻助理需要被自动化调用。** cron 任务、用户脚本、编辑器插件、CI —— 这些都只能调 CLI。
2. **调试成本。** 90% 的内核 bug 可以在 CLI 里复现，不需要开窗口、点按钮。
3. **它几乎是免费的**：因为 ADR-002 的双世界设计，内核不依赖 UI，CLI 只是一个 channel 的另一端。

**约束**：CLI 不得有独立于 GUI 的能力分支 —— 那会分裂成两个产品。CLI 支持的，GUI 必须支持；反之亦然。区别只在**审批交互的呈现方式**（GUI 弹窗 / CLI 阻塞等待 stdin）。

---

## ADR-010：LLM 层只写薄客户端，不用重抽象框架

**状态**：已采纳

**决定**：自写 `zex-llm`，覆盖 OpenAI 兼容与 Anthropic 两种线格式，约 300–500 行核心代码。不使用 `rig`、`llm`、`kalosm` 等 provider 抽象框架。

**理由**：

这类框架的价值在于"支持 30 家 provider"。而 Zex 只需 2 种格式 —— 而且通过可配置的 `base_url`，这 2 种格式实际覆盖了 Ollama、vLLM、LM Studio、one-api、OpenRouter 等绝大多数现实场景。

引入框架的代价是真实的：
- 抽象层泄漏（想用某个 provider 的特有能力时无处下手）
- 升级受制于人
- 调试要穿过别人的抽象

**自己写要处理的真实复杂度**（这些逃不掉，框架也帮不了）：
- SSE 解析的边界情况（chunk 分片、`data:` 前缀、`[DONE]` 标记、keep-alive 注释行）
- 工具调用序列修复（模型返回的 tool_calls 与 tool 消息必须严格配对，否则 API 报错）
- 流式中的 tool_calls 增量拼接（arguments 是分片到达的 JSON 字符串）
- 429 / 5xx / 超时的退避策略

**何时重新考虑**：如果未来要支持第 5 种格式，且手动适配开始重复。
