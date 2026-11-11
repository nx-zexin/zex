//! Shell 命令执行工具。
//!
//! 风险等级是 L2（Impactful）：可以改变系统状态，因此需要审批。
//! 危险的命令识别**不在这里**做 —— 那是 `zex-policy` 的职责（见 SECURITY.md §10）。

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 默认超时。
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 超时硬上限。
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// 输出保留的最大字节数（超出则保留头尾）。
pub const MAX_OUTPUT_BYTES: usize = 60_000;

/// `shell` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// 要执行的命令。在 Linux/macOS 上通过 sh -c 执行，在 Windows 上通过 cmd /C 执行。
    pub command: String,
    /// 超时秒数，默认 120，上限 3600。长时间任务（编译、下载）请显式调大。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 工作目录。默认使用当前工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
}

/// 执行 shell 命令。
pub struct Shell;

#[async_trait]
impl Tool for Shell {
    const NAME: &'static str = "shell";
    const RISK: Risk = Risk::Impactful;
    type Args = ShellArgs;

    fn description() -> &'static str {
        "执行 shell 命令并返回输出。用于列目录、运行构建、测试、git 操作等。\
         非交互式：不要执行需要人工输入的命令（如 vim、需要确认的 apt）。\
         耗时任务请显式调大 timeout_secs。\
         需要读文件内容时优先用 fs_read，它能给出更清晰的行号。"
    }

    async fn call(&self, cx: &dyn ToolCtx, args: ShellArgs) -> Result<ToolOutput> {
        let timeout_secs = args
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let cwd = match &args.cwd {
            Some(c) => Some(cx.resolve_path(c)?),
            None => None,
        };

        let mut cmd = build_command(&args.command);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Windows 上避免控制台窗口闪现（见 docs/PLATFORM.md §5）
        #[cfg(windows)]
        {
            #[allow(unused_imports)]
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }

        let child = cmd.spawn().map_err(|e| Error::ToolFailed {
            tool: Self::NAME.into(),
            message: format!("无法启动命令: {e}"),
        })?;

        let output = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(Error::ToolFailed {
                    tool: Self::NAME.into(),
                    message: format!("命令执行失败: {e}"),
                });
            }
            Err(_) => {
                return Ok(ToolOutput::err(format!(
                    "命令在 {timeout_secs} 秒后超时被终止。若确实需要更久，请调大 timeout_secs（上限 {MAX_TIMEOUT_SECS}）。"
                )));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code();

        let mut body = String::new();
        if !stdout.trim().is_empty() {
            body.push_str(&truncate_output(&stdout));
        }
        // stderr 的标记必须始终加上：即使 stdout 为空，模型也需要知道
        // 这段输出来自 stderr（那是错误信息，含义完全不同）。
        if !stderr.trim().is_empty() {
            if !body.is_empty() {
                body.push_str("\n--- stderr ---\n");
            } else {
                body.push_str("--- stderr ---\n");
            }
            body.push_str(&truncate_output(&stderr));
        }
        if body.trim().is_empty() {
            body.push_str("(命令没有产生输出)");
        }

        let failed = !output.status.success();
        let header = match code {
            Some(c) if c != 0 => format!("[退出码 {c}]\n"),
            Some(_) => String::new(),
            None => "[被信号终止]\n".to_string(),
        };

        Ok(ToolOutput {
            content: format!("{header}{body}"),
            is_error: failed,
            meta: Some(serde_json::json!({
                "exit_code": code,
                "stdout_bytes": output.stdout.len(),
                "stderr_bytes": output.stderr.len(),
            })),
        })
    }
}

/// 按平台构造命令。
fn build_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// 超长输出保留头尾，中间标注省略量。
///
/// 头尾都留是因为：开头通常是错误信息或命令回显，结尾通常是最终结果。
/// 只留开头会丢掉结论，只留结尾会丢掉错误。
pub fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    let head_len = MAX_OUTPUT_BYTES * 2 / 3;
    let tail_len = MAX_OUTPUT_BYTES / 3;
    // 按 char 边界切，避免切碎 UTF-8
    let head: String = s.chars().take(head_len).collect();
    let tail_rev: String = s.chars().rev().take(tail_len).collect();
    let tail: String = tail_rev.chars().rev().collect();
    format!(
        "{head}\n\n... [省略中间内容，原始 {} 字节] ...\n\n{tail}",
        s.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use zex_core::SessionId;

    struct Ctx;
    impl ToolCtx for Ctx {
        fn cwd(&self) -> &Path {
            Path::new("/tmp")
        }
        fn session_id(&self) -> &SessionId {
            static ID: std::sync::OnceLock<SessionId> = std::sync::OnceLock::new();
            ID.get_or_init(|| SessionId::new("ses_test"))
        }
        fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
            Ok(PathBuf::from(raw))
        }
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let out = Shell
            .call(
                &Ctx,
                ShellArgs {
                    command: "echo zex-ok".into(),
                    timeout_secs: None,
                    cwd: None,
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("zex-ok"));
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn nonzero_exit_is_error() {
        let out = Shell
            .call(
                &Ctx,
                ShellArgs {
                    command: "exit 3".into(),
                    timeout_secs: None,
                    cwd: None,
                },
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("退出码 3"));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let out = Shell
            .call(
                &Ctx,
                ShellArgs {
                    command: "echo oops 1>&2".into(),
                    timeout_secs: None,
                    cwd: None,
                },
            )
            .await
            .unwrap();
        assert!(out.content.contains("oops"));
        assert!(out.content.contains("stderr"));
    }

    #[tokio::test]
    async fn timeout_is_reported() {
        let out = Shell
            .call(
                &Ctx,
                ShellArgs {
                    command: "sleep 5".into(),
                    timeout_secs: Some(1),
                    cwd: None,
                },
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("超时"));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "A".repeat(MAX_OUTPUT_BYTES + 1000);
        let out = truncate_output(&s);
        assert!(out.len() < s.len());
        assert!(out.contains("省略中间内容"));
        assert!(out.starts_with('A'));
        assert!(out.ends_with('A'));
    }

    #[test]
    fn truncate_leaves_small_output_intact() {
        assert_eq!(truncate_output("short"), "short");
    }

    #[test]
    fn truncate_does_not_break_utf8() {
        // 中文按 char 切，不能切出非法 UTF-8
        let s = "中".repeat(MAX_OUTPUT_BYTES);
        let out = truncate_output(&s);
        // 能正常遍历说明没有非法序列
        assert!(out.chars().count() > 0);
    }

    #[test]
    fn risk_is_impactful() {
        assert_eq!(Shell::RISK, Risk::Impactful);
    }
}
