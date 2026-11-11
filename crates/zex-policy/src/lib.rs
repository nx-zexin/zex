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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// 正常：按风险分级。
    #[default]
    Normal,
    /// 计划模式：只读，任何 L1+ 一律拒绝。
    Plan,
}

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
    pub fn evaluate(&self, spec: &ToolSpec, args: &Value, mode: Mode) -> Verdict {
        // ─── 第 1 步：硬 Deny 表 ───
        // 不可配置、不可被 grant 覆盖、不可被"跳过确认"覆盖
        if let Some(reason) = rules::hard_deny(spec.name, args) {
            return Verdict::Deny { reason };
        }

        // ─── 第 2 步：模式约束 ───
        // 必须早于 grant 与风险上调，否则“上调后提前 return”会绕过它。
        if mode == Mode::Plan && spec.risk != Risk::ReadOnly {
            return Verdict::Deny {
                reason: "当前处于计划模式（只读），不允许执行会改变状态的操作".into(),
            };
        }

        // ─── 第 3 步：规则上调风险（只上调，不下调）───
        let effective = rules::escalate(spec.name, args, spec.risk);
        let escalated = effective > spec.risk;

        // ─── 第 4 步：grant 匹配（只对 L0–L2 有效，L3 永不匹配）───
        // 被规则上调的调用不查 grant："同类操作以前批准过"不能覆盖新出现的风险形态。
        if effective.grantable() && !escalated {
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

        if escalated {
            let bump = match default_verdict(effective) {
                Verdict::Ask {
                    grantable,
                    double_confirm,
                    ..
                } => (grantable, double_confirm),
                Verdict::Allow { .. } | Verdict::Deny { .. } => (false, false),
            };
            return Verdict::Ask {
                reason: rules::escalation_reason(spec.name, args)
                    .unwrap_or_else(|| format!("`{}` 本次参数命中风险规则，需要确认", spec.name)),
                grantable: bump.0,
                double_confirm: bump.1,
            };
        }

        default_verdict(effective)
    }
}

/// 静态风险 → 默认判定。
fn default_verdict(risk: Risk) -> Verdict {
    match risk {
        Risk::ReadOnly => Verdict::Allow {
            snapshot: false,
            by: ApprovedBy::Auto,
        },
        Risk::Reversible => Verdict::Allow {
            snapshot: true,
            by: ApprovedBy::Auto,
        },
        Risk::Impactful => Verdict::Ask {
            reason: "该操作会改变系统状态".into(),
            grantable: true,
            double_confirm: false,
        },
        Risk::Destructive => Verdict::Ask {
            reason: "该操作不可逆，且可能造成数据丢失".into(),
            grantable: false, // L3 永不可记忆
            double_confirm: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &'static str, risk: Risk) -> ToolSpec {
        ToolSpec {
            name,
            description: "",
            input_schema: serde_json::json!({}),
            risk,
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
            Mode::Normal,
        );
        assert!(matches!(v, Verdict::Ask { .. }), "应为 Ask，实际 {v:?}");
    }

    #[test]
    fn temp_dir_write_is_allowed() {
        // 反面：临时目录不该弹审批，否则正常干活会被打断
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/tmp/zex-probe.txt"}),
            Mode::Normal,
        );
        assert!(v.is_allow());
    }

    #[test]
    fn write_system_dir_is_denied() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/usr/local/bin/zex"}),
            Mode::Normal,
        );
        assert!(matches!(v, Verdict::Deny { .. }), "应为 Deny，实际 {v:?}");
    }

    #[test]
    fn read_only_is_allowed() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_read", Risk::ReadOnly),
            &serde_json::json!({}),
            Mode::Normal,
        );
        assert!(v.is_allow());
        assert!(!v.needs_snapshot());
    }

    #[test]
    fn reversible_is_allowed_with_snapshot() {
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({}),
            Mode::Normal,
        );
        assert!(v.is_allow());
        assert!(v.needs_snapshot());
    }

    #[test]
    fn impactful_asks() {
        let p = policy();
        let v = p.evaluate(
            &spec("shell", Risk::Impactful),
            &serde_json::json!({"command": "echo hi"}),
            Mode::Normal,
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
            Mode::Normal,
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
            Mode::Normal,
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
            Mode::Normal,
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
            Mode::Normal,
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
        );
        assert!(
            matches!(v, Verdict::Deny { .. }),
            "grant 不能突破 plan 模式，得到 {v:?}"
        );
    }

    #[test]
    fn tmp_paths_do_not_trigger_extra_approval() {
        // /tmp 是正常的工作位置，不该因为"不在 HOME 下"就要求审批
        let p = policy();
        let v = p.evaluate(
            &spec("fs_write", Risk::Reversible),
            &serde_json::json!({"path": "/tmp/x.txt"}),
            Mode::Normal,
        );
        assert!(v.is_allow(), "/tmp 不应被额外审批，得到 {v:?}");
    }

    #[test]
    fn approved_by_serializes_for_audit() {
        assert_eq!(ApprovedBy::Auto.as_audit_str(), "auto");
        assert_eq!(ApprovedBy::User.as_audit_str(), "user");
        assert_eq!(ApprovedBy::Grant(7).as_audit_str(), "grant:7");
    }
}
