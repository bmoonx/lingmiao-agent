# 架构设计 — 灵妙（lingmiao）

> 本文档记录 **lingmiao（灵妙）** 的系统架构与设计决策，随开发演进持续更新。
> - **制度性约束** → 见 [`rules.md`](../rules.md)（优先级最高；本文档与其冲突时以 rules.md 为准）。
> - **UI / 交互设计** → 见 [`ux-design.md`](./ux-design.md)（活文档，UI 细节可调整）。
> - **版本号唯一信源 = Git 标签**，本文档不复制版本号，只描述架构。

---

## 1. 项目定位

- **灵妙（lingmiao）** = 早先 Python 版实现（基线 `v0.2.289`）的 **Rust 完全替代重构**。
- 交付物为**纯 Rust 单栈**：早先 Python 版（含 UI）全部弃用，UI 用 Rust 重做。
- 早先 Python 版**仅作本地行为参照基线**（运行它、对照行为），**不进入交付物、不提交**。
- **目标**：消除原版 bug 风险 + 开源（MIT OR Apache-2.0 双许可）。
- **定位**：一个 AI harness —— 上下文管理自动化 + 领域精确化（原实现管线 + 四层记忆的 Rust 化）。

---

## 2. 横向架构决策（Q1–Q9，全部定案）

| 问 | 决策 |
|----|------|
| **Q1** | 完全替代：纯 Rust 单栈，UI 亦重做（ratatui） |
| **Q2** | 砍掉模式层（无 autonomous / design / council…）；UI 只做 TUI，对标 Claude Code —— **UI 与 QL 同一事件循环**，随时向上翻页不卡顿 |
| **Q3** | crate 分层 **6+1**；异步走 **CC 式单循环事件流**（tokio 承载）；**单一二进制、无子命令**，一句 `lingmiao` 直接运行 TUI，其余操作走 TUI 内快捷键 / 斜杠命令 |
| **Q4** | 记忆层 = **多 SQLite**（`rusqlite` bundled，按 store 一库一文件）+ **`fastembed-rs`** 384 维 MiniLM 嵌入 |
| **Q5** | LLM 层 = `reqwest` + 手写 SSE；流式经 **`mpsc` channel 产出 `StreamDelta` 枚举**；内部消息格式沿用 **OpenAI canonical**，Anthropic 做翻译层 |
| **Q6** | 工具层 = **`Tool` trait + `schemars` 派生 JSON Schema**；MCP 用官方 **`rmcp` SDK**；砍掉 `parliament` 工具，保留 file_tools / file_guard / whiteboard / computer_use 四组 |
| **Q7** | 编排 = 每 stage 一个 **`async fn` + 强类型 `TurnContext`**；consolidation（**沉淀阶段**）用 **`tokio::join!` + `Result` 错误边界**；664 行 `StageAgent` 保留为独立 `stage_agent.rs` 复用 |
| **Q8** | 配置 = **`include_str!` 内嵌四 JSON + serde 强类型 struct + 外部覆盖**；错误 = **`thiserror` 分层枚举，保留 `recoverable` 语义**；事件 = **单 `enum Event` + serde + tokio broadcast**（Q9 合并结果事件后为 **10 变体**，加需求① 的 `context_usage`、A→B 交接的 `context_handoff` 后为 **12 变体**） |
| **Q9** | **`C_obs` / `D`(任务追踪) / `F` / `G` / `H` 五阶段合并为单一「沉淀阶段」**：一次 LLM 调用完成 对话观测 + 任务追踪 + 要点记录 + 上下文审计 + 内容质检，输出一个综合 JSON；管线收敛为 **组织上下文 → 工作阶段 → 沉淀阶段**；四个对应 result 事件合并为单一 **`summary_reported`** |

### 收尾决策

- **记忆数据兼容性**：全新开始（schema 重设计，不兼容旧状态目录库；将来可写独立 `lingmiao migrate`）。
- **许可证**：MIT OR Apache-2.0 双许可。
- **仓库形态**：`git init`，仓库名 **lingmiao**（产品标题 **灵妙-Agent**）。
- **产品名**：标题 **灵妙-Agent** / 标识 `lingmiao`（经 `lingmiao-core::brand` 单一信源一处替换；cli 2026-09-29 拍板「只标题叫这个名字，其他还是叫 lingmiao」）。
- **测试唯一信源**：基于 computeruse 运行程序、**视觉模仿用户操作**验证结果；log 作辅助。

---

## 3. crate 分层（6+1）

```
crates/
  lingmiao/        单 bin（启动序列 → 最终运行 TUI）
  lingmiao-core/   基础设施层：config / paths / errors / events / envkeys / logging（Q8）
  lingmiao-memory/ 记忆层：SQLite stores + 嵌入索引（Q4）
  lingmiao-llm/    LLM Provider 层 + 流式协议（Q5）
  lingmiao-tools/  工具注册表 + 内置工具 + MCP client（Q6）
  lingmiao-engine/ QL 编排器 + 组织上下文→工作阶段→沉淀阶段 管线（Q7）
  lingmiao-tui/    ratatui TUI，CC 式单栏（header + 对话流 + input + footer）+ 应用内翻页（Q2）
```

**关键设计点**

- **字面量抽象**：产品名 / 状态目录 / env 前缀 / 日志名 / 事件 target / 启动横幅 / TUI 标题，全部经 `lingmiao-core::brand` 单一信源派生（定名后一处替换）；`prompts.json` 用 `{brand}/{state_dir}/{env_prefix}` 占位符由 engine 渲染。
- **运行时状态目录**（需求③）：记忆落 `<root>/.memory/`（observations / knowledge / context_record / business），运行态落 `<root>/.cache/lingmiao/`（tmp / logs / loop）；二者**均不提交**。`.memory/` 顶层只允许白名单 DB 及其 journal，启动时清理其它散文件（子目录 `constraints/trajectories/embeddings/whiteboard` 保留）。

---

## 4. 数据流与事件

- **管线**：`组织上下文` → `工作阶段` → `沉淀阶段`（Q9 收敛后 3 个 LLM 阶段；`沉淀阶段` 收尾的**知识图谱更新**为纯算法、无 LLM —— 读 #2 中 fact/preference/decision/constraint 的近期观测，对知识图谱做 UPDATE/ADD，原版 MG evolution，2026-09-28 起**并入沉淀阶段**不再单列）。
- **事件总线**：单 `enum Event`（12 变体）+ `EventBus`：写日志（`.jsonl` 机器读 + `.log` 人读）+ fan-out + `tokio::broadcast`。
- **UI 与 QL 同循环**（Q2/Q3）：单一 tokio runtime，UI 与 QL 在同一事件循环内，事件流实时推 UI，向上翻页不卡顿（对标 CC）。
- **回归基线**：跑原版 Python 记录完整事件流 JSONL 作 golden file，Rust 版每里程碑 diff 事件序列。

---

## 5. 里程碑与现状

竖切优先：先打通端到端细线，再逐层填肉。

| 里程碑 | 内容 | 状态 |
|--------|------|------|
| **M0** | workspace 骨架 + `lingmiao-core`（config/paths/errors/events/envkeys/logging） | ✅ |
| **M1** | 最小可用循环：LLM 流式 + 单 stage + 事件推 UI | ✅ |
| **M2** | 记忆层（Q4）：四 SQLite store + 三区隔离 + 384d 嵌入 | ✅ |
| **M3** | 工具层（Q6）：Tool trait/注册表 + 四组内置工具 + rmcp MCP | ✅ |
| **M4** | 组织上下文→工作阶段→沉淀阶段 管线（Q7） | ✅ |
| **M5** | TUI 完整化（Q2）：对话区 + 应用内翻页 + 全 M4 管线驱动 | ✅ |

> 当前最新 tag = **v0.12.0**。具体版本以 `git tag` 为准。
> **四项改进设计已落地**（①UI 交互 ②模型配置 ③记忆目录 ④命名与文档），并经 computeruse 端到端确认：
> - ① TUI：CC 式对话流（`❯` 提问行 / `∴` 思考 / 工具卡 / token 尾行 / markdown 富文本）+ 应用内翻页（PgUp/PgDn/Home/End）。
>   **布局演进（2026-09-20）**：cli 拍板**撤侧栏 → 纯 CC 单栏**（`ux-design.md §12`）—— header（品牌·版本·模型·cwd）+ 对话流 + input + footer 三段
>   （快捷键+状态·会话 token / 上下文组成，无 % / 小白板摘要）；导航改**斜杠命令**（`/help /board /memory /session /tools /model /clear /quit`；`/context` 后续移除），↑↓ 翻输入历史。
>   **CC 视觉对标（2026-09-20，`ux-design.md §13`）**：header 加**品牌 mark + 两行身份块**、hairline 分隔线、用户回合画成**反色提问条**、输入框改**圆角框**（footer 三段不变）。
>   **引擎/工具/TUI 三处对齐（2026-09-21，`ux-design.md §15`）**：footer L2 改 **系统/lock/总/其余** 四数、去 `/context`；引擎按 原版 机制**注入 core lock**（首轮 + 每次工具结果后）；新增 **`search_memory` 跨层聚合工具**（#1/#2/#3 一次召回）。
>   **跨项目检索（2026-09-21）**：新增 **`search_external_memory`** —— **只读**检索另一个项目 `.memory/` 的 chat/main/auditor 三区（observations / KG / archive 混合召回，复用进程内 embedder 保证同一向量空间），补齐相对原版的跨项目记忆复用。目标项目记忆**不写不迁移**（`open_db_readonly`）；`memory_tools` 10→11 工具、三阶段白名单同步。
>   **跨项目嵌入空间守卫（2026-09-21）**：每个 embedding store 在 `store_meta` 表记录写入它的 embedder 后端（`hashing` / `fastembed/all-MiniLM-L6-v2`）；`search_external_memory` **运行时读**目标 store 记录的后端，与本进程后端比对——**不一致即跳过语义/KG 召回**（跨空间 cosine 会失真），退化为纯关键词召回并在结果里给出 `space_warning`，绝不把噪声排进 top-N。该记录**首写者胜**（`INSERT OR IGNORE`），代表「创建向量者」而非「最后打开者」。
>   **真语义嵌入（2026-09-22，④真语义落地）**：删除 `fastembed` 编译开关，`fastembed` 成为**无条件依赖**，生产 embedder 即 `FastEmbedder`（MiniLM-L6-v2）；`default_embedder()` 改为返回 `Result`，**无词法回退**（加载失败即 `Err`，引擎降级为「无记忆运行」而非静默用非语义向量）。ONNX Runtime 经 `ort-download-binaries-rustls-tls` 在**构建期**取入并链接，二进制自包含（无需运行期 `ORT_DYLIB_PATH`）。当某 zone 的既有向量由**不同后端**写入（如旧的词法向量）时，`open_default_zone` 触发一次性 **`Memory::rebuild_embeddings()`**（重算 observations / KG nodes / archive turns 的向量并刷新记录），迁移仅在首开时发生。`HashingEmbedder` 仅保留给确定性测试。
> - ② `models.json` 方言（OpenAI/Anthropic）× 模型组 + `ModelRegistry`；`help.json`（19 主题）内嵌，用户 `/help` 与 AI `help` 工具读同一份。
> - 见 `ux-design.md` / `api-config.md` / `memory-dir.md` / `docs-mechanism.md`。

---

## 6. 测试与验收

- **验收标准 = computeruse 视觉模仿用户操作验证**：真实运行程序 → 截屏/录屏（模仿用户操作：按键、输入、翻页）→ 视觉核对界面与行为；log 作辅助。
- **不用** `textual-mcp-server` / `tui-mcp` 等 MCP 黑盒脚本作为验收手段；**禁止**写 pytest/bash 脚本替代真实验证。
- **回归基线**：原版 Python 事件流 JSONL golden file 对比。
- **静态门禁**：`cargo fmt` + `cargo clippy -- -D warnings` 提交前必须全绿。

---

## 7. 工程约定

- commit message 用 `type: description`（如 `feat: 搭建 M0 workspace 骨架`）。
- 版本号唯一信源 = Git 标签，禁止创建 `VERSION` 文件。
- 不设「改动尽量小」约束（2026-09-16 取消）；允许为正确性做必要的大范围重写，但保持匹配周围命名、缩进、风格。
- 安全红线：禁止 `git push` / `git reset --hard` / 删除重要目录等不可逆操作；密钥只走 `.env`（不入库）。

---

*本文档记录架构性决策，UI 细节见 `ux-design.md`。*
