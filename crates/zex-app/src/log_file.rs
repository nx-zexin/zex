//! 把日志写进文件：`~/.zex/logs/zex.log`。
//!
//! # 为什么需要
//!
//! 终端里那份日志（`tracing_subscriber::fmt`）只在"有人看着终端"时才
//! 有意义。GUI 程序偏偏是最没有终端的形态：
//!
//! - Windows 上二进制是 windows 子系统，双击启动根本没有控制台 ——
//!   stdout/stderr 没有去处，日志直接丢；
//! - Linux/macOS 上从启动器打开，或关掉那个终端窗口，同理。
//!
//! 于是"用户说点了没反应"，开发者手上一条线索都没有。文件日志是唯一
//! 能事后回看的地方。
//!
//! # 轮转
//!
//! 单个文件到 [`MAX_BYTES`] 就改名成 `zex.log.1`（只留一份旧的）。
//! 没人会为了清日志专门去看一眼，所以上限必须自己管 —— 一个跑半年的
//! 桌面应用能把日志写到几个 GB。
//!
//! 不做按天分割：桌面程序不是每天都开，按天切会留下一堆几百字节的空文件。

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 单个日志文件的上限。8 MiB 大约是这个应用几十天的 `warn` 级输出。
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 文件名。轮转时变成 `zex.log.1`。
pub const FILE_NAME: &str = "zex.log";

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// 日志文件的落点：`<配置目录>/logs/zex.log`。
pub fn path() -> PathBuf {
    super::config::config_dir().join("logs").join(FILE_NAME)
}

/// 打开日志文件并装好写入器。返回实际写入的路径。
///
/// 失败只返回 `None`：日志写不了不该让应用起不来（磁盘满、目录只读
/// 都发生过）。调用方照旧只用终端那一层。
pub fn init() -> Option<PathBuf> {
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);

    // 已经存在就不再替换（init 只该调一次）
    let _ = SINK.set(Mutex::new(Sink {
        path: path.clone(),
        file,
        written,
    }));
    Some(path)
}

/// 给 `tracing_subscriber` 的 `fmt::layer().with_writer(..)` 用。
///
/// 没初始化成功时返回一个丢弃写入的 writer，这样调用方不用分支。
pub fn writer() -> MakeSink {
    MakeSink
}

/// 见 [`writer`]。
pub struct MakeSink;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeSink {
    type Writer = SinkWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SinkWriter(
            SINK.get()
                .map(|m| m.lock().unwrap_or_else(|e| e.into_inner())),
        )
    }
}

/// 一次写入的句柄。拿不到锁（没初始化 / 中毒）就静默丢弃。
pub struct SinkWriter<'a>(Option<std::sync::MutexGuard<'a, Sink>>);

impl Write for SinkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.as_mut() {
            Some(sink) => sink.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.as_mut() {
            Some(sink) => sink.flush(),
            None => Ok(()),
        }
    }
}

/// 真正持有文件的那一层。
struct Sink {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
}

impl Sink {
    /// 把当前文件挪成 `.1`，开一个新的。
    ///
    /// 失败就继续往旧文件里写 —— 日志比整洁重要。
    fn rotate(&mut self) {
        let _ = self.file.flush();
        let rotated = self.path.with_file_name(format!("{FILE_NAME}.1"));
        let _ = std::fs::remove_file(&rotated);
        if std::fs::rename(&self.path, &rotated).is_err() {
            return;
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = file;
            self.written = 0;
        }
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() as u64 > MAX_BYTES {
            self.rotate();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 直接造一个指向临时目录的 Sink，绕开 `path()` 里的全局配置目录。
    fn sink_at(dir: &Path) -> Sink {
        let path = dir.join(FILE_NAME);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        Sink {
            path,
            file,
            written: 0,
        }
    }

    #[test]
    fn writes_append_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = sink_at(dir.path());
        sink.write_all(b"first\n").unwrap();
        sink.write_all(b"second\n").unwrap();
        sink.flush().unwrap();

        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(text, "first\nsecond\n");
    }

    #[test]
    fn rotates_when_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = sink_at(dir.path());
        // 假装已经写满
        sink.written = MAX_BYTES;
        sink.write_all(b"after rotate\n").unwrap();
        sink.flush().unwrap();

        let current = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(current, "after rotate\n", "新文件只该有轮转后的内容");
        let rotated = dir.path().join(format!("{FILE_NAME}.1"));
        assert!(rotated.exists(), "旧内容该被挪到 .1");
    }

    #[test]
    fn only_one_old_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = sink_at(dir.path());
        for round in 0..3 {
            sink.written = MAX_BYTES;
            sink.write_all(format!("round {round}\n").as_bytes())
                .unwrap();
            sink.flush().unwrap();
        }
        let rotated = dir.path().join(format!("{FILE_NAME}.1"));
        let text = std::fs::read_to_string(&rotated).unwrap();
        assert_eq!(text, "round 1\n", "只留最近一次轮转的内容");
    }
}
