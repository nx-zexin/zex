//! Zex 组合根：GUI 与 headless 共享的内核装配。
//!
//! 本 crate 是唯一的「组合根」——只有这里可以同时看到所有 crate。
//! 它持有 gpui（UI）与 tokio（内核）两个世界的桥接（见 docs/ARCHITECTURE.md §2）。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod config;

use std::path::PathBuf;

pub use config::{ResolvedConfig, config_dir, load};
use zex_store::Store;

/// 主数据库路径 `~/.zex/zex.db`。
pub fn db_path() -> PathBuf {
    config_dir().join("zex.db")
}

/// 打开（或创建）主数据库。
pub fn open_store() -> zex_core::Result<Store> {
    Store::open(db_path())
}

/// 把已解析的配置装配成内核。
pub fn build_agent(cfg: &ResolvedConfig, cwd: PathBuf) -> zex_core::Result<zex_agent::Agent> {
    let store = open_store()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
}

/// 打开最近一个会话（若无则新建）。
///
/// 这是"关掉再打开能接着聊"的实现：GUI 启动时恢复上次的对话，
/// 而不是每次都开一个空白会话。
pub fn build_agent_resuming(cfg: &ResolvedConfig, cwd: PathBuf) -> zex_core::Result<zex_agent::Agent> {
    let store = open_store()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);

    // 跳过空会话：每次启动都会留下一个没说话的会话壳，
    // 直接恢复它会让用户以为历史丢了。
    let latest = store
        .list_sessions(8)?
        .into_iter()
        .find(|s| !store.load_messages(&s.id).map(|m| m.is_empty()).unwrap_or(true));

    match latest {
        Some(session) => {
            // 恢复失败时退回新建，不让一次数据问题把应用卡死
            match zex_agent::Agent::resume(agent_cfg.clone(), zex_tools::default_tools(), store.clone(), session.id) {
                Ok(a) => Ok(a),
                Err(e) => {
                    tracing::warn!(error = %e, "恢复会话失败，改为新建");
                    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
                }
            }
        }
        None => zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store),
    }
}

/// 装配一个用内存数据库的内核（测试与 --ephemeral 模式）。
pub fn build_ephemeral_agent(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
) -> zex_core::Result<zex_agent::Agent> {
    let store = Store::in_memory()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
}

// ─── 内核线程 ───

use tokio::sync::mpsc;
use zex_agent::{Agent, ApprovalDecision};
use zex_core::{CoreEvent, UiCommand};

/// 内核侧的运行参数。
pub struct CoreHandles {
    /// UI → 内核。
    pub cmd_tx: mpsc::Sender<UiCommand>,
    /// 内核 → UI。
    pub event_rx: mpsc::Receiver<CoreEvent>,
}

/// 在独立线程上启动 tokio 内核。
///
/// 这是 ADR-002 双世界桥接的落地：gpui 主线程只做渲染，
/// 内核在 tokio 世界里跑，两者用 channel 通信（内存 channel，零序列化）。
pub fn spawn_core(mut agent: Agent) -> CoreHandles {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(64);
    let (ev_tx, ev_rx) = mpsc::channel::<CoreEvent>(256);
    let broker = agent.approvals();

    let spawn_result = std::thread::Builder::new()
        .name("zex-core".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "无法创建内核运行时");
                    return;
                }
            };
            rt.block_on(async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        UiCommand::UserPrompt { text, .. } => {
                            // 单轮失败不应杀掉整个内核：把错误作为事件上报，
                            // 会话继续可用。
                            if let Err(e) = agent.run_turn(text, Some(&ev_tx)).await {
                                let _ = ev_tx
                                    .send(CoreEvent::Fatal { message: e.to_string() })
                                    .await;
                            }
                        }
                        UiCommand::Approve { id, remember } => {
                            let _ = broker
                                .resolve(&id, ApprovalDecision::Approve { remember })
                                .await;
                        }
                        UiCommand::Reject { id, reason } => {
                            let _ = broker
                                .resolve(&id, ApprovalDecision::Reject { reason })
                                .await;
                        }
                        UiCommand::SwitchSession { session } => {
                            match agent.switch_session(session.clone()) {
                                Ok(()) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionSwitched {
                                            session: session.to_string(),
                                            history: agent.history().to_vec(),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    // 切换失败保持原会话，别把用户丢在空会话里
                                    let _ = ev_tx
                                        .send(CoreEvent::Warning {
                                            message: format!("切换会话失败：{e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                        UiCommand::Shutdown => break,
                        _ => {}
                    }
                }
            });
        });
    if let Err(e) = spawn_result {
        // 起不来内核线程时 UI 仍可显示，只是发出去的命令没人接 ——
        // 比直接 panic 让整个应用起不来要好。
        tracing::error!(error = %e, "无法启动内核线程");
    }

    CoreHandles { cmd_tx, event_rx: ev_rx }
}
