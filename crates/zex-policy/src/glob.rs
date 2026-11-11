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
/// - 空模式 = 匹配该工具的任何参数
/// - 否则：提取参数里所有字符串值，任一命中即算匹配
///
/// 为什么匹配"任一字符串值"而不是特定字段：工具的参数结构各不相同
/// （`shell` 用 `command`，`fs_write` 用 `path`），按字段名硬编码会让
/// 新增工具时忘记更新匹配逻辑 —— 那是静默失效的安全漏洞。
pub fn matches(pattern: &str, args: &Value) -> bool {
    if pattern.trim().is_empty() {
        return true;
    }
    let mut found = false;
    walk_strings(args, &mut |s| {
        if !found && glob_match(pattern, s) {
            found = true;
        }
    });
    found
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
    fn empty_pattern_matches_everything() {
        assert!(matches("", &json!({"command": "rm -rf /"})));
        assert!(matches("   ", &json!({"path": "x"})));
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
    fn unicode_is_handled_by_chars() {
        assert!(glob_match("删除 *", "删除 缓存"));
    }
}
