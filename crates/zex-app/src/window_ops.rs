//! 自绘标题栏需要的两个窗口动作：拖动、最大化 / 还原。
//!
//! # 为什么不能直接用 gpui 的那两个方法
//!
//! 两个缺陷都在上游，都是 Windows 专属：
//!
//! 1. **`Window::start_window_move()` 在 Windows 上是空函数。**
//!    `gpui-pre` 给它的默认实现是 `fn start_window_move(&self) {}`
//!    （`platform.rs:1378`），而 `gpui-pre-windows` 没有覆盖它。
//!    所以自绘标题栏按下去，什么都不会发生 —— 窗口拖不动。
//! 2. **`Window::zoom_window()` 只会最大化，不会还原。**
//!    `gpui-pre-windows` 的 `zoom()` 无条件 `ShowWindowAsync(SW_MAXIMIZE)`，
//!    既不问当前是否已经最大化，也没有 `SW_RESTORE` 这一支。于是最大化之后
//!    再点一次，等于"再最大化一次"，窗口回不到原来的尺寸。
//!
//! # 为什么修在这里，而不是给上游提 PR 或 fork
//!
//! 这是发布阻塞缺陷，等上游不现实；为一个方法 fork 整个 GUI crate 也不划算。
//! 所以由宿主直接调 Win32：两个动作都交给系统自己走（`WM_NCLBUTTONDOWN`
//! 配 `HTCAPTION`、`ShowWindow(SW_RESTORE/SW_MAXIMIZE)`），顺带白得
//! Aero Snap、拖到屏幕顶部自动最大化、双击标题栏、任务栏预览这些原生行为。
//! 自己用 `SetWindowPos` 跟着鼠标算，这些全都要重写一遍，还容易漏。
//!
//! 非 Windows 平台照旧走 gpui 自己的实现 —— 它在 macOS / Linux 上是好的。

use gpui_kit::Window;

/// 按住标题栏拖动窗口。
pub fn start_drag(window: &mut Window) {
    #[cfg(windows)]
    if win::begin_drag(window) {
        return;
    }
    window.start_window_move();
}

/// 最大化 / 还原。同一个入口给标题栏双击和右上角那个按钮用。
pub fn toggle_maximize(window: &mut Window) {
    #[cfg(windows)]
    if win::toggle_maximize(window) {
        return;
    }
    window.zoom_window();
}

#[cfg(windows)]
mod win {
    // 全 crate 唯一允许 unsafe 的地方：DragWindow / ShowWindowAsync 这些
    // Win32 调用没有安全封装可用（见文件头那两条上游缺陷）。两个调用点，
    // 都在本模块内的两个函数里，句柄拿不到就提前返回。
    #![allow(unsafe_code)]

    use gpui_kit::Window;
    // 必须显式引 trait 并用 UFCS 调用：`Window` 自己有个同名固有方法
    // `window_handle()`，返回的是 gpui 内部的 `AnyWindowHandle`，不是 HWND。
    // 直接写 `window.window_handle()` 会静默命中固有方法，然后类型对不上。
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::{
        HTCAPTION, IsZoomed, SW_MAXIMIZE, SW_RESTORE, SendMessageW, ShowWindowAsync,
        WM_NCLBUTTONDOWN,
    };

    /// 从 gpui 的窗口里取 Win32 句柄。取不到就返回 `None`，调用方回落到
    /// gpui 的实现（虽然那在 Windows 上也是空操作，但至少不 panic）。
    fn hwnd(window: &Window) -> Option<HWND> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(raw) => Some(HWND(raw.hwnd.get() as *mut core::ffi::c_void)),
            _ => None,
        }
    }

    /// 让系统接管拖动。返回 false 表示没拿到句柄，调用方该干别的。
    pub(super) fn begin_drag(window: &Window) -> bool {
        let Some(hwnd) = hwnd(window) else {
            return false;
        };
        unsafe {
            // 顺序不能反：先把自己对鼠标的捕获还回去，再"假装标题栏被按下"。
            // DefWindowProc 收到 `HTCAPTION` 的 `WM_NCLBUTTONDOWN` 会自己进入
            // 拖动循环，直到用户松手才返回 —— 所以这一句是阻塞整个拖拽过程的。
            let _ = ReleaseCapture();
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                Some(LPARAM(0)),
            );
        }
        true
    }

    /// 最大化 / 还原。返回 false 表示没拿到句柄。
    pub(super) fn toggle_maximize(window: &Window) -> bool {
        let Some(hwnd) = hwnd(window) else {
            return false;
        };
        unsafe {
            // 状态必须问系统，不能用 gpui 那份 `is_maximized()`：
            // 我们用 ShowWindow 改的窗口状态，gpui 的缓存要等下一次
            // `WM_SIZE` 回调才更新，这里读到的可能是旧值 —— 表现就是
            // "点了没反应，再点才还原"。
            let maximized = IsZoomed(hwnd).as_bool();
            let _ = ShowWindowAsync(hwnd, if maximized { SW_RESTORE } else { SW_MAXIMIZE });
        }
        true
    }
}
