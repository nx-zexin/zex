//! Zex 的权限层。
//!
//! `Risk`（工具的编译期属性）与 `Verdict`（单次调用的判定）是**两个正交维度**，
//! 见 `docs/SECURITY.md` §10。本 crate 负责后者。
//!
//! ## 四步叠加，顺序不可换
//!
//! ```text
//! 第 0 步  静态风险给默认判定
//! 第 1 步  硬 Deny 表（不可配置、不可被 grant 覆盖）
//! 第 2 步  具体规则（rm/argv 解析、受保护路径、工作区边界）→ 只上调，不下调
//! 第 3 步  grant 匹配（只对 L0–L2 有效，L3 永不匹配）
//! 第 4 步  模式约束（plan 模式把 L1+ 全变 Deny）
//! ```
//!
//! **核心不变量：任何规则都只能把风险往上调，不能往下调。**
//! Zebra 的 `Config.Tools { bash: "allow" }` 违反这条，是唯一不能复刻的设计。

#![forbid(unsafe_code)]
// 测试代码允许 unwrap/expect：断言失败就该 panic，生产代码里才禁止。
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod glob;
pub mod rules;
pub mod secret;

use serde_json::Value;
use std::path::Path;
use zex_core::{Risk, ToolSpec};
use zex_store::Store;

/// 谁批准了这次调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovedBy {
    /// 静态风险即为 L0/L1，无需审批。
    Auto,
    /// 用户当场点了批准。
    User,
    /// 命中了某条授权记忆。
    Grant(i64),
}

impl ApprovedBy {
    /// 审计里记录的形式。
    pub fn as_audit_str(&self) -> String {
        match self {
            ApprovedBy::Auto => "auto".to_string(),
            ApprovedBy::User => "user".to_string(),
            ApprovedBy::Grant(id) => format!("grant:{id}"),
        }
    }
}

/// 权限层对一次工具调用的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 放行。
    Allow {
        /// 执行前是否打快照。
        snapshot: bool,
        /// 依据。
        by: ApprovedBy,
    },
    /// 需要人工审批。
    Ask {
        /// 为什么需要问 —— 会展示在审批弹窗里。
        reason: String,
        /// 能否被"记住"。
        grantable: bool,
        /// 是否需要二次确认（L3）。
        double_confirm: bool,
    },
    /// 拒绝。
    Deny {
        /// 原因，会作为工具结果回给模型。
        reason: String,
    },
}

impl Verdict {
    /// 是否直接放行。
    pub fn is_allow(&self) -> bool {
        matches!(self, Verdict::Allow { .. })
    }

    /// 是否需要打快照。
    pub fn needs_snapshot(&self) -> bool {
        matches!(self, Verdict::Allow { snapshot: true, .. })
    }
}

/// 运行模式。
///
/// 定义在 `zex_core`：UI、命令、内核都要用它，放在这里会逼 core 反向依赖。
/// 这个名字保留成别名，policy 内部读起来仍然自然。
pub use zex_core::RunMode as Mode;

/// 权限判定器。
#[derive(Debug, Clone)]
pub struct Policy {
    store: Store,
}

impl Policy {
    /// 绑定到存储层（授权记忆在库里）。
    pub fn new(store: Store) -> Self {
        Policy { store }
    }

    /// 判定一次工具调用。
    ///
    /// 顺序：硬 Deny → 模式约束 → 静态风险 → 规则上调 → grant。
    ///
    /// 模式约束放在靠前的位置，是因为它是**全局约定**：
    /// 用户说"现在只看"，那就不该有任何写操作被放出去 ——
    /// 包括那些本来会被 grant 放行的。
    pub fn evaluate(&self, spec: &ToolSpec, args: &Value, mode: Mode, cwd: &Path) -> Verdict {
        // ─── 第 1 步：硬 Deny 表 ───
        // 不可配置、不可被 grant 覆盖、不可被"跳过确认"覆盖
        if let Some(reason) = rules::hard_deny(spec.name, args, cwd) {
            return Verdict::Deny { reason };
        }

        // ─── 第 2 步：模式约束 ───
        // 必须早于 grant 与风险上调，否则“上调后提前 return”会绕过它。
        if mode == Mode::Plan && spec.risk != Risk::ReadOnly {
            return Verdict::Deny {
                reason: "计划模式下只读不写：先把方案说清楚，再切模式动手".into(),
            };
        }

        // ─── 第 3 步：规则上调风险（只上调，不下调）───
        let esc = rules::escalate(spec.name, args, spec.risk, cwd);
        let effective = esc.risk;

        // ─── 第 4 步：grant 匹配（只对 L0–L2 有效，L3 永不匹配）───
        // 被规则上调的调用不查 grant："同类操作以前批准过"不能覆盖新出现的风险形态。
        if esc.bump.is_none() && effective.grantable() {
            if let Ok(grants) = self.store.active_grants_for(spec.name) {
                for g in grants {
                    if glob::matches(&g.pattern, args) {
                        return Verdict::Allow {
                            snapshot: effective.needs_snapshot(),
                            by: ApprovedBy::Grant(g.id),
                        };
                    }
                }
            }
        }

        // 上调分两种，处理不一样：
        //
        // - **Dangerous**（sudo / rm -rf / / curl|sh / 强制推送）：任何模式都要问，
        //   「完全访问」放宽的是常规命令，不是换了张脸的高危形态。
        // - **Location**（只是不在家目录下）：交给下面的模式矩阵。「完全访问」
        //   就该放行 —— 用户在 `D:\proj` 或 `/srv/app` 里干活是常态，
        //   为“路径不在 HOME 下”每次都弹审批，等于把这个模式废了。
        if esc.bump == Some(rules::Bump::Dangerous) {
            let bump = match mode_verdict(Mode::Confirm, effective) {
                Verdict::Ask {
                    grantable,
                    double_confirm,
                    ..
                } => (grantable, double_confirm),
                Verdict::Allow { .. } | Verdict::Deny { .. } => (false, false),
            };
            return Verdict::Ask {
                reason: rules::escalation_reason(spec.name, args, cwd)
                    .unwrap_or_else(|| format!("`{}` 本次参数命中风险规则，需要确认", spec.name)),
                grantable: bump.0,
                double_confirm: bump.1,
            };
        }

        mode_verdict(mode, effective)
    }
}

/// 这个模式下，这个风险等级会不会被**自动放行**。
///
/// 给 UI 用的：用户切模式时，界面上正挂着一条审批 —— 如果新模式本来
/// 就会放行它，那条请求就不该继续等他点（否则"切了还在问"）。
///
/// 单一事实来源仍是 [`mode_verdict`]：这里只是把它的结论摊平成布尔，
/// 免得 UI 自己抄一份矩阵、抄漏一格就变成"完全访问还问 L2"这种毛病。
pub fn auto_allows(mode: Mode, risk: Risk) -> bool {
    matches!(mode_verdict(mode, risk), Verdict::Allow { .. })
}

/// 运行模式 × 静态风险 → 默认判定。
///
/// 两条不随模式变化的底线：
/// - **L0 永远放行**：读不需要许可，任何模式都不该把只读拦下来；
/// - **L3 永远逐次确认**：不可逆的事没有"自动"可言，
///   "完全访问"减的是确认次数，不是把不可逆变成可逆。
///
/// 中间两档才是模式真正起作用的地方：
///
/// | 模式 | L1 可逆（写文件） | L2 有影响（跑命令） |
/// |------|------------------|--------------------|
/// | 变更前确认 | 问 | 问 |
/// | 自动编辑 | 自动 + 快照 | 问 |
/// | 完全访问 | 自动 + 快照 | 自动 + 快照 |
fn mode_verdict(mode: Mode, risk: Risk) -> Verdict {
    if risk == Risk::ReadOnly {
        return Verdict::Allow {
            snapshot: false,
            by: ApprovedBy::Auto,
        };
    }
    if risk == Risk::Destructive {
        return Verdict::Ask {
            reason: "该操作不可逆，且可能造成数据丢失".into(),
            grantable: false, // L3 永不可记忆
            double_confirm: true,
        };
    }
    // Plan 已在 evaluate 里提前拒绝，这里只剩 Reversible / Impactful
    match (mode, risk) {
        (Mode::Confirm, Risk::Reversible) => Verdict::Ask {
            reason: "这个操作会修改文件".into(),
            grantable: true,
            double_confirm: false,
        },
        (Mode::Confirm, _) => Verdict::Ask {
            reason: "该操作会改变系统状态".into(),
            grantable: true,
            double_confirm: false,
        },
        (Mode::AutoEdit, Risk::Reversible) => Verdict::Allow {
            snapshot: true,
            by: ApprovedBy::Auto,
        },
        (Mode::AutoEdit, _) => Verdict::Ask {
            reason: "该操作会改变系统状态".into(),
            grantable: true,
            double_confirm: false,
        },
        (Mode::FullAccess, _) => Verdict::Allow {
            snapshot: true,
            by: ApprovedBy::Auto,
        },
        // Plan 走不到这里；真走到了按最保守处理
        (Mode::Plan, _) => Verdict::Deny {
            reason: "计划模式下只读不写：先把方案说清楚，再切模式动手".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 测试用的工作目录：**真实家目录下的子目录**。
    ///
    /// 不能写死一个假路径（如 `/home/u/proj`）：那样相对路径解析出来的
    /// 绝对位置本来就在家目录之外，权限层上调风险反而是**正确**的，
    /// 测试就钉不住我们真正想守的规则。
    fn cwd() -> PathBuf {
        home()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("zex-proj")
    }

    /// 家目录；极端环境拿不到时退回 /tmp。
    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    }

    fn spec(name: &'static str, risk: Risk) -> ToolSpec {
        ToolSpec {
            name,
            description: "",
            input_schema: serde_json::json!({}),
            risk,
            hint: None,
        }
    }

    fn policy() -> Policy {
        Policy::new(Store::in_memory().unwrap())
    }

    #[test]
    fn write_outside_home_escalates_to_ask() {
        // /tmp 是公认的工作位置会被放行，但 /srv 这类"home 外、又不在
        // 系统目录"的路径必须问一句
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/srv/zex-probe.txt"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Ask { .. }), "应为 Ask，实际 {v:?}");
    }

    #[test]
    fn full_access_allows_writing_a_relative_path() {
        // 回归：模型给的是 `src/main.rs` 这种相对路径，权限层拿它和家目录
        // 直接比 → "看起来在外面" → 风险上调 → 「完全访问」也要批准。
        // 用户看到的就是"我明明选了完全访问，写个文件还弹窗"。
        let p = policy();
        for path in ["src/main.rs", "./a.txt", "~/proj/a.txt", "a/../b.txt"] {
            let v = p.evaluate(
                &spec("fs_write", Risk::Reversible),
                &serde_json::json!({"path": path, "content": "x"}),
                Mode::FullAccess,
                &cwd(),
            );
            assert!(v.is_allow(), "`{path}` 在完全访问下应放行，得到 {v:?}");
        }
    }

    #[test]
    fn auto_edit_allows_relative_path_writes() {
        // 同一类误判也让「自动编辑」失效：写个相对路径也变成"有影响"要问。
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "src/lib.rs", "content": "x"}),
            Mode::AutoEdit,
            &cwd(),
        );
        assert!(
            v.is_allow(),
            "自动编辑下写工作目录内的文件应放行，得到 {v:?}"
        );
    }

    #[test]
    fn full_access_still_asks_for_a_dangerous_command() {
        // 另一头不能松：「完全访问」豁免的是"位置"，不是"危险形态"。
        // `git push --force` 不在硬拒绝表里，但属于危险形态 —— 任何模式都问。
        let p = policy();
        let v = p.evaluate(
            &spec("shell", Risk::Impactful),
            &serde_json::json!({"command": "git push --force origin main"}),
            Mode::FullAccess,
            &cwd(),
        );
        assert!(
            matches!(v, Verdict::Ask { .. }),
            "强制推送在完全访问下也要问，得到 {v:?}"
        );
    }

    #[test]
    fn confirm_mode_still_asks_for_a_relative_path_write() {
        // 位置不再"升级"，但该问的还是要问：Confirm 下 L1 本来就问。
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "src/main.rs", "content": "x"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Ask { .. }), "得到 {v:?}");
    }

    #[test]
    fn temp_dir_write_is_allowed() {
        // 临时目录是正常工作位置：自动编辑下照常放行，不该因为"在临时目录"
        // 被额外升成强确认。
        //
        // 路径用平台的临时目录：写死 `/tmp` 在 Windows 上不是绝对路径
        // （没有盘符），会被拼到 cwd 下面，于是这条测试测的就不是临时目录了。
        let probe = std::env::temp_dir().join("zex-probe.txt");
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": probe.to_string_lossy()}),
            Mode::AutoEdit,
            &cwd(),
        );
        assert!(v.is_allow(), "得到 {v:?}");
    }

    #[test]
    fn write_system_dir_is_denied() {
        // 同上：Windows 的 `C:\Windows` 在 SYSTEM_DIRS 里，`/usr` 不在它那儿。
        let target = if cfg!(windows) {
            r"C:\Windows\System32\zex.exe"
        } else {
            "/usr/local/bin/zex"
        };
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": target}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Deny { .. }), "应为 Deny，实际 {v:?}");
    }

    #[test]
    fn read_only_is_allowed() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_read", Risk::ReadOnly),
            &serde_json::json!({}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(v.is_allow());
        assert!(!v.needs_snapshot());
    }

    #[test]
    fn reversible_is_allowed_with_snapshot_under_auto_edit() {
        // 「自动编辑」的全部意义：写文件不再问，但仍然留快照
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({}),
            Mode::AutoEdit,
            &cwd(),
        );
        assert!(v.is_allow());
        assert!(v.needs_snapshot());
    }

    #[test]
    fn reversible_asks_under_confirm() {
        // 「变更前确认」：改文件之前先问
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Ask { .. }), "得到 {v:?}");
    }

    #[test]
    fn mode_matrix_over_risk_levels() {
        // 四档模式 × 四档风险的放行表。改动判定逻辑时先看这张表。
        let p = policy();
        // L0：任何模式都放行
        for m in Mode::all() {
            let v = p.evaluate(
                &spec("fs_read", Risk::ReadOnly),
                &serde_json::json!({}),
                m,
                &cwd(),
            );
            assert!(v.is_allow(), "{m:?} 下 L0 应放行，得到 {v:?}");
        }
        // L1：只有 Confirm 会问
        for m in Mode::all() {
            let v = p.evaluate(
                &spec("fs_write", Risk::Reversible),
                &serde_json::json!({}),
                m,
                &cwd(),
            );
            match m {
                Mode::Confirm => assert!(matches!(v, Verdict::Ask { .. }), "{m:?}"),
                Mode::Plan => assert!(matches!(v, Verdict::Deny { .. }), "{m:?}"),
                _ => assert!(v.is_allow(), "{m:?} 下 L1 应放行，得到 {v:?}"),
            }
        }
        // L2：Confirm / AutoEdit 都问，只有 FullAccess 放行
        for m in Mode::all() {
            let v = p.evaluate(
                &spec("shell", Risk::Impactful),
                &serde_json::json!({"command": "echo hi"}),
                m,
                &cwd(),
            );
            match m {
                Mode::FullAccess => assert!(v.is_allow(), "完全访问下 L2 应放行，得到 {v:?}"),
                Mode::Plan => assert!(matches!(v, Verdict::Deny { .. }), "{m:?}"),
                _ => assert!(
                    matches!(v, Verdict::Ask { .. }),
                    "{m:?} 下 L2 应问，得到 {v:?}"
                ),
            }
        }
        // 切模式时 UI 靠这个决定"正挂着的那条要不要自动放行"：
        // 它必须和 mode_verdict 说到一起去
        assert!(auto_allows(Mode::FullAccess, Risk::Impactful));
        assert!(auto_allows(Mode::AutoEdit, Risk::Reversible));
        assert!(!auto_allows(Mode::AutoEdit, Risk::Impactful));
        assert!(!auto_allows(Mode::Confirm, Risk::Reversible));
        assert!(
            !auto_allows(Mode::FullAccess, Risk::Destructive),
            "完全访问也不放行不可逆"
        );
        assert!(auto_allows(Mode::Confirm, Risk::ReadOnly), "只读不需要许可");

        // L3：除 Plan(直接拒) 外一律强确认，FullAccess 也不例外
        for m in Mode::all() {
            let v = p.evaluate(
                &spec("shell", Risk::Destructive),
                &serde_json::json!({"command": "true"}),
                m,
                &cwd(),
            );
            match m {
                Mode::Plan => assert!(matches!(v, Verdict::Deny { .. }), "{m:?}"),
                _ => assert!(
                    matches!(
                        v,
                        Verdict::Ask {
                            double_confirm: true,
                            grantable: false,
                            ..
                        }
                    ),
                    "{m:?} 下 L3 必须强确认，得到 {v:?}"
                ),
            }
        }
    }

    #[test]
    fn impactful_asks() {
        let p = policy();
        let v = p.evaluate(
            &spec("shell", Risk::Impactful),
            &serde_json::json!({"command": "echo hi"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(
            v,
            Verdict::Ask {
                grantable: true,
                double_confirm: false,
                ..
            }
        ));
    }

    #[test]
    fn destructive_asks_with_double_confirm_and_no_grant() {
        let p = policy();
        let v = p.evaluate(
            &spec("shell", Risk::Destructive),
            &serde_json::json!({"command": "mkfs.ext4 /dev/sda"}),
            Mode::Confirm,
            &cwd(),
        );
        match v {
            Verdict::Deny { .. } => {} // 也可能被硬 Deny 拦掉
            Verdict::Ask {
                grantable,
                double_confirm,
                ..
            } => {
                assert!(!grantable, "L3 永不可记忆");
                assert!(double_confirm);
            }
            other => panic!("期望 Ask 或 Deny，得到 {other:?}"),
        }
    }

    #[test]
    fn plan_mode_denies_writes() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({}),
            Mode::Plan,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn plan_mode_allows_reads() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_read", Risk::ReadOnly),
            &serde_json::json!({}),
            Mode::Plan,
            &cwd(),
        );
        assert!(v.is_allow());
    }

    #[test]
    fn grant_allows_matching_args() {
        let store = Store::in_memory().unwrap();
        store
            .add_grant("shell", "git *", Risk::Impactful, None)
            .unwrap();
        let p = Policy::new(store);
        let v = p.evaluate(
            &spec("shell", Risk::Impactful),
            &serde_json::json!({"command": "git status"}),
            Mode::Confirm,
            &cwd(),
        );
        match v {
            Verdict::Allow {
                by: ApprovedBy::Grant(_),
                ..
            } => {}
            other => panic!("期望被 grant 放行，得到 {other:?}"),
        }
    }

    #[test]
    fn grant_does_not_match_other_args() {
        let store = Store::in_memory().unwrap();
        store
            .add_grant("shell", "git *", Risk::Impactful, None)
            .unwrap();
        let p = Policy::new(store);
        let v = p.evaluate(
            &spec("shell", Risk::Impactful),
            &serde_json::json!({"command": "rm -rf build"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(matches!(v, Verdict::Ask { .. }));
    }

    #[test]
    fn grant_cannot_bypass_hard_deny() {
        let store = Store::in_memory().unwrap();
        // 即使授了全权
        store
            .add_grant("fs_write", "*", Risk::Reversible, None)
            .unwrap();
        let p = Policy::new(store);
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/home/u/.zex/.env", "content": "x"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(
            matches!(v, Verdict::Deny { .. }),
            "硬 Deny 不可被 grant 覆盖，得到 {v:?}"
        );
    }

    #[test]
    fn plan_mode_cannot_be_bypassed_by_escalation() {
        // 回归测试：曾经 escalate 分支提前 return，把 plan 模式绕过去了。
        // /srv 在主目录外 → 会被上调到 Impactful，但 plan 模式必须仍然拒绝。
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/srv/data/x.txt"}),
            Mode::Plan,
            &cwd(),
        );
        assert!(
            matches!(v, Verdict::Deny { .. }),
            "plan 模式必须优先于风险上调，得到 {v:?}"
        );
    }

    #[test]
    fn plan_mode_cannot_be_bypassed_by_grant() {
        // 即使授了全权，plan 模式仍然拒绝写
        let store = Store::in_memory().unwrap();
        store
            .add_grant("fs_write", "*", Risk::Reversible, None)
            .unwrap();
        let p = Policy::new(store);
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "a.txt"}),
            Mode::Plan,
            &cwd(),
        );
        assert!(
            matches!(v, Verdict::Deny { .. }),
            "grant 不能突破 plan 模式，得到 {v:?}"
        );
    }

    #[test]
    fn tmp_paths_do_not_trigger_extra_approval() {
        // /tmp 是正常工作位置：确认模式下会问（和 HOME 下一样），
        // 但不该因为"不在 HOME 下"被升级成二次确认
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/tmp/x.txt"}),
            Mode::Confirm,
            &cwd(),
        );
        assert!(
            matches!(
                v,
                Verdict::Ask {
                    double_confirm: false,
                    ..
                }
            ),
            "/tmp 不应被额外加码，得到 {v:?}"
        );
    }

    #[test]
    fn approved_by_serializes_for_audit() {
        assert_eq!(ApprovedBy::Auto.as_audit_str(), "auto");
        assert_eq!(ApprovedBy::User.as_audit_str(), "user");
        assert_eq!(ApprovedBy::Grant(7).as_audit_str(), "grant:7");
    }
}
