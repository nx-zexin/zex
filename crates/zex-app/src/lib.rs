//! Zex 组合根：GUI 与 headless 共享的内核装配。
//!
//! 本 crate 是唯一的「组合根」——只有这里可以同时看到所有 crate。
//! 它持有 gpui（UI）与 tokio（内核）两个世界的桥接（见 docs/ARCHITECTURE.md §2）。

// 用 deny 而不是 forbid：`window_ops` 里调 Win32 的那一小块必须 unsafe，
// 而 forbid 连锁定它的 `allow` 都不许写（这正是 forbid 与 deny 的区别）。
// 除此之外全 crate 仍然禁止 unsafe，且例外只有那一个模块（`mod win`）。
#![deny(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod config;
pub mod ext_logs;
pub mod ext_tools;
/// 把脚本 API 的 `.d.ts` 生成到技能目录，模型写扩展前查的就是它。
pub mod ext_typings;
pub mod log_file;
pub mod models_dev;
/// 自绘标题栏的窗口动作（拖动 / 最大化还原）。Windows 上绕过 gpui 的两个缺陷。
pub mod window_ops;

use std::path::PathBuf;

pub use config::{
    FileConfig, ModelParams, ProviderEntry, ResolvedConfig, config_dir, load, read_file_config,
    save_file_config,
};

/// 连上 MCP 服务后的三样产物。
///
/// 三元组单独起个名字，是为了让调用点读起来是"拿到了 MCP 这一套东西"，
/// 而不是一长串泛型。
pub type McpWiring = (
    Vec<Arc<dyn zex_core::tool::ErasedTool>>,
    Vec<zex_mcp::ServerStatus>,
    Option<Arc<zex_mcp::McpHub>>,
);

/// 连上配置里声明的所有 MCP 服务端。
///
/// 放在这里而不是 `build_agent_resuming` 里：连接要 spawn 子进程
/// 并等握手（几百毫秒到几秒），不该卡住 agent 构造。
/// 调用方应放到后台线程执行，别阻塞 UI。
pub fn connect_mcp_servers(cfg: &FileConfig) -> McpWiring {
    if cfg.mcp_servers.is_empty() {
        return (Vec::new(), Vec::new(), None);
    }
    zex_mcp::connect_all(&cfg.mcp_servers)
}
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
/// 系统消息该按「整段卡片」还是「一行胶囊」渲染。
///
/// 分界线定在 60 个字：一行放得下的提示（"✓ 任务完成 · 共 3 步，用时 2 分钟"）
/// 仍用胶囊，短提示视觉上不打断阅读节奏；更长的一律铺开成可换行的块 ——
/// 出错说明动辄几十上百字，挤成一行会把两头的关键信息裁掉。
pub fn system_banner_is_long(text: &str) -> bool {
    text.chars().count() > 60 || text.contains('\n')
}

/// 技能目录：`~/.zex/skills`。
pub fn skills_dir() -> PathBuf {
    config_dir().join("skills")
}

/// 随二进制带的技能。
///
/// 编译进二进制而不是放资源目录：技能是「怎么用这个程序」的说明书，
/// 它必须和程序版本一致。跟着安装包走，就不会出现"程序升级了、
/// 说明书还是旧的"那种情况。
const BUNDLED_SKILLS: &[(&str, &str)] = &[(
    "write-extension",
    include_str!("../../../skills/write-extension/skill.md"),
)];

/// `gpui-kit.d.ts` 落在哪个技能的目录里。
///
/// 这份声明是"写扩展"这件事的附件：跟 `skill.md` 放在同一个目录，
/// 技能整个目录拷走就是完整的一套（说明 + API 清单）。
const TYPINGS_SKILL: &str = "write-extension";

/// 记录"我们上次铺下去的是什么"，用来分辨用户改没改。
///
/// 放在技能目录里而不是配置目录：它描述的是这个目录的内容，
/// 跟着目录走，整个 `~/.zex/skills` 拷贝/删除时不会错位。
const BUNDLE_STATE: &str = ".bundled-hashes.json";

/// 把随程序带的技能铺到磁盘。
///
/// 规则是「没改过就更新，改过就不动」——
///
/// - 文件不存在 → 写。
/// - 存在、且内容和上次铺下去的一模一样 → 说明用户没碰过，用新版覆盖。
///   这一条是必须的：技能里写的是 API 事实（`v_flex` 属于哪个模块），
///   留着旧版会**主动误导**模型，比没有更糟。
/// - 存在、但和记录对不上 → 用户改过，一个字都不动。
/// - 存在、但没有记录（这个机制上线前装的）→ 无法判断，于是先把旧内容
///   备份成技能目录里的 `.bundled-backup.md`，再写新版。宁可多一个备份
///   文件，也不能悄悄扔掉别人可能改过的东西。
///
/// 落点是 `<dir>/<名字>/skill.md`（一个技能一个目录，附件放在一起）。
/// 老版本铺的是 `<dir>/<名字>.md`，那段迁移在上面单独处理。
fn install_bundled_skills(dir: &std::path::Path) {
    let state_path = dir.join(BUNDLE_STATE);
    let mut state = read_bundle_state(&state_path);
    let mut dirty = false;

    for (name, body) in BUNDLED_SKILLS {
        let skill_dir = dir.join(name);
        let path = skill_dir.join(zex_skill::SKILL_FILE);

        // 老布局是直接摊在根目录下的 `<name>.md`。如果那份还是我们上次
        // 铺下去的（等于记录里的哈希，说明用户没碰过），删掉 —— 它已经被
        // `<name>/skill.md` 取代，留着只会在加载时撞名。
        // 改过的一律保留：宁可多一个旧文件，也不能删别人写的东西。
        let legacy_path = dir.join(format!("{name}.md"));
        let legacy_key = format!("{name}.md");
        if let Ok(old) = std::fs::read_to_string(&legacy_path) {
            if state.get(&legacy_key).copied() == Some(fnv1a(&old)) {
                if std::fs::remove_file(&legacy_path).is_ok() {
                    state.remove(&legacy_key);
                    dirty = true;
                    tracing::info!(skill = name, "内置技能已从 <name>.md 迁入 <name>/skill.md");
                }
            } else {
                tracing::warn!(
                    skill = name,
                    path = %legacy_path.display(),
                    "旧布局的内置技能被改过，保留原样；它和新的目录版会同时出现，建议手动合并"
                );
            }
        }

        let want = fnv1a(body);
        let existing = std::fs::read_to_string(&path).ok();

        let decision = match &existing {
            None => Decision::Write,
            Some(text) => {
                let have = fnv1a(text);
                if have == want {
                    // 已经是最新的，不用写，但记下来
                    state.insert((*name).to_string(), want);
                    dirty = true;
                    continue;
                }
                match state.get(*name) {
                    Some(recorded) if *recorded == have => Decision::Write,
                    Some(_) => Decision::KeepUserEdited,
                    None => Decision::Migrate,
                }
            }
        };

        match decision {
            Decision::Write => {
                if let Err(e) = std::fs::create_dir_all(&skill_dir) {
                    tracing::warn!(skill = name, error = %e, "无法创建技能目录");
                    continue;
                }
                if let Err(e) = std::fs::write(&path, body) {
                    tracing::warn!(skill = name, error = %e, "无法写入内置技能");
                    continue;
                }
                state.insert((*name).to_string(), want);
                dirty = true;
            }
            Decision::KeepUserEdited => {
                tracing::debug!(skill = name, "内置技能被用户改过，保留原样");
            }
            Decision::Migrate => {
                if let Err(e) = std::fs::create_dir_all(&skill_dir) {
                    tracing::warn!(skill = name, error = %e, "无法创建技能目录");
                    continue;
                }
                let backup = skill_dir.join(".bundled-backup.md");
                if let Some(old) = &existing {
                    let _ = std::fs::write(&backup, old);
                }
                if let Err(e) = std::fs::write(&path, body) {
                    tracing::warn!(skill = name, error = %e, "无法写入内置技能");
                    continue;
                }
                tracing::info!(
                    skill = name,
                    backup = %backup.display(),
                    "内置技能已升级到当前版本，旧内容已备份"
                );
                state.insert((*name).to_string(), want);
                dirty = true;
            }
        }
    }

    if dirty {
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(&state_path, text);
        }
    }
}

/// 一次写入的取舍。
enum Decision {
    /// 没改过 / 不存在：写新版。
    Write,
    /// 用户改过：一个字都不动。
    KeepUserEdited,
    /// 没有记录：备份后写新版。
    Migrate,
}

fn read_bundle_state(path: &std::path::Path) -> std::collections::BTreeMap<String, u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// FNV-1a。用它是为了**跨版本稳定**：这个值要落盘，换个工具链还得认得出来。
/// `DefaultHasher` 做不到这点（不保证跨 Rust 版本稳定）。
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// 加载技能库。
pub fn load_skills() -> std::sync::Arc<zex_skill::SkillRegistry> {
    let dir = skills_dir();
    // 首次运行时建好目录：用户要放技能文件进去，得先有个地方放
    let _ = std::fs::create_dir_all(&dir);
    install_bundled_skills(&dir);
    // 脚本 API 的声明也铺在同一层：模型读技能时在同一目录就能拿到
    // 当前运行时的全部 API，不必先"打开某个扩展"才有一份。
    // 失败只记警告 —— 技能加载不该因为磁盘问题整体失败。
    // 声明是这个技能的附件，落在它的目录里，不摊在根目录
    let typings_dir = dir.join(TYPINGS_SKILL);
    match ext_typings::write_api_declarations(&typings_dir) {
        Ok(written) => {
            // 老版本把声明摊在技能根目录。新的一份已经就位，就把旧的删掉 ——
            // 那是我们生成的、不是用户数据，留着白占 800 KB，还会让人照着
            // 过时的路径去找。
            let legacy = dir.join(ext_typings::FILE_NAME);
            if legacy != written && legacy.is_file() {
                let _ = std::fs::remove_file(&legacy);
                tracing::info!("已把 gpui-kit.d.ts 移进 write-extension/ 目录");
            }
        }
        Err(e) => tracing::warn!(dir = %typings_dir.display(), error = %e, "无法生成脚本 API 声明"),
    }
    let registry = zex_skill::SkillRegistry::load(&dir);
    // 加载时的问题（旧布局、目录里没 skill.md）要能被人看见 ——
    // 静默忽略等于让用户对着一个"为什么不生效"的目录发呆。
    for warning in registry.warnings() {
        tracing::warn!(target: "zex_skill", "{warning}");
    }
    std::sync::Arc::new(registry)
}

/// 打开最近一个会话（若无则新建），并加载技能。
pub fn build_agent_resuming(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
) -> zex_core::Result<zex_agent::Agent> {
    build_agent_resuming_with(cfg, cwd, Vec::new())
}

/// 所有 builder 共用的内核配置。
///
/// 单独抽出来是因为踩过：CLI 那条路（`build_agent_with_team`）曾经自己
/// `AgentConfig::new` 一遍，忘了接 `params` 和技能清单 —— 于是用户在
/// 配置里写的 `max_tokens` 在 CLI 下静默失效，仍然是默认的 8192。
/// 参数只有一处来源，才不会漏。
fn agent_config_for(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
    skills: &Arc<zex_skill::SkillRegistry>,
) -> zex_agent::AgentConfig {
    zex_agent::AgentConfig::new(cfg.provider.clone(), cfg.model.clone(), cwd)
        .with_extra_prompt(skills.prompt_section())
        // 模型参数跟着配置走：温度决定"同样的问法给不给同样的答案"，
        // 是用户最容易感知的一项
        .with_model_params(cfg.model_params.temperature, cfg.model_params.max_tokens)
}

/// 同上，但额外挂上一些工具（例如 MCP 服务提供的）。
///
/// 单独开一个函数而不是给原函数加参数：`build_agent_resuming`
/// 已经是 CLI / GUI 共用的稳定入口，加参数会牵动所有调用点。
pub fn build_agent_resuming_with(
    cfg: &ResolvedConfig,
    cwd: PathBuf,
    extra_tools: Vec<Arc<dyn zex_core::tool::ErasedTool>>,
) -> zex_core::Result<zex_agent::Agent> {
    let store = open_store()?;
    // 上次进程退出时还在 running 的任务已经不可能再跑了：标成失败并
    // 写明原因，否则它们会永远挂在「运行中」，dashboard 上说谎。
    match store.fail_orphaned_running_tasks() {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "已标记上次退出时未完成的任务"),
        Err(e) => tracing::warn!(error = %e, "清理孤儿任务失败"),
    }
    let skills = load_skills();
    let agent_cfg = agent_config_for(cfg, cwd, &skills);

    // 跳过空会话：每次启动都会留下一个没说话的会话壳，
    // 直接恢复它会让用户以为历史丢了。
    let latest = store.list_visible_sessions(8)?.into_iter().find(|s| {
        !store
            .load_messages(&s.id)
            .map(|m| m.is_empty())
            .unwrap_or(true)
    });

    let mut tools = zex_tools::default_tools();
    if !skills.all().is_empty() {
        tools.push(Arc::new(zex_skill::LoadSkill::new(skills.clone())));
    }
    // MCP 工具排在最后：它们风险等级统一是 Impactful，
    // 排在后面能让审批界面上先看到"自己要干什么"
    tools.extend(extra_tools);

    match latest {
        Some(session) => {
            // 恢复失败时退回新建，不让一次数据问题把应用卡死
            match zex_agent::Agent::resume(
                agent_cfg.clone(),
                tools.clone(),
                store.clone(),
                session.id,
            ) {
                Ok(a) => Ok(a),
                Err(e) => {
                    tracing::warn!(error = %e, "恢复会话失败，改为新建");
                    zex_agent::Agent::new(agent_cfg, tools, store)
                }
            }
        }
        None => zex_agent::Agent::new(agent_cfg, tools, store),
    }
}

/// 把配置文件里的模型参数套到这一轮的内核配置上。
///
/// 读不到配置（文件缺失、这个模型没配参数）时回到默认值，而不是沿用
/// 上一个模型留下的值 —— 否则从"上限 2048 的模型"切到没配参数的模型，
/// 会莫名其妙还是 2048。
fn apply_model_params(cfg: &mut zex_agent::AgentConfig, provider: &str, model: &str) {
    let file = read_file_config();
    let params = params_for_model(file.as_ref(), provider, model);
    cfg.temperature = params.temperature;
    cfg.max_tokens = params.max_tokens.unwrap_or(zex_agent::DEFAULT_MAX_TOKENS);
}

/// 从配置里取某个模型的参数；没有就返回全空，交给调用方回落到默认值。
///
/// 单独一个纯函数是为了能测：`apply_model_params` 要读磁盘，测起来得
/// 折腾环境变量，而"没配参数时该回到默认"这条恰恰是最容易写错的地方。
fn params_for_model(file: Option<&FileConfig>, provider: &str, model: &str) -> ModelParams {
    file.and_then(|f| {
        f.providers
            .get(provider)
            .and_then(|e| e.params.get(model).cloned())
    })
    .unwrap_or_default()
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
) -> (
    Arc<zex_agent::subagent::AgentRegistry>,
    Vec<Arc<dyn zex_core::tool::ErasedTool>>,
) {
    use zex_agent::subagent::{
        AgentRegistry, SendMessage, SpawnAgent, TeamStatus, TerminateAgent, WaitForAgents,
    };

    let registry = Arc::new(AgentRegistry::new());
    let mut tools = zex_tools::default_tools();
    tools.push(Arc::new(SpawnAgent {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(TeamStatus {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(WaitForAgents {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(SendMessage {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(TerminateAgent {
        registry: registry.clone(),
    }));
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
    let skills = load_skills();
    let agent_cfg = agent_config_for(cfg, cwd, &skills);
    let broker = Arc::new(zex_agent::ApprovalBroker::new());
    let (registry, tools) =
        attach_team_tools(agent_cfg.clone(), store.clone(), ev_tx, broker.clone());
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
    let skills = load_skills();
    let agent_cfg = agent_config_for(cfg, cwd, &skills);
    let broker = Arc::new(zex_agent::ApprovalBroker::new());
    let (registry, tools) =
        attach_team_tools(agent_cfg.clone(), store.clone(), ev_tx, broker.clone());
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

/// 让智能体自己开关右侧面板。
///
/// 它不碰文件、不跑命令，只往事件通道里塞一条请求 —— UI 收到后
/// 再决定照不照办。所以对模型来说这是一次「请求」而非「执行」：
/// 返回的话术也是"已请求"，不是"已打开"。
pub struct ControlPanel {
    /// 事件通道，用来把请求送到 UI 世界。
    ev_tx: mpsc::Sender<CoreEvent>,
}

impl ControlPanel {
    /// 绑定事件通道。
    pub fn new(ev_tx: mpsc::Sender<CoreEvent>) -> Self {
        Self { ev_tx }
    }
}

/// `control_panel` 的参数。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ControlPanelArgs {
    /// true 打开面板，false 关闭。
    pub open: bool,
    /// 要打开哪个子智能体的面板（用 team_status 里的 id）。
    /// 不填则由界面决定，通常是最近有动静的那个。
    #[serde(default)]
    pub agent: Option<String>,
}

#[async_trait::async_trait]
impl zex_core::tool::Tool for ControlPanel {
    const NAME: &'static str = "control_panel";
    // 只影响界面，不改任何状态
    const RISK: zex_core::Risk = zex_core::Risk::ReadOnly;
    type Args = ControlPanelArgs;

    fn description() -> &'static str {
        "打开或关闭右侧的子智能体面板。想带用户看某个子智能体的过程时用。\
         只能影响界面，不会改变任何文件或进程。"
    }

    async fn call(
        &self,
        _cx: &dyn zex_core::tool::ToolCtx,
        args: Self::Args,
    ) -> zex_core::Result<zex_core::tool::ToolOutput> {
        self.ev_tx
            .send(CoreEvent::PanelControl {
                open: args.open,
                agent: args.agent,
            })
            .await
            .map_err(|_| zex_core::Error::ToolFailed {
                tool: "control_panel".into(),
                message: "界面已经不在了，请求没送到".into(),
            })?;
        Ok(zex_core::tool::ToolOutput::ok(if args.open {
            "已请求打开子智能体面板".to_string()
        } else {
            "已请求关闭子智能体面板".to_string()
        }))
    }
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
            model: s.model,
            mode: s.mode,
        })
        .collect()
}

/// 内核在 tokio 世界里跑，两者用 channel 通信（内存 channel，零序列化）。
pub fn spawn_core(
    mut agent: Agent,
    providers: std::collections::BTreeMap<String, zex_llm::ProviderConfig>,
    default_provider: String,
) -> CoreHandles {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UiCommand>(64);
    let (ev_tx, ev_rx) = mpsc::channel::<CoreEvent>(256);
    let broker = agent.approvals();
    // 启动时那个 agent 只用来接管界面初值，不承担碰执行；
    // 真正的开关在执行时按会话从表里取（见 run_task）
    let cancel_flags: CancelFlags = Arc::new(Mutex::new(HashMap::new()));
    let inboxes: Inboxes = Arc::new(Mutex::new(HashMap::new()));

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
    // 面板开关：和团队工具同一批注册，因为它俩是一件事的两半 ——
    // 派了活、看进度、把人叫出来
    tools.push(Arc::new(ControlPanel::new(ev_tx.clone())));
    // 扩展自检：要 UI 线程画一帧才能验证脚本，所以和面板开关一样走事件通道。
    // 只在 GUI 路径挂（CLI 没有窗口，挂上只会等超时）。
    tools.push(Arc::new(ext_tools::ExtensionCheck::new(ev_tx.clone())));
    tools.push(Arc::new(ext_tools::ExtensionLogs));
    tools.push(Arc::new(ext_tools::ExtensionInstall));
    tools.push(Arc::new(ext_tools::ExtensionUninstall));
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
    let mode_cells: ModeCells = Arc::new(Mutex::new(HashMap::new()));

    let spawn_result = std::thread::Builder::new()
        .name("zex-core".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "无法创建内核运行时");
                    return;
                }
            };
            rt.block_on(async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        UiCommand::UserPrompt {
                            session,
                            text,
                            attachments,
                            model,
                            mode,
                            thinking,
                        } => {
                            // 派到后台执行，主循环立刻回去收下一条命令 ——
                            // 否则一个慢任务会把「切会话」「审批」全堵在队列里。
                            // 同一会话内部仍串行（run_task 里的会话锁），
                            // 避免两个任务同时往同一条 history 追加。
                            let (
                                cfg,
                                tools,
                                store,
                                ev,
                                locks,
                                broker,
                                providers,
                                cells,
                                flags,
                                inboxes,
                            ) = (
                                cfg.clone(),
                                tools.clone(),
                                store.clone(),
                                ev_tx.clone(),
                                session_locks.clone(),
                                broker.clone(),
                                providers.clone(),
                                mode_cells.clone(),
                                cancel_flags.clone(),
                                inboxes.clone(),
                            );
                            let default_provider = default_provider.clone();
                            tokio::spawn(async move {
                                // 请求级模型覆盖：换了模型不必重启内核
                                let mut task_cfg = cfg.clone();
                                // 运行模式走句柄：登录态是"每会话一个"，
                                // 不改进程状态，否则并行会话会互相覆盖
                                let cell = mode_cell_for(&cells, &session, task_cfg.mode);
                                if let Some(m) = mode {
                                    cell.set(m);
                                }
                                task_cfg.mode = cell.get();
                                if let Some(t) = thinking {
                                    task_cfg.thinking = t;
                                }
                                // 这一轮实际用哪个 provider/model：请求级覆盖
                                // 会换掉它，参数也要按换完之后的那个查
                                let mut eff_provider = default_provider.clone();
                                let mut eff_model = task_cfg.model.clone();
                                if let Some(m) = model.as_deref() {
                                    if let Some((p, rest)) = m.split_once('/') {
                                        if let Some(pc) = providers.get(p) {
                                            task_cfg.provider = pc.clone();
                                            task_cfg.model = rest.to_string();
                                            eff_provider = p.to_string();
                                            eff_model = rest.to_string();
                                            // 换模型顺带换预算：从 128K 的模型
                                            // 切到 1M 的，压缩阈值得跟着走
                                            task_cfg.budget =
                                                zex_agent::context::ContextBudget::for_model(rest);
                                        }
                                    }
                                }
                                // 参数**每一轮**都从配置文件重读，不只是换模型时。
                                // 用户在设置里改了温度或上限，下一句话就该按新值走 ——
                                // 要求重启才生效没有道理，而且他看不出来没生效。
                                apply_model_params(&mut task_cfg, &eff_provider, &eff_model);
                                run_task(
                                    task_cfg,
                                    tools,
                                    store,
                                    session,
                                    text,
                                    attachments,
                                    ev,
                                    locks,
                                    broker,
                                    cell,
                                    flags,
                                    inboxes,
                                )
                                .await;
                            });
                        }
                        UiCommand::SetMode { session, mode } => {
                            // 只改句柄：正在跑的那一轮下一次判定就会读到新值，
                            // 不需要等它结束、也不需要重启内核
                            let cell = mode_cell_for(&mode_cells, &session, mode);
                            cell.set(mode);
                            tracing::debug!(session = %session, mode = ?mode, "运行模式已切换");
                        }
                        UiCommand::Transcribe { audio } => {
                            // 转写不走对话模型，直接调 provider 的 ASR 端点
                            let http = agent.http().clone();
                            let provider = agent.provider().clone();
                            let ev = ev_tx.clone();
                            tokio::spawn(async move {
                                let out = zex_llm::transcribe(&http, &provider, audio).await;
                                let (text, error) = match out {
                                    Ok(t) if t.is_empty() => {
                                        (None, Some("没有识别出内容，可能说得太轻".to_string()))
                                    }
                                    Ok(t) => (Some(t), None),
                                    Err(e) => (None, Some(e.to_string())),
                                };
                                let _ = ev.send(CoreEvent::Transcribed { text, error }).await;
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
                        UiCommand::NewSession => match agent.new_session() {
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
                        },
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
                        UiCommand::Steer {
                            session,
                            text,
                            attachments,
                        } => {
                            // 进收件箱，不新开一轮。正在跑的那一轮下一个 step
                            // 开头就会把它并进上下文
                            let inbox = {
                                let mut held =
                                    zex_core::sync::lock_or_panic(&inboxes, "插话收件箱表");
                                held.entry(session)
                                    .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
                                    .clone()
                            };
                            let mut content = vec![zex_core::Part::text(text)];
                            for img in attachments {
                                content.push(zex_core::Part::Image(img));
                            }
                            zex_core::sync::lock_or_panic(&inbox, "插话收件箱")
                                .push(zex_core::Message::User { content });
                        }
                        UiCommand::Cancel { session } => {
                            // 表里没有就现建一个：任务可能还没跑到这一步，
                            // `run_task` 拿到的会是同一个（entry 语义），
                            // 它开跑前会先看一眼 —— 排队期间按的停止不算白按
                            let flag = {
                                let mut held =
                                    zex_core::sync::lock_or_panic(&cancel_flags, "取消开关表");
                                held.entry(session)
                                    .or_insert_with(|| {
                                        Arc::new(std::sync::atomic::AtomicBool::new(false))
                                    })
                                    .clone()
                            };
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
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

    CoreHandles {
        cmd_tx,
        event_rx: ev_rx,
    }
}

/// 每个会话一把锁：同一会话的任务排队，不同会话并行。
type SessionLocks = Arc<Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>;

/// 每个会话一个取消开关。
///
/// `run_task` 每轮都会新建 `Agent`，如果让每个实例自己 new 一个开关，
/// 界面上那个「停止」按的永远是**启动时那个 agent** 的开关 —— 和真正在跑的
/// 那一个毫无关系，点了自然没反应。所以开关按会话存，谁跑都用同一个。
type CancelFlags = Arc<Mutex<HashMap<SessionId, Arc<std::sync::atomic::AtomicBool>>>>;

/// 每个会话一个插话收件箱。
///
/// 和取消开关同理：agent 实例每轮重建，执行中插的话得有个不随实例消失的
/// 落点。`Steer` 往这里投，`run_turn` 每个 step 开头取一次。
type Inboxes = Arc<Mutex<HashMap<SessionId, Arc<Mutex<Vec<zex_core::Message>>>>>>;

/// 每个会话一个模式句柄，UI 与正在跑的那一轮共享。
///
/// 单独一张表而不是塞进 `AgentConfig`：UI 必须能在任务跑着的时候
/// 改模式，而那时 `Agent` 在它自己的任务里，UI 够不着它。
type ModeCells = Arc<Mutex<HashMap<SessionId, zex_core::ModeCell>>>;

/// 取（必要时建）某个会话的模式句柄。
fn mode_cell_for(
    cells: &ModeCells,
    session: &SessionId,
    fallback: zex_core::RunMode,
) -> zex_core::ModeCell {
    // std 锁：这张表只在"取句柄"时碰一下，不跨 await 持有
    let mut guard = zex_core::sync::lock_or_panic(cells, "模式句柄表");
    guard
        .entry(session.clone())
        .or_insert_with(|| zex_core::ModeCell::new(fallback))
        .clone()
}

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
    mode_cell: zex_core::ModeCell,
    cancel_flags: CancelFlags,
    inboxes: Inboxes,
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
        let mut held = zex_core::sync::lock_or_panic(&locks, "会话锁表");
        held.entry(session.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;

    // 这个会话的取消开关。排队等锁期间用户就按了停止的话，这里已经是 true ——
    // 不能装看不见，否则他会看到一个点了没反应、然后照样跑起来的按钮
    let cancel_flag = {
        let mut held = zex_core::sync::lock_or_panic(&cancel_flags, "取消开关表");
        held.entry(session.clone())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone()
    };
    // 消费掉这个请求：排队期间按的停止只该取消这一轮，不能顺着开关
    // 一路把后面几个任务也带走
    if cancel_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = store.finish_task(
            &task_id,
            zex_store::tasks::TaskStatus::Cancelled,
            None,
            Some("排队期间被取消"),
        );
        let _ = ev_tx
            .send(CoreEvent::TurnEnded {
                session: session.clone(),
                turn: zex_core::TurnId::generate(),
                reason: zex_core::EndReason::Cancelled,
                usage: Default::default(),
                steps: 0,
                elapsed_ms: 0,
            })
            .await;
        return;
    }

    // 任务结束时把开关关回去。没有这一步的话，跑到一半按了停止，
    // 那个 `true` 会一直挂到下一次任务——用户下次发消息，agent 第一步
    // 就看见"已被取消"，任务刚开始就结束。
    //
    // 用 Drop 而不是在末尾手写：取消、失败、panic 三条路径都要清。
    struct ResetOnDrop(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for ResetOnDrop {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _reset = ResetOnDrop(cancel_flag.clone());

    // 拿到锁才算真的开始跑
    let _ = store.start_task(&task_id);

    // 这个会话的插话收件箱：`Steer` 往这里投，agent 每个 step 取一次
    let inbox = {
        let mut held = zex_core::sync::lock_or_panic(&inboxes, "插话收件箱表");
        held.entry(session.clone())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .clone()
    };

    let mut agent = match Agent::resume(cfg, tools, store.clone(), session.clone()) {
        Ok(agent) => agent
            .with_shared_approvals(broker)
            .with_mode_cell(mode_cell)
            // 关了键：界面上的「停止」按的就是它
            .with_cancel_flag(cancel_flag)
            // 执行中插的话从这里取
            .with_inbox(inbox),
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
            // 用户按停止也是一条 Err —— 但它不是故障，记成 Cancelled
            // 才不会在工作台上挂一条红色的失败
            let (status, note) = if matches!(e, zex_core::Error::Cancelled) {
                (zex_store::tasks::TaskStatus::Cancelled, "用户中断")
            } else {
                (zex_store::tasks::TaskStatus::Failed, "见错误详情")
            };
            let _ = store.finish_task(&task_id, status, None, Some(note));
        }
    }

    if let Err(e) = outcome {
        // 中断已经由 TurnEnded(Cancelled) 交代过了，再弹一个 Fatal
        // 会在界面上盖一条红色错误 —— 用户明明是自己按的停止
        if !matches!(e, zex_core::Error::Cancelled) {
            let _ = ev_tx
                .send(CoreEvent::Fatal {
                    message: e.to_string(),
                })
                .await;
        }
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

#[cfg(test)]
mod model_params_tests {
    use super::*;
    use crate::config::ProviderEntry;

    fn file_with(params: ModelParams) -> FileConfig {
        let mut f = FileConfig::default();
        f.providers.insert(
            "deepseek".into(),
            ProviderEntry {
                kind: "openai-compat".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                params: [("deepseek-flash".to_string(), params)]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        f
    }

    #[test]
    fn configured_params_are_used() {
        let f = file_with(ModelParams {
            temperature: Some(0.3),
            max_tokens: Some(2048),
        });
        let p = params_for_model(Some(&f), "deepseek", "deepseek-flash");
        assert_eq!(p.temperature, Some(0.3));
        assert_eq!(p.max_tokens, Some(2048));
    }

    #[test]
    fn missing_entry_falls_back_to_defaults() {
        // 模型没配参数：温度交回服务端默认，上限回到 DEFAULT_MAX_TOKENS，
        // 不能沿用上一个模型留下的值
        let f = file_with(ModelParams::default());
        let p = params_for_model(Some(&f), "deepseek", "deepseek-flash");
        assert_eq!(p.temperature, None);
        assert_eq!(p.max_tokens, None);
    }

    #[test]
    fn unknown_provider_or_model_is_harmless() {
        let f = file_with(ModelParams {
            max_tokens: Some(4096),
            ..Default::default()
        });
        assert!(
            params_for_model(Some(&f), "nope", "deepseek-flash")
                .max_tokens
                .is_none()
        );
        assert!(
            params_for_model(Some(&f), "deepseek", "nope")
                .max_tokens
                .is_none()
        );
        assert!(
            params_for_model(None, "deepseek", "deepseek-flash")
                .max_tokens
                .is_none()
        );
    }
}

#[cfg(test)]
mod system_banner_tests {
    use super::*;

    #[test]
    fn short_completion_pill_stays_a_pill() {
        assert!(!system_banner_is_long("✓ 任务完成 · 共 3 步，用时 12 秒"));
    }

    #[test]
    fn long_error_gets_the_wide_layout() {
        // 真实报错带一段原文，远超一行
        let text = format!(
            "✗ 出错了：工具 `fs_write` 的参数不是合法 JSON: {}",
            "字".repeat(200)
        );
        assert!(system_banner_is_long(&text));
    }

    #[test]
    fn newlines_count_as_long_even_when_short() {
        // 短短两行也是多行，胶囊里排不下
        assert!(system_banner_is_long("第一行\n第二行"));
    }
}

#[cfg(test)]
mod control_panel_tests {
    use super::*;
    use zex_core::tool::{Tool, ToolCtx};

    struct MockCtx {
        sid: SessionId,
    }

    impl ToolCtx for MockCtx {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/tmp")
        }
        fn session_id(&self) -> &SessionId {
            &self.sid
        }
        fn resolve_path(&self, raw: &str) -> zex_core::Result<PathBuf> {
            Ok(PathBuf::from(raw))
        }
    }

    #[tokio::test]
    async fn open_request_reaches_the_ui_channel() {
        let (tx, mut rx) = mpsc::channel(4);
        let tool = ControlPanel::new(tx);
        let ctx = MockCtx {
            sid: SessionId::generate(),
        };
        let out = tool
            .call(
                &ctx,
                ControlPanelArgs {
                    open: true,
                    agent: Some("coder-1".into()),
                },
            )
            .await
            .expect("发送应当成功");
        // 回给模型的是「已请求」而不是「已打开」：开不开由界面说了算
        assert!(out.content.contains("已请求"), "got {}", out.content);

        match rx.recv().await.expect("应收到事件") {
            CoreEvent::PanelControl { open, agent } => {
                assert!(open);
                assert_eq!(agent.as_deref(), Some("coder-1"));
            }
            other => panic!("事件类型不对: {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_request_carries_no_agent() {
        let (tx, mut rx) = mpsc::channel(4);
        let tool = ControlPanel::new(tx);
        let ctx = MockCtx {
            sid: SessionId::generate(),
        };
        tool.call(
            &ctx,
            ControlPanelArgs {
                open: false,
                agent: None,
            },
        )
        .await
        .expect("发送应当成功");
        match rx.recv().await.expect("应收到事件") {
            CoreEvent::PanelControl { open, agent } => {
                assert!(!open);
                assert!(agent.is_none());
            }
            other => panic!("事件类型不对: {other:?}"),
        }
    }
}

#[cfg(test)]
mod skills_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zex-skills-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 内置技能现在落在哪：`<dir>/<名字>/skill.md`。
    fn bundled_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("write-extension").join(zex_skill::SKILL_FILE)
    }

    #[test]
    fn bundled_skill_is_installed_into_its_own_directory() {
        let dir = temp_dir("install");
        install_bundled_skills(&dir);
        let path = bundled_path(&dir);
        assert!(path.exists(), "内置技能应被铺到 <名字>/skill.md");
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(once.contains("write-extension") || once.contains("给 zex 写"));
        assert!(
            dir.join(BUNDLE_STATE).exists(),
            "应记下铺的版本，用于下次判断"
        );
    }

    #[test]
    fn unmodified_copy_is_upgraded() {
        let dir = temp_dir("upgrade");
        install_bundled_skills(&dir);
        let path = bundled_path(&dir);
        // 模拟"旧版铺下去、用户没碰"：内容变了，但记录也跟着变
        let stale = "---\nname: write-extension\ndescription: 旧版\n---\n\n旧内容\n";
        std::fs::write(&path, stale).unwrap();
        std::fs::write(
            dir.join(BUNDLE_STATE),
            serde_json::json!({"write-extension": fnv1a(stale)}).to_string(),
        )
        .unwrap();
        install_bundled_skills(&dir);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("旧内容"),
            "没改过的旧版必须被升级，否则会误导模型"
        );
        assert!(after.contains("icon.svg"), "新版内容应到位");
    }

    #[test]
    fn user_edited_copy_is_never_overwritten() {
        let dir = temp_dir("keep");
        let path = bundled_path(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mine = "---\nname: write-extension\ndescription: 我改过的\n---\n\n别动我\n";
        std::fs::write(&path, mine).unwrap();
        // 记录里是别的版本 → 说明用户动过
        std::fs::write(
            dir.join(BUNDLE_STATE),
            serde_json::json!({"write-extension": fnv1a("别的版本")}).to_string(),
        )
        .unwrap();
        install_bundled_skills(&dir);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("别动我"), "内置技能不该覆盖用户改过的版本");
    }

    #[test]
    fn copy_without_record_is_migrated_with_backup() {
        let dir = temp_dir("migrate");
        let path = bundled_path(&dir);
        // 这个机制上线前装的：文件在，但没有记录
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: write-extension\n---\n\n上一代内容\n").unwrap();
        install_bundled_skills(&dir);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("icon.svg"), "没有记录时也要升级到新版");
        let backup = dir.join("write-extension").join(".bundled-backup.md");
        let kept = std::fs::read_to_string(&backup).unwrap();
        assert!(
            kept.contains("上一代内容"),
            "旧内容必须留个备份，不能凭空消失"
        );
    }

    #[test]
    fn legacy_flat_copy_is_removed_after_moving_into_a_directory() {
        // 老布局：`<dir>/write-extension.md`，是我们自己铺的、用户没动过
        let dir = temp_dir("legacy-clean");
        let legacy = dir.join("write-extension.md");
        let old = "---\nname: write-extension\ndescription: 上一版\n---\n\n旧内容\n";
        std::fs::write(&legacy, old).unwrap();
        std::fs::write(
            dir.join(BUNDLE_STATE),
            serde_json::json!({"write-extension.md": fnv1a(old)}).to_string(),
        )
        .unwrap();

        install_bundled_skills(&dir);

        assert!(
            !legacy.exists(),
            "没改过的旧布局文件应被清理，否则会和目录版撞名"
        );
        assert!(bundled_path(&dir).exists(), "新布局要就位");
        let state = read_bundle_state(&dir.join(BUNDLE_STATE));
        assert!(
            !state.contains_key("write-extension.md"),
            "旧键该从记录里去掉"
        );
    }

    #[test]
    fn edited_legacy_flat_copy_is_kept() {
        // 老布局但用户改过：一个字都不能删
        let dir = temp_dir("legacy-keep");
        let legacy = dir.join("write-extension.md");
        std::fs::write(&legacy, "---\nname: write-extension\n---\n\n我改过的\n").unwrap();
        std::fs::write(
            dir.join(BUNDLE_STATE),
            serde_json::json!({"write-extension.md": fnv1a("完全不同的内容")}).to_string(),
        )
        .unwrap();

        install_bundled_skills(&dir);

        assert!(legacy.exists(), "改过的旧文件不能删");
        assert!(bundled_path(&dir).exists(), "新版目录也照铺");
    }

    #[test]
    fn fnv1a_is_stable() {
        // 这个值要落盘跨版本使用，变了就等于所有人的技能都被判成"用户改过"
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a("abc"), 0xe71f_a219_0541_574b);
    }
}
