//! Zex headless 二进制。
//!
//! 与 GUI (`zex`) 共享同一内核，只是 channel 的另一端（ADR-009）。
//! 约束：**不得有独立于 GUI 的能力分支**。

use std::process::ExitCode;

use tokio::sync::mpsc;
use zex_app::{build_agent, load};
use zex_core::CoreEvent;

const HELP: &str = r#"zex-cli — Zex 数字助理的 headless 入口

用法:
  zex-cli "<任务描述>"       执行一次任务，结果输出到 stdout
  zex-cli --version          显示版本
  zex-cli --help             显示本帮助

配置:
  读取 ~/.zex/config.json；也可用环境变量覆盖：
    ZEX_PROVIDER      anthropic | openai-compat
    ZEX_BASE_URL      如 https://api.anthropic.com
    ZEX_MODEL         如 claude-sonnet-4-6
    ZEX_API_KEY_ENV   存放密钥的环境变量名（默认 ANTHROPIC_API_KEY / OPENAI_API_KEY）

退出码:
  0  任务完成
  1  配置错误或任务失败
  2  参数错误
"#;

fn main() -> ExitCode {
    // CLI 需要 tokio runtime；GUI 用 gpui 的 executor，两者互不干扰。
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("无法创建运行时: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async_main())
}

async fn async_main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args[0] == "--version" || args[0] == "-V" {
        println!("zex-cli {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let prompt = args.join(" ");

    let cfg = match load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            return ExitCode::FAILURE;
        }
    };

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("无法获取当前目录: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut agent = match build_agent(&cfg, cwd) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("无法初始化内核: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 事件消费任务：把过程打到 stderr，把最终结果留给 stdout。
    // 这样 `zex-cli "..." > out.txt` 拿到的是干净的结果。
    let (tx, mut rx) = mpsc::channel::<CoreEvent>(256);
    let printer = tokio::spawn(async move {
        use std::io::Write;
        let mut first_token = true;
        while let Some(ev) = rx.recv().await {
            match ev {
                CoreEvent::Token { text, .. } => {
                    if first_token {
                        first_token = false;
                        eprintln!();
                    }
                    eprint!("{text}");
                    let _ = std::io::stderr().flush();
                }
                CoreEvent::Thinking { .. } => {}
                CoreEvent::ToolCallBegin {
                    name,
                    arguments,
                    risk,
                    ..
                } => {
                    eprintln!("\n[{}] {name} {}", risk.label(), compact(&arguments));
                }
                CoreEvent::ToolCallEnd { ok, brief, .. } => {
                    let mark = if ok { "ok" } else { "失败" };
                    eprintln!("  └─ {mark}: {brief}");
                }
                CoreEvent::TurnEnded { reason, usage, .. } => {
                    eprintln!(
                        "\n[{:?}] tokens: in {} / out {}",
                        reason, usage.input_tokens, usage.output_tokens
                    );
                }
                CoreEvent::Warning { message } => eprintln!("\n警告: {message}"),
                CoreEvent::Fatal { message } => eprintln!("\n致命错误: {message}"),
                _ => {}
            }
        }
    });

    let result = agent.run_turn(prompt, Some(&tx)).await;
    drop(tx);
    let _ = printer.await;

    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("任务失败: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 把 JSON 参数压成一行短摘要，供 CLI 打印。
fn compact(v: &serde_json::Value) -> String {
    let s = v.to_string();
    let s = s.replace('\n', " ");
    if s.chars().count() > 140 {
        format!("{}…", s.chars().take(140).collect::<String>())
    } else {
        s
    }
}
