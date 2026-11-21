//! 技能：可复用的做法包。
//!
//! 一个技能就是一段写给模型的"怎么做某类事"的说明，加一个名字。
//! 它解决的是重复交代成本：与其每次都说"改 Rust 代码前先跑 cargo check，
//! 改完记得 fmt"，不如存成技能，需要时唤起一次。
//!
//! # 为什么是文件而不是数据库
//!
//! 技能是**人写的文本**：用户想直接编辑、想提交到 git、想分享给别人。
//! 放进 SQLite 就变成了"只能通过界面改"的东西，反而更麻烦。
//!
//! # 存在哪儿
//!
//! 一个技能是**一个目录**：`~/.zex/skills/<名字>/skill.md`
//!
//! ```text
//! ~/.zex/skills/
//! ├── rust-refactor/
//! │   └── skill.md            ← 说明正文
//! └── write-extension/
//!     ├── skill.md
//!     └── gpui-kit.d.ts       ← 这个技能的附件，和它放在一起
//! ```
//!
//! 目录名就是技能名。附件（脚本、模板、生成的声明）跟说明放在同一个
//! 目录里，整个目录可以直接拷贝或提交到 git 分享出去。
//!
//! # 文件格式
//!
//! ```markdown
//! ---
//! description: 重构 Rust 代码时的检查清单
//! ---
//!
//! 1. 先跑 cargo check 建立基线
//! 2. ...
//! ```
//!
//! frontmatter 里的 `name` 可写可不写：不写就用目录名。写了则以它为准
//! （保持老的扁平技能可以原样搬进来）。
//!
//! 描述缺失时，拿正文第一行顶上。
//!
//! # 旧布局
//!
//! 早先是直接摊在根目录下的 `*.md`。这种文件仍然会被读进来（不给用户
//! 制造"技能突然消失"），但会附一条警告，提示移进目录。

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 技能目录里，一个技能由"子目录 + 这个文件名"组成。
pub const SKILL_FILE: &str = "skill.md";

/// 同一个文件的另一种写法。Linux 上大小写敏感，用户手写 `SKILL.md`
/// 不该让技能凭空消失；macOS/Windows 上两者本来就指向同一个文件。
const SKILL_FILE_ALT: &str = "SKILL.md";

/// 在这个目录里找技能说明。找不到返回 `None`。
fn skill_file_in(dir: &Path) -> Option<PathBuf> {
    [SKILL_FILE, SKILL_FILE_ALT]
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// 一个技能。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// 名字，模型用它来引用。建议用短横线分隔的小写词。
    pub name: String,
    /// 一句话说明"什么时候该用它"。这句话会进系统提示，所以要短。
    pub description: String,
    /// 正文：具体的做法、步骤、注意事项。
    pub body: String,
    /// 来源文件路径（界面上展示、也用于定位问题）。
    pub path: PathBuf,
}

impl Skill {
    /// 解析一个技能文件。
    ///
    /// 不认识的格式一律降级处理而不是报错：用户手写的文件
    /// 少一个冒号不该让整个技能列表加载失败。
    /// `fallback_name` 在 frontmatter 没写 `name` 时顶上 —— 目录布局下
    /// 就是目录名，所以 `demo/skill.md` 不写 frontmatter 也叫 `demo`。
    pub fn parse(text: &str, path: PathBuf, fallback_name: &str) -> Skill {
        let (front, body) = split_frontmatter(text);
        let mut name = String::new();
        let mut description = String::new();

        if let Some(front) = front {
            for line in front.lines() {
                let line = line.trim();
                if let Some((k, v)) = line.split_once(':') {
                    let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    match k.trim() {
                        "name" => name = v,
                        "description" => description = v,
                        _ => {}
                    }
                }
            }
        }

        let body = body.trim().to_string();
        if name.is_empty() {
            name = fallback_name.trim().to_string();
        }
        if name.is_empty() {
            name = "unnamed".to_string();
        }
        if description.is_empty() {
            // 没写描述就拿正文第一行顶上，总比空着强
            description = body
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
        }

        Skill {
            name,
            description,
            body,
            path,
        }
    }
}

/// 把 `---` 包起来的头部切出来，返回 (frontmatter, 正文)。
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let t = t.trim_start();
    if !t.starts_with("---") {
        return (None, text);
    }
    // 跳过第一行（开头的 ---）
    let after_open = match t[3..].find('\n') {
        Some(i) => &t[3 + i + 1..],
        None => return (None, text),
    };
    // 找闭合的 ---
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let front = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return (Some(front), body);
        }
        offset += line.len();
    }
    (None, text)
}

/// 技能库：扫描目录、按名字查找。
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
    /// 加载时的警告（例如某个文件读不了），供界面提示。
    warnings: Vec<String>,
}

impl SkillRegistry {
    /// 从目录加载。目录不存在时返回空库（不是错误 —— 没装技能很正常）。
    ///
    /// 布局：`<dir>/<名字>/skill.md`，目录名即技能名。目录里的其它文件
    /// （附件、脚本、生成的 `.d.ts`）一律不管，它们只是跟着说明放在一起。
    pub fn load(dir: &Path) -> SkillRegistry {
        let mut out = SkillRegistry::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };

        // 先收集再排序，而不是边读边解析：
        // 1. 跳点文件（备份、编辑器交换文件、`.git`）
        // 2. `read_dir` 的顺序由文件系统决定，ext4 和 APFS 不一样。同名
        //    去重"保留第一个"这条规则要可复现，输入就得是排好序的。
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        let mut legacy: Vec<PathBuf> = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            let raw = entry.file_name().to_string_lossy().to_string();
            if raw.starts_with('.') {
                continue;
            }

            if path.is_dir() {
                match skill_file_in(&path) {
                    Some(file) => found.push((raw, file)),
                    None => out.warnings.push(format!(
                        "{}: 这个目录里没有 {SKILL_FILE}，没有当成技能",
                        path.display()
                    )),
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                legacy.push(path);
            }
        }

        found.sort();
        for (name, file) in found {
            match std::fs::read_to_string(&file) {
                Ok(text) => out.skills.push(Skill::parse(&text, file, &name)),
                Err(e) => out.warnings.push(format!("{}: {e}", file.display())),
            }
        }

        // 旧布局（`<dir>/x.md`）仍然读进来：技能突然消失比多一条提示糟。
        legacy.sort();
        for path in legacy {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    out.skills.push(Skill::parse(&text, path.clone(), &stem));
                    out.warnings.push(format!(
                        "`{}` 是旧的扁平布局；技能已改成每个技能一个目录，\
                         移到 `{}` 之后这条提示就没了",
                        path.display(),
                        dir.join(&stem).join(SKILL_FILE).display()
                    ));
                }
                Err(e) => out.warnings.push(format!("{}: {e}", path.display())),
            }
        }

        out.skills.sort_by(|a, b| a.name.cmp(&b.name));
        out.skills.dedup_by(|a, b| a.name == b.name);
        out
    }

    /// 全部技能。
    pub fn all(&self) -> &[Skill] {
        &self.skills
    }

    /// 加载过程中的问题。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 按名字查（大小写不敏感，允许带不带前缀）。
    pub fn get(&self, name: &str) -> Option<&Skill> {
        let n = name.trim().trim_start_matches('/');
        self.skills.iter().find(|s| s.name.eq_ignore_ascii_case(n))
    }

    /// 拼成系统提示里的一段。
    ///
    /// 只列名字和一句话说明：正文可能几千字，全塞进提示会挤掉真正要读的内容。
    /// 模型需要细节时会用 `load_skill` 工具取。
    pub fn prompt_section(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut s = String::from(
            "\n## 可用技能\n\n\
             这些是用户预先存好的做法。遇到对应的场合先用 `load_skill` 取出来照着做，\n\
             不要凭一般经验硬来 —— 它们通常包含用户在这个项目里的具体约定。\n\n",
        );
        for skill in &self.skills {
            s.push_str(&format!("- `{}` — {}\n", skill.name, skill.description));
        }
        s
    }
}

/// `load_skill` 工具：按名字取技能正文。
///
/// 只列清单在提示里，正文按需取 —— 一个技能动辄上千字，
/// 十个技能全塞进提示就没地方放工具结果了。
pub struct LoadSkill {
    registry: std::sync::Arc<SkillRegistry>,
}

impl LoadSkill {
    /// 用给定的技能库构造。
    pub fn new(registry: std::sync::Arc<SkillRegistry>) -> Self {
        LoadSkill { registry }
    }
}

/// `load_skill` 的参数。
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct LoadSkillArgs {
    /// 技能名，见系统提示里列出的清单。
    pub name: String,
}

#[async_trait::async_trait]
impl zex_core::tool::Tool for LoadSkill {
    const NAME: &'static str = "load_skill";
    const RISK: zex_core::Risk = zex_core::Risk::ReadOnly;
    type Args = LoadSkillArgs;

    fn description() -> &'static str {
        "取出一个技能的完整正文。系统提示里只列了技能名和一句话说明，\
         真正动手前先用它把细节读出来 —— 技能里通常写着用户在这个项目里的具体约定。"
    }

    async fn call(
        &self,
        _cx: &dyn zex_core::tool::ToolCtx,
        args: LoadSkillArgs,
    ) -> zex_core::Result<zex_core::tool::ToolOutput> {
        match self.registry.get(&args.name) {
            Some(s) => Ok(zex_core::tool::ToolOutput::ok(s.body.clone())),
            None => {
                let names: Vec<&str> = self
                    .registry
                    .all()
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect();
                Ok(zex_core::tool::ToolOutput::err(if names.is_empty() {
                    "当前没有任何技能。".to_string()
                } else {
                    format!(
                        "没有 `{}` 这个技能。可用的有：{}",
                        args.name,
                        names.join(", ")
                    )
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个技能：`<dir>/<name>/skill.md`（目录名即技能名，不写 name）。
    fn write_skill(dir: &Path, name: &str, desc: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(SKILL_FILE),
            format!("---\ndescription: {desc}\n---\n\n正文\n"),
        )
        .unwrap();
    }

    #[test]
    fn loads_one_directory_per_skill() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "demo", "示例技能");

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].name, "demo", "目录名就是技能名");
        assert_eq!(reg.all()[0].description, "示例技能");
        assert!(reg.all()[0].path.ends_with("demo/skill.md"));
    }

    #[test]
    fn attachments_live_next_to_the_skill_file() {
        // 目录布局的用处：附件跟着说明放在一起，不该被当成别的技能
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "write-ext", "写扩展");
        std::fs::write(
            dir.path().join("write-ext/gpui-kit.d.ts"),
            "declare const x: 1;",
        )
        .unwrap();
        std::fs::write(dir.path().join("write-ext/run.sh"), "echo hi").unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].name, "write-ext");
    }

    #[test]
    fn frontmatter_name_wins_over_directory_name() {
        // 老技能搬进目录时可以保持原来的名字
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("dir-name");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(SKILL_FILE),
            "---\nname: real-name\ndescription: 说明\n---\n\n正文\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all()[0].name, "real-name");
    }

    #[test]
    fn uppercase_skill_md_also_works() {
        // Linux 上大小写敏感：用户手写 SKILL.md 不该让技能消失
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("upper");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            "---\ndescription: 大写文件名\n---\n\n正文",
        )
        .unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1);
        assert_eq!(reg.all()[0].name, "upper");
    }

    #[test]
    fn load_skips_dotfiles() {
        // 备份、编辑器交换文件、.git 都不是技能
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "x", "新版描述");
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join(".hidden/skill.md"), "不该读进来").unwrap();
        std::fs::write(dir.path().join(".bundled-backup-x.md"), "旧版").unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1, "只该留下真技能：{:?}", reg.all());
        assert_eq!(reg.all()[0].description, "新版描述");
    }

    #[test]
    fn load_ignores_non_markdown_in_root() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "a", "A");
        std::fs::write(dir.path().join("notes.txt"), "不是技能").unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn directory_without_skill_md_is_reported_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty-dir")).unwrap();
        std::fs::write(dir.path().join("empty-dir/README.md"), "只有个说明").unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert!(reg.all().is_empty(), "没有 skill.md 就不算技能");
        assert!(
            reg.warnings().iter().any(|w| w.contains("empty-dir")),
            "该提示一句：{:?}",
            reg.warnings()
        );
    }

    #[test]
    fn legacy_flat_file_is_still_loaded_with_a_warning() {
        // 旧的 `skills/x.md` 仍然可用，但不能无声无息 —— 用户得知道该搬家
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("old.md"),
            "---\ndescription: 老布局\n---\n\n正文\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1, "老布局也要读进来，不能凭空消失");
        assert_eq!(reg.all()[0].name, "old");
        assert!(
            reg.warnings().iter().any(|w| w.contains("old/skill.md")),
            "该提示搬到目录里：{:?}",
            reg.warnings()
        );
    }

    #[test]
    fn load_is_sorted_and_deduplicated_by_name() {
        let dir = tempfile::tempdir().unwrap();
        // 目录顺序和技能名顺序故意不一致，验证按 name 排
        write_skill(dir.path(), "z-dir", "后");
        write_skill(dir.path(), "a-dir", "先");
        // 两个目录声明同一个 name：去重保留目录名排序靠前的那个
        for (d, desc) in [("dup-b", "重复 B"), ("dup-a", "重复 A")] {
            let p = dir.path().join(d);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join(SKILL_FILE),
                format!("---\nname: same\ndescription: {desc}\n---\n\n正文\n"),
            )
            .unwrap();
        }

        let reg = SkillRegistry::load(dir.path());
        let names: Vec<_> = reg.all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a-dir", "same", "z-dir"]);
        let same = reg.get("same").unwrap();
        assert_eq!(same.description, "重复 A", "按目录名排序后取第一个");
    }

    #[test]
    fn load_of_a_missing_directory_is_empty_not_an_error() {
        let reg = SkillRegistry::load(Path::new("/nonexistent/zex-skills"));
        assert!(reg.all().is_empty());
    }

    #[test]
    fn parses_frontmatter() {
        let text = "---\nname: rust-check\ndescription: 改完 Rust 代码后的检查\n---\n\n1. cargo fmt\n2. cargo check\n";
        let s = Skill::parse(text, PathBuf::from("/tmp/x.md"), "x");
        assert_eq!(s.name, "rust-check");
        assert_eq!(s.description, "改完 Rust 代码后的检查");
        assert!(s.body.contains("cargo fmt"));
        assert!(!s.body.contains("---"));
    }

    #[test]
    fn falls_back_to_the_given_name_and_first_line() {
        let text = "先读文件再改，别猜。\n\n细节...";
        let s = Skill::parse(text, PathBuf::from("/tmp/demo/skill.md"), "demo");
        assert_eq!(s.name, "demo");
        assert_eq!(s.description, "先读文件再改，别猜。");
    }

    #[test]
    fn tolerates_broken_frontmatter() {
        // 少了闭合的 ---，按没有 frontmatter 处理，不 panic
        let text = "---\nname: 半截\n\n正文";
        let s = Skill::parse(text, PathBuf::from("/tmp/broken/skill.md"), "broken");
        assert_eq!(s.name, "broken");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let mut r = SkillRegistry::default();
        r.skills.push(Skill {
            name: "Rust-Check".into(),
            description: "d".into(),
            body: "b".into(),
            path: PathBuf::new(),
        });
        assert!(r.get("rust-check").is_some());
        assert!(r.get("/RUST-CHECK").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn prompt_section_lists_names_only() {
        let mut r = SkillRegistry::default();
        r.skills.push(Skill {
            name: "a".into(),
            description: "做 A 的方法".into(),
            body: "这一大段正文不该出现在提示里".into(),
            path: PathBuf::new(),
        });
        let p = r.prompt_section();
        assert!(p.contains("`a` — 做 A 的方法"));
        assert!(!p.contains("这一大段正文"));
    }
}
