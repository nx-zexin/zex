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
        .list_visible_sessions(8)?
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

/// 装配带子智能体的工具集。
///
/// GUI 和 CLI 共用同一份装配逻辑 —— `zex_cli.rs` 顶部的约定是
/// 「不得有独立于 GUI 的能力分支」，所以这里不各写一套。
pub fn attach_team_tools(
    agent_cfg: zex_agent::AgentConfig,
    store: Store,
    ev_tx: mpsc::Sender<CoreEvent>,
    broker: Arc<zex_agent::ApprovalBroker>,
) -> (Arc<zex_agent::subagent::AgentRegistry>, Vec<Arc<dyn zex_core::tool::ErasedTool>>) {
    use zex_agent::subagent::{AgentRegistry, SendMessage, SpawnAgent, TeamStatus, TerminateAgent, WaitForAgents};

    let registry = Arc::new(AgentRegistry::new());
    let mut tools = zex_tools::default_tools();
    tools.push(Arc::new(SpawnAgent { registry: registry.clone() }));
    tools.push(Arc::new(TeamStatus { registry: registry.clone() }));
    tools.push(Arc::new(WaitForAgents { registry: registry.clone() }));
    tools.push(Arc::new(SendMessage { registry: registry.clone() }));
    tools.push(Arc::new(TerminateAgent { registry: registry.clone() }));
    registry.install(agent_cfg, tools.clone(), store, ev_tx, broker);
    (registry, tools)
}

/// 带子智能体能力的常规 agent（落到 `~/.zex/zex.db`）。
pub fn build_agent_with_team(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
    ev_tx: mpsc::Sender<CoreEvent>,
) -> zex_core::Result<(zex_agent::Agent, Arc<zex_agent::subagent::AgentRegistry>)> {
    let store = open_store()?;
    match store.fail_orphaned_running_tasks() {
        Ok(0) | Err(_) => {}
        Ok(n) => tracing::info!(count = n, "已标记上次退出时未完成的任务"),
    }
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    let broker = Arc::new(zex_agent::ApprovalBroker::new());
    let (registry, tools) = attach_team_tools(agent_cfg.clone(), store.clone(), ev_tx, broker.clone());
    let agent = zex_agent::Agent::new(agent_cfg, tools, store)?.with_shared_approvals(broker);
    Ok((agent, registry))
}

/// 带子智能体能力的 ephemeral agent（CLI 用）。
///
/// 返回注册表：调用方在 `run_turn` 之后还要等子 agent 收尾。
pub fn build_ephemeral_agent_with_team(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
    ev_tx: mpsc::Sender<CoreEvent>,
) -> zex_core::Result<(zex_agent::Agent, Arc<zex_agent::subagent::AgentRegistry>)> {
    let store = Store::in_memory()?;
    let agent_cfg = zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd);
    let broker = Arc::new(zex_agent::ApprovalBroker::new());
    let (registry, tools) = attach_team_tools(agent_cfg.clone(), store.clone(), ev_tx, broker.clone());
    let agent = zex_agent::Agent::new(agent_cfg, tools, store)?.with_shared_approvals(broker);
    Ok((agent, registry))
}

// ─── 内核线程 ───

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use zex_agent::{Agent, AgentConfig, ApprovalBroker, ApprovalDecision};
use zex_core::{CoreEvent, SessionId, UiCommand};

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
        .list_visible_sessions(20)
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
pub fn spawn_core(
    mut agent: Agent,
    providers: std::collections::BTreeMap<String, zex_llm::ProviderConfig>,
) -> CoreHandles {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(64);
    let (ev_tx, ev_rx) = mpsc::channel::<CoreEvent>(256);
    let broker = agent.approvals();
    let cancel_flag = agent.cancel_flag();

    // 任务执行器的原料。都是克隆成本很低的类型（Arc / 配置结构），
    // 每个任务拿一份，互不干扰。
    let cfg = agent.config().clone();
    let store = agent.store().clone();

    // 子智能体注册表：所有会话共用一个。工具要拿到 Arc 才能 spawn，
    // 而注册表又要工具集才能构造子 agent —— 用 install 打破这个循环。
    let registry = Arc::new(zex_agent::subagent::AgentRegistry::new());
    let mut tools = agent.tools().to_vec();
    tools.push(Arc::new(zex_agent::subagent::SpawnAgent {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(zex_agent::subagent::TeamStatus {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(zex_agent::subagent::WaitForAgents {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(zex_agent::subagent::SendMessage {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(zex_agent::subagent::TerminateAgent {
        registry: registry.clone(),
    }));
    registry.install(
        cfg.clone(),
        tools.clone(),
        store.clone(),
        ev_tx.clone(),
        broker.clone(),
    );
    // 把上次的团队捞回来。必须在 install 之后、开始收命令之前。
    registry.restore();
    let session_locks: SessionLocks = Arc::new(Mutex::new(HashMap::new()));

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
                        UiCommand::UserPrompt { session, text, attachments, model } => {
                            // 派到后台执行，主循环立刻回去收下一条命令 ——
                            // 否则一个慢任务会把「切会话」「审批」全堵在队列里。
                            // 同一会话内部仍串行（run_task 里的会话锁），
                            // 避免两个任务同时往同一条 history 追加。
                            let (cfg, tools, store, ev, locks, broker, providers) = (
                                cfg.clone(),
                                tools.clone(),
                                store.clone(),
                                ev_tx.clone(),
                                session_locks.clone(),
                                broker.clone(),
                                providers.clone(),
                            );
                            tokio::spawn(async move {
                                // 请求级模型覆盖：换了模型不必重启内核
                                let mut task_cfg = cfg.clone();
                                if let Some(m) = model.as_deref() {
                                    if let Some((p, rest)) = m.split_once('/') {
                                        if let Some(pc) = providers.get(p) {
                                            task_cfg.provider = pc.clone();
                                            task_cfg.model = rest.to_string();
                                        }
                                    }
                                }
                                run_task(
                                    task_cfg, tools, store, session, text, attachments, ev,
                                    locks, broker,
                                )
                                .await;
                            });
                        }
                        UiCommand::Transcribe { audio } => {
                            // 转写不走对话模型，直接调 provider 的 ASR 端点
                            let http = agent.http().clone();
                            let provider = agent.provider().clone();
                            let ev = ev_tx.clone();
                            tokio::spawn(async move {
                                let out =
                                    zex_llm::transcribe(&http, &provider, audio).await;
                                let (text, error) = match out {
                                    Ok(t) if t.is_empty() => (
                                        None,
                                        Some("没有识别出内容，可能说得太轻".to_string()),
                                    ),
                                    Ok(t) => (Some(t), None),
                                    Err(e) => (None, Some(e.to_string())),
                                };
                                let _ = ev
                                    .send(CoreEvent::Transcribed { text, error })
                                    .await;
                            });
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
                                        match agent.store().list_visible_sessions(1) {
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

/// 每个会话一把锁：同一会话的任务排队，不同会话并行。
type SessionLocks = Arc<Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>;

/// 一次任务执行。
///
/// 这是并行化的核心：任务不再依附于某个长期存在的 `Agent`，而是
/// 现取会话历史 → 跑 → 写回。会话状态以库为准，`Agent` 退化成
/// 一次性的执行器。
#[allow(clippy::too_many_arguments)]
async fn run_task(
    cfg: AgentConfig,
    tools: Vec<Arc<dyn zex_core::tool::ErasedTool>>,
    store: zex_store::Store,
    session: SessionId,
    text: String,
    attachments: Vec<zex_core::message::ImageRef>,
    ev_tx: mpsc::Sender<CoreEvent>,
    locks: SessionLocks,
    broker: Arc<ApprovalBroker>,
) {
    // 先落一条 queued 记录，再去等锁。
    //
    // 顺序很关键：排队中的任务在界面上是可见的（工作台的「排队中」），
    // 如果等拿到锁才建记录，用户连自己派过活都看不到。
    let task_id = zex_core::TaskId::generate().to_string();
    let _ = store.create_task(&task_id, session.as_str(), &text, "");

    // 同一会话串行：两条任务同时往一条 history 追加会乱序，
    // 而且它们看到的上下文也不一致。tokio 的 Mutex 是公平的，
    // 先来的先拿 —— 所以顺带就有了队列语义。
    let lock = {
        let mut held = locks.lock().expect("会话锁表中毒");
        held.entry(session.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // 拿到锁才算真的开始跑
    let _ = store.start_task(&task_id);

    let mut agent = match Agent::resume(cfg, tools, store.clone(), session.clone()) {
        Ok(agent) => agent.with_shared_approvals(broker),
        Err(e) => {
            let _ = store.finish_task(
                &task_id,
                zex_store::tasks::TaskStatus::Failed,
                None,
                Some(&e.to_string()),
            );
            let _ = ev_tx
                .send(CoreEvent::Fatal {
                    message: e.to_string(),
                })
                .await;
            return;
        }
    };

    // 先给个能看的标题：GLM/DeepSeek 这类模型跑长任务时，
    // 等整轮结束再命名，侧栏会空白好几分钟。
    if let Ok(Some(t)) = agent.quick_title(&text) {
        let _ = ev_tx
            .send(CoreEvent::SessionTitled {
                session: session.to_string(),
                title: t,
            })
            .await;
    }

    let outcome = agent
        .run_turn_with_images(text, attachments, Some(&ev_tx))
        .await;

    match &outcome {
        Ok(done) => {
            let summary: String = done.chars().take(120).collect();
            let _ = store.finish_task(
                &task_id,
                zex_store::tasks::TaskStatus::Completed,
                Some(&summary),
                None,
            );
        }
        Err(e) => {
            let _ = store.finish_task(
                &task_id,
                zex_store::tasks::TaskStatus::Failed,
                None,
                Some(&e.to_string()),
            );
        }
    }

    if let Err(e) = outcome {
        let _ = ev_tx
            .send(CoreEvent::Fatal {
                message: e.to_string(),
            })
            .await;
    }

    if let Ok(Some(title)) = agent.maybe_autotitle().await {
        let _ = ev_tx
            .send(CoreEvent::SessionTitled {
                session: session.to_string(),
                title,
            })
            .await;
    }
}
