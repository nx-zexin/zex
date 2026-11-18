# Zex

Zex 是我给自己写的桌面助理。Rust + gpui，模型走 OpenAI 兼容接口或 Anthropic 接口。
它跑在本机上，能读写文件、执行命令、抓网页、派几个子智能体并行干活，也能装别人写的应用面板。

目前只有 Linux 上真在跑，开发和使用都在这台机器上。Windows 和 macOS 只在 CI 里编译、跑测试，
界面还没在那边打开过。

## 为什么自己写

现成的聊天客户端，大多是聊天框加几个工具，剩下的丢给用户。但一个能改你文件的助理，
真正麻烦的是另外几件事：它到底被允许动什么、跑一小时会不会把上下文撑爆、关掉再开还记不记得
刚才在干嘛。这几块占了仓库里绝大部分代码，界面反而是最薄的一层。

## 现在能用的

- 多会话：每个会话各自跑，切走不打断别的；任务会排队，工作台里能看到在跑、排队、需要处理
- 对话里能看工具调用的参数和结果。写文件、执行命令这类会先问你
- 右侧面板：文件预览、应用、子智能体状态，可以拖宽
- 输入框：Enter 发送，Shift+Enter 换行，@ 引文件，/ 打命令，按麦克风说话直接转成文字
- 单轮可选运行模式（变更前确认 / 自动编辑 / 计划模式 / 完全访问）和思考强度，
  也可以只给这个会话换一个模型
- 工具就这些：`fs_read`、`fs_write`、`shell`、`web_fetch`，加上技能加载、五个子智能体工具、
  面板控制和四个应用管理工具
- MCP：stdio 起子进程，它声明的工具会自动并进来
- 应用：一个目录加一个 `main.js`，挂进右侧面板，能画界面、能调 agent、能连网络（得你同意）

## 跑起来

需要先告诉它用哪个模型：

```bash
mkdir -p ~/.zex
cat > ~/.zex/config.json <<'JSON'
{
  "providers": {
    "anthropic": { "kind": "anthropic", "base_url": "https://api.anthropic.com" },
    "local": { "kind": "openai-compat", "base_url": "http://127.0.0.1:11434/v1" }
  },
  "default_model": "anthropic/claude-sonnet-4-6"
}
JSON
echo 'ANTHROPIC_API_KEY=sk-...' > ~/.zex/.env && chmod 600 ~/.zex/.env
```

`base_url` 换成 Ollama、vLLM 之类的地址就能用本地模型。

```bash
cargo build --workspace
cargo run -p zex-app --bin zex          # 界面
cargo run -p zex-app --bin zex-cli -- "看看当前目录是什么项目"
```

两个二进制跑的是同一个内核。CLI 多了两个开关：`--plan` 只读，任何改动都被拒；
`--ephemeral` 用内存库，不写 `~/.zex/zex.db`。审批在终端里问，
`y` 批准、`n` 拒绝、`a` 批准并记住这一类，300 秒没人应答按拒绝处理。
配置也能用环境变量覆盖：`ZEX_PROVIDER`、`ZEX_BASE_URL`、`ZEX_MODEL`、`ZEX_API_KEY_ENV`。

## 权限是怎么判的

四步，顺序不能换：

```
硬 Deny 表 → 运行模式约束 → 风险上调 → grant 匹配
```

硬 Deny 表不可配置，写进去就是不许：zex 自己的 `config.json` 和 `zex.db`（能写它等于把锁拆了）、
系统目录、`curl | sh` 这类管道执行、`dd` / `mkfs` 动块设备、`rm -rf /` 的几种写法。
命令是先用 argv 分词再判的，因为 `rm -fr`、`/bin/rm`、`command rm` 这些变体，
正则根本写不全。

风险分四档，四档配什么待遇要看运行模式：

| 档 | 例子 | 怎么处理 |
|---|---|---|
| 只读 | `fs_read`、`web_fetch` | 任何模式都直接做 |
| 可撤销 | `fs_write` | 默认模式要问；切到自动编辑后直接做 |
| 有影响 | `shell`、MCP 工具 | 默认模式要问，可以记住这一类；完全访问才不问 |
| 不可逆 | 格式化、删系统目录、动密钥 | 哪个模式都问，还要二次确认，不给记住 |

默认模式是「变更前确认」，所以刚装好时写文件也会问一句。

运行模式插在中间判定，不是最后的开关：计划模式下所有会改东西的操作直接拒，连问都不问；
而参数命中风险上调规则的调用，无论什么模式都要问一次，因为「完全访问」放宽的是日常命令，
不是换了张脸的 `rm -rf`。

审批超时按拒绝算，改了参数会重新判一次，一次批准管不了下一次调用。

审计表用 SQLite trigger 挡掉 `UPDATE` 和 `DELETE`，程序想改也改不了。

## 上下文

长任务一定会把窗口撑爆，所以压缩是常驻逻辑，不是出错之后的补救：

- 单条工具结果太大，留头 2/3 和尾 1/3，中间标一句「原始 N token」
- 快到窗口 70% 的时候，把即将丢掉的消息融进一份滚雪球摘要里，之后几轮不重复做
- 到 85% 就跳过冷却直接压

几个坑是踩过才写的：一轮之内只认一个 token 数，不能一边用 API 报的用一边又累加本地估算
（同一批 token 数两遍，会提前触发压缩）；预算和截断都按 token 算，按字符算等于对中文多罚四倍；
压缩之后必须把工具调用和工具结果重新配对，边界切在中间 API 会直接返回 400。
摘要也不能伪装成用户消息，否则模型会把历史指令当成你现在的要求。

## 应用和联网

应用就是一段跑在进程里的 JavaScript，目录放 `~/.zex/extensions/<id>/`，
里面一个 `gpui-shell.json`、一个 `main.js`，图标可选。不用重新编译也不用重新打包。

数据按形状分两套：几个设置项用 `localStorage`，多行记录用 SQLite。SQLite 是一个应用一个库文件，
路径由宿主按 id 拼，脚本碰不到主库（那里有全部会话、审计和授权），`ATTACH` 和 `PRAGMA`
在作者器那层就被挡了。

联网这块绕了一圈。清单里的 `capabilities.network.hosts` 只算申请，因为清单是模型写的，
它自己说「我要连哪儿」不该算数。真正放行的是你确认之后存进 `config.json` 的授权，
生效的是两边取交集：

- 没问过：打开应用时弹一张卡，允许还是不允许
- 说不允许：记一个空列表，以后不再问，想改去设置里改
- 允许了：只放行清单里那几条。之后清单自己加了一条，那条不通，会重新问你

顺手发现脚本里的 `fetch` 读的是挂载那一刻的能力集，绕过了我们自己那道闸，所以挂载前后
要临时换一次能力集，挂完立刻换回来。

## 东西都放哪

- `~/.zex/config.json`：provider、默认模型、MCP、应用授权
- `~/.zex/.env`：密钥，权限 600
- `~/.zex/zex.db`：会话、消息、审计、授权、任务，一个库
- `~/.zex/skills/*.md`：技能，带 frontmatter。提示里只放名字和一句话，正文等 `load_skill` 取
- `~/.zex/extensions/<id>/`：应用
- `~/.zex/state/`：应用的 localStorage 和各自的库文件

`ZEX_CONFIG_DIR` 能把整个 `~/.zex` 挪走，`ZEX_UI_FONT` 换界面字体。

## 还没做的

- 记忆。一张表都还没有，现在只能靠翻会话历史
- 知识库检索。`zex-knowledge` 只有 8 行
- 文件快照和污点追踪。设计写在文档里，代码还没写，所以现在还做不到「写之前自动备份」
- 系统管理那一套：服务、包管理、电源、剪贴板、通知、托盘、截屏、GUI 自动化。
  `zex-platform` 目前只有 8 行。托盘一直没做，Wayland 下那玩意不太靠得住
- 浏览器控制（CDP）
- 定时任务和主动性，也就是「你不看着它也能自己去做」
- 自我进化
- 项目级配置和 `AGENTS.md` 的信任门。文档里有方案，代码里只有一条硬 Deny 路径

## 代码结构

| crate | 干什么 |
|---|---|
| `zex-core` | 消息、工具、事件、风险这些纯类型。不依赖 IO、tokio、gpui |
| `zex-llm` | 两套协议适配，SSE 解析，重试，token 计数 |
| `zex-store` | SQLite：会话、消息、审计、授权、任务、子智能体 |
| `zex-policy` | 风险判定、审批流转、硬 Deny、写审计 |
| `zex-agent` | agent loop、上下文预算和压缩、工具调度、子智能体 |
| `zex-tools` | fs / shell / web 三个工具 |
| `zex-skill` | 技能发现和 `load_skill` |
| `zex-mcp` | MCP 客户端，stdio 上的 JSON-RPC |
| `zex-voice` | 录音和 WAV 编码，固定 16 kHz |
| `zex-extension` | 应用宿主：脚本面板、一应用一库、按应用授权联网 |
| `zex-app` | 组合根。GUI 和 CLI 两个二进制共用这里的装配 |

依赖是单向的，`zex-core` 在最底下。它一旦多出 tokio、rusqlite、reqwest 之类的依赖，
说明分层已经破了。库代码里不许 `panic!` 和 `unwrap()`，错误用 `thiserror` 定义。

`cargo test` 现在是 436 个测试，`cargo fmt --all -- --check` 和
`cargo clippy --workspace --all-targets -- -D warnings` 是 CI 门槛。
CI 配了三个平台，Linux 那份是真跑过的，另外两个目前只是编译和测试通过。

## 文档

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)：分层、双世界桥接、agent loop、数据层
- [docs/SECURITY.md](docs/SECURITY.md)：四档风险、审批、审计、提示注入
- [docs/EXTENSIONS.md](docs/EXTENSIONS.md)：应用系统，包括网络那块的取舍
- [docs/DECISIONS.md](docs/DECISIONS.md)：架构决策记录，被否决的方案也留着
- [docs/PLATFORM.md](docs/PLATFORM.md)：三平台策略和能力矩阵
- [docs/M1-REPORT.md](docs/M1-REPORT.md)：权限、持久化、压缩、界面的实现记录和实测结果
- [docs/research/inherited-design.md](docs/research/inherited-design.md)：从上一个 Go 项目里
  挑出来要继承的设计，以及扔掉的四万行编排代码

## License

Apache-2.0
