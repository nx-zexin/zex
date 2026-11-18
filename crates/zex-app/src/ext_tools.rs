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
        "校验一个 zex 扩展能否加载并渲染出第一帧。\
         写或改完扩展的 main.js 之后必须调用它：它会在不显示窗口的情况下\
         真的求值一遍脚本、物化一遍元素树，把脚本自己的报错原文返回给你。\
         成功时返回元素树的文字结构（可用来确认布局层级是否如你所愿）。\
         常见错误：导入了不存在的名字（v_flex 属于 gpui-base，不属于 gpui-kit）。"
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
                content: format!("扩展 `{app_id}` 校验没有返回内容（既无元素树也无错误）"),
                is_error: true,
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
