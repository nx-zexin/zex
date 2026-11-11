//! 工具契约。
//!
//! 设计要点（见 `docs/ARCHITECTURE.md` §5、ADR-004）：
//!
//! 1. **Schema 自动生成** —— `Args` 的字段文档注释直接成为给模型的参数说明，
//!    因此不可能与实现漂移。写注释就是在写给模型看的提示词。
//! 2. **`RISK` 编译期强制** —— 无默认值，漏标即编译失败。
//! 3. **静态分发 + 类型擦除** —— 工具用泛型实现，运行时通过 [`ErasedTool`] 统一调度。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::ids::SessionId;
use crate::risk::Risk;

/// 一个工具对模型暴露的元数据。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// 工具名（模型调用时使用）。
    pub name: &'static str,
    /// 给模型看的描述。应说明"什么时候该用"，而不只是"它做什么"。
    pub description: &'static str,
    /// 参数 JSON Schema，由 `schemars` 从 `Args` 生成。
    pub input_schema: serde_json::Value,
    /// 风险等级。
    pub risk: Risk,
}

/// 工具执行结果。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 回给模型的文本内容。
    pub content: String,
    /// 是否失败。失败时模型会看到错误并可能换方法。
    pub is_error: bool,
    /// 结构化附加信息（不发给模型，供 UI / 审计使用）。
    pub meta: Option<serde_json::Value>,
}

impl ToolOutput {
    /// 成功结果。
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
            meta: None,
        }
    }

    /// 失败结果。
    pub fn err(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
            meta: None,
        }
    }

    /// 附加结构化元信息。
    pub fn with_meta(mut self, meta: serde_json::Value) -> Self {
        self.meta = Some(meta);
        self
    }
}

/// 工具执行时能拿到的上下文。
///
/// 用 trait 而非具体 struct，让 `zex-tools` 的实现在 `zex-core` 之外注入
/// （工作目录解析、快照、事件发送都属于上层关注点）。
pub trait ToolCtx: Send + Sync {
    /// 当前工作目录。
    fn cwd(&self) -> &Path;

    /// 当前会话 id。
    fn session_id(&self) -> &SessionId;

    /// 把用户/模型给的路径解析为绝对路径。
    ///
    /// 实现方**必须**归一化（消解 `..`、展开 `~`、解析符号链接），
    /// 否则授权匹配可被 `../` 绕过（见 `docs/SECURITY.md` §3.2）。
    fn resolve_path(&self, raw: &str) -> Result<PathBuf>;

    /// 该路径是否命中密钥黑名单（`~/.zex/.env` 等）。
    ///
    /// 默认实现供测试使用；生产实现应读取真实配置。
    fn is_secret_path(&self, path: &Path) -> bool {
        path.file_name()
            .is_some_and(|n| n == ".env" || n == "id_rsa")
    }
}

/// 一个工具的实现契约。
///
/// 实现者只需声明 `NAME` / `RISK` / `Args` / `description` / `call`，
/// [`ErasedTool`] 由 blanket impl 自动提供。
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// 工具名。约定为 `snake_case`，与文件名一致。
    const NAME: &'static str;

    /// 风险等级。**无默认值** —— 漏标必须是编译错误。
    const RISK: Risk;

    /// 参数类型。字段的文档注释会成为给模型的参数说明。
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    /// 给模型看的描述。写"什么时候用"，不只是"做什么"。
    fn description() -> &'static str;

    /// 按**具体参数**判断风险等级。
    ///
    /// 默认取静态的 [`Self::RISK`]。只有参数会实质改变风险的工具有理由覆盖它——
    /// 典型是 shell：`ls` 和 `rm -rf` 都是"执行命令"，但风险天差地别。
    fn risk_for(&self, _args: &Self::Args) -> Risk {
        Self::RISK
    }

    /// 执行。
    async fn call(&self, cx: &dyn ToolCtx, args: Self::Args) -> Result<ToolOutput>;
}

/// 类型擦除后的工具，用于统一调度。
///
/// 不要手动实现 —— 任何 `T: Tool` 自动获得此实现。
#[async_trait]
pub trait ErasedTool: Send + Sync {
    /// 元数据（含自动生成的 schema）。
    fn spec(&self) -> ToolSpec;

    /// 按参数判断风险。参数解析失败时退回静态等级（保守）。
    fn risk_for(&self, args: &serde_json::Value) -> Risk {
        let _ = args;
        self.spec().risk
    }

    /// 用未解析的 JSON 调用。
    ///
    /// 参数反序列化失败时返回 [`Error::BadArgs`]，错误信息会回给模型让它自我纠正。
    async fn call_json(&self, cx: &dyn ToolCtx, args: serde_json::Value) -> Result<ToolOutput>;
}

#[async_trait]
impl<T: Tool> ErasedTool for T {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: T::NAME,
            description: T::description(),
            input_schema: schema_of::<T::Args>(),
            risk: T::RISK,
        }
    }

    fn risk_for(&self, args: &serde_json::Value) -> Risk {
        match serde_json::from_value::<T::Args>(args.clone()) {
            Ok(parsed) => Tool::risk_for(self, &parsed),
            // 参数都解析不了，按静态等级走 —— 宁可多问一句
            Err(_) => T::RISK,
        }
    }

    async fn call_json(&self, cx: &dyn ToolCtx, args: serde_json::Value) -> Result<ToolOutput> {
        let parsed: T::Args = serde_json::from_value(args).map_err(|e| Error::BadArgs {
            tool: T::NAME.to_string(),
            message: e.to_string(),
        })?;
        T::call(self, cx, parsed).await
    }
}

/// 从参数类型生成 JSON Schema。
///
/// `schemars` 会把字段文档注释写进 schema 的 `description`，这正是模型看到的提示词。
pub fn schema_of<A: JsonSchema>() -> serde_json::Value {
    let schema = schemars::schema_for!(A);
    serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({"type": "object"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    struct Echo;

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        /// 要回显的文本
        text: String,
    }

    #[async_trait]
    impl Tool for Echo {
        const NAME: &'static str = "echo";
        const RISK: Risk = Risk::ReadOnly;
        type Args = EchoArgs;

        fn description() -> &'static str {
            "回显一段文本"
        }

        async fn call(&self, _cx: &dyn ToolCtx, args: Self::Args) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(args.text))
        }
    }

    fn dummy_ctx() -> impl ToolCtx {
        struct Ctx;
        impl ToolCtx for Ctx {
            fn cwd(&self) -> &Path {
                Path::new("/tmp")
            }
            fn session_id(&self) -> &SessionId {
                static ID: std::sync::OnceLock<SessionId> = std::sync::OnceLock::new();
                ID.get_or_init(|| SessionId::new("ses_test"))
            }
            fn resolve_path(&self, raw: &str) -> Result<PathBuf> {
                Ok(PathBuf::from(raw))
            }
        }
        Ctx
    }

    #[tokio::test]
    async fn erased_tool_dispatches() {
        let tool = Echo;
        let erased: &dyn ErasedTool = &tool;
        let out = erased
            .call_json(&dummy_ctx(), serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out.content, "hi");
        assert!(!out.is_error);
    }

    #[test]
    fn schema_is_derived_from_args_with_doc_comments() {
        let spec = Echo.spec();
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.risk, Risk::ReadOnly);
        // 文档注释应出现在 schema 里 —— 这是"注释即提示词"的保证
        let text = spec.input_schema.to_string();
        assert!(
            text.contains("要回显的文本"),
            "schema 应包含字段文档: {text}"
        );
    }

    #[tokio::test]
    async fn bad_args_yields_tool_error_not_panic() {
        let tool = Echo;
        let erased: &dyn ErasedTool = &tool;
        let res = erased.call_json(&dummy_ctx(), serde_json::json!({})).await;
        match res {
            Err(Error::BadArgs { tool, .. }) => assert_eq!(tool, "echo"),
            other => panic!("期望 BadArgs，得到 {other:?}"),
        }
    }
}
