# 灵妙配套文档与体验机制（需求④）

> 状态：**已定稿**（2026-09-16 · cli 已拍 4 待拍点，见 §5）
> 所属：需求④ —— AI harness 定名「灵妙 lingmiao」+ 配套的用户体验与文档机制
> 约束：`rules.md` 优先级最高；版本号唯一信源 = Git tag（本文不写死版本）

---

## 0. 需求原文

> 「AI harness 定名『灵妙』（英文 lingmiao），并准备配套的用户体验与文档机制。」

**命名已定**（`decision/灵妙 lingmiao`）。本文只解决后半句：**配套什么、放在哪、给谁看**。

- 「**用户体验机制**」= 首次接触的引导 + 品牌呈现 + 帮助的可达性（需求①已覆盖「界面内可发现性」，本文补齐「第一次打开时」和「开箱品牌」）。
- 「**文档机制**」= 一套**载体分层 + 受众分工**的约定，让文档既有「为什么」（设计）也有「怎么做」（使用），且**不随代码漂移**。

---

## 1. 命名落地（品牌）

### 1.1 改名清单 —— 唯一信源已抽象好，改动受控

`brand.rs` 早已把产品名抽成唯一信源（`da58a38`），定名后**只改一处 + 几处散文**。
下表记录**改名完成后的现状**（全部已落定为 `lingmiao` / `灵妙`）：

| 位置 | 当前值 | 影响 |
|---|---|---|
| `brand.rs` `NAME` | `灵妙-Agent` | 启动横幅 / TUI 标题 / prompts 渲染 |
| `brand.rs` `SLUG` | `lingmiao` | env 前缀 / 日志文件名 / tracing target |
| `brand.rs` `BIN` | `lingmiao` | 命令名 / `msg:` 前缀 / Cargo bin 名 / `run.sh` |
| `brand.rs` `ENV_PREFIX` | `LINGMIAO` | `brand::env()` 派生（自动跟随） |
| `brand.rs` `EVENTS_TARGET` | `lingmiao.events` | 跟随 |
| `7× Cargo.toml` 包名 | `lingmiao-core` … `lingmiao-tui` | crate 名 / `use` 路径 |
| 根 `Cargo.toml` `description`/`repository` | `灵妙 …` / 新仓库名 | 散文 |
| `README.md` 散文 | `灵妙-Agent`（标题）/ `lingmiao` | 散文 |
| `.gitignore` | `/.memory/` + `/.cache/` | 需求③ |
| `assets/*.json` `_note` | `灵妙` | 内嵌提示 |

> **状态目录不再受品牌影响**：需求③ 已把 `.memory/` 拆成 `.memory`（固定字面量）+
> `.cache/`，与品牌解耦。定名只影响上面这些「品牌呈现」处。

### 1.2 命令名（`BIN`）—— 已定案：`lingmiao`

`BIN` = 用户在终端敲的命令，也是 `msg:` 前缀、`run.sh`、Cargo bin 名。

**cli 拍板（2026-09-16）：命令名改 `lingmiao`** —— 品牌名与命令名统一，最直白、最少歧义。

连带影响：`crates/lingmiao/Cargo.toml` 的 `[[bin]] name`、TUI 提示里的 `msg:`/示例命令行、
`run.sh` 注释与示例、README 快速开始，统一改用 `lingmiao`。
（备选 `lm`（更短）未采纳。）

### 1.3 与需求③ / 需求② 的措辞协调 —— 已订正

需求③ `memory-dir.md`、需求② `api-config.md` 原写的 env 前缀是隐式/硬写值
（改名前值）。定名 `SLUG=lingmiao` 后，env 前缀 = **`LINGMIAO_*`**
（`LINGMIAO_MODELS` / `LINGMIAO_MEMORY` / `LINGMIAO_CACHE_DIR` …），
缓存目录 = **`<项目>/.cache/lingmiao/`**（`brand::BIN` 派生）。

**已同步订正** `memory-dir.md`（§3.1 / §3.2 / §3.5 / §5 / §6）与
`api-config.md`（env 名 → `LINGMIAO_MODELS`）。两文档一律以「brand 派生」为准，不写死字面量。

---

## 2. 文档机制：三类载体 × 三种受众

**核心原则 = 「为什么」与「怎么做」分离，各自单一受众**（避免一份文档同时背两种职责而失焦）。

| 载体 | 受众 | 回答 | 形态 | 位置 | 生命周期 |
|---|---|---|---|---|---|
| `README.md` | 开源访客 / 新用户 | **是什么 / 怎么装 / 怎么跑** | 短、入口 | 仓库根 | 稳定 |
| `docs/*.md` | **开发者** | **为什么这么设计** | 设计文档 | 仓库 `docs/` | 随设计演进（活文档） |
| `assets/help.json` | **用户 + AI** | **怎么用**（运行时） | 主题表（内嵌） | `lingmiao-core/assets/` | 随功能演进，零代码 |
| `assets/prompts.json` | **AI** | 运行时行为准则 | 系统提示 | `lingmiao-core/assets/` | 随管线演进 |

### 2.1 两个「运行时」载体（需求②已落地设计）

- `help.json`（19 主题全集）+ `help` 工具 + `prompts.json._base` 指引 —— 见
  [`api-config.md §6`](./api-config.md)。**用户**可读 `/help`，**AI** 可调 `help` 工具。
- 这是需求②「配置用 JSON + 良好配套文档」组合逻辑的落点：JSON 无注释 → 可读性转移到
  **内置帮助 + AI 提示词**。

### 2.2 `docs/*.md` 现状与约定

| 文件 | 定位 |
|---|---|
| `architecture.md` | 稳定系统架构（Q1–Q9 / 6+1 crate / 数据流 / 里程碑） |
| `ux-design.md` | UX 活文档（布局 / 侧栏 / 面板 / 交互，UI 细节可调） |
| `api-config.md` | 需求② 已定稿 |
| `memory-dir.md` | 需求③ 已定稿 |
| `docs-mechanism.md` | 本文 —— 需求④ |
| `rules.md`（仓库根） | 制度约束，优先级最高 |

**活文档约定**：`docs/*.md` 每份头部声明「状态 + 受众 + 与 rules.md 的关系」；设计讨论随谈随记
（cli 19:21 指令），**引用版本号一律不写死**（信源 = Git tag）。

### 2.3 `README.md` 重写 —— ✅ 本轮已完成

原 README 停留在「**M0 骨架就绪**、后续里程碑 M1–M5 待做」——与事实（tag 到 v0.6.0、M0–M5 全落地）
**严重不符**，且缺「怎么用 / 怎么配模型」。**本轮已按以下要点重写**（见仓库根 `README.md`）：

1. 现状校正：M0–M5 全落地、纯 Rust 单栈、单二进制；
2. 快速开始：`./run.sh` 或 `cargo run`；
3. 配置模型：指向 `models.json` + `/help`（一句话 + 链接）；
4. 文档索引：Table of Contents 指向 `docs/`；
5. 品牌落名：**灵妙 lingmiao**（命令名改名随实现落地，见 §5-3）。

---

## 3. 用户体验配套机制

需求①已把「**界面内的可发现性**」做实（侧栏功能导航 / 工具卡说人话 / 底部快捷键行 / 斜杠菜单中文说明，
见 `ux-design.md`）。需求④补齐界面之外的三块：

### 3.1 首次运行引导（first-run）

- **无配置也能跑**：`models.json` 缺失 → 回落内置默认（deepseek/kimi/claude 三组，key 走 env），
  保证「装完就能启动」。
- **缺 key 时给可行动提示**（非报错堆栈）：「未检测到 API key → 设 `LINGMIAO_MODELS` 指向你的
  `models.json`，或设置 `DEEPSEEK_API_KEY` 环境变量。输入 `/help models` 查看配置方法。」
- **首次启动打印一行引导**：`输入 /help 查看用法；输入 /model 切换模型。`（学 CC 的 banner 提示行）。

### 3.2 帮助的可达性：`/help` ↔ `help` 工具呼应

- **用户侧**：斜杠命令 `/help [主题]` —— 与 AI 调用的 `help` 工具**读同一份 `help.json`**，
  两个入口、一份真源（不重复维护）。
- **AI 侧**：`help` 工具注册进所有 stage 白名单，模型每轮可见其 `description`（何时该查帮助）。

### 3.3 品牌呈现（三处亮相）

| 处 | 内容 | 现状 |
|---|---|---|
| 启动横幅 | `灵妙 (Rust) vX.Y.Z` | 现为 `灵妙 (Rust) v…` |
| TUI 标题 | `灵妙`（侧栏顶部状态区） | 现为 `灵妙` |
| 帮助/提示语言 | 一律用「灵妙」自称 | 待改 |

> 品牌呈现靠 `brand::NAME`，改 `brand.rs` 一处即全亮。

---

## 4. 与其它需求 / 文档的关系

- **需求①（UX）**：需求④承接其「可发现性」，补「首次引导 + 品牌」；两者共同指向非技术用户。
- **需求②（API 配置）**：`help.json` + `help` 工具 + `prompts` 提示 = 需求④文档机制的**第一块落地**（已完成设计）。
- **需求③（记忆目录）**：品牌定名影响其 env 前缀字样（§1.3），已一并订正，逻辑不变。
- **`rules.md`**：文档机制的「活文档 / 版本不写死 / rules 优先」三条约定与 rules.md 一致。

---

## 5. 待拍点 → 已拍板（2026-09-16 cli）

| # | 点 | cli 拍板 |
|---|---|---|
| 1 | **命令名 `BIN`** | **`lingmiao`**（命令名与品牌统一，§1.2） |
| 2 | **用户文档载体** | **`help.json` 内嵌为准**（+ `/help`，两入口一真源） |
| 3 | **命名落地时机** | **写进方案；实现落地过程中全部更新**（见下） |
| 4 | **README** | **现在就重写** ✅（本轮已完成） |

**第 3 点含义**：命名决策**写进本方案**（docs），实际改名（`brand.rs` / `Cargo.toml` ×7 /
`README` 里的命令名 / `assets` / `.gitignore` / `run.sh`）在**实现阶段统一执行**——
不单开「改名提交」，而是随需求②③④ 的代码落地一起更新，避免 docs 与代码二次漂移。
（注：`brand.rs` 的改名与需求③ 的 `STATE_DIR` 退役**强耦合**——`brand.rs` 有
`STATE_DIR == "." + SLUG` 的守卫测试，SLUG 改名必须与 `STATE_DIR` 退役同批做，故一并延到实现阶段。）

---

## 6. 落地清单（实现阶段统一执行）

> **时机**（§5-3）：命名写进本方案，以下**代码 / 散文改动随实现阶段统一执行**（不单独改名提交）。

- [ ] `brand.rs`：`NAME` → `灵妙`、`SLUG`/`ENV_PREFIX`/`EVENTS_TARGET`/`BIN` → `lingmiao*`（`BIN` = `lingmiao`，§5-1）
- [ ] `7× Cargo.toml` + 根 `Cargo.toml` 散文（`description` / bin name = `lingmiao`）
- [x] `README.md` 重写（§2.3 —— **本轮已完成**）
- [ ] `assets/*.json` `_note` 品牌化
- [ ] `.gitignore`：`/.memory/` → `/.memory/`（与需求③一并）
- [x] 需求③ `memory-dir.md` / 需求② `api-config.md` 的 env 前缀字样订正为 `LINGMIAO_*`（§1.3 —— **本轮已完成**）
- [ ] `run.sh` / TUI 示例命令行：`lingmiao` → `lingmiao`
- [ ] 验证：`fmt` / `clippy -D warnings` / `cargo test` 全绿 + Xvfb 实跑看横幅/标题

---

*本文档随讨论更新；品牌与文档约定随需求④一并与 `rules.md` 对齐。*
