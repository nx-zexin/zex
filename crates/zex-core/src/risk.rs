//! 风险分级与执行决策。
//!
//! 四档模型见 `docs/SECURITY.md` §2。**每个工具必须在编译期声明自己的
//! `Risk`**（`Tool::RISK` 无默认值），漏标会导致编译失败 —— 这是有意为之：
//! 默认值意味着"忘记标注的工具会被静默当作安全工具放行"。

use serde::{Deserialize, Serialize};

/// 工具的风险等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// L0：不改变任何状态。读文件、看屏幕、查系统信息、web 请求。
    ///
    /// 静默执行，记审计。
    ReadOnly,

    /// L1：可撤销的变更。写文件、建目录、装包、写记忆。
    ///
    /// 静默执行 + 自动快照，记审计。**这是默认档位。**
    Reversible,

    /// L2：有外部影响或难以撤销。执行任意命令、改系统配置、对外发消息、花钱。
    ///
    /// 首次审批，可"记住此类以后不问"。
    Impactful,

    /// L3：不可逆或高危。格式化、删系统目录、改引导、动凭据、提权。
    ///
    /// 强制逐次审批 + 二次确认，**不接受授权记忆**。
    Destructive,
}

impl Risk {
    /// 该等级是否允许被"记住授权"（写入 `grants` 表）。
    ///
    /// L3 永远为 `false` —— 高危操作必须每次确认。
    pub fn grantable(self) -> bool {
        !matches!(self, Risk::Destructive)
    }

    /// 是否需要人工审批（在无匹配授权的情况下）。
    pub fn needs_approval(self) -> bool {
        matches!(self, Risk::Impactful | Risk::Destructive)
    }

    /// 是否需要在执行前自动打快照。
    pub fn needs_snapshot(self) -> bool {
        matches!(self, Risk::Reversible)
    }

    /// 简短标签，用于 UI 与审计。
    pub fn label(self) -> &'static str {
        match self {
            Risk::ReadOnly => "只读",
            Risk::Reversible => "可逆",
            Risk::Impactful => "有影响",
            Risk::Destructive => "高危",
        }
    }
}

impl std::fmt::Display for Risk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// 权限层对一次工具调用的裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 放行。`snapshot` 为真时执行前先打快照。
    Allow {
        /// 是否需要在执行前快照。
        snapshot: bool,
    },
    /// 需要向用户请求审批。agent loop 应挂起等待。
    Ask {
        /// 命中的授权（若有），用于展示"这次为何还要问"。
        near_miss: Option<String>,
    },
    /// 拒绝执行。
    Deny {
        /// 拒绝原因，会作为工具结果回给模型。
        reason: String,
    },
}

impl Decision {
    /// 静默放行，不快照。
    pub fn allow() -> Self {
        Decision::Allow { snapshot: false }
    }

    /// 放行并要求快照。
    pub fn allow_with_snapshot() -> Self {
        Decision::Allow { snapshot: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l3_is_never_grantable() {
        assert!(!Risk::Destructive.grantable());
        assert!(Risk::Impactful.grantable());
        assert!(Risk::Reversible.grantable());
        assert!(Risk::ReadOnly.grantable());
    }

    #[test]
    fn risk_ordering_is_escalating() {
        // 依赖 derive(PartialOrd)：声明顺序即严重程度顺序
        assert!(Risk::ReadOnly < Risk::Reversible);
        assert!(Risk::Reversible < Risk::Impactful);
        assert!(Risk::Impactful < Risk::Destructive);
    }

    #[test]
    fn only_l2_l3_need_approval() {
        assert!(!Risk::ReadOnly.needs_approval());
        assert!(!Risk::Reversible.needs_approval());
        assert!(Risk::Impactful.needs_approval());
        assert!(Risk::Destructive.needs_approval());
    }

    #[test]
    fn only_l1_snapshots() {
        assert!(Risk::Reversible.needs_snapshot());
        assert!(!Risk::ReadOnly.needs_snapshot());
        assert!(!Risk::Impactful.needs_snapshot());
    }
}
