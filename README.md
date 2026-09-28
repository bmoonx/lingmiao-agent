# 灵妙-Agent

**灵妙-Agent**（仓库名 / 命令名 **lingmiao**）是一个开源 AI 编程助手引擎 —— 用纯 Rust 实现的 Claude Code 风格终端 TUI。
它在终端里跑一个「对话 → 检索记忆 → 作答 → 沉淀知识」的本地智能体循环，模型可自由配置、随时热切换。

> 标题为 **灵妙-Agent**，标识与命令名统一为 `lingmiao`（crate 包名 / 二进制 / 目录 / 环境变量前缀）。
> 代码层改名已随实现落地 —— 见 [`docs/docs-mechanism.md`](docs/docs-mechanism.md)。
> 早先的 Python 版实现仅保留作**行为参照基线**，不进入交付物。

## 特性

- **纯 Rust 单栈**：6 库 + 1 二进制 —— `lingmiao-core`（基础设施）/ `lingmiao-llm`（模型网关）/
  `lingmiao-memory`（记忆）/ `lingmiao-tools`（工具）/ `lingmiao-engine`（对话管线）/ `lingmiao-tui`（界面）。
- **多组模型可编排**：`models.json` 外置配置，方言（OpenAI / Anthropic）× 模型组 ——
  加一个来源零改代码，运行时热切换。
- **三层记忆**：观测日志 / 知识图谱 / 归档，每项目一份、存于 `.memory/`。
- **CC 风格 TUI**：单 tokio 事件循环，纯 CC 单栏（header + 对话流 + input + footer）+ 应用内翻页
  （PgUp/PgDn/Home/End，全屏 alternate buffer）；footer 常驻状态·会话 token / 上下文组成 / 小白板摘要。
- **内置帮助**：`/help` 与 AI 调用的 `help` 工具读**同一份** `help.json`，用户与 AI 都能查。

## 状态

里程碑 **M0–M5 全部落地**（当前 `git tag` 到 `v0.12.26`）：

| 里程碑 | 内容 |
|---|---|
| M0 | workspace 骨架 + 基础设施层（config / paths / errors / events / envkeys / logging） |
| M1 | 最小 LLM 循环 |
| M2 | 记忆层（SQLite store + 三区隔离 + 384d 嵌入） |
| M3 | 工具层（Tool trait + schemars 注册表 + 内置工具四组 + rmcp MCP 客户端） |
| M4 | 对话管线 **组织上下文 → 工作阶段 → 沉淀阶段** |
| M5 | TUI 完整化（对话区 + 应用内翻页 + 全管线驱动） |

> 设计文档见 [`docs/`](docs/)。四项改进设计（①UI 交互 ②模型配置 ③记忆目录 ④命名与文档）
> 已全部落地（`v0.7.0`），并已通过 computeruse 端到端确认；随后的 TUI 打磨（`v0.8.0`：
> ∴ 思考流 / 活动行 / 富文本渲染 / 6 段上下文条 / 过滤内部工具卡）亦已 computeruse 实拍确认。
>
> **布局演进**：cli 于 2026-09-20 拍板**撤侧栏 → 纯 CC 单栏**（[`docs/ux-design.md §12`](docs/ux-design.md)），
> 布局与导航已按 §12 落地：header（品牌·版本·模型·cwd）/ footer 三段（快捷键+状态·token / 上下文组成，无 % / 小白板摘要）
> / 导航改斜杠命令 / ↑↓ 翻历史。随后的 **CC 视觉对标（`v0.10.0` §13：品牌 mark + hairline 分隔 + 反色提问条 + 圆角输入框）** 亦已 :99 实拍确认。

## 快速开始

需要 Rust 工具链（见 `rust-toolchain.toml`）。

```sh
./run.sh              # debug 构建并启动 TUI（二进制缺失时自动 build）
./run.sh --release    # 用 release 构建（更快、更小）
./run.sh --check      # 只做环境自检（key / 配置 / 二进制），不启动
```

或直接：

```sh
cargo run
```

首次运行若未配置模型会回落内置默认；缺 API key 时按界面提示设置环境变量或 `models.json` 即可。

## 配置模型

模型配置写在**与程序同级**的 `models.json`（也可用 `LINGMIAO_MODELS` 指定），
支持多组 provider / 多模型，改完重启生效、界面里也能随时切换。查完整说明：

- 交互内输入 `/help models`；
- 或让 AI 调 `help` 工具（读同一份内置 `help.json`）；
- 设计细节见 [`docs/api-config.md`](docs/api-config.md)。

## 文档索引（[`docs/`](docs/)）

| 文件 | 内容 |
|---|---|
| [`architecture.md`](docs/architecture.md) | 系统架构（Q1–Q9 决策 / crate 分层 / 数据流 / 里程碑） |
| [`ux-design.md`](docs/ux-design.md) | UX 设计（布局 / 面板 / 交互；§12 纯 CC 单栏 · §13 CC 视觉对标） |
| [`api-config.md`](docs/api-config.md) | 需求② 模型配置机制 |
| [`memory-dir.md`](docs/memory-dir.md) | 需求③ 记忆目录（`.memory/`） |
| [`docs-mechanism.md`](docs/docs-mechanism.md) | 需求④ 命名与文档机制 |
| [`rules.md`](rules.md) | 工程规范（优先级最高） |

## 开发

```sh
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt
```

## 许可

双许可，任选其一：[MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE)。
