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
    // 上次进程退出时还在 running 的任务已经不可能再跑了：标成失败并
    // 写明原因，否则它们会永远挂在「运行中」，dashboard 上说谎。
    match store.fail_orphaned_running_tasks() {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "已标记上次退出时未完成的任务"),
        Err(e) => tracing::warn!(error = %e, "清理孤儿任务失败"),
    }
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    zex_agent::Agent::new(agent_cfg, zex_tools::default_tools(), store)
}

/// 打开最近一个会话（若无则新建）。
///
/// 这是"关掉再打开能接着聊"的实现：GUI 启动时恢复上次的对话，
/// 而不是每次都开一个空白会话。
pub fn build_agent_resuming(cfg: &ResolvedConfig, cwd: PathBuf) -> zex_core::Result<zex_agent::Agent> {
    let store = open_store()?;
    // 上次进程退出时还在 running 的任务已经不可能再跑了：标成失败并
    // 写明原因，否则它们会永远挂在「运行中」，dashboard 上说谎。
    match store.fail_orphaned_running_tasks() {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "已标记上次退出时未完成的任务"),
        Err(e) => tracing::warn!(error = %e, "清理孤儿任务失败"),
    }
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
/// 从 store 读最近会话，转成事件里的摘要。
fn session_summaries(agent: &Agent) -> Vec<zex_core::event::SessionSummary> {
    agent
        .store()
        .list_sessions(20)
        .unwrap_or_default()
        .into_iter()
        .map(|s| zex_core::event::SessionSummary {
            id: s.id.to_string(),
            title: s.title,
            updated_at: s.updated_at,
        })
        .collect()
}

/// 内核在 tokio 世界里跑，两者用 channel 通信（内存 channel，零序列化）。
pub fn spawn_core(mut agent: Agent) -> CoreHandles {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(64);
    let (ev_tx, ev_rx) = mpsc::channel::<CoreEvent>(256);
    let broker = agent.approvals();
    let cancel_flag = agent.cancel_flag();

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
                            // 每轮都落一条任务记录：这是「我不在的时候它做了
                            // 什么」的唯一数据来源。建任务失败不影响执行本身。
                            let task_id = zex_core::TaskId::generate().to_string();
                            let session = agent.session_id().as_str().to_string();
                            if let Err(e) =
                                agent.store().create_task(&task_id, &session, &text, "")
                            {
                                tracing::warn!(error = %e, "建任务记录失败，继续执行");
                            }
                            let _ = agent.store().start_task(&task_id);

                            let outcome = agent.run_turn(text, Some(&ev_tx)).await;

                            match &outcome {
                                Ok(done) => {
                                    let summary: String = done.chars().take(120).collect();
                                    let _ = agent.store().finish_task(
                                        &task_id,
                                        zex_store::tasks::TaskStatus::Completed,
                                        Some(&summary),
                                        None,
                                    );
                                }
                                Err(e) => {
                                    let _ = agent.store().finish_task(
                                        &task_id,
                                        zex_store::tasks::TaskStatus::Failed,
                                        None,
                                        Some(&e.to_string()),
                                    );
                                }
                            }

                            // 单轮失败不应杀掉整个内核：把错误作为事件上报，
                            // 会话继续可用。
                            if let Err(e) = outcome {
                                let _ = ev_tx
                                    .send(CoreEvent::Fatal { message: e.to_string() })
                                    .await;
                            }
                            // 首轮之后给会话起个名字。内部会跳过已有标题的会话，
                            // 所以后续轮次不会重复请求模型。
                            match agent.maybe_autotitle().await {
                                Ok(Some(title)) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionTitled {
                                            session: agent.session_id().to_string(),
                                            title,
                                        })
                                        .await;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    // 起标题失败不值得打扰用户
                                    tracing::warn!(error = %e, "自动生成标题失败");
                                }
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
                        UiCommand::RenameSession { session, title } => {
                            let title = title.trim().to_string();
                            if title.is_empty() {
                                let _ = ev_tx
                                    .send(CoreEvent::Warning {
                                        message: "标题不能为空".into(),
                                    })
                                    .await;
                            } else if let Err(e) = agent.store().rename_session(&session, &title) {
                                let _ = ev_tx
                                    .send(CoreEvent::Warning {
                                        message: format!("重命名失败：{e}"),
                                    })
                                    .await;
                            } else {
                                let _ = ev_tx
                                    .send(CoreEvent::SessionTitled {
                                        session: session.to_string(),
                                        title,
                                    })
                                    .await;
                            }
                        }
                        UiCommand::ClearSession { session } => {
                            let result = if session == *agent.session_id() {
                                agent.clear()
                            } else {
                                agent.store().clear_messages(&session).map(|_| ())
                            };
                            match result {
                                Ok(()) => {
                                    // 只有清的是当前会话才需要重置视图，否则只刷新侧栏
                                    let (history, active) = if session == *agent.session_id() {
                                        (Vec::new(), agent.session_id().to_string())
                                    } else {
                                        (agent.history().to_vec(), agent.session_id().to_string())
                                    };
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionSwitched {
                                            session: active,
                                            history,
                                            sessions: session_summaries(&agent),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::Warning {
                                            message: format!("清空会话失败：{e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                        UiCommand::DeleteSession { session } => {
                            let was_active = session == *agent.session_id();
                            match agent.store().delete_session(&session) {
                                Ok(()) => {
                                    // 删掉的正是当前会话：换个地方落脚，
                                    // 否则 UI 会停在一个已经不存在的会话上
                                    if was_active {
                                        match agent.store().list_sessions(1) {
                                            Ok(list) if !list.is_empty() => {
                                                let _ = agent.switch_session(list[0].id.clone());
                                            }
                                            _ => {
                                                let _ = agent.new_session();
                                            }
                                        }
                                    }
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionSwitched {
                                            session: agent.session_id().to_string(),
                                            history: agent.history().to_vec(),
                                            sessions: session_summaries(&agent),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::Warning {
                                            message: format!("删除会话失败：{e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                        UiCommand::NewSession => {
                            match agent.new_session() {
                                Ok(()) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionSwitched {
                                            session: agent.session_id().to_string(),
                                            history: Vec::new(),
                                            sessions: session_summaries(&agent),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::Warning {
                                            message: format!("新建会话失败：{e}"),
                                        })
                                        .await;
                                }
                            }
                        }
                        UiCommand::SwitchSession { session } => {
                            match agent.switch_session(session.clone()) {
                                Ok(()) => {
                                    let _ = ev_tx
                                        .send(CoreEvent::SessionSwitched {
                                            session: session.to_string(),
                                            history: agent.history().to_vec(),
                                            sessions: session_summaries(&agent),
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
                        UiCommand::Cancel { .. } => {
                            // 内核一次只跑一轮，不必核对 turn id
                            cancel_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        UiCommand::Shutdown => break,
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
