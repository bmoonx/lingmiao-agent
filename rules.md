# rules.md — lingmiao 项目规范

> 本文件是项目的制度性约束，优先级高于任何默认行为。
> 每次开工前先读本文件；决策前先查 `task`（任务状态）、`correction`（历史教训）、`decision`（过往决策）。

## 1. 项目定位

- **lingmiao** = **灵妙（英文 lingmiao）**，是早先 Python 版实现（基线 `v0.2.289`）的 **Rust 完全替代重构**（决策 Q1）。产品定名与文档机制见 `docs/docs-mechanism.md`。
- 交付物为**纯 Rust 单栈**：Python 版（含 UI）全部弃用，UI 用 Rust 重做。
- 原 Python 版**仅作本地行为参照基线**（运行它、对照行为），**不进入交付物、不提交**。
- 目的：消除原版 bug 风险 + 开源。

## 2. 横向框架决策（Q1~Q9，全部定案）

| 问 | 决策 |
|----|------|
| Q1 | 完全替代：纯 Rust 单栈，UI 亦重做（ratatui） |
| Q2 | 砍掉模式层（无 autonomous/design/council…）；UI 只做 TUI，对标 Claude Code 效果——UI 与 QL 同一事件循环，随时向上翻页不卡顿 |
| Q3 | crate 分层 **6+1**；异步走 CC 式单循环事件流（tokio 承载）；**单一二进制、无子命令**，一句 `lingmiao` 直接运行 TUI，其余操作走 TUI 内快捷键/斜杠命令 |
| Q4 | 记忆层 = **多 SQLite**（`rusqlite` bundled，按 store 一库一文件）+ **`fastembed-rs`** 384 维 MiniLM 嵌入 |
| Q5 | LLM 层 = `reqwest` + 手写 SSE；流式经 **`mpsc` channel 产出 `StreamDelta` 枚举**；内部消息格式沿用 **OpenAI canonical**，Anthropic 做翻译层 |
| Q6 | 工具层 = **`Tool` trait + `schemars` 派生 JSON Schema**；MCP 用官方 **`rmcp` SDK**；砍掉 `parliament` 工具，保留 file_tools / file_guard / whiteboard / computer_use 四组 |
| Q7 | 编排 = 每 stage 一个 **`async fn` + 强类型 `TurnContext`**；consolidation（**沉淀阶段**）用 **`tokio::join!` + `Result` 错误边界**；664 行 `StageAgent` 保留为独立 `stage_agent.rs` 复用 |
| Q8 | 配置 = **`include_str!` 内嵌四 JSON + serde 强类型 struct + 外部覆盖**；错误 = **`thiserror` 分层枚举，保留 `recoverable` 语义**；事件 = **单 `enum Event` + serde + tokio broadcast**（原 13 变体一比一，Q9 合并结果事件后为 **10 变体**） |
| Q9 | （2026-09-16 设计变更 · 重新设计）**`C_obs`/`D`(任务追踪)/`F`/`G`/`H` 五阶段合并为单一「沉淀阶段」**：一次 LLM 调用完成 对话观测 + 任务追踪 + 要点记录 + 上下文审计 + 内容质检，输出一个综合 JSON；管线收敛为 **组织上下文 → 工作阶段 → 沉淀阶段**（2026-09-28 阶段名规范化；原版纯算法的 `I-知识图谱更新` 已并入沉淀阶段）；四个对应的 result 事件（`task_tracked`/`observations_extracted`/`context_audited`/`turn_assessed`）合并为单一 **`summary_reported`** |

### 收尾决策

- **记忆数据兼容性**：全新开始（schema 重设计，不兼容旧状态目录库；将来可写独立 `lingmiao migrate`）。
- **许可证**：MIT OR Apache-2.0 双许可。
- **仓库形态**：`git init`，仓库名 **lingmiao**；产品**标题**为 **灵妙-Agent**（标识 / 命令名 / 目录仍为 `lingmiao`，cli 2026-09-29 拍板）。
- **版本号规范**：沿用原版「大.中.小」三段式（semver 风格）。

## 3. 目录结构（6+1 分层）

```
crates/
  lingmiao/        单 bin（启动序列 → 最终运行 TUI）
  lingmiao-core/   基础设施层：config / paths / errors / events / envkeys / logging（Q8）
  lingmiao-memory/ 记忆层：SQLite stores + 嵌入索引（Q4）
  lingmiao-llm/    LLM Provider 层 + 流式协议（Q5）
  lingmiao-tools/  工具注册表 + 内置工具 + MCP client（Q6）
  lingmiao-engine/ QL 编排器 + 管线 stages（Q7）
  lingmiao-tui/    ratatui TUI，CC 式单栏（header + 对话流 + input + footer）（Q2）
```

`.memory/` 是**记忆目录**（持久资产：7 个白名单 DB + `constraints/`/`trajectories/`/`embeddings/`/`whiteboard/` 子目录），**不提交**（见 `.gitignore`）；`.cache/lingmiao/` 是**易失缓存目录**（logs/tmp/loop），随时可删、不进交付物。
`.memory/` 顶层只允许 7 个白名单 DB 及其 journal，启动时清理其它散落文件；子目录不递归清理。

**编译产物只保留最新**：`target/` 不提交（见 `.gitignore`）。清理时用 `cargo clean` 清空**全部**历史产物、再 `cargo build` 重建当前所需，使 `target/` 只含最新一次构建；**禁止**让 `incremental/` 与旧 `deps/` 的重复哈希产物长期累积（曾累积至 52G）。

## 4. 开发顺序（竖切优先）

先打通端到端细线，再逐层填肉：

- **M0** workspace 骨架 + `lingmiao-core`（config/paths/errors/events/envkeys/logging）
- **M1** 最小可用循环：llm 流式 + 单 stage + 事件推 UI（跑通「输入一句话、逐 token 输出」）
- **M2** 记忆层（Q4）
- **M3** 工具层（Q6）
- **M4** 管线 stages 全量（Q7）
- **M5** TUI 完整化（Q2）

## 5. 代码规范

- **格式化 / lint**：`cargo fmt` + `cargo clippy -- -D warnings`，提交前必须全绿（对应原版 black/ruff）。
- **不设「改动尽量小」约束**（2026-09-16 取消）；允许为正确性做必要的大范围重写，只需保持匹配周围命名、缩进、风格。
- 不幻构：工具返回的才是真相；没查就答 = 猜。
- 不主动创建 `*.md` / README，除非明确要求。

## 6. 测试与验收

- **真实运行验证**：优先在真实进程里运行程序、观察行为并核对界面与输出；log 作辅助。
- **不限定唯一工具**：`computer_use` / `textual-mcp-server` / `tui-mcp` 等任一可用工具均可用于验证；**禁止**写 pytest/bash 脚本替代真实运行观察，**禁止**虚构验证结果。
- **回归基线**：跑原版 Python 记录完整事件流 JSONL 作 golden file，Rust 版每里程碑 diff 事件序列。
- 优先真实运行验证，不强制覆盖率。

## 7. 版本与发布

- **版本号唯一信源 = Git 标签**（`vMAJOR.MINOR.PATCH`）；禁止创建 `VERSION` 文件。
- **中版本号（MINOR）要谨慎**：**仅在重大变更时递增**（里程碑落地、架构级重构、破坏性接口/行为变更）。日常迭代**不得**随手升 MINOR——中版本号换太频繁会稀释「重大节点」的信号价值。
- **小版本号（PATCH）承载每一个 commit**：每个落地提交（含 UI 更换/调整、bug 修复、文档/配置改动）都递增 PATCH，一个 commit 对应一个 PATCH 标签（如一轮 UI 更换即打 `vX.Y.(Z+1)`）。
- **大版本号（MAJOR）**：仅在破坏性、不兼容的重大变更时递增。
- commit message 用 `type: description`（如 `feat: 搭建 M0 workspace 骨架`）。
- **每次打 tag 后必须构建**：打完 tag 立即 `cargo build`，确认 tag 对应代码可编译、`target/debug/lingmiao` 产物落位；未构建或构建失败不得视为「已发布」。
- **debug 下 `lingmiao` 命令软链接到 debug 二进制**：`ln -sf "$PWD/target/debug/lingmiao" /usr/local/bin/lingmiao`，保证一句 `lingmiao` 启动最新 debug 产物；debug 阶段只用 debug 二进制，不软链接 release。
- **UI 内版本号与 tag 对齐**：UI 版本号来自 `env!("CARGO_PKG_VERSION")`（`lingmiao-core`），即 workspace `Cargo.toml` 的 `version`。打 tag 时**同步把 `version` 更新为与 tag 相同的三段式**（tag `v0.12.5` → `version = "0.12.5"`），使 UI 显示版本 == 最新 Git tag；禁止两者漂移。
- **release 另外验证**：release 走 `cargo build --release`（产物 `target/release/lingmiao`），并**单独**走 computeruse 视觉验证，不与 debug 的构建/验证混用。

## 8. 安全红线

- 禁止 `git push`、`git reset --hard`、删除重要目录等不可逆操作，除非明确授权。
- 不点不明链接、不执行交易/转账/下单。
- 密钥只走 `.env`（不入库）；`.env` 已在 `.gitignore`。
