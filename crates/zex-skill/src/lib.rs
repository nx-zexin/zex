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
//! 所以存储是 `~/.zex/skills/*.md`，一行 frontmatter 加正文。
//!
//! # 格式
//!
//! ```markdown
//! ---
//! name: rust-refactor
//! description: 重构 Rust 代码时的检查清单
//! ---
//!
//! 1. 先跑 cargo check 建立基线
//! 2. ...
//! ```
//!
//! frontmatter 缺失时，文件名当名字，首行非空文本当描述。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub fn parse(text: &str, path: PathBuf) -> Skill {
        let fallback_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".to_string());

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
            name = fallback_name;
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
    pub fn load(dir: &Path) -> SkillRegistry {
        let mut out = SkillRegistry::default();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return out,
        };

        // 先收集路径再排序，而不是边读边解析。
        //
        // 两个理由，都是"同一份目录在不同机器上要给出同一个结果"：
        //
        // 1. **跳点文件。** 内置技能升级时会在旁边留一份
        //    `.bundled-backup-<name>.md`，它是同一个技能的旧版本（同样的
        //    `name`、旧的 `description`）。读进来就成了两个同名技能，
        //    去重时留下哪个全看目录顺序 —— 表现是"提示词里的技能描述
        //    忽新忽旧"。
        // 2. **排序。** `read_dir` 的顺序由文件系统决定，ext4 和 APFS 不一样。
        //    同名去重"保留第一个"这条规则要可复现，输入就得是排好序的。
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
            .filter(|path| {
                // 点文件一律跳过：备份、编辑器交换文件、隐藏笔记都不是技能
                !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            })
            .collect();
        paths.sort();

        for path in paths {
            match std::fs::read_to_string(&path) {
                Ok(text) => out.skills.push(Skill::parse(&text, path)),
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
                let names: Vec<&str> = self.registry.all().iter().map(|s| s.name.as_str()).collect();
                Ok(zex_core::tool::ToolOutput::err(if names.is_empty() {
                    "当前没有任何技能。".to_string()
                } else {
                    format!("没有 `{}` 这个技能。可用的有：{}", args.name, names.join(", "))
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个技能文件。
    fn write_skill(dir: &Path, file: &str, name: &str, desc: &str) {
        std::fs::write(
            dir.join(file),
            format!("---\nname: {name}\ndescription: {desc}\n---\n\n正文\n"),
        )
        .unwrap();
    }

    #[test]
    fn load_skips_dotfiles() {
        // 回归：内置技能升级会留下 `.bundled-backup-x.md`（同名、旧描述），
        // 早先它也会被当成技能读进来，去重时留下哪个全看目录顺序 ——
        // 表现是提示词里的技能描述忽新忽旧。
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "x.md", "x", "新版描述");
        write_skill(dir.path(), ".bundled-backup-x.md", "x", "旧版描述");
        write_skill(dir.path(), ".x.md.swp", "x", "编辑器交换文件");
        std::fs::write(dir.path(), "").ok();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1, "只该留下真文件：{:?}", reg.all());
        assert_eq!(reg.all()[0].description, "新版描述");
    }

    #[test]
    fn load_ignores_non_markdown() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "a.md", "a", "A");
        std::fs::write(dir.path().join("notes.txt"), "不是技能").unwrap();
        std::fs::create_dir(dir.path().join("sub.md")).unwrap();

        let reg = SkillRegistry::load(dir.path());
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn load_is_sorted_and_deduplicated_by_name() {
        let dir = tempfile::tempdir().unwrap();
        // 文件名顺序和技能名顺序故意不一致，验证按 name 排
        write_skill(dir.path(), "z.md", "aaa", "先");
        write_skill(dir.path(), "a.md", "zzz", "后");
        write_skill(dir.path(), "dup.md", "aaa", "重复的");

        let reg = SkillRegistry::load(dir.path());
        let names: Vec<_> = reg.all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["aaa", "zzz"]);
        // 同名保留文件名排序后的第一个（a.md 之前是 dup.md? 不 —— dup < z，
        // 所以 aaa 这条应该来自 dup.md）
        assert_eq!(reg.all()[0].description, "重复的");
    }

    #[test]
    fn load_of_a_missing_directory_is_empty_not_an_error() {
        let reg = SkillRegistry::load(Path::new("/nonexistent/zex-skills"));
        assert!(reg.all().is_empty());
    }

    #[test]
    fn parses_frontmatter() {
        let text = "---\nname: rust-check\ndescription: 改完 Rust 代码后的检查\n---\n\n1. cargo fmt\n2. cargo check\n";
        let s = Skill::parse(text, PathBuf::from("/tmp/x.md"));
        assert_eq!(s.name, "rust-check");
        assert_eq!(s.description, "改完 Rust 代码后的检查");
        assert!(s.body.contains("cargo fmt"));
        assert!(!s.body.contains("---"));
    }

    #[test]
    fn falls_back_to_filename_and_first_line() {
        let text = "先读文件再改，别猜。\n\n细节...";
        let s = Skill::parse(text, PathBuf::from("/tmp/no-front.md"));
        assert_eq!(s.name, "no-front");
        assert_eq!(s.description, "先读文件再改，别猜。");
    }

    #[test]
    fn tolerates_broken_frontmatter() {
        // 少了闭合的 ---，按没有 frontmatter 处理，不 panic
        let text = "---\nname: 半截\n\n正文";
        let s = Skill::parse(text, PathBuf::from("/tmp/broken.md"));
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
