//! [models.dev](https://models.dev) 目录接入。
//!
//! 存在的理由：手打 `base_url` / `api_key_env` / 模型名全是易错项，
//! 而模型上下线比程序发版快得多。这里把目录拉下来，让用户点选而不是拼字符串。
//!
//! 三条约束：
//! 1. **不阻塞 UI**。拉取与解析都放后台线程，界面先显示缓存（可能过期）再刷新。
//! 2. **网络失败不致命**。目录只是便利功能，拿不到时退回手填。
//! 3. **缓存落盘**。7MB 的 JSON 每次启动都拉一遍既慢又失礼，缓存 24 小时。

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use zex_core::{Error, Result};

/// 上游目录地址。
///
/// 用 `api.json` 而不是 `catalog.json`：后者多出的 `models` 段是
/// provider 无关的规范模型，我们只需要"某 provider 能调哪些模型"。
const SOURCE: &str = "https://models.dev/api.json?type=all";

/// 缓存多久算新鲜。
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 一个模型的公开元数据。
///
/// 只留界面上真要显示的字段：上下文长度、价格、能力标记。
/// 上游字段有二十多个，全量映射等于把别人的 schema 抄进来。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// 调用时使用的模型名。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 上下文窗口（token）。
    pub context: Option<u32>,
    /// 单次输出上限（token）。
    pub output: Option<u32>,
    /// 每百万输入 token 的美元价。
    pub cost_in: Option<f64>,
    /// 每百万输出 token 的美元价。
    pub cost_out: Option<f64>,
    /// 是否有推理能力。
    pub reasoning: bool,
    /// 是否支持工具调用。
    pub tool_call: bool,
    /// 是否支持图片输入。
    pub vision: bool,
    /// 知识截止日期（`YYYY-MM`）。
    pub knowledge: Option<String>,
}

impl ModelInfo {
    /// 一行摘要：`200k 上下文 · $0.15/$0.60 · 工具`。
    pub fn summary(&self) -> String {
        let mut bits = Vec::new();
        if let Some(c) = self.context {
            bits.push(format!("{} 上下文", short_tokens(c)));
        }
        if let Some(o) = self.output {
            bits.push(format!("{} 输出", short_tokens(o)));
        }
        match (self.cost_in, self.cost_out) {
            (Some(i), Some(o)) => bits.push(format!("${i}/${o}")),
            (Some(i), None) => bits.push(format!("${i} 输入")),
            _ => {}
        }
        if self.tool_call {
            bits.push("工具".into());
        }
        if self.reasoning {
            bits.push("推理".into());
        }
        if self.vision {
            bits.push("视觉".into());
        }
        bits.join(" · ")
    }
}

/// 把 token 数压成人读的写法：1048576 → `1M`，200000 → `200k`。
fn short_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        // 1024*1024 这类值按 1M 显示，别写成 1.0M
        let m = n as f64 / 1_000_000.0;
        if (m - m.round()).abs() < 0.05 {
            format!("{}M", m.round() as u64)
        } else {
            format!("{m:.1}M")
        }
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// 一个 provider 及其模型清单。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// 目录里的 provider id，也是配置里的 provider 名。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// API 基础地址。上游有 26 个 provider 不填（SDK 内置），这里是 `None`。
    pub api: Option<String>,
    /// 密钥环境变量名，取第一个。
    pub env: Vec<String>,
    /// 上游 SDK 包名，用来推断协议。
    pub npm: String,
    /// 文档地址。
    pub doc: Option<String>,
    /// 模型清单。
    pub models: Vec<ModelInfo>,
}

impl ProviderInfo {
    /// 建议的协议：`anthropic` 或 `openai-compat`。
    pub fn kind(&self) -> &'static str {
        if self.npm.contains("anthropic") {
            "anthropic"
        } else {
            "openai-compat"
        }
    }

    /// 建议的 base_url：上游给的优先，没有就查内置兜底表。
    pub fn base_url(&self) -> String {
        self.api
            .clone()
            .or_else(|| default_base_url(&self.id).map(str::to_string))
            .unwrap_or_default()
    }

    /// 是不是我们的协议层能直接对接的。
    ///
    /// 上游两百多个 provider 里大多数走自己的原生 SDK（Bedrock、Vertex 之类），
    /// 用 OpenAI 兼容层去调它们的 `/chat/completions` 是不通的。
    /// 这里只做**提示**，不禁用 —— 用户自建网关经常长得像但不在名录里。
    pub fn natively_supported(&self) -> bool {
        matches!(
            self.npm.as_str(),
            "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@ai-sdk/anthropic"
        )
    }
}

/// 少数 provider 上游不给 `api`（SDK 内置了端点），但我们必须显式写。
fn default_base_url(id: &str) -> Option<&'static str> {
    match id {
        "anthropic" => Some("https://api.anthropic.com/v1"),
        "openai" => Some("https://api.openai.com/v1"),
        "google" => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        "groq" => Some("https://api.groq.com/openai/v1"),
        "cerebras" => Some("https://api.cerebras.ai/v1"),
        "mistral" => Some("https://api.mistral.ai/v1"),
        "xai" => Some("https://api.x.ai/v1"),
        "deepseek" => Some("https://api.deepseek.com"),
        "moonshotai" => Some("https://api.moonshot.cn/v1"),
        "zai" => Some("https://api.z.ai/api/paas/v4"),
        _ => None,
    }
}

/// 整个目录。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// 按 id 升序，界面直接按顺序展示。
    pub providers: Vec<ProviderInfo>,
}

impl Catalog {
    /// 解析上游 JSON。
    ///
    /// 容错优先：单个字段缺失或类型不对只让那个字段变 `None`，
    /// 不整条丢弃 —— 上游偶尔会改字段类型，为此丢掉整个 provider 不值得。
    pub fn parse(json: &str) -> Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| Error::Config(format!("models.dev 返回的不是合法 JSON: {e}")))?;
        let obj = raw
            .as_object()
            .ok_or_else(|| Error::Config("models.dev 顶层不是对象".into()))?;

        let mut providers = Vec::with_capacity(obj.len());
        for (id, p) in obj {
            let Some(p) = p.as_object() else { continue };
            // 没有模型的 provider 在界面上是死条目，直接跳过
            let Some(models_obj) = p.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            let models: Vec<ModelInfo> = models_obj
                .iter()
                .filter_map(|(mid, m)| parse_model(mid, m))
                .collect();
            if models.is_empty() {
                continue;
            }
            providers.push(ProviderInfo {
                id: id.clone(),
                name: str_field(p, "name").unwrap_or_else(|| id.clone()),
                api: str_field(p, "api"),
                env: p
                    .get("env")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                npm: str_field(p, "npm").unwrap_or_default(),
                doc: str_field(p, "doc"),
                models,
            });
        }
        providers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Catalog { providers })
    }

    /// 按 id 查 provider。
    pub fn find(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// 按 id / 展示名搜 provider。
    pub fn search(&self, q: &str) -> Vec<&ProviderInfo> {
        let q = q.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.providers.iter().collect();
        }
        self.providers
            .iter()
            .filter(|p| {
                p.id.to_ascii_lowercase().contains(&q) || p.name.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    /// 按模型名搜 provider。
    ///
    /// 与 [`Catalog::search`] 分开是有意的：聚合网关（302.AI 之类）
    /// 自带上百个模型，混在一起搜的话搜 "deepseek" 会先出来一堆
    /// 网关 —— 用户要找的通常是官方那家。先搜名字，搜不到再看这个。
    pub fn search_by_model(&self, q: &str) -> Vec<&ProviderInfo> {
        let q = q.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.providers
            .iter()
            .filter(|p| {
                p.models
                    .iter()
                    .any(|m| m.id.to_ascii_lowercase().contains(&q))
            })
            .collect()
    }
}

/// 解析单个模型。任何一项坏掉就退化成 `None`，不影响其它模型。
fn parse_model(id: &str, m: &serde_json::Value) -> Option<ModelInfo> {
    let obj = m.as_object()?;
    // 过滤非对话模型：embedding / rerank / 语音这些调不通 chat 接口，
    // 列出来只会让人点了报错
    let lower = id.to_ascii_lowercase();
    const NON_CHAT: [&str; 5] = ["embed", "rerank", "whisper", "tts", "moderation"];
    if NON_CHAT.iter().any(|k| lower.contains(k)) {
        return None;
    }
    let output_modalities = obj
        .get("modalities")
        .and_then(|v| v.get("output"))
        .and_then(|v| v.as_array());
    if let Some(mods) = output_modalities {
        let has_text = mods.iter().any(|x| x.as_str() == Some("text"));
        if !has_text {
            return None;
        }
    }
    let vision = obj
        .get("modalities")
        .and_then(|v| v.get("input"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().any(|x| x.as_str() == Some("image")))
        .unwrap_or(false);
    let limit = obj.get("limit");
    let cost = obj.get("cost");

    Some(ModelInfo {
        id: id.to_string(),
        name: str_field(obj, "name").unwrap_or_else(|| id.to_string()),
        context: limit
            .and_then(|l| l.get("context"))
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32),
        output: limit
            .and_then(|l| l.get("output"))
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32),
        cost_in: cost
            .and_then(|c| c.get("input"))
            .and_then(serde_json::Value::as_f64),
        cost_out: cost
            .and_then(|c| c.get("output"))
            .and_then(serde_json::Value::as_f64),
        reasoning: bool_field(obj, "reasoning"),
        tool_call: bool_field(obj, "tool_call"),
        vision,
        knowledge: str_field(obj, "knowledge"),
    })
}

fn str_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn bool_field(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> bool {
    obj.get(key).and_then(serde_json::Value::as_bool).unwrap_or(false)
}

/// 缓存文件位置：`~/.zex/cache/models-dev.json`。
pub fn cache_path() -> PathBuf {
    crate::config_dir().join("cache").join("models-dev.json")
}

/// 读缓存。返回 `(catalog, 是否已过期)`。
///
/// 过期也照返：界面上有旧目录总比空白好，随后后台刷新即可。
pub fn load_cache() -> Option<(Catalog, bool)> {
    let path = cache_path();
    let meta = std::fs::metadata(&path).ok()?;
    let fresh = meta
        .modified()
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| age < TTL)
        .unwrap_or(false);
    let text = std::fs::read_to_string(&path).ok()?;
    let catalog: Catalog = serde_json::from_str(&text).ok()?;
    if catalog.providers.is_empty() {
        return None;
    }
    Some((catalog, !fresh))
}

/// 写缓存。失败只记日志 —— 缓存写不进去不该让功能不可用。
pub fn save_cache(catalog: &Catalog) {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    if let Ok(text) = serde_json::to_string(catalog) {
        let _ = std::fs::write(&path, text);
    }
}

/// 从上游拉取并解析。
pub async fn fetch_remote() -> Result<Catalog> {
    let resp = reqwest::Client::new()
        .get(SOURCE)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| Error::Other(format!("连不上 models.dev: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Other(format!(
            "models.dev 返回 {}",
            resp.status().as_u16()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Other(format!("读取 models.dev 响应失败: {e}")))?;
    Catalog::parse(&text)
}

/// 同步版拉取：给调用方在自己的线程里直接跑。
///
/// UI 线程上没有 tokio reactor，而 gpui 的 executor 也不跑 IO ——
/// 所以这里自带一个 current_thread runtime，调用方只管扔到后台线程去。
pub fn fetch_blocking() -> Result<Catalog> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Other(format!("无法创建网络运行时: {e}")))?;
    rt.block_on(fetch_remote())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "deepseek": {
        "id": "deepseek", "name": "DeepSeek",
        "api": "https://api.deepseek.com", "env": ["DEEPSEEK_API_KEY"],
        "npm": "@ai-sdk/openai-compatible", "doc": "https://docs.deepseek.com",
        "models": {
          "deepseek-flash": {
            "id": "deepseek-flash", "name": "DeepSeek Flash",
            "reasoning": true, "tool_call": true,
            "modalities": {"input": ["text","image"], "output": ["text"]},
            "limit": {"context": 1048576, "output": 65536},
            "cost": {"input": 0.15, "output": 0.6}
          },
          "text-embedding-3": {
            "id": "text-embedding-3", "name": "Embedding",
            "modalities": {"input": ["text"], "output": ["text"]}
          }
        }
      },
      "anthropic": {
        "id": "anthropic", "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"],
        "npm": "@ai-sdk/anthropic",
        "models": {
          "claude-sonnet-5": {"id": "claude-sonnet-5", "name": "Claude Sonnet 5"}
        }
      },
      "emptyprov": {"id": "emptyprov", "name": "Empty", "models": {}}
    }"#;

    #[test]
    fn parses_providers_and_models() {
        let c = Catalog::parse(SAMPLE).unwrap();
        // 没有模型的 provider 不进来
        assert_eq!(c.providers.len(), 2);
        let d = c.find("deepseek").unwrap();
        assert_eq!(d.name, "DeepSeek");
        assert_eq!(d.api.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(d.env, vec!["DEEPSEEK_API_KEY".to_string()]);
    }

    #[test]
    fn filters_non_chat_models() {
        let c = Catalog::parse(SAMPLE).unwrap();
        let d = c.find("deepseek").unwrap();
        // embedding 不该出现在候选里
        assert_eq!(d.models.len(), 1);
        assert_eq!(d.models[0].id, "deepseek-flash");
    }

    #[test]
    fn maps_limit_and_cost() {
        let c = Catalog::parse(SAMPLE).unwrap();
        let m = &c.find("deepseek").unwrap().models[0];
        assert_eq!(m.context, Some(1_048_576));
        assert_eq!(m.output, Some(65_536));
        assert_eq!(m.cost_in, Some(0.15));
        assert!(m.reasoning && m.tool_call && m.vision);
    }

    #[test]
    fn infers_kind_from_npm() {
        let c = Catalog::parse(SAMPLE).unwrap();
        assert_eq!(c.find("deepseek").unwrap().kind(), "openai-compat");
        assert_eq!(c.find("anthropic").unwrap().kind(), "anthropic");
    }

    #[test]
    fn base_url_falls_back_when_api_missing() {
        let c = Catalog::parse(SAMPLE).unwrap();
        // anthropic 在上游没有 api 字段
        assert!(c.find("anthropic").unwrap().api.is_none());
        assert_eq!(
            c.find("anthropic").unwrap().base_url(),
            "https://api.anthropic.com/v1"
        );
        // 上游给了就用上游的
        assert_eq!(
            c.find("deepseek").unwrap().base_url(),
            "https://api.deepseek.com"
        );
    }

    #[test]
    fn search_matches_provider_name_only() {
        let c = Catalog::parse(SAMPLE).unwrap();
        // 按 provider id
        assert_eq!(c.search("deep").len(), 1);
        // 空串 = 全量
        assert_eq!(c.search("  ").len(), 2);
        // 模型名不走 search：它属于 search_by_model
        assert!(c.search("claude-sonnet").is_empty());
    }

    #[test]
    fn search_by_model_finds_owner() {
        let c = Catalog::parse(SAMPLE).unwrap();
        let hit = c.search_by_model("claude-sonnet");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].id, "anthropic");
        // 空串不该把全家都倒出来
        assert!(c.search_by_model("  ").is_empty());
    }

    #[test]
    fn short_tokens_is_readable() {
        assert_eq!(short_tokens(1_048_576), "1M");
        assert_eq!(short_tokens(200_000), "200k");
        assert_eq!(short_tokens(512), "512");
        assert_eq!(short_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn summary_skips_missing_fields() {
        let m = ModelInfo {
            context: Some(200_000),
            tool_call: true,
            ..Default::default()
        };
        let s = m.summary();
        assert!(s.contains("200k"));
        assert!(s.contains("工具"));
        // 没有价格就不该冒出 `$` 之类的空壳
        assert!(!s.contains('$'));
    }

    #[test]
    fn rejects_non_object_root() {
        assert!(Catalog::parse("[1,2,3]").is_err());
        assert!(Catalog::parse("not json").is_err());
    }

    #[test]
    fn roundtrips_through_serde() {
        let c = Catalog::parse(SAMPLE).unwrap();
        let text = serde_json::to_string(&c).unwrap();
        let back: Catalog = serde_json::from_str(&text).unwrap();
        assert_eq!(c, back);
    }
}
