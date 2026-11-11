//! 命令只读性判定。
//!
//! **只此一份**。`zex-tools` 用它决定工具的静态风险，`zex-policy` 用它
//! 做风险上调 —— 两边判据不同的话，会出现"工具说是只读、策略层说不是"
//! 这种自相矛盾，审批弹窗就会莫名其妙地冒出来。
//!
//! 判定刻意保守：认不出来的一律当"可能改变状态"，宁可多问一句。
//! 这不是安全降级 —— `ls` 和删库都走同一个 `shell` 工具，但前者什么
//! 都不改。一律要审批的后果是弹窗泛滥，用户学会无脑点"批准"，更危险。

/// 断言不改状态的命令。
///
/// 只收"读了就完事"的：列目录、看文件、算校验和。任何可能写盘、
/// 联网发数据、启动服务的都不在列。
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "pwd", "echo",
    "which", "type", "file", "stat", "du", "df", "tree", "basename", "dirname",
    "realpath", "readlink", "whoami", "uname", "date", "sort", "uniq", "cut",
    "tr", "column", "nl", "md5sum", "sha256sum", "printenv", "locale", "ps",
    "free", "uptime", "hostname", "id", "groups",
];

/// 一旦出现就意味着可能在写、在串联、在求值。
///
/// 有这些符号就不当只读 —— 哪怕命令开头是 `ls`：加个重定向就已经在写盘了。
const WRITE_MARKERS: &[&str] = &[">", "<", "&", ";", "`", "$(", "${"];

/// 这条命令是否只读。
///
/// 管道按段判断：`ls | head` 是两段只读，`ls | xargs ...` 不是。
pub fn is_read_only(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    // 换行等于串联多条命令，别费劲解析
    if cmd.contains('\n') || WRITE_MARKERS.iter().any(|m| cmd.contains(m)) {
        return false;
    }
    cmd.split('|').all(|seg| {
        let seg = seg.trim();
        let first = match seg.split_whitespace().next() {
            Some(w) => w,
            None => return false,
        };
        if first == "sudo" || first == "doas" || first == "su" || first == "pkexec" {
            return false;
        }
        if first == "git" {
            return git_is_read_only(seg);
        }
        READ_ONLY_COMMANDS.contains(&first)
    })
}

/// `git` 只放行纯查询的子命令。
///
/// 删分支、切分支、暂存都会改工作区，所以只认白名单里那几个。
fn git_is_read_only(seg: &str) -> bool {
    const SAFE: &[&str] = &[
        "status", "log", "diff", "show", "rev-parse", "ls-files", "blame",
        "shortlog", "describe", "cat-file", "grep", "remote", "config",
    ];
    let mut words = seg.split_whitespace();
    let _ = words.next(); // 吃掉 "git"
    match words.next() {
        // `git config` 不带 --set 是读，但带参数就可能写；保守起见只放
        // 明确只读的那批，config 需要额外判断，这里直接排除更省事。
        Some(sub) => SAFE.contains(&sub) && sub != "config",
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_reads() {
        assert!(is_read_only("ls"));
        assert!(is_read_only("ls -la crates"));
        assert!(is_read_only("cat /etc/os-release"));
        assert!(is_read_only("grep -rn foo ."));
        assert!(is_read_only("find . -name '*.rs'"));
        assert!(is_read_only("git status"));
        assert!(is_read_only("git log --oneline -5"));
    }

    #[test]
    fn read_only_pipelines() {
        assert!(is_read_only("ls | head -20"));
        assert!(is_read_only("cat Cargo.toml | grep members"));
    }

    #[test]
    fn writes_are_not_read_only() {
        assert!(!is_read_only("ls > out.txt"));
        assert!(!is_read_only("echo hi >> log"));
        assert!(!is_read_only("cat a && cat b"));
        assert!(!is_read_only("ls; pwd"));
        assert!(!is_read_only("touch x"));
        assert!(!is_read_only("mkdir d"));
        assert!(!is_read_only("cargo build"));
        assert!(!is_read_only("sudo ls"));
    }

    #[test]
    fn git_writes_are_not_read_only() {
        assert!(is_read_only("git show HEAD"));
        assert!(!is_read_only("git branch -D feature"));
        assert!(!is_read_only("git checkout main"));
        assert!(!is_read_only("git"));
    }

    #[test]
    fn pipeline_ending_in_writer() {
        assert!(!is_read_only("cat list | xargs touch"));
        assert!(!is_read_only("find . | xargs rm"));
    }

    #[test]
    fn odd_input() {
        assert!(!is_read_only(""));
        assert!(!is_read_only("   "));
        assert!(!is_read_only("ls\npwd"));
    }
}
