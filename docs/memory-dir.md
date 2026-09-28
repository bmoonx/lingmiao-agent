# 记忆目录设计（需求③）

> 状态：**已定稿**（2026-09-16 · 3 个开放点已由 cli 拍板，见 §6）
> 所属：需求③ —— 记忆改存 `.memory/` 文件夹（替代此前的单一状态目录）
> 约束：`rules.md` 优先级最高；版本号唯一信源 = Git tag（本文不写死版本）

---

## 0. 拍板结论（2026-09-16 cli）

cli 对 3 个开放点的答复（原话：`1、每个项目一份，跟现在保持一致 2、项目里给个缓存路径 3、统一`）：

| # | 开放点 | cli 拍板 | 对上一版草案的改动 |
|---|---|---|---|
| 1 | 记忆「全局一份」vs「每项目一份」 | **每项目一份**（跟现在保持一致） | 锚点 = **cwd**；**撤销**「exe 同级」 |
| 2 | 易失物（logs/tmp/loop）去向 | **项目内给个缓存路径** | **撤销**「系统缓存 `~/.cache/`」 |
| 3 | env 前缀统一 | **统一**到 `brand::env()` 派生 | 按推荐执行 |

**「程序同级」的语义落点**：需求③原话是「程序同级 `.memory/`」。cli 本轮明确取「每项目一份」——
即按「程序在项目目录内运行 → 项目目录即程序所在目录」理解，锚点落在**项目目录（cwd）**，
而不是可执行文件 `lingmiao` 的物理所在目录（后者才是「全局一份」）。

---

## 1. 要解决的问题（cli 原话）

> 「③记忆存于程序同级 `.memory` 文件夹（替代当前的单一状态目录）。」

拆成两件事：

1. **位置** —— 记忆目录从旧的单一状态目录改为 `.memory/`；**保持每项目一份**（锚点仍为项目目录）。
2. **语义分离** —— 旧状态目录把「持久记忆资产」与「易失运行物」混在一起，要拆开：
   记忆进 `.memory/`（要备份/带走），易失物进**项目内的缓存路径**（随时可删）。

---

## 2. 现状（读源码实锤）

`crates/lingmiao-core/src/paths.rs` + `brand.rs`：

- 旧状态目录 `brand::STATE_DIR`（单一目录），**挂在进程工作目录（cwd）下**——
  Claude-Code 风格：cwd = 项目根，内部状态藏于项目下的隐藏目录。
- `Paths::at(root)` = `root/<state>/{memory,tmp,logs,loop}`（旧布局）。
- `Paths::prepare()`：建目录 → **清 `memory/` 白名单外的文件** → 建 4 个空库。
- 白名单 `ALLOWED_DBS` = 7 个库：`context_record / observations / knowledge /
  business / schedules / whiteboard / autonomous`（+ 各自 `-wal/-shm/-journal`）。
- 启动序列在 `crates/lingmiao/src/main.rs`：① `Paths::detect()`（cwd 根）→ ② `load_env`
  → ③ `prepare()` → ④ `logging::init(&paths.logs_dir)`。
- `.gitignore` 忽略了旧状态目录（另有 `/.cache/` `/.fastembed_cache/`）；
  `rules.md §3` 记旧状态目录 = 运行时内部状态目录。
- 本机实际布局（多出来的）：`{constraints/, trajectories/, memory/, logs/}`。

**问题**：旧状态目录把两类本性完全不同的东西混在一起——

| 类别 | 内容 | 本性 | 用户想怎么对待 |
|---|---|---|---|
| **持久记忆资产** | `memory/*.db`（7 库）、嵌入索引、`constraints/`、`trajectories/` | 资产，**要带走 / 备份 / 可读** | 跟项目一起、便携、能拷 |
| **易失运行物** | `logs/`、`tmp/`、`loop/` | 垃圾，**随时可删** | 不该被备份、不进交付物 |

**位置结论**：保持每项目一份（锚点 = cwd），与原版行为一致——**只改名 + 拆两类**，不改「跟着项目走」这条。

---

## 3. 设计：`.memory/`（项目内 · 记忆专属）+ 项目缓存路径

### 3.1 布局

```
<项目目录>/（cwd —— 每项目一份）
  .memory/                  ← 记忆本体（持久资产，要备份/带走）
    context_record.db       ← 7 个记忆库（白名单）
    observations.db
    knowledge.db
    business.db
    schedules.db
    whiteboard.db
    autonomous.db
    constraints/            ← 自学习约束（auto mode）
    trajectories/           ← 轨迹 JSONL
    embeddings/             ← 嵌入索引（如有；模型权重另见下）

  .cache/lingmiao/          ← 项目内缓存路径（易失，随时可删）
    logs/                   ← 日志（原旧状态目录/logs/）
    tmp/                    ← 草稿（原旧状态目录/tmp/）
    loop/                   ← 循环状态（原旧状态目录/loop/）
```

**一句话**：`.memory/` 只放**记忆本体**（要备份的东西）；日志 / 临时 / 循环状态是易失垃圾，
挪进**项目内的缓存路径** `.cache/lingmiao/`，不污染记忆、不进备份、不混进交付物。

- 缓存目录名 `.cache/lingmiao/`（`lingmiao` = `brand::BIN` 派生）。
- `.gitignore` 已有 `/.cache/`，天然覆盖该缓存路径——无需新增忽略规则。
- 嵌入**模型权重**是共享的大文件（同一份 MiniLM），保持全局缓存 `~/.cache/lingmiao-models/`（`embed.rs` 现行为），
  不随项目复制；`.memory/embeddings/` 若存在只放**本项目**的索引数据。

### 3.2 位置解析

```
LINGMIAO_MEMORY   →  <cwd>/.memory                    （env 显式覆盖，最高优先）
LINGMIAO_CACHE_DIR →  <cwd>/.cache/lingmiao           （易失物缓存根）
默认（无 env）→  <cwd>/.memory  与  <cwd>/.cache/lingmiao
```

- 锚点 = cwd（每项目一份），与旧状态目录同级同锚。
- 不再需要「exe 同级」「`~/.local/share/` 末级回退」——项目目录是用户自己的工作区，可写。
- 与需求② 的 `LINGMIAO_MODELS`（exe 同级 → cwd → 内置默认）**共用同一套 `resolve_user_dir()` 回退工具**，
  但需求③的锚点更简单（cwd + env 覆盖）。

### 3.3 清理白名单（对应 `prepare()`）

`cleanup_memory_dir()` 规则从「`memory/` 下只留 7 DB」升级为：

- `.memory/` **顶层**允许：7 个 DB（+ journal）+ **允许子目录** `embeddings/ / constraints/ / trajectories/`；
- 其余**顶层散落文件**删除（防脏）；
- **不递归**进子目录（子目录内容各自管理，索引天然可重建）。

### 3.4 迁移（旧状态目录 → 新 `.memory/`）

- **v1 不自动迁移**（对齐收尾决策「记忆全新开始，不兼容旧库」）。
- 首次启动若发现 cwd 下有旧的 `<state>/memory/*.db`，仅**提示一次**（指向新位置），不搬、不删。
- 将来可提供独立 `lingmiao migrate`（Q3 已留「其余操作走 TUI/斜杠命令」的口子）。

### 3.5 代码影响面

| 层 | 改动 |
|---|---|
| `lingmiao-core/brand.rs` | 新增 `MEMORY_DIR = ".memory"`（固定约定名，非品牌派生）+ `CACHE_DIR = ".cache"`（易失物根）；`STATE_DIR`（旧单一状态目录）退役 |
| `lingmiao-core/paths.rs` | `Paths` 字段重划：`memory_dir=<cwd>/.memory`；`tmp/logs/loop_dir=<cwd>/.cache/lingmiao/*`；`state_dir` 字段 → `cache_dir`；白名单按 §3.3 |
| `lingmiao-core/logging.rs` | `logs_dir` 指向 `.cache/lingmiao/logs`（调用方无需改，取 `paths.logs_dir`） |
| `lingmiao/src/main.rs` | 启动序列不变（仍 `Paths::detect()`），仅新增「旧状态目录提示」一步 |
| `lingmiao-memory/embed.rs` | 现 `LINGMIAO_EMBED_MODEL_DIR` → `brand::env("EMBED_MODEL_DIR")` = `LINGMIAO_EMBED_MODEL_DIR`；模型权重缓存保持全局（现硬编码 `~/.cache/lingmiao-models/`，随定名改 `~/.cache/lingmiao-models/`） |
| `lingmiao-tools/computer_use.rs` | 现 `LINGMIAO_COMPUTER_USE` / `LINGMIAO_COMPUTER_USE_DISPLAY` → `brand::env(...)` = `LINGMIAO_*` |
| `envkeys.rs` | `~/<STATE_DIR>/.env`（全局 key 文件）保留；`state_dir` 引用改 `brand` 常量 |
| `.gitignore` | `/.memory/` → `/.memory/`；`/.cache/` 保留 |
| `rules.md §3` | 旧状态目录表述改 `.memory/`（记忆）+ `.cache/lingmiao/`（易失） |
| `run.sh` | 注释里 `LINGMIAO_CONFIG_DIR` 说明统一为 `LINGMIAO_CONFIG_DIR`（脚本已做转换）；示例命令 `lingmiao` → `lingmiao` |

---

## 4. 与其它需求的关系

- **需求②（models.json）**：同为「程序读取固定文件」，共享 `resolve_user_dir()` 回退工具；env 前缀同批统一。
- **需求④（文档机制）**：`help.json` 的 `memory` / `layout` 主题即本文的用户版说明。
- **面板② ContextBar**：无耦合（占比走 provider `input_tokens`）。

---

## 5. 决策点（已定案）

| # | 点 | 定案 |
|---|---|---|
| A | **内容范围** | `.memory/` 只放**持久记忆资产**；易失物（logs/tmp/loop）移**项目内缓存** `.cache/lingmiao/` |
| B | **位置解析** | `LINGMIAO_MEMORY` → `<cwd>/.memory`（每项目一份，无 exe 同级）|
| C | **目录名** | `.memory` **固定字面量**（与 `.git`/`.env` 同类约定，非品牌派生） |
| D | **清理白名单** | 顶层 = 7 DB + 3 允许子目录；散落文件删；不递归 |
| E | **迁移** | v1 不自动迁移，仅提示；`lingmiao migrate` 延后 |
| F | **env 前缀** | **统一到 `brand::env()` 派生**（定名后 `LINGMIAO_*`），消灭 `LINGMIAO_*` 硬写 |

---

## 6. 开放点 → 已拍板（存档）

1. **每项目一份**（跟现在保持一致）→ 锚点 = cwd。✅ 定案。
2. **项目内缓存路径** → 易失物放 `<项目>/.cache/lingmiao/`。✅ 定案。
3. **统一** → env 前缀统一到 `brand::env()` 派生。✅ 定案。

**✅ env 统一清单**（一处改全部）：现 `LINGMIAO_EMBED_MODEL_DIR`、`LINGMIAO_COMPUTER_USE`、
`LINGMIAO_COMPUTER_USE_DISPLAY`，及新拟的 `LINGMIAO_MEMORY` / `LINGMIAO_CACHE_DIR` / `LINGMIAO_MODELS`
→ 全部改走 `brand::env()`，定名后得 `LINGMIAO_*`（`LINGMIAO_MODELS` / `LINGMIAO_MEMORY` / …）。

**✅ 缓存目录名已定**：`<项目>/.cache/lingmiao/`（`lingmiao` = `brand::BIN` 派生）。
