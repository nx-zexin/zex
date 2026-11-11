//! Zex headless 二进制。
//!
//! 与 GUI (`zex`) 共享同一内核，只是 channel 的另一端（ADR-009）。
//! 约束：**不得有独立于 GUI 的能力分支** —— CLI 支持的，GUI 必须支持；
//! 区别只在审批交互的呈现方式（GUI 弹窗 / CLI 阻塞等 stdin）。

use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use zex_agent::ApprovalDecision;
use zex_app::{build_agent, build_ephemeral_agent, load};
use zex_core::{CoreEvent, RememberScope};

const HELP: &str = r#"zex-cli — Zex 数字助理的 headless 入口

用法:
  zex-cli [选项] "<任务描述>"

选项:
  --ephemeral    使用内存数据库，不写入 ~/.zex/zex.db
  --plan         计划模式（只读），任何会改变状态的操作都会被拒绝
  --version      显示版本
  --help         显示本帮助

配置:
  读取 ~/.zex/config.json；也可用环境变量覆盖：
    ZEX_PROVIDER      anthropic | openai-compat
    ZEX_BASE_URL      如 https://api.anthropic.com
    ZEX_MODEL         如 claude-sonnet-4-6
    ZEX_API_KEY_ENV   存放密钥的环境变量名（默认 ANTHROPIC_API_KEY / OPENAI_API_KEY）

审批:
  有副作用的操作会在此终端询问：[y] 批准 / [n] 拒绝 / [a] 批准并记住此类。
  无人应答（300 秒）按拒绝处理。

退出码:
  0  任务完成
  1  配置错误或任务失败
  2  参数错误
"#;

fn main() -> ExitCode {
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

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("zex-cli {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let ephemeral = args.iter().any(|a| a == "--ephemeral");
    let plan = args.iter().any(|a| a == "--plan");
    args.retain(|a| !a.starts_with("--"));
    if args.is_empty() {
        eprintln!("缺少任务描述。用 --help 查看用法。");
        return ExitCode::FAILURE;
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

    let built = if ephemeral {
        build_ephemeral_agent(&cfg, cwd)
    } else {
        build_agent(&cfg, cwd)
    };
    let mut agent = match built {
        Ok(a) => a,
        Err(e) => {
            eprintln!("无法初始化内核: {e}");
            return ExitCode::FAILURE;
        }
    };
    if plan {
        agent.set_mode(zex_policy::Mode::Plan);
    }

    let broker = agent.approvals();
    let (tx, mut rx) = mpsc::channel::<CoreEvent>(256);

    // 事件消费任务：过程走 stderr，结果留给 stdout。
    // 这样 `zex-cli "..." > out.txt` 拿到的是干净的结果。
    let printer = tokio::spawn(async move {
        use std::io::Write;
        let mut first_token = true;
        let mut stdin = BufReader::new(tokio::io::stdin());

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
                CoreEvent::ApprovalRequired { req } => {
                    eprintln!("\n╭─ 需要确认 [{}] {}", req.risk.label(), req.tool);
                    eprintln!("│ {}", req.summary);
                    eprintln!("│ 参数: {}", compact(&req.arguments));
                    let hint = if req.grantable {
                        "[y] 批准  [n] 拒绝  [a] 批准并记住此类"
                    } else {
                        "[y] 批准  [n] 拒绝   （高危操作不可记忆）"
                    };
                    eprint!("╰─ {hint} > ");
                    let _ = std::io::stderr().flush();

                    let mut line = String::new();
                    let decision = match stdin.read_line(&mut line).await {
                        Ok(0) => ApprovalDecision::reject("输入已关闭"),
                        Ok(_) => {
                            let ans = line.trim().to_ascii_lowercase();
                            match ans.as_str() {
                                "y" | "yes" => ApprovalDecision::approve(),
                                "a" | "all" if req.grantable => ApprovalDecision::Approve {
                                    remember: Some(RememberScope::Pattern {
                                        pattern: None,
                                        days: None,
                                    }),
                                },
                                "" => ApprovalDecision::reject("空输入按拒绝处理"),
                                other => ApprovalDecision::reject(format!(
                                    "用户回复 `{other}`，视为拒绝"
                                )),
                            }
                        }
                        Err(e) => ApprovalDecision::reject(format!("读取输入失败: {e}")),
                    };
                    if !broker.resolve(&req.id, decision).await {
                        eprintln!("（审批请求已失效）");
                    }
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
    let s = v.to_string().replace('\n', " ");
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}
