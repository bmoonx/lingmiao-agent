# 元认知平台设计（需求⑤）

> 状态：**已落地（M6 · M6.0–M6.4）**（2026-09-21）
> 所属：需求⑤ —— 代码里的每个机制都能通过一个「平台」让 AI 访问、并下钻到深层
> 约束：`rules.md` 优先级最高；版本号唯一信源 = Git tag（本文不写死版本）
> 接口以代码为准（见 §10 落地记录）；本文解释设计动机。

---

## 0. 需求原文（cli）

> 「元认知机制不是给个文档放在那里让 AI 查就行了，而是**代码里的每个机制都能通过一个平台让 AI 去访问查到深层**。」

这句话否掉的是一种形态、指向另一种：

| 否掉 | 指向 |
|---|---|
| 「给 AI 一份文档让它自己翻」 | 「给 AI 一个**工具入口**，它一问就能拿到该机制当前的**运行时真值**」 |
| 表层检索（search / list） | **机制本体**的深层（排序/去重/嵌入后端/store schema/stage 图/白名单/配置解析链/事件枚举） |
| 文档 = 真源（AI 靠手抄） | **工具 = 真源**（读运行时），文档降级为人类参考 |

---

## 1. 现状落差：机制本体是黑箱

现在 AI 能摸到的只有两类入口，**都停在表层**：

| 入口 | 内容 | 深度 |
|---|---|---|
| `memory_tools`（11 个工具） | `search_memory / search_observations / search_knowledge / search_archive / search_external_memory / memory_stats / memory_kinds / list_observations / list_archive / update_memory / update_knowledge` | 只有**检索 / 列举 / 写入** |
| `help` 工具 | 读静态 `assets/help.json`（17 主题） | 本质是**一份文档** |
| `docs/*.md` | 本文同级的设计文档 | 写给人看得，AI 要自己去翻 |

**机制本体全黑** —— 下列事实目前只写在代码与文档里，AI 无法「问」，只能猜或读源码：

| 机制 | 现在的真值躺在哪 |
|---|---|
| 记忆检索：候选 → 排序 → 去重 | `lingmiao-tools/src/memory_tools.rs`（`SearchMemoryTool`） |
| 关键词排序权重 / 语义检索 | `lingmiao-memory/src/observations.rs`（`rank_by_relevance` / `search_semantic`） |
| 嵌入后端 / 维度 / 覆盖率 | `lingmiao-memory/src/embed.rs`、`lib.rs::default_embedder` |
| 三区隔离（chat/main/auditor） | `lingmiao-memory/src/store.rs`（`Zone`） |
| store schema | `observations.rs` / `knowledge.rs` / `archive.rs` / `business.rs` |
| 管线 stage 图 | `lingmiao-engine/src/engine.rs`、`lingmiao-core/assets/stages.json` |
| 每 stage 的工具白名单 | `lingmiao-core/assets/stages.json` |
| 配置解析链 | `lingmiao-core/src/config.rs`（`discover`/`read_json`）、`paths.rs`、`brand.rs` |
| 事件枚举 | `lingmiao-core/src/events.rs`（`Event`，12 变体） |

> 「**两入口一真源**」这套（需求②§6 只在 `help` 上用）要**扩到全部机制**：工具入口读运行时真源，
> 文档降级为人类参考。

---

## 2. 设计原则

1. **平台化，不是文档化**：机制以「目录 + 逐机制下钻」的形态暴露，而不是再写一份 md。
2. **运行时真源**：工具读的是**当前进程的真实状态**（配置实际解析结果、store 实际行数、当前嵌入后端…），
   不是文档里抄的期望值；文档只解释「为什么」。
3. **可下钻**：先 `list`（机制目录），再 `show(机制, 视图)` 逐层深入。
4. **复用既有机制**：遵循 Q6（`Tool` trait + `schemars`），与 `memory_tools` / `help` 平级，
   不新建抽象。
5. **默认在场**：与 `help` 一样注册进**每个 stage 的白名单**，AI 随时可问。
6. **只读优先**：平台工具不改变系统状态（无 `is_mutating`）。

---

## 3. 机制目录（catalog）

平台的核心是一张**静态目录**：每个机制一条记录，带 **id / 名称 / 一句话 / 代码位置 / 可下钻视图**。
它是 `meta_list` 的数据源，也是「文档 = 参考、工具 = 真源」里的那张索引。

| id | 机制 | 一句话 | 代码位置 | 可下钻视图 |
|---|---|---|---|---|
| `search` | 记忆检索 | 混合召回 → 排序 → 去重 → top-N | `lingmiao-tools/src/memory_tools.rs`、`lingmiao-memory/src/observations.rs` | `candidates` / `ranking` / `dedup` |
| `embed` | 嵌入 | 文本 → 384 维向量，全库共享一个 embedder | `lingmiao-memory/src/embed.rs`、`lib.rs` | `backend` / `dim` / `coverage` |
| `zones` | 三区隔离 | chat / main / auditor 三套独立 store | `lingmiao-memory/src/store.rs` | `layout` / `counts` |
| `store` | store schema | 四库表结构（obs / KG / archive / business） | `lingmiao-memory/src/{observations,knowledge,archive,business}.rs` | `schema` |
| `pipeline` | 管线 stage 图 | 组织上下文 → 工作阶段 → 沉淀阶段 | `lingmiao-engine/src/engine.rs` | `graph` / `stages` |
| `whitelist` | 工具白名单 | 每 stage 允许调用的工具集 | `lingmiao-core/assets/stages.json` | `by_stage` |
| `config` | 配置解析链 | 内嵌 → 外部覆盖 → 最终值 | `lingmiao-core/src/config.rs`、`brand.rs` | `chain` / `source` / `values` |
| `events` | 事件总线 | 单 `enum Event` + broadcast | `lingmiao-core/src/events.rs` | `variants` |
| `tools` | 工具注册表 | 分组 + 名称 + canonical schema | `lingmiao-tools/src/{tool.rs,lib.rs}` | `groups` |

> 目录是**代码里的常量**（与机制同 crate），机制改动时与之同批评审；新增机制 = 目录加一行 + 一个 `show` 分支。

---

## 4. 平台形态：`meta` 自省工具组

在 `lingmiao-tools` 新增一组与 `memory` / `business` / `help` 平级的工具组，
新文件 `crates/lingmiao-tools/src/meta_tools.rs`，`lib.rs` 里注册进 `ToolRegistry`。

### 4.1 三个工具

| 工具 | 参数 | 行为 | 真源 |
|---|---|---|---|
| `meta_list` | （无）| 列出机制目录：`id` + 名称 + 一句话 + 代码位置 | 静态 catalog（§3） |
| `meta_show` | `mechanism`（必填）、`view`（可选，缺省给该机制的默认视图）| 下钻一个机制的某个视图，返回**运行时真值** | 见 §5 |
| `meta_state` | （无）| 平台实时快照：各 store 行数、嵌入后端/维度/覆盖率、配置来源、stage 数与事件变体数 | 运行时（`Memory` / `Config` / `Paths`） |

- **只读**：三者 `is_mutating() == false`。
- **注册**：`meta_list` / `meta_show` 只依赖静态目录；`meta_state` 需 `Memory` 句柄 →
  整组随 `full_registry`（带 `Memory`）注册（对齐 `memory_tools`）；`help` 仍在 `default_registry`。
- **白名单**：三个工具加入 `stages.json` **每个 stage** 的 `tools`（同 `help` 的做法）。
- **提示**：`prompts.json._base` 追加一行「需要了解平台/自身机制时调用 `meta_*` 工具下钻，不要凭记忆猜测」。

### 4.2 为什么是「目录 + show」而不是「一机制一工具」

一机制一工具（`search_trace` / `embed_info` / `pipeline_info` …）会让工具表线性膨胀、
每加一个机制就要多加一个 schema。**目录 + 通用 `show`** 让新增机制只改 catalog 一行 + 一个匹配分支，
工具数量恒定，模型也只需记住「先 list 再 show」一条心法。

---

## 5. 逐机制下钻（view 定义）

`meta_show` 按 `(mechanism, view)` 分派。**能读运行时的读运行时，纯静态的返回常量**，逐项标注来源：

### 5.1 `search` —— 检索
- `candidates`：各层召回数量（observations 语义 ≤30 + 关键词 ≤10、KG 语义 ≤30、archive 关键词 ≤10）。
- `ranking`：排序规则 —— observations 关键词按字段权重 `name=10 / topic=5 / content=3` + 新近度；
  语义按 cosine；`search_memory` 里关键词命中记固定分 `KEYWORD_SCORE=0.30`。
- `dedup`：去重键 `(layer, id)`，重复保留最高分；snippet 截断 `SNIPPET_CHARS=240`。
- 真源：召回深度/固定分为 `memory_tools.rs` 常量（即调用点本身）；**字段权重为
  `observations.rs::FIELD_WEIGHT_{NAME,TOPIC,CONTENT}`**（与 `rank_by_relevance` 同源，非副本）。
- 备注（可选增强）：如需看「**最近一次真实检索**的候选→精排→去重 trace」，需在
  `SearchMemoryTool` 里加一个 `OnceLock`/环形缓冲保存最后一次调用的中间态 —— 见 §9 待拍点。

### 5.2 `embed` —— 嵌入
- `backend`：当前后端 —— 生产即 `FastEmbedder`（MiniLM-L6-v2）；**④真语义（2026-09-22）起 `fastembed`
  为无条件依赖、无词法回退**，加载失败返回 `Err`。`HashingEmbedder`（词法哈希）仅保留给确定性测试。
  真源：`lib.rs::default_embedder`。
- 向量空间迁移：某 zone 的既有向量若由**不同后端**写入，`open_default_zone` 触发一次性
  `Memory::rebuild_embeddings()`（重算 observations / KG / archive 向量并刷新记录）；该记录**首写者胜**。
- `dim`：`EMBEDDING_DIM = 384`（全库共享的向量空间）。
- `coverage`：各 store 里 `embedding IS NULL` 的行数（`missing_embeddings()`）与总行数。

### 5.3 `zones` —— 三区隔离
- `layout`：`chat=<root>/.memory/`、`main=.memory/main/`、`auditor=.memory/auditor/`。真源：`store.rs::Zone`。
- `counts`：三区 × 四库的行数矩阵（运行时扫描）。

### 5.4 `store` —— schema
- `schema`：四库**运行时**表结构（列名 + 类型 + not_null/pk + 索引），由 `PRAGMA table_info` +
  `sqlite_master` 从活连接实读（`Observations/KnowledgeGraph/Archive::schema()`）；
  business 为自由 schema，列其真实表名。真源 = 运行时 SQLite catalog，**不是**手抄的列名常量。

### 5.5 `pipeline` —— stage 图
- `graph`：`组织上下文 → 工作阶段 → 沉淀阶段`（2026-09-28 阶段改名；原版纯算法的 `I-知识图谱更新` 已并入 `沉淀阶段`，不再单列，故为三段）。
  真源：`engine.rs::run_turn`（I 无 `stages.json` 行，故 `graph` 列出而 `stages` 不含）。
- `stages`：每 stage 的 `timeout`（缺省回退 600s）。真源：`Config::stages()`。

### 5.6 `whitelist` —— 工具白名单
- `by_stage`：`Config::stages()` 里每个 stage 的 `tools` 列表**实况**（不是文档抄的），并与
  **静态已知工具名表**（各模块 `TOOL_NAMES` 常量并集，`MetaTools::known_tool_names()`）求差，
  标出「白名单列了但未注册」的悬空项（`mcp` 为动态组，不计入）。用静态名表而非 `ToolRegistry`
  句柄，是为避免「注册表含 meta 工具、meta 工具又持有注册表」的循环依赖。

### 5.7 `config` —— 配置解析链
- `chain`：`LINGMIAO_CONFIG_DIR` → 外部同名 JSON → 内嵌 `include_str!` 默认。真源：`config.rs::discover/read_json`。
- `source`：本次进程**实际生效**的来源（`override_dir()` 是否存在 + `has_pipeline()` 是否通过时的回退）。
- `values`：关键值摘要（help 主题数、stage 数、core_lock 上限、search_strategy 是否注入…）。
- （配套需求②的 `models.json`：`LINGMIAO_MODELS` → exe 同级 → cwd → 内嵌默认。）

### 5.8 `events` —— 事件枚举
- `variants`：11 个 `Event` 变体及其 `event_type` 线上名（`stage_started` … `mg_updated`）。真源：`events.rs`。

### 5.9 `tools` —— 工具注册表
- `groups`：分组 → 工具名（`file_tools` / `whiteboard` / `memory` / `business` / `help` / `meta` / `computer_use`[门控] / `mcp`）。
- 与 `whitelist` 视图呼应，回答「这个工具属于哪组、哪几个 stage 能用」。

---

## 6. 与现有入口 / 文档的关系

| 入口 | 定位 | 与平台的关系 |
|---|---|---|
| `help` 工具 | 用户/AI 的「怎么做」运行手册 | 保留；平台补「机制真值」这半边 |
| `memory_tools` | 记忆的**内容**读写 | 平台补「记忆**机制**」（排序/去重/嵌入/store） |
| `meta_*`（新增） | 机制本体 + 运行时状态 | 本文核心 |
| `docs/*.md` | 人类设计参考（为什么） | **降级**：不再承载「AI 手抄的真值」，改为解释设计动机 |
| 源码 | 终极真源 | 平台是它在运行时的可问答投影 |

一句话：**工具读真源、文档讲动机、源码是根**。

---

## 7. 与其它需求的关系

- **需求①（UX）**：无直接耦合；平台是给 AI 看的，不是 UI 面板。
- **需求②（API 配置）**：`config` 视图把「内嵌→外部覆盖→最终值」显式化，是需求②解析链的可下钻投影。
- **需求③（记忆目录）**：`zones` / `store` 视图把需求③的目录布局与三区隔离显式化。
- **需求④（文档机制）**：本文承接其「两入口一真源」，把范围从 `help` 一个入口扩到**全机制**；
  文档从真源降级为参考。
- **rules.md**：遵循「不幻构 / 工具返回才是真相」；平台正是把这条原则做成机制。

---

## 8. 落地计划（M6 · 元认知平台）

> **状态：M6.0–M6.4 已全部完成**（落地记录见 §10）。
> 竖切优先：先让 `meta_list` / `meta_state` 跑起来（能自报家底），再逐个补 `show` 视图。

1. **M6.0 骨架**：`meta_tools.rs` + catalog 常量 + `meta_list` / `meta_state`，注册进
   `full_registry`，`stages.json` 全 stage 白名单 + `prompts.json._base` 接线。
2. **M6.1 静态视图**：`meta_show` 的 `pipeline` / `whitelist` / `events` / `tools` / `config`（多数读常量与 `Config`）。
3. **M6.2 运行时视图**：`embed` / `zones` / `store` / `search`（需 `Memory` 句柄；`search` 的 trace 视待拍点）。
4. **M6.3 验证**：`cargo fmt` + `clippy -D warnings` + `cargo test`（含白名单绑定测试）全绿；
   用 **Xvfb/cu 实跑**，在对话里让 AI 调 `meta_list` / `meta_show` 观察返回（黑盒视觉验证，rules §6）。
5. **A 补档**：实现跑通后回填本文（接口以代码为准），确保文档不漂移。

**影响面**：`lingmiao-tools`（新增 `meta_tools.rs` + `lib.rs` 注册）、`lingmiao-core/assets/stages.json`（白名单）、
`assets/prompts.json`（`_base`）、`lingmiao-tools` 绑定测试、本文档。**不改**既有工具行为。

---

## 9. 决策（已定 · 离线拍板）

> 用户离线，按设计建议的默认拍板，均可逆、可随时改回。

| # | 点 | 决定 | 理由 |
|---|---|---|---|
| 1 | 工具组命名 | **`meta`** | 短、与「元认知」直接对应；`introspect`/`self` 更啰嗦 |
| 2 | 落点文件名 | **`docs/meta-platform.md`**（本文） | 已存在，沿用 |
| 3 | `search` 是否存检索 trace | **先不存**（只给规则常量与召回深度） | 平台工具保持**无状态、只读**；trace 属 M6.x 增强，需给 `SearchMemoryTool` 加缓冲 |
| 4 | `meta_state` 是否含跨区计数 | **含**（`zones`/`counts` 给 chat/main/auditor） | 审计要看三区；main/auditor 走**只读探测**（`read_zone_counts`，无副作用），未跑过则为 `null` |
| 5 | `docs` 是否标注「降级为参考」 | **是** | 见 §6；本文头部亦标注「接口以代码为准」 |

---

## 10. 落地记录（A 补档 · 接口以代码为准）

M6.0–M6.3 全部完成；`cargo fmt` + `clippy -D warnings` + `cargo test --workspace` 全绿，并用真实进程黑盒验证。

### 10.1 新增 / 改动的文件

| 文件 | 改动 |
|---|---|
| `crates/lingmiao-tools/src/meta_tools.rs` | **新增**：`Mechanism` catalog（9 条常量）+ `MetaTools`（三个工具的共享态）+ `meta_list` / `meta_show` / `meta_state` 三个 `Tool` + `register()` |
| `crates/lingmiao-tools/src/lib.rs` | 注册 `meta` 组进 `full_registry`；`full_registry` 增参 `cfg: &Config`（下钻视图要读**运行中的**配置，而非重新 discover 默认） |
| `crates/lingmiao-core/assets/stages.json` | 三个 `meta_*` 加入**每个** stage 白名单（同 `help`） |
| `crates/lingmiao-core/assets/prompts.json` | `_base` 追加一句：需要自身机制真值时调 `meta_*`，不要凭记忆猜 |
| `crates/lingmiao-memory/src/embed.rs` | `Embedder` trait 增 `backend()`（默认 `custom`）；`HashingEmbedder`→`hashing`，`FastEmbedder`→`fastembed/all-MiniLM-L6-v2` |
| `crates/lingmiao-memory/src/{observations,knowledge,archive}.rs` | 增只读 `missing_embeddings()`；observations/knowledge 增 `embedder_backend()`；三者增 `schema()`（运行时 schema，见 §10.4） |
| `crates/lingmiao-memory/src/observations.rs` | 新增 **`FIELD_WEIGHT_{NAME,TOPIC,CONTENT}`** 单一真源常量（`rank_by_relevance` 与 meta 同源消费），见 §10.4 |
| `crates/lingmiao-memory/src/store.rs` | 增 `ZoneCounts` + `read_zone_counts()`（**只读**探测，不建库不改 schema）；增 `TableSchema`/`ColumnInfo` + `read_table_schema()`（运行时 schema） |
| `crates/lingmiao-memory/src/lib.rs` | 导出 `ZoneCounts` / `read_zone_counts` / `TableSchema` / `ColumnInfo` / `read_table_schema` / `FIELD_WEIGHT_*`；`Memory::embedder_backend()` |
| `crates/lingmiao-core/src/events.rs` | 增 `EVENT_VARIANTS`（variant↔wire 名表）+ 一致性测试 |
| `crates/lingmiao-tools/src/memory_tools.rs` | 召回深度常量化并 `pub`（调用点即真源）；字段权重**不再在此留副本**（迁至 `observations.rs` 单一真源） |
| `crates/lingmiao-engine/src/engine.rs` | `full_registry` 调用点补 `cfg` 实参 |

### 10.2 接口（以代码为准，与 §4/§5 的差异）

- `meta_show` 分派键 `(mechanism, view)`，view 省略给该机制**默认视图**（catalog 首项）；未知机制给出候选 id，未知 view 报 `InvalidArgs`。
- **`whitelist`/`tools` 视图用静态分组名表**（各模块 `TOOL_NAMES` 常量并集）而非注册表句柄——避免「注册表含 meta 工具、meta 工具又持有注册表」的循环依赖；`mcp` 为动态组，不计入悬空判定。
- `zones`/`counts`：chat 走**实时** `Memory`；main/auditor 走 `read_zone_counts` 只读探测，目录不存在 → `null`。
- `store`/`schema`：由各 store 的 `schema()` **运行时**查询 `PRAGMA table_info` + `sqlite_master`
  得到（列名/类型/not_null/pk/索引），非手抄常量（见 §10.4）。

### 10.3 验证（M6.3 · 黑盒视觉）

- 单测：`meta_tools` 6 项（catalog 唯一性、目录枚举、运行时视图、未知机制/view、whitelist 悬空、全视图可渲染）+ 白名单绑定（含 meta 三工具每 stage 在场）。
- **真实进程**：`target/debug/lingmiao` 在 Xvfb(`:99`) 起 TUI，对话让模型调工具，视觉确认：
  - `meta_list` → 工具卡 `meta_list 0.3ms`，返回「元认知平台 · 机制目录（9）」全表；
  - `meta_show(embed/backend)` → 工具卡 `meta_show(embed) 0.1ms`，返回运行时真值（测试期后端为 `hashing`，生产为 `fastembed/all-MiniLM-L6-v2`）。

### 10.4 审计修正（M6.4 · 「真源不抄」收口）

首轮审计指出三处「文档/副本当真源」的隐患，本轮收口（改动小、可回滚）：

1. **字段权重单一真源**：`rank_by_relevance` 原硬编码 `10.0/5.0/3.0`，而 `memory_tools.rs` 另立
   一份 `FIELD_WEIGHT_*` 副本，meta 读的是**副本**。修正：常量落在
   `observations.rs::FIELD_WEIGHT_{NAME,TOPIC,CONTENT}`，`rank_by_relevance` 与 meta **同源消费**；
   `memory_tools.rs` 的副本删除。加锁步测试 `field_weights_are_the_documented_contract`（钉契约值）
   + `keyword_scores_follow_field_weights`（行为验证：单行时 score = 权重 + recency 1.0）。
2. **store schema 改运行时**：原 `store_schema()` 手抄列名列表（会与 DDL 漂移）。修正：新增
   `store.rs::TableSchema`/`ColumnInfo`/`read_table_schema()`，各 store 增 `schema()`，
   meta `store/schema` **运行时**读 `PRAGMA table_info` + `sqlite_master`。
   加测试 `read_table_schema_reads_the_live_catalog` + `schema_is_read_from_the_runtime_catalog`
   + `store_schema_is_read_at_runtime`。
3. **文档与代码对齐**：§5.6 原写「与 `ToolRegistry::contains` 求交」与实现（静态 `TOOL_NAMES` 名表）不符，
   §5.4 原写「可选运行时」亦不精确；本轮据实订正为「静态已知名表求差」与「运行时实读」。

验证：`cargo fmt` + `clippy --workspace --all-targets -D warnings` + `cargo test --workspace` 全绿
（含上述 6 项新/改测试）。

### 10.5 自省修正（M6.5 · pipeline graph 补齐 I 阶段）

`pipeline`/`graph` 原写死 `[B, C, 总结]` 三段，与真实管线（`engine.rs::run_turn` 跑
B → C → 总结 → **I-知识图谱更新**，`help.json` topic=pipeline 亦列四段）不符 —— 自省视图自身漂移。
本轮补齐：`graph` 改为四段（`STAGE_I_KG` 一并读 `config` 常量），`pipeline` 目录一句话与 §3/§5.5 同步；
新增锁步测试 `pipeline_graph_lists_every_run_turn_stage`（钉住四段，防再漂移）。I 阶段纯算法、无
`stages.json` 行，故 `graph` 列四段而 `stages` 视图仍按配置行给三段，二者并不矛盾（已在视图内注明）。

### 10.6 阶段名规范化（2026-09-28 · graph 收为三段）

阶段名从 Python 老术语（`字母-中文` 混写）改为**纯中文**：`组织上下文` / `工作阶段` / `沉淀阶段`，
且原版收尾的纯算法 **`I-知识图谱更新` 并入 `沉淀阶段`**（`STAGE_I_KG` 归并，`stage_i` 上报同一阶段名）。
故 `graph` 由四段收为**三段**，`pipeline_graph_lists_every_run_turn_stage` 锁步断言同步改为三段；
`config.json` 阶段路由旧键（`B-上下文选择`/`C-对话`/`总结流程`）经 `canonical_stage` 别名表仍可命中。

---

*本文档随讨论更新；接口以实现为准（A 补档），设计动机以本文为准。*
