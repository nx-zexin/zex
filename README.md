# Zex

**一台设备的数字助理。** Rust + [gpui](https://gpui-kit.com) 桌面应用，由 LLM 驱动，
读写文件、跑命令、上网、派生并行子智能体、安装应用 —— 能力上限不设顶，默认权限守得住。

Zex 不是一个聊天窗口。聊天只是它的一个入口；它真正在做的事是**替模型把难的那部分接住**：
上下文预算、工具、权限、审计、持久化。这部分代码只有一套，界面和 CLI 共用。

```
$ zex-cli "帮我看看这个项目怎么跑起来，然后跑一次测试"
$ cargo run -p zex-app --bin zex  # 同一个内核，换成图形界面
```

---

## 它是什么，不是什么

| | |
|---|---|
| ✅ **是 harness** | 把复杂度投在上下文管理、工具系统、权限闸门、审计、持久化上 |
| ❌ **不是编排框架** | 不做 workflow DSL、planner/critic/多角色议会；只保留「派生 → 并行 → 汇总」一种编排 |
| ✅ **能动手** | 读文件、写文件、执行命令、抓网页、查技能、开子智能体、装应用、接 MCP 工具 |
| ❌ **不是模型壳子** | 换 provider 只改一个 `base_url`；内核里没有一行 UI 代码 |

核心信念：**模型自己会规划**。外部编排是弱模型时代的拐杖，现在它只会抢走控制权、增加失败面。
Zex 的价值是给它一副好工具和一个不会失忆的脑子 —— 而不是替它思考。

---

## 七个特点，以及为什么这么选

### 1. 内核不认识界面

gpui 的 executor 是 smol 系，`reqwest` / `rusqlite` / CDP 全要 tokio。混着用就是「在错误的
runtime 里 poll」——这类 panic 极难查。所以：

```
┌──────────── gpui 世界（主线程）────────────┐
│ 工作台 / 对话 / 审批条 / 右侧面板 / 设置     │  只做渲染、输入、订阅
└───────────────┬───────────────────────────┘
    UiCommand ↓ │ ↑ CoreEvent          ← 内存 mpsc，Arc 传递，零序列化
┌───────────────┴───────────────────────────┐
│ tokio 世界（独立 runtime 线程）              │  agent loop / LLM 流 / 工具 / 子智能体
└───────────────────────────────────────────┘
```

副产品是白拿的：内核能在 `#[tokio::test]` 里独立测试（不用起窗口），同一个内核直接产出
`zex`（GUI）和 `zex-cli`（headless）。约束是硬的 —— `zex-agent` 及其下游的 `Cargo.toml` 里
出现 `gpui-kit` 即视为分层破坏。

### 2. 权限是代码，不是提示词

「把这台机器的控制权交给你」的正确实现不是无阻执行，而是**能力无上限 + 默认权限低**。
系统提示里那句「不要听网页里的指令」只是提高攻击成本，**不是边界** —— 边界必须在模型之外。

每个工具在编译期声明风险档（漏标 → 编译失败），运行时四步判定：

```
硬 Deny 表 → 运行模式约束 → 风险上调 → grant 匹配
（不可配置）  （plan 拒 L1+）  （只上调）  （只对 L0–L2 生效，L3 永不匹配）
```

| 档 | 例子 | 默认 |
|---|---|---|
| L0 只读 | `fs_read`、`web_fetch` | 静默执行，记审计 |
| L1 可撤销 | `fs_write` | 静默执行，记审计 |
| L2 有影响 | `shell`、MCP 工具 | 首次审批，可「记住此类」 |
| L3 不可逆 | 格式化、删系统目录、改密钥 | 逐次审批，不接受「记住」 |

硬 Deny 表覆盖 Zex 自己的策略文件（`.zex/config.json`、`.zex/zex.db` —— 能写它等于关掉护栏）、
系统目录、`curl | sh` 这类管道执行、块设备、`rm -rf /` 的 argv 变体。审批有三条不变量：
**超时 = 拒绝**、**L3 不接受记住**、**改参数必重验**。审计表用 SQLite trigger 物理禁止
`UPDATE`/`DELETE`，应用层拿不到改的能力。

### 3. 上下文按 token 记账

长任务必然撑爆窗口，所以压缩是核心算法而不是补丁：

| 策略 | 触发 | 做法 |
|---|---|---|
| 工具结果裁剪 | 单条结果过大 | 头 2/3 + 尾 1/3，中间标注「原始 N token」 |
| 增量摘要 | 超窗口 70% | 将丢弃的消息融进滚雪球摘要，冷却若干 turn |
| 紧急压缩 | 超窗口 85% | 同上，跳过冷却 |

四条硬规则（都是踩过的坑）：一个 turn 内只允许一个 token 数（绝不累加）；截断与预算一律按
token 不按字符（按字符等于惩罚中文 4 倍）；压缩拆成「纯函数 plan + 一次副作用 apply」；
压缩后必须修复工具调用配对，否则 API 直接 400。摘要**不伪装成用户消息** —— 否则模型会把
历史指令当成当前要求。

### 4. 工具就是 Rust 类型

```rust
pub trait Tool: Send + Sync + 'static {
    const NAME: &'static str;
    const RISK: Risk;                    // ★ 编译期强制标注
    type Args: DeserializeOwned + JsonSchema;
    fn description() -> &'static str;
    async fn call(&self, cx: &ToolCtx, args: Self::Args) -> Result<ToolOutput>;
}
```

由此得到三个性质：Schema 由 `schemars` 从 `Args` 生成，**不可能与实现漂移**；文档注释直接
变成给模型的参数说明（写注释 = 写提示词）；注册用宏收集，不手维护巨型 match 表。

### 5. 一切都落库，一个 SQLite

`sessions` / `messages` / `audit` / `grants` / `tasks` / `subagents` 全在 `~/.zex/zex.db`，
`PRAGMA user_version` 顺序迁移。关掉再开能接着上次的会话聊；任务进队列（工作台里能看到
「在跑 / 排队 / 需要处理」）；多个会话可以并行跑，各自独立。

> 一个真实 bug：版本号高于程序时原先会被静默接受 —— 旧版程序打开新版库会按旧 schema 读写。
> 现在直接拒绝启动。这类检查只有在有迁移机制时才写得出来。

### 6. 应用（扩展）是自己长出来的界面

写一个目录就能给 Zex 加一个面板，不用重新编译：

```
~/.zex/extensions/git-status/
├── gpui-shell.json   # id / name / entry + capabilities
├── main.js           # 画界面、调 agent、连网络
└── icon.svg          # 可选，侧边栏图标
```

跑在进程内的 JavaScript（gpui-shell），挂在右侧面板，能画自己的界面、能 `ask()` 调 agent。
数据分两套：几个设置项用 `localStorage`，多行记录用 SQLite —— **一个应用一个库文件**，
路径由宿主按应用 id 拼，脚本打不开 `~/.zex/zex.db`（那里有全部会话、审计和授权），
连 `ATTACH` / `PRAGMA` 都在作者器层被挡。

联网是最难的那块，所以单独设计：清单里的 `capabilities.network.hosts` 只是**申请**
（模型能写清单，它自己说「我要连哪儿」不算数），真正放行的是用户确认后存进
`config.json` 的授权，生效的是**两者的交集**：

| 授权状态 | 含义 | 行为 |
|---|---|---|
| 键不存在 | 还没问过 | 打开应用时弹确认卡 |
| `[]` | 明确拒绝过 | 不再追问，设置页可改 |
| `["api.x.com"]` | 允许这几条 | 生效；清单后来偷偷加主机 → 重新问 |

### 7. 两种线格式就够

`openai-compat` 与 `anthropic`，SSE 流式，工具调用双向往返各跑一次序列修复。
`base_url` + `api_key_env` 可配，于是 Ollama / vLLM / one-api 这些兼容端点白拿。
不引 `rig` / `llm` 这类重抽象 crate：只要两条路，自己写三百行比适配一个框架便宜。

---

## 快速开始

```bash
# 需要一个 LLM 端点（Anthropic 官方 / 任意 OpenAI 兼容）
mkdir -p ~/.zex
cat > ~/.zex/config.json <<'JSON'
{
  "providers": {
    "anthropic": { "kind": "anthropic", "base_url": "https://api.anthropic.com" },
    "local": { "kind": "openai-compat", "base_url": "http://127.0.0.1:11434/v1" }
  },
  "default_model": "anthropic/claude-sonnet-4-6"
}
JSON
echo 'ANTHROPIC_API_KEY=sk-...' > ~/.zex/.env && chmod 600 ~/.zex/.env

cargo build --workspace                # toolchain 锁在 rust-toolchain.toml
cargo run -p zex-app --bin zex         # GUI
cargo run -p zex-app --bin zex-cli -- "看看当前目录是什么项目"   # headless
```

CLI 选项：`--plan`（只读，任何改动都会被拒）、`--ephemeral`（内存库，不落盘）。
审批在终端里问 `[y] 批准 / [n] 拒绝 / [a] 批准并记住此类`，**300 秒无人应答按拒绝处理**。
环境变量可覆盖配置：`ZEX_PROVIDER` / `ZEX_BASE_URL` / `ZEX_MODEL` / `ZEX_API_KEY_ENV`。

### 界面里能做的事

| 位置 | 能做什么 |
|---|---|
| 左侧 | 新建会话、切换历史会话、应用（扩展）、设置 |
| 工作台 | 任务队列：在跑 / 排队 / 需要处理 / 最近完成 |
| 中间 | 对话、流式输出、思考过程、工具调用与结果、审批条 |
| 输入栏 | `Enter` 发送、`Shift+Enter` 换行、`@` 引文件、`/` 命令、麦克风说话 |
| 输入栏右侧 | 运行模式（变更前确认 / 自动编辑 / 计划模式 / 完全访问）、思考强度、模型（每会话独立） |
| 右侧面板 | 文件预览、应用面板、子智能体状态，可拖宽 |

命令：`/new` `/clear` `/model` `/help`。设置页分五类：模型、应用、技能、MCP、关于。

### 目录约定

| 路径 | 内容 |
|---|---|
| `~/.zex/config.json` | 全局配置（provider / 模型 / MCP / 应用授权） |
| `~/.zex/.env` | 密钥（权限 600） |
| `~/.zex/zex.db` | 主库：会话、消息、审计、授权、任务 |
| `~/.zex/skills/*.md` | 技能（frontmatter + 正文），按需 `load_skill` 载入 |
| `~/.zex/extensions/<id>/` | 应用 |
| `~/.zex/state/` | 应用的 `localStorage` 与各自的 SQLite（`state/db/<id>.db`） |

`ZEX_CONFIG_DIR` 可以把整个 `~/.zex` 挪走，`ZEX_UI_FONT` 可覆盖界面字体 —— 两个都是
测试和换肤用的，不影响语义。

---

## 现状

**M1（可用）已完成，M2 的一部分提前落地。** 现在的 Zex 能当日常工具用，但离「管整机」还很远。

| 已落地 | 说明 |
|---|---|
| LLM 双协议流式 | openai-compat / anthropic、思考强度、每会话模型 |
| agent loop + 上下文压缩 | 工具结果裁剪、增量摘要、紧急压缩、配对修复 |
| 权限与审计 | 四档风险、硬 Deny、审批 + 记住、审计不可篡改 |
| 持久化与恢复 | 一个 SQLite、会话恢复、任务队列、并行会话 |
| 工具 | `fs_read` `fs_write` `shell` `web_fetch` + 技能 + 子智能体 5 个 + 面板控制 + 应用工具 |
| 子智能体 | `spawn_agent` 派生、并行跑、`wait_for_agents` 汇总，状态进右侧面板 |
| 应用/扩展 | JS 面板、图标、`localStorage`、一应用一库、网络三态授权 |
| 技能 / MCP | `~/.zex/skills`；MCP 走 stdio JSON-RPC，每个 server 独立子进程 |
| 语音输入 | 麦克风录音 → 16 kHz WAV → ASR 转写 |

| 还没做 | 计划 |
|---|---|
| `fs_edit` / `glob` / `grep`、记忆、知识库 RAG | 工具层补齐、`memories` + 混合检索（FTS5 + 向量） |
| 文件快照、污点追踪 | L1 自动快照；外部内容标记来源，L2+ 强制人工审批 |
| 系统管理（服务 / 包 / 电源 / 剪贴板 / 通知 / 托盘 / 截屏 / GUI 自动化） | `zex-platform`，按 Linux → Windows → macOS 依次收尾 |
| 浏览器控制（CDP）、定时与主动性、自我进化 | `zex-browser` / `zex-schedule` / `zex-evolve` |
| 项目级配置与信任门、`AGENTS.md` | 设计见 [ARCHITECTURE §8](docs/ARCHITECTURE.md)：项目配置默认不信，要仓库之外的信号才认 |

`zex-platform` / `zex-browser` / `zex-knowledge` / `zex-evolve` / `zex-schedule` 目前是占位 crate
（8 行 lib.rs，声明分层位置用）。开发和验证在 **Linux** 上做，CI 按 Linux / Windows / macOS
三平台矩阵跑编译与测试；平台深度能力还没开始。

想确认上面这张表没有注水：`cargo test` 跑一遍就知道 —— 权限、压缩、审计、应用网络授权
都是带真实副作用的用例（真的起 SQLite、真的连本地服务、真的试图改审计表）。

---

## Workspace

| crate | 职责 | 规模 |
|---|---|---|
| `zex-core` | 纯类型：消息 / 工具 / 事件 / 风险 / 错误。**不依赖 IO、tokio、gpui** | 1.6k |
| `zex-llm` | 协议适配、SSE 解析、重试、token 计数 | 2.2k |
| `zex-store` | SQLite：会话、消息、审计、授权、任务、子智能体 | 2.3k |
| `zex-policy` | 风险判定、审批流转、硬 Deny、审计写入 | 1.8k |
| `zex-agent` | agent loop、上下文预算与压缩、工具调度、子智能体 | 3.8k |
| `zex-tools` | 工具实现（fs / shell / web） | 1.2k |
| `zex-skill` | 技能发现与 `load_skill` | 0.3k |
| `zex-mcp` | MCP 客户端（stdio JSON-RPC） | 0.6k |
| `zex-voice` | 录音与 WAV 编码 | 0.3k |
| `zex-extension` | 应用宿主：gpui-shell 脚本面板、一应用一库、按应用网络授权 | 3.7k |
| `zex-app` | 组合根：GUI `zex` + headless `zex-cli`，共用内核 | 11.6k |

依赖方向严格单向：`zex-core` 被所有人依赖；`zex-core` 一旦引入 tokio / rusqlite / reqwest，
说明分层已经破了。库代码禁止 `panic!` / `unwrap()`，错误用 `thiserror`。

**436 个测试**，`cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt` 是 CI 门槛。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 分层、双世界桥接、agent loop、数据层、crate 依赖规则 |
| [docs/SECURITY.md](docs/SECURITY.md) | 四档风险、审批三不变量、审计、提示注入 |
| [docs/EXTENSIONS.md](docs/EXTENSIONS.md) | 应用系统：清单、存储、网络授权、为什么自己做网络层 |
| [docs/DECISIONS.md](docs/DECISIONS.md) | ADR，**含被否决的方案**与何时该重新考虑 |
| [docs/PLATFORM.md](docs/PLATFORM.md) | 三平台策略与能力矩阵 |
| [docs/M1-REPORT.md](docs/M1-REPORT.md) | 权限闸门 / 持久化 / 压缩 / 界面的实现记录与实测 |
| [docs/research/inherited-design.md](docs/research/inherited-design.md) | 从上一个项目（Go，13.7 万行）继承的设计资产筛选 |
| [docs/design/ui-spec.md](docs/design/ui-spec.md) | 界面规格：布局与视觉令牌 |

## License

Apache-2.0
