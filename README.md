# Zex

**一台设备的数字助理（Digital Assistant）** —— Rust + [gpui-kit](https://gpui-kit.com) 桌面应用。

Zex 拿到整机控制权：管理操作系统、操作浏览器、读写你的文件、记住你的习惯、在你不看着的时候也能主动做事。它由 LLM 驱动，只支持 **OpenAI 兼容** 与 **Anthropic** 两种 API 格式。

---

## 它是什么，不是什么

| | |
|---|---|
| ✅ **是一个 harness** | 把复杂度投在上下文管理、工具、权限、审计、持久化上 |
| ❌ **不是编排框架** | 不做 workflow DSL、不做多角色议会、不做 planner/critic/orchestrator |
| ✅ **常驻** | 随系统登录启动，托盘待命，能定时、能感知事件、能主动汇报 |
| ❌ **不是一个聊天窗口** | 聊天只是它的一个入口，不是它的本体 |

核心信念：**模型能自己规划，外部编排只会抢走它的控制权并增加失败面。** Zex 的价值在于给它一副好工具和一个不会失忆的脑子 —— 而不是替它思考。

---

## 架构一图

```
┌──────────────── gpui 世界（主线程）────────────────┐
│  Entity<ChatView> / Entity<ApprovalView> / 托盘      │
│  只做：渲染、输入、订阅事件                           │
└────────────────────┬──────────────────────────────┘
                     │  UiCommand (mpsc)  ↓
                     │  CoreEvent  (mpsc)  ↑     ← 内存 channel，零序列化
┌────────────────────┴──────────────────────────────┐
│  tokio 世界（后台 runtime）                          │
│  agent loop / LLM 流 / 工具执行 / 定时器 / CDP       │
└───────────────────────────────────────────────────┘
```

内核不知道 UI 存在。所以同一个内核同时产出 `zex`（GUI）与 `zex-cli`（headless）。

详见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

---

## 快速开始

```bash
# 首次：验证三平台构建链
cargo check --workspace

# 运行 GUI
cargo run -p zex-app

# 运行 headless（不需要 UI）
cargo run -p zex-app --bin zex-cli -- "帮我看看当前目录是什么项目"
```

---

## 文档索引

| 文档 | 内容 |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 分层、双世界桥接、crate 依赖规则 |
| [docs/DECISIONS.md](docs/DECISIONS.md) | 架构决策记录（ADR），含被否决的方案 |
| [docs/PLATFORM.md](docs/PLATFORM.md) | 三平台策略、能力矩阵、CI 约束 |
| [docs/SECURITY.md](docs/SECURITY.md) | 权限分级、审批、审计、提示注入防护 |
| [docs/M0-PLAN.md](docs/M0-PLAN.md) | M0 逐项任务与验收标准 |
| [docs/research/inherited-design.md](docs/research/inherited-design.md) | 从 Zebra 继承的设计资产筛选 |

---

## 状态

**M0（骨架）进行中。** 见 [docs/M0-PLAN.md](docs/M0-PLAN.md)。

## License

Apache-2.0
