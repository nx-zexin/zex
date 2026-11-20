//! 把脚本 API 的类型声明（`gpui-kit.d.ts`）落到技能目录里。
//!
//! # 解决什么问题
//!
//! 模型写扩展前要先查 API（`v_flex` 在哪个模块、`on_click` 收几个参数）。
//! 这份 API 清单只有一处是权威的：`gpui-shell` 生成的 `gpui-kit.d.ts`。
//! 但它过去**只存在于已经打开过的扩展目录里** —— 运行时是在首次挂载某个
//! 应用时顺手把声明写进去的。于是新装机器上什么扩展都没打开过时，
//! 模型手里一份 API 文档都没有，只能凭印象写，写出来的东西必然对不上。
//!
//! # 为什么生成而不是随二进制带一份
//!
//! 一份手抄的 `.d.ts` 会和运行时版本脱钩：gpui-shell 加了方法、改了参数，
//! 抄件还是旧的，模型照着抄件写，运行时报错却看不出为什么。这里直接问
//! 运行时要（`gpui_shell::type_declarations`），**声明永远等于当前运行时**，
//! 升级 gpui-shell 后下一次启动自动刷新，不需要谁记得同步。
//!
//! # 为什么放在技能目录
//!
//! 用户/模型的动线就是"读技能 → 照做"。技能文件在 `~/.zex/skills/`，
//! 把声明放同一个目录，`write-extension.md` 里就能指个相对位置说完；
//! 技能库只认 `*.md`，这个 `.d.ts` 不会被当成技能解析。

use std::path::{Path, PathBuf};

/// 与 `gpui-shell` 的常量对齐：编辑器按前缀找文件，这里沿用同一个名字。
pub const FILE_NAME: &str = "gpui-kit.d.ts";

/// 写一份声明到 `dir/gpui-kit.d.ts`，内容与当前运行时一致。
///
/// 只在内容变化时才落盘：每次启动都重写一遍，除了让文件时间戳乱跳没有
/// 任何好处（用户如果把它纳入 git，还会天天看到假 diff）。
///
/// 返回写到的路径；内容没变时也返回路径（文件已经是对的）。
pub fn write_api_declarations(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(FILE_NAME);
    let text = gpui_shell::type_declarations(&gpui_shell::FrozenComponentRegistry::default());

    // 内容一样就不碰：省一次写盘，也保住文件的修改时间
    if let Ok(existing) = std::fs::read_to_string(&path) {
        if existing == text {
            return Ok(path);
        }
    }

    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_runtime_declarations() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_api_declarations(dir.path()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // 三个模块各要有自己的声明块：少了任何一个，模型就会猜错 import 来源
        assert!(text.contains("declare module \"gpui-kit\""), "缺 gpui-kit 声明");
        assert!(text.contains("declare module \"gpui-base\""), "缺 gpui-base 声明");
        // 文件得足够大：两万多行的 API 清单，几百字节说明生成器没跑起来
        assert!(text.len() > 100_000, "声明文件太小，只有 {} 字节", text.len());
        // 关键 API 得在里面，否则模型查不到就还是会猜
        for needle in ["v_flex", "on_click", "read_from_clipboard", "View"] {
            assert!(text.contains(needle), "声明里找不到 `{needle}`");
        }
    }

    #[test]
    fn identical_content_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_api_declarations(dir.path()).unwrap();
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();

        // 第二次调用：内容一样，不该重新写盘
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_api_declarations(dir.path()).unwrap();
        let second = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first, second, "内容没变时不该重写文件");
    }

    #[test]
    fn overwrites_a_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "// 上一版留下的旧声明").unwrap();
        let path = write_api_declarations(dir.path()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("declare module"), "旧内容必须被覆盖");
        assert!(!text.contains("上一版留下的旧声明"));
    }
}
