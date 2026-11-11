# Zebra → Zex：值得继承的设计资产

> 调研对象：`/home/adam/workspace/ChorusDev/Zebra`（Go，13.7 万行，已冻结为参考实现）
> 调研方式：只读源码 + 真实数据目录（`~/.agent/memory/` 下的经验卡片）+ `AGENTS.md` / `docs/MANUAL.md`
> 目的：把「设计决策」从「Go 实现」里剥出来。凡是需要看代码才能理解的东西，都不算资产。

---

## 一句话总结

**六项里真正必须继承的是三项：记忆格式（含卡片质量门与保守淘汰协议）、权限守卫的三态模型与审批记忆粒度、项目配置信任门。这三项的共同点是——它们的正确形状是被事故教出来的，重写一遍要再踩一遍。**

压缩策略继承**参数与保留原则**，不继承实现（Zebra 的 token 计量是双份累加的，本身就是 bug）。

技能格式继承**布局与注入方式**，扔掉三套 legacy 路径，以及整个 skills.sh 在线安装链路（那是给 CLI 用的，桌面助理不需要）。

自我进化：**继承循环协议与回滚机制，砍掉五级验证里的后三级**——Zebra 的 `contract_test` 是把模型给的 Go 代码片段拼成 `_test.go` 再 `go test -run`，`behavior` 检查是"文件里有没有 `package ` 字样"。这两级不构成验证。`zex-evolve` 应推迟到 M4，且第一版只做「提案 + 人工批准」。

还有一个容易漏的点：**Zex 的 L0–L3 风险分级和 Zebra 的 Allow/Ask/Deny 是两个正交维度**，不能相互替代。前者是工具的静态属性（编译期常量），后者是单次调用的判定。叠起来的规则写在 §4。

---

## 0. 证据边界

每个结论都标了代码位置，数值是源码里的字面量而不是估的。凡是 Zebra 里**已废弃但仍留在代码里**的东西（legacy 路径、死表、注释掉的自动重启）按"包袱"处理并说明原因；凡是"代码里没有"的能力（例如**衰减机制**）明确说没有——避免新项目照着想象去实现一个不存在的东西。

---

## 1. 技能（skill）格式

### 1.1 现状是怎么做的

**磁盘布局**（`internal/skills/skills.go:56-73`）：

```
<项目>/.agent/skills/<name>/SKILL.md      ← 协议布局，一份技能一个目录
<项目>/.agent/memory/skills/**/*.md       ← 旧布局
<项目>/.chorus/memory/skills/**/*.md      ← 更旧的兼容布局
~/.agent/skills/  + ~/.agent/memory/skills/   ← 全局层兜底
```

扫描顺序是先项目后全局，`seen[id]` 去重，**项目层赢**——所以项目可以覆盖一份共享技能而不必改它。

**目录式 vs 扁平式**（`findSkillFiles`，`skills.go:186`）：

- `skills/<name>/SKILL.md`：目录里只要含 `SKILL.md`（大小写不敏感，兼容老卡片的 `skill.md`），就算一个技能，**不再下降**（`fs.SkipDir`）。目录其余部分是它的资产树（references/、workflows/、脚本）。
- `skills/<name>.md`：扁平布局，单个文件即一个技能。

**ID 回退链**（`skills.go:95-105`）：`frontmatter.id` → `frontmatter.name` → 路径推导（目录技能取父目录名，扁平取文件名）。

**加载后如何进入上下文**：Zebra 有两条路径，都值得注意。

1. `FormatForSystemPrompt`（`skills.go:122`）会渲染 `## Available Skills` 段，每项含 name / ID / description / **文件路径** / tags，并附一句
   > "When a user's task matches a skill description, use the `read` tool to load the skill file first, then follow its instructions."
2. 但**真实运行路径是另一条**：`internal/kernel/memory_index.go` 的 `buildMemoryIndexBlock` 把 skill 卡片和 experience 卡片统一渲染成 `<available_memory>` XML 块，只给 name / description / **绝对路径**，正文一律不给，让模型自己 `read`。

也就是说：**技能不是被注入的，是被索引的**。索引是指针表，正文按需读。这条设计让技能库可以无限增长而不吃 context。

**在线安装**（`internal/skills/registry.go`，约 690 行）：查 skills.sh 公共索引 + 从 GitHub raw 拉文件树。它的安全约束具体到数字：单请求 30s、整个安装 10min、单文件 2 MiB、单技能最多 16384 文件（实测：ppt-master 有 13k 个 svg）、总计 64 MiB、8 个并发 worker（串行要几小时）。

值得注意的是校验发生在写盘**之前**（`validatePlan`）：拒绝 `..`、绝对路径、重复路径、超限文件，必须含 `SKILL.md`；目标目录已存在则报错，不静默覆盖。

### 1.2 该继承的

1. **目录式布局 `<name>/SKILL.md` + 资产树。** 这是唯一能容纳"带参考资料和工作流的技能"的形状。
2. **`fs.SkipDir` 语义**：目录里有 SKILL.md 就整体算一个技能。Zebra 在这里踩过坑——早先按 `.md` 后缀全量递归，ppt-master 的 230 个 reference 文件把技能列表淹了（`skills.go:186-194` 注释原话）。
3. **ID 回退链 + 跨层去重、项目优先。** 卡片经常没写 id（ppt-master 只给 name/description），空 id 会让它无法去重、无法匹配、无法出现在列表里。
4. **索引注入而非正文注入。** 这是技能库能长大而 context 不炸的唯一原因。
5. **安装路径的写盘前校验。** 顺序比规则本身更重要：先验全部，再写第一个字节。

### 1.3 该扔的

1. **三套 legacy 路径。** `.chorus/memory/skills/`、`memory/skills/`、`skills/` 同时存在，是 Chorus → Zebra 迁移的化石。新项目开局就一套：`<config>/skills/<name>/SKILL.md`。
2. **扁平 `<name>.md` 布局。** 它和目录式并存只会制造歧义（同名时谁赢？Zebra 靠 `seen` 顺序，实际推出后想改也改不了）。
3. **skills.sh 在线安装整条链路（约 690 行）。** 它解决的问题是"CLI 用户想装别人的技能"，桌面助理的正确姿势是 M4 之后做"从本地文件夹导入"或者干脆不做。**尤其不要继承 `resolvePlan` 的自动探测顺序**（root → skills/ → .agent/skills/ → .claude/skills/ → skills/src → 全树搜同名 SKILL.md），那是一套为兼容生态妥协出来的启发式。
4. **`tarballExtract`**（`registry.go:645`）：注释自己写着 "unused today but the parse path is validated by tests"。死代码 + 测试，纯负担。同类的还有 `walkMarkdownFiles`（已被 `findSkillFiles` 取代）。

### 1.4 给 Rust 的建议

```rust
// zex-core/src/skill.rs
pub struct SkillCard {
    pub id: SkillId,                  // 稳定键：去重/匹配/引用都用它
    pub name: String,                 // 面向模型的显示名
    pub description: String,          // 一行，决定可检索性
    pub tags: Vec<String>,
    pub dir: PathBuf,                 // 技能根目录
    pub entry: PathBuf,               // dir/SKILL.md
    pub source: SkillSource,          // Project | Global
}

pub enum SkillSource { Project, Global }   // 同 id 时 Project 覆盖 Global

// 加载规则（写死，不做启发式）：
//   <project>/.zex/skills/<name>/SKILL.md   ← 项目层
//   ~/.zex/skills/<name>/SKILL.md           ← 全局层
//   含 SKILL.md 的目录即一个技能，不下降
//   id 回退：frontmatter.id → frontmatter.name → 目录名
```

常量：

```rust
pub const SKILL_ENTRY: &str = "SKILL.md";
pub const MAX_SKILL_ASSETS: usize = 4096;        // 不用 16384，那是为 ppt-master 特调的
pub const MAX_SKILL_FILE_BYTES: u64 = 2 << 20;
pub const MAX_SKILL_TOTAL_BYTES: u64 = 32 << 20;
```

注入格式沿用 XML 而不是 Markdown 列表——原因见 §2.5（转义是硬要求）：

```xml
<available_skills>
  <skill>
    <name>修复 Go 测试失败</name>
    <description>定位并修复失败的 Go 测试，直到全绿</description>
    <location>/abs/path/.zex/skills/go-test-fix/SKILL.md</location>
  </skill>
</available_skills>
```

并在系统提示里保留那句关键指令：**"任务匹配时先用 read 打开技能文件，再按其步骤执行。"** 没有这句，模型会以为索引就是内容。

---

## 2. 记忆（memory）格式

这是 Zebra 里最厚、教训最多的一块（`internal/memory/` 约 9.7k 行）。它的复杂度几乎全部来自两件事：**中文检索**和**不丢卡片**。这两件事新项目都要重做，所以必须读懂。

### 2.1 现状是怎么做的

**事实源与索引分离**（`store.go:1-10` 包注释）：

- 事实源 = Markdown 卡片文件：`<config>/memory/experiences/<id>.md`、`<config>/memory/skills/<id>.md`。
- 索引 = `<config>/memory.db`（SQLite），**可重建**。`memory verify --repair` 从磁盘重建，不重写卡片、不动 `updated_at`。
- 另有一份**写穿镜像**：`<config>/memories/*.md`（`.agent` 协议格式，字段 `id/title/content/importance/tags`），是派生物。

**卡片类型**（`store.go:38`）：

```go
const ( TypeSkill CardType = "skill"; TypeExperience CardType = "experience" )
```

只有两种。没有 scope（项目/全局/用户），没有 kind 细分。`CardDirForType` 把类型映射到目录（`experience` → `experiences`，注意单复数差异，源码专门为这个差异写了函数）。

**frontmatter 字段**（`store.go:60`，`Frontmatter`）：

| 字段 | 说明 |
|---|---|
| `id` / `name` / `description` / `created_at` / `updated_at` | 必填，`yaml` 标签**不带** omitempty |
| `tags` | 字符串数组 |
| `type` | 只有 experience 写（缺省视为 skill——这是个坑，见 §9） |
| `relates_to_skill` | 经验关联到哪个技能 |
| `severity` | `low/medium/high`，映射到协议镜像的 `importance` |
| `complexity_score` / `generality_score` / `frequency_score` / `success_rate` | LLM 蒸馏时自评的浮点/整数分 |
| `access_count` / `last_accessed_at` | **`yaml:"-"`，只存索引，永不写卡片文件** |

最后那条的设计理由（源码注释）：卡片每次 `Put` 都写文件，如果读一次就改一次文件，mtimes 会全部抖动、整个库会按"最后更新"重排。

**存储 schema**（`store_schema.go`）：`memory_entries`（主表，`type` 带 CHECK、`file_path` UNIQUE）+ `memory_usage_daily`（每天一行观测快照）+ `memory_vec USING vec1`（向量）+ `memory_fts USING fts5(name, description, body_content, content=memory_entries, tokenize='trigram')`，FTS 靠 INSERT/DELETE/UPDATE 三个触发器同步。

**回调路径**（`internal/kernel/memory_index.go`，每轮一次）：

- 用**本轮任务描述**作为查询去检索卡片。
- 候选上限 `memoryIndexCandidateLimit = 40`，渲染预算 `memoryIndexBudgetTokens = 1200`（tiktoken 计数，模型未知时回退估算）。
- 只渲染 name / description / **绝对路径**，experience 额外带 `severity` 属性。
- 分 `<available_experiences>` / `<available_skills>` 两组，被预算截掉的写一行"(N more cards were omitted… Use the memory_search tool to find them.)"。
- 所有字段 XML 转义（`escapeXML`）——源码注释记录了 severity 漏转义的历史 bug。

**检索算法**（`store_query.go` + `store_text_query.go`）：向量检索（`vec1_cos_distance`）与全文检索各取至多 `textCandidateLimit = 500` 条 → **RRF 融合**（`1/(k+rank)` 求和，`rrfK = 60`）→ 应用 offset/limit。RRF 是必须的：余弦距离与 BM25 的量纲没有可比性，直接相加是错的。其他常量：`defaultSearchLimit = 50`、`ftsMinTermRunes = 3`、`maxQueryPieces = 64`。

中文处理是三层的：查询词按 3 字滑动窗口切开（重叠），窗口 ≥3 字走 FTS5 `MATCH ... OR`，<3 字（「滚动」「Go」）走 `LIKE` 扫描；Go 侧再按字段加权排序（name 命中 +3，description +2，body +1），并列时用 `usageTieBreak`（先比 `access_count`，再比 `last_accessed_at`，都没用过则保持 SQL 的 recency 顺序）。

**卡片质量门**（`quality.go`）：

- `genericCardNames` 是一张 40 项的黑名单，含 `使用场景`、`问题上下文`、`问题表现`、`解决方案`、`核心教训`、`usage`、`overview`、`untitled` 等。
- `CardProblems` 报四类问题：name 空 / name 泛化 / description 空 / 正文空。
- `NormalizeCard` 只做**机械可修复**的两件事：description 缺失→取正文第一段非标题非列表的散文（上限 200 rune）；name 泛化→用 description 截断（上限 60 rune）替代。**永远不发明知识。**
- 蒸馏时质量欠佳的卡片**仍然写入**（`evolution.go:222` 注释："Dropping it would lose knowledge silently, which is the one outcome a memory system must not produce"）。

**生命周期**——这是最值钱的部分，因为它全部是"不做某事"：

| 机制 | 位置 | 规则 |
|---|---|---|
| 使用反馈 | `store_access.go` | 只有**显式检索**（`memory_search` 工具、CLI `memory get/search`）才 `access_count+1`。**进入提示词索引不计数**——每轮几十次，计了就没有信息量 |
| 观测快照 | `store_schema.go` `recordUsageSnapshot` | 每次打开 store 记当天一行（UTC 日期，同日 upsert 覆盖）。永不删除，历史就是价值 |
| 观测报告 | `store_stats.go` | 报 `DefaultHorizons = 30/90/180/365 天` 各自会选中多少；超过观测窗口的标 `??`（`Misleading()`）；`stable()` 要求「从未检索」在 **≥30 天**内没变化才算稳定 |
| 淘汰 | `store_prune.go` | **不删除**，移到 `memory/.archive/<UTC 时间戳>/`；`--unused` 必须配 `--older-than`（否则选中全库）；一次归档超过半数需 `--force`（`pruneForceRatio = 2`，即 owned×2 > total） |
| 孤卡片 | `store_prune.go` | 扫描范围外的卡片文件分两种动作：id 已索引→归档（内容在别处），id 未索引→**纳管**（纳管 = 移入类型目录 + 建索引） |
| 归档 | `store_archive.go` | 只读列举（生产者 prune/curator、时间、张数、字节数），`--restore <id>` 移回；**没有自动保留期，没有 `--drop`**，因为归档里常常是唯一副本 |
| 审计 | `store_audit.go` | 报 9 类漂移：文件缺失 / 未索引文件 / ID 冲突 / 不可解析 / 游离文件 / 协议镜像缺失 / 镜像多余 / 缺向量 / 孤儿向量 |
| 策展 | `internal/curator/` | 每 5 次蒸馏触发 light（≤5 动作，禁删），5 次 light 触发 medium（≤10 动作，删≤30%），5 次 medium 触发 deep（≤20 动作，删≤30%）；动作含 merge/condense/sync_background/mark_stale |

**必须点明的一件事：Zebra 没有衰减机制。** 没有 `importance *= 0.99^days`、没有半衰期、没有时间加权打分。它用的替代品是「使用次数 + 最近访问时间」作为**并列打破**（tie-break），相关度永远优先——所以冷门但贴题的卡片不会被热门卡片压下去。这个取舍是有意的，注释写得很清楚。新项目如果要做衰减，是**新增**，不是继承。

### 2.2 该继承的

1. **Markdown 卡片 = 事实源，数据库 = 可重建索引。** 这一条支撑了后面所有能力（审计、修复、导入导出、手工编辑、迁移）。任何把记忆只存 DB 的设计，都会在第一次 schema 变更时失去用户的全部记忆。
2. **`skill | experience` 二分。** 不是 taxonomy 洁癖：两者的正文模板、注入分组、蒸馏提示词、判官分类规则全都不同（skill 给"执行步骤"，experience 给"问题上下文/表现/解决方案/核心教训"）。合并成一种会让两边都含糊。
3. **使用遥测不进卡片文件。** 索引专属字段是正确的位置。
4. **「name + description 即可检索性」这个判断，以及围绕它的质量门。** 提示词索引只展示这两个字段，所以它们写砸 = 卡片不存在。黑名单 + 从正文补 description 是成本极低、收益极高的一步。
5. **保守淘汰的全部四条规则**：不删只归档 / `--unused` 必须配年龄 / 过半需 `--force` / 归档无自动过期。
6. **观测窗口与趋势。** `access_count = 0` 有两个成因（没用 vs 还没轮到），单次读数分不清。每日快照 + "从未检索集合是否停止收缩" + ≥30 天判定，是把这件事变成可决策问题的唯一办法。
7. **RRF 融合 + 三层中文检索（窗口/trigram/LIKE 兜底）。** 中文任务的描述是一句话，不是关键词表；AND 语义会一条都匹配不到，这个坑 Zebra 踩过。
8. **显式检索才计数。**
9. **质量门宽松（欠佳也存）+ 独立修复通道（normalize --apply）。** 存储侧不丢知识，清理侧可预演。

### 2.3 该扔的

1. **写穿协议镜像 `memories/*.md`。** 这是整个包里性价比最低的机制：只在 `Put` 时写，所以导入的卡片没有副本、删掉的卡片留下孤儿；没有任何东西读回它；`memory export` 专门为它存在，`audit` 为它多报两类问题。**Zex 只需要一份卡片文件 + 一个 DB。**
2. **三套 legacy 路径 / 兼容布局。** 同 §1。
3. **`orphan` + `adopt` 整套机制**（`store_prune.go:600-780`，约 200 行）。它存在的原因是 Zebra 历史上把卡片从 `memory/*.md` 搬到 `memory/skills|experiences/`，残留文件成了"谁也看不见的卡片"。新项目开局就没有这个历史。**如果新项目的卡片目录是唯一入口，这个问题不可表达。**
4. **`severity` → `importance` 的字符串映射**（`memories.go:memoryImportance`）。`low/medium/high` 三档既不够用也没有任何地方消费（只有注入时的 XML 属性）。新项目直接定义 `importance: u8`（1–5）或干脆不做。
5. **`complexity_score` / `generality_score` / `frequency_score` / `success_rate` 四个 LLM 自评分数。** 没有任何代码用它们排序（排序只用匹配度 + 使用次数）。模型给自己的作品打 1–10 分是噪声。留着只会让蒸馏提示词变长、卡片多四行。
6. **`memory_usage_daily` 的"每开一次 store 写一行"。** 想做同一件事，正确形状是一张 `memory_events(memory_id, ts, kind)` 事件表（还能顺便算趋势、算最近命中、算冷却），而不是每天一个聚合快照。但**目标（观测窗口 + 趋势）必须保留**。
7. **`vec1` 扩展 + `embeddingDim = 512`（bge-small-zh-v1.5）、`ftsTokenizer = "trigram"` 的版本迁移逻辑**（`migrateFTS` 读 `sqlite_master` 比对 DDL 再 DROP 重建）。都是跟随 Go 生态的选择。Rust 侧用 `sqlite-vec`/`vec0`，维度按所选模型走（512/768/1024），**把维度作为一个常量配一处**；从第一天就用 trigram，迁移路径不需要——但 `CREATE VIRTUAL TABLE IF NOT EXISTS` 不替换已有 tokenizer 这个坑本身要记住（见 §9 第 2 条）。

### 2.4 Rust 侧数据结构建议

```rust
// zex-store/src/memory.rs
pub enum MemoryKind { Skill, Experience }

pub struct MemoryCard {
    pub id: MemoryId,
    pub kind: MemoryKind,
    pub scope: Scope,            // ← 新增。见下方说明
    pub title: String,           // 对应 Zebra 的 name
    pub summary: String,         // 对应 Zebra 的 description；空 = 不可检索
    pub body: String,
    pub tags: Vec<String>,
    pub importance: u8,          // 1..=5，替代 low/medium/high
    pub relates_to: Option<SkillId>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    // 遥测，只存 DB 不写文件：
    pub hits: u64,
    pub last_hit: Option<OffsetDateTime>,
}
```

**关于 `scope`：** Zebra 没有这个概念，但 Zex 的 `ARCHITECTURE.md` §6 已经把 `scope` 写进 schema 了。这不是照抄，而是必须新增——桌面助理有"这个项目专用的经验"和"跨项目通用的偏好"两种东西，Zebra 用"项目级/全局级目录"隐式表达，代价是切换项目就看不见全局卡片之外的任何东西。要在 Rust 侧显式化成列：`scope ∈ {User, Project}`，检索时 `scope = User OR project_id = current`。

常量（建议有出处，不要随手改）：

```rust
pub const INDEX_BUDGET_TOKENS: usize = 1200;     // Zebra 实测可用
pub const INDEX_CANDIDATES: usize = 40;
pub const RRF_K: f64 = 60.0;
pub const FTS_MIN_CHARS: usize = 3;              // trigram 粒度
pub const MAX_QUERY_PIECES: usize = 64;
pub const SEARCH_CANDIDATES: usize = 500;
pub const PRUNE_MAX_FRACTION: f64 = 0.5;         // 过半需显式确认
pub const OBSERVE_MIN_DAYS: i64 = 30;            // 判定"从未被用"的最短观测期
pub const SUMMARY_MAX_CHARS: usize = 200;        // NormalizeCard 的 description 上限
pub const TITLE_MAX_CHARS: usize = 60;
```

### 2.5 三个必须保留的细节

1. **索引块必须是 XML + 全字段转义。** 卡片内容来自仓库内的文件（不可信输入）。Zebra 的教训：serde 里漏转义一个属性值，含 `"` 的值会提前关闭属性，含 `<` 的值会破坏块嵌套——模型拿到的就是一段被破坏的提示词。Rust 侧用一个专门的 `escape_xml` 且**只在一个函数里拼索引**。
2. **索引里给绝对路径，不给正文。** 模型用 `fs_read` 自己打开。这一条同时解决了 context 预算和"卡片更新后模型看到旧内容"的问题。
3. **被截断时要告诉模型"还有 N 张，用 memory_search 找"**。没有这句话，被预算截掉的卡片就是永久不可达。

---

## 3. 上下文压缩

### 3.1 现状是怎么做的

`internal/compressor/`（约 1.3k 行）。策略一句话：**维护一个滚雪球式的全局摘要，每次只把"即将丢弃的消息"融进已有摘要**。

压缩后结构 = `[一条包在 <context-summary> 里的 user 消息] + [最近 N 个 assistant turn 的原始消息]`。

**全部阈值**（`compressor.go:36-56` 的默认值）：

| 字段 | 默认 | 含义 |
|---|---|---|
| `Threshold` | 0.70 | 估算用量 / contextWindow 达到即标记"需要压缩" |
| `EmergencyThreshold` | 0.85 | 单独一条更狠的路径：`estimateTotalTokens(messages) × 1.5 / contextWindow` |
| `PreserveRecentTurns` | 10 | 保留最近多少个 **assistant 消息**（`findRecentTurnsBoundary` 从尾部倒数） |
| `SummaryTokenBudget` | 800 | 摘要本身的目标长度（写进提示词，不强制） |
| `SummaryInputLimit` | 12000 | 送去总结的**字符**上限 |
| `CooldownTurns` | 3 | 两次压缩之间至少隔多少 turn |
| `ToolResultMaxChars` | 3000 | 单条工具结果超过就截断 |
| 消息条数下限 | 6 | `len(messages) < 6` 直接不压缩 |

**触发判定有两套并行**：`UpdateRealTokenCount`（API 回报的 `total_tokens`）与 `AddTurnTokens`（本地 tiktoken 累加）。两者都写进同一个 `accumulatedTokens`，然后与 `contextWindow` 比 `Threshold`。冷却只抑制非紧急触发。

**什么被保留 / 什么被丢弃**（`fuseIntoSummary` 的 system prompt 就是规格说明）：保留文件路径/函数名/变量名（"绝对不能丢失"）、关键决策及理由、错误及解决方案、当前进度（已完成/进行中/待完成）；允许压缩**过程描述**（"正在读取文件" → "已读取"），但不允许压缩事实。

**降级路径**（`ruleBasedFallback`）：LLM 调用失败时，逐行 `- ` 前缀化 + 每行截 100 字符 + 总长按 `tokenBudget × 2` 字符截断，拼在旧摘要后。**永远返回可用摘要，不返回空。**

**压缩后必须修复工具调用配对**（`compressor_repair.go`）：扫一遍 `tool_call.id` 与 `tool_result.tool_call_id` 的**交集**，把只有一边的删掉。不做这一步，压缩边界切在调用和结果之间就会让 API 返回 400（"tool_call_id not found"）。

**token 计数**：`tokencounter` 用 tiktoken，未知模型回退 `o200k_base`，再回退 `(len+3)/4`。另有 `estimateMessageTokens` 的分档估算：CJK 每字 1.0 token、ASCII 字母 0.25、其他 0.6，每条消息 +30 开销。

### 3.2 该继承的

1. **增量摘要（fusion）而不是"重新总结全部历史"。** 后者每压缩一次就要重读全部旧消息，成本随对话长度线性增长；fusion 的输入恒定 = 旧摘要 + 待丢弃消息。
2. **冷却（3 turn）。** 没有它，工具结果很大的时候会连续几轮压缩，每次压缩都要一次 LLM 调用 —— 钱和时间双输。
3. **单条工具结果先截断（3000 字符，头 2/3 + 尾 1/3，中间插一句"原始 N 字符"）。** 这是最便宜的一层，能挡掉绝大多数"一次 grep 把 context 塞满"。
4. **工具调用配对修复。** 这是正确性要求，不是优化。
5. **摘要提示词里的四条保留清单。** 直接可复用，中文措辞都不用改。
6. **降级摘要永远不返回空。** 摘要为空会导致模型丢失全部前情，比摘要难看严重得多。
7. **摘要作为一条独立消息插在最前面，带显式标签。** 模型需要知道"这是我之前的经历的压缩，不是用户说的话"。

### 3.3 该扔的

1. **双 token 计量。** `UpdateRealTokenCount` 把 API 的 `total_tokens` **赋值**给 `accumulatedTokens`，`AddTurnTokens` 又在其上**累加**本地计数。下一轮 API 回报的 total 已经包含了上一轮本地的增量，于是同一批 token 被数两遍。这是明确的 bug（会提前触发压缩），不是设计。
2. **`estimateTotalTokens(messages) × 1.5` 的魔法系数。** 只有在 `EmergencyThreshold` 那条路径用。1.5 从哪来没人知道。
3. **`newTokens = before × len(result) / len(messages)`**（`compressor.go:applyCompression`）。按消息**条数**比例推 token 数，把统计量变成了装饰。统计数据如果不可信，不如不报。
4. **`SummaryInputLimit` 用字符数。** 中文和英文的字符/token 比差 4 倍，用字符当预算等于对中文惩罚 4 倍。Rust 侧一律用 token。
5. **把摘要包成 `ai.UserMessage`。** 语义上它不是用户说的。Rust 侧给 `Role::Summary`（或至少 `system` + 显式标记），避免模型把摘要里的指令当成用户要求。
6. **`<context-summary>` 里的中文固定文案**。文案要可配置/可本地化，但这段是唯一不需要继承的部分。

### 3.4 Rust 侧建议

```rust
// zex-agent/src/context/budget.rs
pub struct ContextBudget {
    pub window: usize,                 // 来自模型元数据
    pub compress_at: f32,              // 0.70
    pub emergency_at: f32,             // 0.85
    pub keep_recent_turns: usize,      // 10
    pub summary_max_tokens: usize,     // 800
    pub cooldown_turns: u32,           // 3
    pub tool_result_max_tokens: usize, // ← 从 3000 字符改成 ~1000 token
    pub min_messages: usize,           // 6
}

pub enum TokenSource { Provider(u32), Estimated(u32), Mixed }  // 单一真源，别混
```

三条硬规则：

1. **一个 turn 内只允许一个 token 数**。优先用 provider 回报的 `usage`；没有就用本地 tiktoken 全量重算（消息数在几百这个量级，重算成本可忽略）。**不要累加。**
2. **截断和预算都按 token 算**，字符只用于 UI 显示。
3. **压缩是纯函数 + 一次副作用**：`plan(messages, budget) -> Option<CompressionPlan>`（无 IO），`apply(plan, summary) -> Vec<Message>`。Zebra 用"持锁快照 → 解锁跑 LLM → 再接锁写回"三段式实现同样的事（`compressor.go:planCompression/applyCompression`），思路对但 Go 的锁让它变复杂；Rust 侧把 `Compressor` 放在单任务里就不需要锁。

`ARCHITECTURE.md` §4 里那张表（70% 滑动窗口 / 85% 摘要 / 95% 记忆置换）比 Zebra 多一档"记忆置换"——那是新设计，Zebra 没有。**Zebra 的实测只支持 0.70 和 0.85 两个数字**，95% 那一档没有数据支撑，实现时要特别小心（把早期内容写进 memory 再丢正文，如果写失败就丢知识了；建议先只做前两档）。

---

## 4. 权限守卫

### 4.1 现状是怎么做的

`pkg/guard/`（约 3k 行）。它是**工具执行前的一道闸**，挂在 agent loop 的 `BeforeToolCall` 钩子上。

**三态模型**（`guard.go:56`）：

```go
const ( Allow Decision = iota; Ask; Deny )
```

包注释把设计意图写得极清楚，值得整段继承：

> *Deny 保留给"在这里本质上永远不合法的调用"（`rm -rf /`、`mkfs`、写 `/etc/passwd`）。问这些等于把用户的注意力浪费在一个答案永远是"不"的问题上。*
> *Ask 用于"合法但超出任务隐含范围"的调用（写工作区外、sudo、下载管道给 shell）。一律拒绝会挡住真正的活；静默放行是护栏漏水的标准方式。*

**分层**：`Policy.Assess` 是**纯函数**（无 IO、不阻塞、无 ctx，可表驱动测试）；`Guard.Authorize` 才负责找人。判定优先级（`policy.go:Assess`，源码明确列为 1→4）：

1. **Deny 规则** —— 结构性的，**不可被配置关掉**。注释原话："A guardrail whose kill switch is a file the agent itself can write is not a guardrail."
2. `Config.Tools` 的逐工具显式判定 —— 位置在 ask 规则**之上**，所以既能收紧（`read → ask`）也能放宽（`bash → allow`，从而不再问 sudo 和强制推送）。
3. Ask 规则。
4. Allow。`Config.SkipConfirm` 在 2、3 之间生效（Ask → Allow），Deny 在 1 已经解决完了，碰不到。

**规则表**（`rules.go`）：两张表，`denyCommandRules` 8 条 + `askCommandRules` 11 条。

Deny：fork 炸弹正则、`dd of=/dev/sd*`、`> /dev/sd*`、`mkfs|wipefs`、`fdisk|parted|sgdisk ... /dev/`、写 `/etc/passwd|shadow|group|sudoers`、`chmod 777 /`、`> /`。

Ask：`sudo|doas`、`curl|wget ... | sh`、`shutdown|reboot|halt|poweroff`、`systemctl|launchctl|service`、`git push --force*`、`npm|cargo|yarn|pnpm|poetry publish`、`kill -9 -1` / `pkill -9`、`crontab`、`chmod 777`、`mount|umount`、`docker|podman --privileged`。

**`rm` 不走正则表，走解析**（`policy.go:removeDeny/removeAsk`）。理由写在 `rules.go:174`：表格版的 flag 模式停在第一个 `--`，于是 `sudo rm -rf --no-preserve-root /` 被读成"普通 sudo"。解析实现：
- `rmIsRecursive`：只看短 flag 簇里的 `r/R` 和 `--recursive`；
- `rmTargets`：非 `-` 开头的 token，去引号；
- `rm -rf dir/*` 去掉尾部 `/*` 按目录本身判定；`rm -rf /*` 剥完变成 `/`（有专门的分支处理）；
- Deny：目标是 `/` 或 `$HOME` → `rm-root`；递归且落在 `systemDirs`（`/etc /usr /var /bin /sbin /boot /lib /opt /sys /proc /dev /System /Library`）→ `rm-system`；
- Ask：目标在工作区外且不在 tmp → `delete-outside-workspace`。

**路径规则**：`writeDenyPaths` 17 条（`~/.ssh/*`、`~/.aws/credentials`、`~/.gnupg/*`、`~/.netrc`、`~/.git-credentials`、`~/.pgpass`、`~/.docker/config.json`、`~/.kube/config`、`~/.npmrc`、`~/.config/gh/hosts.yml`、**`~/.agent/.env`**、**`~/.agent/config.json`**、`/etc/*`、`/boot/*`、`/sys/*`、`/proc/*`、`/usr/bin/*`、`/usr/sbin/*`、`/bin/*`、`/sbin/*`、`/System/*`、`/Library/Launch{Daemons,Agents}/*`）；`readAskPaths` 9 条（读凭据 → Ask 但**不提供"记住"**）。

三个细节：
- `~` 展开同时认 `~`、`$HOME`、`${HOME}`（"模型三种写法都写"）。
- 路径解析必须与工具解析**方式相同**（`expandUserPath` 的注释：否则 `../../etc/passwd` 会被判成另一个文件）。
- `~/.agent/.env` 和 `~/.agent/config.json` 在**写拒绝**表里，因为往那写等于给自己的下一次进程注入 `PATH`/`LD_PRELOAD`。

**审批记忆**（`guard.go:Authorize`）：

- 只有 Ask 且 `RememberKey != ""` 的可记忆；**Deny 永不记忆**（"policy 是静态规则，静音它会让护栏依赖用户看不见的状态"）。
- `RememberKey` 三种粒度：
  - 命令类：`"cmd:" + kind + ":" + cmd`（整条命令字符串）
  - 工作区外写入：`"dir:" + filepath.Dir(path)`（**按目录，不按文件**——批量改 20 个文件只需答一次）
  - 项目外删除：`"cmd:" + cmd`
- **读取凭据不给 RememberKey**："always allow reading ~/.ssh 不是值得持久化的偏好"。
- 记忆是**进程内** `map[string]bool`，会话级，不落盘。

**审批通道的三个坑**（`guard.go` / `approval.go` 注释）：

1. **`askMu` 串行化**：并行工具批次会给三个待确认调用同时弹框，用户分不清哪个答案属于哪个。串行化只让真正需要人的调用排队，批次其余部分仍并发。
2. **人改过参数必须重新过规则**（`AuthorizeWithoutAsking`）：不重验就等于给了"批准 ls，执行 rm -rf"的旁路。而且这个重验**不能**调 `Authorize`——`askMu` 非可重入，会死锁；两次询问同一个决定是用户赢不了的循环。所以重验里 Ask → Deny。
3. **改了参数的批准不写入记忆**（注释：记忆按规则族存，写进去会让下一次同类调用直接放行**修改前**的参数）。

**无人应答时**（`Options.AllowFallback`，默认 false）：没有 Approver / ctx 取消 / 超时 → **Deny**。注释："guardrail 联系不到用户时不能猜，只有 deny 是可恢复的那种猜测。" 超时在 headless 入口是 5 分钟（MANUAL §4.8），TUI 无限但可取消。

**配置**（`config.go`）：`~/.agent/guard.json`，**只读全局层，不读项目**（"随仓库分发的副本可以替克隆它的人放宽护栏"）。`ZEBRA_GUARD`（值 `off/0/false/no`）优先于文件，`ZEBRA_GUARD_CONFIG` 可指向别处，两者的共同理由是"信号来自仓库之外"。削弱型配置（关闭 / skip_confirm）各打一次 warn（`sync.Once`）。

### 4.2 该继承的

**这是六项里继承价值最高的一块。** 具体：

1. **三态 + 严禁合并 Ask 与 Deny。** 这是全篇最重要的一个判断，理由如上引原文。
2. **Deny 不可被配置关掉，只能被环境变量关掉（而环境变量必须来自仓库外）。** 这条把"护栏的开关"物理上移出了 agent 的写权限范围。
3. **纯函数分类器 + 独立的人机层。** 让"规则"可以被表驱动测试穷举，让"找人"可以被替换（TUI / CLI / MCP 宿主）。
4. **Deny 永不记忆 / 读凭据不给记忆 / 工作区外写入按目录记忆。** 三条粒度决策都是正确的。
5. **改参数必重验；改参数的批准不入记忆。**
6. **无人应答默认拒绝。**
7. **`rm` 用解析而非正则。** 这条省下来的是一类真实事故。
8. **`~`/`$HOME`/`${HOME}` 三种拼法 + 与工具一致的路径解析 + symlink 解析。**
9. **受保护路径里包含 agent 自己的配置文件。**
10. **规则文件只读全局层。**
11. **告警去重（`sync.Once`）**：worker 模式每个任务重建一次 agent，逐任务重复同一条 warn 只会把它淹成背景噪。

### 4.3 该扔的

1. **`Config.Tools` 位置在 ask 规则之上、可以放宽到 allow。** 在 L0–L3 模型里这个口子太宽（`bash: "allow"` 一开，sudo/force-push/写工作区外全免）。Rust 侧要让"放宽"只能通过**grant 模式**（工具 + 参数 glob），而不是"整个工具免检"。
2. **`ZEBRA_GUARD=off` 整关护栏。** 桌面应用里这个开关的存在意义是 0，风险无限。保留 `skip_confirm` 一档足够（deny 仍然生效）。
3. **正则表的所有具体模式。** 模式本身可以照抄（它们是领域知识），但**不要照抄"以正则为唯一手段"的思路**。Rust 侧应先解析 argv（`shell-words` / 自建 tokenizer），对 `rm`/`dd`/`chmod`/`git` 做结构化判定，正则只作为兜底。
4. **进程内 `map[string]bool` 记忆。** 桌面应用常驻，授权应该持久化到 `grants` 表（`SECURITY.md` §3.2 已经这么设计了），并且可撤销、可查看。
5. **`pkg/pending` 的全局单例注册表**（`approval.go` 包注释说明这是为了打破"agent 需要 hook、hook 需要 guard、guard 需要 UI、UI 从 agent 建"的循环依赖）。Rust 侧用 channel + `Arc<ApprovalQueue>` 注入即可，不需要全局。
6. **`allow` 一个"计划模式全禁"的并行机制**。Zebra 有 mode（plan）和 guard 两套拦截，检查顺序是"模式门 → 护栏"。Rust 侧应统一成一个 `Verdict` 计算：先静态风险（L 级）、再模式约束、再具体规则。

### 4.4 Rust 侧建议：L0–L3 与 Allow/Ask/Deny 的正确叠法

Zex 已有 L0–L3（`SECURITY.md` §2），Zebra 有 Allow/Ask/Deny。**它们不是同一层**：

- `Risk` 是**工具的编译期属性**（Zebra 没有，用工具名硬编码在 `commandTools`/`writeTools`/`readPathTools` 三个 map 里）。
- `Verdict` 是**单次调用的判定结果**（Zebra 的核心，Zex 还没有）。

叠法建议：

```rust
// 第 0 步：静态风险给出默认判定
fn default_verdict(risk: Risk) -> Verdict {
    match risk {
        Risk::L0ReadOnly   => Verdict::Allow,
        Risk::L1Reversible => Verdict::Allow,        // 但走自动快照
        Risk::L2Impactful  => Verdict::Ask,
        Risk::L3Destructive=> Verdict::Ask,          // 且 require_double_confirm
    }
}

// 第 1 步：硬 Deny 表（不可配置、不可被 grant 覆盖、不可被 skip 覆盖）
// 第 2 步：具体规则（rm 解析 / 受保护路径 / 工作区边界）→ 只能**上调**风险，不能下调
// 第 3 步：grant 匹配（工具 + 参数 glob）→ 只对 L0..L2 有效，L3 永不匹配
// 第 4 步：模式约束（plan 模式把 L1+ 全变 Deny）
```

**关键约束：规则只能上调风险等级，不能下调。** Zebra 的 `Config.Tools { bash: "allow" }` 违反这条，是新项目不要复刻的唯一一处。

Grant 匹配（对齐 `SECURITY.md` §3.2 的 glob 思路，但要防绕过）：

```rust
pub struct Grant {
    pub tool: ToolName,
    pub pattern: GrantPattern,   // glob，如 "shell:cmd=git *"
    pub scope: GrantScope,       // Session | Persistent
}

// 匹配前必须归一化：
// - 路径：绝对化 + symlink 解析 + `..` 消解（std::fs::canonicalize，不存在时对父目录做）
// - 命令：不做 shell 展开（展开就是漏洞），原串 + argv 双模式
// - 默认不匹配 = 拒绝
// - grant 只对 L0..L2 有效，L3 永不匹配；且写 grants 表而非进程内存，才能「可撤销」
```

硬 Deny 表（照搬 Zebra 的语义）：`SYSTEM_DIRS = [/etc /usr /var /bin /sbin /boot /lib /opt /sys /proc /dev /System /Library]`；`PROTECTED_WRITE_GLOBS` 除 Zebra 那 17 条外，**必须包含 `~/.zex/.env` 与 `~/.zex/config.json`**（能写自己的策略文件 = 没有策略）。

---

## 5. 自我进化

### 5.1 现状是怎么做的

代码分两块：

- `internal/evolution/`（516 行）：**会话结束后**判官打分 + 蒸馏一张卡片。真正的"知识沉淀"。
- `internal/selfevolve/`（约 2k 行）：**SAV 循环**（Self-Aware → Act → Verify → Distill → Continue），会改自己的 Go 源码、跑测试、编译。

#### 循环协议（`cycle.go:RunCycle`）

```
SelfAware  → LLM 分析能力缺口，产 1~3 个提案（JSON）
Act        → 快照目标文件（内容 + sha256）→ LLM 生成代码 → 解析 === FILE: === 块 → 写盘
Verify     → 五级验证，任一级失败 → 回滚快照 + status=failed
Distill    → 写人类可读 journal + LLM 生成 skill 卡片
Continue   → go build -o build/zebra 验证；写 checkpoint(status=completed, next=recompile_and_restart)；打印"请手动重启"
```

`MaxCyclesPerRun` 默认 3；`DryRun` 只跑 SelfAware 就返回 `skipped`。

#### 五级验证（`verify.go`）

| 级别 | 实现 | 有效性判断 |
|---|---|---|
| `compile` | `go build ./...` | ✅ 真实 |
| `unit_test` | `go test ./... -count=1 -timeout=120s` | ✅ 真实 |
| `contract_test` | 把提案里每条 acceptance criterion 的 `TestCode` 拼成一个 `selfevolve_contract_<id>_test.go`，跑 `go test -run 'TestContract_<id>'`，再用字符串 `--- PASS: ac-1` / `--- FAIL: ac-1` 判每条是否通过 | ⚠️ 依赖模型写出可编译的 Go 测试代码，且判据是 stdout 字符串 |
| `regression` | **再跑一遍 `TestCmd`**（和 unit 完全相同） | ❌ 重复劳动 |
| `behavior` | 读生成的文件，做字符串检查：有 `package ` 吗？代码行 ≥3 吗？`_test.go` 里有 `func Test` 吗？`pkg/tools/` 下的文件引了 `pkg/agent` 吗？有自己定义 `AgentTool` 接口吗？ | ❌ 这不是行为检查 |

`behavior` 那几条的内容本身是有信息量的：它记录了这个代码生成器**反复犯同一个结构错误**（自己定义 `AgentTool` 接口而不是用 `agent.AgentTool` 结构体）。也就是说这级检查是用"字符串包含"给"模型改不动自己的坏习惯"打补丁。作为验证机制它无效，作为**踩坑记录**它有价值。

#### 护栏

- `Act` 阶段：拒绝绝对路径、含 `..`、非 `.go` 后缀（`validateGeneratedPath`）。
- 快照：`Snapshot{Files map[string][]byte, Checksums map[string]string}`，新建文件记 `nil`，回滚时 `nil` → 删除。
- 每阶段落 `checkpoint.json`（`<memory>/selfevolve-checkpoint.json`）：`Status ∈ {idle, self_aware, acting, verifying, distilling, needs_restart, completed}` + `Snapshot` + `PendingProposal` + PID + BinaryPath。
- 崩溃恢复（`ShouldResume`）：`status ∈ {acting, verifying, distilling}` 且有 snapshot → **启动时先回滚**。
- 提案质量门（`annotateAndValidateProposals`）：description / approach / files / acceptance_criteria 任一为空 → `rejected`。
- journal：每个 cycle 一个 md（提案/动机/方案/涉及文件/验收标准/验证结果表/错误详情/经验教训）+ `journal.md` 追加一行摘要；SelfAware 下一轮读最近 30 行。

#### 自动重启这件事，实际是关掉的

`recompile.go` 里有完整的 `RecompileAndRestart`：构建 `bin.new` → `os.Rename` 覆盖原二进制 → `os.StartProcess` 起新进程 → `os.Exit(0)`。

但 `RunCycle` → `finishSuccessCycle` **只构建不重启**，然后打印：

> 📋 下一步：请手动重启进程以加载新能力

**这是整个 selfevolve 包里最重要的一条经验**：自我替换二进制 + 自重启被放弃了，退回到"构建验证 + 人工重启"。理由可以推断（源码没直说）：`os.Exit(0)` 会丢掉上游 HTTP 连接、会话状态和用户的期待，而新二进制起不来时用户面对的是一个死进程。

#### 会话后蒸馏（`internal/evolution/`）

- 门槛：会话 trace ≥3 条，且 `session.Status == completed`。
- 判官（`judge prompt`）：7 个维度（频率/复杂度/成功率/普适性/规划价值/审查价值/干预价值），**score < 6 或 type=none 就丢弃**。
- 判官 prompt 里最值钱的是**分类指南**：
  - Skill：步骤复杂、多工具协作、可跨项目复用、频率高。
  - Experience：任务简单但含一个"容易忽略的细节/坑"、涉及特定工具的边缘情况、成功路径里有"绕开限制"的技巧、thinking 里出现"差点走错但纠正了"、用户中途纠正过。
  - "小场景 ≠ 不值得记"。6–7 分的小场景默认 Experience，只有 8+ 才 Skill。
  - 出现 `USER_INTERVENTION` 事件 → 优先提 Experience，**分数至少 +2**。
- 蒸馏完先 `NormalizeCard` 再 `CardProblems`，欠佳也存。

### 5.2 该继承的

1. **快照 → 改动 → 验证 → 回滚 这个四步闭环**，以及"验证失败必须回滚到字节级一致"的严格性（`Files` + `Checksums`，新建文件回滚 = 删除）。
2. **checkpoint 每阶段落盘 + 崩溃恢复时先回滚。** 这是"自我修改"能安全存在的前提：进程随时可能死在中途。
3. **提案质量门**（四个必填字段任一为空即拒绝）。模型很擅长产出"标题正确、内容为空"的提案。
4. **"不改就不算完成"的验证纪律**：至少 `compile` + `test` 两级必须真实执行，不能靠模型自述。
5. **journal 人类可读 + 下一轮读回最近 30 条。** 这是唯一让循环不重复提案的机制（Zebra 用它防"反复提同一个能力"）。
6. **判官的分类指南与 +2 规则。** 这套 prompt 是多次调优的产物，直接可复用。
7. **"欠佳也存 + 独立修复通道"**（见 §2.2）。
8. **自我重启被放弃这个结论。** 新项目不必再试一次。

### 5.3 该扔的

1. **`contract_test` 的生成方式。** 让模型写 Go 测试代码再拼装成 `_test.go`，等于把"验证"建立在"被验证对象的输出"上。正确形状：验收标准用**结构化断言的 DSL**（如 `Assert::FileContains{path, needle}` / `Assert::ExitCode{cmd, 0}` / `Assert::JsonPath`），由 harness 自己执行，模型只填参数。
2. **`regression` 级**：和 `unit` 跑同一条命令，纯装饰。
3. **`behavior` 级**：字符串包含不是行为验证。它要做的事（"别自己定义 AgentTool 接口"）应该由 `cargo check` 里的 trait bound 强制，而不是生成后再拿 grep 查。
4. **`recompile.go` 整份**（97 行）。桌面应用自替换二进制在 macOS（代码签名 / GateKeeper）和 Windows（文件锁）上基本不可行，Linux 上也只是"能跑但不该跑"。
5. **`internal/curator/` 的三级级联阈值**（5 → 5 → 5，动作数 5/10/20，删除比例 0/0.3/0.3）。这套数字没有任何证据支撑，且"每 5 次抽取触发一次 LLM 策展"在桌面应用里是持续烧钱。要做合并去重，用**确定性的相似度阈值**触发（见 §5.4），不要用"计数到了就叫模型"。
6. **`actSystemPrompt` 里那一大段"必须遵循的 Go 代码模式示例"。** 它是 Go 专属的，且暴露了"模型需要看 30 行样板才写对"这一事实。Rust 侧应该让样板由 `schemars` 派生自动给出。

### 5.4 Rust 侧建议（`zex-evolve`）

**先降级目标。** 第一版不做"自动改代码"，只做"提案 + 人工批准 + 落地为记忆卡片/技能"：

```rust
pub struct Proposal {
    pub title: String,
    pub problem: String,            // 观察到什么具体失败
    pub evidence: Vec<EvidenceId>,  // 指向会话/审计事件的指针；不给证据的提案不收
    pub approach: String,
    pub targets: Vec<PathBuf>,      // 只允许 workspace 内、只允许 .rs
    pub acceptance: Vec<Assert>,    // 结构化断言，不是代码字符串
    pub status: ProposalStatus,     // Proposed → Approved → Applied → Verified → Rejected
}

pub enum Assert {
    CargoCheckOk,
    TestPasses { filter: String },
    FileContains { path: PathBuf, needle: String },
    ExitCode { cmd: Vec<String>, want: i32 },
}
```

护栏（意图照搬 Zebra，值按新项目定）：`MAX_PROPOSALS_PER_RUN = 3`、`MAX_FILES_PER_PROPOSAL = 8`、`MAX_PATCH_BYTES = 200 KiB`，加一条 Zebra 没有的 **`FORBIDDEN_TARGETS`**：`Cargo.toml` / `Cargo.lock` / `rust-toolchain.toml` / `crates/zex-policy/` / `crates/zex-core/src/tool.rs`，写死在代码里而不是配置里。Zebra 只禁止"非 .go 文件"和"绝对路径"，理论上模型可以改自己的验证器。**自我进化的第一护栏是"不能改验证自己的代码"。**

**去重是必须的。** 现有数据就是证据：`~/.agent/memory/experiences/` 下四张卡片里有三张是同一件事（Electron 项目"运行"后的窗口存活诊断，时间跨度两天）。缺的不是判官，是"落盘前查重"：

```rust
// 落盘前：embedding 余弦相似度
//   ≥ 0.92 → 不新建，走 merge（正文追加"补充"段，tags 取并集，updated_at 刷新，hits 相加）
//   0.85..0.92 → 新建但标 duplicate_of，等人工确认
//   < 0.85 → 正常新建
```

这条比 `curator` 那套"5 次触发 LLM 策展"便宜两个数量级，且确定性。

---

## 6. 项目配置信任门（`pkg/trust`）

### 6.1 现状是怎么做的

一句话：**项目级配置默认不生效，除非有一个来自仓库之外的信号说我信这个目录。**

**信号只有两个**（`trust.go:ProjectTrusted`）：
1. 环境变量 `ZEBRA_TRUST_PROJECT` ∈ `1/true/yes/on`；
2. 全局名单 `~/.agent/trusted-projects`（每行一个绝对路径，`#` 注释，文件权限 0600）。

`cwd` 为空 → 永不信任。

**它防的具体攻击面**（`project_configs.go:projectConfigProbes` + MANUAL §4.2）——这张表是关键，因为它穷举了"一个 `.agent/` 目录能干什么"：

| 文件 | 攻击 |
|---|---|
| `.env` | 注入 `PATH` / `LD_PRELOAD` → 之后**任何**子进程被劫持 |
| `models.json` | 把内置模型的 `baseUrl` 指向攻击者服务器 → **偷 API Key** |
| `mcp.json` / `mcp_servers.json` | 声明一个 MCP "服务器" = 声明一条要执行的命令 |
| `workflows/` | 工作流节点可执行命令 |
| `cron-tasks.json` | 定时任务的提示词会**自主执行**（不需要用户在场） |

**明确不受门禁的**：`config.json`（"只含模型选择与是否显示工具调用，风险低"）与**全局层的一切**。

**两条无条件例外**（即使项目已被信任也照旧生效）：
1. 项目级模型不得覆盖内置模型的 `baseUrl` / `headers`；
2. 项目级 `.env` 不得设置进程控制变量 —— `dangerousEnvKeys` 34 个 + `dangerousEnvPrefixes` 4 个（`LD_` / `DYLD_` / `GIT_CONFIG_` / `BASH_FUNC_`）。

清单里值得单独说的几类：解释器入口（`NODE_OPTIONS` / `PYTHONSTARTUP` / `PYTHONPATH` / `PERL5LIB` / `PERL5OPT` / `RUBYOPT` / `RUBYLIB` / `JAVA_TOOL_OPTIONS` / `_JAVA_OPTIONS` / `JDK_JAVA_OPTIONS`）、git 钩子与分页器（`GIT_SSH*` / `GIT_EXTERNAL_DIFF` / `GIT_PAGER` / `PAGER` / `EDITOR` / `VISUAL`）、shell 入口（`IFS` / `ENV` / `BASH_ENV` / `PROMPT_COMMAND` / `SHELL` / `TMPDIR` / `SSH_AUTH_SOCK`）、以及 **`ZEBRA_GUARD` / `ZEBRA_GUARD_CONFIG`**。

最后一对是**后来补的**：源码注释明说 `ZEBRA_GUARD_CONFIG` 曾经不在清单里，于是"被信任项目的 `.env` 可以把它指向一份 `{"enabled": false}`，从而把护栏整个关掉"。这条教训的普适形式是：**凡是能削弱安全策略的开关，都必须进同一份白名单审核**（Zex 侧对应 `ZEX_POLICY_*`）。

**三个实现细节**，每个都对应一次 bug：

1. **`WarnUntrusted` 先判存在再打警告**（`configPresent`）。在没有配置的目录里也打"已忽略"等于摧毁了"没有配置"和"配置被忽略"的区别。源码注释直接点名 `.env` 曾在机器上每个目录都告警。
2. **标签用导出常量**（`WhatDotEnv` / `WhatModels` / `WhatMCP` / `WhatWorkflows` / `WhatCron`）。各 loader 自己传字符串 → 首屏提醒和日志警告说的东西漂移。
3. **`IgnoredFor(cwd)` 注册表**：TUI 把 slog 重定向到文件（避免破坏渲染），所以用户看不到警告、只看到"任务列表是空的"——看起来像 loader 坏了。要有独立的内存记录供 UI 查询。

**`canonical(path)`**：`Abs` + `EvalSymlinks` + `Clean`。信任一个目录的各种写法都指向同一处。

### 6.2 该继承的 —— 几乎全部

这是六项里**最应该整套照搬**的一块。信任门的价值 100% 在"清单完整"和"信号来源正确"上，重写时少考虑一项就是一个洞。

1. **默认不信任 + 信号必须来自仓库外。** 一句话：仓库里的任何东西都是攻击者可控的，包括"我信任我自己"这句话。
2. **`projectConfigProbes` 那张表的具体内容。** 它是一份攻击面清单，不是配置清单。
3. **两条无条件例外**（端点不可覆盖 + 进程控制变量不可设），以及"例外不受信任门影响"这个设计。
4. **`dangerousEnvPrefixes` 四个前缀 + 名单里"能削弱护栏的开关"这一条纪律。**
5. **`WarnUntrusted` 的存在性前置判断 + 导出标签常量 + `IgnoredFor` UI 查询接口。**
6. **`canonical` 路径归一化。**
7. **allowlist 文件 0600 + 自解释的文件头注释。**
8. **`TrustSource(cwd)`** 返回 `""` / `"ZEBRA_TRUST_PROJECT=1"` / `"allowlist:<path>"` —— 状态展示要能说清"凭什么信"。

### 6.3 该扔的

1. **环境变量 `ZEBRA_TRUST_PROJECT=1` 作为全局开关。** 在 CLI 里它合理（"这条命令我信"）；在**常驻桌面应用**里它是错的：一个进程级环境变量会影响该进程之后打开的所有项目。Zex 是常驻的，必须去掉这个信号，只保留 allowlist，并把它做成 UI 上可点的按钮。
2. **"信任变更不改变当前会话，需要重启"。** MANUAL §4.2 明说"各配置在启动时一次性读入内存，没有运行期重载路径"。桌面应用必须支持运行期重载（用户点"信任此项目"之后立刻生效），否则体验是"我点了没反应"。
3. **`config.json` 豁免。** Zebra 认为它只含模型选择，风险低。但 Zex 的配置里会有 `guard.json` 级别的策略（默认风险档、审批开关），一旦项目级可写就是洞。**豁免清单要重新审一遍，不要照抄。**
4. **每行一个路径的纯文本 allowlist。** 需要记录"什么时候信的、信的是什么"（尤其如果将来要做"只信任配置、不信任 hooks"的分级），用表更合适。

### 6.4 Rust 侧建议

```rust
// zex-policy/src/trust.rs
pub struct ProjectTrust { pub root: PathBuf, pub granted_at: OffsetDateTime, pub via: TrustVia }

/// 项目级需要信任门的东西。新增一项 = 改这个枚举（编译器会提醒你改 list()）。
pub enum GatedConfig { DotEnv, Models, Mcp, Workflows, CronTasks }

/// 无条件生效（与信任无关）的两条规则
pub fn env_key_forbidden(key: &str) -> bool;   // 34 键 + 4 前缀，含自削弱开关
pub fn may_override_builtin_endpoint() -> bool { false }   // 恒定 false

// 存储（全局 DB，不在项目目录——因为要带 granted_at 与 via）：
// trusted_projects(root TEXT PRIMARY KEY, granted_at TEXT, via TEXT)
```

**行为契约（三条，写进测试）：**
1. 未信任项目里存在 `GatedConfig` 任一项 → 该项**不被加载** + UI 首屏列出被忽略项 + 一条日志。
2. 未信任项目里**不存在**任何 `GatedConfig` → **零告警**（否则每个目录都告警）。
3. 信任某项目时，`.env` 里含 `PATH`/`LD_PRELOAD`/`ZEX_POLICY*` 等键 → 该键被单独丢弃并告警，**其余键正常生效**（不是整个文件作废）。

---

## 7. 稳定契约（来自 `AGENTS.md` / `docs/MANUAL.md`）

这一节只提炼**跨实现仍然必须成立**的约定。

### 7.1 目录与优先级

```
<项目>/.agent/          项目级，随仓库走，默认不受信任
~/.agent/               全局级，永不受信任门限制
同名配置：项目级覆盖全局级
```

Zebra 的目录内容（MANUAL §4.1）里，**新项目会保留的形状**：`config.json`、`.env`、`models.json`、`mcp.json`、`skills/`、`memory/`（`experiences/` `skills/` `.archive/`）+ `memory.db`、`workflows/`、`cron-tasks.json`、`agents/<id>/agent.md`、`system_prompt.md`、`runtime/session_<id>/`、`logs/`。

**该扔的**：`memories/*.md`（§2.3）、`.chorus/`（任何形态）、`ZEBRA.md` 回退、`mcp_servers.json` 兼容格式、`agents/<id>/{config.json,meta.json}` 兼容格式。

### 7.2 指导文件 vs 系统提示词

Zebra 明确区分两条线（MANUAL §6.8）：

- `system_prompt.md`（可被 `--system-prompt-file` / `--system-prompt` 覆盖）**替换**系统提示词；
- `AGENTS.md`（回退 `ZEBRA.md`）**不替换**任何东西，作为上下文注入，让 agent 遵守项目规范。

这个区分值得保留（`internal/config/config_guide.go:14`）：项目里有 `AGENTS.md` 就该被读到，但不该因此丢掉产品的系统提示词。优先级：`--system-prompt-file` > `--system-prompt` > `<config>/system_prompt.md` > 内置默认。

### 7.3 CLI 的安全惯例 —— 这部分必须照搬

`zebra memory` 一族命令的语义是整个 Zebra 里最成熟的运维设计，逐条：

| 惯例 | 具体表现 |
|---|---|
| **默认预演，`--apply` 才执行** | `memory prune` 默认只打印计划；`memory normalize` 默认只报告（`NormalizeCards(ctx, apply=false)`） |
| **必须给出条件** | `--orphans` / `--unused` / `--older-than` 至少一个，否则报错 |
| **危险组合被禁止** | `--unused` 必须配 `--older-than`（否则选中全库） |
| **大批量需二次确认** | 一次归档超过半数 → 拒绝执行，提示加 `--force` |
| **不删除** | 归档到 `memory/.archive/<时间戳>/`，可 `--restore <id>` 找回 |
| **保留期语法** | `--older-than 180d` / `12w` / `36h`。`ParseRetention` 支持 `d`/`w`/`h`，**不支持 `y`**，而且 `FormatAge` 为此特意不输出 `y`（源码注释：报告不能建议一个命令会拒绝的值） |
| **预演输出说明意图而非暗示** | 每个候选标 `[归档]` 或 `[纳管]` |
| **同一份报告只写一次** | `PrunePlan.Preview()` 放在 memory 包里，让 CLI 和 TUI 无法各自描述同一个计划（注释原话："Two descriptions of one destructive action is one too many"） |
| **修复建议要分情况** | `AuditReport.RepairHint()` 按问题类型给不同命令，不是无脑附一句 "run verify --repair" |

**新项目要继承的是这套惯例本身**，不是命令名。Zex 侧只需要：`list|get|search|delete|prune|archive|verify|stats`，其中 `prune` 接受 `--unused` / `--older-than 180d` / `--type exp` / `--apply` / `--force`。并把"默认 dry-run + `--apply`"做成**一个共用的 CLI 类型**（`DryRunnable`），而不是每个命令自己实现——否则迟早有一个命令忘记默认预演。

### 7.4 一个被低估的工程资产

Zebra 有 `cmd/equivcheck`（结构重构的机械校验：比对两侧源码的包级特征多重集——字符串字面量、导出标识符、非注释代码行）+ `internal/docscheck`（校验手册的围栏成对、锚点有效、示例可编译、示例文件与文档逐字一致）+ `make verify-docs-live`（对真实二进制实跑手册承诺过的行为，**不需要模型**）。

这三件事的动机都是同一个：**"零逻辑变更"这类承诺靠人眼读 diff 无法保证**。Zex 是重写项目，短期内不会有等价重构，但 `docscheck` 的思路立刻可用：把本文档里那些数字（1200 / 40 / 0.70 / 0.85 / 3000 / 3 / 30 天 …）做成**编译期常量**，并写一个测试断言"文档里的数字 = 代码里的常量"。Zebra 里已经出现过文档建议一个命令会拒绝的值的 bug（`FormatAge` 的 `1y`）。

---

## 8. 真实经验卡片分析（`~/.agent/memory/experiences/`）

四张真实卡片，挑三张看格式。

### 8.1 三个样例

**① `exp-electron-duplicate-launch-verify.md`**（40 行）
`type: experience`，`severity: medium`，tags 6 个（electron/vite/进程管理/窗口激活/wayland/xdotool）。
四段式：`## 问题上下文` / `## 问题表现` / `## 解决方案`（编号 1–4）/ `## 核心教训`（一句话）。

**② `exp-electron-dev-window-liveness-check.md`**（47 行）
同结构，tags 6 个。区别是解决方案里有**分层的验证清单**（端口 / 进程 / 窗口 / 日志，逐条给命令）。

**③ `exp-rewrite-audit-crossplatform-arch-validation.md`**（40 行）
同结构，tags 5 个（架构设计/技术选型/跨平台/代码盘点/需求澄清）。这条是"方法论"型经验，不是"故障排查"型。

### 8.2 格式观察
| 特征 | 值 |
|---|---|
| frontmatter | `id` / `name` / `description` / `tags` / `created_at` / `updated_at` / `type` / `severity` —— 与 `memory.Frontmatter` 完全对齐，且 `type: experience` **写了**（扫描器把缺 type 视为 skill，这一步不能省） |
| `id` | 文件名（去 `.md`）= frontmatter `id`，前缀 `exp-` 或 `skill-` |
| `name` | 一句中文，含领域 + 动作（"Electron 开发模式重复启动指令与窗口存活诊断"），**不是章节名** |
| `description` | 一句，写**触发条件 + 要做什么**，无一例空 |
| 正文 | 恰好四段，标题固定 |
| 长度 | 40–47 行，正文 35–40 行 |
| 时间戳 | `created_at` 带 `+08:00`、`updated_at` 是 `Z`（UTC）—— 不一致，说明写入侧没统一格式 |

### 8.3 内容上的共同特征（这才是真正的"写作规范"）

1. **上下文写具体到可复现**：`concurrently`、端口 5176、`DISPLAY=:1`、`--ozone-platform=x11 --disable-gpu`、`XDG_SESSION_TYPE=wayland`。没有一句"某个 Electron 项目"。
2. **现象用时间线**：第一次怎样、第二次怎样、第三次发现了什么。
3. **指出"看起来正常但其实坏了"的那个指标**：端口 200 + 进程在 = 假象，决定性证据在窗口层（`xwininfo` 报 `BadWindow`）。
4. **把用户的重复指令当成信号**："同一条指令出现第二次以上，默认含义是'结果与我预期不符'"——这是一条关于**如何解读用户行为**的经验，比任何命令都有价值。
5. **解决方案给命令不给结论；核心教训一句话，且是可执行的判断规则**，不是"要注意"。

### 8.4 该继承 / 该扔

**继承**：四段式模板 + `description` 必须写触发条件 + `id` 用语义化 slug + 正文具体到命令与数值 + 结尾一句话教训。**该扔**：`severity`（没有消费方，见 §2.3）；时间戳统一成 UTC。

**必须修的一件事**：四张卡片里三张是同一天同一件事的三个版本。这是**缺查重**，不是缺判官——判官只看单次会话，它无法知道库里已有一张几乎一样的卡。方案见 §5.4。

Rust 侧卡片模板（写进 `zex-evolve` 的蒸馏提示词）：

```markdown
---
id: exp-<slug>
kind: experience
title: <领域 + 动作，禁止用章节名>
summary: <触发条件 + 教训，一句话>
tags: [..]
importance: 3
created_at: <RFC3339 UTC>
---

## 情境
<具体到可复现：版本、端口、环境变量、命令>

## 现象
<时间线；明确指出哪个指标具有误导性>

## 处理
<编号步骤，每条给命令>

## 教训
<一句话，可执行的判断规则>
```

---

## 9. 踩过的坑清单

按"新项目会重犯"的概率排序，不按模块。每条都标了出处。

### A. 检索与存储

1. **FTS5 默认 tokenizer 会把整段中文当一个 token** → 搜「滚动」搜不到「聊天窗口滚动跳动」。修法：`tokenize='trigram'` + 查询按 3 字滑窗切 + <3 字走 `LIKE`。（`store_schema.go:ftsTokenizer`、`store_text_query.go:planTextQuery`）
2. **`CREATE VIRTUAL TABLE IF NOT EXISTS` 不会替换已存在的 tokenizer**。升级二进制后旧库还在用 `unicode61`，中文检索继续坏，而 `IF NOT EXISTS` 一声不响。修法：读 `sqlite_master` 比对 DDL，不一致就 DROP + 重建 + `rebuild`。（`store_schema.go:migrateFTS`）
3. **中文查询用 AND 语义匹配不到任何东西**。任务描述是一句句子，不是关键词表。修法：OR + Go 侧加权排序。（`store_query.go:searchByText` 注释）
4. **向量分与 BM25 分量纲不同，直接相加是错的**。修法：RRF（`1/(60+rank)` 求和）。（`store_query.go:fuseRanks`）
5. **查询出错与"没有匹配"长得一样**，因为 `List` 吞掉了 error，检索 bug 无法上报。（`store_query.go:List` 注释）
6. **`access_count = 0` 有两个成因，单次读数分不清**（没用 vs 还没轮到）。修法：每日快照 + 观测窗口 + 趋势 + ≥30 天才判稳定。（`store_stats.go` 全篇）
7. **`--unused` 单用会选中整个记忆库**，因为计数从 0 开始。实测 `--unused --older-than 90d` 选中 13/14 张。（`store_prune.go:Validate` + `pruneForceRatio` 注释）
8. **`--unused --older-than N` 与"故意清空记忆库"在数据上无法区分**。修法：过半需 `--force`。（同上）
9. **归档目录必须以点开头**，否则 `scanOrphanFiles` 会把它扫回来，"prune 之后 verify 仍不 clean"。（`store_prune.go:archiveDirName` 注释）
10. **归档里常常是唯一副本**（curator 是先复制再删原件），所以不能有自动过期、不能有 `--drop`。（`store_archive.go` 包注释）
11. **扫描范围外的卡片文件既不索引也不报错**，`audit` 一直显示 clean。（`store_prune.go:scanOrphanFiles` 注释）
12. **只有 `Put` 时写协议镜像** → 导入的卡片没有副本、删掉的卡片留下孤儿副本、`normalize` 之后副本过期。（`store_protocol.go` 包注释，实测"14 张卡片 + 空 memories/ 目录"）
13. **`rm -rf dir/*` 剥掉 `/*` 后要按目录判定**，否则 `rm -rf /*` 看起来就是一个普通路径。（`policy.go:removeTargets`）

### B. 文件格式与序列化

14. **手写 frontmatter 写入 + 逐行解析**：`id: %s` 不转义、其他值用 `%q`，含冒号的 id 或含转义引号的 name 回读后值变了。修法：`yaml.Marshal` 写、`yaml.Unmarshal` 读、失败降级逐行解析。（`store_markdown.go`）
15. **body 无条件追加换行** → 每读回重写一次就多一个空行。修法：`TrimRight("\n")` + 恰好一个 `\n`。（同上）
16. **协议副本的 id 被 sanitize 成文件名** → 中文 id 全变下划线，副本无法回指卡片，且它宣称的 id 也不是真实 id。修法：文件名字符串清洗，frontmatter 里写真 id。（`memories.go:renderProtocolMemory` 注释）
17. **文件名 sanitize 导致两个不同 id 撞同一个路径** → 静默覆盖另一张卡。修法：源文件用原 id 作文件名。（`store.go:writeCardFilesLocked`）
18. **yaml tag 缺 `omitempty`** → 每张卡多 6 行 `""`/`[]`/`null` 噪声，技能卡还会宣称 `complexity_score: null`。（`store.go:Frontmatter` 注释）
19. **`type` 与 `Frontmatter.Type` 两个字段** → 只设一个时，卡片写进 `experiences/` 却读回成 `skill`。（`store.go:Put` 注释）
20. **模型会把 YAML frontmatter 包在 ```markdown 围栏里**，且必须在检查 `---` 前缀**之前**剥围栏，否则整块围栏（连带反引号）被当成正文。（`evolution_parse.go:parseDistilledContent`）
21. **`FormatAge` 输出 `1y`，而 `ParseRetention` 不认 `y`** —— 报告建议了一个命令会拒绝的值。（`store_stats_test.go:155` 注释）

### C. SQL 细节

22. **UPSERT 的源是 SELECT 时，不带 WHERE 会被 SQLite 解析成消费 ON CONFLICT 子句**，报 `near "DO": syntax error`。修法：`SELECT ... WHERE true`。（`store_schema.go:recordUsageSnapshotLocked` 注释）
23. **迁移只加列不清理死表**：`memory_tags` 从第一次提交起就没有任何 INSERT，一直躺在用户库里。（`store_schema.go:droppedTables`）
24. **迁移必须幂等**，因为 `memory.db` 是长期用户数据、`init` 每次进程启动都跑。（`store_schema.go:columnMigrations` 注释）
25. **DDL 里永远不要插值用户输入**（表名/列名只来自编译期常量）。（同上，注释出现两次）

### D. 上下文与提示词

26. **提示词索引曾无条件注入所有 experience，且完全没注入 skill** → 蒸馏出来的技能一轮都没生效过。（`internal/kernel/memory_index.go:buildMemoryIndexBlock` 注释）
27. **索引字段漏 XML 转义**：`severity` 含 `"` 会提前关闭属性，含 `<` 会破坏整块嵌套，模型拿到的是一段被破坏的提示词。（`memory_index.go:formatMemoryEntry` 注释）
28. **蒸馏模型把正文第一个标题当 name** → 五张卡片都叫「使用场景」，description 全空 → 全部不可检索。（`quality.go` 包注释、`evolution.go:cardFieldRequirements`）
29. **压缩后 tool_call / tool_result 配对会断** → API 400。（`compressor_repair.go`）
30. **摘要为空的降级不可接受**，必须永远返回可用摘要。（`compressor_fuse.go` 注释）
31. **双 token 计量导致重复累加，压缩提前触发**（§3.3 第 1 条）。

### E. 权限与审批

32. **规则表版 `rm` 检查被 `--` 骗过**：`sudo rm -rf --no-preserve-root /` 被读成普通 sudo，因为 flag 模式停在第一个双横线。（`rules.go:rmInvocationRe` 注释）
33. **用户改了参数却按原参数执行** = 给了"批准 ls，跑 rm -rf"的旁路。修法：改后必重验，且重验不能调 `Authorize`（`askMu` 非可重入 → 死锁）。（`guard.go:AuthorizeWithoutAsking` 注释）
34. **按规则族记忆的批准会放行"修改前"的参数**（用户只对改后的那条点过头）。修法：改了参数的批准不入记忆。（`guard.go:ask` 注释）
35. **并行工具批次同时弹三个确认框**，用户分不清答案归属。修法：`askMu` 串行化提问，只让需要人的调用排队。（`guard.go:askMu` 注释）
36. **审批队列每会话只有单槽位** → 并行批次两条 gated 调用互相覆盖，第一条的 goroutine **永久挂起**。（`approval.go` 包注释）
37. **"当前输入是不是一个审批回答"判断错** → 输入进了错误分支，agent 永久阻塞（ask 工具的 v0.8.3 事故）。（`approval.go:IsApprovalPending` 注释）
38. **`ZEBRA_GUARD_CONFIG` 曾不在危险键清单里** → 被信任项目的 `.env` 能指向 `{"enabled": false}`，把护栏整个关掉。（`trust.go:dangerousEnvKeys` 注释）
39. **worker 每个任务重建一次 agent** → 逐任务重复同一条"护栏被削弱"告警，淹成背景噪。修法：`sync.Once` 只去重日志，判定照常每次执行。（`config.go` 注释）
40. **`~` 只认一种写法会被绕过**，模型还会写 `$HOME` 和 `${HOME}`。（`rules.go:expandUserPath`）
41. **护栏的路径解析必须与工具一致**，否则 `../../etc/passwd` 判的是另一个文件。（同上注释）
42. **`rm -rf` 指到工作区外**比 `rm -rf /` 更可能发生，且没有 git 可恢复 → 必须 Ask，不能只有 Deny。（`policy.go:removeAsk`）
43. **计划模式与护栏两套拦截** → 检查顺序必须是"模式门（map 查询）→ 护栏（可能弹窗）"，否则会先弹框再说"模式不允许"。

### F. 信任门

44. **在没有配置的目录里也打"已忽略"告警** → 摧毁"没有配置"和"配置被忽略"的区别（`.env` 曾在机器上每个目录都告警）。（`trust.go:WarnUntrusted` + `configPresent`）
45. **各 loader 自己传字符串标签** → 首屏提醒和日志警告说的东西不一致。修法：导出常量。（`project_configs.go` 注释）
46. **TUI 把 slog 重定向到文件** → 用户在 UI 里看不到"配置被忽略"，只看到"列表是空的"，看起来像 loader 坏了。修法：独立注册表 + `IgnoredFor(cwd)`。（`trust.go:ignored` 注释）
47. **"用 `probe` 而不是报告 loader 恰好警告过什么"**：靠 loader 自报，会在代码路径变化时恰好漏掉提醒。（`PresentProjectConfigs` 注释）

### G. 自我进化

48. **进程可能死在中途**（acting/verifying）→ 启动时必须先按 checkpoint 里的 snapshot 回滚。（`selfevolve/checkpoint.go:ShouldResume`）
49. **自动替换二进制 + 自重启被放弃**，退回"构建验证 + 人工重启"。`recompile.go` 整份还在但不在主路径上。（`cycle.go:printSuccessInstructions`）
50. **`regression` 级与 `unit` 跑同一条命令**，纯装饰；`behavior` 级是"文件里有没有 `package ` 字样"。（`verify.go:runRegressionTests/runBehaviorCheck`）
51. **判官只看单次会话，无法知道库里已有近似卡片** → 同一天产生三张几乎一样的经验卡。缺的是落盘前查重。（`~/.agent/memory/experiences/` 实测）
52. **模型反复犯同一个结构错误**（自己定义 `AgentTool` 接口而不是用现成结构体），才需要在提示词里贴 30 行样板、在 `behavior` 里字符串检查。**这说明"用提示词约束生成代码的结构"是无效的**，应该由类型系统约束。（`selfevolve/act.go:actSystemPrompt`、`verify.go:runBehaviorCheck`）

### H. 工程护栏（元教训）

53. **"零逻辑变更"这类承诺靠人眼读 diff 无法保证**。Zebra 为此写了 `cmd/equivcheck`（比对包级特征多重集：字符串字面量 / 导出标识符 / 非注释代码行）和 `internal/docscheck`（手册的围栏、锚点、示例可编译、示例文件逐字一致）。
   **对新项目的直接启示**：把本文档的数字做成编译期常量，并写测试断言"文档 = 常量"。

---

## 附：一句话结论回收

| 项 | 判决 | 一句话理由 |
|---|---|---|
| 技能格式 | **继承布局与注入，扔实现** | 「目录 + SKILL.md + `fs.SkipDir` + 索引给路径」是对的；三套 legacy 路径和在线安装是包袱 |
| 记忆格式 | **继承（最高优先级）** | 事实源/索引分离 + 质量门 + 保守淘汰 + 观测窗口，全部由事故教出来；附带扔掉写穿镜像与四个自评分 |
| 压缩 | **继承参数，重写实现** | 0.70/0.85/10/800/3/3000 可用；双 token 计量和按条数估 token 是 bug |
| 权限守卫 | **继承（最高优先级）** | 三态不可合并、Deny 不可配置关掉、改参数必重验、无人应答默认拒绝——四条各对应一次真实事故 |
| 自我进化 | **降级继承，砍掉 3/5 验证+自重启** | 快照/checkpoint/回滚必须留；`contract`/`regression`/`behavior` 三级不构成验证；M4 再做，第一版只提案 |
| 信任门 | **继承（最高优先级，接近整套照搬）** | 价值 100% 在"攻击面清单完整"和"信号来自仓库外"；只去掉 env 全局开关、补上运行期重载 |
