//! Zex GUI 二进制。
//!
//! M0 阶段目标（`docs/M0-PLAN.md` T4）：验证 **gpui 世界 ↔ tokio 世界** 的桥接。
//! 验收标准：tokio 线程每秒发一个事件，UI 上的计数逐秒递增且不 panic。
//!
//! 这不是最终 UI。真正的对话界面在 T8 实现。

use gpui_kit::*;
use tokio::sync::mpsc;

/// 内核 → UI 的示例事件。
///
/// 真实实现使用 `zex_core::CoreEvent`；这里刻意用一个最小类型，
/// 让桥接本身成为唯一被验证的东西。
#[derive(Debug)]
enum DemoEvent {
    Tick(u64),
}

/// 内核侧：跑在独立 tokio runtime 线程上。
///
/// 真实实现里这里是整个 agent 内核。它**完全不知道 gpui 存在** ——
/// 这正是能在 CLI 里复用同一内核的原因（ADR-002 / ADR-009）。
fn spawn_core_thread(tx: mpsc::Sender<DemoEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("无法创建 tokio runtime: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let mut n = 0u64;
            loop {
                n += 1;
                // 用 blocking_send 会阻塞 tokio worker；这里用 send().await
                if tx.send(DemoEvent::Tick(n)).await.is_err() {
                    // UI 已关闭，内核优雅退出
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });
    })
}

struct DemoApp {
    ticks: u64,
}

impl Render for DemoApp {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_xl().child("Zex"))
            .child(div().child(format!("内核心跳: {}", self.ticks)))
            .child(div().text_sm().child("M0 · 双世界桥接验证"))
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 内核 → UI 的事件通道。容量小是有意的：UI 跟不上时内核应被反压，
    // 而不是无限堆积事件把内存吃光。
    let (event_tx, mut event_rx) = mpsc::channel::<DemoEvent>(64);
    let core_thread = spawn_core_thread(event_tx);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);

        let (handle, view) =
            match gpui_kit::open_window(WindowOptions::default(), cx, |_window, cx| {
                cx.new(|_| DemoApp { ticks: 0 })
            }) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("无法创建窗口: {e}");
                    return;
                }
            };

        // gpui 侧：消费内核事件并驱动重绘。
        //
        // 注意这里 await 的是 tokio 的 mpsc receiver —— tokio 的 channel
        // 不依赖 runtime 上下文，可以在 gpui 的 smol executor 上安全轮询。
        // 若将来换成依赖 tokio runtime 的类型（如 tokio::fs），必须经
        // Handle::spawn 转到内核线程，不能在这里直接 await。
        //
        // 用 WeakEntity：窗口关闭后 entity 被释放，update 返回 Err，
        // 这正是内核优雅退出的信号（见 spawn_core_thread 里的 send 失败分支）。
        let weak = view.downgrade();
        let _ = handle;
        cx.spawn(async move |cx| {
            while let Some(ev) = event_rx.recv().await {
                let DemoEvent::Tick(n) = ev;
                let alive = weak
                    .update(cx, |this, cx| {
                        this.ticks = n;
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    });

    // gpui 事件循环结束后回收内核线程
    let _ = core_thread.join();
}
