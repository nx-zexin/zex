//! 子智能体：一个 agent 可以拉起若干个子 agent 并行干活。
//!
//! 设计要点：
//!
//! 1. **子 agent 有自己的会话** —— 上下文完全隔离，不会污染主对话。
//!    这也是"多个 session 不混淆内容"在纵向上成立的保证。
//! 2. **一次性实例** —— 子 agent 跑完即弃，状态以库为准，和主 agent 一致。
//! 3. **事件照常上报** —— 子 agent 的事件带自己的 session，UI 按归属过滤；
//!    切到那个会话就能看到它干了什么。
//!
//! 父 agent 通过 `spawn_agent` / `team_status` / `wait_for_agents` 三个工具
//! 驱动这套机制。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;

use zex_core::tool::{ErasedTool, Tool, ToolCtx, ToolOutput};
use zex_core::{CoreEvent, Error, Result, Risk, SessionId};
use zex_store::Store;

use crate::{Agent, AgentConfig};

/// 只有父 agent 才该有的工具。
///
/// 子 agent 拿到它们会看到兄弟 agent 并试图干预 —— 层级应该单向：
/// 父能指挥子，子看不到兄弟，也生不了孙子。
const TEAM_TOOLS: &[&str] = &[
    "spawn_agent",
    "team_status",
    "wait_for_agents",
    "send_message",
    "terminate_agent",
];

/// 子智能体的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubAgentStatus {
    /// 正在跑。
    Running,
    /// 跑完了。
    Completed,
    /// 出错了。
    Failed(String),
    /// 被叫停了。
    Terminated,
}

impl SubAgentStatus {
    /// 是否已经结束（无论成败）。
    pub fn is_done(&self) -> bool {
        !matches!(self, SubAgentStatus::Running)
    }

    /// 给人看的中文标签。
    pub fn label(&self) -> &'static str {
        match self {
            SubAgentStatus::Running => "执行中",
            SubAgentStatus::Completed => "已完成",
            SubAgentStatus::Failed(_) => "失败",
            SubAgentStatus::Terminated => "已停止",
        }
    }
}

/// 对外暴露的快照。内部结构不外泄，UI 和工具都只看这个。
#[derive(Debug, Clone)]
pub struct SubAgentInfo {
    /// 调用方指定的标识。
    pub id: String,
    /// 角色（coder / reviewer / researcher …）。
    pub role: String,
    /// 这个子 agent 负责什么。
    pub description: String,
    /// 它独立占用的会话。
    pub session: SessionId,
    /// 当前状态。
    pub status: SubAgentStatus,
    /// 跑完之后的结果摘要。
    pub result: Option<String>,
}

struct Entry {
    role: String,
    description: String,
    session: SessionId,
    status: SubAgentStatus,
    result: Option<String>,
    /// 叫停这个 agent 的开关。跨轮次有效。
    cancel: Arc<AtomicBool>,
}

/// 构造子 agent 需要的原料。
///
/// 单独拿出来注入而不是塞进构造函数，是为了解开循环依赖：
/// 工具需要 `Arc<AgentRegistry>` 才能 spawn，而 registry 又需要工具集。
struct Deps {
    cfg: AgentConfig,
    /// 给子 agent 用的工具集 —— **不含**团队工具本身。
    ///
    /// 子 agent 拿到 `team_status` 会看到一堆「我沒派过」的 agent，
    /// 实测直接把模型搞迷糊了（它开始琢磨要不要干预别人）。
    /// 层级只往下走，兄弟之间不互相可见。
    tools: Vec<Arc<dyn ErasedTool>>,
    store: Store,
    ev_tx: mpsc::Sender<CoreEvent>,
    /// 子 agent 的审批也要走这条通道，否则用户在界面上根本看不到
    /// 它卡在等授权。
    broker: Arc<crate::approval::ApprovalBroker>,
}

/// 子智能体注册表。
///
/// 全局一个，所有会话共用。
pub struct AgentRegistry {
    agents: Mutex<HashMap<String, Entry>>,
    deps: OnceLock<Deps>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    /// 建一个空注册表。
    pub fn new() -> Self {
        AgentRegistry {
            agents: Mutex::new(HashMap::new()),
            deps: OnceLock::new(),
        }
    }

    /// 注入构造原料。只能成功一次，重复调用会被忽略。
    /// 注入构造原料。
    ///
    /// `tools` 是**父 agent 的完整工具集**；这里会剔除团队工具，
    /// 子 agent 只拿基础能力（读写、shell、网络）。
    pub fn install(
        &self,
        cfg: AgentConfig,
        tools: Vec<Arc<dyn ErasedTool>>,
        store: Store,
        ev_tx: mpsc::Sender<CoreEvent>,
        broker: Arc<crate::approval::ApprovalBroker>,
    ) {
        let tools: Vec<Arc<dyn ErasedTool>> = tools
            .into_iter()
            .filter(|t| !TEAM_TOOLS.contains(&t.spec().name))
            .collect();
        let _ = self.deps.set(Deps {
            cfg,
            tools,
            store,
            ev_tx,
            broker,
        });
    }

    /// 拉起一个子 agent 去干活。
    ///
    /// 立刻返回，不等它跑完 —— 这样父 agent 可以连着派好几个，让它们并行。
    pub fn spawn(
        self: &Arc<Self>,
        id: String,
        role: String,
        description: String,
        task: String,
    ) -> Result<String> {
        let deps = self.deps.get().ok_or_else(|| Error::ToolFailed {
            tool: "spawn_agent".into(),
            message: "注册表尚未初始化".into(),
        })?;

        {
            let map = self.agents.lock().expect("子智能体表中毒");
            if map.contains_key(&id) {
                return Err(Error::ToolFailed {
                    tool: "spawn_agent".into(),
                    message: format!("`{id}` 已经存在，换一个 id 或先 terminate"),
                });
            }
        }

        // 每个子 agent 独立开一个会话：上下文互不干扰，用户也能在侧栏
        // 点进去看它到底做了什么。
        let mut agent = Agent::new(deps.cfg.clone(), deps.tools.clone(), deps.store.clone())?
            .with_shared_approvals(deps.broker.clone());
        let session = agent.session_id().clone();
        let cancel = agent.cancel_flag();
        let _ = deps
            .store
            .rename_session(&session, &format!("子智能体 · {id}"));

        self.agents.lock().expect("子智能体表中毒").insert(
            id.clone(),
            Entry {
                role: role.clone(),
                description,
                session,
                status: SubAgentStatus::Running,
                result: None,
                cancel,
            },
        );

        self.broadcast();

        let ev_tx = deps.ev_tx.clone();
        let this = self.clone();
        let handle_id = id.clone();
        // 子 agent 在后台跑：父 agent 立刻拿回控制权，继续派下一个活。
        tokio::spawn(async move {
            let outcome = agent.run_turn(task, Some(&ev_tx)).await;
            let mut map = this.agents.lock().expect("子智能体表中毒");
            if let Some(entry) = map.get_mut(&handle_id) {
                match outcome {
                    Ok(done) => {
                        entry.status = SubAgentStatus::Completed;
                        entry.result = Some(done);
                    }
                    Err(e) => {
                        entry.status = SubAgentStatus::Failed(e.to_string());
                    }
                }
            }
            drop(map);
            this.broadcast();
        });

        Ok(id)
    }

    /// 给一个已经跑完的子 agent 再派一轮活。
    ///
    /// 它会接着自己那个会话的上下文继续 —— 相当于追问，不是重新开一个。
    pub fn send(self: &Arc<Self>, to: &str, message: String) -> Result<String> {
        let deps = self.deps.get().ok_or_else(|| Error::ToolFailed {
            tool: "send_message".into(),
            message: "注册表尚未初始化".into(),
        })?;

        let (session, cancel) = {
            let mut map = self.agents.lock().expect("子智能体表中毒");
            let entry = map.get_mut(to).ok_or_else(|| Error::ToolFailed {
                tool: "send_message".into(),
                message: format!("没有 `{to}` 这个子智能体"),
            })?;
            if !entry.status.is_done() {
                return Err(Error::ToolFailed {
                    tool: "send_message".into(),
                    message: format!("`{to}` 还在执行，等这一轮结束再发"),
                });
            }
            entry.status = SubAgentStatus::Running;
            entry.result = None;
            (entry.session.clone(), entry.cancel.clone())
        };

        // 上一轮如果是被叫停的，开关还是按下状态，得复位
        cancel.store(false, Ordering::Relaxed);

        let mut agent = Agent::resume(
            deps.cfg.clone(),
            deps.tools.clone(),
            deps.store.clone(),
            session,
        )?
        .with_shared_approvals(deps.broker.clone())
        .with_cancel_flag(cancel);

        let ev_tx = deps.ev_tx.clone();
        let this = self.clone();
        let handle_id = to.to_string();
        tokio::spawn(async move {
            let outcome = agent.run_turn(message, Some(&ev_tx)).await;
            let mut map = this.agents.lock().expect("子智能体表中毒");
            if let Some(entry) = map.get_mut(&handle_id) {
                // 被叫停的不要用错误覆盖掉「已停止」这个更有信息量的状态
                if matches!(entry.status, SubAgentStatus::Terminated) {
                    return;
                }
                match outcome {
                    Ok(done) => {
                        entry.status = SubAgentStatus::Completed;
                        entry.result = Some(done);
                    }
                    Err(e) => entry.status = SubAgentStatus::Failed(e.to_string()),
                }
            }
        });

        self.broadcast();
        Ok(format!("已投递给 `{to}`。"))
    }

    /// 叫停一个子 agent。
    ///
    /// 正在跑的那一轮会在下一个检查点退出；已经跑完的就只是标记状态。
    pub fn terminate(&self, id: &str) -> Result<String> {
        let mut map = self.agents.lock().expect("子智能体表中毒");
        let entry = map.get_mut(id).ok_or_else(|| Error::ToolFailed {
            tool: "terminate_agent".into(),
            message: format!("没有 `{id}` 这个子智能体"),
        })?;
        entry.cancel.store(true, Ordering::Relaxed);
        entry.status = SubAgentStatus::Terminated;
        drop(map);
        self.broadcast();
        Ok(format!("已叫停 `{id}`。"))
    }

    /// 把当前团队状态推给界面。
    ///
    /// 用 `try_send`：这是一个"顺便通知一下"的信号，通道满了就算了，
    /// 不能因此拖住正在派活的 agent。
    fn broadcast(&self) {
        if let Some(deps) = self.deps.get() {
            // 先落库：进程随时可能没，界面刷新晚一点没关系，数据丢了不行
            for a in self.snapshot() {
                let state = match &a.status {
                    SubAgentStatus::Running => zex_store::subagents::SubAgentState::Running,
                    SubAgentStatus::Completed => zex_store::subagents::SubAgentState::Completed,
                    SubAgentStatus::Failed(_) => zex_store::subagents::SubAgentState::Failed,
                    SubAgentStatus::Terminated => zex_store::subagents::SubAgentState::Terminated,
                };
                let _ = deps.store.upsert_sub_agent(
                    &a.id,
                    &a.role,
                    &a.description,
                    a.session.as_str(),
                    state,
                    a.result.as_deref(),
                );
            }

            let agents = self
                .snapshot()
                .into_iter()
                .map(|a| zex_core::event::SubAgentView {
                    id: a.id,
                    role: a.role,
                    status: a.status.label().to_string(),
                    description: a.description,
                    session: a.session.to_string(),
                    result: a.result,
                })
                .collect();
            let _ = deps
                .ev_tx
                .try_send(CoreEvent::TeamUpdated { agents });
        }
    }

    /// 从库里恢复名册。
    ///
    /// 上次退出时还在跑的，一律改判「已停止」——进程都换了，
    /// 那些 agent 手里的上下文没了，标成「执行中」只会让界面撒谎。
    /// 恢复后仍然可以 `send_message` 追问它们（会话历史还在库里）。
    pub fn restore(&self) {
        let Some(deps) = self.deps.get() else {
            return;
        };
        let _ = deps.store.interrupt_orphan_sub_agents();
        let records = deps.store.list_sub_agents().unwrap_or_default();
        {
            let mut map = self.agents.lock().expect("子智能体表中毒");
            for r in records {
                let status = match r.status {
                    zex_store::subagents::SubAgentState::Completed => SubAgentStatus::Completed,
                    zex_store::subagents::SubAgentState::Failed => {
                        SubAgentStatus::Failed("上一次执行失败".into())
                    }
                    _ => SubAgentStatus::Terminated,
                };
                map.insert(
                    r.id.clone(),
                    Entry {
                        role: r.role,
                        description: r.description,
                        session: SessionId::from(r.session_id),
                        status,
                        result: r.result,
                        cancel: Arc::new(AtomicBool::new(false)),
                    },
                );
            }
        }
        self.broadcast();
    }

    /// 当前所有子 agent 的状态。
    pub fn snapshot(&self) -> Vec<SubAgentInfo> {
        let map = self.agents.lock().expect("子智能体表中毒");
        let mut list: Vec<SubAgentInfo> = map
            .iter()
            .map(|(id, e)| SubAgentInfo {
                id: id.clone(),
                role: e.role.clone(),
                description: e.description.clone(),
                session: e.session.clone(),
                status: e.status.clone(),
                result: e.result.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 等到全部子 agent 结束，或超时。
    pub async fn wait_all(&self, timeout: Duration) -> Vec<SubAgentInfo> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let list = self.snapshot();
            if list.iter().all(|a| a.status.is_done()) {
                return list;
            }
            if tokio::time::Instant::now() >= deadline {
                // 超时了就不能再谎称它们在跑：那个 agent 可能卡在一个
                // 永远不返回的请求上（实测卡过 7 分钟）。
                self.abandon_running();
                return self.snapshot();
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }

    /// 把仍在执行的 agent 标成失败并叫停。
    ///
    /// 调用方已经放弃等它们了，界面不该继续显示「执行中」。
    /// 那个后台 task 之后如果真的跑完，也不会覆盖这里的状态
    /// （见 `send` 里对 `Terminated` 的保护）。
    fn abandon_running(&self) {
        let mut map = self.agents.lock().expect("子智能体表中毒");
        let mut changed = false;
        for entry in map.values_mut() {
            if matches!(entry.status, SubAgentStatus::Running) {
                entry.cancel.store(true, Ordering::Relaxed);
                entry.status = SubAgentStatus::Failed("等待超时，已放弃".into());
                changed = true;
            }
        }
        drop(map);
        if changed {
            self.broadcast();
        }
    }
}

// ─── 工具 ───

/// `spawn_agent` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnArgs {
    /// 唯一标识，随手起个短名字，例如 `coder-1`。
    pub id: String,
    /// 角色，例如 `coder` / `reviewer` / `researcher`。
    pub role: String,
    /// 一句话说明它负责什么。
    pub description: String,
    /// 交给它的任务。要写清楚输入、期望产出和边界。
    pub task: String,
}

/// 拉起一个子智能体。
pub struct SpawnAgent {
    /// 注册表。工具在后台跑，所以这里必须持有所有权。
    pub registry: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for SpawnAgent {
    const NAME: &'static str = "spawn_agent";
    const RISK: Risk = Risk::Reversible;
    type Args = SpawnArgs;

    fn description() -> &'static str {
        "拉起一个子智能体并行干活。它会独立开一个会话，上下文与当前对话隔离。\
         调用后立刻返回，不会等它跑完 —— 可以连着派好几个让它们并行。\
         适合可以拆开、彼此不依赖的子任务。派完记得用 wait_for_agents 收结果。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: SpawnArgs) -> Result<ToolOutput> {
        let id = self.registry.spawn(
            args.id.clone(),
            args.role.clone(),
            args.description.clone(),
            args.task,
        )?;
        Ok(ToolOutput::ok(format!(
            "已拉起 `{id}`（{}）。它独立跑，不会阻塞你；\
             用 team_status 看进展，wait_for_agents 收结果。",
            args.role
        )))
    }
}

/// `team_status` 的参数（无）。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

/// 查看团队状态。
pub struct TeamStatus {
    /// 注册表。
    pub registry: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for TeamStatus {
    const NAME: &'static str = "team_status";
    const RISK: Risk = Risk::ReadOnly;
    type Args = EmptyArgs;

    fn description() -> &'static str {
        "查看所有子智能体的实时状态：谁在跑、谁跑完了、结果是什么。\
         在等结果或者想确认某个子任务有没有卡住时用。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, _args: EmptyArgs) -> Result<ToolOutput> {
        let list = self.registry.snapshot();
        if list.is_empty() {
            return Ok(ToolOutput::ok("当前没有子智能体在运行。"));
        }
        let mut out = String::new();
        for a in &list {
            out.push_str(&format!(
                "· {}（{}）— {}\n  {}",
                a.id, a.role, a.status.label(), a.description
            ));
            if let SubAgentStatus::Failed(e) = &a.status {
                out.push_str(&format!("\n  错误：{e}"));
            }
            if let Some(r) = &a.result {
                let brief: String = r.chars().take(200).collect();
                out.push_str(&format!("\n  结果：{brief}"));
            }
            out.push('\n');
        }
        Ok(ToolOutput::ok(out))
    }
}

/// `wait_for_agents` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitArgs {
    /// 最多等多少秒，默认 300。超时会把尚未完成的也一起返回。
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// 等子智能体跑完。
pub struct WaitForAgents {
    /// 注册表。
    pub registry: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for WaitForAgents {
    const NAME: &'static str = "wait_for_agents";
    const RISK: Risk = Risk::ReadOnly;
    type Args = WaitArgs;

    fn description() -> &'static str {
        "阻塞等待所有子智能体结束，返回各自的结果。\
         默认最多等 300 秒；超时的话会把还没跑完的也一并返回，你可以再等一次。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: WaitArgs) -> Result<ToolOutput> {
        let secs = args.timeout_seconds.unwrap_or(300).min(1800);
        let list = self
            .registry
            .wait_all(Duration::from_secs(secs))
            .await;
        if list.is_empty() {
            return Ok(ToolOutput::ok("没有子智能体需要等待。"));
        }
        let pending = list.iter().filter(|a| !a.status.is_done()).count();
        let mut out = String::new();
        for a in &list {
            out.push_str(&format!("【{}】{}\n", a.id, a.status.label()));
            match &a.status {
                SubAgentStatus::Completed => {
                    if let Some(r) = &a.result {
                        out.push_str(r);
                        out.push('\n');
                    }
                }
                SubAgentStatus::Failed(e) => out.push_str(&format!("错误：{e}\n")),
                SubAgentStatus::Terminated => out.push_str("已被叫停。\n"),
                SubAgentStatus::Running => out.push_str("仍在执行，稍后再等。\n"),
            }
            out.push('\n');
        }
        if pending > 0 {
            out.push_str(&format!("还有 {pending} 个没跑完。"));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_agents() {
        let r = AgentRegistry::new();
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn spawn_without_install_fails_cleanly() {
        // 没装依赖就 spawn，应该报错而不是 panic
        let r = Arc::new(AgentRegistry::new());
        let err = r
            .spawn("a1".into(), "coder".into(), "写代码".into(), "干活".into())
            .unwrap_err();
        assert!(err.to_string().contains("尚未初始化"), "got: {err}");
    }

    #[tokio::test]
    async fn wait_on_empty_returns_immediately() {
        let r = AgentRegistry::new();
        let list = r.wait_all(Duration::from_secs(5)).await;
        assert!(list.is_empty());
    }

    #[test]
    fn status_done_semantics() {
        assert!(!SubAgentStatus::Running.is_done());
        assert!(SubAgentStatus::Completed.is_done());
        assert!(SubAgentStatus::Failed("x".into()).is_done());
    }
}

/// `send_message` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendArgs {
    /// 目标子智能体的 id。
    pub to: String,
    /// 要追加给它的内容。它会接着自己那轮的上下文继续。
    pub message: String,
}

/// 给子智能体追加一轮。
pub struct SendMessage {
    /// 注册表。
    pub registry: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for SendMessage {
    const NAME: &'static str = "send_message";
    const RISK: Risk = Risk::Reversible;
    type Args = SendArgs;

    fn description() -> &'static str {
        "给一个子智能体追加一轮任务。它会接着自己的会话上下文继续，\
         相当于追问或补充要求，而不是重新开一个。\
         对方还在执行时会失败 —— 先 wait_for_agents 或 team_status 确认。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: SendArgs) -> Result<ToolOutput> {
        let msg = self.registry.send(&args.to, args.message)?;
        Ok(ToolOutput::ok(msg))
    }
}

/// `terminate_agent` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TerminateArgs {
    /// 要叫停的子智能体 id。
    pub id: String,
}

/// 叫停子智能体。
pub struct TerminateAgent {
    /// 注册表。
    pub registry: Arc<AgentRegistry>,
}

#[async_trait]
impl Tool for TerminateAgent {
    const NAME: &'static str = "terminate_agent";
    const RISK: Risk = Risk::Reversible;
    type Args = TerminateArgs;

    fn description() -> &'static str {
        "叫停一个子智能体。它在跑的话会在下一个检查点退出；\
         发现派错了活或者它跑偏了时用。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: TerminateArgs) -> Result<ToolOutput> {
        let msg = self.registry.terminate(&args.id)?;
        Ok(ToolOutput::ok(msg))
    }
}

#[cfg(test)]
mod fixes {
    use super::*;

    /// 卡住的 agent 不能在超时后还自称「执行中」。
    #[tokio::test]
    async fn wait_timeout_marks_stuck_agents_failed() {
        let reg = AgentRegistry::new();
        // 手工塞一个永远不结束的条目，模拟 LLM 请求挂住
        reg.agents.lock().unwrap().insert(
            "stuck-1".into(),
            Entry {
                role: "coder".into(),
                description: "卡住的".into(),
                session: SessionId::generate(),
                status: SubAgentStatus::Running,
                result: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );

        let list = reg.wait_all(Duration::from_millis(120)).await;
        assert_eq!(list.len(), 1);
        assert!(
            list[0].status.is_done(),
            "超时后不该还显示执行中，实际: {:?}",
            list[0].status
        );
        assert!(matches!(list[0].status, SubAgentStatus::Failed(_)));
    }

    /// 已经完成的不该被超时清理波及。
    #[tokio::test]
    async fn wait_timeout_leaves_finished_alone() {
        let reg = AgentRegistry::new();
        {
            let mut m = reg.agents.lock().unwrap();
            m.insert(
                "done-1".into(),
                Entry {
                    role: "coder".into(),
                    description: "完成的".into(),
                    session: SessionId::generate(),
                    status: SubAgentStatus::Completed,
                    result: Some("好了".into()),
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
            m.insert(
                "stuck-1".into(),
                Entry {
                    role: "coder".into(),
                    description: "卡住的".into(),
                    session: SessionId::generate(),
                    status: SubAgentStatus::Running,
                    result: None,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        let list = reg.wait_all(Duration::from_millis(120)).await;
        let done = list.iter().find(|a| a.id == "done-1").unwrap();
        assert_eq!(done.status, SubAgentStatus::Completed);
        assert_eq!(done.result.as_deref(), Some("好了"));
    }

    #[test]
    fn team_tools_are_not_leaked_to_children() {
        // 子 agent 的工具集里不该有团队工具
        let parent_tools: Vec<&str> = TEAM_TOOLS.to_vec();
        for t in &parent_tools {
            assert!(
                TEAM_TOOLS.contains(t),
                "`{t}` 应该是团队工具，不该出现在子 agent 手里"
            );
        }
        // 基础工具不在名单里，说明不会被剔除
        assert!(!TEAM_TOOLS.contains(&"fs_read"));
        assert!(!TEAM_TOOLS.contains(&"shell"));
    }
}
