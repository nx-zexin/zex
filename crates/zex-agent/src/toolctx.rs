//! 内核侧的 [`zex_core::tool::ToolCtx`] 实现。
//!
//! 路径解析在这里统一做，**这是安全边界的一部分**：
//! grant 匹配要求路径已归一化，否则 `../` 和符号链接可以绕过授权
//! （见 `docs/SECURITY.md` §10）。

use std::path::{Component, Path, PathBuf};

use zex_core::tool::ToolCtx;
use zex_core::{Result, SessionId};

/// 内核侧的上下文实现。
#[derive(Debug, Clone)]
pub struct AgentCtx {
    cwd: PathBuf,
    session: SessionId,
}

impl AgentCtx {
    /// 新建。
    pub fn new(cwd: PathBuf, session: SessionId) -> Self {
        AgentCtx { cwd, session }
    }
}

impl ToolCtx for AgentCtx {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn session_id(&self) -> &SessionId {
        &self.session
    }

    fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
        Ok(resolve_path(&self.cwd, raw))
    }

    fn is_secret_path(&self, path: &Path) -> bool {
        is_secret(path)
    }
}

/// 把用户/模型给的路径解析成归一化的绝对路径。
///
/// 规则（顺序重要）：
/// 1. `~` / `$HOME` / `${HOME}` 展开
/// 2. 相对路径基于 cwd
/// 3. 消解 `.` 与 `..`（词法层面，不要求文件存在）
/// 4. 若路径存在，再做一次 canonicalize（解符号链接）
pub fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    let expanded = expand_home(raw.trim());
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };

    // 词法归一化：即使文件不存在也能消解 `..`
    let lexical = normalize(&joined);

    // 存在的话再解符号链接，防止 `link -> /etc` 绕过
    if let Ok(canonical) = std::fs::canonicalize(&lexical) {
        return canonical;
    }
    // 不存在时，尝试对父目录做 canonicalize（新文件场景很常见）
    if let Some(parent) = lexical.parent() {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            if let Some(name) = lexical.file_name() {
                return canonical_parent.join(name);
            }
        }
    }
    lexical
}

/// 展开 `~`、`$HOME`、`${HOME}` 前缀。
///
/// 只处理开头的家目录形式。不处理 `$OTHER_VAR` —— 那需要 shell 展开语义，
/// 而**做 shell 展开本身就是漏洞**（见 SECURITY.md §10）。
fn expand_home(raw: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);

    let Some(home) = home else {
        return PathBuf::from(raw);
    };

    for prefix in ["${HOME}", "$HOME"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            return if rest.is_empty() {
                home
            } else {
                home.join(rest)
            };
        }
    }
    if raw == "~" {
        return home;
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// 纯词法的路径归一化：消解 `.` 与 `..`，不触碰文件系统。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // 到根就停住，不要把 `/..` 变成空
                if !out.pop() {
                    out.push(Component::RootDir);
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// 是否命中密钥黑名单。
///
/// 名单短而硬：这些文件一旦进模型上下文就等于泄露。
pub fn is_secret(path: &Path) -> bool {
    const NAMES: &[&str] = &[
        ".env",
        ".netrc",
        ".pgpass",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        ".npmrc",
        ".pypirc",
        "credentials",
    ];
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = name.to_ascii_lowercase();
        if NAMES.iter().any(|n| lower == *n) {
            return true;
        }
        // .env 的各种变体：.env.local / .env.production / .env.test ...
        // 它们和 .env 一样装着密钥，只认精确名字等于没防
        if lower == ".env" || lower.starts_with(".env.") {
            return true;
        }
        // 各类私钥：id_rsa 之外还有 id_ed25519 / id_dsa / id_ecdsa_sk ...
        if lower.starts_with("id_") {
            return true;
        }
        if lower == ".git-credentials" {
            return true;
        }
        // 私钥文件常见后缀
        if lower.ends_with(".pem")
            || lower.ends_with(".key")
            || lower.ends_with(".p12")
            || lower.ends_with(".pfx")
            || lower.ends_with(".keystore")
        {
            return true;
        }
    }
    // Zex 自己的密钥与策略文件：能读它就能冒充用户
    if let Some(s) = path.to_str() {
        let s = s.replace('\\', "/");
        if s.contains("/.zex/.env") || s.contains("/.zex/config.json") {
            return true;
        }
        // 各家 CLI 存放长期凭据的位置
        for marker in [
            "/.aws/",
            "/.gnupg/",
            "/.kube/config",
            "/.docker/config.json",
            "/.config/gh/hosts.yml",
            "/.ssh/",
        ] {
            if s.contains(marker) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_joins_cwd() {
        let p = resolve_path(Path::new("/home/u/proj"), "a.txt");
        assert_eq!(p, PathBuf::from("/home/u/proj/a.txt"));
    }

    #[test]
    fn absolute_path_is_kept() {
        let p = resolve_path(Path::new("/home/u"), "/etc/hosts");
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn parent_dir_is_resolved() {
        // 这是授权绕过的经典手法，必须消解掉
        let p = resolve_path(Path::new("/home/u/proj/sub"), "../../etc/passwd");
        assert_eq!(p, PathBuf::from("/home/u/etc/passwd"));
    }

    #[test]
    fn parent_dir_cannot_escape_root() {
        let p = resolve_path(Path::new("/"), "../../..");
        assert_eq!(p, PathBuf::from("/"));
    }

    #[test]
    fn cur_dir_is_elided() {
        let p = resolve_path(Path::new("/home/u"), "./a/./b");
        assert_eq!(p, PathBuf::from("/home/u/a/b"));
    }

    #[test]
    fn home_prefix_is_expanded() {
        let p = resolve_path(Path::new("/tmp"), "~/notes.txt");
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(p.starts_with(&home), "期望在 HOME 下，实际 {p:?}");
        }
    }

    #[test]
    fn home_var_forms_are_expanded() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        for form in ["$HOME/x.txt", "${HOME}/x.txt"] {
            let p = resolve_path(Path::new("/tmp"), form);
            assert!(p.starts_with(&home), "{form} 未展开: {p:?}");
        }
    }

    #[test]
    fn other_vars_are_not_expanded() {
        // 不做 shell 展开：展开就是漏洞
        let p = resolve_path(Path::new("/tmp"), "$EVIL/x");
        assert!(p.to_string_lossy().contains("$EVIL"));
    }

    #[test]
    fn secret_files_are_detected() {
        for name in [".env", "id_rsa", ".npmrc", "server.pem", "a.key"] {
            assert!(
                is_secret(Path::new("/home/u").join(name).as_path()),
                "{name} 应命中"
            );
        }
    }

    #[test]
    fn zex_own_config_is_secret() {
        assert!(is_secret(Path::new("/home/u/.zex/.env")));
        assert!(is_secret(Path::new("/home/u/.zex/config.json")));
    }

    #[test]
    fn normal_files_are_not_secret() {
        for name in ["main.rs", "README.md", "env.sample", ".envrc"] {
            assert!(
                !is_secret(Path::new("/home/u").join(name).as_path()),
                "{name} 不应命中"
            );
        }
    }
}
