//! 内容搜索工具：`grep`。
//!
//! # 为什么内置引擎，而不是调 `rg`
//!
//! ripgrep 的快是库给的，`rg` 只是它的命令行外壳。所以这里直接用
//! `grep-searcher` + `grep-regex` + `ignore` —— 和 `rg` 同一套引擎，
//! 但**不依赖用户装了什么**。
//!
//! 这不是洁癖：Windows 上普通用户的机器里没有 `rg`（不在系统路径、
//! 也没用包管理器装过），而"让模型搜一下这个仓库"是最基本的动作。
//! 靠 shell 调 `rg`，结果就是"开发者机器上好好的，到用户那儿就是
//! 'rg 不是内部或外部命令'"。
//!
//! # 边界
//!
//! - **跟着 .gitignore 走**：默认跳过被忽略的文件、隐藏目录、二进制文件，
//!   和 rg 的默认行为一致。想全量搜就显式 `hidden: true`。
//! - **行数 + 字节 + 时间三重上限**：一条 grep 动辄几千行，全塞进上下文
//!   等于拿窗口换噪音。截断处写明原因，模型据此缩窄条件。
//! - **密钥文件不进上下文**：命中先落地，再按宿主的 `is_secret_path`
//!   过滤一遍（见 [`Grep::call`] 里的注释，顺序不能反）。

use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use zex_core::tool::{Tool, ToolCtx, ToolOutput};
use zex_core::{Error, Result, Risk};

/// 默认返回的命中行数上限。
pub const DEFAULT_MAX_HITS: usize = 200;

/// 命中行数的硬上限。
pub const MAX_HITS: usize = 2_000;

/// 输出字节上限。行数没到上限但每行都很长时，靠这个兜底。
pub const MAX_OUTPUT_BYTES: usize = 60_000;

/// 单次搜索的时间上限。超大仓库上宁可给部分结果，也不要卡住整轮。
pub const TIME_BUDGET_MS: u128 = 20_000;

/// `grep` 的参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// 正则表达式（Rust regex 语法）。只想按字面量搜就把 fixed_strings 设成 true。
    pub pattern: String,
    /// 搜哪个目录或文件，默认当前工作目录。
    #[serde(default)]
    pub path: Option<String>,
    /// 只搜匹配这些 glob 的文件，例如 `*.rs`、`src/**/*.toml`。可给多个。
    #[serde(default)]
    pub globs: Option<Vec<String>>,
    /// 区分大小写。默认不区分 —— 用户几乎总是想宽松匹配。
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// 把 pattern 当普通字符串，不走正则。
    #[serde(default)]
    pub fixed_strings: Option<bool>,
    /// 连隐藏文件和被 .gitignore 忽略的文件一起搜。默认 false。
    #[serde(default)]
    pub hidden: Option<bool>,
    /// 最多返回多少条命中，默认 200，上限 2000。
    #[serde(default)]
    pub max_hits: Option<usize>,
}

/// 按内容搜索文件。
pub struct Grep;

#[async_trait]
impl Tool for Grep {
    const NAME: &'static str = "grep";
    const RISK: Risk = Risk::ReadOnly;
    type Args = GrepArgs;

    fn description() -> &'static str {
        "按内容搜索文件（自带引擎，不依赖 ripgrep 是否安装）。\
         返回 `路径:行号:内容`，默认跳过 .gitignore 里的文件、隐藏文件和二进制文件。\
         找某个字符串/函数定义在哪、谁引用了它，用这个而不是 shell 里的 grep 或 rg。\
         结果被截断时会写明原因，此时请用 globs 或更精确的 pattern 缩小范围。"
    }

    async fn call(&self, cx: &dyn ToolCtx, args: GrepArgs) -> Result<ToolOutput> {
        let root = match args.path.as_deref() {
            Some(p) => cx.resolve_path(p)?,
            None => cx.cwd().to_path_buf(),
        };
        if !root.exists() {
            return Ok(ToolOutput::err(format!("路径不存在：{}", root.display())));
        }
        // 直接冲着密钥文件去搜，一开始就拒掉，不用等搜完再筛
        if cx.is_secret_path(&root) {
            return Err(Error::Denied(format!(
                "`{}` 命中密钥黑名单，不允许读取",
                root.display()
            )));
        }

        let max_hits = args.max_hits.unwrap_or(DEFAULT_MAX_HITS).clamp(1, MAX_HITS);
        let pattern = args.pattern.clone();
        let matcher = build_matcher(&args)?;

        // 搜索是同步 + CPU 密集的，扔到阻塞线程池，别占着 async 运行时
        let hits = tokio::task::spawn_blocking(move || search(&root, &matcher, &args, max_hits))
            .await
            .map_err(|e| Error::ToolFailed {
                tool: Self::NAME.into(),
                message: format!("搜索线程异常：{e}"),
            })??;

        // 密钥过滤**放在结果落地之后**：`is_secret_path` 是宿主的策略
        // （读配置、认 .env / id_rsa 这一串），而 `spawn_blocking` 里的闭包
        // 要求 'static，借不到 `cx`。所以让搜索先把路径带回来，这里再筛 ——
        // 反正一条命中的路径只是一个 PathBuf，多带一点不心疼。
        let mut rows: Vec<Row> = hits
            .rows
            .into_iter()
            .filter(|r| !cx.is_secret_path(&r.path))
            .collect();
        if pattern.trim().is_empty() {
            rows.clear();
        }
        Ok(render(rows, hits.truncated))
    }
}

/// 一条命中的原始数据。带绝对路径是为了回来之后还能按策略过滤。
struct Row {
    path: PathBuf,
    line: u64,
    text: String,
    display: String,
}

/// 一次搜索的产出。
struct Hits {
    rows: Vec<Row>,
    /// 因为什么提前停了
    truncated: Vec<&'static str>,
}

/// 编译正则。
fn build_matcher(args: &GrepArgs) -> Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(!args.case_sensitive.unwrap_or(false))
        .fixed_strings(args.fixed_strings.unwrap_or(false));
    builder.build(&args.pattern).map_err(|e| Error::ToolFailed {
        tool: "grep".into(),
        message: format!("正则无法编译（{}）：{e}", args.pattern),
    })
}

/// 真正的搜索。同步执行，由调用方放进 spawn_blocking。
fn search(root: &Path, matcher: &RegexMatcher, args: &GrepArgs, max_hits: usize) -> Result<Hits> {
    let started = Instant::now();
    let mut out = Hits {
        rows: Vec::new(),
        truncated: Vec::new(),
    };

    // 走路：单文件直接把根当成唯一入口，目录则按 .gitignore 遍历
    let mut walk = WalkBuilder::new(root);
    // `hidden: true` 的语义是"别管 .gitignore，也别跳过点文件" —— 和
    // rg 的 `-u` 对齐。只放开 `hidden()` 是不够的：那样 target/ 里的东西
    // 照样被 .gitignore 挡着，用户会发现"我明明开了全量搜"。
    let all = args.hidden.unwrap_or(false);
    walk.hidden(!all)
        .ignore(!all)
        .git_ignore(!all)
        .git_global(!all)
        .git_exclude(!all)
        // 允许父目录里的 .gitignore 生效：在子目录里搜时，仓库根的规则同样算数
        .parents(true)
        .follow_links(false);
    if let Some(globs) = &args.globs {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for g in globs {
            let spec = if g.starts_with('!') {
                g.clone()
            } else {
                // 用户写 `*.rs` 想要的是"任意深度"，rg 也是这个语义
                format!("**/{g}")
            };
            overrides.add(&spec).map_err(|e| Error::ToolFailed {
                tool: "grep".into(),
                message: format!("glob 写法看不懂（{g}）：{e}"),
            })?;
        }
        walk.overrides(overrides.build().map_err(|e| Error::ToolFailed {
            tool: "grep".into(),
            message: format!("glob 无法编译：{e}"),
        })?);
    }

    let mut searcher = SearcherBuilder::new()
        // 二进制文件直接跳过：搜出来的是一堆 \x00，对模型没有意义
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();

    let mut bytes = 0usize;

    for entry in walk.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if started.elapsed().as_millis() > TIME_BUDGET_MS {
            out.truncated.push("时间预算 20s");
            break;
        }

        let path = entry.path();
        let display = display_path(path, root);
        let mut budget_out = false;

        let result = searcher.search_path(
            matcher,
            path,
            UTF8(|lnum, line| {
                if out.rows.len() >= max_hits {
                    out.truncated.push("命中数上限");
                    budget_out = true;
                    return Ok(false);
                }
                let text = line.trim_end_matches(['\n', '\r']).to_string();
                bytes += display.len() + text.len() + 12;
                out.rows.push(Row {
                    path: path.to_path_buf(),
                    line: lnum,
                    text,
                    display: display.clone(),
                });
                if bytes >= MAX_OUTPUT_BYTES {
                    out.truncated.push("输出体积上限");
                    budget_out = true;
                    return Ok(false);
                }
                Ok(true)
            }),
        );
        // 读不了的文件（权限、链接失效）跳过，不打断整次搜索
        if result.is_err() {
            continue;
        }
        if budget_out {
            break;
        }
    }

    Ok(out)
}

/// 相对根目录的短路径：绝对路径每行都要多几十个字符，纯浪费上下文。
fn display_path(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.display().to_string();
    if text.is_empty() {
        // 根本身就是那个文件
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    } else {
        text
    }
}

/// 把结果排成人读的样子。
fn render(rows: Vec<Row>, truncated: Vec<&'static str>) -> ToolOutput {
    if rows.is_empty() {
        return ToolOutput::ok("没有命中。");
    }
    let files: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.display.as_str()).collect();
    let total = rows.len();
    let mut text = rows
        .iter()
        .map(|r| format!("{}:{}:{}", r.display, r.line, r.text))
        .collect::<Vec<_>>()
        .join("\n");

    let mut notes = Vec::new();
    if files.len() > 1 {
        notes.push(format!("{} 个文件", files.len()));
    }
    if !truncated.is_empty() {
        notes.push(format!("已截断：{}", truncated.join("、")));
    }
    if !notes.is_empty() {
        text.push_str(&format!(
            "\n\n... [{total} 条命中；{}] ...",
            notes.join("，")
        ));
    }
    ToolOutput::ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zex_core::SessionId;

    struct Ctx {
        root: PathBuf,
    }

    impl ToolCtx for Ctx {
        fn cwd(&self) -> &Path {
            &self.root
        }
        fn session_id(&self) -> &SessionId {
            static ID: std::sync::OnceLock<SessionId> = std::sync::OnceLock::new();
            ID.get_or_init(|| SessionId::new("ses_grep_test"))
        }
        fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
            let p = Path::new(raw);
            Ok(if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.root.join(p)
            })
        }
        fn is_secret_path(&self, path: &Path) -> bool {
            path.file_name().is_some_and(|n| n == ".env")
        }
    }

    fn ctx_for(dir: &Path) -> Ctx {
        Ctx {
            root: dir.to_path_buf(),
        }
    }

    fn args(pattern: &str, path: &Path) -> GrepArgs {
        GrepArgs {
            pattern: pattern.into(),
            path: Some(path.display().to_string()),
            globs: None,
            case_sensitive: None,
            fixed_strings: Some(true),
            hidden: None,
            max_hits: None,
        }
    }

    async fn run(a: GrepArgs) -> String {
        let dir = a.path.clone().unwrap();
        let ctx = ctx_for(Path::new(&dir));
        Grep.call(&ctx, a).await.unwrap().content
    }

    #[tokio::test]
    async fn finds_a_literal_across_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "helper 在别处\n").unwrap();
        let out = run(args("helper", dir.path())).await;
        assert!(out.contains("a.rs:2:"), "{out}");
        assert!(out.contains("b.txt:1:"), "{out}");
        assert!(out.contains("2 个文件"), "多文件要报文件数：{out}");
    }

    #[tokio::test]
    async fn case_insensitive_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "Hello World\n").unwrap();
        let mut a = args("hello", dir.path());
        a.fixed_strings = None; // 走正则
        let out = run(a).await;
        assert!(out.contains("Hello World"), "{out}");
    }

    #[tokio::test]
    async fn respects_gitignore_unless_hidden_is_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/gen.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

        let mut a = args("needle", dir.path());
        let out = run(a.clone()).await;
        assert!(out.contains("src.rs"), "{out}");
        assert!(!out.contains("gen.rs"), "被 gitignore 的文件不该搜：{out}");

        a.hidden = Some(true);
        let out = run(a).await;
        assert!(
            out.contains("gen.rs"),
            "hidden=true 时要连忽略的一起搜：{out}"
        );
    }

    #[tokio::test]
    async fn globs_filter_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("a.md"), "needle\n").unwrap();
        let mut a = args("needle", dir.path());
        a.globs = Some(vec!["*.rs".into()]);
        let out = run(a).await;
        assert!(out.contains("a.rs"), "{out}");
        assert!(!out.contains("a.md"), "glob 该把 md 排掉：{out}");
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"\x00\x01needle\x02\x00").unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let out = run(args("needle", dir.path())).await;
        assert!(out.contains("a.txt"), "{out}");
        assert!(!out.contains("blob.bin"), "二进制文件要跳过：{out}");
    }

    #[tokio::test]
    async fn secret_files_never_reach_the_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "API_KEY=sk-xxx\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "API_KEY=sk-yyy\n").unwrap();
        let mut a = args("API_KEY", dir.path());
        a.hidden = Some(true); // 否则点文件本来就被跳过，测不出过滤逻辑
        let out = run(a).await;
        assert!(out.contains("a.txt"), "{out}");
        assert!(!out.contains("sk-xxx"), "密钥文件不能进上下文：{out}");
    }

    #[tokio::test]
    async fn bad_regex_reports_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_for(dir.path());
        let mut a = args("(unclosed", dir.path());
        a.fixed_strings = Some(false);
        let err = Grep.call(&ctx, a).await.unwrap_err().to_string();
        assert!(err.contains("正则无法编译"), "{err}");
    }

    #[tokio::test]
    async fn caps_hits_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..50).map(|i| format!("needle {i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), body).unwrap();
        let mut a = args("needle", dir.path());
        a.max_hits = Some(5);
        let out = run(a).await;
        assert!(out.contains("命中数上限"), "{out}");
        assert_eq!(
            out.lines().filter(|l| l.contains("needle")).count(),
            5,
            "{out}"
        );
    }
}
