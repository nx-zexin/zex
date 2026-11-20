//! 系统剪贴板工具：`clipboard_read` / `clipboard_write`。
//!
//! # 为什么不用 shell
//!
//! 读剪贴板本来可以 `wl-paste` / `xclip` / `pbpaste` / `Get-Clipboard`
//! 各写一条，但那样四套平台四种二进制，发行版还不一定装了 —— 一个
//! "读剪贴板"的能力不该以"你装没装 xclip"为前提。所以用 `arboard`：
//! X11 / Wayland / macOS / Windows 各自实现，同一份代码。
//!
//! # 线程
//!
//! `arboard::Clipboard` 不是 `Send`，而且 Linux 上创建它会连 X/Wayland
//! 服务端。所以每次调用在 `spawn_blocking` 里现建现用、用完就丢 ——
//! 拿它当长驻对象会在窗口销毁、会话切换时变成一堆悬空连接。

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::task::spawn_blocking;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 单次读取的字符硬上限。
///
/// 剪贴板里塞一整份日志很常见，而工具结果会进上下文。20 万字符
/// （约 5 万 token）已经超出任何一次有用的判断所需，再多就是拿窗口
/// 换噪音 —— 截断处写明原始长度，模型可以据此改问"只看开头"。
pub const MAX_CHARS: usize = 200_000;

// ─── clipboard_read ───

/// `clipboard_read` 没有参数。
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReadArgs {}

/// 读取系统剪贴板里的文本。
pub struct ClipboardRead;

#[async_trait]
impl Tool for ClipboardRead {
    const NAME: &'static str = "clipboard_read";
    const RISK: Risk = Risk::ReadOnly;
    type Args = ReadArgs;

    fn description() -> &'static str {
        "读取系统剪贴板里的文本。用户说「我复制了一段…」「看下我粘贴板」时用它，\
         不要让他再贴一遍。只能读文本：图片、文件（含路径列表）读不出来，\
         那种情况请让用户把文件存到磁盘再给路径。空剪贴板返回空字符串。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, _args: ReadArgs) -> Result<ToolOutput> {
        let text = read_text().await?;
        if text.is_empty() {
            return Ok(ToolOutput::ok("（剪贴板是空的，或者里面不是文本）"));
        }
        let chars = text.chars().count();
        if chars > MAX_CHARS {
            let head: String = text.chars().take(MAX_CHARS).collect();
            return Ok(ToolOutput::ok(format!(
                "{head}\n\n... [剪贴板共 {chars} 字符，只给了前 {MAX_CHARS} 字符] ..."
            )));
        }
        Ok(ToolOutput::ok(text))
    }
}

// ─── clipboard_write ───

/// `clipboard_write` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// 要放进剪贴板的文本。会被整体替换掉原内容。
    pub text: String,
}

/// 把文本写进系统剪贴板。
pub struct ClipboardWrite;

#[async_trait]
impl Tool for ClipboardWrite {
    const NAME: &'static str = "clipboard_write";
    // L2 而不是 L1：文件写错了还能从快照里捞回来，剪贴板被覆盖就是覆盖了 ——
    // 用户原来复制的东西没有任何地方留着。
    const RISK: Risk = Risk::Impactful;
    type Args = WriteArgs;

    fn description() -> &'static str {
        "把一段文本放进系统剪贴板，方便用户直接粘贴。\
         只在用户明确要「复制这个」「放到剪贴板」时用；\
         它会覆盖用户原来复制的内容。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: WriteArgs) -> Result<ToolOutput> {
        let len = args.text.chars().count();
        write_text(args.text).await?;
        Ok(ToolOutput::ok(format!("已写入剪贴板（{len} 字符）")))
    }
}

// ─── 后端 ───

/// 读一段文本；空剪贴板或非文本内容都返回空串。
async fn read_text() -> Result<String> {
    run(move || arboard::Clipboard::new().map_err(err_text)?.get_text().map_err(err_text)).await
}

/// 写入文本。
async fn write_text(text: String) -> Result<()> {
    run(move || {
        arboard::Clipboard::new()
            .map_err(err_text)?
            .set_text(text)
            .map_err(err_text)
    })
    .await
}

/// 在阻塞线程里跑一段剪贴板操作，并把平台错误翻译成人话。
///
/// 报错要能指导下一步：无头环境（CI、SSH）和"Wayland 没给权限"
/// 是两回事，混成一句 `Clipboard error` 用户没法自救。
async fn run<T, F>(job: F) -> Result<T>
where
    F: FnOnce() -> std::result::Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    spawn_blocking(job)
        .await
        .map_err(|e| Error::ToolFailed {
            tool: "clipboard".into(),
            message: format!("剪贴板线程崩了：{e}"),
        })?
        .map_err(|message| Error::ToolFailed {
            tool: "clipboard".into(),
            message,
        })
}

/// 平台错误 → 一句能照做的说明。
fn err_text(e: arboard::Error) -> String {
    match e {
        arboard::Error::ClipboardNotSupported => {
            "这个环境没有可用的剪贴板（无头会话 / 没有图形界面时正常）。\
             请让用户把内容直接贴到对话里，或存成文件给路径。"
                .to_string()
        }
        arboard::Error::ClipboardOccupied => {
            "剪贴板被别的程序占着，稍后重试一次；还不行就让用户手动粘贴。".to_string()
        }
        other => format!("剪贴板不可用：{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_tool_has_no_arguments() {
        // 零参数的工具也必须有 Args 类型，否则注册表拿不到 schema
        let schema = schemars::schema_for!(ReadArgs);
        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["type"], "object");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_display_fails_with_a_readable_message() {
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
        {
            return;
        }
        let err = read_text().await.unwrap_err().to_string();
        assert!(err.contains("剪贴板"), "报错要提到剪贴板：{err}");
    }

    /// 真机上的往返验证：写进去、读回来。
    ///
    /// 默认 `#[ignore]`：它要动用户真实的剪贴板，还会把他原来复制的内容
    /// 顶掉 —— CI 和日常 `cargo test` 都不该干这个。需要时手动跑：
    /// `cargo test -p zex-tools -- --ignored manual_clipboard_roundtrip`
    #[tokio::test]
    #[ignore = "会覆盖真实剪贴板，只在手动验证平台后端时跑"]
    async fn manual_clipboard_roundtrip() {
        let stamp = format!("zex-clipboard-probe-{}", std::process::id());
        write_text(stamp.clone()).await.expect("写剪贴板失败");
        let got = read_text().await.expect("读剪贴板失败");
        assert_eq!(got, stamp);
    }
}
