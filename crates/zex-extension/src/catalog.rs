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
//! ├── main.js
//! └── icon.svg           # 可选，清单里不写，靠文件名认
//! ```
//!
//! `gpui-shell.json` 直接复用上游的清单格式，不自造一套：这样同一个目录
//! 既能被 zex 挂载，也能用 `gpui-shell dev` 单独跑起来调试。
//!
//! 上游的清单 `deny_unknown_fields`，所以不能往里加自定义字段。
//! 需要额外元数据时另开文件，别去污染它。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

/// 清单文件名，与上游一致。
pub const MANIFEST: &str = "gpui-shell.json";

/// 缺省入口。
pub const DEFAULT_ENTRY: &str = "main.js";

/// 图标的约定文件名。
///
/// 为什么是文件而不是清单字段：上游的 `gpui-shell.json` 开了
/// `deny_unknown_fields`，往里塞 `icon` 会让清单整个解析失败 ——
/// 那样 zex 能显示图标，`gpui-shell dev` 却跑不起来。约定文件名
/// 对上游完全透明，两边都能用。
pub const ICON: &str = "icon.svg";

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
    /// 清单里申请的能力。只作展示与留痕，本版不生效。
    pub request: CapabilityRequest,
    /// 图标的原始 SVG 字节（`icon.svg` 存在时才有）。
    ///
    /// 扫描时就读进来，而不是每次渲染再读盘：侧边栏一行一个图标，
    /// 每帧都去 open/read 一遍太亏。图标只有几 KB，内存里放着无妨。
    pub icon: Option<Arc<[u8]>>,
}

impl Extension {
    /// 入口文件的完整路径。
    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.entry)
    }

    /// 这个扩展的 SQLite 库文件（`~/.zex/state/db/<id>.db`）。
    ///
    /// 路径由宿主按 id 拼，脚本拿不到也改不了 —— 这是"应用碰不到
    /// zex 自己的库"的实现方式，见 [`crate::store_db`]。
    pub fn db_file(&self) -> PathBuf {
        crate::store_db::db_path(&state_dir(), &self.id)
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

/// 扩展在清单里申请的能力（**只是申请**）。
///
/// # 为什么必须显示出来
///
/// 模型能写扩展，也就能写清单。如果清单一律被忽略，"申请"和"授予"之间
/// 就没有可见的中间态 —— 用户既不知道自己装的东西想要什么，也无从判断
/// 一个来路不明的扩展安不安全。
///
/// # 哪些真的生效
///
/// 只有 **`storage`**：宿主建运行时把 `Capabilities::new().storage(true)`
/// 装进全局默认策略，配上 `set_storage_path` 的落盘文件。注意它是**全局**
/// 的 —— 没在清单里申请存储的应用一样能用 `localStorage`，所有应用共享
/// 同一个 JSON（见 [`storage_path`]）。这是当前结构（全应用共用一份
/// `load_application` 策略）的必然结果，不是疏漏。
///
/// 文件、网络、剪贴板、子进程都**不生效**：它们是按应用授予才有意义的东西，
/// 而现在是「全局一套能力」，按清单授予等于把 A 申请的权限递给 B ——
/// 比不授予更糟。这部分仍然只做三件事：**解析、展示、留痕**，
/// 等换成上游的 per-app `Policy` 再接上。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityRequest {
    /// 可读目录（原文，未展开 `${dataDir}` 这类占位符）。
    pub fs_read: Vec<String>,
    /// 可写目录。
    pub fs_write: Vec<String>,
    /// 可执行的命令；`Some("*")` 表示不限。
    pub execute: Option<Vec<String>>,
    /// 可访问的网络主机。
    pub network_hosts: Vec<String>,
    /// 是否要读剪贴板。
    ///
    /// 与 `clipboard_write` 分开：读是把用户剪贴板里的东西交给脚本
    /// （可能含密码），写只是覆盖剪贴板。两者的风险不是一回事，
    /// 合成一个布尔值会让"只想提供复制按钮"的应用也被当成能读。
    pub clipboard_read: bool,
    /// 是否要写剪贴板。
    pub clipboard_write: bool,
    /// 是否要 localStorage。
    pub storage: bool,
    /// 是否要 `process.exit`。
    pub exit: bool,
}

impl CapabilityRequest {
    /// 一项都不申请。
    pub fn is_empty(&self) -> bool {
        self.fs_read.is_empty()
            && self.fs_write.is_empty()
            && self.execute.is_none()
            && self.network_hosts.is_empty()
            && !self.clipboard_read
            && !self.clipboard_write
            && !self.storage
            && !self.exit
    }

    /// 有没有"宽授权"。
    ///
    /// 三类：不限命令、把整个家目录交出去、放行任意主机。
    /// 它们不是错，但值得在界面上单独标出来 —— 一个只需要读自己数据目录的
    /// 扩展，没有理由申请这三样里的任何一个。
    pub fn broad(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(cmds) = &self.execute {
            if cmds.iter().any(|c| c == "*") {
                out.push("可执行任意命令".to_string());
            }
        }
        for root in self.fs_read.iter().chain(self.fs_write.iter()) {
            if root.contains("${homeDir}") || root == "/" || root == "~" {
                out.push(format!("整个家目录：{root}"));
            }
        }
        if self.network_hosts.iter().any(|h| h == "*") {
            out.push("任意网络主机".to_string());
        }
        out
    }

    /// 拼成一句给人看的话，例如 `读 2 处 · 写 1 处 · 网络 1 个主机`。
    pub fn summary(&self) -> String {
        let mut bits = Vec::new();
        if !self.fs_read.is_empty() {
            bits.push(format!("读 {} 处", self.fs_read.len()));
        }
        if !self.fs_write.is_empty() {
            bits.push(format!("写 {} 处", self.fs_write.len()));
        }
        if let Some(cmds) = &self.execute {
            bits.push(if cmds.iter().any(|c| c == "*") {
                "执行任意命令".to_string()
            } else {
                format!("执行 {} 个命令", cmds.len())
            });
        }
        if !self.network_hosts.is_empty() {
            bits.push(format!("网络 {} 个主机", self.network_hosts.len()));
        }
        match (self.clipboard_read, self.clipboard_write) {
            (true, true) => bits.push("剪贴板 读+写".to_string()),
            (true, false) => bits.push("剪贴板 读".to_string()),
            (false, true) => bits.push("剪贴板 写".to_string()),
            (false, false) => {}
        }
        if self.storage {
            bits.push("存储".to_string());
        }
        if self.exit {
            bits.push("退出".to_string());
        }
        bits.join(" · ")
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

/// 扩展本地存储的落盘位置：`~/.zex/state/extensions-store.json`。
///
/// # 为什么是"一个文件"而不是"每个应用一个"
///
/// `set_storage_path` 和 `set_capabilities` 改的都是上游的**全局默认
/// Policy**，而 zex 挂应用走的是 `load_application`（全应用共用一个
/// policy）。所以在当前结构下，这个 JSON 天然被所有应用共享：
/// 键空间共享、8 MiB / 1024 键 / 单值 1 MiB 也是全局算的。
///
/// 要每个应用各一个文件，得换成上游的 `plugin::Plugin::load`
/// （它内部是 `with_storage_path(<data_dir>/<id>/store.json)`）——
/// 那是一整套插件生命周期，不是两行能改的。先共享，等 per-app 策略
/// 真落地了再拆。
pub fn storage_path() -> PathBuf {
    state_dir().join("extensions-store.json")
}

/// 宿主自己的状态目录：`~/.zex/state`。
///
/// 和 `~/.zex` 根目录分开：根目录放配置和 zex 自己的库（`zex.db`），
/// 这里放**扩展产生的数据**（localStorage 的 JSON、每个应用的 SQLite）。
/// 分开了，用户想清理扩展数据时不用怕误删会话。
pub fn state_dir() -> PathBuf {
    config_dir().join("state")
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
    // id 会变成目录名、宿主模块名（`zex.<id>`）和库文件名。
    // 任何一处被注入 `../` 都会出事，所以入口就挡
    if !crate::store_db::valid_app_id(&manifest.id) {
        return Err(format!(
            "id `{}` 不合法：只能用字母、数字、`-`、`_`、`.`，且不能含 `..`",
            manifest.id
        ));
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
        icon: read_icon(dir),
        dir: dir.to_path_buf(),
        request: parse_request(&text),
    }))
}

/// 读 `icon.svg`。
///
/// 读不出来就当没有 —— 图标是装饰，缺了不该让整个应用装不上。
/// 空文件也当没有：`svg().data(&[])` 会让 resvg 报解析错误，
/// 那是由每帧刷一条日志的代价换来的一个空方块，不值。
fn read_icon(dir: &Path) -> Option<Arc<[u8]>> {
    let bytes = std::fs::read(dir.join(ICON)).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(Arc::from(bytes))
}

/// 从清单原文里取能力申请。
///
/// 单独解析一遍原始 JSON，而不是给 `ManifestFile` 加字段：上游清单
/// `deny_unknown_fields`，字段形状偶尔会加；这里只认我们看得懂的键，
/// 认不出就当作没申请 —— 缺省必须是"什么都不给"。
fn parse_request(text: &str) -> CapabilityRequest {
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(text) else {
        return CapabilityRequest::default();
    };
    let Some(caps) = raw.get("capabilities") else {
        return CapabilityRequest::default();
    };
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let fs = caps.get("fs");
    let exec = fs.and_then(|f| f.get("execute")).and_then(|e| match e {
        // 上游两种写法：命令数组，或字符串 "*"
        serde_json::Value::Array(_) => Some(strings(Some(e))),
        serde_json::Value::String(s) => Some(vec![s.clone()]),
        _ => None,
    });
    let network = caps.get("network");
    CapabilityRequest {
        fs_read: strings(fs.and_then(|f| f.get("read"))),
        fs_write: strings(fs.and_then(|f| f.get("write"))),
        execute: exec,
        network_hosts: strings(network.and_then(|n| n.get("hosts"))),
        // 上游清单的写法是 `"clipboard": {"read": true, "write": true}`。
        // 读、写分别记：给出去的权利越小，用户点"允许"时越敢点。
        clipboard_read: caps
            .get("clipboard")
            .and_then(|c| c.get("read"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        clipboard_write: caps
            .get("clipboard")
            .and_then(|c| c.get("write"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        // 上游这个字段默认给 true；这里按"没写就是没要"处理，
        // 因为我们的实现里存储也要显式授予
        storage: caps
            .get("storage")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        exit: caps
            .get("process")
            .and_then(|p| p.get("exit"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// 扫描默认位置。
pub fn scan_default() -> Vec<Extension> {
    scan(&extensions_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("zex-ext-test-{tag}-{}", std::process::id()));
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
        write_ext(
            &root,
            "evil",
            r#"{"id":"evil","name":"E","entry":"../../etc/x.js"}"#,
            "",
        );
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
    fn reads_icon_svg_from_convention_file() {
        let root = temp_root("icon");
        write_ext(&root, "a", r#"{"id":"a","name":"A"}"#, "");
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/></svg>"#;
        std::fs::write(root.join("a").join(ICON), svg).unwrap();
        let list = scan(&root);
        let icon = list[0].icon.as_ref().expect("icon.svg 存在就该读进来");
        assert_eq!(&**icon, svg.as_bytes());
    }

    #[test]
    fn missing_or_empty_icon_is_none() {
        let root = temp_root("noicon");
        write_ext(&root, "a", r#"{"id":"a","name":"A"}"#, "");
        write_ext(&root, "b", r#"{"id":"b","name":"B"}"#, "");
        std::fs::write(root.join("b").join(ICON), "").unwrap();
        let list = scan(&root);
        assert!(list[0].icon.is_none(), "没有 icon.svg");
        assert!(list[1].icon.is_none(), "空文件也算没有");
    }

    #[test]
    fn icon_is_not_a_manifest_field() {
        // 图标走文件名约定，清单里写 icon 也不会被当字段
        let root = temp_root("icon-field");
        write_ext(&root, "a", r#"{"id":"a","name":"A","icon":"x.svg"}"#, "");
        let list = scan(&root);
        assert_eq!(list.len(), 1, "上游清单允许的额外键不该导致失败");
        assert!(list[0].icon.is_none(), "清单里的 icon 不参与，只看文件");
    }

    #[test]
    fn session_id_is_namespaced() {
        let ext = Extension {
            id: "demo".into(),
            name: "D".into(),
            entry: "main.js".into(),
            dir: PathBuf::from("/tmp/x"),
            request: CapabilityRequest::default(),
            icon: None,
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
        assert_eq!(
            list.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    fn parse(caps_json: &str) -> CapabilityRequest {
        parse_request(&format!(
            r#"{{"id":"x","name":"X","capabilities":{caps_json}}}"#
        ))
    }

    #[test]
    fn absent_capabilities_mean_nothing_requested() {
        let r = parse_request(r#"{"id":"x","name":"X"}"#);
        assert!(r.is_empty(), "没写 capabilities 就是什么都不要");
        assert!(r.broad().is_empty());
    }

    #[test]
    fn reads_fs_and_network_lists() {
        let r = parse(
            r#"{"fs":{"read":["${dataDir}","/tmp/a"],"write":["${dataDir}"]},
                          "network":{"hosts":["api.example.com"]}}"#,
        );
        assert_eq!(r.fs_read.len(), 2);
        assert_eq!(r.fs_write.len(), 1);
        assert_eq!(r.network_hosts, ["api.example.com"]);
        assert!(!r.is_empty());
        assert!(r.broad().is_empty(), "具体路径不算宽授权");
    }

    #[test]
    fn execute_accepts_list_and_star() {
        let listed = parse(r#"{"fs":{"execute":["git","rg"]}}"#);
        assert_eq!(
            listed.execute.as_deref(),
            Some(&["git".to_string(), "rg".to_string()][..])
        );
        assert!(listed.broad().is_empty());

        let star = parse(r#"{"fs":{"execute":"*"}}"#);
        assert!(star.broad().iter().any(|b| b.contains("任意命令")));
    }

    #[test]
    fn flags_home_directory_and_any_host_as_broad() {
        let r = parse(r#"{"fs":{"read":["${homeDir}"]},"network":{"hosts":["*"]}}"#);
        let broad = r.broad();
        assert!(
            broad.iter().any(|b| b.contains("家目录")),
            "拿到：{broad:?}"
        );
        assert!(
            broad.iter().any(|b| b.contains("任意网络主机")),
            "拿到：{broad:?}"
        );
    }

    #[test]
    fn summary_is_readable_and_empty_when_nothing_requested() {
        assert_eq!(CapabilityRequest::default().summary(), "");
        let r = parse(r#"{"fs":{"read":["a"],"write":["b"]},"clipboard":{"read":true}}"#);
        let s = r.summary();
        assert!(
            s.contains("读 1 处") && s.contains("写 1 处") && s.contains("剪贴板"),
            "{s}"
        );
    }

    #[test]
    fn clipboard_read_and_write_are_separate() {
        // 只想提供「复制」按钮的扩展不应该连带拿到读权限：
        // 两份权利合成一个布尔值，用户点「允许」时根本看不出来。
        let only_write = parse(r#"{"clipboard":{"write":true}}"#);
        assert!(only_write.clipboard_write && !only_write.clipboard_read);
        assert_eq!(only_write.summary(), "剪贴板 写");

        let only_read = parse(r#"{"clipboard":{"read":true}}"#);
        assert!(only_read.clipboard_read && !only_read.clipboard_write);
        assert_eq!(only_read.summary(), "剪贴板 读");

        let both = parse(r#"{"clipboard":{"read":true,"write":true}}"#);
        assert_eq!(both.summary(), "剪贴板 读+写");

        // 一个子键都没写时两个都是 false，而不是“写了这个键就算要”
        let empty = parse(r#"{"clipboard":{}}"#);
        assert!(!empty.clipboard_read && !empty.clipboard_write);
        assert!(empty.is_empty());
    }

    #[test]
    fn malformed_capabilities_degrade_to_nothing() {
        // 形状不对时宁可不给，也不要猜
        let r = parse(r#"{"fs":{"read":"不是数组"}}"#);
        assert!(r.fs_read.is_empty());
    }
}
