# API 配置机制设计（需求②）

> 状态：**已定稿**（2026-09-16 · 格式定为 **JSON** + 内置帮助/工具/提示词机制）
> 所属：需求② —— 编译后同级目录读固定配置文件、支持多组 provider/模型
> 约束：`rules.md` 优先级最高；版本号唯一信源 = Git tag（本文不写死版本）

---

## 1. 要解决的问题（cli 原话）

> 「现在的问题是每个 API 来源都需要单独的代码来适配，需要改引擎的代码。
>  我如果发布的话希望能保持目前这种随时编排和切换模型的能力，同时让用户可以容易使用。」

拆成三件事：

1. **加一个新 API 来源不该改代码** —— 现状是必须改、必须重新编译。
2. **保住「随时编排 / 切换模型」的能力** —— 原版 TUI 有模型下拉，随时热切换。
3. **让用户容易使用** —— 面向非技术用户：改一个文件就行。

**2026-09-16 追加（cli）**：

> 「给用户用的配置文件用 json 格式，同时需要良好的配套文档，内置在开发环境内，
>  让 AI 可以通过工具调用来获取帮助信息，且给 AI 配套相关的提示词。」

→ 配置格式由 TOML 改为 **JSON**；并新增「内置帮助文档 + `help` 工具 + AI 提示词」
一条完整链路（见 §6）。

---

## 2. 根因：代码把两件事揉进了一个枚举

现状（`crates/lingmiao-llm/src/provider.rs`）：`Provider` 是**编译期枚举**，3 个变体
`DeepSeek | Kimi | Claude`，每个变体**硬编码**了 env 名、默认 base_url、默认 model、
以及协议方言（`chat_url()` / `build_body()` 里的 `match`）。

于是两类完全不同的东西被塞进同一个 enum：

| 类别 | 例子 | 本性 | 该不该改代码 |
|---|---|---|---|
| **协议方言** | openai 的 `/chat/completions`、anthropic 的 `/v1/messages` | 真·代码（适配器） | 是，不可避免，但**极少** |
| **端点数据** | base_url、api_key、model id | 纯数据 | **绝不该** |

**结论**：因为把「数据」也焊进了「代码」，所以加一个来源（GLM / 通义 / OpenRouter /
本地 vLLM / Ollama …）都要改 enum + 重编译。**正解 = 把数据抽出来做成配置文件，
enum 只留「方言」这一维。**

---

## 3. 设计：方言（代码） × 模型组（数据）

### 3.1 只剩两种方言要写代码

- `openai` —— `/chat/completions` + `Authorization: Bearer`，OpenAI wire 格式。
  **覆盖绝大多数来源**：DeepSeek、Kimi、智谱 GLM、通义 Qwen、OpenRouter、SiliconFlow、
  本地 vLLM / Ollama（OpenAI 兼容端点）…… → 加这些来源 **零代码**。
- `anthropic` —— `/v1/messages` + `x-api-key`，Messages API 翻译层**已实现**
  （`crates/lingmiao-llm/src/anthropic.rs`，1:1 移植原版 `llm/anthropic.py`：OpenAI↔Anthropic
  的 message / tool / thinking / SSE 双向翻译）。`models.json` 里 `protocol: "anthropic"`
  的组（如内置 `claude`）直接可用，无需再改代码。

> 加一个 OpenAI 兼容来源 = 加一段 JSON；加一个全新协议 = 写一个新方言适配器（罕见）。

### 3.2 配置文件 `models.json`（与二进制同级 · JSON 格式）

用户点名的「编译后程序同级固定文件」。格式定为 **JSON**（2026-09-16 cli 拍板）——
与内部 assets（`prompts` / `stages` / `locks` / `mcp` 全是 JSON）统一，AI 读写可靠、
可做 JSON Schema 校验。

```json
{
  "_comment": "灵妙 模型配置。与程序同级；改完重启生效，界面里也可随时切换。加来源=加一个 group，无需改代码。完整说明：调用 help 工具，topic=\"models\"。",

  "default": { "group": "deepseek", "model": "deepseek-v4-pro" },

  "groups": [
    {
      "id": "deepseek",
      "label": "DeepSeek",
      "protocol": "openai",
      "base_url": "https://api.deepseek.com",
      "api_key": "sk-xxxx",
      "models": [
        { "id": "deepseek-v4-pro",   "label": "DeepSeek V4 Pro",   "thinking": true },
        { "id": "deepseek-v4-flash", "label": "DeepSeek V4 Flash" }
      ]
    },
    {
      "id": "glm",
      "label": "智谱 GLM",
      "protocol": "openai",
      "base_url": "https://open.bigmodel.cn/api/paas/v4",
      "api_key_env": "GLM_API_KEY",
      "models": [
        { "id": "glm-4.6", "label": "GLM-4.6" }
      ]
    },
    {
      "id": "claude",
      "label": "Claude",
      "protocol": "anthropic",
      "base_url": "https://api.anthropic.com",
      "api_key_env": "CLAUDE_API_KEY",
      "models": [
        { "id": "claude-fable-5", "label": "Claude Fable 5", "supports_vision": true }
      ]
    }
  ]
}
```

字段：

| 键 | 必需 | 说明 |
|---|---|---|
| `default.group` / `default.model` | 否 | 启动默认；缺省取第一个 group 的第一个 model |
| `groups[].id` | 是 | 组标识（= 一个 API 来源） |
| `groups[].label` | 否 | 界面显示名（缺省用 `id`） |
| `groups[].protocol` | 是 | 方言：`openai` \| `anthropic`（**唯一需代码的一维**） |
| `groups[].base_url` | 是 | 端点 |
| `groups[].api_key` / `api_key_env` | 二者其一 | 内联明文 / 读环境变量 |
| `groups[].models[]` | 是 | 组内多模型 |
| `models[].id` | 是 | 模型 id（发给 provider） |
| `models[].label` | 否 | 显示名 |
| `models[].*`（其余键） | 否 | 透传进请求体（如 DeepSeek `thinking`） |

> **2026-09-27（cli）**：**已移除 `models[].context_window`**。系统**不假设任何模型的上下文窗口大小**（provider API 对此不准确，硬编码值是猜的），故不再有窗口字段 / 占比 / 阈值配色 —— 上下文行只报 provider 实测 token（见 [`ux-design.md` §21](./ux-design.md)）。为使旧 `models.json` 里的该键**不被当作 `extra` 透传进请求体**，loader 仍**消费**但**忽略**它。

**JSON 无注释的补偿**：允许 `_` 前缀键（`_comment` / `_note`）承载极简内联提示，
解析时忽略——与现有 assets 的 `_note` 约定一致。真正的说明放 §6 的内置帮助文档。

### 3.3 加载优先级

`LINGMIAO_MODELS=<file>`（显式指定）→ `<exe 同级>/models.json` → `<cwd>/models.json`
→ 内置默认（deepseek/kimi/claude 三组预设，key 走 env）。任一环节缺失即向下回退，
与现有 `Config::discover()` 的「不兼容就回退内嵌默认」行为一致。

### 3.4 运行时的「编排 / 切换」

- **活动模型** = `(group.id, model.id)` 一对。
- **切换列表** = 把所有 `group × model` 拍平（替代原版硬编码的 `_MODEL_LIST`）。
  TUI 模型切换器遍历它；切换 = 用该条 spec 重建 `Client`（**无需改代码**）。
- **持久化** = 选中的组/模型写入状态目录 `config.json` 的 `llm` 键，
  重启后恢复（对齐原版 `factory.py` 的 `.memory/config.json` 热切换）。
- **热切换** = 在 Q2/Q3 的单 tokio 事件循环里发事件重建 client，无跨线程回调。

### 3.5 代码影响面（受控）

- `Provider` 枚举 → 缩成 `Dialect { OpenAi, Anthropic }`（方言确实该是代码）。
- `LlmConfig` → 换成 `ModelSpec { group_id, label, dialect, base_url, api_key, model, extra }`。
- `client.rs`：`match` 方言；`build_body()` 的 DeepSeek 专有 `thinking` 字段改为 `extra` 透传。
- 新增 `ModelRegistry`（读 `models.json`）：供切换器查询 + `switch_model()` 使用。
- `engine` / `tui`：provider 名显示改读 spec；`/model` 命令列出可选项。

> 现状核实（2026-09-16）：`provider.rs` 里 **`Dialect` / `ModelRegistry` / `models.*` 均尚未落地**，
> 仍是编译期 `Provider` 枚举。本节为落地目标，随 M 阶段实现。

---

## 4. 对下游需求的承接

- **面板② ContextBar 阈值**：~~`input_tokens / model.context_window` —— 窗口上限由此处
  `context_window` 字段提供，需求② 一并解决。~~ **2026-09-27 作废**：不再假设窗口（§21）。
- **`.memory`（需求③）**：无耦合，独立推进。
- **原版 `create_flash_client()`**：原版对非对话类阶段用便宜模型兜底 —— 是否需要
  「按阶段分配不同模型」的编排，见待拍点 E。

---

## 5. 决策（已定案 · 2026-09-16）

| # | 点 | 定案 |
|---|---|---|
| A | 配置格式 | **JSON**（`models.json`；与内部 assets 统一，AI 读写可靠、可 Schema 校验。`_` 前缀键作内联提示） |
| B | 结构 | **组内含多模型** —— `groups[]`（一个来源）+ `groups[].models[]`（组内多模型） |
| C | key 存放 | **内联 `api_key` 与 `api_key_env` 二者都支持**（就近取用）；`models.json` 进 `.gitignore` |
| D | 文件位置 | **`LINGMIAO_MODELS` → exe 同级 → cwd → 内置默认**，逐级回退（对齐现有 `Config::discover()` 语义） |
| E | 「编排」范围 | **v1 只做「随时切换单一活动模型」**；「按阶段/角色分配模型」保留接口 |
| F | 配套文档机制 | **内置帮助文档 + `help` 工具 + AI 提示词**（见 §6）；`help` 工具注册进所有 stage 白名单 |

> **A 的取舍说明**：JSON 无注释，故说明必须外置——这正是 cli「配置用 JSON +
> 良好配套文档」的组合逻辑：把「可读性」从注释转移到**内置帮助文档 + AI 提示词**。
>
> **E 的取舍说明**：原版其实两件事都做过——TUI 里切主模型 + `create_flash_client()`
> 给非对话类阶段兜底。但「按阶段分配」会让配置面与用户心智都复杂化，与需求②
> 「让用户容易使用」相冲突。故 v1 收敛为「单一活动模型热切换」；数据模型
> （`ModelSpec` 带 `group_id`）已预留按角色引用的能力，将来启用零重构（M4 决定是否开）。

---

## 6. 配套文档 · `help` 工具 · AI 提示词（2026-09-16 新增）

cli 的四点要求：①用户配置用 JSON；②良好配套文档，内置；③AI 可通过工具调用获取帮助；
④给 AI 配套提示词。落成一条**自文档化链路**。

### 6.1 内置帮助文档 `help.json`

- **位置**：`crates/lingmiao-core/assets/help.json`，`include_str!` 内嵌（对齐 Q8「内嵌四 JSON」约定），
  **随二进制发布**——即「内置在（运行）环境内」。
- **结构**：主题表，每个主题一段 Markdown 正文 + 关键词（供模糊检索）：

```json
{
  "_note": "灵妙内置帮助文档（随二进制发布）。AI 通过 help 工具查询；用户可读同名 docs/*.md。",
  "topics": {
    "overview":      { "title": "灵妙是什么",              "keywords": ["简介","overview","介绍"], "body": "…" },
    "install":       { "title": "安装与启动",              "keywords": ["安装","启动","install"], "body": "…" },
    "layout":        { "title": "程序与文件布局",          "keywords": ["目录","布局","layout",".memory"], "body": "…" },
    "models":        { "title": "配置模型（models.json）", "keywords": ["配置","模型","api","group","provider","key"], "body": "…" },
    "config":        { "title": "配置总览",                "keywords": ["配置","config","覆盖"], "body": "…" },
    "env":           { "title": "环境变量与 .env",         "keywords": ["env",".env","密钥","变量"], "body": "…" },
    "commands":      { "title": "快捷键与斜杠命令",        "keywords": ["快捷键","命令","slash"], "body": "…" },
    "whiteboard":    { "title": "小白板",                  "keywords": ["白板","whiteboard"], "body": "…" },
    "session":       { "title": "会话与历史",              "keywords": ["会话","历史","session"], "body": "…" },
    "pipeline":      { "title": "对话管线：组织上下文 → 工作阶段 → 沉淀阶段", "keywords": ["管线","阶段","流程"], "body": "…" },
    "memory":        { "title": "记忆体系与目录",          "keywords": ["记忆","memory",".memory"], "body": "…" },
    "tools":         { "title": "可用工具清单",            "keywords": ["工具","tool"], "body": "…" },
    "mcp":           { "title": "MCP 扩展",                "keywords": ["mcp","扩展"], "body": "…" },
    "logs":          { "title": "日志与诊断",              "keywords": ["日志","log","诊断"], "body": "…" },
    "troubleshoot":  { "title": "常见问题（FAQ）",         "keywords": ["faq","排障","问题"], "body": "…" },
    "errors":        { "title": "错误说明",                "keywords": ["错误","error"], "body": "…" },
    "for-ai":        { "title": "给 AI：生成/修改配置与用法", "keywords": ["ai","生成","schema"], "body": "…" }
  }
}
```

> **主题范围（2026-09-16 cli 拍板：完整覆盖）**：不做「先核心后扩」的裁剪，首次落地即覆盖
> **全部功能面**（入门 / 配置 / 使用 / 原理 / 排障 + 给 AI），上表 17 个主题为初始全集。
> 分组视图：**入门**（overview/install/layout）· **配置**（models/config/env）·
> **使用**（commands/whiteboard/session）· **原理**（pipeline/memory/tools/mcp）·
> **排障**（logs/troubleshoot/errors）· **给 AI**（for-ai，schema）。

- **两个受众、两份载体**（不重复劳动）：
  - `docs/*.md` = **开发者设计文档**（本仓库设计过程、为什么这么做）；
  - `assets/help.json` = **运行时帮助**（面向用户与 AI，「怎么做」）。
- `for-ai` 主题给出 `models.json` 的**准确 schema + 示例**，让 AI 能直接生成/修改配置。

### 6.2 `help` 工具（lingmiao-tools 新增）

- **name**：`help`（短、直观；与用户侧斜杠命令 `/help` 呼应）。
- **参数**：`{ "topic": "string" }`（可选）。
- **行为**：
  - 不带 `topic` → 列出全部主题（id + 标题）；
  - 带 `topic` → 返回该主题的 Markdown 正文；
  - 未知 `topic` → 关键词模糊匹配给建议 + 列出全表。
- **性质**：只读、非变更；**注册进所有 stage 的工具白名单**（AI 随时可查自身用法）。
- 复用现有 `Tool` trait + `ToolRegistry`（Q6），无新机制。

### 6.3 AI 提示词

- **`prompts.json` 的 `_base`** 追加一行：

  > 「需要了解自身的配置、功能或工具用法时，调用 `help` 工具查询内置文档，不要凭记忆猜测。」

- **`help` 工具的 `description` 本身即最关键的提示**——它直接进 OpenAI canonical
  tools array，模型每轮都能看到「何时该调 help」。
- **`for-ai` 主题**承载「如何正确生成/修改 `models.json`」的 schema 与示例。

### 6.4 与需求④的关系

本机制 = **需求④「灵妙配套文档机制」的第一块落地**（2026-09-16 cli 定：**首次即完整覆盖全部主题**，
见 §6.1 的 17 主题全集；后续随功能演进继续补 `help.json`，零代码）。

---

## 7. 影响面小结

| 层 | 改动 |
|---|---|
| `lingmiao-core` | 新增 `assets/help.json` + `Config` 暴露 `help_topics()`；`ModelRegistry` 读 `models.json` |
| `lingmiao-llm` | `Provider` → `Dialect`；`LlmConfig` → `ModelSpec` |
| `lingmiao-tools` | 新增 `help` 工具；注册进默认 registry 与所有 stage 白名单 |
| `assets/prompts.json` | `_base` 追加 help 指引；`assets/stages.json` 各 stage 加 `help` |
| `assets/models.json`（新） | 用户编辑面；`.gitignore` 排除 |
| `docs/` | 本文件（设计）；`help.json` 的正文可与 docs 共享素材 |

---

## 8. `config.json` —— 用户配置文件 + 阶段模型路由（cli 2026-09-27）

**（cli 2026-09-27）** cli 拍板：「用户在 lingmiao 二进制同层次做一个 `config.json`，用于所有用户配置，其中包括 agent 的模型每个阶段」。

### 8.1 文件与优先级

程序按下列优先级找 `config.json`（任一缺失即向下回退，全缺则用内置默认 + `models.json` 链）：

1. `LINGMIAO_CONFIG=<file>`（显式指定）
2. **二进制同级目录**的 `config.json`
3. 当前工作目录的 `config.json`

> 注意：Linux 下 `current_exe()` 会解析符号链接。debug 阶段 `/usr/local/bin/lingmiao` 软链到 `target/debug/lingmiao`，故「二进制同级」= `target/debug/`。程序**同时**看 exe 目录与 cwd，放项目根也能被读到（与 `models.json` 的做法一致）。

`config.json` 已进 `.gitignore`（可能内联明文 key）。

### 8.2 结构

```json
{
  "models": {
    "default": { "group": "deepseek", "model": "deepseek-v4-flash-vision-exp" },
    "groups": [ /* 与 models.json 的 groups 完全同构 */ ]
  },

  "stages": {
    "组织上下文": { "group": "deepseek", "model": "deepseek-v4-flash" },
    "工作阶段":      { "group": "deepseek", "model": "deepseek-v4-pro" },
    "沉淀阶段":    { "group": "kimi",     "model": "kimi-for-coding" }
  }
}
```

| 键 | 必需 | 说明 |
|---|---|---|
| `models` | 否 | 模型目录（`models.json` 同构）。缺省 → 回退 `models.json` 链（§3.3）。 |
| `stages` | 否 | **阶段 → 模型** 路由。键必须是管线全名：`组织上下文` / `工作阶段` / `沉淀阶段`（旧名 `B-上下文选择` / `C-对话` / `总结流程` 仍兼容）。 |
| `stages.<阶段>.group` | 是 | API 来源（目录里的 `groups[].id`）。 |
| `stages.<阶段>.model` | 否 | 组内模型 id。留空 = 该组第一个模型。 |

### 8.3 语义与回退

- **一个 group 就是一个 API 来源** → 不同阶段可以走**不同厂商**（例：`沉淀阶段` 用 Kimi，`工作阶段` 用 DeepSeek）。这就是「阶段可用不同 API」的实现方式，无需改代码。
- 某阶段未写（或整块 `stages` 缺失）→ 该阶段用默认模型，行为与旧版（无路由）完全一致。
- 路由里的 group/model 不存在、或该组 `api_key_env` 未设 → 该条**跳过并 `warn`**，阶段回退默认模型；**不会**让启动失败（与记忆层初始化同样的乐观策略）。
- 路由条目若解析结果等于默认模型，视为**冗余**（不建 client），故 `engine.stage_routes()` 只列真正的覆盖。
- `{model}` 提示词占位符在每阶段渲染时填**该阶段实际使用的模型**（路由生效时不再是默认模型）。
- 启动时若存在任何路由，UI 在启动横幅后打一行 `· 阶段模型路由：工作阶段=deepseek/deepseek-v4-pro · …`（否则额外 provider 不可见）；日志同时 `info!` 一行。
- `DEEPSEEK_MODEL` 之类**旧式 `model_env` 覆盖不作用于路由条目**（路由点名了确切模型，被 env 悄悄改写＝撒谎）；它仍作用于交互目录（`models`/`models.json`）。

### 8.4 实现落点

| 层 | 改动 |
|---|---|
| `lingmiao-llm` | 新增 `userconfig.rs`（`UserConfig` / `StageSel`，含加载优先级 + 单测）；`ModelRegistry::load()` 先看 `LINGMIAO_MODELS`，再看 `config.json` 的 `models`，再走 `models.json` 链；新增 `ModelRegistry::from_json_sourced` / `resolve_checked`；`build_with_key` 增加 `honour_model_env` 开关 |
| `lingmiao-engine` | `Engine` 增 `llm_by_stage: HashMap<String, Client>` + `llm_for(stage)` / `stage_routes()`；`stage_agent()` / `render_stage_system()` / `turn()` 改用 `llm_for()`；`from_env` 构建路由（`build_stage_clients`） |
| `lingmiao-tui` | 身份行显示 `工作阶段` 的路由模型；启动打一行路由提示；`/model` 文案改为指向 `config.json` |
| `assets/help.json` | `models` / `config` / `layout` / `env` / `for-ai` 五个主题同步 `config.json` 与 `stages` 说明 |
| `.gitignore` | 新增 `/config.json` |

### 8.5 验证

- `cargo test --workspace` 全绿；`lingmiao-llm` 新增 4 个 `userconfig` 单测 + `resolve_checked` 相关；`lingmiao-engine` 新增 `stage_routing_builds_a_client_per_stage` / `unresolvable_stage_routes_fall_back_to_the_default` / `llm_for_prefers_the_stage_route_and_defaults_otherwise`。
- **computeruse :99 真进程**：写一份 `stages` 路由文件 → `LINGMIAO_CONFIG=… ./target/debug/lingmiao` → 日志出现 `config.json stage routing active routes=组织上下文=deepseek/deepseek-v4-flash, 工作阶段=deepseek/deepseek-v4-pro, 沉淀阶段=deepseek/deepseek-v4-flash`；界面身份行显示 `deepseek-v4-pro`（`工作阶段` 路由），启动提示行显示三阶段路由；发一轮真实提问跑通 组织上下文 → 工作阶段。
