//! 扩展自检工具：把"能不能加载并渲染"变成模型能读的一次调用。
//!
//! # 为什么这个工具是关键路径上的
//!
//! zex 要能按用户要求写扩展，就得让模型知道自己写错了什么。没有这一步，
//! 自我扩展等于"生成猜测 + 用户看日志"。有了它，闭环才成立：
//! 写 → 查 → 读错误 → 改。
//!
//! # 它不做的事
//!
//! 不判定"这个界面好不好看"、不截图、不跑交互。只回答两个问题：
//! 模块能不能求值、第一帧能不能物化出元素。这两步覆盖了绝大多数手写错误
//! （导出名写错、组件用错模块、`cx` 用错地方）。
//!
//! # 风险等级
//!
//! `Reversible`（L1）：它确实会执行脚本代码。但执行发生在能力集为空的
//! 沙箱里 —— 既不能读文件也不能联网，唯一的副作用是可能卡住一帧
//! （沙箱有 2 秒上限兜底）。

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use zex_core::event::CoreEvent;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 等 UI 回话的上限。
///
/// 比沙箱自己的 2 秒渲染上限宽裕得多：这条链还要过事件通道、开一个隐藏窗口。
/// 太短会把"机器忙"误报成"脚本有问题"，而误报比慢更糟 ——
/// 模型会去改一段本来就对的代码。
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// `ext_check` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckArgs {
    /// 扩展 id（`~/.zex/extensions/<id>` 的目录名，也是清单里的 id）。
    pub app_id: String,
}

/// 校验一个扩展能不能加载并渲染出第一帧。
pub struct ExtensionCheck {
    ev_tx: mpsc::Sender<CoreEvent>,
}

impl ExtensionCheck {
    /// 用事件通道构造（工具要借它把请求送到 UI 线程）。
    pub fn new(ev_tx: mpsc::Sender<CoreEvent>) -> Self {
        Self { ev_tx }
    }
}

#[async_trait]
impl Tool for ExtensionCheck {
    const NAME: &'static str = "ext_check";
    const RISK: Risk = Risk::Reversible;
    type Args = CheckArgs;

    fn description() -> &'static str {
        "校验一个 zex 扩展。写或改完 main.js 之后调用它。\
         它会真的求值一遍脚本，把模块解析、导入名、默认导出的错误原文\
         返回给你 —— 这覆盖了绝大多数手写错误（例如 v_flex 属于 gpui-base，\
         不属于 gpui-kit）。求值通过说明代码是对的，但不代表布局如你所愿：\
         要确认实际画面，请让用户点开这个扩展，或用 ext_logs 看运行期异常。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: CheckArgs) -> Result<ToolOutput> {
        let app_id = args.app_id.trim().to_string();
        if app_id.is_empty() {
            return Err(Error::BadArgs {
                tool: Self::NAME.into(),
                message: "app_id 不能为空".into(),
            });
        }

        // 先清空日志缓冲：校验引起的那批错误一定是这次的，
        // 混着上一轮的记录，模型会去修一个已经修好的问题
        crate::ext_logs::clear();
        let (id, ev) = zex_extension::check::request(&app_id);
        let rx = zex_extension::check::register(&id);
        if self.ev_tx.send(ev).await.is_err() {
            return Err(Error::ToolFailed {
                tool: Self::NAME.into(),
                message: "内核已停止，无法校验".into(),
            });
        }

        let report = match tokio::time::timeout(WAIT_TIMEOUT, rx).await {
            Ok(Ok(report)) => report,
            Ok(Err(_)) => {
                return Err(Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: "校验被丢弃（界面可能已关闭）".into(),
                });
            }
            Err(_) => {
                return Err(Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: format!("等了 {} 秒没有回音，界面可能卡住了", WAIT_TIMEOUT.as_secs()),
                });
            }
        };

        match (report.tree, report.error) {
            (_, Some(err)) => Ok(ToolOutput {
                content: format!("扩展 `{app_id}` 校验失败：\n{err}"),
                // 失败对模型来说是**可用的信息**（它下一步要据此改代码），
                // 所以带 is_error 但内容完整，而不是抛异常把上下文丢掉
                is_error: true,
                meta: None,
            }),
            (Some(tree), None) => Ok(ToolOutput {
                content: format!("扩展 `{app_id}` 校验通过。元素树：\n{tree}"),
                is_error: false,
                meta: None,
            }),
            (None, None) => Ok(ToolOutput {
                content: format!(
                    "扩展 `{app_id}` 校验通过。{}",
                    report.note.unwrap_or_else(|| "脚本求值正常。".into())
                ),
                is_error: false,
                meta: None,
            }),
        }
    }
}

/// `ext_logs` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogsArgs {
    /// 最多返回多少条（默认 40，最多 200）。返回的是最近的若干条。
    #[serde(default)]
    pub lines: Option<usize>,
    /// 只看这个级别及以上：`info` / `warn` / `error`。默认不过滤。
    #[serde(default)]
    pub min_level: Option<String>,
}

/// 读扩展的运行期日志。
///
/// 和 `ext_check` 分工：`ext_check` 回答"能不能加载、第一帧长什么样"，
/// 这里回答"跑起来之后出了什么事"。交互触发的错误（点击回调抛异常、
/// 未处理的 Promise 拒绝）只会出现在这里。
pub struct ExtensionLogs;

#[async_trait]
impl Tool for ExtensionLogs {
    const NAME: &'static str = "ext_logs";
    const RISK: Risk = Risk::ReadOnly;
    type Args = LogsArgs;

    fn description() -> &'static str {
        "读取扩展的运行期日志：脚本里的 console.* 输出、点击/渲染回调抛的异常、\
         未处理的 Promise 拒绝。排查「点了按钮没反应」这类问题时先看它。\
         注意：日志是全局缓冲，不按扩展区分（上游事件里没有应用字段），\
         同时开发多个扩展时按内容自行判断。调用 ext_check 会清空缓冲，\
         所以那之前的历史日志看不到。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: LogsArgs) -> Result<ToolOutput> {
        let lines = args.lines.unwrap_or(40).clamp(1, 200);
        let entries = crate::ext_logs::recent(lines, args.min_level.as_deref());
        if entries.is_empty() {
            return Ok(ToolOutput {
                content: "没有日志。可能是：确实没输出、已经被 ext_check 清空、或日志级别被过滤掉了"
                    .into(),
                is_error: false,
                meta: None,
            });
        }
        let mut out = String::with_capacity(entries.len() * 80);
        for e in &entries {
            out.push_str(&format!("[{}] {}: {}\n", e.level, e.target, e.message));
        }
        Ok(ToolOutput {
            content: out,
            is_error: false,
            meta: None,
        })
    }
}

/// 扩展 id 的合法性。
///
/// id 会直接变成目录名拼在 `~/.zex/extensions/` 后面，所以必须挡住
/// 任何能跳出那一层的写法 —— 否则 `ext_install` 就成了"在任意路径建目录"。
fn check_app_id(id: &str) -> std::result::Result<(), String> {
    if id.trim().is_empty() {
        return Err("app_id 不能为空".into());
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") || id.starts_with('.') {
        return Err(format!(
            "`{id}` 不是合法的扩展 id（不能含 / \\ .. ，也不能以 . 开头）"
        ));
    }
    Ok(())
}

/// `ext_install` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct InstallArgs {
    /// 扩展 id：目录名，也是清单里的 id。
    pub app_id: String,
    /// 可选：入口文件名，默认 `main.js`。
    #[serde(default)]
    pub entry: Option<String>,
    /// 可选：显示名，默认用 id。
    #[serde(default)]
    pub name: Option<String>,
}

/// 把目录登记成一个扩展（写清单、重新扫描）。
///
/// # 它不做什么
///
/// **不写脚本、不生成代码。** 脚本由模型用 `fs_write` 写好后，这个工具
/// 只负责"把目录变成 zex 认得的扩展"：补一份 `gpui-shell.json`，
/// 然后让界面重新扫描。分开是有意的——写代码和登记是两件事，
/// 混在一起的话，"只想改个名字"也得重写一遍脚本。
///
/// # 风险等级
///
/// `Reversible`（L1）：它只创建一个纯文本清单文件，不执行任何东西。
/// 真正的执行发生在用户点开面板时（那一步有自己的沙箱与能力约束）。
pub struct ExtensionInstall;

#[async_trait]
impl Tool for ExtensionInstall {
    const NAME: &'static str = "ext_install";
    const RISK: Risk = Risk::Reversible;
    type Args = InstallArgs;

    fn description() -> &'static str {
        "把一个已经写好脚本的目录登记成 zex 扩展：创建/补全 gpui-shell.json 并刷新扫描。\
         脚本本身要用 fs_write 先写好（通常放在 ~/.zex/extensions/<id>/main.js）。\
         登记完可以用 ext_check 校验，用户在「设置 → 扩展」里点开。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: InstallArgs) -> Result<ToolOutput> {
        let id = args.app_id.trim();
        check_app_id(id).map_err(|message| Error::BadArgs {
            tool: Self::NAME.into(),
            message,
        })?;
        let entry = args.entry.unwrap_or_else(|| "main.js".to_string());
        if entry.contains('/') || entry.contains("..") {
            return Err(Error::BadArgs {
                tool: Self::NAME.into(),
                message: format!("entry 必须是目录内的相对路径，收到 `{entry}`"),
            });
        }

        let dir = zex_extension::extensions_dir().join(id);
        std::fs::create_dir_all(&dir).map_err(|e| Error::ToolFailed {
            tool: Self::NAME.into(),
            message: format!("无法创建目录 {}: {e}", dir.display()),
        })?;

        // 入口不存在时明确说出来：否则用户点开只看到白屏
        let entry_path = dir.join(&entry);
        let entry_note = if entry_path.exists() {
            String::new()
        } else {
            format!("\n注意：{entry} 还不存在，先用 fs_write 写好再打开。")
        };

        let manifest_path = dir.join(zex_extension::catalog::MANIFEST);
        if !manifest_path.exists() {
            let name = args.name.unwrap_or_else(|| id.to_string());
            let manifest = serde_json::json!({
                "id": id,
                "name": name,
                "entry": entry,
            });
            let text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
            std::fs::write(&manifest_path, text).map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("无法写清单 {}: {e}", manifest_path.display()),
            })?;
        }

        let count = zex_extension::scan_default().len();
        Ok(ToolOutput {
            content: format!(
                "已登记扩展 `{id}`（入口 {entry}）。当前共有 {count} 个扩展。{entry_note}\n\
                 下一步：用 ext_check 校验；用户在「设置 → 扩展」点开，或固定到侧边栏直接点。"
            ),
            is_error: false,
            meta: None,
        })
    }
}

/// `ext_uninstall` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UninstallArgs {
    /// 要删除的扩展 id。
    pub app_id: String,
    /// 必须显式传 true。删目录不可逆，默认拒绝。
    pub confirm: bool,
}

/// 删除一个扩展的目录。
///
/// 风险等级刻意定成 `Destructive`（L3）：它删的是用户磁盘上的文件，
/// 而且不进回收站。L3 不受"记住授权"影响，每次都要用户点。
pub struct ExtensionUninstall;

#[async_trait]
impl Tool for ExtensionUninstall {
    const NAME: &'static str = "ext_uninstall";
    const RISK: Risk = Risk::Destructive;
    type Args = UninstallArgs;

    fn description() -> &'static str {
        "删除一个扩展及其目录（不可恢复，不进回收站）。必须显式传 confirm=true。\
         只删 ~/.zex/extensions/<id> 这一层，不会动别的地方。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: UninstallArgs) -> Result<ToolOutput> {
        if !args.confirm {
            return Err(Error::BadArgs {
                tool: Self::NAME.into(),
                message: "删除扩展不可恢复，需要显式传 confirm=true".into(),
            });
        }
        let id = args.app_id.trim();
        check_app_id(id).map_err(|message| Error::BadArgs {
            tool: Self::NAME.into(),
            message,
        })?;

        let dir = zex_extension::extensions_dir().join(id);
        if !dir.is_dir() {
            return Err(Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("没有这个扩展：{}", dir.display()),
            });
        }
        std::fs::remove_dir_all(&dir).map_err(|e| Error::ToolFailed {
            tool: Self::NAME.into(),
            message: format!("删除失败 {}: {e}", dir.display()),
        })?;
        Ok(ToolOutput {
            content: format!("已删除扩展 `{id}`（{}）。已固定的侧边栏入口会在下次启动时消失。", dir.display()),
            is_error: false,
            meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_ids() {
        assert!(check_app_id("demo").is_ok());
        assert!(check_app_id("git-status").is_ok());
        assert!(check_app_id("com.example.notes").is_ok());
        assert!(check_app_id("a_b-c1").is_ok());
    }

    #[test]
    fn rejects_anything_that_escapes_the_directory() {
        // 这几条一旦放行，ext_install 就能在扩展目录之外建目录
        assert!(check_app_id("../etc").is_err());
        assert!(check_app_id("a/b").is_err());
        assert!(check_app_id("a\\b").is_err());
        assert!(check_app_id(".hidden").is_err());
        assert!(check_app_id("   ").is_err());
    }
}
