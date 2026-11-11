//! 工具输出里的凭据脱敏。
//!
//! 这是密钥保护的**最后一道**防线。前两道（路径黑名单、命令检测）都是
//! 穷举，总有绕法：`env`、`cat ~/.aws/credentials`、`curl` 回来的响应体、
//! 甚至编译器报错里带的连接串。穷举挡不住的部分，靠这一层兜住 ——
//! 只要结果里出现凭据形态，进上下文之前就被抹掉。
//!
//! 取舍：宁可误伤（把长随机串当密钥抹掉），也不放过。模型拿不到真值，
//! 最多是多问一句；泄露了则无法挽回。

/// 脱敏后的占位符。
const MASK: &str = "[REDACTED]";

/// 变量名里出现这些词，就认为它的值是凭据。
const SENSITIVE_NAME_PARTS: &[&str] = &[
    "KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
];

/// 各 provider 凭据的固定前缀。
///
/// 之所以要前缀：只按"长随机串"判定会误伤 UUID、哈希、base64 内容；
/// 而真实凭据几乎都带可识别前缀。
const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk-ant-",      // Anthropic
    "sk-proj-",     // OpenAI 项目 key
    "sk-",          // OpenAI 及其兼容实现
    "AKIA",         // AWS access key
    "ASIA",         // AWS 临时凭据
    "ghp_",         // GitHub PAT
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",  // GitHub 细粒度 PAT
    "glpat-",       // GitLab
    "AIza",         // Google API key
    "xoxb-",        // Slack bot
    "xoxp-",        // Slack user
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "sk_live_",     // Stripe
    "sk_test_",
    "rk_live_",
    "hf_",          // Hugging Face
];

/// 前缀之后至少还要有多少个凭据字符，才认定为真凭据。
///
/// 门槛的作用：避免把 `sk-learn`、`hf_transfer` 这类正常词抹掉。
const MIN_CREDENTIAL_BODY: usize = 16;

/// 判断一个字符是否属于凭据字符集。
fn is_credential_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=')
}

/// 对一个字符串做脱敏。
///
/// 返回新字符串；没有命中时原样返回（不分配新内容以外的东西）。
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // PEM 私钥是跨行块，必须整体先处理，否则 base64 主体逐行看都像普通文本
    let text = redact_pem_blocks(text);

    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line));
    }
    out
}

/// 抹掉 PEM 私钥块。
///
/// 公钥（`BEGIN PUBLIC KEY`）保留 —— 它本来就是公开的，抹掉只会让
/// 调试 `ssh-keygen -y` 之类的输出变得没法看。
fn redact_pem_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_private = false;

    for line in text.split('\n') {
        let trimmed = line.trim();
        let is_begin = trimmed.starts_with("-----BEGIN") && trimmed.ends_with("-----");
        let is_end = trimmed.starts_with("-----END") && trimmed.ends_with("-----");

        if !in_private {
            if is_begin && trimmed.contains("PRIVATE KEY") {
                in_private = true;
                out.push_str("-----BEGIN PRIVATE KEY-----");
                out.push('\n');
                out.push_str(MASK);
                continue;
            }
            out.push_str(line);
            out.push('\n');
        } else if is_end && trimmed.contains("PRIVATE KEY") {
            in_private = false;
            out.push_str("-----END PRIVATE KEY-----");
        }
        // 块内其余行（base64 主体）直接丢弃
    }

    // 上面统一加了换行，去掉末尾多出来的那个，保持原文形状
    if out.ends_with('\n') && !text.ends_with('\n') {
        out.pop();
    }
    out
}

/// 逐行处理：先看是不是「敏感变量赋值」，再扫描凭据前缀。
fn redact_line(line: &str) -> String {
    if let Some(redacted) = redact_assignment(line) {
        return redacted;
    }
    redact_prefixes(line)
}

/// 处理 `NAME=value` / `NAME: value` / `--name value` 这类赋值。
///
/// 返回 `None` 表示这一行不是敏感赋值，交给前缀扫描处理。
fn redact_assignment(line: &str) -> Option<String> {
    // 找分隔符：优先 '='，其次 ':'（YAML/JSON 风格）
    let (name, rest, sep) = {
        let eq = line.find('=');
        let colon = line.find(':');
        match (eq, colon) {
            (Some(e), Some(c)) if e < c => (&line[..e], &line[e + 1..], '='),
            (Some(e), None) => (&line[..e], &line[e + 1..], '='),
            (_, Some(c)) => (&line[..c], &line[c + 1..], ':'),
            _ => return None,
        }
    };

    // 变量名要长得像个变量名（去掉前导空格与引号）
    let name = name.trim().trim_start_matches(['-', '"', '\'']);
    if name.is_empty() || name.len() > 80 {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '[' | ']' | ' '))
    {
        return None;
    }

    let upper = name.to_ascii_uppercase();
    if !SENSITIVE_NAME_PARTS.iter().any(|p| upper.contains(p)) {
        return None;
    }

    let value = rest.trim().trim_matches(['"', '\'', ',']);
    // 空值、已脱敏、明显是占位符的，不用再处理
    if value.is_empty()
        || value == MASK
        || value.eq_ignore_ascii_case("null")
        || value.eq_ignore_ascii_case("none")
        || value.starts_with("${")
    {
        return None;
    }

    // 太短的值大概率是开关（`DEBUG=true`、`TOKEN=1`），抹掉反而添乱；
    // 但既然后缀是 KEY/SECRET，长一点就按凭据处理。
    if value.chars().count() < 8 {
        return None;
    }

    Some(format!("{name}{sep}{MASK}"))
}

/// 扫描已知前缀，抹掉其后的凭据主体。
fn redact_prefixes(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;

    'outer: while i < chars.len() {
        // 前缀必须出现在词边界上，避免把 `task-...` 里的 `sk-` 当凭据
        let at_boundary = i == 0 || !chars[i - 1].is_ascii_alphanumeric();

        if at_boundary {
            let rest: String = chars[i..].iter().collect();
            for prefix in CREDENTIAL_PREFIXES {
                if !rest.starts_with(prefix) {
                    continue;
                }
                let body_start = i + prefix.chars().count();
                let mut end = body_start;
                while end < chars.len() && is_credential_char(chars[end]) {
                    end += 1;
                }
                if end - body_start >= MIN_CREDENTIAL_BODY {
                    out.push_str(prefix);
                    out.push_str(MASK);
                    i = end;
                    continue 'outer;
                }
            }
        }

        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 对一条历史消息就地脱敏。
///
/// 为什么历史也要处理：脱敏只作用于"新的工具输出"，可密钥可能是在
/// 防护上线之前就落库的。恢复会话时若不重扫一遍，等于每次启动都把
/// 旧明文重新喂给模型 —— 防线形同虚设。
pub fn redact_message(msg: &mut zex_core::Message) {
    use zex_core::{Message, Part};
    match msg {
        Message::System { content } => *content = redact(content),
        Message::User { content } | Message::Assistant { content, .. } => {
            for part in content.iter_mut() {
                if let Part::Text { text } = part {
                    *text = redact(text);
                }
            }
        }
        Message::Tool { content, .. } => *content = redact(content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_openai_key_in_env_assignment() {
        let s = "OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789";
        let r = redact(s);
        assert!(!r.contains("abcdefghij"), "凭据主体应被抹掉: {r}");
        assert!(r.contains("OPENAI_API_KEY="), "变量名要留着，否则看不出发生了什么");
        assert!(r.contains(MASK));
    }

    #[test]
    fn redacts_aws_credentials_file() {
        let s = "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let r = redact(s);
        assert!(!r.contains("wJalrXUtn"), "不应残留: {r}");
    }

    #[test]
    fn redacts_bare_key_by_prefix() {
        let r = redact("token is sk-ant-api03-abcdefghijklmnopqrstuvwxyz here");
        assert!(!r.contains("abcdefghij"), "{r}");
        assert!(r.contains("token is"));
    }

    #[test]
    fn keeps_ordinary_text() {
        for s in [
            "cargo build --release",
            "这是一个普通的中文句子",
            "fn main() { println!(\"hi\"); }",
            "sk-learn is a typo",   // 前缀后不够长，不该动
            "hf_transfer",            // 同上
        ] {
            assert_eq!(redact(s), s, "不该改动: {s}");
        }
    }

    #[test]
    fn keeps_short_flag_values() {
        // 太短的值多半是开关，不是凭据
        let s = "DEBUG=true\nTOKEN=1";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn keeps_placeholder_values() {
        let s = "API_KEY=${OPENAI_KEY}\nTOKEN=null";
        assert_eq!(redact(s), s, "占位符不该被抹: {s}");
    }

    #[test]
    fn handles_multiline_preserving_shape() {
        let s = "line1\nOPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx\nline3";
        let r = redact(s);
        assert_eq!(r.lines().count(), 3, "行数应保持");
        assert!(r.starts_with("line1\n"));
        assert!(r.ends_with("\nline3"));
    }

    #[test]
    fn redacts_github_and_google_and_slack() {
        let cases = [
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "AIzaSyAabcdefghijklmnopqrstuvwxyz012345",
            concat!("xoxb-", "123456789012-abcdefghijklmnop"),
        ];
        for c in cases {
            let r = redact(c);
            assert!(r.contains(MASK), "未识别: {c}");
            assert!(!r.contains("abcdefghij"), "残留: {r}");
        }
    }

    #[test]
    fn redacts_pem_private_key_block() {
        let s = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\nQUJDREVGR0hJSktMTU5PUA==\n-----END RSA PRIVATE KEY-----";
        let r = redact(s);
        assert!(!r.contains("MIIEowIBAA"), "私钥主体应被抹掉: {r}");
        assert!(r.contains(MASK));
        assert!(r.contains("PRIVATE KEY"), "应保留块标记，提示这里被处理过");
    }

    #[test]
    fn keeps_pem_public_key() {
        let s = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----";
        assert_eq!(redact(s), s, "公钥不该被抹");
    }

    #[test]
    fn redacts_openssh_private_key() {
        let s = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU\n-----END OPENSSH PRIVATE KEY-----";
        let r = redact(s);
        assert!(!r.contains("b3BlbnNzaC1rZXkt"), "{r}");
    }

    #[test]
    fn redacts_inside_history_message() {
        use zex_core::Message;
        let mut m = Message::Tool {
            call_id: zex_core::ToolCallId::from("c1".to_string()),
            name: Some("shell".into()),
            content: "ZEX_TEST_SECRET=sk-fake1234567890abcdefghijklmn".into(),
            is_error: false,
        };
        redact_message(&mut m);
        let text = m.text();
        assert!(!text.contains("fake1234567890"), "历史消息应被脱敏: {text}");
        assert!(text.contains(MASK));
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(redact(""), "");
    }
}
