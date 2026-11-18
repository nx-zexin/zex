//! 配置加载。
//!
//! 优先级（后者覆盖前者）：
//! 1. 内置默认
//! 2. `~/.zex/config.json`
//! 3. 环境变量（`ZEX_*`）
//!
//! 密钥**只存环境变量名**，不存密钥本身 —— 配置可以被打印、被提交，
//! 而密钥永不进入模型上下文（`docs/SECURITY.md` §5.3）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zex_core::{Error, Result};
use zex_llm::{ProviderConfig, ProviderKind};

/// 单个模型的采样参数。
///
/// 只收"换了会让输出明显不同"的那几个。全部透传反而危险：
/// 不同 provider 支持的字段不一样，写错了会被服务端直接拒掉。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelParams {
    /// 采样温度，0–2。越低越确定。留空用服务端默认。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 单次输出 token 上限。留空用程序默认（8192）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// 一个 provider 的 JSON 表示。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// `openai-compat` 或 `anthropic`。
    pub kind: String,
    /// 基础 URL。
    pub base_url: String,
    /// 密钥所在的环境变量名。
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 额外请求头。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 这个 provider 下可选的模型名。
    ///
    /// 留空时按 provider 名回退到内置清单（见 [`builtin_models`]）。
    /// 不在这里硬编码全部：模型上下线比程序发版快，用户能自己写才跟得上。
    #[serde(default)]
    pub models: Vec<String>,
    /// 按模型名索引的采样参数。
    ///
    /// 单开一个映射而不是把 `models` 改成对象数组：老配置里
    /// `models` 是字符串数组，改结构会让所有现存配置失效。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ModelParams>,
}

/// `config.json` 的结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileConfig {
    /// 所有 provider。
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    /// 默认模型，形如 `anthropic/claude-sonnet-4-6`。
    #[serde(default)]
    pub default_model: Option<String>,
    /// MCP 服务端，键是服务名。
    ///
    /// 留空即不启用 MCP。每个服务是独立子进程，启动时会尝试连接 ——
    /// 连不上的只是不挂工具，不影响应用启动。
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, zex_mcp::ServerConfig>,
    /// 固定到侧边栏的扩展 id，按固定顺序排列。
    ///
    /// 存的是 id 而不是完整清单：扩展装在哪、叫什么，都以目录扫描为准。
    /// 这里只是「用户希望它在侧边栏露头」这一条意见，
    /// 所以扩展被卸掉之后，这里残留的 id 会被安静地忽略。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_extensions: Vec<String>,
    /// 用户已批准其能力申请的扩展 id。
    ///
    /// 只记"用户看过并同意了"，**不等于能力已生效**：当前是进程级单一能力集，
    /// 按应用授予会串味。这一栏留给 per-app 策略落地后直接使用，
    /// 现在的作用是留痕 + 界面区分"未审阅"与"已批准"。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_extensions: Vec<String>,
}

/// 解析后的运行时配置。
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// provider 连接配置。
    pub provider: ProviderConfig,
    /// 模型名。
    pub model: String,
    /// 默认模型所属的 provider 名。
    pub provider_name: String,
    /// 配置文件里声明的全部 provider（按名字索引）。
    ///
    /// 界面上的模型切换要能列出可选项，光有默认那一个不够。
    pub all_providers: BTreeMap<String, ProviderConfig>,
    /// 每个 provider 下可选的模型（provider → 模型名列表）。
    ///
    /// 供模型菜单做二级展示：一级机构，二级模型。
    pub models: BTreeMap<String, Vec<String>>,
    /// 当前默认模型的采样参数。
    ///
    /// 只有默认那个模型带参数：切换模型走的是请求级覆盖，
    /// 那时由 `params_for` 现查，不必让 ResolvedConfig 背着全部。
    pub model_params: ModelParams,
}

impl ResolvedConfig {
    /// 查某个模型配置的参数（`provider/model` 或裸模型名）。
    pub fn params_for(&self, full: &str) -> Option<ModelParams> {
        let file = read_file_config()?;
        let (p, m) = match full.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => (self.provider_name.clone(), full.to_string()),
        };
        file.providers.get(&p)?.params.get(&m).cloned()
    }
}

/// Zex 的配置目录 `~/.zex`。
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZEX_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".zex")
}

/// 加载并合并配置。
pub fn load() -> Result<ResolvedConfig> {
    let file = read_file_config().unwrap_or_default();

    // ─── 环境变量覆盖 ───
    let env_kind = std::env::var("ZEX_PROVIDER").ok();
    let env_base = std::env::var("ZEX_BASE_URL").ok();
    let env_model = std::env::var("ZEX_MODEL").ok();
    let env_key_env = std::env::var("ZEX_API_KEY_ENV").ok();

    if env_kind.is_some() || env_base.is_some() {
        let kind_str = env_kind.unwrap_or_else(|| "anthropic".to_string());
        let kind = parse_kind(&kind_str)?;
        let base_url = env_base.unwrap_or_else(|| default_base_url(kind).to_string());
        let api_key_env = env_key_env.or_else(|| default_key_env(kind).map(str::to_string));
        let model = env_model.or(file.default_model.clone()).ok_or_else(|| {
            Error::Config(
                "通过环境变量配置时，还需要 ZEX_MODEL（例如 ZEX_MODEL=claude-sonnet-4-6）".into(),
            )
        })?;
        let provider = ProviderConfig {
            kind,
            base_url,
            api_key_env,
            headers: BTreeMap::new(),
        };
        let mut all = BTreeMap::new();
        all.insert(kind_str.clone(), provider.clone());
        let mut models = BTreeMap::new();
        models.insert(kind_str.clone(), model_list(&ProviderEntry::default(), &kind_str, Some(&model)));
        return Ok(ResolvedConfig {
            provider,
            model,
            provider_name: kind_str,
            all_providers: all,
            models,
            // 走环境变量配置时没有 provider 条目可查，用默认参数
            model_params: ModelParams::default(),
        });
    }

    // ─── 走配置文件 ───
    let default_model = file.default_model.clone().ok_or_else(|| {
        Error::Config(format!(
            "还没有配置模型。\n\
             方式一：创建 {}，内容形如\n{}",
            config_dir().join("config.json").display(),
            example_config()
        ))
    })?;

    // default_model 形如 "provider/model"，也允许不带动前缀
    let (provider_name, model) = match default_model.split_once('/') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => {
            // 只有一个 provider 时允许省略前缀
            if file.providers.len() == 1 {
                (
                    file.providers.keys().next().cloned().unwrap_or_default(),
                    default_model,
                )
            } else {
                return Err(Error::Config(format!(
                    "default_model `{default_model}` 缺少 provider 前缀，应为 `provider/model`"
                )));
            }
        }
    };

    let entry = file.providers.get(&provider_name).ok_or_else(|| {
        Error::Config(format!(
            "配置里找不到 provider `{provider_name}`，已定义的有：{}",
            file.providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;

    let kind = parse_kind(&entry.kind)?;
    // 收集全部 provider 供界面列出
    let mut all_providers = BTreeMap::new();
    for (name, e) in &file.providers {
        if let Ok(k) = parse_kind(&e.kind) {
            all_providers.insert(
                name.clone(),
                ProviderConfig {
                    kind: k,
                    base_url: e.base_url.clone(),
                    api_key_env: e.api_key_env.clone(),
                    headers: e.headers.clone(),
                },
            );
        }
    }

    // 每个 provider 的模型清单：默认模型的 provider 要把当前模型排进去，
    // 否则菜单里选不中正在用的那个
    let mut models = BTreeMap::new();
    for (name, e) in &file.providers {
        let cur = (name == &provider_name).then_some(model.as_str());
        models.insert(name.clone(), model_list(e, name, cur));
    }

    let model_params = entry.params.get(&model).cloned().unwrap_or_default();

    Ok(ResolvedConfig {
        provider: ProviderConfig {
            kind,
            base_url: entry.base_url.clone(),
            api_key_env: entry.api_key_env.clone(),
            headers: entry.headers.clone(),
        },
        model,
        provider_name,
        all_providers,
        models,
        model_params,
    })
}

/// 内置的 provider → 常见模型清单。
///
/// 只是**起点**：配置里写了 `models` 就以配置为准。模型上下线比程序发版快，
/// 这里列的东西用户随时能覆盖。
fn builtin_models(provider: &str) -> &'static [&'static str] {
    match provider.to_ascii_lowercase().as_str() {
        // DeepSeek 现行模型（官方定价页，2026-11）：
        // deepseek-flash 与 deepseek-v4-pro。旧的 deepseek-chat /
        // deepseek-reasoner 已不在文档里，仍可调用但会映射到新模型。
        "deepseek" => &["deepseek-flash", "deepseek-v4-pro"],
        "anthropic" | "claude" => &[
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ],
        "openai" | "gpt" => &["gpt-5", "gpt-5-mini", "o4"],
        "moonshot" | "moonshotai" | "kimi" => &["kimi-k2", "kimi-k2-turbo"],
        "google" | "gemini" => &["gemini-2.5-pro", "gemini-2.5-flash"],
        _ => &[],
    }
}

/// 一个 provider 的可选模型清单。
///
/// 顺序：配置显式声明 > 内置清单 > 当前模型兜底。
/// 当前模型永远排在第一位 —— 它是"正在用的"，不该藏在列表深处。
fn model_list(entry: &ProviderEntry, name: &str, current: Option<&str>) -> Vec<String> {
    let mut list: Vec<String> = if !entry.models.is_empty() {
        entry.models.clone()
    } else {
        builtin_models(name)
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    };
    if let Some(cur) = current {
        list.retain(|m| m != cur);
        list.insert(0, cur.to_string());
    }
    list
}

/// 读取 `~/.zex/config.json`（解析失败按空配置处理）。
pub fn read_file_config() -> Option<FileConfig> {
    let path = config_dir().join("config.json");
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "配置文件解析失败，已忽略");
            None
        }
    }
}

// ─── 配置变更（纯数据操作，便于测试）───
//
// 这些函数只动 `FileConfig`，不碰磁盘也不碰界面。
// 放这里而不是塞在 UI 里，是因为它们最容易出错的地方是「改名之后
// 默认模型指向谁」这类边界，那正是单测该覆盖的。

/// 新增或修改一个 provider。
///
/// `editing` 非空表示这是一次改名：旧名下的模型列表和默认模型指针
/// 会一起搬到新名下 —— 不搬的话，改个名字就把整组模型弄丢了。
pub fn upsert_provider(
    cfg: &mut FileConfig,
    name: &str,
    entry: ProviderEntry,
    editing: Option<&str>,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::Config("provider 名不能为空".into()));
    }
    if editing != Some(name) && cfg.providers.contains_key(name) {
        return Err(Error::Config(format!("已存在名为 `{name}` 的 provider")));
    }

    let mut entry = entry;
    if let Some(old) = editing {
        if let Some(prev) = cfg.providers.remove(old) {
            // 调用方没显式给模型时，继承旧的
            if entry.models.is_empty() {
                entry.models = prev.models;
            }
        }
        // 默认模型跟着改名走
        if let Some(dm) = &cfg.default_model {
            if let Some(rest) = dm.strip_prefix(format!("{old}/").as_str()) {
                cfg.default_model = Some(format!("{name}/{rest}"));
            }
        }
    }
    cfg.providers.insert(name.to_string(), entry);
    Ok(())
}

/// 把某个扩展 id 加进/移出固定列表，返回**操作后**是否固定。
///
/// 抽成纯函数是为了能单独测：固定列表是"用户偏好"，它出错的表现是
/// 侧边栏莫名其妙多一行或少一行，光看界面很难判断是扫描的问题还是配置的问题。
pub fn toggle_pinned(pinned: &mut Vec<String>, id: &str) -> bool {
    match pinned.iter().position(|x| x == id) {
        Some(i) => {
            pinned.remove(i);
            false
        }
        None => {
            pinned.push(id.to_string());
            true
        }
    }
}

/// 删除一个 provider，并在必要时挪走默认模型指针。
pub fn remove_provider(cfg: &mut FileConfig, name: &str) {
    cfg.providers.remove(name);
    let points_here = cfg
        .default_model
        .as_deref()
        .map(|d| d.starts_with(&format!("{name}/")))
        .unwrap_or(false);
    if points_here {
        cfg.default_model = cfg
            .providers
            .iter()
            .find_map(|(p, e)| e.models.first().map(|m| format!("{p}/{m}")));
    }
}

/// 给某个 provider 增加一个模型。
///
/// 返回 false 表示已经存在（调用方据此提示用户，而不是静默跳过）。
pub fn add_model(cfg: &mut FileConfig, provider: &str, model: &str) -> bool {
    let Some(entry) = cfg.providers.get_mut(provider) else {
        return false;
    };
    if entry.models.iter().any(|m| m == model) {
        return false;
    }
    entry.models.push(model.to_string());
    true
}

/// 移除一个模型，并在必要时改掉默认模型。
pub fn remove_model(cfg: &mut FileConfig, provider: &str, model: &str) {
    if let Some(entry) = cfg.providers.get_mut(provider) {
        entry.models.retain(|m| m != model);
    }
    let full = format!("{provider}/{model}");
    if cfg.default_model.as_deref() == Some(full.as_str()) {
        cfg.default_model = cfg
            .providers
            .get(provider)
            .and_then(|e| e.models.first())
            .map(|m| format!("{provider}/{m}"));
    }
}

/// 把配置写回 `~/.zex/config.json`。
///
/// 先写临时文件再改名：写到一半崩溃会留下半截 JSON，
/// 下次启动就读不到配置了。
pub fn save_file_config(cfg: &FileConfig) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::Config(format!("创建配置目录失败: {e}")))?;
    let path = dir.join("config.json");
    let tmp = dir.join("config.json.tmp");
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| Error::Config(format!("序列化配置失败: {e}")))?;
    std::fs::write(&tmp, text).map_err(|e| Error::Config(format!("写配置失败: {e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::Config(format!("保存配置失败: {e}")))?;
    Ok(())
}

fn parse_kind(s: &str) -> Result<ProviderKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "openai" | "openai-compat" | "openai_compatible" => Ok(ProviderKind::OpenAiCompat),
        other => Err(Error::Config(format!(
            "未知的 provider 类型 `{other}`，支持：anthropic / openai-compat"
        ))),
    }
}

fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::OpenAiCompat => "https://api.openai.com",
    }
}

fn default_key_env(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
        ProviderKind::OpenAiCompat => Some("OPENAI_API_KEY"),
    }
}

/// 示例配置，用于错误提示。
pub fn example_config() -> String {
    r#"{
  "providers": {
    "anthropic": {
      "kind": "anthropic",
      "base_url": "https://api.anthropic.com",
      "api_key_env": "ANTHROPIC_API_KEY"
    },
    "local": {
      "kind": "openai-compat",
      "base_url": "http://127.0.0.1:11434/v1"
    }
  },
  "default_model": "anthropic/claude-sonnet-4-6"
}"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_falls_back_to_builtin() {
        // 配置里没写 models 时按 provider 名给一份起点
        let e = ProviderEntry::default();
        let list = model_list(&e, "deepseek", Some("deepseek-flash"));
        // 正在用的模型永远排第一
        assert_eq!(list[0], "deepseek-flash");
        assert!(list.iter().any(|m| m == "deepseek-v4-pro"));
        // 内置清单里也有它，但不能出现两次
        assert_eq!(list.iter().filter(|m| *m == "deepseek-flash").count(), 1);
    }

    #[test]
    fn model_list_prefers_configured() {
        let e = ProviderEntry {
            models: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        let list = model_list(&e, "deepseek", Some("b"));
        assert_eq!(list, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn unknown_provider_still_lists_current_model() {
        // 没见过的 provider 名不该让菜单变空
        let e = ProviderEntry::default();
        let list = model_list(&e, "my-local-llama", Some("llama-3"));
        assert_eq!(list, vec!["llama-3".to_string()]);
    }

    #[test]
    fn parses_both_kinds() {
        assert!(matches!(
            parse_kind("anthropic"),
            Ok(ProviderKind::Anthropic)
        ));
        assert!(matches!(
            parse_kind("openai"),
            Ok(ProviderKind::OpenAiCompat)
        ));
        assert!(matches!(
            parse_kind("openai-compat"),
            Ok(ProviderKind::OpenAiCompat)
        ));
        assert!(matches!(
            parse_kind("ANTHROPIC"),
            Ok(ProviderKind::Anthropic)
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse_kind("gemini").is_err());
    }

    #[test]
    fn config_dir_is_under_home() {
        let d = config_dir();
        assert!(d.ends_with(".zex") || std::env::var("ZEX_CONFIG_DIR").is_ok());
    }

    #[test]
    fn file_config_roundtrip() {
        let json = example_config();
        let c: FileConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c.providers.len(), 2);
        assert_eq!(
            c.default_model.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(c.providers["local"].api_key_env, None);
    }

    #[test]
    fn provider_without_key_env_is_allowed() {
        // 本地 Ollama 不需要鉴权
        let c: FileConfig = serde_json::from_str(
            r#"{"providers":{"o":{"kind":"openai-compat","base_url":"http://localhost:11434/v1"}}}"#,
        )
        .unwrap();
        assert!(c.providers["o"].api_key_env.is_none());
    }

    fn entry(models: &[&str]) -> ProviderEntry {
        ProviderEntry {
            kind: "openai-compat".into(),
            base_url: "https://example.com/v1".into(),
            api_key_env: Some("KEY".into()),
            headers: Default::default(),
            models: models.iter().map(|s| s.to_string()).collect(),
            params: Default::default(),
        }
    }

    fn cfg_with() -> FileConfig {
        let mut c = FileConfig::default();
        c.providers.insert("a".into(), entry(&["m1", "m2"]));
        c.default_model = Some("a/m1".into());
        c
    }

    #[test]
    fn add_provider_then_read_back() {
        let mut c = FileConfig::default();
        upsert_provider(&mut c, "p", entry(&["x"]), None).unwrap();
        assert_eq!(c.providers["p"].models, vec!["x"]);
    }

    #[test]
    fn rejects_duplicate_name_on_create() {
        let mut c = cfg_with();
        let e = upsert_provider(&mut c, "a", entry(&["n"]), None);
        assert!(e.is_err(), "同名新增应当被拒绝");
    }

    #[test]
    fn rename_keeps_models_and_default() {
        let mut c = cfg_with();
        // 改名时调用方不给模型，应当继承旧的
        let mut e = entry(&[]);
        e.base_url = "https://new/v1".into();
        upsert_provider(&mut c, "b", e, Some("a")).unwrap();

        assert!(!c.providers.contains_key("a"), "旧名应当消失");
        assert_eq!(c.providers["b"].models, vec!["m1", "m2"], "模型列表要跟过来");
        assert_eq!(c.default_model.as_deref(), Some("b/m1"), "默认模型要跟着改名");
        assert_eq!(c.providers["b"].base_url, "https://new/v1");
    }

    #[test]
    fn remove_provider_moves_default_pointer() {
        let mut c = cfg_with();
        c.providers.insert("b".into(), entry(&["n1"]));
        remove_provider(&mut c, "a");

        assert!(!c.providers.contains_key("a"));
        // 默认原来指向 a/m1，a 没了要挪到一个还活着的
        assert_eq!(c.default_model.as_deref(), Some("b/n1"));
    }

    #[test]
    fn remove_last_provider_clears_default() {
        let mut c = cfg_with();
        remove_provider(&mut c, "a");
        assert!(c.default_model.is_none(), "没有 provider 时默认模型应为空");
    }

    #[test]
    fn add_model_dedups() {
        let mut c = cfg_with();
        assert!(!add_model(&mut c, "a", "m1"), "已存在的模型不该重复加入");
        assert!(add_model(&mut c, "a", "m3"));
        assert_eq!(c.providers["a"].models, vec!["m1", "m2", "m3"]);
    }

    #[test]
    fn remove_default_model_falls_back() {
        let mut c = cfg_with();
        remove_model(&mut c, "a", "m1");
        assert_eq!(c.providers["a"].models, vec!["m2"]);
        assert_eq!(c.default_model.as_deref(), Some("a/m2"), "默认要换成剩下的第一个");
    }

    #[test]
    fn remove_non_default_model_keeps_default() {
        let mut c = cfg_with();
        remove_model(&mut c, "a", "m2");
        assert_eq!(c.default_model.as_deref(), Some("a/m1"));
    }

    #[test]
    fn model_params_round_trip_json() {
        let json = r#"{
            "kind": "openai-compat",
            "base_url": "https://x/v1",
            "models": ["m1"],
            "params": { "m1": { "temperature": 0.3, "max_tokens": 2048 } }
        }"#;
        let e: ProviderEntry = serde_json::from_str(json).expect("应能解析");
        let p = e.params.get("m1").expect("参数应存在");
        assert_eq!(p.temperature, Some(0.3));
        assert_eq!(p.max_tokens, Some(2048));
    }

    #[test]
    fn old_config_without_params_still_loads() {
        // 加 params 字段之前写下的配置：没有该键也要能读
        let json = r#"{"kind":"anthropic","base_url":"https://x","models":["m"]}"#;
        let e: ProviderEntry = serde_json::from_str(json).expect("老配置应能解析");
        assert!(e.params.is_empty());
    }

    #[test]
    fn empty_params_are_not_serialized() {
        // 没配参数的 provider 不该在文件里多出一个 "params": {}
        let e = ProviderEntry {
            kind: "openai-compat".into(),
            base_url: "https://x".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("params"), "空参数表不应写进文件：{s}");
    }

    #[test]
    fn params_only_hold_given_fields() {
        let json = r#"{ "temperature": 0.7 }"#;
        let p: ModelParams = serde_json::from_str(json).expect("应能解析");
        assert_eq!(p.temperature, Some(0.7));
        assert_eq!(p.max_tokens, None, "没写的字段应保持 None，而不是 0");
    }

    #[test]
    fn pinning_toggles_and_reports_new_state() {
        let mut pinned: Vec<String> = Vec::new();
        assert!(toggle_pinned(&mut pinned, "demo"), "第一次是固定");
        assert_eq!(pinned, ["demo"]);
        assert!(!toggle_pinned(&mut pinned, "demo"), "第二次是取消");
        assert!(pinned.is_empty());
    }

    #[test]
    fn pinning_keeps_order_and_ignores_duplicates() {
        let mut pinned = vec!["a".to_string(), "b".to_string()];
        // 已固定过的再 toggle 一次是取消，不会插出第二份
        assert!(!toggle_pinned(&mut pinned, "a"));
        assert_eq!(pinned, ["b"]);
        assert!(toggle_pinned(&mut pinned, "c"));
        // 顺序按固定先后，不排序 —— 用户摆的顺序就是他要的顺序
        assert_eq!(pinned, ["b", "c"]);
    }

    #[test]
    fn empty_pin_list_is_not_serialized() {
        let cfg = FileConfig::default();
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(!s.contains("pinned_extensions"), "空列表不该写进文件：{s}");
    }

    #[test]
    fn old_config_without_pin_field_still_loads() {
        // 老版本写的 config.json 里没有这个字段，必须能照常读
        let cfg: FileConfig = serde_json::from_str(r#"{"default_model":"a/b"}"#).unwrap();
        assert!(cfg.pinned_extensions.is_empty());
    }
}
