//! 扩展的目录约定与清单解析。
//!
//! # 为什么是目录而不是打包格式
//!
//! 和 [`zex_skill`] 同一个理由：扩展是**人写的代码**。用户要能直接改一行、
//! 保存、看见效果；要能把目录扔进 git；要能打包成 zip 发给同事。
//! 塞进 SQLite 或自定义 tar 之后，这三件事都要额外做一遍。
//!
//! # 目录形状
//!
//! ```text
//! ~/.zex/extensions/git-status/
//! ├── gpui-shell.json     # id / name / entry，上游认的格式
//! └── main.js
//! ```
//!
//! `gpui-shell.json` 直接复用上游的清单格式，不自造一套：这样同一个目录
//! 既能被 zex 挂载，也能用 `gpui-shell dev` 单独跑起来调试。
//!
//! 上游的清单 `deny_unknown_fields`，所以不能往里加自定义字段。
//! 需要额外元数据时另开文件，别去污染它。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 清单文件名，与上游一致。
pub const MANIFEST: &str = "gpui-shell.json";

/// 缺省入口。
pub const DEFAULT_ENTRY: &str = "main.js";

/// 一个已安装扩展的静态信息。
///
/// 拿到它不需要加载任何脚本代码 —— 设置页列清单、显示权限，
/// 都只是读文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    /// 扩展 id，取自清单。同时用作会话 id 的一部分。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 入口文件名，相对于目录。
    pub entry: String,
    /// 扩展目录（绝对路径）。
    pub dir: PathBuf,
}

impl Extension {
    /// 入口文件的完整路径。
    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.entry)
    }

    /// 这个扩展专用的会话 id。
    ///
    /// 用它做事件分流：内核产出的事件带 `session`，UI 据此判断
    /// 该往哪个面板投递。给每个扩展一个独立会话，就不必再往
    /// 事件里加一层"这是谁的请求"字段 —— 现成的机制已经够用。
    pub fn session_id(&self) -> String {
        format!("ext-{}", self.id)
    }
}

/// 清单里我们真正需要的字段。
///
/// 只声明这几个：上游还有 `version` / `capabilities` / `dependencies`，
/// 那些由 gpui-shell 自己在加载时读，我们抄一份只会抄漏。
/// `deny_unknown_fields` 在这里**不能**开 —— 开了就会把上游的合法清单当非法。
#[derive(Debug, Deserialize)]
struct ManifestFile {
    id: String,
    name: String,
    #[serde(default)]
    entry: Option<String>,
}

/// 扩展根目录：`~/.zex/extensions`。
pub fn extensions_dir() -> PathBuf {
    config_dir().join("extensions")
}

/// 配置目录 `~/.zex`。
///
/// 与 `zex_app::config_dir` 同一规则（含 `ZEX_CONFIG_DIR` 覆盖），
/// 但不依赖 zex-app —— 那会让 crate 之间成环。
fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ZEX_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".zex")
}

/// 扫描全部扩展，按 id 排序。
///
/// 单个目录坏掉（清单缺失、JSON 语法错、id 重复）只跳过它自己，
/// 并且用 `tracing::warn` 说出来 —— 一个手滑的扩展不该让整页空白。
pub fn scan(root: &Path) -> Vec<Extension> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<Extension> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        match read_one(&dir) {
            Ok(Some(ext)) => found.push(ext),
            Ok(None) => {}
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "扩展目录无法读取"),
        }
    }
    // id 重复时保留先出现的一个（目录名有序，结果稳定）
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found.dedup_by(|a, b| a.id == b.id);
    found
}

/// 读一个扩展目录。没有清单时返回 `Ok(None)` —— 那不是错误，
/// 只是这个目录不是扩展（比如用户随手放了个 README）。
fn read_one(dir: &Path) -> Result<Option<Extension>, String> {
    let manifest_path = dir.join(MANIFEST);
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Ok(None);
    };
    let manifest: ManifestFile =
        serde_json::from_str(&text).map_err(|e| format!("{MANIFEST} 解析失败: {e}"))?;
    if manifest.id.trim().is_empty() {
        return Err("id 不能为空".into());
    }
    let entry = manifest
        .entry
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENTRY.to_string());
    // 入口必须是目录内的相对路径：允许 `../` 的话，一个扩展就能
    // 让宿主去执行它目录之外的文件
    if entry.starts_with('/') || entry.contains("..") {
        return Err(format!("entry 必须是目录内的相对路径，收到 `{entry}`"));
    }
    Ok(Some(Extension {
        id: manifest.id,
        name: manifest.name,
        entry,
        dir: dir.to_path_buf(),
    }))
}

/// 扫描默认位置。
pub fn scan_default() -> Vec<Extension> {
    scan(&extensions_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zex-ext-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_ext(root: &Path, dir: &str, manifest: &str, entry_body: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(MANIFEST), manifest).unwrap();
        std::fs::write(d.join(DEFAULT_ENTRY), entry_body).unwrap();
    }

    #[test]
    fn reads_manifest_and_entry() {
        let root = temp_root("basic");
        write_ext(
            &root,
            "demo",
            r#"{"id":"demo","name":"示例","entry":"index.js"}"#,
            "",
        );
        std::fs::write(root.join("demo").join("index.js"), "").unwrap();
        let list = scan(&root);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "demo");
        assert_eq!(list[0].name, "示例");
        assert_eq!(list[0].entry, "index.js");
        assert!(list[0].entry_path().ends_with("demo/index.js"));
    }

    #[test]
    fn entry_defaults_to_main_js() {
        let root = temp_root("default-entry");
        write_ext(&root, "a", r#"{"id":"a","name":"A"}"#, "");
        let list = scan(&root);
        assert_eq!(list[0].entry, "main.js");
    }

    #[test]
    fn skips_dirs_without_manifest() {
        let root = temp_root("nomanifest");
        std::fs::create_dir_all(root.join("not-an-ext")).unwrap();
        std::fs::write(root.join("not-an-ext").join("README.md"), "hi").unwrap();
        assert!(scan(&root).is_empty());
    }

    #[test]
    fn skips_broken_manifest_but_keeps_others() {
        let root = temp_root("broken");
        write_ext(&root, "good", r#"{"id":"good","name":"好"}"#, "");
        write_ext(&root, "bad", "{ 这不是 json", "");
        let list = scan(&root);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "good");
    }

    #[test]
    fn rejects_entry_escaping_directory() {
        let root = temp_root("escape");
        write_ext(&root, "evil", r#"{"id":"evil","name":"E","entry":"../../etc/x.js"}"#, "");
        assert!(scan(&root).is_empty(), "逃出目录的 entry 必须被拒");
    }

    #[test]
    fn tolerates_upstream_only_fields() {
        // 上游清单里的 version / capabilities 不该让解析失败
        let root = temp_root("upstream");
        write_ext(
            &root,
            "full",
            r#"{"id":"full","name":"F","version":"1.0.0","shell-version":"0.9",
                "entry":"main.js","capabilities":{"fs":{"read":["."]}}}"#,
            "",
        );
        let list = scan(&root);
        assert_eq!(list.len(), 1, "上游字段不该被当成非法");
    }

    #[test]
    fn session_id_is_namespaced() {
        let ext = Extension {
            id: "demo".into(),
            name: "D".into(),
            entry: "main.js".into(),
            dir: PathBuf::from("/tmp/x"),
        };
        // 前缀让"是不是扩展的会话"一眼可判
        assert_eq!(ext.session_id(), "ext-demo");
    }

    #[test]
    fn sorts_by_id_and_dedups() {
        let root = temp_root("dedup");
        write_ext(&root, "zz", r#"{"id":"b","name":"B"}"#, "");
        write_ext(&root, "aa", r#"{"id":"a","name":"A"}"#, "");
        let list = scan(&root);
        assert_eq!(list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }
}
