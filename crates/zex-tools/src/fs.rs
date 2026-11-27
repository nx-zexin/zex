//! 文件系统工具：`fs_read` / `fs_write`。

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 单次读取的默认行数上限。
pub const DEFAULT_LIMIT: usize = 2000;

/// 单次读取的硬上限（防止一次把 context 塞满）。
pub const MAX_LIMIT: usize = 20_000;

/// 单次读取的字节硬上限。
pub const MAX_BYTES: usize = 512 * 1024;

// ─── fs_read ───

/// `fs_read` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// 文件路径。可为绝对路径，或相对当前工作目录。
    pub path: String,
    /// 起始行号，从 1 开始。大文件请配合 limit 分段读取。
    #[serde(default)]
    pub offset: Option<usize>,
    /// 最多读取多少行，默认 2000，上限 20000。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// 读取文件内容。
pub struct FsRead;

#[async_trait]
impl Tool for FsRead {
    const NAME: &'static str = "fs_read";
    const RISK: Risk = Risk::ReadOnly;
    const HINT: Option<&'static str> =
        Some("读文件用它，返回带行号；不要用 shell 的 cat 代替，行号是后续精确修改的依据");
    type Args = ReadArgs;

    fn description() -> &'static str {
        "读取文件内容。返回带行号的文本，便于后续精确修改。\
         大文件请用 offset/limit 分段读取，不要试图一次读完。\
         目录不是文件，请用 shell 的 ls。"
    }

    async fn call(&self, cx: &dyn ToolCtx, args: ReadArgs) -> Result<ToolOutput> {
        let path = cx.resolve_path(&args.path)?;

        // 密钥文件永不进入模型上下文（SECURITY.md §5.3）
        if cx.is_secret_path(&path) {
            return Err(Error::Denied(format!(
                "`{}` 命中密钥黑名单，不允许读取",
                path.display()
            )));
        }

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("无法访问 `{}`: {e}", path.display()),
            })?;

        if meta.is_dir() {
            return Ok(ToolOutput::err(format!(
                "`{}` 是目录，不是文件。列目录请用 shell 的 ls。",
                path.display()
            )));
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("读取 `{}` 失败: {e}", path.display()),
            })?;

        let truncated_bytes = bytes.len() > MAX_BYTES;
        let slice = if truncated_bytes {
            &bytes[..MAX_BYTES]
        } else {
            &bytes[..]
        };

        // 二进制检测：含 NUL 就认定为二进制
        if slice.iter().take(8192).any(|&b| b == 0) {
            return Ok(ToolOutput::err(format!(
                "`{}` 看起来是二进制文件（{} 字节），无法作为文本读取。",
                path.display(),
                meta.len()
            )));
        }

        let text = String::from_utf8_lossy(slice);
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        let mut out = String::new();
        let mut emitted = 0usize;

        // 行号宽度按实际总行数算。固定 6 位是给"百万行"留的余量，
        // 但绝大多数文件只有几十上百行，白占 5 个字符的缩进 ——
        // 在卡片里看是左边一大条空白，对模型也是每行白付几个 token。
        let total_lines = text.lines().count();
        let num_width = total_lines.to_string().len().max(2);

        for (i, line) in text.lines().enumerate() {
            if i + 1 < offset {
                continue;
            }
            if emitted >= limit {
                continue; // 继续数行数以给出总数
            }
            out.push_str(&format!(
                "{:>num_width$}\t{}\n",
                i + 1,
                line,
                num_width = num_width
            ));
            emitted += 1;
        }

        if emitted == 0 {
            return Ok(ToolOutput::err(format!(
                "`{}` 没有第 {offset} 行（共 {total_lines} 行）。",
                path.display()
            )));
        }

        let mut notes = Vec::new();
        if offset > 1 || emitted < total_lines {
            notes.push(format!(
                "本次显示第 {offset}–{} 行，共 {total_lines} 行",
                offset + emitted - 1
            ));
        }
        if truncated_bytes {
            notes.push(format!(
                "文件超过 {MAX_BYTES} 字节，已截断；建议用 offset/limit 分段读取"
            ));
        }

        if !notes.is_empty() {
            out.push_str(&format!("\n[{}]\n", notes.join("；")));
        }

        Ok(ToolOutput::ok(out).with_meta(serde_json::json!({
            "path": path.display().to_string(),
            "lines": total_lines,
            "bytes": meta.len(),
            "truncated": truncated_bytes,
        })))
    }
}

// ─── fs_write ───

/// `fs_write` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// 文件路径。可为绝对路径，或相对当前工作目录。父目录会自动创建。
    pub path: String,
    /// 要写入的完整内容（覆盖已有内容）。
    pub content: String,
    /// 若为 true，则追加而不是覆盖。
    #[serde(default)]
    pub append: bool,
}

/// 写入文件。
pub struct FsWrite;

#[async_trait]
impl Tool for FsWrite {
    const NAME: &'static str = "fs_write";
    const RISK: Risk = Risk::Reversible;
    // 没有"替换第 N 行"的工具，只有整体写回。这条必须说出来，
    // 否则模型会按编辑器的直觉只写改动的那几行，把文件截断。
    const HINT: Option<&'static str> =
        Some("整文件写回：改之前先 fs_read 看清原文，再交出完整内容；漏掉的部分就是删掉了");
    type Args = WriteArgs;

    fn description() -> &'static str {
        "写入文件。默认覆盖整个文件，append=true 时追加。\
         父目录不存在会自动创建。修改已有文件的局部内容时，\
         建议先 fs_read 看清原文再整体写回，避免误删相邻代码。"
    }

    async fn call(&self, cx: &dyn ToolCtx, args: WriteArgs) -> Result<ToolOutput> {
        let path = cx.resolve_path(&args.path)?;

        if cx.is_secret_path(&path) {
            return Err(Error::Denied(format!(
                "`{}` 命中密钥黑名单，不允许写入",
                path.display()
            )));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| Error::ToolFailed {
                        tool: Self::NAME.into(),
                        message: format!("创建目录 `{}` 失败: {e}", parent.display()),
                    })?;
            }
        }

        let existing = tokio::fs::metadata(&path).await.ok().map(|m| m.len());

        let write_result = if args.append {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|e| Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: format!("打开 `{}` 失败: {e}", path.display()),
                })?;
            f.write_all(args.content.as_bytes())
                .await
                .map_err(|e| Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: format!("写入 `{}` 失败: {e}", path.display()),
                })?;
            f.flush().await.map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("flush `{}` 失败: {e}", path.display()),
            })?;
        } else {
            tokio::fs::write(&path, args.content.as_bytes())
                .await
                .map_err(|e| Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: format!("写入 `{}` 失败: {e}", path.display()),
                })?;
        };
        let _ = write_result;

        let lines = args.content.lines().count();
        let action = if args.append { "追加" } else { "写入" };
        Ok(ToolOutput::ok(format!(
            "已{action} `{}`：{} 行，{} 字节{}",
            path.display(),
            lines,
            args.content.len(),
            existing
                .map(|n| format!("（原大小 {n} 字节）"))
                .unwrap_or_default()
        ))
        .with_meta(serde_json::json!({
            "path": path.display().to_string(),
            "bytes": args.content.len(),
            "lines": lines,
            "append": args.append,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use zex_core::SessionId;

    struct Ctx {
        root: PathBuf,
    }

    impl ToolCtx for Ctx {
        fn cwd(&self) -> &Path {
            &self.root
        }
        fn session_id(&self) -> &SessionId {
            static ID: std::sync::OnceLock<SessionId> = std::sync::OnceLock::new();
            ID.get_or_init(|| SessionId::new("ses_test"))
        }
        fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
            let p = Path::new(raw);
            Ok(if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.root.join(p)
            })
        }
        fn is_secret_path(&self, path: &Path) -> bool {
            path.file_name().is_some_and(|n| n == ".env")
        }
    }

    fn tmp() -> (tempfile::TempDir, Ctx) {
        let d = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            root: d.path().to_path_buf(),
        };
        (d, ctx)
    }

    #[tokio::test]
    async fn read_returns_line_numbers() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join("a.txt"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let out = FsRead
            .call(
                &ctx,
                ReadArgs {
                    path: "a.txt".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("1\tone"));
        assert!(out.content.contains("3\tthree"));
    }

    #[tokio::test]
    async fn read_respects_offset_and_limit() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join("a.txt"), "1\n2\n3\n4\n5\n")
            .await
            .unwrap();
        let out = FsRead
            .call(
                &ctx,
                ReadArgs {
                    path: "a.txt".into(),
                    offset: Some(2),
                    limit: Some(2),
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("2\t2"));
        assert!(out.content.contains("3\t3"));
        assert!(!out.content.contains("4\t4"));
        assert!(out.content.contains("共 5 行"));
    }

    #[tokio::test]
    async fn read_refuses_secret_files() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join(".env"), "SECRET=1")
            .await
            .unwrap();
        let e = FsRead
            .call(
                &ctx,
                ReadArgs {
                    path: ".env".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Denied(_)));
    }

    #[tokio::test]
    async fn read_reports_binary() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join("b.bin"), [0u8, 1, 2, 3])
            .await
            .unwrap();
        let out = FsRead
            .call(
                &ctx,
                ReadArgs {
                    path: "b.bin".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("二进制"));
    }

    #[tokio::test]
    async fn read_directory_hint() {
        let (_d, ctx) = tmp();
        tokio::fs::create_dir(ctx.root.join("sub")).await.unwrap();
        let out = FsRead
            .call(
                &ctx,
                ReadArgs {
                    path: "sub".into(),
                    offset: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("是目录"));
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let (_d, ctx) = tmp();
        let out = FsWrite
            .call(
                &ctx,
                WriteArgs {
                    path: "x/y/z.txt".into(),
                    content: "hi".into(),
                    append: false,
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("已写入"));
        assert_eq!(
            tokio::fs::read_to_string(ctx.root.join("x/y/z.txt"))
                .await
                .unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn write_append_keeps_existing() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join("a.txt"), "A").await.unwrap();
        FsWrite
            .call(
                &ctx,
                WriteArgs {
                    path: "a.txt".into(),
                    content: "B".into(),
                    append: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(ctx.root.join("a.txt"))
                .await
                .unwrap(),
            "AB"
        );
    }

    #[tokio::test]
    async fn write_overwrites_by_default() {
        let (_d, ctx) = tmp();
        tokio::fs::write(ctx.root.join("a.txt"), "OLD CONTENT")
            .await
            .unwrap();
        FsWrite
            .call(
                &ctx,
                WriteArgs {
                    path: "a.txt".into(),
                    content: "NEW".into(),
                    append: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(ctx.root.join("a.txt"))
                .await
                .unwrap(),
            "NEW"
        );
    }

    #[tokio::test]
    async fn write_refuses_secret_files() {
        let (_d, ctx) = tmp();
        let e = FsWrite
            .call(
                &ctx,
                WriteArgs {
                    path: ".env".into(),
                    content: "x".into(),
                    append: false,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Denied(_)));
    }

    #[test]
    fn risk_levels_are_correct() {
        assert_eq!(FsRead::RISK, Risk::ReadOnly);
        assert_eq!(FsWrite::RISK, Risk::Reversible);
    }
}
