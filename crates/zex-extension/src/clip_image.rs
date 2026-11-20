//! 剪贴板里的图片：读出来、存成缩略文件、再放回去。
//!
//! # 为什么必须由宿主来做
//!
//! gpui-shell 给脚本的 `cx.read_from_clipboard()` 只回答**文本**
//! （`ClipboardItem::text()` 只拼 `String` 和 `ExternalPaths` 两种条目，
//! 遇到 `Image` 直接跳过），所以截图复制进剪贴板之后，脚本那头看到的是
//! `undefined` —— 和"剪贴板是空的"完全一样，没法区分，也就没法记录。
//!
//! 脚本也不能读文件（`fs` 能力没开），更不能把字节画出来（`image()` 只接受
//! 资产根内的相对路径，`data:` 和绝对路径都会被拒）。所以这条路只有宿主能走：
//!
//! ```text
//! 剪贴板 Image 条目 ──(这里)──> ~/.zex/state/clip-thumbs/<hash>.<ext>
//!                                       │
//!                                       └─> image("clip-thumbs/<hash>.<ext>")
//! ```
//!
//! 中间那个文件是给 `image()` 用的：zex 的资源源（见 zex-app 的 ZexAssets）
//! 会把 `clip-thumbs/` 前缀映射到这个目录。
//!
//! # 文件名就是内容哈希
//!
//! 同一张图重复复制不会写第二份，也不需要额外的去重表；删了文件再复制同一张
//! 图，会重新写一份、名字依旧一样。脚本侧据此判断"这条是不是新的"。

use std::io::Write as _;
use std::path::{Path, PathBuf};

use gpui_shell::gpui::{ClipboardEntry, ImageFormat};

/// 存缩略图的子目录名。脚本里的 `image()` 路径就是这个前缀。
pub const DIR_NAME: &str = "clip-thumbs";

/// 保留多少个缩略文件。截图一多也就几十张，超出的按修改时间删最旧的。
const KEEP_FILES: usize = 240;

/// 单张图的上限。再大就不值得往界面上贴，也免得一次读盘把面板卡住。
const MAX_BYTES: usize = 32 * 1024 * 1024;

/// 缩略图落在哪儿：`~/.zex/state/clip-thumbs`。
pub fn dir() -> PathBuf {
    crate::catalog::state_dir().join(DIR_NAME)
}

/// 一次抓取的结果。
#[derive(Clone, Debug)]
pub struct Captured {
    /// 相对路径，直接交给脚本的 `image()`。
    pub rel_path: String,
    /// 像素宽。非 PNG 时是 0（不解析，界面按默认比例画）。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// 原始字节数，仅供界面显示"这张图多大"。
    pub bytes: usize,
}

/// 从剪贴板捞一张图出来存盘。没有图（或图太大）时返回 `None`。
///
/// `item` 是 `App::read_from_clipboard()` 的产物 —— 调用方负责拿，这里只管
/// 挑出 `Image` 条目。
pub fn capture(item: Option<gpui_shell::gpui::ClipboardItem>) -> Option<Captured> {
    let item = item?;

    // 剪贴板里可能同时有文本和图片（有些工具两种都给）。这里只要图。
    let image = item.entries().iter().find_map(|entry| match entry {
        ClipboardEntry::Image(image) => Some(image),
        _ => None,
    })?;

    store(image.format, &image.bytes)
}

/// 把一段图片字节存成缩略文件。给后台监视线程用（那边拿不到 `Image`，
/// 只有裸字节 + 推断出来的格式）。
pub fn store(format: ImageFormat, bytes: &[u8]) -> Option<Captured> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }

    let ext = format.extension();
    let name = format!("{:016x}.{}", hash_bytes(bytes), ext);
    let dir = dir();
    let target = dir.join(&name);

    // 已经存过就不重复写：同一张图复制一百次也只有一份文件
    if !target.exists() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "无法创建剪贴板图片目录");
            return None;
        }
        // 先写临时文件再改名：中途崩了不会留下半张图让 image() 去解析
        let tmp = dir.join(format!("{name}.tmp"));
        let written = std::fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &target)) {
            tracing::warn!(path = %target.display(), error = %e, "写入剪贴板图片失败");
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        prune(&dir);
    }

    let (width, height) = dimensions(bytes, format);
    Some(Captured {
        rel_path: format!("{DIR_NAME}/{name}"),
        width,
        height,
        bytes: bytes.len(),
    })
}

/// 从 MIME 类型猜图像格式。
pub fn format_of_mime(mime: &str) -> ImageFormat {
    ImageFormat::from_mime_type(mime).unwrap_or(ImageFormat::Png)
}

/// 把一条记录的资产路径解成一个安全的文件名。
fn name_of(rel_path: &str) -> Option<&str> {
    let name = rel_path.strip_prefix(DIR_NAME)?.trim_start_matches('/');
    // 只认这一层里的普通文件名，别让脚本用 ../ 指向别处
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    Some(name)
}

/// 读出缩略图的原始字节，用来放回剪贴板。
///
/// 返回字节而不是 `gpui::Image`：写剪贴板那条路走的是 `clip_watch` 自己的
/// 服务线程（GPUI 的 Wayland 实现在这里只能写文本），所以这里不该把
/// 渲染层的类型引进来。
pub fn bytes_for(rel_path: &str) -> Option<Vec<u8>> {
    let name = name_of(rel_path)?;
    let bytes = std::fs::read(dir().join(name)).ok()?;
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// 由资产路径推断图像格式（看扩展名，认不出再看魔数）。
pub fn format_of_name(rel_path: &str) -> ImageFormat {
    let name = name_of(rel_path).unwrap_or("");
    let path = Path::new(name);
    let Ok(bytes) = std::fs::read(dir().join(name)) else {
        return ImageFormat::Png;
    };
    format_of(path, &bytes)
}

/// 删掉一条记录对应的缩略文件。
pub fn forget(rel_path: &str) {
    let Some(name) = rel_path.strip_prefix(DIR_NAME) else {
        return;
    };
    let name = name.trim_start_matches('/');
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return;
    }
    let _ = std::fs::remove_file(dir().join(name));
}

// ---------- 内部 ----------

/// FNV-1a。够用：只拿来做文件名，不承担安全职责。
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// 从文件头读宽高。只认 PNG（截图都是 PNG），其余返回 0 ——
/// 界面拿到 0 就按固定宽高比画，不会因此错位。
fn dimensions(bytes: &[u8], format: ImageFormat) -> (u32, u32) {
    if format != ImageFormat::Png || bytes.len() < 24 {
        return (0, 0);
    }
    // IHDR 的宽高在 PNG 签名(8) + 长度(4) + "IHDR"(4) 之后，各 4 字节大端
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w, h)
}

fn format_of(path: &Path, bytes: &[u8]) -> ImageFormat {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "bmp" => ImageFormat::Bmp,
        "tiff" | "tif" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        "pnm" => ImageFormat::Pnm,
        _ => {
            // 扩展名认不出来就看魔数，别默认当成 PNG 让解码器去猜
            if bytes.starts_with(&[0xFF, 0xD8]) {
                ImageFormat::Jpeg
            } else if bytes.starts_with(b"RIFF") {
                ImageFormat::Webp
            } else if bytes.starts_with(b"GIF8") {
                ImageFormat::Gif
            } else {
                ImageFormat::Png
            }
        }
    }
}

/// 超出 KEEP_FILES 就按修改时间从旧到新删。
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if !path.is_file() {
                return None;
            }
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();

    if files.len() <= KEEP_FILES {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    let drop_count = files.len() - KEEP_FILES;
    for (_, path) in files.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_and_distinct() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
    }

    #[test]
    fn reads_png_dimensions() {
        // 最小 PNG 头：签名 + IHDR 长度 + "IHDR" + 宽 40 高 30
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&40u32.to_be_bytes());
        bytes.extend_from_slice(&30u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0]);
        assert_eq!(dimensions(&bytes, ImageFormat::Png), (40, 30));
    }

    #[test]
    fn non_png_reports_no_dimensions() {
        let bytes = vec![0xFF, 0xD8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(dimensions(&bytes, ImageFormat::Jpeg), (0, 0));
    }

    #[test]
    fn rejects_paths_that_escape_the_directory() {
        assert!(bytes_for("clip-thumbs/../secret.png").is_none());
        assert!(bytes_for("clip-thumbs/a/b.png").is_none());
        assert!(bytes_for("other/x.png").is_none());
        assert!(bytes_for("clip-thumbs/").is_none());
    }

    #[test]
    fn detects_format_from_magic_bytes() {
        assert_eq!(format_of(Path::new("x"), &[0xFF, 0xD8, 0, 0]), ImageFormat::Jpeg);
        assert_eq!(format_of(Path::new("x"), b"RIFF____"), ImageFormat::Webp);
        assert_eq!(format_of(Path::new("x"), b"GIF89a"), ImageFormat::Gif);
        assert_eq!(format_of(Path::new("x"), b"\x89PNG"), ImageFormat::Png);
    }
}
