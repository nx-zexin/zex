//! 把 zex.ico 以资源 ID 1 嵌进 Windows 二进制。
//!
//! 为什么是 ID 1：gpui-pre-windows 的 `load_icon()` 用
//! `LoadImageW(module, MAKEINTRESOURCE(1), IMAGE_ICON, ...)` 取图标，
//! 失败就退到空句柄，窗口/任务栏全走系统默认图标。
//! 嵌了 ID 1，Explorer 的 exe 图标、运行窗口、任务栏三处就都有图了。
//!
//! Linux/macOS 不需要这一步：X11 走 `WindowOptions::icon`（运行时像素），
//! Wayland 走 `.desktop` 的 `Icon=zex`，macOS 走 app bundle。

fn main() {
    // build script 的 CWD 是本 crate 根目录
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    let resource_dir = std::path::Path::new("resources/windows");
    let rc_file = resource_dir.join("zex.rc");
    let ico_file = resource_dir.join("zex.ico");
    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!("cargo:rerun-if-changed={}", ico_file.display());
    let _ = embed_resource::compile(rc_file, embed_resource::ParamsIncludeDirs([resource_dir]));
}
