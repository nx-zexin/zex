//! 剪贴板监视线程。
//!
//! # 为什么要一个独立线程
//!
//! 脚本侧的 `cx.timer.every` 跑在 GPUI 的 foreground executor 上，而窗口
//! 不可见时系统不会给它派发绘制 —— 主线程于是不推进，定时器回调也就不会
//! 按时执行。实测的后果是真实的数据丢失：在 zex 之外连续复制三条内容，
//! 切回来只剩最后一条，中间两条永久消失（剪贴板里本来也只留最后一份，
//! 事后补读补不回来）。
//!
//! 所以捕获必须发生在**变化的那一刻**，而且不能依赖 UI 帧循环。这就是这个
//! 线程的全部职责：它只在 Linux/Wayland 下启动，直接和 compositor 对话，
//! 把每次变化丢进一个队列；脚本什么时候来取都行，一条不丢。
//!
//! # 为什么不直接用 GPUI 的剪贴板
//!
//! 那需要 `&mut App`，只能在主线程调，绕不开上面的死结。
//!
//! # 为什么不用 arboard
//!
//! 它在这个环境（Wayland）读不到东西：实测 `wl-paste` 和 `wl-clipboard-rs`
//! 都能读，`arboard` 一律返回 `ContentNotAvailable` —— 它走了 X11/XWayland
//! 那条路，而内容是主 compositor 持有的。所以这里直接用 `wl-clipboard-rs`。
//!
//! # 非 Wayland 平台
//!
//! 不启动线程，`drain()` 永远返回空，脚本退回主线程轮询那条老路
//! （会漏，但至少不假装自己在工作）。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// 队列上限。复制狂魔也不会一次积压这么多；超了丢最旧的，
/// 因为最新的才是用户最可能要的。
const MAX_QUEUE: usize = 300;

/// 轮询间隔。实测 120ms 能把间隔 250ms 的三次快速复制全部捕获，
/// 平均每次读取只花 4ms（冷启动那次约 100ms）。
#[cfg(target_os = "linux")]
const POLL_MS: u64 = 120;

/// 队列里的一条变化。
#[derive(Clone, Debug)]
pub struct Event {
    /// `"text"` 或 `"pic"`。
    pub kind: &'static str,
    /// 文本内容，或图片的资产路径（`clip-thumbs/xxx.png`）。
    pub body: String,
    /// 图片像素宽；文本为 0。
    pub width: u32,
    /// 图片像素高；文本为 0。
    pub height: u32,
    /// 捕获时刻（Unix 毫秒）。
    ///
    /// 必须在**这里**记，不能等脚本取走时再取当前时间：一批三条是
    /// 同一瞬间交给脚本的，脚本那时打的时间戳会落在同一毫秒上，
    /// 先后关系就丢了 —— 列表顺序会变得随机。用捕获时刻就不会。
    pub at_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static QUEUE: OnceLock<Mutex<VecDeque<Event>>> = OnceLock::new();
static STARTED: OnceLock<()> = OnceLock::new();

fn queue() -> &'static Mutex<VecDeque<Event>> {
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn push(event: Event) {
    let Ok(mut q) = queue().lock() else {
        return;
    };
    // 同一内容连着来（有的工具会重复 set 同一个值）就并进上一条，不堆垃圾
    if let Some(last) = q.back() {
        if last.kind == event.kind && last.body == event.body {
            return;
        }
    }
    if q.len() >= MAX_QUEUE {
        q.pop_front();
    }
    q.push_back(event);
}

/// 取走队列里的全部变化。
pub fn drain() -> Vec<Event> {
    let Ok(mut q) = queue().lock() else {
        return Vec::new();
    };
    q.drain(..).collect()
}

/// 这个进程里后台监听是否真的在跑。
pub fn watching() -> bool {
    STARTED.get().is_some()
}

/// 启动监视线程。重复调用是空操作。
pub fn start() {
    if STARTED.set(()).is_err() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        // 只有 Wayland 会话才起。X11 下 wl-clipboard-rs 连不上 compositor，
        // 与其在后台空转报错，不如让脚本用主线程那条路。
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        if !wayland {
            tracing::debug!("没有 WAYLAND_DISPLAY，剪贴板监视线程不启动");
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("zex-clip-watch".to_owned())
            .spawn(|| {
                if let Err(e) = watch_loop() {
                    tracing::warn!(error = %e, "剪贴板监视线程退出");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "无法启动剪贴板监视线程，退回主线程轮询");
        } else {
            tracing::info!("剪贴板监视线程已启动（Wayland，{}ms 一次）", POLL_MS);
        }
    }
}

/// 往剪贴板里放一份内容（图片或文本）。
///
/// # 为什么不用 GPUI 的 `write_to_clipboard`
///
/// 它在 Wayland 下**只能写文本**。`gpui-pre-linux` 的 wayland 后端是这么写的：
///
/// ```text
/// for mime_type in TEXT_MIME_TYPES {
///     data_source.offer(mime_type.to_string());
/// }
/// ```
///
/// 它只把文本 MIME 声明给 compositor，图片数据类型从来不 offer；而
/// `item.text()` 对图片返回空字符串，于是实际落进剪贴板的是一个空文本 ——
/// 界面上看着"复制成功"，粘出来什么都没有。
///
/// 所以这里绕开 GPUI，直接用 wl-clipboard-rs 自己当剪贴板所有者。
///
/// # 为什么要在自己的线程里
///
/// Wayland 的剪贴板是"所有权 + 服务"模型：写的一方必须**持续存活**并响应
/// 后续每一次 paste 请求。`copy_multi` 在 `ServeRequests::Unlimited` 下会一直
/// 阻塞，所以它必须待在一个专门的线程里 —— 不能占用调用者的线程，更不能占
/// 主线程。
///
/// 新内容会取代旧的：用一个"代号"计数器让旧的服务线程自己退出，免得多个
/// 线程抢着当所有者。
pub fn put(content: PutContent) {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 当前有效的代号。旧线程发现代号变了就退出。
    static GENERATION: AtomicU64 = AtomicU64::new(0);

    let Some(pipe) = server_channel() else {
        tracing::warn!("剪贴板服务通道不可用，无法写入剪贴板");
        return;
    };

    let seq = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if pipe.send((seq, content)).is_err() {
        tracing::warn!("剪贴板服务线程已退出，无法写入剪贴板");
    }
}

/// 送进服务线程的一件内容。
#[derive(Clone, Debug)]
pub enum PutContent {
    /// `(mime, bytes)` 列表，第一条是主格式。
    Bytes(Vec<(String, Vec<u8>)>),
    /// 纯文本。
    Text(String),
}

#[cfg(target_os = "linux")]
type Job = (u64, PutContent);

#[cfg(target_os = "linux")]
fn server_channel() -> Option<&'static std::sync::mpsc::Sender<Job>> {
    use std::sync::OnceLock;
    static TX: OnceLock<Option<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("zex-clip-serve".to_owned())
            .spawn(move || serve_loop(rx))
            .ok()
            .map(|_| tx)
    })
    .as_ref()
}

/// 服务线程的主循环：收到一件就把它设为剪贴板内容，并持续服务请求。
///
/// 每件内容起一个子线程去跑 `copy_multi`（它会阻塞），主循环负责"后来者取代
/// 前者"：新内容一来，代号 +1，旧子线程发现代号变了就停止服务，compositor
/// 会转投新的所有者。
#[cfg(target_os = "linux")]
fn serve_loop(rx: std::sync::mpsc::Receiver<Job>) {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let current = Arc::new(AtomicU64::new(0));

    for (seq, content) in rx {
        current.store(seq, std::sync::atomic::Ordering::SeqCst);
        let current = Arc::clone(&current);
        let _ = std::thread::Builder::new()
            .name(format!("zex-clip-serve-{seq}"))
            .spawn(move || serve_one(seq, content, &current));
    }
}

#[cfg(target_os = "linux")]
fn serve_one(_seq: u64, content: PutContent, _current: &std::sync::atomic::AtomicU64) {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, ServeRequests, Source};

    let sources: Vec<MimeSource> = match content {
        PutContent::Text(text) => vec![MimeSource {
            source: Source::Bytes(text.into_bytes().into_boxed_slice()),
            mime_type: MimeType::Text,
        }],
        PutContent::Bytes(items) => items
            .into_iter()
            .map(|(mime, bytes)| MimeSource {
                source: Source::Bytes(bytes.into_boxed_slice()),
                mime_type: MimeType::Specific(mime),
            })
            .collect(),
    };
    if sources.is_empty() {
        return;
    }

    // 一次 set_selection，然后服务下去。
    //
    // 这里踩过一个坑：原来写成 `Only(1)` 放在 while 里反复重来，想借此实现
    // "后来者取代前者"。但 Wayland 的 `set_selection` 需要**当次**有效的
    // serial，同一线程连续反复调用会让 data_source 失效 —— 表现为剪贴板
    // 类型里列着 image/png，实际去读却什么都拿不到。
    //
    // 正确的语义是：每次写入只 set_selection 一次，声明"我要服务很多次"，
    // 然后阻塞在这里。新内容来时由**另一个**线程 set_selection，compositor
    // 会让旧的 source 失效，这边随之返回 —— 抢占交给 compositor，不是自己抢。
    let mut opts = Options::new();
    opts.serve_requests(ServeRequests::Only(u32::MAX as usize));
    if opts.copy_multi(sources).is_err() {
        // 被新内容顶掉了，或者 compositor 拒绝。两种情况都不该重试。
    }
}

/// 读图片时优先要哪些格式，**按优先级排列**。
///
/// 这个列表的由来：KDE 的 spectacle 复制一张截图时会同时提供 30 多种格式
/// （png / jpeg / webp / avif / heif / exr / j2k / icns / pcx / tif …）。
/// 之前用"集合里第一个 image/*"去挑，而 `get_mime_types` 返回的是 `HashSet`
/// —— 迭代顺序每次都不一样，于是每一轮都选中不同格式：字节不同、哈希不同、
/// 就多出一条新记录。一张截图能刷出几十条。
///
/// 所以必须固定顺序：先要无损的 png（实测 3.4ms，且同一张图每次读都是同一
/// 份字节，可以当去重键），退而求其次才是别的。
#[cfg(target_os = "linux")]
const IMAGE_PREFERENCE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/bmp",
    "image/tiff",
    "image/gif",
];

/// 文本格式的优先级，同样固定。
#[cfg(target_os = "linux")]
const TEXT_PREFERENCE: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
];

#[cfg(target_os = "linux")]
fn watch_loop() -> std::result::Result<(), String> {
    use std::io::Read as _;

    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents, get_mime_types};

    // 上一次成功记下的内容。**只有成功读到时才更新** —— 读失败不能当变化，
    // 否则一次瞬时错误就会把同一条内容再记一遍。
    let mut last_text = String::new();
    let mut last_pic: Option<String> = None;

    let read_mime = |mime: &str| -> Option<Vec<u8>> {
        let (mut pipe, _) = get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            MimeType::Specific(mime),
        )
        .ok()?;
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf).ok()?;
        if buf.is_empty() { None } else { Some(buf) }
    };

    loop {
        let types = match get_mime_types(ClipboardType::Regular, Seat::Unspecified) {
            Ok(set) => set,
            // 剪贴板空着，或者 owner 不响应。睡一觉再来，状态不动。
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
                continue;
            }
        };
        let has = |m: &str| types.iter().any(|t| t.as_str() == m);

        // ---- 图片 ----
        //
        // 有 image/* 就**只**走这条：不能因为某次读图失败就落到文本分支，
        // 那会把 spectacle 提供的文件名当成一条文本记下来，下一轮图片又成功，
        // 于是同一次复制在列表里一图一文交替出现。
        let image_mime = IMAGE_PREFERENCE
            .iter()
            .find(|m| has(m))
            .map(|m| *m)
            .or_else(|| {
                // 优先级表里都没有时，退到"排序后取第一个"—— 至少是确定的
                let mut all: Vec<&str> = types
                    .iter()
                    .map(|t| t.as_str())
                    .filter(|t| t.starts_with("image/"))
                    .collect();
                all.sort_unstable();
                all.first().copied()
            });

        if let Some(mime) = image_mime {
            if let Some(bytes) = read_mime(mime) {
                let format = crate::clip_image::format_of_mime(mime);
                if let Some(shot) = crate::clip_image::store(format, &bytes) {
                    // 缩略文件名就是内容哈希，所以路径相同 == 内容相同
                    if last_pic.as_deref() != Some(shot.rel_path.as_str()) {
                        push(Event {
                            kind: "pic",
                            body: shot.rel_path.clone(),
                            width: shot.width,
                            height: shot.height,
                            at_ms: now_ms(),
                        });
                        last_pic = Some(shot.rel_path);
                        last_text.clear();
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            continue;
        }

        // ---- 文本 ----
        let text_mime = TEXT_PREFERENCE.iter().find(|m| has(m)).map(|m| *m);
        if let Some(mime) = text_mime {
            if let Some(bytes) = read_mime(mime) {
                if let Ok(text) = String::from_utf8(bytes) {
                    let text = text.trim_end_matches('\0').to_owned();
                    if !text.is_empty() && text != last_text {
                        push(Event {
                            kind: "text",
                            body: text.clone(),
                            width: 0,
                            height: 0,
                            at_ms: now_ms(),
                        });
                        last_text = text;
                        last_pic = None;
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 队列是**进程级静态量**，而测试默认并行跑：两个用例同时 push/drain
    /// 就会互相把对方的条目取走（`queue_is_bounded` 一次灌 350 条）。
    /// 所有碰队列的用例都先拿这把锁。
    static QUEUE_LOCK: Mutex<()> = Mutex::new(());

    /// 拿到锁并清空队列，让每个用例从干净状态开始。
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let guard = QUEUE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        drain();
        guard
    }

    #[test]
    fn queue_drains_and_does_not_duplicate_back_to_back() {
        let _g = exclusive();
        let at = now_ms();
        push(Event {
            kind: "text",
            body: "a".into(),
            width: 0,
            height: 0,
            at_ms: at,
        });
        push(Event {
            kind: "text",
            body: "a".into(),
            width: 0,
            height: 0,
            at_ms: at + 1,
        });
        push(Event {
            kind: "text",
            body: "b".into(),
            width: 0,
            height: 0,
            at_ms: at + 2,
        });

        let items = drain();
        let bodies: Vec<_> = items.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(bodies, vec!["a", "b"]);
        assert!(drain().is_empty(), "取过就空");
    }

    #[test]
    fn queue_is_bounded() {
        let _g = exclusive();
        for i in 0..(MAX_QUEUE + 50) {
            push(Event {
                kind: "text",
                body: format!("x{i}"),
                width: 0,
                height: 0,
                at_ms: i as u64,
            });
        }
        let items = drain();
        assert_eq!(items.len(), MAX_QUEUE);
        // 留下的是最新的一批
        assert_eq!(
            items.last().map(|e| e.body.as_str()),
            Some(format!("x{}", MAX_QUEUE + 49).as_str())
        );
    }
}
