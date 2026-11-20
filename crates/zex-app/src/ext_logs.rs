//! 运行期日志回流：把脚本的 `console.*` 和运行时错误抓进环形缓冲，
//! 让模型能读到"跑起来之后出了什么事"。
//!
//! # 为什么需要它
//!
//! `ext_check` 只回答"能不能加载、第一帧长什么样"。可扩展真正出问题的地方
//! 多数在**交互之后**：点一下按钮抛异常、回调里用了失效的 `cx`、
//! 异步任务被拒。这些错误只写进 `tracing`，界面上一片安静 ——
//! 用户看到的是"点了没反应"，模型更是完全不知道。
//!
//! 把它们收进一个读得到的缓冲，模型就能自己发现"我写的点击回调报错了"。
//!
//! # 归属的诚实说明
//!
//! `tracing` 事件里没有"这条是哪个扩展打的"这个字段，上游也不提供。
//! 所以这里是**全局**缓冲，不按 app 分桶。判断归属只能靠上下文
//! （同一时间里只开发一个扩展时，这就够了）。宁可给出带说明的全局日志，
//! 也不要编一个看起来精确、实际会串的归属。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// 缓冲上限。
///
/// 200 条：够覆盖"打开面板、点几下、出错"这一小段，又不至于把上下文撑爆。
/// 超出后丢最旧的 —— 最近的错误才是要修的那个。
const CAPACITY: usize = 200;

/// 一条抓到的日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// 级别：`info` / `warn` / `error` 等。
    pub level: String,
    /// 来源（tracing target）。
    pub target: String,
    /// 正文。
    pub message: String,
    /// 单调递增的序号，用来判断先后（不依赖时钟）。
    pub seq: u64,
}

fn buffer() -> &'static Mutex<VecDeque<LogEntry>> {
    static BUF: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// 序号计数器（单调，不受系统时间跳变影响）。
fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 这些 target 前缀的消息值得留给模型看。
///
/// `gpui_shell` 覆盖脚本的 `console.*` 与运行时错误（点击回调、渲染失败、
/// 未处理的 Promise 拒绝）；`zex_extension` 是我们自己那一层的诊断。
fn is_interesting(target: &str) -> bool {
    target.starts_with("gpui_shell") || target.starts_with("zex_extension")
}

/// 记一条（内部用，也供测试）。
fn push(level: &str, target: &str, message: String) {
    let entry = LogEntry {
        // 统一小写：tracing 给的是 "INFO"，而调用方填的是 "info"，
        // 两套写法迟早对不上
        level: level.to_ascii_lowercase(),
        target: target.to_string(),
        message,
        seq: next_seq(),
    };
    let mut held = match buffer().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if held.len() == CAPACITY {
        held.pop_front();
    }
    held.push_back(entry);
}

/// 取最近的日志，可按最低级别过滤。
///
/// `lines` 是上限（最新的那些），`min_level` 为空表示不过滤。
pub fn recent(lines: usize, min_level: Option<&str>) -> Vec<LogEntry> {
    let held = match buffer().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let floor = min_level.map(|l| l.to_ascii_lowercase());
    // 先筛再取尾部：VecDeque 的 rev 和 filter 叠在一起拿不到双端迭代器
    let filtered: Vec<LogEntry> = held
        .iter()
        .filter(|e| match &floor {
            None => true,
            Some(f) => level_rank(&e.level) >= level_rank(f),
        })
        .cloned()
        .collect();
    let start = filtered.len().saturating_sub(lines);
    filtered[start..].to_vec()
}

/// 级别排序：数字越大越严重。
fn level_rank(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" | "warning" => 3,
        "error" => 4,
        _ => 2,
    }
}

/// 清空缓冲。
///
/// 每次 `ext_check` 之前调用：校验产生的那批错误一定是这次的，
/// 混进上一轮的就分不清哪条是刚冒出来的。
pub fn clear() {
    let mut held = match buffer().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.clear();
}

/// 当前条数。
pub fn len() -> usize {
    match buffer().lock() {
        Ok(g) => g.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

/// 把事件的字段拼成一行文字。
struct MessageVisitor {
    out: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.out.push_str(value);
        } else {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(field.name());
            self.out.push('=');
            self.out.push_str(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.out.push_str(&text);
        } else {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(field.name());
            self.out.push('=');
            self.out.push_str(&text);
        }
    }
}

/// `tracing` 的一层：把感兴趣的日志抄进缓冲。
///
/// 只抄 `info` 及以上：`debug`/`trace` 是框架自己的噪声，
/// 混进来反而让真正的错误找不到。
pub struct CaptureLayer;

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !is_interesting(meta.target()) {
            return;
        }
        if *meta.level() > Level::INFO {
            return;
        }
        let mut visitor = MessageVisitor { out: String::new() };
        event.record(&mut visitor);
        let message = if visitor.out.is_empty() {
            // 只有 target 没有字段的事件（少数宏会这样）也要留个痕迹
            "(无消息)".to_string()
        } else {
            visitor.out
        };
        push(meta.level().as_str(), meta.target(), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些用例共用同一个全局缓冲，并行跑会互相污染 ——
    /// 所以先抢一把锁，再清空。锁的用处是串行，不是保护数据。
    fn fresh() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = match LOCK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear();
        guard
    }

    #[test]
    fn keeps_only_the_latest_capacity() {
        let _guard = fresh();
        for i in 0..CAPACITY + 25 {
            push("error", "gpui_shell::script", format!("e{i}"));
        }
        assert_eq!(len(), CAPACITY, "缓冲不该无限长");
        let all = recent(CAPACITY, None);
        assert_eq!(all.last().unwrap().message, format!("e{}", CAPACITY + 24));
        assert!(!all.iter().any(|e| e.message == "e0"), "最旧的应被挤掉");
    }

    #[test]
    fn filters_by_min_level() {
        let _guard = fresh();
        push("info", "gpui_shell::script", "普通日志".into());
        push("warn", "gpui_shell::script", "警告".into());
        push("error", "gpui_shell::script", "错误".into());

        let only_err = recent(10, Some("error"));
        assert_eq!(only_err.len(), 1);
        assert_eq!(only_err[0].message, "错误");

        let warn_up = recent(10, Some("warn"));
        assert_eq!(warn_up.len(), 2, "warn 应包含 error");

        assert_eq!(recent(10, None).len(), 3);
    }

    #[test]
    fn lines_returns_the_most_recent_ones_in_order() {
        let _guard = fresh();
        for i in 0..10 {
            push("error", "gpui_shell::script", format!("n{i}"));
        }
        let last3 = recent(3, None);
        assert_eq!(
            last3.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
            ["n7", "n8", "n9"],
            "要最近的三条，且保持时间顺序"
        );
    }

    #[test]
    fn targets_outside_the_script_layer_are_ignored() {
        assert!(is_interesting("gpui_shell::script"));
        assert!(is_interesting("gpui_shell::engine::quickjs"));
        assert!(is_interesting("zex_extension::host"));
        // 内核自己的日志不往里塞：它会淹没脚本那几条
        assert!(!is_interesting("zex_agent"));
        assert!(!is_interesting("gpui::text_system"));
    }

    /// 用一个作用域内的真实 subscriber 走一遍 Layer —— 单测 push() 测不到
    /// "事件到底有没有被这一层接住"。
    #[test]
    fn layer_captures_script_events_and_skips_noise() {
        let _guard = fresh();
        use tracing_subscriber::layer::SubscriberExt as _;

        let subscriber = tracing_subscriber::registry().with(CaptureLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "gpui_shell::script", "点击回调炸了");
            tracing::warn!(target: "gpui_shell::script", "警告一条");
            tracing::info!(target: "gpui_shell::script", "console.log 的内容");
            // 这两个不该进来：级别太低、或者不属于脚本层
            tracing::debug!(target: "gpui_shell::script", "调试噪声");
            tracing::error!(target: "zex_agent", "内核自己的错");
        });

        let got = recent(10, None);
        assert_eq!(got.len(), 3, "应收三条，实际：{got:?}");
        assert!(got.iter().any(|e| e.message.contains("点击回调炸了")));
        assert!(
            got.iter()
                .any(|e| e.level == "info" && e.message.contains("console.log"))
        );
        assert!(!got.iter().any(|e| e.message.contains("调试噪声")));
        assert!(!got.iter().any(|e| e.message.contains("内核自己的错")));
    }

    #[test]
    fn level_ranking_matches_severity() {
        assert!(level_rank("error") > level_rank("warn"));
        assert!(level_rank("warn") > level_rank("info"));
        assert!(level_rank("info") > level_rank("debug"));
        // 不认识的级别按 info 处理，不要因为拼错就把它藏起来
        assert_eq!(level_rank("whatever"), level_rank("info"));
    }
}
