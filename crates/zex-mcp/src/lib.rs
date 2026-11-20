//! MCP（Model Context Protocol）客户端。
//!
//! 只实现 stdio 传输 —— 它是目前生态里最常用的一种，而且不需要
//! 额外的网络和鉴权设施。HTTP/SSE 传输留到真有需求时再加。
//!
//! # 协议要点
//!
//! MCP 是一层 JSON-RPC 2.0：子进程收 `\n` 分隔的消息，回同样格式。
//! 握手三步：
//!
//! 1. `initialize` 报上自己的协议版本和客户端信息
//! 2. 收到 server 的能力清单
//! 3. 发 `notifications/initialized` 通知（无 id，不期待回复）
//!
//! 之后 `tools/list` 拿工具清单，`tools/call` 执行。
//!
//! # 为什么单独一个进程
//!
//! 每个 MCP server 是一个独立子进程：它崩了不会带走 Zex，
//! 它需要的运行环境（node/python/…）也不用和 Zex 一致。

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// MCP 客户端使用的协议版本。
///
/// 服务端可能回一个更新的版本，我们按自己声明的能力行事即可 ——
/// 协议保证向后兼容。
const PROTOCOL_VERSION: &str = "2024-11-05";

/// MCP 相关的错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 子进程启动失败。
    #[error("启动 MCP 服务失败: {0}")]
    Spawn(String),
    /// 收发消息失败。
    #[error("MCP 通信失败: {0}")]
    Io(String),
    /// 服务端返回了 JSON-RPC 错误。
    #[error("MCP 服务返回错误 {code}: {message}")]
    Rpc {
        /// JSON-RPC 错误码。
        code: i64,
        /// 错误说明。
        message: String,
    },
    /// 响应格式不符合预期。
    #[error("MCP 响应格式异常: {0}")]
    Protocol(String),
    /// 服务不是可用状态。
    #[error("MCP 服务不可用: {0}")]
    Unavailable(String),
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// MCP 的 I/O 宿主。
///
/// `tokio::process` 的句柄绑在创建它的 runtime 上，跨 runtime 用会崩
/// （"there is no reactor running"）。而调用方可能是 GPUI 的 executor，
/// 根本不是 tokio。所以 MCP 独占一个 runtime：连接和通讯都在它上面跑，
/// 外部只通过 `call` 投递任务、等结果。
pub struct McpHub {
    rt: tokio::runtime::Runtime,
}

impl McpHub {
    /// 建一个专用 runtime（2 个 worker 足够：MCP 请求是串行来的）。
    pub fn new() -> Result<McpHub> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("zex-mcp")
            .build()
            .map_err(|e| Error::Spawn(format!("建 MCP runtime 失败: {e}")))?;
        Ok(McpHub { rt })
    }

    /// 在 MCP runtime 上执行一段异步代码并等结果。
    pub fn run<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.rt.block_on(fut)
    }

    /// 把一次工具调用投递到 MCP runtime，返回可 await 的结果。
    ///
    /// 这样调用方在任意 runtime 里都能用：真正的 IO 发生在 MCP runtime。
    pub async fn call(
        &self,
        client: Arc<McpClient>,
        tool: String,
        args: serde_json::Value,
    ) -> Result<CallResult> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.rt.spawn(async move {
            let r = client.call_tool(&tool, args).await;
            let _ = tx.send(r);
        });
        rx.await.map_err(|e| Error::Io(e.to_string()))?
    }
}

/// 一个 MCP 服务端的配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// 可执行文件（`npx`、`python`、绝对路径都行）。
    pub command: String,
    /// 传给它的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 额外的环境变量。
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// 服务端声明的一个工具。
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolInfo {
    /// 工具名（服务端命名空间内唯一）。
    pub name: String,
    /// 给模型看的描述。
    #[serde(default)]
    pub description: String,
    /// 参数 JSON Schema。
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

/// `tools/call` 的结果。
#[derive(Debug, Clone)]
pub struct CallResult {
    /// 拼好的文本内容。
    pub text: String,
    /// 服务端是否标记为错误。
    pub is_error: bool,
}

/// 与一个 MCP 服务端的连接。
pub struct McpClient {
    name: String,
    stdin: Mutex<ChildStdin>,
    /// 读响应时要用；和 `stdin` 分开锁，避免读写互相阻塞。
    stdout: Mutex<BufReader<ChildStdout>>,
    child: Mutex<Child>,
    next_id: AtomicU64,
    /// 依次分配 id，但响应必须按 id 匹配 —— 见 `request` 的说明。
    serial: Mutex<()>,
}

impl McpClient {
    /// 启动子进程并完成握手。
    pub fn spawn(name: &str, cfg: &ServerConfig) -> Result<Arc<McpClient>> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr 留在父进程：服务端的日志不该吞掉。
            // 但如果没人读，管道写满会让子进程卡死，所以用 inherit。
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Spawn(format!("{}: {e}", cfg.command)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Spawn("拿不到 stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Spawn("拿不到 stdout".into()))?;

        let client = Arc::new(McpClient {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(child),
            next_id: AtomicU64::new(1),
            serial: Mutex::new(()),
        });

        Ok(client)
    }

    /// 服务名（配置里的键）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 发 `initialize` + `notifications/initialized`。
    pub async fn initialize(&self) -> Result<serde_json::Value> {
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "zex", "version": env!("CARGO_PKG_VERSION") }
        });
        let result = self.request("initialize", params).await?;

        // 通知：没有 id，也不期待响应
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.send_raw(&note).await?;
        Ok(result)
    }

    /// 列出服务端提供的工具。
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| Error::Protocol("tools/list 没有返回 tools 数组".into()))?;
        let mut out = Vec::new();
        for t in tools {
            match serde_json::from_value::<McpToolInfo>(t.clone()) {
                Ok(info) => out.push(info),
                Err(e) => tracing_warn(&format!("跳过格式异常的工具定义: {e}")),
            }
        }
        Ok(out)
    }

    /// 调用一个工具。
    pub async fn call_tool(&self, tool: &str, args: serde_json::Value) -> Result<CallResult> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": args }),
            )
            .await?;

        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // content 是 [{type:"text", text:"..."}] 形式的数组
        let mut text = String::new();
        if let Some(items) = result.get("content").and_then(|c| c.as_array()) {
            for item in items {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some(other) => {
                        // 图片/资源等类型暂不支持，明确说出来而不是静默丢弃
                        text.push_str(&format!("[{} 类型的内容暂不支持显示]", other));
                    }
                    None => {}
                }
            }
        }
        Ok(CallResult { text, is_error })
    }

    /// 发一个请求并等它的响应。
    async fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        // 串行化「发送 + 等响应」：MCP 允许并发请求（靠 id 区分），
        // 但我们的读循环是单线程的，并发下容易把别人的响应读走。
        // 工具调用本来就是一条一条来的，串行几乎不损失什么。
        let _guard = self.serial.lock().await;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_raw(&msg).await?;

        // 读到匹配 id 的消息为止；中间的服务端通知直接丢弃
        let mut stdout = self.stdout.lock().await;
        loop {
            let mut line = String::new();
            let n = stdout
                .read_line(&mut line)
                .await
                .map_err(|e| Error::Io(e.to_string()))?;
            if n == 0 {
                return Err(Error::Unavailable(format!(
                    "{} 在等待 {method} 响应时退出",
                    self.name
                )));
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                // 服务端往 stdout 打日志很常见，跳过而不是报错
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(Error::Rpc {
                    code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                    message: err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("未知错误")
                        .to_string(),
                });
            }
            return Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }

    async fn send_raw(&self, msg: &serde_json::Value) -> Result<()> {
        let mut line = serde_json::to_string(msg).map_err(|e| Error::Protocol(e.to_string()))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
        stdin.flush().await.map_err(|e| Error::Io(e.to_string()))?;
        Ok(())
    }

    /// 关掉子进程。
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

fn tracing_warn(msg: &str) {
    eprintln!("[mcp] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_defaults() {
        let cfg: ServerConfig = serde_json::from_str(r#"{"command":"npx"}"#).expect("应能解析");
        assert_eq!(cfg.command, "npx");
        assert!(cfg.args.is_empty());
        assert!(cfg.env.is_empty());
    }

    #[test]
    fn tool_info_parses_camel_case_schema() {
        let v = serde_json::json!({
            "name": "read_file",
            "description": "读一个文件",
            "inputSchema": {"type": "object", "properties": {}}
        });
        let info: McpToolInfo = serde_json::from_value(v).expect("应能解析");
        assert_eq!(info.name, "read_file");
        assert_eq!(info.input_schema["type"], "object");
    }
}

// ─── 工具适配 ───

/// 把 MCP 工具包成 Zex 的 `ErasedTool`。
///
/// `ToolSpec.name` 是 `&'static str`，而 MCP 工具名在运行时才知道。
/// 这里把名字泄漏成 `'static`：工具数量有限、进程内不变，
/// 泄漏的字节数可以忽略。换成 `String` 得改 `ToolSpec` 及其所有调用点，
/// 代价远大于收益。
pub struct McpTool {
    hub: Arc<McpHub>,
    client: Arc<McpClient>,
    /// 暴露给模型的名字：`mcp__<server>__<tool>`，见 `qualified_name`。
    name: &'static str,
    description: &'static str,
    input_schema: serde_json::Value,
    /// 服务端原始工具名（调用时要原样传回去）。
    remote_name: String,
}

/// 拼接对外的工具名。
///
/// 加前缀的原因：不同 MCP 服务端很可能都有叫 `read_file` 的工具，
/// 直接并进工具集会撞名。
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

impl McpTool {
    /// 由服务端工具信息构造。
    pub fn new(
        hub: Arc<McpHub>,
        client: Arc<McpClient>,
        server: &str,
        info: &McpToolInfo,
    ) -> McpTool {
        let full = qualified_name(server, &info.name);
        let desc = if info.description.is_empty() {
            format!("来自 MCP 服务 `{server}` 的工具 `{}`", info.name)
        } else {
            format!("{}（MCP 服务 `{server}`）", info.description)
        };
        McpTool {
            hub,
            client,
            name: Box::leak(full.into_boxed_str()),
            description: Box::leak(desc.into_boxed_str()),
            input_schema: if info.input_schema.is_null() {
                serde_json::json!({"type": "object", "properties": {}})
            } else {
                info.input_schema.clone()
            },
            remote_name: info.name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl zex_core::tool::ErasedTool for McpTool {
    fn spec(&self) -> zex_core::tool::ToolSpec {
        zex_core::tool::ToolSpec {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema.clone(),
            // MCP 工具干什么的我们不知道 —— 可能是读文件，也可能是删库。
            // 按 L2（有外部影响）处理，让它走审批而不是默认放行。
            risk: zex_core::Risk::Impactful,
        }
    }

    async fn call_json(
        &self,
        _cx: &dyn zex_core::tool::ToolCtx,
        args: serde_json::Value,
    ) -> zex_core::Result<zex_core::tool::ToolOutput> {
        let res = self
            .hub
            .call(self.client.clone(), self.remote_name.clone(), args)
            .await
            .map_err(|e| zex_core::Error::ToolFailed {
                tool: self.name.to_string(),
                message: e.to_string(),
            })?;
        Ok(if res.is_error {
            zex_core::tool::ToolOutput::err(res.text)
        } else {
            zex_core::tool::ToolOutput::ok(res.text)
        })
    }
}

/// 连上配置里所有服务端，收集它们的工具。
///
/// 单个服务失败不影响其它：某个 MCP 服务装坏了不该让整个应用起不来。
pub fn connect_all(
    configs: &std::collections::BTreeMap<String, ServerConfig>,
) -> (
    Vec<Arc<dyn zex_core::tool::ErasedTool>>,
    Vec<ServerStatus>,
    Arc<McpHub>,
) {
    let hub = match McpHub::new() {
        Ok(h) => Arc::new(h),
        Err(e) => {
            return (
                Vec::new(),
                vec![ServerStatus {
                    name: "-".into(),
                    ok: false,
                    detail: e.to_string(),
                    tool_count: 0,
                }],
                Arc::new(McpHub::new().expect("MCP runtime 建不起来")),
            );
        }
    };
    let (tools, status) = hub.run(connect_inner(configs, hub.clone()));
    (tools, status, hub)
}

async fn connect_inner(
    configs: &std::collections::BTreeMap<String, ServerConfig>,
    hub: Arc<McpHub>,
) -> (Vec<Arc<dyn zex_core::tool::ErasedTool>>, Vec<ServerStatus>) {
    let mut tools: Vec<Arc<dyn zex_core::tool::ErasedTool>> = Vec::new();
    let mut status = Vec::new();

    for (name, cfg) in configs {
        let client = match McpClient::spawn(name, cfg) {
            Ok(c) => c,
            Err(e) => {
                status.push(ServerStatus {
                    name: name.clone(),
                    ok: false,
                    detail: e.to_string(),
                    tool_count: 0,
                });
                continue;
            }
        };

        if let Err(e) = client.initialize().await {
            status.push(ServerStatus {
                name: name.clone(),
                ok: false,
                detail: format!("握手失败: {e}"),
                tool_count: 0,
            });
            client.shutdown().await;
            continue;
        }

        let infos = match client.list_tools().await {
            Ok(v) => v,
            Err(e) => {
                status.push(ServerStatus {
                    name: name.clone(),
                    ok: false,
                    detail: format!("列工具失败: {e}"),
                    tool_count: 0,
                });
                client.shutdown().await;
                continue;
            }
        };

        let n = infos.len();
        for info in &infos {
            tools.push(Arc::new(McpTool::new(
                hub.clone(),
                client.clone(),
                name,
                info,
            )));
        }
        // client 交给工具持有，不再主动关闭：进程随应用退出
        status.push(ServerStatus {
            name: name.clone(),
            ok: true,
            detail: format!("已连接，{} 个工具", n),
            tool_count: n,
        });
    }

    status.sort_by(|a, b| a.name.cmp(&b.name));
    (tools, status)
}

/// 一个服务端的连接结果，供设置页展示。
#[derive(Debug, Clone)]
pub struct ServerStatus {
    /// 配置里的服务名。
    pub name: String,
    /// 是否连上了。
    pub ok: bool,
    /// 说明（错误原因或工具数量）。
    pub detail: String,
    /// 挂上来的工具数。
    pub tool_count: usize,
}
