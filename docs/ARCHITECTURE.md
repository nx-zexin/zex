# Zex 架构

> 本文档描述 Zex 的整体结构与硬性约束。改架构先改本文档。

---

## 1. 总纲

Zex 是一个 **harness**，不是编排框架。

三条不可动摇的判断：

1. **模型自己会规划。** 外部编排（workflow / planner / critic / 多角色议会）是弱模型时代的拐杖，现在只会抢走控制权、增加失败面。Zex 只保留"派生 → 并行 → 汇总"这一种编排形态。
2. **该继承的是设计资产，不是代码资产。** 旧项目 Zebra（Go，13.7 万行）里真正要带走的是文件格式、协议、阈值、踩坑教训 —— 见 `docs/research/inherited-design.md`。
3. **能力上限要高，默认权限要低。** 这两件事必须分开，否则第一天就被自己 `rm -rf`。

---

## 2. 双世界桥接

整个架构里最容易翻车的地方，先说清。

gpui 的 async executor 是 smol 系；`reqwest` / `tokio::fs` / CDP 都要 tokio。混用会产生"在错误 runtime 里 poll"的运行时恐慌。

**解法：内核是一个 tokio 世界，UI 是一个 gpui 世界，两者用 typed channel 桥接。**

```
┌──────────────── gpui 世界（主线程）────────────────┐
│  Entity<ChatView> / Entity<ApprovalView> / Tray     │
└────────────────────┬───────────────────────────────┘
                     │  UiCommand  ↓ (mpsc::Sender)
                     │  CoreEvent  ↑ (mpsc::Receiver)
┌────────────────────┴───────────────────────────────┐
│  tokio 世界（独立 runtime 线程）                     │
│  agent loop / LLM 流 / 工具 / 定时器 / CDP           │
└────────────────────────────────────────────────────┘
```

### 硬性规则

- **不是 IPC。** 是内存 channel，`Arc<CoreEvent>` 传递，零序列化、零拷贝。
- **内核不得依赖 gpui。** `zex-agent` 及其下游的 `Cargo.toml` 里出现 `gpui-kit` 即为分层破坏，CI 检查。
- **UI 不得直接调用内核的 async 函数。** 只能 `send(UiCommand)`。
- **`CoreEvent` 必须是可 `Clone` 的纯数据**，不含 `Entity` / `Window` 等 gpui 类型。

### 为什么这么设计

副产品就是你白拿到的东西：内核可以在 `#[tokio::test]` 里独立测试，不需要启动 UI；同一个内核直接产出 `zex-cli`（headless），供脚本和 cron 调用。

### 事件流示例

```rust
// zex-core/src/event.rs
#[derive(Debug, Clone)]
pub enum CoreEvent {
    TurnStarted { session: SessionId, turn: TurnId },
    Token        { turn: TurnId, text: String },
    Thinking     { turn: TurnId, text: String },
    ToolCallBegin{ turn: TurnId, call: ToolCall },
    ToolCallEnd  { turn: TurnId, call_id: String, ok: bool, brief: String },
    ApprovalReq  { req: ApprovalRequest },
    TurnEnded    { turn: TurnId, reason: EndReason },
    Error        { message: String, fatal: bool },
}

#[derive(Debug, Clone)]
pub enum UiCommand {
    UserPrompt   { text: String, attachments: Vec<Attachment> },
    Approve      { req_id: ApprovalId, remember: Option<RememberScope> },
    Reject       { req_id: ApprovalId, reason: String },
    Cancel       { turn: TurnId },
    Configure    { change: ConfigChange },
}
```

---

## 3. Workspace 划分

```
zex/
├── Cargo.toml                  # [workspace]
├── rust-toolchain.toml
├── crates/
│   ├── zex-core/               # 纯类型：Message/Tool/Event/Risk/Error。零 IO
│   ├── zex-llm/                # LLM 客户端：openai-compat + anthropic 双协议，SSE 流式
│   ├── zex-store/              # SQLite：会话/记忆/知识库/审计。唯一数据入口
│   ├── zex-policy/             # 权限分级、审批队列、熔断、审计写入
│   ├── zex-agent/              # agent loop、上下文预算、压缩、工具调度
│   ├── zex-tools/              # 工具实现
│   ├── zex-platform/           # 三平台抽象 trait + cfg 实现
│   ├── zex-browser/            # CDP 客户端 + 浏览器生命周期
│   ├── zex-knowledge/          # 切块、embedding、混合检索
│   ├── zex-evolve/             # distill / 提案 / 验证 / 落盘
│   ├── zex-schedule/           # cron + 事件触发 + 主动性
│   └── zex-app/                # GPUI UI + 组合根（bin: zex, zex-cli）
```

### 依赖方向（严格单向）

```
zex-core   ←  所有层
zex-store  ←  zex-agent, zex-knowledge, zex-evolve, zex-policy
zex-policy ←  zex-tools
zex-platform ← zex-tools, zex-app
zex-agent  ←  zex-app
```

**`zex-core` 一旦引入 tokio / rusqlite / reqwest，说明分层已经破了。**

CI 用 `cargo-deny` 的自定义规则 + 一个脚本检查 `crates/zex-core/Cargo.toml` 的依赖白名单。

### 各 crate 职责边界

| crate | 该做 | 不该做 |
|---|---|---|
| `zex-core` | 类型定义、错误类型、纯函数 | 任何 IO、任何 async |
| `zex-llm` | 协议适配、SSE 解析、重试、token 计数 | 决定"该不该调用" |
| `zex-store` | SQL、迁移、事务 | 业务规则 |
| `zex-policy` | 风险判定、审批流转、审计写入 | 执行工具 |
| `zex-agent` | loop、上下文组装、压缩、分发 | 具体工具实现 |
| `zex-tools` | 每个工具自包含 | 自己判权限（一律走 policy） |
| `zex-platform` | 三平台能力抽象 | 平台专属业务逻辑 |
| `zex-app` | UI、组合、生命周期 | 任何可被 CLI 复用的逻辑 |

---

## 4. Agent Loop

刻意做薄。**一个 while 循环 + 一次 LLM 调用 + 工具分发 + 压缩判断。** 没有状态机。

```
loop {
    ctx    = context.build(budget)?            // 系统提示 + 记忆召回 + 历史 + 工具定义
    stream = llm.stream(ctx).await?            // 逐 token 发 CoreEvent::Token
    resp   = stream.finish().await?            // 拿到完整 assistant 消息

    if resp.tool_calls.is_empty() {
        break resp.text
    }

    for call in resp.tool_calls {              // 可配置并发度，默认串行
        decision = policy.gate(&call).await?;
        result = match decision {
            Allow => tools.dispatch(call).await?,       // 记审计
            Ask   => wait_user(call).await?,            // 挂起，等 UI 回传
            Deny  => Denied { reason },
        };
        history.push(result);
        checkpoint()?;                              // 断点续跑
    }
}
```

### 上下文管理（本项目的核心算法）

| 策略 | 触发 | 动作 |
|---|---|---|
| 工具结果裁剪 | 单条 > 8k token | 头尾保留，中间省略并标注 |
| 滑动窗口 | 超预算 70% | 从最旧的非关键消息开始丢 |
| 摘要压缩 | 超预算 85% | 早期对话摘要成一条 system 消息 |
| 记忆置换 | 超预算 95% | 早期内容转 memory 写库，正文丢弃 |
| 断点续跑 | 进程退出 | 每次工具调用后 checkpoint，重启从最后一致点恢复 |

阈值初值取自 Zebra 的实测经验，见 `docs/research/inherited-design.md`。**阈值是可调参数，不是硬编码。**

### 什么不该出现在 loop 里

- 任务分解 → 模型自己决定要不要调 `subtask` 工具
- 自我批评 → 模型自己决定
- 重试策略 → 放 `zex-llm`，按错误类型决定（429 退避、5xx 重试、4xx 不重试）

---

## 5. 工具系统：类型即 Schema

```rust
// zex-core/src/tool.rs
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,   // 由 schemars 生成
    pub risk: Risk,
}

pub trait Tool: Send + Sync + 'static {
    const NAME: &'static str;
    const RISK: Risk;                       // ★ 编译期强制标注
    type Args: DeserializeOwned + JsonSchema;

    fn description() -> &'static str;
    async fn call(&self, cx: &ToolCtx, args: Self::Args) -> Result<ToolOutput>;
}
```

### 由此得到的三个性质

1. **Schema 自动生成**（`schemars::schema_for!(Args)`），不可能与实现漂移。
   文档注释直接变成给模型的参数说明 —— **写注释就是在写给模型看的提示词。**

2. **`RISK` 是编译期常量。** 新增工具忘标风险 → 编译失败，而不是运行时静默按 L0 放行。
   这一条能防掉绝大多数权限事故。

3. **注册用宏自动收集**，不手维护巨型 match 表。

### 工具清单（M0 只做 5 个）

| M0 | 工具 | Risk |
|---|---|---|
| ✅ | `fs_read` | ReadOnly |
| ✅ | `fs_write` | Reversible |
| ✅ | `shell` | Impactful |
| ✅ | `web_fetch` | ReadOnly |
| ✅ | `ask` | ReadOnly（向用户提问） |
| M1 | `fs_edit` `glob` `grep` | ReadOnly / Reversible |
| M2 | `memory_*` `skill_*` | Reversible |
| M3 | `os_*` `browser_*` | Impactful / Destructive |
| M4 | `knowledge_*` | ReadOnly |

---

## 6. 数据层

**一个 SQLite 库。** 会话、记忆、知识库、审计、授权、定时任务全在里面。

理由：Zebra 是 `memory.db` + 一堆 JSON + 独立 ZebraKnowledge 服务，碎片化导致备份/迁移/一致性全是坑。助理的数据层越简单越好：**一个库、一套迁移、一个备份点。**

```sql
sessions(id, title, created_at, updated_at, model, cwd, parent_id)
messages(id, session_id, role, content, tool_calls, tool_call_id, tokens, created_at)

memories(id, scope, kind, title, body, importance, created_at, accessed_at, hits)
memories_fts USING fts5(title, body, content=memories)
memories_vec USING vec0(embedding float[1024])

skills(name, description, body, path, version, created_at)

kb_docs(id, source, path, hash, mtime, meta)
kb_chunks(id, doc_id, ord, content)
kb_chunks_fts USING fts5(content, content=kb_chunks)
kb_chunks_vec USING vec0(embedding float[1024])

audit(id, ts, kind, risk, tool, args_digest, result, approved_by, session_id)
grants(id, pattern, risk, granted_at, expires_at, revoked_at)
jobs(id, kind, schedule, prompt, enabled, last_run, next_run)
```

### 四条硬性要求

1. **`memories` / `kb_chunks` 同时有 FTS5 和向量索引**，检索走混合排序（BM25 + cosine，RRF 融合）。
   纯向量在"精确名词"上很差，纯全文在"语义"上很差。混合是必须的。

2. **`audit` 表用 SQLite trigger 禁止 UPDATE/DELETE。** 应用层拿不到改的能力 —— 这是审计可信的物理保证。

3. **`grants` 是权限的记忆。** 用户说"以后别再问"就写一行；UI 里能列出、能单独撤销。

4. **迁移从 M0 就上。** 不要等到有用户数据才补。用 version 表 + 顺序脚本。

---

## 7. LLM 层

只支持两种线格式：

| Provider | 端点约定 | 流式 | 工具调用 |
|---|---|---|---|
| `openai-compat` | `POST {base}/chat/completions` | SSE `data: {...}` + `[DONE]` | `tool_calls` / `role:"tool"` |
| `anthropic` | `POST {base}/v1/messages` | SSE `event: content_block_delta` | `tool_use` / `tool_result` |

**不引入 `rig` / `llm` 等重抽象 crate。** 它们的 provider 矩阵对 Zex 是负资产 —— 我们只要两条路，自己写 300 行比适配一个框架的抽象层便宜。

### 统一内部表示

```rust
pub enum Message {
    System(String),
    User { content: Vec<Part> },
    Assistant { content: Vec<Part>, tool_calls: Vec<ToolCall> },
    Tool { call_id: String, content: String },
}

pub enum Part { Text(String), Image(ImageRef), Thinking(String) }
```

协议适配层负责 `Message ⇄ 线格式` 双向映射，并在两端都跑一次**工具调用序列修复**（Zebra 有踩坑记录）。

### 配置

```json
{
  "providers": {
    "anthropic": { "kind": "anthropic", "base_url": "https://api.anthropic.com", "api_key_env": "ANTHROPIC_API_KEY" },
    "local":     { "kind": "openai-compat", "base_url": "http://127.0.0.1:11434/v1", "api_key_env": null }
  },
  "default_model": "anthropic/claude-sonnet-4-6"
}
```

`base_url` + `api_key_env` 可配置，就白拿到了对 Ollama / vLLM / one-api 等所有兼容端点的支持。

---

## 8. 目录约定

| 路径 | 内容 |
|---|---|
| `~/.zex/config.json` | 全局配置 |
| `~/.zex/.env` | 密钥（权限 600） |
| `~/.zex/zex.db` | 主数据库 |
| `~/.zex/skills/<name>/skill.md` | 技能 |
| `~/.zex/memories/*.md` | 记忆（写穿，与 DB 同步） |
| `<project>/.zex/` | 项目级配置 |
| `<project>/AGENTS.md` | 项目指导（兼容生态惯例） |

### 项目级配置信任门（继承 Zebra 教训）

项目级配置**默认不受信任**，忽略并告警。理由：克隆来的仓库可借 `config.json` 劫持 API 端点（把密钥发到攻击者服务器）、借 `.env` 注入 `PATH` / `LD_PRELOAD`、借 `skills/` 执行任意命令。

授权方式（信号必须来自仓库之外）：
- `export ZEX_TRUST_PROJECT=1`
- 或把项目绝对路径写入 `~/.zex/trusted-projects`（每行一个，`#` 注释）

两条**始终生效**的例外规则：
- 项目级配置不得覆盖内置 provider 的 `base_url` / `headers`
- 项目级 `.env` 不得设置进程控制变量（`PATH`、`LD_*`、`DYLD_*`、`NODE_OPTIONS`、`GIT_SSH_COMMAND` 等）

---

## 9. 代码规范

- **格式化**：`cargo fmt`，CI 强制。
- **Lint**：`cargo clippy --workspace --all-targets -- -D warnings`，CI 强制。
- **错误**：库代码禁止 `panic!` / `unwrap()` / `expect()`（测试和 `main` 除外）。
  用 `thiserror` 定义错误类型，`anyhow` 只在 `zex-app` 边界使用。
- **文件**：单文件 ≤ 500 行，单函数 ≤ 100 行。超了拆。
- **测试**：新功能必须配测试。`zex-core` / `zex-llm` / `zex-policy` 目标覆盖率 > 60%。
- **文档**：公开 API 必须有文档注释 —— **因为文档注释会变成工具 schema 和提示词。**

---

## 10. 演进路线

| 阶段 | 周期 | 目标 |
|---|---|---|
| **M0 骨架** | 1–2 周 | workspace + gpui 壳 + 双协议 LLM + agent loop + 5 工具 + 流式 UI + CLI |
| **M1 可用** | 3–4 周 | 会话持久化、上下文压缩、权限闸门 + 审批 UI、审计 |
| **M2 助理化** | 4–6 周 | 记忆、技能、cron + 常驻 + 托盘、远程通道 |
| **M3 接管设备** | 4–6 周 | OS 管理（三平台）、浏览器 CDP、屏幕感知 |
| **M4 进化** | 4–6 周 | 知识库 RAG、自我进化循环 |
| **M5 打磨** | — | 打包、自动更新、性能、文档 |
