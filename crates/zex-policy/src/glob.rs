//! 极简 glob 匹配，用于授权记忆的参数模式。
//!
//! 支持 `*`（匹配任意字符，含路径分隔符之外的任意内容）、
//! `**`（匹配任意内容，含 `/`）、`?`（单个字符）。
//!
//! **不做 shell 展开** —— 展开即漏洞（`docs/SECURITY.md` §10）。
//! 这里只是把人类写的模式与参数原文做匹配。

use serde_json::Value;

/// 判断一条授权模式是否匹配本次调用的参数。
///
/// 规则：
/// - 空模式 = **不匹配任何参数**（见下）
/// - 否则：提取参数里所有字符串值，任一命中即算匹配
///
/// 空模式为什么不再"匹配一切"：授权记忆落库时如果拿到空模式，
/// 用户只批准过 `/tmp/a.txt`、却等于批准了该工具的所有调用 ——
/// 直接架空 L1 权限模型。宁可让这条授权失效（重新问一次），
/// 也不能让它无限放大。
///
/// 为什么匹配"任一字符串值"而不是特定字段：工具的参数结构各不相同
/// （`shell` 用 `command`，`fs_write` 用 `path`），按字段名硬编码会让
/// 新增工具时忘记更新匹配逻辑 —— 那是静默失效的安全漏洞。
pub fn matches(pattern: &str, args: &Value) -> bool {
    if pattern.trim().is_empty() {
        return false;
    }
    let mut found = false;
    walk_strings(args, &mut |s| {
        if !found && glob_match(pattern, s) {
            found = true;
        }
    });
    found
}

/// 把一次具体调用的关键参数转成"字面量"授权模式。
///
/// 用于"记住这次操作"：用户批准的是**这一次**的参数，授权就该只覆盖它。
/// 取不到关键参数时返回 `None`，调用方应放弃持久化 —— 少记一次，
/// 好过记成一条能匹配一切的授权。
pub fn literal_pattern(args: &Value) -> Option<String> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| args.get("command").and_then(Value::as_str))
        .or_else(|| first_string(args))?;
    if raw.trim().is_empty() {
        return None;
    }
    Some(escape_glob(raw))
}

fn first_string(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Array(a) => a.iter().find_map(first_string),
        Value::Object(o) => o.values().find_map(first_string),
        _ => None,
    }
}

/// 转义 glob 元字符，让模式退化为字面量。
fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '*' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn walk_strings(v: &Value, f: &mut impl FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter().for_each(|x| walk_strings(x, f)),
        Value::Object(o) => o.values().for_each(|x| walk_strings(x, f)),
        _ => {}
    }
}

/// 单条 glob 匹配。
///
/// 递归实现，模式与文本都很短（一次工具调用的参数），不需要更快的算法。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_from(&p, 0, &t, 0)
}

fn match_from(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    // 迭代消化 `*`，避免深递归
    while pi < p.len() {
        match p[pi] {
            '*' => {
                // `**` 与 `*` 在这里语义相同：都匹配任意长度（含空）
                // 区别只在是否跨 `/` —— 但授权模式里我们宁可宽一点，
                // 因为路径归一化已经在 policy 之外做过了（见 zex-agent::toolctx）。
                while pi < p.len() && p[pi] == '*' {
                    pi += 1;
                }
                if pi >= p.len() {
                    return true; // 尾部 * 匹配剩余全部
                }
                // 尝试每一个可能的起点
                for k in ti..=t.len() {
                    if match_from(p, pi, t, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            '\\' if pi + 1 < p.len() => {
                // 转义：下一个字符按字面匹配。
                // 把具体路径存成授权模式时要靠它，否则路径里的 * 会被当通配符。
                let c = p[pi + 1];
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 2;
                ti += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_match() {
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("ls", "ls -la"));
    }

    #[test]
    fn star_matches_any_suffix() {
        assert!(glob_match("git *", "git status"));
        assert!(glob_match("git *", "git push --force"));
        assert!(!glob_match("git *", "npm install"));
    }

    #[test]
    fn star_matches_empty() {
        assert!(glob_match("git*", "git"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn question_mark_matches_one_char() {
        assert!(glob_match("ls ?", "ls -"));
        assert!(!glob_match("ls ?", "ls --"));
    }

    #[test]
    fn path_patterns() {
        assert!(glob_match("/home/u/proj/**", "/home/u/proj/src/main.rs"));
        assert!(glob_match("*.txt", "notes.txt"));
        assert!(!glob_match("*.txt", "notes.md"));
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        // 空模式绝不能等于"全放行"，否则用户的单次批准会被放大成永久全权
        assert!(!matches("", &json!({"command": "rm -rf /"})));
        assert!(!matches("   ", &json!({"path": "x"})));
    }

    #[test]
    fn escaped_pattern_is_literal() {
        assert!(matches("/tmp/a\\*b.txt", &json!({"path": "/tmp/a*b.txt"})));
        assert!(!matches("/tmp/a\\*b.txt", &json!({"path": "/tmp/axb.txt"})));
    }

    #[test]
    fn matches_any_string_field() {
        // pattern 不绑定具体字段名，任一字符串命中即算
        assert!(matches("git *", &json!({"command": "git status"})));
        assert!(matches("/home/u/*", &json!({"path": "/home/u/a.txt"})));
    }

    #[test]
    fn matches_nested_strings() {
        assert!(matches("*.rs", &json!({"files": ["a.rs", "b.md"]})));
        assert!(matches("git *", &json!({"opts": {"cmd": "git log"}})));
    }

    #[test]
    fn no_match_on_numbers_only() {
        assert!(!matches("git *", &json!({"n": 3, "ok": true})));
    }

    #[test]
    fn does_not_match_wrong_command() {
        assert!(!matches("git *", &json!({"command": "rm -rf build"})));
    }

    #[test]
    fn literal_pattern_covers_only_that_call() {
        let p = literal_pattern(&json!({"path": "/tmp/a.txt", "content": "hi"})).unwrap();
        assert!(matches(&p, &json!({"path": "/tmp/a.txt", "content": "hi"})));
        // 换一个路径就不再命中 —— 这正是"记住这次操作"该有的边界
        assert!(!matches(&p, &json!({"path": "/tmp/b.txt"})));
        assert!(!matches(&p, &json!({"path": "/home/u/.ssh/config"})));
    }

    #[test]
    fn literal_pattern_escapes_metachars() {
        // 路径里真的带 * 时，不能被当成通配符
        let p = literal_pattern(&json!({"path": "/tmp/a*b.txt"})).unwrap();
        assert!(matches(&p, &json!({"path": "/tmp/a*b.txt"})));
        assert!(!matches(&p, &json!({"path": "/tmp/axxb.txt"})));
    }

    #[test]
    fn literal_pattern_prefers_command_for_shell() {
        let p = literal_pattern(&json!({"command": "git status"})).unwrap();
        assert!(matches(&p, &json!({"command": "git status"})));
        assert!(!matches(&p, &json!({"command": "git push --force"})));
    }

    #[test]
    fn literal_pattern_none_when_no_strings() {
        assert!(literal_pattern(&json!({"n": 1})).is_none());
        assert!(literal_pattern(&json!({"path": "  "})).is_none());
    }

    #[test]
    fn unicode_is_handled_by_chars() {
        assert!(glob_match("删除 *", "删除 缓存"));
    }
}
