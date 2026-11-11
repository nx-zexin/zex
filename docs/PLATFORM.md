# 三平台策略

> 目标：**一份 Rust 代码，Linux / Windows / macOS 三平台最终能力等价。**
> 手段：把平台差异变成每天都要过的 CI 关卡，而不是收尾任务。

---

## 1. 平台承诺的准确含义

"三平台同时支持"容易产生两种误解，先澄清：

| 误解 | 实际承诺 |
|---|---|
| 三个平台同时遇到同一个坑、同时解决 | ✅ 三个平台**从 M0 起就能编译能运行**，CI 每次提交三平台全绿 |
| 三个平台的功能在同一个时刻全部完成 | ❌ 平台**深度能力**（GUI 自动化、系统管理）按平台依次收尾 |

**为什么深度能力不能完全并行**：macOS 的 TCC 授权（辅助功能 / 屏幕录制 / 完全磁盘访问）+ 公证签名 + `CGEvent` 权限，是 3–4 周的纯审批流程工作；Windows 的 UAC 提升、SmartScreen、EV 证书同理。这些**不是写代码能加速的**，它们受制于平台方的审核节奏。

**执行顺序**（M3 阶段）：

```
Linux 完整 ──→ Windows 完整 ──→ macOS 完整
（开发机，最快验证）  （发布主战场）   （授权流程最长，提前启动申请）
```

> ⚠️ **macOS 的开发者账号、签名证书、公证流程建议在 M1 就开始申请**，不要等 M3 写完代码才发现签不出来。

---

## 2. 能力矩阵

| 能力 | Linux | Windows | macOS | M 阶段 |
|---|---|---|---|---|
| 文件读写 | `std::fs` + `nix` | `std::fs` + `windows` | `std::fs` + `objc2` | M0 |
| 进程管理 | `sysinfo` / `/proc` | `sysinfo` / Toolhelp32 | `sysinfo` / `libproc` | M3 |
| Shell 执行 | `sh -c` / `bash` | `cmd /c` / PowerShell | `sh -c` / zsh | M0 |
| 服务管理 | systemd | SCM | launchd | M3 |
| 包管理 | apt / dnf / pacman / zypper | winget | brew | M3 |
| 开机自启 | `.desktop` / systemd user | 注册表 Run / 计划任务 | LaunchAgent | M2 |
| 电源控制 | `systemctl` / `loginctl` | `SetSuspendState` | `pmset` | M3 |
| 剪贴板 | `wl-clipboard` / `xclip` | `windows` crate | `NSPasteboard` | M2 |
| 原生通知 | `notify-rust` (D-Bus) |  toast | `UNUserNotificationCenter` | M2 |
| 托盘 | `ksni` (StatusNotifier) | `Shell_NotifyIcon` | `NSStatusItem` | M2 |
| 截屏 | X11 / `grim`(wl) / `spectacle` | `Windows.Graphics.Capture` | `CGWindowListCreateImage` | M3 |
| GUI 自动化 | AT-SPI2 (`atspi`) | UI Automation | AXUIElement (`objc2`) | M3 |
| 全局热键 | X11 grab / portal | `RegisterHotKey` | `CGEventTap` | P2 |
| 麦克风 | `cpal` | `cpal` | `cpal` | P1 |
| 浏览器 CDP | ✅ 统一 | ✅ 统一 | ✅ 统一 | M3 |

**三平台完全一致的**：LLM 层、agent loop、工具调度、SQLite 存储、CDP 协议、权限审计。这些占代码量的 70%，且不受平台影响 —— 这是选 Rust 而不是"三套原生"的核心收益。

---

## 3. 抽象层设计

`zex-platform` 每个能力一个 trait，`#[cfg]` 三份实现。

```rust
// zex-platform/src/lib.rs

pub trait SystemControl: Send + Sync {
    fn snapshot(&self) -> Result<SystemSnapshot>;
    fn processes(&self) -> Result<Vec<ProcessInfo>>;
    fn kill(&self, pid: u32, force: bool) -> Result<()>;
    fn power(&self, action: PowerAction) -> Result<()>;
    /// 平台是否支持此能力。不支持时上层隐藏 UI，而不是弹错误。
    fn capabilities(&self) -> CapabilitySet;
}

pub trait ServiceControl: Send + Sync {
    fn list(&self) -> Result<Vec<ServiceInfo>>;
    fn start(&self, id: &str) -> Result<()>;
    fn stop(&self, id: &str) -> Result<()>;
    fn enable_at_login(&self, id: &str, on: bool) -> Result<()>;
}

pub trait GuiAutomation: Send + Sync {
    fn windows(&self) -> Result<Vec<WindowInfo>>;
    fn tree(&self, win: WindowId) -> Result<AxNode>;
    fn invoke(&self, node: &NodeId, action: Action) -> Result<()>;
    fn input(&self, ev: InputEvent) -> Result<()>;
}

pub trait Screenshot: Send + Sync {
    fn capture(&self, area: Option<Rect>) -> Result<Image>;
}

pub trait PackageManager: Send + Sync {
    fn name(&self) -> &'static str;          // "pacman" / "winget" / "brew"
    fn search(&self, query: &str) -> Result<Vec<PackageInfo>>;
    fn install(&self, id: &str) -> Result<()>;
}
```

### 三条硬性规则

1. **不支持的能力返回 `Err(Capability::Unsupported)`，永不 panic、永不 `todo!()`。**
   `todo!()` 在三平台代码里是最危险的宏 —— 它在 Linux 上永远不会跑到，在 Windows 上一跑就崩。
   CI 用 `clippy::todo` + 自定义 grep 拦截。

2. **上层必须调 `capabilities()` 决定是否展示入口**，而不是尝试后捕获错误。
   用户不该看到一个"点了没反应"的按钮。

3. **平台实现只做机制，不做策略。** 比如"删文件前要不要确认"是 `zex-policy` 的事，`zex-platform` 只负责"能不能删"。

### 目录约定

```
crates/zex-platform/src/
├── lib.rs           # trait 定义 + re-export
├── traits.rs        # 各能力 trait
├── capability.rs    # CapabilitySet
├── fs.rs            # 跨平台通用的部分直接写这里
├── sys/
│   ├── mod.rs       # #[cfg] 分发
│   ├── linux.rs
│   ├── windows.rs
│   └── macos.rs
├── gui/
│   ├── mod.rs
│   ├── linux_atspi.rs
│   ├── windows_uia.rs
│   └── macos_ax.rs
└── screen/
    ├── mod.rs
    ├── linux.rs
    ├── windows.rs
    └── macos.rs
```

---

## 4. CI 是唯一能防止"三平台"变成口号的东西

```yaml
# .github/workflows/ci.yml
strategy:
  fail-fast: false          # ★ 关键：不要一个平台挂了就取消其他平台
  matrix:
    os: [ubuntu-latest, windows-latest, macos-latest]
```

每个平台必跑：

| 步骤 | 命令 |
|---|---|
| 格式 | `cargo fmt --all -- --check` |
| 编译 | `cargo check --workspace --all-targets` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| 测试 | `cargo test --workspace` |
| 分层 | 自定义脚本检查 `zex-core` 依赖白名单 |

### 铁律

> **任何 commit 不允许只有一个平台绿。**

这条比任何架构图都管用。没有它，"跨平台"永远是一句写在 README 里的话。

### 附加检查

```bash
# 禁止 todo!() / unimplemented!() 进入 zex-platform
! grep -rn "todo!\|unimplemented!" crates/zex-platform/src/

# 禁止内核依赖 gpui（分层破坏）
! grep -q "gpui-kit" crates/zex-core/Cargo.toml crates/zex-agent/Cargo.toml
```

---

## 5. 各平台的坑（持续补充）

### Linux

| 坑 | 应对 |
|---|---|
| **Wayland vs X11 分裂** | GUI 自动化在 Wayland 下受限（无全局坐标读取、无法模拟其他窗口的输入）。策略：**X11 提供完整能力，Wayland 提供受限能力并明确告知**。截屏走 portal（`org.freedesktop.portal.Screenshot`）。 |
| 发行版碎片（apt/dnf/pacman） | `PackageManager` 运行时探测，不编译期绑定 |
| AT-SPI2 需要 accessibility bus | 检测 `AT_SPI_BUS_ADDRESS`，未启用时降级为截屏方案并提示用户开启 |
| glibc 版本导致二进制不通用 | 发布用 musl 静态链接，或标注最低 glibc |

### Windows

| 坑 | 应对 |
|---|---|
| **UAC**：非提升进程无法操作系统目录/服务 | 主进程不提权；需要提权的操作通过独立提权 helper 执行（用户看见 UAC 弹窗 = 天然的审批环节） |
| **SmartScreen** | 必须买 EV 证书签名，否则每次下载都红色警告 |
| 长路径 (>260 字符) | manifest 里声明 `longPathAware`，代码统一用 `\\?\` 前缀 |
| 控制台窗口闪现 | 所有子进程用 `CREATE_NO_WINDOW` 标志 |
| 中文路径 / GBK 输出 | 子进程统一 `chcp 65001` 或强制 UTF-8 |

### macOS

| 坑 | 应对 |
|---|---|
| **TCC 授权**：辅助功能、屏幕录制、完全磁盘访问 | 首次使用时引导用户到系统设置授权；检测未授权时明确告知缺哪个权限。**这是 M3 最大的时间黑洞。** |
| **公证（notarization）** | 必须 Apple Developer 账号 + 公证流程；提前 2 个月启动 |
| App Sandbox 与"整机控制"根本冲突 | **不启用 App Sandbox**，走 Developer ID + 公证分发 |
| `CGEventTap` 需要辅助功能权限 | 同上 |
| Apple Silicon / Intel 双架构 | 发布 universal binary（`lipo`） |

---

## 5.5 验证状态（2026-11 实测）

这一节记录**实际跑过什么**，和上面的"计划怎么应对"区分开。
没跑过的就是未知，不因为文档写了就当已知。

### 交叉编译（Linux 主机）

| target | 范围 | 结果 |
|---|---|---|
| `x86_64-pc-windows-gnu` | 全部 6 个 crate（含 `zex-app` + gpui-kit） | ✅ 通过 |
| `x86_64-apple-darwin` | 同上 | ⚠️ **卡在 C 依赖** |

Windows 那次把 `gpui-kit`、`gpui-pre-platform`、`accesskit_windows`
一路编译通过 —— 纯 Rust 部分对 Windows 没有隐藏的 Linux 假设。

macOS 失败点在 `aws-lc-sys`（reqwest/rustls 的传递依赖）：它要用
`cc` 编译 `.S` 汇编，而 `-mmacosx-version-min` 只有 Apple 的 clang
认。**这不是我们代码的问题**，是交叉编译本身的限制 —— 在 macOS
机器上编译同一个 crate 不会遇到。

**结论：macOS 只能靠 CI 或真机验证，交叉编译这条路走不通。**

### 运行时行为

| 平台 | 窗口 | 关窗拦截 | shell | 路径 |
|---|---|---|---|---|
| Linux (niri/Wayland) | ✅ 真机 | ✅ 真机（有任务时拦下） | ✅ 真机 | ✅ 真机 |
| Windows | ❓ | ❓ | ❓ | ❓ |
| macOS | ❓ | ❓ | ❓ | ❓ |

### 已知的平台相关代码（已做条件编译）

| 位置 | 处理 |
|---|---|
| `zex-tools/shell.rs` | `sh -c` / `cmd /C` 分叉，Windows 加 `CREATE_NO_WINDOW` |
| `zex-app/config.rs` | `HOME` / `USERPROFILE` 兜底 |
| `zex-policy/rules.rs` | 同上，拿不到 HOME 时保守判定 |

### 还没验证的具体项

- `on_window_should_close` 在 Windows/macOS 的实现 —— gpui 有对应
  的平台 trait 方法，但行为没实测（Windows 上是 `WM_CLOSE`，
  macOS 是 `windowShouldClose:`，语义可能不同）
- 文件名大小写敏感性（Windows 不敏感）
- 路径分隔符在 `glob` 匹配里的行为
- 长路径 >260 字符
- 中文路径 + 控制台编码

---

## 6. 构建与发布

| 平台 | 产物 | 备注 |
|---|---|---|
| Linux | `.AppImage` + `.tar.gz` | AppImage 免安装；tar.gz 给包管理器 |
| Windows | `.msi`（WiX）+ portable `.zip` | 必须签名 |
| macOS | `.dmg`（universal）+ `.app` | 必须签名 + 公证 |

工具：`cargo-dist` 可覆盖大部分，Linux AppImage 需自己补。

> 打包在 M5，但**签名证书现在就要开始申请**。
