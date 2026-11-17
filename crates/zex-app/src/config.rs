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
}
