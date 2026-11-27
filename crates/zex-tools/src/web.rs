//! 网页抓取工具。
//!
//! 只做"取回并转成可读文本"，不做浏览器自动化 —— 需要点击、登录、JS 渲染
//! 的场景属于 `zex-browser`（CDP），见 ADR-003。

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 抓取内容的字节上限。
pub const MAX_BYTES: usize = 2 * 1024 * 1024;

/// 默认超时。
pub const TIMEOUT_SECS: u64 = 30;

/// 返回给模型的字符上限。
pub const MAX_CHARS: usize = 100_000;

/// `web_fetch` 的参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchArgs {
    /// 完整 URL，必须包含 http:// 或 https://。
    pub url: String,
    /// 是否把 HTML 转成纯文本。默认 true；设为 false 可拿到原始 HTML（用于解析结构化数据）。
    #[serde(default)]
    pub raw: Option<bool>,
}

/// 抓取网页。
pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    const NAME: &'static str = "web_fetch";
    const HINT: Option<&'static str> = Some(
        "抓已知 URL 的正文用它；不要用 shell 的 curl/wget，那回来的是 HTML 源码，还得再洗一遍",
    );
    const RISK: Risk = Risk::ReadOnly;
    type Args = FetchArgs;

    fn description() -> &'static str {
        "抓取一个网页或 HTTP API，返回可读文本。只做静态抓取：\
         不执行 JavaScript、不保持登录态、不点击按钮。\
         需要你给出确切 URL；需要登录或点击才能拿到的内容，\
         做不到，请让用户把内容复制过来或另想办法。"
    }

    async fn call(&self, _cx: &dyn ToolCtx, args: FetchArgs) -> Result<ToolOutput> {
        if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
            return Err(Error::BadArgs {
                tool: Self::NAME.into(),
                message: "url 必须以 http:// 或 https:// 开头".into(),
            });
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .user_agent("Mozilla/5.0 (compatible; Zex/0.1)")
            .build()
            .map_err(|e| Error::Other(format!("无法创建 HTTP 客户端: {e}")))?;

        let resp = client
            .get(&args.url)
            .send()
            .await
            .map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("请求失败: {e}"),
            })?;

        let status = resp.status();
        if !status.is_success() {
            return Ok(ToolOutput::err(format!("HTTP {status}（{}）", args.url)));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = resp.bytes().await.map_err(|e| Error::ToolFailed {
            tool: Self::NAME.into(),
            message: format!("读取响应失败: {e}"),
        })?;

        if bytes.len() > MAX_BYTES {
            return Ok(ToolOutput::err(format!(
                "响应体 {} 字节，超过 {} 字节上限。请换用更具体的 URL 或 API 参数。",
                bytes.len(),
                MAX_BYTES
            )));
        }

        let body = String::from_utf8_lossy(&bytes).into_owned();
        let is_html = content_type.contains("html")
            || body.trim_start().starts_with("<!DOCTYPE")
            || body.trim_start().starts_with("<html");

        let use_raw = args.raw.unwrap_or(false);
        let text = if is_html && !use_raw {
            html_to_text(&body)
        } else {
            body
        };

        let truncated = text.chars().count() > MAX_CHARS;
        let text: String = if truncated {
            text.chars().take(MAX_CHARS).collect()
        } else {
            text
        };

        let mut out = format!(
            "URL: {}\nContent-Type: {}\n\n{text}",
            args.url, content_type
        );
        if truncated {
            out.push_str(&format!("\n\n[内容超过 {MAX_CHARS} 字符，已截断]"));
        }

        Ok(ToolOutput::ok(out).with_meta(serde_json::json!({
            "url": args.url,
            "status": status.as_u16(),
            "bytes": bytes.len(),
            "html": is_html,
        })))
    }
}

/// 把 HTML 粗转成纯文本。
///
/// 刻意保持简单：目的是让模型能读，不是给人类看的排版。
/// 真正需要结构化提取时应传 `raw: true` 由模型自己解析。
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes: Vec<char> = html.chars().collect();
    let mut i = 0usize;
    let n = bytes.len();

    while i < n {
        let c = bytes[i];
        if c == '<' {
            // 找到标签结束
            let mut j = i + 1;
            while j < n && bytes[j] != '>' {
                j += 1;
            }
            let tag: String = bytes[i + 1..j.min(n)].iter().collect();
            let tag_lower = tag.to_ascii_lowercase();
            let tag_name = tag_lower
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string();

            // script / style 整段丢弃
            if (tag_name == "script" || tag_name == "style") && !tag_lower.starts_with('/') {
                let close = format!("</{tag_name}");
                let rest: String = bytes[j.min(n)..].iter().collect();
                if let Some(pos) = rest.to_ascii_lowercase().find(&close) {
                    let skip_chars = rest[..pos].chars().count();
                    i = j.min(n) + skip_chars;
                    i = skip_chars_and_find_gt(&bytes, i);
                    continue;
                }
                break;
            }

            // 块级标签换成换行。
            // 闭合标签不加：`<div>a</div><div>b</div>` 只应产生一个换行，
            // 否则每个块会多出一个空行。
            let is_closing = tag_lower.starts_with('/');
            if !is_closing
                && matches!(
                    tag_name.as_str(),
                    "p" | "div"
                        | "br"
                        | "li"
                        | "tr"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "blockquote"
                        | "pre"
                        | "table"
                )
            {
                out.push('\n');
            }
            i = j + 1;
            continue;
        }

        if c == '&' {
            // 实体解码
            let mut j = i + 1;
            while j < n && j - i < 10 && bytes[j] != ';' {
                j += 1;
            }
            if j < n && bytes[j] == ';' {
                let ent: String = bytes[i + 1..j].iter().collect();
                if let Some(decoded) = decode_entity(&ent) {
                    out.push_str(decoded);
                    i = j + 1;
                    continue;
                }
            }
            out.push('&');
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }

    // 压缩连续空行
    let mut result = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        result.push_str(trimmed);
        result.push('\n');
    }
    result.trim().to_string()
}

/// 从 `i` 开始找到下一个 `>` 之后的位置。
fn skip_chars_and_find_gt(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '>' {
        i += 1;
    }
    (i + 1).min(chars.len())
}

fn decode_entity(ent: &str) -> Option<&'static str> {
    Some(match ent {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use zex_core::SessionId;

    struct Ctx;
    impl ToolCtx for Ctx {
        fn cwd(&self) -> &Path {
            Path::new("/tmp")
        }
        fn session_id(&self) -> &SessionId {
            static ID: std::sync::OnceLock<SessionId> = std::sync::OnceLock::new();
            ID.get_or_init(|| SessionId::new("ses_test"))
        }
        fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
            Ok(PathBuf::from(raw))
        }
    }

    #[test]
    fn strips_tags() {
        assert_eq!(html_to_text("<p>hello</p>"), "hello");
    }

    #[test]
    fn keeps_block_structure() {
        let t = html_to_text("<div>a</div><div>b</div>");
        assert_eq!(t, "a\nb");
    }

    #[test]
    fn drops_script_and_style() {
        let t = html_to_text("<p>x</p><script>var a=1;</script><style>.a{}</style><p>y</p>");
        assert!(!t.contains("var a"));
        assert!(!t.contains(".a{"));
        assert!(t.contains('x'));
        assert!(t.contains('y'));
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(html_to_text("a &amp; b &lt;c&gt;"), "a & b <c>");
    }

    #[test]
    fn unknown_entity_is_kept_literally() {
        assert!(html_to_text("&unknown;").contains("&unknown;"));
    }

    #[test]
    fn collapses_blank_lines() {
        let t = html_to_text("<p>a</p>\n\n\n\n<p>b</p>");
        assert!(!t.contains("\n\n\n"));
    }

    #[test]
    fn handles_chinese() {
        let t = html_to_text("<p>你好，世界</p>");
        assert_eq!(t, "你好，世界");
    }

    #[test]
    fn malformed_html_does_not_hang() {
        // 未闭合标签不应死循环
        let t = html_to_text("<p>unclosed <div>text");
        assert!(t.contains("unclosed"));
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let e = WebFetch
            .call(
                &Ctx,
                FetchArgs {
                    url: "file:///etc/passwd".into(),
                    raw: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(e, Error::BadArgs { .. }));
    }

    #[test]
    fn risk_is_read_only() {
        assert_eq!(WebFetch::RISK, Risk::ReadOnly);
    }
}
