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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 配置文件里声明的全部 provider（按名字索引）。
    ///
    /// 界面上的模型切换要能列出可选项，光有默认那一个不够。
    pub all_providers: BTreeMap<String, ProviderConfig>,
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
        all.insert(kind_str, provider.clone());
        return Ok(ResolvedConfig {
            provider,
            model,
            all_providers: all,
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

    Ok(ResolvedConfig {
        provider: ProviderConfig {
            kind,
            base_url: entry.base_url.clone(),
            api_key_env: entry.api_key_env.clone(),
            headers: entry.headers.clone(),
        },
        model,
        all_providers,
    })
}

fn read_file_config() -> Option<FileConfig> {
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
