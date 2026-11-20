//! 扩展校验：把"能不能加载/渲染"变成一次可以回给模型的工具调用。
//!
//! # 为什么这件事非做不可
//!
//! 一个能写代码的平台，模型必须能自己知道写错了。没有这一步，
//! "自我扩展"就退化成"生成猜测 + 人肉看日志"。
//!
//! # 为什么要绕一圈
//!
//! `ShellRuntime::check` 内部会 `window.replace_root()` + `window.draw()`，
//! 也就是说它**必须在一帧里跑**。而模型调用工具跑在 tokio 线程上，
//! 那里没有 `Window`，也没有 gpui 的帧循环。
//!
//! 所以链路是：
//!
//! ```text
//! 工具（内核线程）  ──注册等待者──→  发 CoreEvent::ExtensionCheck
//!                                      ↓ 事件通道（本就是内核 → UI）
//! UI 线程           开隐藏窗口 → runtime.check() → 关窗口
//!                                      ↓ oneshot
//! 工具              拿到 元素树 / 结构化错误 → 作为工具输出回给模型
//! ```
//!
//! 等待者放在进程级注册表里，而不是塞进事件：
//! `CoreEvent` 必须是可 `Clone` 的纯数据（见 `zex_core::event` 的模块文档），
//! 塞不进一个 `oneshot::Sender`。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::oneshot;
use zex_core::event::CoreEvent;

/// 一次校验的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// 成功时是整棵元素树的文字描述（`debug_tree()` 的输出）。
    pub tree: Option<String>,
    /// 失败时的错误原文（含脚本自己的栈）。
    pub error: Option<String>,
    /// 通过但没有渲染时的一句说明（为什么没有树、怎么才能有）。
    pub note: Option<String>,
}

impl CheckReport {
    /// 成功。
    pub fn ok(tree: String) -> Self {
        Self {
            tree: Some(tree),
            error: None,
            note: None,
        }
    }

    /// 只求值、未渲染的通过。没有元素树 —— 也没有开过窗口。
    pub fn validated(note: impl Into<String>) -> Self {
        Self {
            tree: None,
            error: None,
            note: Some(note.into()),
        }
    }

    /// 失败。
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            tree: None,
            error: Some(message.into()),
            note: None,
        }
    }

    /// 是否通过。
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// 等待中的校验请求：请求号 → 收结果的人。
///
/// 进程级而不是按实例传递：工具的构造发生在 `build_agent_*` 里，
/// 那里看不到 UI 视图；把表做成全局，两边都能拿到同一份，
/// 也不必为了这一件事给所有构造函数加参数。
fn pending() -> &'static Mutex<HashMap<String, oneshot::Sender<CheckReport>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, oneshot::Sender<CheckReport>>>> =
        OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 发起一次校验，返回一个将来会拿到结论的 receiver。
///
/// 调用方要自己把 [`CoreEvent::ExtensionCheck`] 送进事件通道 ——
/// 这里不持有通道，是为了让本模块保持"只管配对"这一件事。
pub fn register(id: &str) -> oneshot::Receiver<CheckReport> {
    let (tx, rx) = oneshot::channel();
    let mut held = match pending().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.insert(id.to_string(), tx);
    rx
}

/// UI 线程算完之后回填结论。没有等待者时是空操作。
pub fn deliver(id: &str, report: CheckReport) {
    let waiter = {
        let mut held = match pending().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        held.remove(id)
    };
    if let Some(tx) = waiter {
        let _ = tx.send(report);
    }
}

/// 当前有几个校验在等。
pub fn waiting() -> usize {
    match pending().lock() {
        Ok(g) => g.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

/// 某个 id 是否正在等回音。
///
/// 给测试用：`waiting()` 数的是全局表，和别的测试并行跑时会看到它们的
/// 记录 —— 判断"某个具体 id 有没有留下痕迹"才是并行下稳定的断言。
#[cfg(test)]
pub fn is_waiting(id: &str) -> bool {
    match pending().lock() {
        Ok(g) => g.contains_key(id),
        Err(poisoned) => poisoned.into_inner().contains_key(id),
    }
}

/// 构造一个校验请求事件。
pub fn request(app_id: &str) -> (String, CoreEvent) {
    let id = zex_core::ids::ToolCallId::generate().to_string();
    let ev = CoreEvent::ExtensionCheck {
        id: id.clone(),
        app_id: app_id.to_string(),
    };
    (id, ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_wakes_the_waiter() {
        let id = "check-1";
        let rx = register(id);
        assert_eq!(waiting(), 1);
        deliver(id, CheckReport::ok("v_flex".into()));
        assert_eq!(waiting(), 0, "回填之后必须从表里消失");
        let report = rx.blocking_recv().expect("应收到结论");
        assert!(report.is_ok());
        assert_eq!(report.tree.as_deref(), Some("v_flex"));
    }

    #[test]
    fn deliver_without_waiter_is_a_noop() {
        // UI 可能因为窗口关掉而晚到一步，不能 panic
        //
        // 断言的是"没有增加"而不是 `== 0`：`waiting()` 数的是全局表，
        // 和别的测试并行跑时会看到它们注册的等待者 —— 写死 0 就是 flaky
        deliver("nobody", CheckReport::failed("x"));
        assert!(!is_waiting("nobody"), "投递给不存在的等待者不该凭空建一条");
    }

    #[test]
    fn error_report_is_not_ok() {
        let r = CheckReport::failed("SyntaxError: Could not find export 'v_flex'");
        assert!(!r.is_ok());
        assert!(r.error.unwrap().contains("v_flex"));
    }

    #[test]
    fn request_carries_a_unique_id() {
        let (a, ev_a) = request("demo");
        let (b, _) = request("demo");
        assert_ne!(a, b, "两次校验不能撞号");
        match ev_a {
            CoreEvent::ExtensionCheck { id, app_id } => {
                assert_eq!(id, a);
                assert_eq!(app_id, "demo");
            }
            _ => panic!("应构造 ExtensionCheck"),
        }
    }

    #[test]
    fn validated_report_is_ok_but_carries_no_tree() {
        let r = CheckReport::validated("脚本求值通过");
        assert!(r.is_ok(), "只求值的通过也是通过");
        assert!(r.tree.is_none(), "没有渲染就不该编造元素树");
        assert!(r.note.unwrap().contains("求值"));
    }
}
