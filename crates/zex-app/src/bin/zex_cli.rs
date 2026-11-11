//! Zex headless 二进制。
//!
//! 与 GUI (`zex`) 共享同一内核，只是 channel 的另一端（ADR-009）。
//! 约束：**不得有独立于 GUI 的能力分支**。
//!
//! M0 阶段（`docs/M0-PLAN.md` T9）：先打通 CLI 形态本身。
//! 真正的对话能力在 T6/T7 接入后可用。

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        println!("zex-cli {}", env!("CARGO_PKG_VERSION"));
        println!();
        println!("用法:");
        println!("  zex-cli \"<prompt>\"     执行一次任务，结果输出到 stdout");
        println!("  zex-cli --version      显示版本");
        println!();
        println!("M0 骨架阶段：内核尚未接入，见 docs/M0-PLAN.md T6/T7。");
        return ExitCode::SUCCESS;
    }

    if args[0] == "--version" || args[0] == "-V" {
        println!("zex-cli {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let prompt = args.join(" ");
    eprintln!("[zex-cli] 收到任务: {prompt}");
    eprintln!("[zex-cli] 内核尚未接入（M0 T6/T7 待完成）");
    ExitCode::SUCCESS
}
