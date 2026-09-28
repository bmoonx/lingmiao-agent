# UX 设计 — 灵妙（lingmiao）

> **活文档**：随讨论持续更新。记录 UX 改进方案（4 项需求）的设计过程与定案。
> - **架构性决策**（布局形态 / 面板职责 / 主题策略）→ 相对稳定。
> - **UI 细节**（间距 / 字号 / 具体文案）→ 可调整，不必锁死。
> - 系统架构见 [`architecture.md`](./architecture.md)，制度约束见 [`rules.md`](../rules.md)。

---

## 0. 设计方式（硬性要求）

- **全程用 computeruse 启动真实窗口对比取证，不得凭空设计**（用户点名第 1 条）。
- **交互模式：纯键盘，无鼠标操作** —— 完全模仿 CC（cli 2026-09-16 19:37 拍板，见 §11）。
- 三方真实窗口基线：CC（Claude Code）、原版（Python Textual）、当前 lingmiao。

---

## 1. 基线（computeruse 实拍）

### CC v2.1.273
- 终端白底，**不自绘背景**（完全用终端 ANSI palette）。
- 反色提问行（`❯` 前缀 + 整行底纹）/ 思考灰行（`∴`，**正体**；默认折叠为一行摘要 —— §12.7 实拍订正：旧文档误记为「斜体」）。
- 工具卡：`●Bash(echo 你好)` `⎿ 你好`。
- 尾行 `◜ Churned for 4s · done 18:39`（灰、正体；2026-09-28 §27.3 去掉 CC 星形）/ 右下 Token 计数。

### 原版 原版 Python v0.2.289（Textual）
- 终端黑底，**不自绘背景**；三栏布局。
- 左栏：状态 + 模型下拉 + 功能列表；中栏：对话；右栏：小白板。
- 底部：输入框 + 常驻快捷键提示行。

### lingmiao v0.6.0（需求① 之前的基线）
- 单栏极简（`lingmiao-tui` app.rs + lib.rs ≈ 923 行），缺侧栏 / 监控面板 / 上下文条。
- **> v0.7.0 起已落地**：侧栏四段常驻 + CC 式对话主区，详见 §11「实现说明」。

---

## 2. 布局（已定案 ✅ · ⚠ 已被 §12 取代）

| 拍板点 | 结论 |
|--------|------|
| 第 1 拍板点 · 布局形态 | **否定方案 A（纯 CC 单栏）→ 方案 D**：侧栏常驻 + CC 式对话主区 |
| 第 2 拍板点 · 栏数 | **D1 = 两栏**（侧栏 ｜ CC 对话），最干净；放弃 D2（两栏+白板折叠）/ D3（三栏硬还原） |

**理由**：用户需要「高级感」，用户不一定是专业程序员（同事也没摸清 CC 的功能）；要 Textual 质感 + CC 的细节品位。**约束**：本项目 UI 用 ratatui（Q2 已定），是**复刻 Textual 观感**，非引入 Textual。

**新增两项设计目标**：① 高级感（对标 Textual）② 可发现性（面向非技术用户）。

---

## 3. 侧栏结构（D1 · ⚠ 已被 §12 取代）

上 → 下四段：

```
┌─────────────────────────┐
│ ① 顶部状态区             │  灵妙版本·时间 / 模型行(可下拉) / 阶段状态行
├─────────────────────────┤
│ ② 中部 功能导航列表      │  对话/小白板/记忆/会话/工具/配置/帮助
│                         │  (中文+图标，可上下选中)
├─────────────────────────┤
│ ③ 底部 小白板 (≤1/3 高)  │  常驻显示
├─────────────────────────┤
│ ④ 最底 Token/进度 + 快捷键│  MeterBar + 常驻快捷键提示
└─────────────────────────┘
```

**小白板落点**（已定案 ✅）：**常驻侧栏最下区，占侧栏高度 ≤ 1/3**（否决赛讨论中的「中区 ^b 切换」方案）。理由：白板始终可见 → 可发现性更强，呼应非技术用户「别让用户猜」。

---

## 4. 主区 · CC 式对话流（已实现 ✅）

按 CC / 原版 的行首字形落地（对照原版 `cc_clone.py` / `conversation.py`）：

| 行 | 字形 | 样式 | 对应 CC / 原版 原版 |
|----|------|------|--------------------|
| 用户 | `❯ 文本` | `❯` 强调色粗体 | `❯`（CC 橙）/ `👤 用户`（原版） |
| 思考 | `∴ 文本` | 折叠 / 展开均灰正体 + 关键词提亮（§12.7；2026-09-24 CC 对齐） | `∴`（CC 灰 / 正体 / 正常字距，page27 实拍）/ `∴`（原版灰） |
| 回复 | `● 文本` | `●` 正文色粗体，续行缩进对齐（§12.12：去橙，回归 CC 单色） | `●`（CC/原版） |
| 工具·运行中 | `⠋ 名称(主参数) · Ns` | 黄色盲文 spinner | `⠋ name(args) · Ns`（原版） |
| 工具·完成 | `● [阶段] 英文名(主参数) 12ms` | `●` **绿**（失败红）粗体，**阶段标签**低饱和 `STAGE_TAG`，名称粗体，参数正文色（§18 ⑥⑦） | `● name(args) (12ms)`（原版） |
| 工具结果 | `  ⎿ 结果` | **中灰**（`TOOL_RESULT`=ANSI 37，比思考 `TEXT_MUTED`=#AAAAAA 亮一档；§12.16 订正 §12.15 的最亮 `TEXT`） | `⎿`（CC/原版） |
| 尾行 | `◜ {动词} for 12.4s · done 11:29:46`（列 0 起，§12.18） | 灰 | `✻ Brewed for 4s`（CC）· **2026-09-24 按 CC 去掉 `· N 个工具`**（§12.13） |

- **强调色** = CC 品牌橙 `#D77757`（即 §5 的「单一强调色」）；思考**折叠摘要**用中性灰（`TEXT_MUTED`，正体），**展开**亦为中性灰正体（`TEXT_MUTED`，去青 / 去斜体，对齐 CC —— page27 实拍），其中**代码 / API 关键词**提亮到**柔蓝** `THINKING_KW`（`#B1B9F9`，§12.19；2026-09-24，无加粗）。
- **工具名英文**（cli 2026-09-24 ⑥）：直接显示工具**原名**（`bash` / `read_file` / `search_memory`…），与 CC 的 `Bash` / `Read` 同族 —— 订正早先的「说人话中文名」（`tool_label()` 已删，见 §18）。
- **主参数**按工具取规范键（`bash→command`、`read_file→file_path`…），渲染成 `名称(值)`，与 CC 的 `Bash(echo 你好)` 同形。
- **思考流**：工作阶段 `reasoning_content` → `∴` 块，在回复 / 工具卡之前冲入（`commit_thinking`），与 CC「先想后答」顺序一致；**默认折叠为一行灰摘要**，`ctrl+o` 展开全文（§12.7，2026-09-21 起；此前为「默认展开」）。
- **运行中转场**：`ToolStarted` → `⠋ … · Ns`；`ToolCalled` → 完成卡。repaint 由 100ms ticker 驱动出 spinner 动画。
- **阶段徽标**（2026-09-18）：给对话区每条 `∴` 思考 / `●` 回复 / `●` 工具卡 / 运行中工具卡前缀一个阶段标签 —— `[检索记忆]`（组织上下文）/ `[正在作答]`（工作阶段）/ `[沉淀整理]`（沉淀阶段）。**§12.13（2026-09-24「先按 CC 的样式做」）曾将其全去**；**§18 ⑦（2026-09-24）又要求「加回、饱和度拉低降低存在感」，故现为低饱和 `STAGE_TAG`（`#6B7480`）标签**。组织上下文的检索工具与思考**仍不隐藏**（此项**反转**了早先的 minor-5 过滤；沉淀阶段的机器 JSON 与内部记账工具仍不入对话区）。
- **Markdown 渲染**（2026-09-18）：回复按 Markdown 渲染 —— 标题去 `#` 变强调色粗体、列表成 `-`/`1.`、代码块去 ``` 围栏并缩进、行内代码去白底、粗体/斜体生效；**流式过程也走同一渲染**（边流边渲染，不再先示原文）。实现 = `tui-markdown` 解析 + 后处理（其 0.3.6 设计上保留 `#` 与 ``` 标记，故需后处理去除）。

> **验证状态**：`v0.8.0` 起已由 **computeruse :99 真实进程**实拍确认 —— 真回合「用两句话解释…」
> 对话区出现 `∴` 思考块（2026-09-21 起默认折叠为一行灰摘要；此前为灰青斜体全文）→ `●` 回复 → `◜ {动词} for … · done …` 尾行。2026-09-18 实拍确认：回复以 Markdown 渲染（标题/列表/代码块去标记）。
> **2026-09-24 按 CC 去除阶段徽标**（§12.13）：每条内容不再带 `[检索记忆]`/`[正在作答]`/`[沉淀整理]` 徽标。
> 结构自检另有 `cargo run -p lingmiao-tui --example preview`。

（细节可调；目标是复刻 CC 的「每处细节都打磨到位」的观感。）

---

## 5. 主题（已定案 ✅）

**终端原生（terminal-native / ANSI）**，**不硬编码底色**。

- 实拍证据：CC = 终端白底、原版 Python = 终端黑底，二者**都不自绘背景**，直接用终端 ANSI palette。
- 做法：theme 令牌层 = 「引用终端 ANSI 色 + 单一强调色（只用在关键动作）+ 边框字符集 + 内边距常量」，一处定义、全局引用。
- 质感靠**结构**（边框/缩进/留白），不靠自绘配色。统一 = 高级；随手各写各的 = 廉价。

---

## 6. 原版 TUI 六大痛点与对策

| # | 痛点（cli 提 + 源码/实拍核实） | 根因 | 对策 |
|---|-------------------------------|------|------|
| ① | 输入框单行，不能多行 | Textual `Input` 单行 | 多行 + **带光标行编辑**：Enter 发送 / Shift+Enter 换行 / 自适应增高 / 折行编辑；←→ 移光标 · Home/End 行首尾 · `Ctrl+A/E` 行首尾 · `Ctrl+U/K` 删到行首/行尾 · `Ctrl+W` 删词 · `Ctrl+Z` 撤销栈 · 按词移动（§12.8） |
| ② | 卡 | 双线程 + 40+ 处 `call_from_thread`，GIL 饿死渲染 | tokio 单循环（Q2/Q3 已定） |
| ③ | ContextBar「上下文组成条」形同虚设 | 比例全为硬编码魔数估算 | **做成真计量**（见 §8） |
| ④ | 阶段框（A-J 阶段表）仅调试信息 | 把调试信息当界面 | 翻译成人话（见 §7） |
| ⑤ | 复制粘贴不好用 | ctrl+y/shift+insert + pyperclip 外部依赖 | 内建剪贴板：拖动选中（左/右键）+ `⌃Y`（或**有选中时**的 `⌃C`）复制 + `⌃V`/`shift+insert` 粘贴 + `Esc` 丢弃选择 / 零外部依赖（§24、§28） |
| ⑥ | 时间/状态/控件更新不及时 | `call_from_thread` marshal（同 ②） | 单循环根治 |

**根因归纳**：6 条中 4 条同源 = 双线程模型。③④ 细分：**③ = ContextBar**、**④ = A-J 阶段表**，是侧栏里两个不同控件。

---

## 7. 面板① 状态监控面板（重做痛点④）

原版把 `A-J 阶段表` 直接铺出来（内行黑话 + 空表 + 「等待 B 阶段上下文数据…」）。重做成**三块**：

```
┌─ 状态 ───────────────────┐
│ 灵妙 vX.Y.Z     18:36:48  │  品牌·时间（活的，秒跳）
│ ● 就绪  DeepSeek V4 Pro ▾ │  状态灯 + 模型(可切)
├─ 本轮处理 ────────────────┤
│ ✓ 检索记忆          0.3s  │  已完成=绿勾 + 真耗时
│ ✓ 整理上下文        0.8s  │
│ ◐ 正在作答          1.2s  │  进行中=动 spinner
│ ○ 沉淀知识               │  未开始=灰点
├─ 本次会话 ────────────────┤
│ 12.3K↓  4.1K↑  🔧 3 次   │  真 token + 真工具数
│ ⏱ 4.2s    会话 f3a2       │
└──────────────────────────┘
```

**改动要点**
- **阶段去黑话**：`组织上下文 / 工作阶段 / 沉淀阶段` → **检索记忆 / 正在作答 / 沉淀整理**（正好对上 Q9 收敛后的 3 阶段管线）。
- **情绪价值**：动 spinner、逐帧跳动的计时、完成时 ✓ 变绿 + 一行本轮小结。
- **实用**：真实分阶段耗时（哪步慢一眼看出）、真工具调用数、错误/中断直接红显。
- **固定不抖**：内容变化不引起重排。

> 三块结构默认照此稿定；如有异议可调整。

---

## 8. 面板② 上下文组成条（做成真的痛点③）

### 8.1 定案：**完整常驻** ✅（cli 19:15 拍板 · ⚠ 放置位置已被 §12 取代：改 footer，无 %）

否决了「甲（常驻一行 + 点开）」与「乙（收进诊断视图）」两案，选**完整常驻**。

**动机 —— 「便利店选址」心理学**：
- **零行动成本 / 路径依赖**：便利店的价值在「顺路」不在「好」；常驻 = 扫一眼成本 ≈ 0，点开 = 专程跑一趟。
- **可得性启发**：信息要在眼前，「该精简了」的念头才会自己冒出来；藏起来 = 等于不存在。
- **曝光效应**：常驻条每次路过都在无声强化「我在掌控」。
- **持续环境意识**：像仪表盘，低到不打扰但一直在。
- **焦虑拦截**：在「是不是快满了」焦虑冒头那一瞬间就地给答案。
- **反向约束**：便利店从不拿喇叭喊你进店 → 常驻的心理成本要压到最低（不闪 / 不跳 / 不常驻红字）。

### 8.2 关键修正（实拍认错）

> **原版 ContextBar 本来就是常驻的** —— 它一直固定在侧栏最底部，只是显示空态「等待 B 阶段上下文数据…」。
> 所以「常驻」不是它失效的原因。真正公式 = **选址（位置）× 完整门面（常驻）× 真货（真计量）**，三者缺一不可：
> - 缺真货 → 原版：漂亮的假仪表盘（魔数系数）。
> - 缺选址 → 原版：明明常驻，却埋在侧栏最底部死区。
> - 缺完整门面 → 「甲」方案：橱窗里只摆招牌，想买东西还得进店。

### 8.3 分层原则（升级，非否定）

| 层 | 上轮 | **现在** |
|----|------|---------|
| **常驻层** | 一行极简条（只给情绪价值） | **完整橱窗**：真分段构成 + 各段占比 + 阈值状态，**不用点开** |
| **展开层** | 点开才看真诊断 | **动手层**：**键盘选中**某分段（方向键切换 + Enter）→ 该段明细 / 裁剪建议 |

### 8.4 两条硬约束

1. **颜色 = 信号，不是装饰**：常态一律中性灰；**只有越过阈值才变色**：`<70%` 绿·充裕 / `70–90%` 黄·偏满 / `>90%` 红·接近上限。颜色一出现即「有事」，余光扫一眼就懂。
2. **固定不抖**：行数、列宽、分段顺序全固定，数据变了也不重排。

### 8.5 「做成真的」三步法（不再拍脑袋）

原版为什么假（源码实锤 `monitor.py compute_context_segments()`）：比例全是魔数系数（`n_obs*75` / `n_nodes*50` / `n_turns*125` / `tools=2800` / `skills=75` / `strategy=20`），`n_obs/n_nodes` 只从最新一条 B stage 取一次；分段名是内部实现词；唯一真值 = provider `input_tokens`，堆叠条自注 `estimate`。

真做法：

1. **注入时记账** —— 工作阶段拼 prompt 时每段文本就在手里，直接记**真实字符数**（`section_chars`，非系数）。
2. **用 provider 实测校准** —— `k = 真实 input_tokens ÷ 本次注入总字符`；各段 token = `section_chars × k` → **分项之和自动等于真总数**（顺手消灭原版「估算 vs 实测」两行并存的尴尬）。
3. **诚实标注** —— 总计 = provider 实测；分项 = 「实测文本量 × 校准系数」（比例真实、总量真实）。

**分段名用户化**：`System 模板 / _env 块 / 工具 Schemas / KG 节点` → **规则底座 / lock / 知识记忆 / 历史观测 / 最近对话 / 工具能力 / 当前提问**（**7 段**；`lock` = base+stage 组合后的 core lock，§15.3 起注入，紧随 `规则底座`）。

**可选增强**：ASCII / CJK 分脚本系数（英文段落与中文段落的 char→token 比不同）。

**待定输入**：模型 `context_window` 来源 —— config 里每模型标注 or provider 元数据，**并入需求②（多组 API 配置）一起定**。

### 8.6 常驻块形态（示意，细节可调）

```
┌─ 上下文 ─────────────────┐
│ ▰▰▰▰▰▰▱▱▱▱ 41%   充裕    │
│ 52.0K / 128K             │
│ 规则底座   ██    8.2K  6%  │
│ 知识记忆   █     5.1K  4%  │
│ 历史观测   ███   9.0K  7%  │
│ 最近对话   █████ 22.4K 17% │  ← 谁在膨胀，一眼定位
│ 工具能力   ██    4.8K  4%  │
│ 当前提问   █     2.5K  2%  │
│ 实测输入 52.0K / 128K 41%  │  ← provider 真值
└───────────────────────────┘
```

**键盘导航**（无鼠标）：Tab / 方向键在侧栏各区块与上下文分段间切换焦点，Enter 展开所选分段明细；常驻块本身只读、不响应鼠标。

### 8.7 侧栏高度分配（已定案 ✅）

侧栏需容纳 5 组内容（顶部状态 / 本轮处理 / 功能导航 / 上下文条② / 小白板 / 底部 Token），高度是真约束。

**原则：锚点固定 + 弹性区按优先级降级**

- **固定锚点**：顶部状态行（永远在）、底部 Token + 快捷键（永远在）。
- **弹性区优先级**：上下文条②（**完整常驻**，已拍板保护）≥ 小白板（**≤1/3**，已拍板保护）> 功能导航 > 本轮处理。
- **动态展开**：本轮处理空态收 1 行（`● 就绪`），有轮次时才展开为 3 行。

**排布 A —— 40 行终端（宽松，全展开）**

| 区段 | 行 |
|---|---|
| 状态行（版本·时间 + 模型行） | 2 |
| 本轮处理（标题 + 3 阶段） | 4 |
| 本次会话（token / 工具 / 耗时） | 2 |
| 分隔 | 1 |
| 功能导航（标题 + 7 项） | 8 |
| 上下文组成条②（完整常驻） | 9 |
| 小白板（≤1/3） | 10 |
| Token MeterBar | 1 |
| 快捷键提示 | 1 |
| 分隔 / 留白 | 2 |
| **合计** | **40** |

**排布 B —— 24 行终端（紧凑，降级但保两块保护项）**

| 区段 | 行 |
|---|---|
| 状态行（版本·时间·模型 并一行） | 2 |
| 本轮处理（单行动态：`◐ 正在作答 1.2s`） | 1 |
| 上下文组成条②（完整常驻，压缩：条 + 6 段 + 实测） | 8 |
| 小白板（≤1/3） | 8 |
| 功能导航（选中项 + `▸` 展开） | 1 |
| Token + 快捷键 | 1 |
| 分隔 / 留白 | 3 |
| **合计** | **24** |

**降级规则**：只压弹性区（状态行并一行 / 本轮处理并一行 / 功能导航收单行），**绝不动两块拍板保护项**（上下文条完整常驻、小白板 ≤1/3）。低于 24 行（极端）：小白板压到 1/4 高，**不取消常驻**。

**定案 ✅（cli 2026-09-16 19:37）**：24 行下功能导航 = **B1** —— 导航常显选中项 + `▸` 展开。B2（7 行常显）未采纳；可发现性改由「选中项高亮 + Enter 展开 + 纯键盘导航」保证（见 §11）。40 行版照排布 A。

---

## 9. 四项需求与状态

| # | 需求 | 状态 |
|---|------|------|
| ① | 交互与对话区样式（模仿 CC + 原版，computeruse 全程取证） | ✅ **已封版**（布局 / 主题 / 白板落点 / 侧栏分配 / 纯键盘交互，见 §2–§11） |
| ② | 编译后同级固定配置文件读 API 配置，支持多组 provider/model | ✅ **已定稿**（[`api-config.md`](./api-config.md)；含 `context_window` 来源） |
| ③ | 记忆改存项目内 `.memory/` 目录（替代 `.memory/`） | ✅ **已定稿**（[`memory-dir.md`](./memory-dir.md)） |
| ④ | AI harness 定名「灵妙 lingmiao」+ 配套 UX 与文档机制 | ✅ **已定稿**（[`docs-mechanism.md`](./docs-mechanism.md)） |

**推进路线**：需求① 封版 → ②③④ 均定稿 → **四项全部落地并通过 computeruse 端到端确认（`v0.7.0`）** → **TUI 打磨 T1–T15 落地并实拍确认（`v0.8.0`）** → **§12 撤侧栏 → 纯 CC 单栏落地（`v0.9.0`）** → **§13 CC 视觉对标（品牌 mark / hairline / 反色提问条 / 圆角输入框，`v0.10.0`）** → **§14 主题令牌层（P0）+ 活动动词轮换 + done 时间戳（P2）+ 空闲 tips 轮播（P3）**（`theme.rs` 分层令牌 / `app.rs` 去硬编码 / `motion.rs` 动词轮换 + tip 轮播 / CC 式 44 动词）→ **UI 打磨 v2：§12.6 footer 溢出（①-1）+ §12.7 思考折叠（①-3）+ §12.8 输入框带光标行编辑器（②）+ §12.9 CJK 断词/禁则/列表悬挂缩进（③）+ §12.10 工具输出折叠 + 闲置吉祥物（④）+ §12.11 行距/留白/配色（⑤）**（`v0.11.0`）→ **§15 footer 四数（系统/lock/总/其余）+ 去 `/context` + core lock 注入 + `search_memory` 跨层聚合工具**（引擎 / 工具 / TUI 三处）。**Page27 清单 ①-1/①-2/①-3/②/③/④/⑤ 全部 ✅，泛目标「落地」达成。** → **§17 斜杠命令补全菜单**（cli 2026-09-23 验收发现的缺口，`crates/lingmiao-tui`；`/` 实时补全弹窗、`↑↓/Tab/Enter` 全键盘操作）→ **§12.12 对话区配色向 CC 单色收拢**（page40，cli 2026-09-24；活动行去青 / 品牌 mark 红 + 名白 / `●` 去橙绿）。

---

## 10. 待定 / 待拍板

- [x] **侧栏高度分配**：**已定案** —— 40 行照排布 A，24 行功能导航取 **B1**（选中项 + `▸` 展开，见 §8.7）。
- [x] **面板① 三块结构**：照 §7 稿定（无异议）。
- [x] **模型 `context_window` 来源**：**已定** —— 归入需求② `models.json` 每模型 `context_window` 字段。
- [x] **需求②③**：**已定稿**（[`api-config.md`](./api-config.md) / [`memory-dir.md`](./memory-dir.md)）。
- [x] **需求④ 待拍点**：**已定案** —— 命令名 = `lingmiao`；文档载体 = `help.json` 内嵌；命名落地 = 写进方案、实现阶段统一更新；README 已重写（见 [`docs-mechanism.md §5`](./docs-mechanism.md)）。**四项需求全部收口。**

---

## 11. 交互模式（纯键盘 · 无鼠标 · 模仿 CC）

**定案（cli 2026-09-16 19:37）**：全部操作**纯键盘**，不引入鼠标交互，模式完全模仿 CC（Claude Code 本身即纯键盘 TUI）。

- **导航**：方向键 / Tab 在侧栏区块与上下文分段间移动焦点；Enter 进入/展开；Esc 返回。
- **功能导航**：↑↓ 选中、Enter 进入；`▸` 表示该项可展开（承接 §8.7 的 B1）。
- **上下文条②**：常驻块**只读**，焦点进入后 ←→ / ↑↓ 切换分段，Enter 看该段明细 / 裁剪建议（§8.3 展开层）。
- **对话区**：多行编辑器 Enter 发送 / Shift+Enter 换行；翻页 **PageUp / PageDown / Home / End**（应用内翻页，见下方「实现说明」）。
- **剪贴板**：bracketed paste（粘贴事件，`\r\n` 归一为 `\n`，§28 起**真正开启** `EnableBracketedPaste`）内建，另有 `⌃V` / `shift+insert` 直读 OS 剪贴板；文本选取因 TUI 捕获鼠标（§16.3）而**应用内自实现** —— 左/右键拖动选中、`⌃Y`（或**有选中时**的 `⌃C`）显式复制、`Esc` 丢弃选择，零外部依赖（§6 痛点⑤、§24、§28）。
- **快捷键提示**：侧栏最底常驻一行 + 侧栏底部 Token MeterBar（学原版），降低非技术用户的记忆负担。

> **实现说明（2026-09-16，ratatui 落地）**：持久侧栏与「inline viewport（保留终端原生 scrollback）」不可兼得 —— inline viewport 没有固定列，装不下常驻侧栏。故最终取 **全屏 alternate buffer + 两栏**（§2 D1）；翻页改由**应用内**实现：对话区维护 `scroll` 偏移，`PageUp`/`PageDown` 一次 10 行、`Home`/`End` 跳到两端，上翻时在对话区左下角显示「↑ 已上翻 N 行 · ctrl+end 回到底部」定位提示。短终端（< 38 行）自动降级为 **§8.7 排布 B**（状态并一行 / 本轮处理并一行 / 导航收 B1 / 上下文条压缩 8 行），**两块保护项（完整上下文条 + 小白板 ≤1/3）绝不取消**。剪贴板因 Ctrl+C 已绑定「退出」（Q3），改为**内建 bracketed paste**（覆盖 Ctrl+V / 粘贴），文本选取沿用终端原生选择。（⚠ **本节已过时**：侧栏已于 §12 撤除、原生选择已被 §24 应用内拖选取代，`⌃C` 的语义也由 §28 改为「有选中即复制 / 无选中连按两次退出」；本节保留为历史记录。）

> 鼠标（若终端驱动）不作为设计路径；所有能力必须**仅靠键盘可达**。（2026-09-27：滚轮翻页 §16.3 与左/右键拖选 + `⌃Y` 复制 §24 是**加速例外**，都不是唯一路径。）

---

## 12. 布局转向：撤侧栏 → 纯 CC 单栏（2026-09-20 拍板 ✅）

> ⚠ **本节取代 §2 / §3 / §8.1 / §8.6 / §8.7**（D1 两栏、侧栏四段、「完整常驻」上下文块位置、侧栏高度分配）。
> 未提及的其余设计（§4 对话流字形、§5 主题、§6 痛点、§7 三块信息去黑话、§8.5 真计量三步）**继续有效**。

**拍板（cli 2026-09-20）**：
1. **撤侧栏，走纯 CC 单栏** —— 侧栏「没什么大用」，必要内容按 **CC 的 UI 逻辑**并入。
2. **上下文 → footer**，但**不显示百分比**，直接显示**实际 token 数量 + 组成**。
3. **小白板 → footer 摘要**（改称「footer 摘要」）。

### 12.1 新布局（单栏 · 对话区无边框）

```
▟▀▙ 灵妙 lingmiao vX.Y.Z          ← 启动横幅（一次性打进对话历史，会随对话滚走；**无常驻 header**，§12.17）
▜▄▛ deepseek-v4-pro · ~/ai/lingmiao

❯ 帮我看看这个文件
∴ 思考 4s · ctrl+o 展开                                           ← 思考折叠：灰头 + 末尾 ≤4 行实时（§16.1；2026-09-24 无徽标）
● 这个文件……                                                     ← 回复（markdown）
● 读取文件(src/app.rs)  12ms
  ⎿ fn main() { … }
◜ 正在作答… 8s · esc 中断                                          ← 行内瞬时状态（CC 式，无阶段串；列 0 起，§12.18）
◜ {动词} for 12.4s · done 11:29:46                                 ← 尾行（2026-09-24 无 `· N 个工具`，§12.13；列 0 起，§12.18；2026-09-28 §27.3 去 CC 星形）

❯ 输入…（多行，Enter 发送；←→ 移光标、Ctrl+Z 撤销）                ← 输入框
⏎ 发送 · ←→ 移光标 · ⇧⏎ 换行 · ⌃Z 撤销 · / 命令 · esc 取消 · ⌃C 退出  ● 就绪 · 42.8K↓ 4.7K↑ │ ← footer L1
上下文 系统 5.9K · lock 147 · 总 16.7K · 其余 10.7K              │ ← footer L2（四数 · 无 % / 无窗口数字，§15.1）
白板内容首行…                            灵妙 v0.12.0 · deepseek-v4-pro · ~/ai/lingmiao │ ← footer L3（左白板 §16.2 / 右身份 §12.17）
```

> **分行**（§12.9）：对话区长行按 CJK 禁则预折行 —— ASCII 词不被切断、`。，` 不落行首、`（「` 不留行尾；markdown 列表项续行按标记宽度悬挂缩进（不顶格）。反色提问条按显示宽度（CJK=2）补空格到窗宽。
> **留白**（§12.11）：相邻回合块之间补一行空行（同回合内不插），转录有 CC 式呼吸感。

### 12.2 侧栏六块 → CC 归宿

| 侧栏块 | CC 归宿 | 迁移 |
|--------|---------|------|
| ① 状态（品牌·版本·时钟·模型） | 启动横幅（品牌·版本·模型·cwd，§12.17）+ footer L3 右（身份）+ footer 左（状态灯） | ✅ 无损 |
| ② 本轮处理（3 阶段） | 主区行内 `◜ 阶段 · Ns · esc`（已实现）+ 尾行 | ✅ 直接删侧栏块 |
| ③ 本次会话（tokens/tools/耗时） | footer L1 右（`42.8K↓ 4.7K↑`） | ✅ 无损 |
| ④ 功能导航 | **斜杠命令**（`/help /model /memory /board …`）+ 中文说明菜单 | ✅ 等价（`/context` 于 §15.2 移除） |
| ⑤ 上下文组成条 | footer L2（token 数 + 组成，**无 %**） | ⚠ 降级（非整块） |
| ⑥ 小白板 | footer L3（**当前页内容首行**，灰字、无标签；空则不显示，§16.2）；`/board` 看全文 | ⚠ 降级（一行） |

### 12.3 footer 设计（3 行）

- **L1**：快捷键提示（左） + 状态灯 · 会话 token（右）。提示行按可用宽度**逐级丢弃最不重要分段**（§14.3 遗留），当前全形 = `⏎ 发送 · ←→ 移光标 · ⇧⏎ 换行 · ⌃Z 撤销 · / 命令 · esc 取消 · ⌃C 退出`（`⌃C 退出` / `⏎ 发送` / `esc 取消` 优先级最高，窄终端先丢 `⌃Z` / `←→` / `⇧⏎` / `/ 命令`）。
- **L2**：~~`上下文 <实际tokens> · <段名 各段tokens>…`~~ **已被 §15.1 取代** → ` 上下文 系统 <t> · lock <t> · 总 <t> · 其余 <t>`（四数）。**不出现 `%`、也不出现窗口数字**（`/128.0K` 已去，见 §12.6）；越阈值仍以**颜色**作信号（承 §8.4「颜色=信号」）。按可用宽度自适应截断，绝不溢出 footer。
- **L3**：~~`小白板 ▸ <当前页标题> · <摘要>`~~ **已被 §16.2 取代** → **白板当前页内容的首个非空行**（`whiteboard_suggestion()`），中性灰、**无 `小白板 ▸` 标签**、按宽度截断；空页则该行为空（「如有则显示，没有则不显示」）；`/board` 看全文。**（2026-09-24 §12.17：本行**右端再常驻右对齐** `灵妙 vX.Y.Z · <model> · <cwd>` 身份，取代原顶部 header。）**

### 12.4 连带影响

- **导航**：侧栏 ↑↓ 导航取消（`NAV_ITEMS`/`nav`/`activate_nav` 删除）；功能走斜杠命令 + 补全菜单（补全菜单于 **§17 落地**）。↑↓ 改为历史/输入用（补全面板打开时 ↑↓ 先走面板）。
- **可发现性**：非技术用户的引导改由「斜杠菜单带中文说明」承担（需求① 既有条款）。
- **上下文分段展开**：原侧栏 Tab/←→/Enter 焦点改为命令式 —— ~~`/context` 列出各段明细 + 裁剪建议~~（`/context` 已于 **§15.2 移除**，其内容与旧 L2 重复且带误导窗口数字）。
- **文件改动（`crates/lingmiao-tui/src/app.rs`）**：删 `render_sidebar`/`render_sidebar_compact`/`render_meter`/`render_keys`/`NAV_ITEMS`；新增 `render_header`/`render_footer`；`render()` 改单栏；`context_segment_lines` 改为 footer 内联（去 %）。受影响测试（`arrow_keys_navigate_sidebar_*` / `enter_on_nav_*` / `render_two_columns_*` / `compact_layout_*` / `tab_focuses_context_*`）需重写。

**待确认（1 处）**：「组成占比」的渲染形态 —— 暂定 = **各段真实 token 数**（token 数本身即占比信息）；若想额外画相对条（`▰▱`），说一声即可。

### 12.5 落地状态（✅ 已实现，`crates/lingmiao-tui/src/app.rs`）

- `render()` 改**单栏**：`[header(1), body(Min), footer(3)]`；删 `render_sidebar`/`render_sidebar_compact`/`render_meter`/`render_keys`/`titled`/`NAV_ITEMS`/`nav`/`activate_nav`/`ctx_focus`/`chrono_hms`/`pad_display`。
- 新增 `render_header`（品牌·版本·模型·cwd）、`render_body` / `render_conversation`（**对话区无边框**）/ `render_input`（`❯ ` 前缀 + 多行）、`render_footer`（L1 快捷键+状态·token / L2 上下文 token **无 %** / L3 小白板摘要）。
- 导航改**斜杠命令**：`/help /context /board /memory /session /tools /model /clear /quit`；~~`/context` 打印各段明细 + 裁剪建议~~（`/context` 于 **§15.2 移除**）；**↑↓ 翻输入历史**（draft 暂存 / 恢复）。
- `本轮处理` 三阶段 → 主区行内 `◜ {动词}… Ns · esc 中断`（真实状态驱动）；`/session` 列各阶段真实耗时。（2026-09-24 按 CC 去掉阶段串，见 §12.13。）
- 测试重写：`up_down_browse_input_history` / `context_command_lists_all_sections_with_advice` / `render_single_column_into_a_test_backend` / `compact_layout_renders_on_a_short_terminal` / ~~`wb_summary_parses_title_and_first_line`~~（→ §16.2 `whiteboard_suggestion_is_the_first_content_line`）；`cargo test --workspace` 全绿。

### 12.6 footer L2 去窗口数字 + 溢出自适应（2026-09-21 落地 ✅ · UI 打磨 v2 ①-1）

cli 2026-09-21 拍板 **footer 去掉窗口数字**：L2 原形 `上下文 7.2K/128.0K · …` 里的 `128.0K`（取自 `models.json` 的 `context_window`）被读成「快满了没」的误导性仪表，而真正该看的是**已消耗的真实 token + 它的组成**；窗口数字（连同 `%` / 阈值视图）~~保留在 `/context` 里按需查~~（`/context` 于 **§15.2 移除**，窗口口径随之下线）。

- **L2 新形**：` 上下文 <实际tokens> · <段名 各段tokens>…` —— 只留真实 token + 组成（无 `%`、无窗口）；越阈值仍以**颜色**作信号（§8.4）。
- **溢出自适应**（cli P0「footer 溢出」）：L2 / L3 均按 `area.width` 拟合 —— L2 逐段追加，放不下就**丢掉尾部段并补 `…`**（始终预留省略号位），极窄时再兜底 `truncate`；L3 优先保标题、摘要放不下则补 `· …`、标题超宽则截断。整行**绝不超出窗宽**。
- **落地**：`crates/lingmiao-tui/src/app.rs` 的 `footer_context_line(width)` / `footer_whiteboard_line(width)` 加宽度参数；`render_footer` 传 `area.width`。
- **测试**：`footer_context_line_sheds_sections_to_fit`（宽 = 全段无 `…` / 无窗口；40 列 = 保总 token 且以 `…` 收尾且 ≤ 40；4 列 = 最小宽度不溢出）；更新 `context_usage_event_populates_segments`（不再含 `/`）、`render_single_column_into_a_test_backend`（断言不含 `128.0K`）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 162 passed / 0 failed；computeruse :99 实拍（见 obs / 小白板）。

### 12.7 思考折叠（灰头 + 实时尾部几行 + `ctrl+o` 展开）（2026-09-21 落地 ✅ · UI 打磨 v2 ①-3 · §16.1 修订 · 2026-09-24 CC 对齐）

cli 实证（page27）**CC 思考 = 灰 / 正体 / 正常字距**，且**默认折叠为一行摘要**；灵妙的旧形（青 / 斜体 / 字距怪）与 CC 相去甚远，故对标 CC 重做。

- **默认折叠** = **灰头 + 实时尾部**（§16.1 修订，2026-09-21）：一行灰头 `∴ 思考 {N}s · ctrl+o 展开` —— 中性灰 `TEXT_MUTED`、**正体无斜体**、正常字距（2026-09-24 按 CC 去掉阶段徽标，见 §12.13）。**（2026-09-24 §12.16 订正：标题 `∴ 思考 {N}s` 改白 `TEXT`，折叠提示与推理正文仍 `TEXT_MUTED` 灰。）** 其下再显**末尾 ≤8 行实时推理**（`THINKING_TAIL_LINES`：4 →（2026-09-24）→ 6 →（2026-09-28 §27.2）→ **8**，灰、正体，随流式更新），避免「全收起来看着像卡住」。CC 同形为 `∴ Thought for {N}s (ctrl+o to expand)`。
- **`ctrl+o` 展开**：切换折叠态后显示**完整 reasoning** —— 与折叠态同为**灰正体**（`TEXT_MUTED`、无斜体）。cli 2026-09-21 已定「青色的不好看，字体也不对」，2026-09-24 再次指出对话区与 CC 差距大；故**去青、去斜体**，向 CC 看齐（旧形为静青 `THINKING` 斜体，属灵妙自有样式、非 CC 形态，现弃用）。
- **关键词高亮**（cli 2026-09-24：「CC 支持在 thinking 里高亮关键词，咱们也要有」）：思考正文里的**代码 / API 关键词**提亮到**柔蓝** `THINKING_KW`（= CC 实测 `#B1B9F9`，**无加粗**，§12.19），其余保持中性灰。判定规则（纯函数 `thinking_keyword_runs` + `is_keyword_token`）：把文本切成「标识符式」连续段（ASCII 词字符 + `_ . / :` 与反引号），某段算关键词当且仅当含 `_` / `/` / `::`、形如 `name.ext` 文件名（点后 ≥2 字符）、或被一对反引号包裹；纯散文与标点**不**成为关键词（「少即是多」，不滥用强调）。折叠尾部与展开全文**同规则**，且拼接后与原文**逐字相等**。
- **`ctrl+o` 再按收回**；折叠态切换对**整个对话区已提交的思考块 + 在飞的实时思考**同时生效。
- **实现**：`App.thinking_expanded: bool`（默认 `false`）+ `thinking_started: Instant`（算摘要里的 `N`s）；`scrollback: Vec<Item>` 新增 `Item::Thinking { stage, text, secs }` —— 思考块**结构化存储**（不预渲染成行），渲染时按折叠态现渲染，故与工具卡的**转录顺序不变**；纯函数 `thinking_lines()` 供折叠 / 展开两态共用。
- **订正**：`theme.rs` 旧注释「CC renders thinking as italic cyan」与 page27 实拍矛盾，已改为「静青 = **展开**样式，非 CC 折叠样式」；§4 表格 / §14 同步订正（外部参照保真 —— 参照行为须实拍核对，不据旧注释断言）。
- **测试**：~~`thinking_collapsed_by_default`（折叠 = 一行、含 `ctrl+o` 提示、含徽标、**不含全文**）~~ → **§16.1 起** `thinking_collapsed_shows_header_and_live_tail`（折叠 = 灰头 + 尾部数行、含 `ctrl+o` 提示、长推理的**头部**被折掉）、`ctrl_o_toggles_thinking_expansion`（`ctrl+o` 展开显全文、再按收回）；更新 `reasoning_streams_into_the_thinking_block` / ~~`b_stage_reasoning_and_answer_carry_stage_badges`~~（§12.13 更名 `…are_shown_without_badges`）以适配折叠默认。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 164 passed / 0 failed；computeruse :99 实拍（折叠一行灰 + `ctrl+o` 展开全文）。
- **2026-09-24 CC 对齐 + 关键词高亮**：`thinking_lines()` 展开态由静青斜体改**灰正体**；`theme.rs` 新增 `THINKING_KW`（**定值 `#B1B9F9` 柔蓝**，§12.19 订正：原为终端默认前景白 + 加粗）；`app.rs` 新增纯函数 `thinking_keyword_runs` / `is_keyword_token` / `thinking_text_spans`，折叠尾部与展开全文共用。新测 `is_keyword_token_classifies_code_tokens` / `thinking_keyword_runs_flags_code_tokens_and_preserves_text`（关键词命中 + 拼接还原原文）/ `thinking_block_is_grey_upright_with_keyword_emphasis`（无青 / 无斜体 / 有关键词提亮 span）。验证：`cargo test --workspace` 300 passed / 0 failed；computeruse :99 实拍（展开思考为灰正体、`search_memory` / `app.rs` 等关键词提亮、无青色 / 斜体）。<br>**已落地**（§12.12 + §12.13，2026-09-24）：活动行青→灰；`●` 回复 / 工具 `●` / 品牌 mark 收成 CC 单色（红 logo + 白字）；工具结果刷 JSON→**折一行**；阶段徽标**按 CC 去除**；活动行 / 尾行**去阶段串与 `· N 个工具`**。

### 12.8 输入框带光标行编辑器（2026-09-21 落地 ✅ · UI 打磨 v2 ②）

cli 2026-09-21 追加：**「输入框比 CC 差得远，要多行 / 全键盘 / 复制粘贴好用」**。旧输入框是单 `String`，光标只能停在**末尾**（`←→` 完全不处理）。本节把它做成一个**带光标的多行行编辑器**（对标 CC 的行编辑体验）。

- **数据模型**：新增 `crates/lingmiao-tui/src/editor.rs`（与 `motion.rs` 同思路，纯逻辑可单测），`App` 持有一个 `Editor`（`text` + `cursor` + `undo`）。光标是 **字符索引**（非字节）—— CJK / emoji 编辑永不落在码点中间 panic；缓冲量级小，不引入 rope。
- **键位**（全部在 `on_key` 里、`KeyCode::Char` 兜底**之前** match）：
  - `←` / `→` 移光标一字符；`Alt+←/→` 或 `Ctrl+←/→` 按词移动。
  - `Home` / `End` 到**当前行**首/尾；`Ctrl+A` / `Ctrl+E` 同义（emacs）。
  - `Ctrl+U` 删到行首 · `Ctrl+K` 删到行尾 · `Ctrl+W` 删前一个词 · `Ctrl+Z` 撤销。
  - `Delete` 删光标处字符（`cursor` 不动）；`Backspace` 删光标前字符。
  - `↑` / `↓`：**多行时**在行间移动（**保持列偏好**）；**单行**时维持原「翻输入历史」（CC 亦如此）。
- **翻页改绑**：`Home` / `End` 让位给「行首/行尾」后，对话区翻页到两端改绑 **`Ctrl+Home` / `Ctrl+End`**（`PageUp` / `PageDown` 不变）。
- **撤销栈**：`undo: Vec<(String, usize)>`（text + cursor 快照），每次**变更性编辑前** push，`cap = 100`；`Ctrl+Z` pop 恢复。历史召回 / 草稿恢复（`set_text`）算导航，不入栈。
- **渲染光标**：`render_input` 按 `cursor` 算 `(行号, 列)` —— 行号 = 光标前 `\n` 数，列 = 该行前缀的 **display width**（CJK 按 2 列）；纯函数 `cursor_row_col()` 供单测。续行两空格缩进 / 首行 `❯ ` 均宽 2，故列偏移恒 `+2`。
- **不冲突**：既有 `Ctrl+C`（§28 改为：有选中即复制 / 无选中连按两次退出）/ `Ctrl+O`（思考折叠）/ `Ctrl+J`（换行）/ `Esc`（取消）保持。
- **/help keys 行**同步补 `←→ 移光标 · Home/End 行首尾 · ⌃A/⌃E · ⌃←/⌃→ · ⌃U/⌃K · ⌃W · ⌃Z`。
- **测试**（`editor.rs` 11 + `app.rs` 8）：`left_right_move_cursor_and_insert_mid_line`（`ab` 中间插成 `aXb`）/ `backspace_deletes_before_cursor` / `home_end_move_within_line` / `ctrl_u_kills_to_line_start` / `ctrl_w_deletes_word` / `undo_restores_previous_text` / `up_down_moves_within_multiline_then_browses_history` / `input_cursor_is_placed_mid_line`（复用 `TestBackend` 断言光标移列）；`editor.rs` 另有 `cjk_edits_stay_on_char_boundaries` / `up_down_keep_the_column_preference` / `word_movement_steps_by_words` 等纯函数测试。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿；computeruse :99 实拍（多行输入 → `←→` 移光标 → 中间插入 → `Ctrl+U`/`Ctrl+Z` → 多行 `↑↓`）。

### 12.9 CJK 断词 / 标点禁则 / 列表悬挂缩进（2026-09-21 落地 ✅ · UI 打磨 v2 ③）

v0.10.0 实拍暴露的渲染分行问题（cli 清单 P0）：旧 `wrap_text()` 纯按 `char_width` **逐字符**断行 —— 长英文词 / URL 会被从中间切断，且无 CJK 禁则（`。，、` 会落到行首）；对话区又直接交给 ratatui `Paragraph::wrap()` 逐字符重断，同病。本节把分行收回自己手里。

- **新模块 `crates/lingmiao-tui/src/wrap.rs`**（与 `editor.rs`/`motion.rs` 同思路，纯逻辑、零新依赖）：`pub fn wrap_cjk(text, width) -> Vec<String>`。
  - **token 化**：一段 **ASCII 非空白连续串 = 一个不可切词**；其余每字符（CJK / 全角标点 / emoji）各自成 token。断点优先落在空格；只有**单词本身 > width** 时才硬切（`split_at_width`）。每段 `display_width ≤ width`。
  - **CJK 禁则（避头尾）**：两份字符集 `CLOSING`（行首禁则 `。，、；：！？）」』】》〉”’…—～%）]}｝>,.!?;:`）与 `OPENING`（行尾禁则 `（「『【《〈“‘([{｛<`）。当断点会制造违例时**回溯一个 token**（closing 退到下行的首字之前、opening 被带到下一行），故标点始终保持邻字、`closing` 绝不独起一行。极端窄行（无法两全）以保证**不超宽**为先。
  - **悬挂缩进**：`wrap_cjk_hanging(text, width, hanging)` 让列表项续行缩进 `hanging` 列、与条目文本左对齐（§ 列表分行）。
- **替换全部旧调用点**：反色提问条 / 工具卡 / 思考块由 `wrap_text` 改 `wrap_cjk`；**同时把反色条文本预算从 `width-2` 修为 `width-3`**（` ❯ ` / 续行缩进均 3 列），使条右缘精确落在 `width`（cli 曾抱怨右缘错位）。
- **对话区预折行**：`render_conversation` 先用 `wrap_line()`（内部调 `wrap_cjk` / 列表项调 `wrap_cjk_hanging`）把每行展成不超宽的 `Line`，再 `Paragraph::new(...)` 渲染并**去掉 ratatui 的 `.wrap()`** —— 避免它二次逐字符重断、破坏禁则。已适配的分行行（反色条 / 工具卡 / 思考块）本就 ≤ 窗宽，原样透传以保留其 span 样式；仅**超宽行**重发（复用该行主 span 样式）。
- **列表悬挂缩进**：`list_hanging()` 识别 `^(\s*)([-*+]|\d+\.)\s` 前缀，计算 `hanging = display_width(前缀)`，无标记续行顶格。
- **测试**（`wrap.rs` 6 + `app.rs` 3）：`does_not_split_ascii_word` / `closing_punct_never_starts_a_line` / `opening_punct_never_ends_a_line` / `long_unbreakable_word_hard_breaks` / `every_segment_fits_width` / `list_continuation_is_indented`（纯函数）；`wrap_line_keeps_cjk_width_exact_for_the_reverse_bar`（条右缘对齐）/ `list_hanging_detects_markers` / `wrap_line_indents_list_continuation`。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿；computeruse :99 实拍（中英混排 + 句读 + markdown 列表的长回复逐行核对：英文词不裂、`。，` 不落行首、列表续行缩进对齐）。

### 12.10 工具输出折叠 + 闲置吉祥物（2026-09-21 落地 ✅ · UI 打磨 v2 ④ P1）

cli 2026-09-21 打磨（page27）：长工具输出会**刷屏**，CC 是把它**折叠成一行摘要 + `ctrl+o` 展开**；且空闲态缺一点「有生命感」（CC 空闲有 `✻` idle 动画）。本节对标落地两块 —— **折叠（P1-a）** 与 **吉祥物（P1-b）**。

#### P1-a 工具输出折叠

- **现状问题**：`push_tool_card` 把工具结果**全量**塞进 scrollback；一次 `read_file`/`grep`/`bash` 的大结果会占据满屏，把前后回合挤出视野。
- **结构化存储**：`scrollback: Vec<Item>` 新增 `Item::ToolOutput { header, body }`（与 §12.7 `Item::Thinking` 同思路）—— `● name(args)` 头**整行保留**，`⎿ result` body 存**已折行**的 `Vec<Line>`（复用 §12.9 的 `wrap_cjk`，不二次重断）。转录顺序不变。
- **单常量折叠**：`TOOL_OUTPUT_COLLAPSE_LINES = 3`（2026-09-24 由 8 收窄，§12.13；2026-09-28 §27.2 确认 3，与 head 同步）。body 超过阈值且**未展开**时，只显**首 `TOOL_OUTPUT_HEAD_LINES = 3` 行** + 一行 `∴ … 还有 {M} 行 · ctrl+o 展开`（`TEXT_MUTED`、正体，与思考折叠同款，对齐在 `⎿` body 之下）；未超阈值则**原样全显**（无提示）。对齐 CC 的一行 `⎿ …` 摘要。
- **`ctrl+o` 复用**：既有 `ctrl+o`（§12.7）扩展为**全局折叠开关** —— 一键同时展开/收回**思考块**与**工具输出**；`App.output_expanded: bool`（默认 `false`）与 `thinking_expanded` 同步翻转。这样「一个键看全部隐藏细节」。
- **统一渲染入口**：`item_lines(&self, it, width)` 是**唯一**决定折叠态外观的地方 —— `take_scrollback`（loop/测试）与 `render_conversation` 共用，两者永不发散。
- **`RESULT_PREVIEW_CHARS` 400 → 2000**（`crates/lingmiao-engine/src/stage_agent.rs`）：`Event::ToolCalled.result_preview` 是 display-only（模型仍收全量），但 400 字符的预览**永远到不了 8 行折叠阈值**，会让折叠功能**在真实管线里死掉**。放大到 2000 让真实 `read_file`/`grep` 结果**真的会折叠**（+ 单测 `result_preview_cap_exceeds_the_fold_threshold`）。

#### P1-b 闲置吉祥物

- **形状**：灵妙的微型吉祥物 = 3 glyph 笑脸，**正体、正常字距**（cli 明确反感「字距怪」的斜体 / CJK 宽字形）。`mascot(busy)`：空闲 `•ᴗ•`（微笑）/ 回合在飞 `•-•`（打盹）。
- **位置**：输入框**右缘**（承接 §14.4 tip 占位位）；`push_mascot(&mut spans, inner_width, busy)` 按 `display_width` 右对齐、`ACCENT`（品牌橙）—— 状态一换形态，给空框一点「sign of life」。
- **让位规则**（纯 priority）：有 tip 时贴 tip 行右缘；回合在飞且缓冲空时换「打盹」形态；用户开始输入则吉祥物让位给文本。`push_mascot` 纯函数（作用于已建 `spans`），可在无终端下单测。
- **`ctrl+o` 提示同步**：`/help` keys 行 `⌃O 展开思考` → `⌃O 展开折叠`（现已含工具输出）。

- **测试**：`long_tool_output_is_collapsed`（30 行结果 → 只显 8 行 body + 含 `ctrl+o 展开` + 含 `还有` 计数 + 尾部 `row30` 隐藏）、`ctrl_o_expands_collapsed_tool_output`（默认折叠 → `ctrl+o` 显全量 → 再按收回）、`short_tool_output_is_not_folded`（阈值内全显、无提示）、`idle_state_shows_mascot`（空闲帧含吉祥物 glyph）+ `result_preview_cap_exceeds_the_fold_threshold`（engine）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 198 passed / 0 failed；computeruse :99 实拍（`⎿` 显 8 行 + `∴ … 还有 50 行 · ctrl+o 展开`，`ctrl+o` 展开全量；吉祥物 `•ᴗ•` 橙落输入框右缘）。

### 12.11 行距 / 留白 / 配色（2026-09-21 落地 ✅ · UI 打磨 v2 ⑤ P2）

Page27 清单末项（CC 对齐）：转录缺 CC 那种**呼吸感**（回合之间没有留白，挤成一坨），且主题中灰偏暗（折叠提示 / `⎿` 结果发糊）。本节两块 —— **留白（P2-a）** 与 **配色（P2-b）**。

#### P2-a 回合间留白

> ⚠ **已被 §12.14 取代（2026-09-24）**：旧规则 = 仅**相邻「回合块」之间**补一行空行、块内部不插。后经 cli 2026-09-24 ③「控件间隔1行」修订为**每个 block 前都补一行**（见 §12.14）。下列 `Item::is_turn_start()` / 纯函数 `blank_line_between_turns()` 已随该修订**删除**，本段仅存历史。

- **规则**：**相邻「回合块」之间**统一补**一行空行**。一个回合块 = 用户反色条 → 思考 → 回复 → 工具卡（+`⎿` 结果）→ 总结行；块**内部不插空行**（工具卡与其 `⎿` 结果之间也不空），只有块与块之间有一条空行。cli 明确要求转录有 CC 的呼吸感。
- **结构**：`Item` 新增 `UserTurn { lines }` 变体（用户反色条从裸 `Item::Line` 升级为独立变体），于是转录能**识别回合起点**。`Item::is_turn_start()` 只对用户回合为真。
- **纯函数** `blank_line_between_turns(starts: &[bool]) -> Vec<bool>`：输入每项「是否回合起点」的布尔表，输出每项「是否在其前补空行」——**首个回合不加**（转录顶端无前导空行），其后每个回合起点加一行，其余全不加。纯逻辑、可单测。
- **统一展平入口** `transcript_lines(&items)`（`take_scrollback` / `history_lines` 共用）：按 `blank_line_between_turns` 决定的位置插空行，再逐项 `item_lines` 展平 —— 测试 / loop 与 `render_conversation` 看到的布局**完全一致**。
- **收束旧散点**：`push_answer` / `push_tail` 原先各自在末尾补一个空行（造成块内空行 + 块末双重空行），已移除；留白现由**唯一**的回合分隔器负责。

#### P2-b 配色提亮

- **问题**：中灰令牌 `DarkGray`（ANSI bright-black）在深色终端上偏暗 —— 折叠提示 `ctrl+o 展开` 与 `⎿` 结果「发糊」，发丝线/圆角框也近乎隐形。
- **改动**：`crates/lingmiao-tui/src/theme.rs` 三个结构/中灰令牌 `DarkGray → Gray`（提亮一档）—— `RULE`（发丝线）、`BORDER`（圆角框）、`TEXT_MUTED`（次要文字 / `⎿` 结果 / 折叠提示）。对齐 CC 浅色主题的 border `#7a6c52` / line `#8a7f6d`（本为**中灰**，非 bright-black）。
- **不动**：品牌强调色 `ACCENT`（CC 固定橙）与已定案的语义信号 `RUNNING` / `WARN` / `SUCCESS` / `ERROR` **原值不改**。`TEXT`（Reset，最亮）保持不变，故「正文亮 / chrome 中灰」的层次依旧。
- **守卫**：仍受 §14-P0 `header_uses_theme_tokens` 约束（widget 层禁裸 `Color::`，帧内每个前景色须在令牌白名单内）。

- **测试**：`blank_line_between_turns_places_one_separator_per_block`（纯函数：`[T,-,-,T,-,T] → [F,F,F,T,F,T]`；前导非回目项不加前导空行；无回合则无分隔）、`turns_are_separated_by_one_blank_line`（端到端：两回合 → **恰好一行**空行，且落在第二回合反色条之前，块内无空行）、`muted_and_structural_tokens_are_lightened_but_accents_kept`（`RULE/BORDER/TEXT_MUTED == Gray`；`ACCENT/RUNNING/WARN/SUCCESS/ERROR` 原值）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 201 passed / 0 failed；computeruse :99 实拍（回合间留白可见；发丝线/圆角框/折叠提示不再发糊）。

### 12.12 对话区配色向 CC 单色收拢（2026-09-24 落地 ✅ · page40）

cli 2026-09-24：「对话历史栏目里的颜色和排版跟 CC 还有很大差距」。`:99` 同屏实拍 CC 2.1.278 ↔ 灵妙 v0.12.0（各真发一轮，`/tmp/computerse/sidebyside-1790214788058.png`）归因 = **信息过量 + 强调色滥用**：CC 是「少即是多」（红 logo + 白字 + 灰次要，2–3 色），灵妙却橙/青/绿齐飞。

本轮（可逆配色项，离线自拍板；依据 = cli 明示「青→灰 / `●` 回归单色 / 品牌区改红 logo」+ 上述实拍）落地：

| 元素 | 旧 | 新 | 令牌 |
|------|----|----|------|
| 活动行 `◜ {动词}… 已 Ns` | 青 | **灰** | `THINKING` → `TEXT_MUTED` |
| 品牌 mark `▟▀▙` | 橙 | **红** | `ACCENT` → `LOGO`（新令牌 = `Red`） |
| 品牌名 `灵妙 lingmiao` | 青 | **白** | `INFO` → `TEXT` |
| 回复 `●` bullet | 橙 | **白** | `ACCENT` → `TEXT` |
| 工具 `●`（成功） | 绿 | **白** | `SUCCESS` → `TEXT`；失败仍 `ERROR`（红=信号，§8.4） |
| ↳ **订正（2026-09-24）** | 上格「白」 | **绿** | `TEXT` → `SUCCESS`（§12.14） |

> **⚠ 订正**：上表把「工具 `●` 绿→白」算作 CC 单色收拢的一环，属**误判** —— CC 的「克制」= **每种控件各有语义色**，并非通盘单色。cli 2026-09-24 `:99` 同屏实测 CC 2.1.278 工具 `●` 本就是**纯绿 RGB(0,205,0)**，故已按 **§12.14** 改回 `SUCCESS` 绿（失败仍 `ERROR` 红=信号）。

- **强调色收敛**：`ACCENT` 由「含回复 bullet + 品牌 mark」收窄为**仅交互性 affordance**（`❯` 提示符、补全面板、markdown 标题）。
- **令牌清理**：`THINKING` / `INFO` 收拢后无用户，**删除**；新增 `LOGO`（`Red`，独立于 `ERROR`，免品牌色与错误信号语义混淆）。
- **不动 `RUNNING`**：仍用于**状态**（footer 状态行 + 运行中工具 spinner，§14-P1 语义分离）；本轮的**转录活动行**是 CC 那种安静的灰进度行，与「状态信号」相区分（`theme.rs` 已注明此裁定，避免被误读为违反 §14-P2）。
- **守卫更新**：`header_uses_theme_tokens` 白名单（去 `THINKING`/`INFO`、加 `LOGO`，断言 brand mark 画 `LOGO`）；`thinking_block_is_grey_upright_with_keyword_emphasis` 改为断言思考 span 仅 `TEXT_MUTED` / `THINKING_KW`。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **300 passed / 0 failed**；computeruse :99 实拍（红 logo + 白名 + 白 `●` + 工具 `●` 白 + 活动行灰，与 CC 同屏观感一致）。
- **✅ 后续落地（§12.13，2026-09-24）**：page40 其余项 —— 工具结果刷 JSON→**折一行**、**阶段徽标按 CC 去除**、活动行 / 尾行收成 CC 形（去阶段串与 `· N 个工具`）。

### 12.13 对话区排版按 CC 收拢（2026-09-24 落地 ✅ · 继 §12.12）

cli 2026-09-24：「**先按 CC 的样式做，做好后我再决策增加 lingmiao 自有的元素**」= 承接 §12.12 配色收拢，把剩下的**信息量差异**（灵妙比 CC 多的东西）全部按 CC 砍掉，灵妙自有元素留待后续再决策加回。

`:99` 同屏实拍（CC 2.1.278 ↔ 灵妙）归因 = **信息过量**：CC 是「少即是多」，灵妙却在装饰上做加法。本轮落地（均为可逆的呈现层改动）：

| 项 | 旧（灵妙自有） | 新（按 CC） | 实现 |
|----|----------------|-------------|------|
| 工具结果 | 折**前 8 行**（`COLLAPSE_LINES=8`） | **折 1 行**摘要（`COLLAPSE_LINES=3` / `HEAD_LINES=1`）+ `∴ … 还有 M 行 · ctrl+o 展开` | `item_lines` / 新常量 `TOOL_OUTPUT_HEAD_LINES` |
| 阶段徽标 `[检索记忆]`/`[正在作答]`/`[沉淀整理]` | 挂 **4 处**（工具卡 / 思考 / 回复 / 活动+运行工具） | **全去**（CC 不挂任何徽标） | 渲染点去 `stage_badge()` 调用；`stage_badge()` 保留 `#[allow(dead_code)]` 以备加回 |
| 活动行 | `◜ {动词}… 已 Ns · ✓检索记忆 · ✓正在作答 · ◐沉淀整理 · esc 中断` | `◜ {动词}… Ns · esc 中断` | `render_conversation` 活动行 |
| 尾行 | `◜ {动词} for Ns · **{N} 个工具** · done HH:MM:SS` | `◜ {动词} for Ns · done HH:MM:SS` | `push_tail()`（`turn_tools` 计数保留 `#[allow(dead_code)]`） |
| 总结调试行 | `· 总结 type=… audit=… quality=…` | **删除**（CC 无） | `on_turn_done` |

- **保留（属 cli 明确功能需求，非装饰）**：思考折叠的**尾部实时行**（`THINKING_TAIL_LINES`，§16.1 起 4 → 2026-09-28 §27.2 起 **8** —— cli 2026-09-21 明示「不能完全收起来，不然等待很焦急」），与 CC 的纯 1 行不同；若 cli 要纯 CC 单行，改常量即可。
- **可加回**：`stage_badge()` / `stream_stage` / `RunningTool.stage` / `turn_tools` 均保留（标 `#[allow(dead_code)]` + 注释），后续决策「增加灵妙自有元素」时可一处复用的最小改动接回。
- **测试**：`long_tool_output_is_collapsed`（只显 `HEAD_LINES` 行）；`short_tool_output_is_not_folded`（阈值内全显）；`tool_call_event_adds_card_and_counts` / `reasoning_streams_into_the_thinking_block` / `thinking_collapsed_shows_header_and_live_tail` 改断言 **无徽标**；`pipeline_stage_tool_cards_show_their_stage` → `…are_shown_without_badges`；`b_stage_reasoning_and_answer_carry_stage_badges` → `…are_shown_without_badges`；`turn_tail_line_carries_elapsed_and_tool_count` → `turn_tail_line_is_cc_shaped`（无「个工具」）；`done_commits_answer_and_summary`（无 `audit=…` 调试行）；`activity_line_shows_while_busy`（无阶段串）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **300 passed / 0 failed**；computeruse :99 真进程实拍（重启为新二进制后发一轮 bash `ls -la`）——工具卡 `● 运行命令(bash 命令执行 shell ls)`（**无阶段徽标**）、结果 `⎿ … 还有 71 行 · ctrl+o 展开`、活动行 `✻ Creating… 81s · esc 中断`、尾行 `✻ Hatching for 195.0s · done 11:29:46`（无「个工具」、无「· 总结」调试行）。

### 12.14 CC 区块间隔 1 行（控件间隔1行）（2026-09-24 落地 ✅ · 取代 §12.11 P2-a）

cli 2026-09-24 ③「**控件间隔1行**」：CC 的转录里**每个 block 前**都留一行空白（用户反色条 / 思考 / 工具卡 / 回复 / 尾行 / 命令输出 / 错误各自成块），而不只是「回合之间」。本轮把 §12.11 P2-a 的『仅回合间』规则推广为『**每块前恰 1 行**』，并把上一轮 §12.12 收成白色的工具 `●` 按 CC 实测修回绿色。

- **规则**：每个 block 写入转录前，`blank_separator()` **恰好补一行空行**；转录**首块不加**（无前导空行）；相邻两空行**永不叠加**（每块自己加、块内不加）。
- **block 定义**：用户反色条（`Item::UserTurn`）、思考块（`Item::Thinking`）、工具卡（`Item::ToolOutput`）、回复、尾行、命令输出（`push_info`）、错误（`push_error`）—— 各为一块。
- **实现**：新方法 `App::blank_separator()`（`if !scrollback.is_empty() { push 一行空 Line }`），在每个块写入前调用一次；`transcript_lines()` 的展平据此改为**直通 map**（分隔已在 push 时落好），删去旧的 `Item::is_turn_start()` + 纯函数 `blank_line_between_turns()`（§12.11 P2-a 那套）。
- **9 处调用点**：
  1. `push_info` —— 命令输出块（含 `/help` 整段：`push_help` 把多行合成**一次** `push_info`，故只加**一个**前缀而非每行一个）；
  2. `push_error` —— 错误块；
  3. `commit_thinking` —— 思考块；
  4. `push_tool_card` —— 工具卡；
  5. `push_turn`（user） —— 用户反色条；
  6. `push_answer` —— 回复块；
  7. `push_tail` —— 尾行；
  8. `push_help` —— 整段 help 视作**一块**（经第 1 点的 `push_info` 单次调用落地）；
  9. `render_conversation` —— 在飞的**实时**块（实时思考 / 运行中工具卡 / 流式回复 / 活动行）各自在其前补一行，使在飞转录与落定后**读起来一致**。
- **工具 `●` 恢复绿**：`push_tool_card` 成功 `●` 由 §12.12 的 `TEXT`（白）改回 `SUCCESS`（绿）；失败仍 `ERROR`（红=信号，§8.4）。依据 cli 2026-09-24 `:99` 同屏实测 CC 2.1.278 工具 `●` = **RGB(0,205,0)** —— CC 的「克制」是**每种控件各有语义色**，不是通盘单色，故 §12.12 把工具 `●` 收白属**过头的误判**（§12.12 表格已加订正行）。
- **取代 §12.11 P2-a**：旧「相邻回合块之间补一行、块内不插」作废（见 §12.11 P2-a 顶部警示）。
- **测试**：`blank_separator_puts_one_gap_between_blocks`（两块 → `[块, 空, 块]`，首块无前导空行）、`blocks_are_separated_by_one_blank_line`（端到端：无前导 / 尾随空行、无连续双空行、第二回合条前恰一行空）；`long_tool_output_is_collapsed` 等沿用。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **300 passed / 0 failed**；computeruse :99 真进程实拍（用户栏 / 思考 / 工具卡 / 回复 / 尾行**各间隔 1 行**，工具 `●` 为绿）。

### 12.15 工具结果用正文亮色（CC 层次）（2026-09-24 落地 ✅ · 订正 §12.11 P2-b / fe14e4b）

> ⚠ **本节「工具结果 = 最亮 `TEXT`」已由 §12.16 订正**：cli 2026-09-24 后续要求「工具调用返回值 = 比思考亮一档的**中灰**」，非最亮正文色。

cli 2026-09-24：「CC 与 python 原版配色都很有层次，灵妙**思考却发白、输出部分没上色**」。fe14e4b 已把**思考**改回 CC 深灰（`TEXT_MUTED` = `DarkGray` = ANSI 90），但同一提交把「**`⎿` 工具结果**」也一并压成深灰 —— 与 CC 实测不符，把原本该有的**两层层次**（亮数据 / 灰叙述）压平成一层。

- **CC 实测**（page40 :99 同屏实拍 + `cc-toolrun-1790221161498.png` 300% 放大）：CC 2.1.278 的工具结果（`⎿ <stdout>`，如 Bash 的 `ls -la` 输出块）是**最亮的前景色**（近纯白），而**思考**（`∴ The command ran…`）才是灰的 —— 即「**结果 = 被读取的主体数据（亮）**，思考 = 叙述（灰）」，两层分明。fe14e4b 把两者一起压暗，反而丢了这层层次（cli 说的「没给颜色」）。
- **改动**（`crates/lingmiao-tui/src/app.rs`）：`push_tool_card` 的 `⎿` body 颜色由 `TEXT_MUTED`（深灰）改回 [`TEXT`]（终端默认前景 = 最亮）；**失败仍 `ERROR`（红=信号，§8.4）**。工具名 / 主参数 / 结果现都用正文色，与 CC 一致（工具卡本就靠**绿色 `●`** 与**耗时灰**分层，不靠把结果压灰）。文字「耗时」与折叠提示 `∴ … 还有 M 行 · ctrl+o 展开` 仍是灰（`TEXT_MUTED`）。
- **与 §12.11 P2-b / fe14e4b 的关系**：那两处把 `TEXT_MUTED` 提亮 / 压暗属**令牌值**调整（思考/尾行/提示用），**有效**；本节点只**订正其溢出**——工具结果不属「次要灰」那一类，而是「主体亮色」。故不是推翻 fe14e4b，而是收窄它的作用面。
- **测试**：`tool_result_is_bright_like_cc_not_muted`（`⎿` 行 span 前景 = `TEXT`，且**不**含 `TEXT_MUTED`）、`error_tool_result_stays_red`（失败工具 `⎿` 仍为 `ERROR`）；另加两条 markdown 令牌回归（`markdown_chrome_uses_theme_tokens_not_raw_colours`：渲染出的每个前景色都来自 `theme::` 令牌，无裸 `Color::`；`blockquote_text_is_primary_not_dimmed`：引用条 `│` 灰、被引正文亮）—— 把「chrome 灰 / 正文亮」这条原则从工具结果扩到 markdown 元素。同时订正 `theme.rs` 两处 stale 注释（`TEXT_MUTED` 文档仍把 `⎿` **结果正文**列为灰；模块头仍称回复 `●` 用 accent）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **304 passed / 0 failed**；computeruse :99 真进程实拍（**提交后**二进制 `7d2074d`，重启 :99 真跑一轮 `ls -la /tmp | head -3`）——工具卡 `● 运行命令(…)` **绿**点 + `⎿ exit=0` **亮色** + 其上的 `∴ 思考` **灰** + 折叠提示 `∴ … 还有 3 行` **灰**，两层分明。截图 `/tmp/computerse/ling-v4-s1215-1790226880667.png`（放大 `/tmp/computerse/ling-v4-s1215-zoom.png`）。**逐像素取色**：思考行 / 折叠提示 `(127,127,127)` = `TEXT_MUTED`；工具结果 `⎿` / 工具名 / 回复 `(242,243,245)` = `TEXT`；工具 `●` `(0,198,12)` ≈ CC 绿 `RGB(0,205,0)`。

### 12.16 配色三订正：工具结果中灰 / 思考标题白 / footer 底行不缩进（2026-09-24 落地 ✅ · 订正 §12.15 / §12.7 / §16.2）

cli 2026-09-24：「1、工具调用返回值显示为比思考稍微亮一点点的灰色，3 行 2、思考的标题用白色 3、最后一行常驻那个文字行改为不缩进，从画面最左边开始 —— 以上三条都是 CC 的样式」。

- **① 工具结果 = 中灰（比思考亮一档），显示 3 行**：`theme.rs` 新增令牌 `TOOL_RESULT = Color::Gray`（ANSI 37）；`app.rs` `push_tool_card` 的 `⎿` body 由 `TEXT`（最亮，§12.15）改回**中间灰** `TOOL_RESULT`；`TOOL_OUTPUT_HEAD_LINES` `1 → 3`。CC 里 `⎿ 工具结果` 是比思考亮的灰阶（xterm :99 实拍 思考 `161` 灰 / 结果 `242` 亮），灵妙用 ANSI 灰阶「上一档」表达：思考 `TEXT_MUTED`（当时 = `DarkGray`/SGR 90，**2026-09-24 §12.18 订正为固定 `#AAAAAA`**）、结果 `TOOL_RESULT = Gray`（SGR 37）。**失败仍 `ERROR` 红**（信号）。
- **② 思考标题 = 白**：`app.rs` `thinking_lines` 折叠标题 `∴ 思考 {N}s` 由 `ACCENT`（橙，commit `1740a04`）改 `TEXT`（终端默认前景 = 白）。折叠提示 `· ctrl+o 展开` 与推理正文**保持灰**（灰 = 隐藏 / 次要用，cli 规则「返回值或该隐藏的东西是灰色，其他都有颜色」）。
- **③ footer 最底常驻行不缩进**：`footer_suggestion_line` 去掉前导空格，从**列 0** 起（§16.2 的「中性灰一行、无标签」不变）。
- **测试**：`tool_result_is_a_middle_grey_above_the_reasoning`（`⎿` 前景 = `TOOL_RESULT`，且**非** `TEXT_MUTED`、**非** `TEXT`；取代 `tool_result_is_bright_like_cc_not_muted`）、`thinking_title_is_white_not_grey`（标题含 `TEXT` span；取代 `thinking_title_is_coloured_not_grey`）、`muted_text_is_cc_dark_grey_and_structural_lines_stay_mid_grey`（加 `TOOL_RESULT == Color::Gray` 断言）；`header_uses_theme_tokens` 的 allowed 集补 `TOOL_RESULT`。
- **验证**：`cargo fmt --all` 无差异；`clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-tui` **96 passed / 0 failed**；computeruse :99 真进程实拍（重启为新二进制后真跑一轮「用一句话说明你在做什么」→ 组织上下文检索出工具卡 + 工作阶段思考块）。**逐像素取色**：思考标题 `(242,243,245)` = 白、思考正文 `(63,63,63)` = ANSI 90 深灰、工具结果 `⎿` `(192,192,192)` = ANSI 37 浅灰（比思考亮）、工具结果**恰 3 行**（y 带 `127–142` / `151–166` / `175–188`）+ `∴ … 还有 N 行 · ctrl+o 展开`；footer 最底行左起 ink `x=3`（CJK 字形边距 = 列 0 起；对比 L1 `x=11` / L2 `x=16` 均带前导空格）。截图 `/tmp/computerse/mid2-1790232459163.png`。

### 12.17 去掉常驻 header：身份改「启动横幅 + footer 右下角」（2026-09-24 落地 ✅ · 取代 §13.1 / §13.2）

cli 2026-09-24：**「header 部分是 CC 没有的，它只有启动的时候在对话历史里打印，咱们也保持一致，内容转移至 footer 右下角」**。

- **去掉常驻 header 带 + hairline**：CC 顶部**没有**固定 header —— 撤掉 §13 起的 `▟▀▙ 灵妙… / model · cwd` 两行块**与** `─` 发丝线（`render_header` / `render_rule` 删除）。`render()` 现为 `[body(Min), footer(3)]`，对话区从**第 0 行**起。
- **启动横幅**（`App::push_startup_banner()`，`lib.rs` 启动时调一次）：把身份块**一次性**打进对话历史（`▟▀▙ 灵妙 lingmiao vX.Y.Z` / `▜▄▛ <model> · <cwd>`）—— 与 CC 启动时在对话里印 logo + 身份块同形，随对话**向上滚走**，不是常驻带。
- **footer 右下角常驻身份**：footer **最底行**（L3）**右侧右对齐**常驻 `灵妙 vX.Y.Z · <model> · <cwd>`（`TEXT_MUTED` 灰）；左侧仍是白板内容（§16.2，不缩进）。身份宽取 `min(文本宽, 屏宽/2)`，窄屏截断、白板行相应收窄，**绝不溢出**。
- **测试**：新增 `no_persistent_header_identity_at_footer_bottom_right`（空态首行无品牌 → 无 header；footer 最底行右对齐含身份；`push_startup_banner` 后首行是横幅）；`header_uses_theme_tokens`（去 `RULE` 断言，改断言启动横幅画 `LOGO`）、`render_single_column_into_a_test_backend` / `compact_layout_renders_on_a_short_terminal` 改为先 `push_startup_banner()` 再断言品牌 / 模型。
- **验证**：`cargo fmt --all` 无差异；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-tui` 97 passed / 0 failed（workspace 全绿）；computeruse :99 真进程实拍（VTE 渲染器 140×44）—— 启动态顶部是**启动横幅**（无 header 带 / 无发丝线），footer 最底行**右对齐** `灵妙 v0.12.0 · deepseek-v4-pro · ~/ai/lingmiao`（左为白板内容）；真跑一轮「用一句话说明你在做什么」→ 横幅随转录**上滚消失**（证其属对话内容、非固定 header），用户反色条 / 思考块 / 工具卡正常。截图 `/tmp/computerse/hdr-ling3.png`（启动）/ `hdr-turn1.png`（回合中）/ `hdr-turn2.png`（横幅已滚走）。

### 12.18 次要灰改固定中灰对齐 CC + 活动行 / 尾行顶格（2026-09-24 落地 ✅ · 订正 §12.16 / §4 / §12.7）

cli 2026-09-24（看新截图）：①「灰色的颜色比 CC 的暗了」②「活动行要跟屏幕最左侧对齐，不要跟二级缩进对齐」。

- **① 次要灰 `TEXT_MUTED`：ANSI 90 → 固定中灰 `#AAAAAA`**。§12.16 用 `TEXT_MUTED = Color::DarkGray`（ANSI 90）对齐 CC 的 ANSI `gray` 表项，但在 cli 终端上 ANSI 90 渲染 ≈ **RGB 90**，比 CC 实拉的同屏 muted 正文（**≈ RGB 170**，`/tmp/computerse/` 逐像素）明显偏暗。故令牌改用**固定** `Color::Rgb(0xAA,0xAA,0xAA)`（=170），与 CC 实际所绘一致、不受终端 ANSI-90 映射影响；仍暗于工具结果（`TOOL_RESULT`，ANSI 37 ≈ 192）与正文（`TEXT`），层次不变。
- **② 活动行 + 尾行顶格**：`render_conversation` 的活动行与 `push_tail` 的尾行去掉 2 空格前导（`  ✻ …` → `✻ …`），从**列 0** 起、与 CC 的 `✻ …` 一致；不再跟二级缩进（思考正文的 2 列缩进）对齐。
- **测试**：`muted_text_is_cc_mid_grey_and_structural_lines_stay_mid_grey`（`TEXT_MUTED == Rgb(0xAA,0xAA,0xAA)`；取代 `…_dark_grey_…`）、`turn_tail_line_is_cc_shaped`（尾行以 `✻` 起、无前导空白）。
- **验证**：`cargo fmt --all` 无差异；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿（lingmiao-tui 97 passed）；computeruse :99 真进程实拍 —— **逐像素取色**：思考正文 / 活动行 / footer 提示 `(170,170,170)`（= CC 亮灰；旧值 `(94,92,100)`）、思考标题 `(242,243,245)` 白；活动行 `✻` ink `x=11` ≈ 列 0（对比思考正文缩进 `x=34`）。截图 `/tmp/computerse/ling3-turn-1790234290541.png` / `ling3-done-*.png` / `ling3-tail-*.png`。

### 12.19 思考关键词高亮：白+粗 → CC 的柔蓝（2026-09-24 落地 ✅ · 订正 §12.7 / §4）

cli 2026-09-24（「新图片，高亮的处理你看看」，附 CC ↔ 灵妙 同屏截图）：**思考块里的关键词高亮处理**与 CC 不像。

- **CC 的做法（同屏实测）**：CC 把 reasoning 里的代码 token（截图里是 `` `ai` ``）画成**柔蓝**（逐像素取色 = **`#B1B9F9`**，periwinkle），**常规字重**；同一段其余正文是均匀的灰 `#AAAAAA`。
- **灵妙旧做法**：`THINKING_KW = Color::Reset`（终端默认前景 = 白）**+ `BOLD`** —— 白 + 粗与 `●` 回复正文同色，读起来像「思考里混进了白字」，既违背「灰 = 思考」的规则，也不像 CC 的高亮。
- **订正**：`theme.rs::THINKING_KW` 由 `Color::Reset` → **固定** `Color::Rgb(0xB1,0xB9,0xF9)`（对齐 CC 实测值）；`app.rs::thinking_lines` 的关键词样式去掉 `Modifier::BOLD`（CC 的高亮是**色相**，不是字重）。token 划分逻辑（`thinking_keyword_runs` / `is_keyword_token`）**不变**。
- **测试**：`thinking_block_is_grey_upright_with_keyword_emphasis` 增断言 —— 关键词 span **不带 `BOLD`**；`muted_text_is_cc_mid_grey_and_structural_lines_stay_mid_grey` 增断言 —— `THINKING_KW == Rgb(0xB1,0xB9,0xF9)`；`header_uses_theme_tokens` 允许集加入 `THINKING_KW`。
- **验证**：`cargo fmt --all` 无差异；`clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-tui` **97 passed / 0 failed**。computeruse :99 真进程（VTE 渲染器 + `GDK_BACKEND=x11`）真回合实拍逐像素：思考块里的 `read_file` = **`(177,185,249)` = `#B1B9F9`**（命中），同段正文 `(170,170,170)` 灰、非粗。截图 `/tmp/hl_turn.png` / `hl_kw_zoom.png`。

---

## 13. CC 视觉对标 — chrome 细化（2026-09-20 落地 ✅）

> §12 立起了**单栏骨架**；本节把「外壳」对齐 CC 的**视觉讲究** —— 身份区 / 提问行 / 输入框 / 分隔线。
> 补齐后首屏与对话区不再像「裸 ratatui」，而像 CC 一样有**层次**。§12 的 footer 三段**不变**。
>
> ⚠ **§13.1（身份区 header）与 §13.2（分隔线 hairline）已被 §12.17 取代**（cli 2026-09-24：CC 无 header）—— 身份改「启动横幅 + footer 右下角常驻」，发丝线撤除。§13.3（反色提问条）/ §13.4（圆角输入框）/ §13.5（与 §12 的关系）仍有效。

### 13.1 身份区（header）

CC 首屏左侧印一枚**品牌 mark** + 右侧两行身份块；灵妙照做：

- **mark**：`▟▀▙` / `▜▄▛` 两行块字符，`ACCENT`（品牌橙）—— 占 6 列，像一枚印章。
- **身份块**（右）：行 1 `灵妙 lingmiao  vX.Y.Z`；行 2 `<model> · <cwd>`。
- **短终端折叠**（`height < 14`）：mark 与身份并成**一行** `▟▀▙ 灵妙…`，省一行给对话区。

### 13.2 分隔线（hairline）

身份区与对话区之间一条**整宽 `─` 细线**（`DarkGray`）—— CC 用它把「壳」与「内容」分开；没有它，header 与对话会糊在一起。

### 13.3 提问行（反色条）

CC 把**每个用户回合画成一条反白（reverse-video）横条** —— 整行反色、像一块实心带。灵妙照做：

- `❯` 前缀保留 `ACCENT` + 加粗 + 反色；正文整段 `REVERSED`，**补空格到窗宽**使其成一条实心带。
- 长提问按窗宽换行（CJK 按 2 列计）；续行以三空格对齐。

### 13.4 输入框（圆角框）

CC 的 prompt 是一个**带框的输入区**。灵妙改用 `Block::default().borders(ALL).border_type(Rounded)` 包住输入：

- 首行 `❯ `，续行两空格缩进；光标按框内（`block.inner()`）偏移定位。
- 输入框高度随多行内容增长（`+2` 行容边框），`clamp(3, 9)`。

### 13.5 与 §12 的关系

| 项 | §12（骨架） | §13（细化） |
|----|-------------|-------------|
| header | 单行文本 | **mark + 两行身份块**（短终端折叠） |
| 分隔 | 无 | **hairline `─`** |
| 提问行 | `❯ text`（accent 前缀） | **整行反色条** |
| 输入框 | 无边框 `❯ ` 前缀 | **圆角框** `Block(Rounded)` |
| footer | 3 行（快捷键 / 上下文无 % / 白板摘要） | **不变** |

### 13.6 落地状态（✅ 已实现，`crates/lingmiao-tui/src/app.rs`）

- 新增 `render_rule()`（hairline）；`render_header(compact)` 加品牌 mark + 两行身份块。
- `push_turn(user=true)` 改为**反色条**：按窗宽换行 + 补空格 + `REVERSED`（`❯` 双色）。
- `render_input` 改用**圆角框**，光标按 `block.inner()` 偏移；`render_body` 输入高度 `+2` 容边框。
- 验证：`cargo fmt --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿。
- **computeruse :99 实拍**（132×40）：首屏 `▟▀▙ 灵妙 lingmiao vX.Y.Z / deepseek-v4-pro · ~/ai/lingmiao` + hairline + 圆角输入框；真回合「只回答两个字：你好」→ **反色提问条**（`❯ 只回答两个字：你好` 整行反白）+ `∴` 思考行 + `● [正在作答] 你好` + footer L2 上下文 `6.8K/128.0K · 规则底座 2.6K · 知识记忆 0 · 历史观测 0`（无 %；当时形 —— `/128.0K` 于 §12.6 去除）。

**待续（下一轮）**：~~主题令牌层扩充~~（→ §14 ✅）、~~活动动词轮换~~（→ §14 ✅）、~~tips 轮播~~（→ §14.4 ✅）。

---

## 14. 主题令牌层 + 活动动词（2026-09-20 落地 ✅）

> §13 补齐了「外壳形」，本节把 CC 的**主题层次**与**活动动词**两块细节落地 —— 这是 page24 诊断「差距在主题层+身份+动词+密度」里剩下的两项（身份区见 §13）。

### 14.1 主题令牌层（§14-P0）

`crates/lingmiao-tui/src/theme.rs` 从 5 个常量扩为**分层令牌**，一处定义、全局引用；widget 层**禁止裸 `Color::`**（有回归单测守卫）。值源 page24 记录的 CC 浅色主题（`bg #fffdf8 · surface #f4efe4 · border #7a6c52 · line #8a7f6d · text #42392e`）。

**结构层**（keyed 到终端 palette，**不自绘背景**）：

| 令牌 | 语义 | 值 |
|------|------|----|
| `SURFACE` | 面板顶层角色（保留：本 TUI 不绘背景） | `Reset` |
| `BORDER` | 边框（圆角输入框） | `DarkGray` |
| `RULE` | 整宽 `─` 发丝线（§13.2） | `DarkGray` |
| `TEXT` | 正文（模型行 / 工具参数 / 白板标题） | `Reset`（终端默认前景） |
| `TEXT_MUTED` | 次要 / 元信息（徽标 / 提示 / 耗时 / 版本 / cwd） | `DarkGray` |
| `SELECTION_FG` / `SELECTION_BG` | 上翻条反色对（§11） | `Black` / `Gray` |

**语义层**（颜色 = 信号，§8.4）：

| 令牌 | 语义 | 值 |
|------|------|----|
| `SUCCESS` | 成功（工具点 / 就绪 bullet） | `Green` |
| `WARN` | **告警**（上下文阈值条 / 故障提示） | `Yellow` |
| `ERROR` | 错误 | `Red` |
| `LOGO` | 品牌 mark（header 红 logo；§12.12 配色收拢，cli 2026-09-24） | `Red` |
| `RUNNING` | **进行中活动**（header `作答中…` / 运行中 spinner） | `Yellow` |

**映射原则（terminal-native）**：灵妙**跟随系统终端主题**（§5，2026-09-16 拍板「跟随系统终端主题」），CC 亦不自绘背景（obs「CC 不自绘背景」），故结构角色映射到**终端自带调色板**而非固定 RGB，明/暗底都醒目。唯一例外 = 品牌强调色 `ACCENT`（RGB 橙 `#D77757`，CC 固定值）。`TEXT` 取 `Reset`（默认前景）——实拍发现固定 `Gray` 在浅底反比 muted 更淡（语义倒置），故修正。

**`RUNNING` vs `WARN`**：两者同为 amber，但**语义分离** —— `RUNNING` 表「正常工作中」（不是告警），`WARN` 严格保留给真告警（阈值条 / 错误提示）。此前活动行/状态灯误用 `WARN` 属语义倒置，已改走 `RUNNING`（§14-P2 顺手改）。

**强回归守卫**：单测 `header_uses_theme_tokens` 渲染真帧后断言**帧内每个前景色都在令牌白名单内**，防回退到散落 `Color::`。

### 14.2 活动动词轮换（§14-P2）

CC 的活动/尾行动词取自一个 **44 个 gerund 的轮盘**（page24 记录，CC 2.1.278）。灵妙照做：

- **活动行**（`is_busy()` 且无运行中工具卡时）：`◜ {动词}… {N}s · esc 中断` —— 动词每 **800ms** 换一个（`VERBS[(elapsed_ms / 800) % 44]`），前导标记每 **120ms** 在 `◜ ◝ ◞ ◟` 间轮转（2026-09-28 §27.3 由 CC 星形族换为旋转弧：星形在本机等宽字体缺失、被比例字体替补且宽度不一 → 抖动），长回合可见地「在动」。（2026-09-24 按 CC 去掉阶段串与「已」字，见 §12.13。）
- **尾行**（回合结束）：`◜ {动词} for {N}s · done HH:MM:SS` —— `done` 用 `chrono::Local`（**本地时钟，非 UTC**）；动词 = 完成时轮盘落点，前导标记固定为 `TAIL_MARK`（`◜`，= 旋转弧第 0 帧，2026-09-28 §27.3 替掉 `✻`）。与 CC 的 `✻ Cooked for 4s · done 18:39` 同形（标记不同）。（2026-09-24 去掉 `· {N} 个工具`，见 §12.13。）
- 单测：`activity_verbs_rotate_with_elapsed`（800ms 步进 + 绕回 + 44 项无空）、`done_timestamp_is_local_wall_clock`（`HH:MM:SS` 形状）、`turn_tail_line_carries_elapsed_and_tool_count`（尾行含动词 + `for` + `done`）。

### 14.3 落地状态（✅ 已实现，`crates/lingmiao-tui/src/theme.rs` + `src/app.rs`）

- `theme.rs`：新增结构层（`SURFACE/BORDER/RULE/TEXT/TEXT_MUTED/SELECTION_*`）+ 语义层（`SUCCESS/WARN/ERROR/LOGO/RUNNING`）；保留 `ACCENT/CODE_FG/LINK/SPINNER`（`THINKING`/`INFO` 于 §12.12 配色收拢后无用户，已删）。
- `app.rs`：42 处裸 `Color::` 全改走 `theme::`（widget 层 `grep Color::` 归零，仅剩注释/白名单断言）；活动行改动词轮换、尾行加 `done` 时间戳；`status_span` / 运行中 spinner 由 `WARN` 改 `RUNNING`。
- `motion.rs`（§14-P3 抽出）：`VERBS` + `activity_verb()` + `now_hms()`（§14-P2）与 `TIPS` + `TIP_COOLDOWN` + `tip_index()`/`tip_at()`（§14-P3）—— 三个**纯函数**聚一处，`app.rs` 只调用（职责更清，也更好单测）。
- 顺手（§12.3 遗留）：`footer_hint()` 按可用宽度**逐级丢弃最不重要分段**，保住 `⌃C 退出`，不再硬截断尾部。
- 验证：`cargo fmt --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿。
- **computeruse :99 实拍**：首屏 mark 橙 / 品牌名青 / 模型默认 / 版本·cwd 灰 / 发丝线灰 / 圆角输入框灰；真回合「只回答两个字：你好」→ 整行反色提问条（`❯` ACCENT）+ `∴` 灰青思考行（当时形；§12.7 起默认折叠为灰摘要）+ `[检索记忆]` 灰徽标 + 活动行 `✻ {动词}… 已 Ns`；回合结束 `● 就绪` 绿 + 尾行含 `done HH:MM:SS`。**颜色分层 + 动词轮换 + done 时间戳全部可见**。

### 14.4 空闲 tips 轮播（§14-P3）

CC 在**输入框空闲态**轮播一条小贴士（页24 记录其有 cooldown）；二进制字符串表实拍取证，例如
`Use /clear to start fresh when switching topics and free up context`、
`Hit Enter to queue up additional messages while Claude is working.`、
`Use /memory to view and manage Claude memory`、
`Use /statusline to set up a custom status line that will display beneath the input box`。
灵妙照做，`TIPS: [&str; 10]` 全部指向**本 TUI 真实存在的命令 / 按键**：

- **位置**：**输入框 placeholder**（CC 同形）—— 输入缓冲为空且 `State::Idle` 时，`❯ ` 后以 `TEXT_MUTED` 灰字显示一条 tip，取代空框；一旦用户开始输入 / 有回合在飞，tip 让位（这正是「priority」规则，天然满足）。
- **cooldown**：`TIP_COOLDOWN = 8s`；`tip_index(elapsed_ms, n) = (elapsed_ms / 8000) % n`，每 8s 换一条、循环。时间源 = `App.tip_started`（`Instant`）。
- **priority（纯规则）**：`placeholder_tip()` 只在 `!is_busy() && input.is_empty()` 时返回 `Some(tip)`，否则 `None`。
- **纯函数**：`tip_index()` / `tip_at()` 抽在 `crates/lingmiao-tui/src/motion.rs`，与动词轮换、时钟同处，便于单测。
- 单测：`tips_rotate_with_cooldown`（8s 步进 + 绕回 + 10 项无空）、`idle_input_shows_a_tip_but_yields_to_typing_or_busy`（空闲显示 `/help` tip；输入后 / busy 时隐藏）。

**待续（下一轮）**：本任务 page24 P0–P3 已全部落地（§13 外壳 / §14-P0 主题 / §14-P2 动词 / §14-P3 tips），任务可闭合。

---

## 15. footer 四数 + core lock 注入 + `search_memory` 聚合工具（2026-09-21 落地 ✅）

> ⚠ 本节**取代 §12.3 的 L2 形态**与 **§12.4 的 `/context` 展开**（`/128.0K` 窗口口径一并下线）。

cli 2026-09-21 拍板（page28/29「按我的要求来」「可以」）三件事：
1. footer L2 显示 **系统提示词 token / lock token / 总 token / 其余 token**；
2. 补 **core lock 注入**（对齐 原版 `_build_lock` / `_reinject_lock` 机制）；
3. 补**真 `search_memory` 跨层聚合工具**（工作阶段系统提示已承诺它，但此前报错不存在）。

### 15.1 footer L2 → 系统 / lock / 总 / 其余

- 旧 L2 列出**全部分段**（`上下文 <总> · 规则底座 … · 知识记忆 … · 历史观测 …`），过长、且与 `/context` 重复。
- 新 L2 = **四个数**：` 上下文 系统 <n> · lock <n> · 总 <n> · 其余 <n>`。
  - **系统** = 工作阶段 system prompt（`规则底座` 段字符数 × 校准系数 `input_tokens ÷ total_chars`，§8.5 三步法）。
  - **lock** = base+stage 组合 core lock 的字符数 × 同一系数。
  - **总** = provider 实测 `input_tokens`（真值）。
  - **其余** = `总 − 系统 − lock` —— 检索记忆 / 历史 / 工具 schemas / 工具结果 / 提问等其余的合计。
- 仍**无 `%`、无窗口数字**（§12.6）；越阈值仍以**颜色**作信号（§8.4）。按 `width` 自适应截断，绝不溢出（cli P0）。
- 实现（`crates/lingmiao-tui/src/app.rs`）：`footer_context_line(width)` 改四数；新增 `section_tokens(name)` 按名字取该分段 token（与组件同源的 `chars × 系数`）。

### 15.2 移除 `/context` 命令

`/context` 的段明细与旧 L2 重复、且带误导性的 `128.0K` 窗口，cli 拍板**去掉**：`COMMANDS` 9 → 8、`push_context_detail()` 删除；随后输入 `/context` 回「未知命令」，`/help` 也不再列它。窗口口径随之下线，footer 只留**真 token**。

### 15.3 core lock 注入（原版 parity）

- **问题**：`locks.json` 定义了 base + 每阶段 core lock，但**从不注入**（死配置）—— 长工具循环里，模型对「当前阶段核心职责」的记忆会在不断增长的转录中被冲淡。
- **对齐 原版**：`crates/lingmiao-engine/src/stage_agent.rs` 的 `StageAgent` 新增 `core_lock: String` + `with_core_lock()`；引擎每阶段传 `cfg.full_core_lock(stage)`（= base + stage，截断到 `core_lock_max_len=300`）。
- **注入时机**（`reinject_lock()`，纯函数）：① **首个 user turn**（`user_content = prompt + lock`）；② **每次工具结果之后**把 lock 追加到最新一条消息。空 lock 为 **no-op**，故 lock-less 阶段 / 测试**逐字节不变**。
- **上下文条同步**：`crates/lingmiao-engine/src/engine.rs` 的 `context_sections` 新增 `lock` 段（紧随 `规则底座`），常用分段 6 → **7 段**（§8.5 分段名同步）。

### 15.4 `search_memory` 跨层聚合工具

- **问题**：工作阶段系统提示承诺 `search_memory`，但工具集里**只有分层工具**（`search_observations` / `search_knowledge` / `search_archive`），模型调它会失败。
- **做法**（`crates/lingmiao-tools/src/memory_tools.rs`）：新增 `SearchMemoryTool` —— 一次调用**同时**从 #2 observations（语义 + 关键词）/ #3 knowledge（语义）/ #1 archive（关键词）召回，按 `(layer, id)` 合并去重（保留最高分）、按分排序、取 top-N（默认 10，上限 20）。单层失败不沉整体（逐层 try，收集 `errors`）。
- **入册**：`TOOL_NAMES` 9 → 10 + `register()`；`crates/lingmiao-core/assets/stages.json` 三阶段（组织上下文 / 工作阶段 / 沉淀阶段）白名单均加 `search_memory`。
- **参数**：`keyword` 或 `query`（别名，取一即可，皆空 → `InvalidArgs`）；无命中时返回友好提示（建议换词 / `memory_stats` / `update_memory`）。
- **单测**：`search_memory_merges_layers`（一次召回同时含 observations + knowledge）、`search_memory_accepts_query_alias_and_rejects_empty`。

### 15.5 验证

- `cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **204 passed / 0 failed**。
- **computeruse :99 实拍**：真回合调用了 `search_memory`（工具卡 `search_memory(keyword="灵妙 TUI", limit=10)` → 返回 10 条 observations）；footer L2 = **`上下文 系统 5.9K · lock 147 · 总 16.7K · 其余 10.7K`**（四数齐）；`/context` 回「未知命令: /context」。

---

## 16. UI 修改四点（cli 2026-09-21 反馈）（2026-09-21 落地 ✅）

cli 2026-09-21 四条修改意见，均 `:99` 真进程实拍验收（`crates/lingmiao-tui`）。

### 16.1 思考折叠保留实时尾部（修订 §12.7）

- **问题**：「思考部分不能完全收起来，还是要有几行实时显示最后部分，不然看起来等待很焦急」——原折叠只剩一行摘要，长推理时读起来像卡住。
- **做法**（`crates/lingmiao-tui/src/app.rs`）：`thinking_lines()` 折叠态改为 **灰头 + 末尾 ≤4 行**（新常量 `THINKING_TAIL_LINES = 4`）—— 头部 `∴ 思考 {N}s · ctrl+o 展开`（灰、正体；2026-09-24 去阶段徽标，见 §12.13），其下按 `wrap_cjk` 折行取**最后 4 段**（灰 `TEXT_MUTED`、正体、缩进 2 列），随流式每帧更新。`ctrl+o` 仍展开全文。
- **测试**：`thinking_collapsed_shows_header_and_live_tail`（长推理：头部被折、尾部可见、头含 `ctrl+o` 与徽标）、`ctrl_o_toggles_thinking_expansion`（展开显全文）。

### 16.2 小白板内容 → footer 底部灰字（删除 `小白板 ▸` 行）

- **问题**：「小白板的内容可以显示在最下面那个青色的位置…如有则显示，没有则不显示」「footer 的小白板可以删除」。
- **做法**：footer L3 由 `小白板 ▸ <标题> · <摘要>` 改为**当前页内容的首个非空行**（新纯函数 `whiteboard_suggestion()`；跳过 `[N] 标题` 头与 `（…）` 占位），中性灰、**无标签**、按宽度截断；空页 → 该行为空（不显示）。**（2026-09-24 §12.16 订正：从列 0 起，不缩进。）**
- **测试**：`whiteboard_suggestion_is_the_first_content_line` 取代 `wb_summary_parses_title_and_first_line`；`render_single_column_into_a_test_backend` / `compact_layout_renders_on_a_short_terminal` 断言改为白板内容（且不再有 `小白板` 标签）。

### 16.3 鼠标滚轮翻页（§11 补充：键盘之外新增滚轮加速）

- **问题**：「鼠标滚轮要能像 CC 一样使用自如，可以滚动对话历史，现在一滚动就是输入栏，出不去了」——未开鼠标捕获时，终端把滚轮转成 ↑/↓，被输入框的历史浏览吞掉。
- **做法**（`crates/lingmiao-tui/src/lib.rs` + `app.rs`）：`run()` 进出 `EnableMouseCapture` / `DisableMouseCapture`；`run_loop` 处理 `TermEvent::Mouse` 的 `ScrollUp` / `ScrollDown` → `App::scroll_up/scroll_down(WHEEL_LINES = 3)`；`PageUp/PageDown` 改调同一对方法（行为不变）。**起初只响应滚轮，点击/其它鼠标事件一律忽略**；2026-09-27 起左/右键**拖动**在对话区做应用内选择（§24，仍只是加速，所有能力仍仅靠键盘可达）—— §11「纯键盘」精神不变。
- **实拍**：`:99` 真进程 `xdotool click 4/5` → 对话区上翻/下翻，上翻时底部显 `↑ 已上翻 N 行 · ctrl+end 回到底部`。

### 16.4 验证

- `cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` **204 passed / 0 failed**。
- **computeruse :99 实拍**：真回合思考块显「灰头 + 尾部数行」实时滚动（组织上下文/工作阶段均验）；footer L3 = 白板内容首行（无 `小白板 ▸`）；滚轮上翻 18 行（6 notch × 3）后再下翻回底。

---

## 17. 斜杠命令补全菜单（2026-09-23 落地 ✅）

cli 2026-09-23 端到端验收发现：「`/` 无实时补全弹窗（仅 `/help` 列命令）」。§12.4 早已声称「功能走斜杠命令 **+ 补全菜单**」，但补全菜单**从未实现** —— 本节把它补上（对标 CC 的 `/` 命令面板）。

### 17.1 交互

- **打开**：输入缓冲为**单行**、以 `/` 开头、且命令词后**尚无空格**（即正在打命令名）时，输入框**上方**浮出补全面板；一旦打了空格（开始写参数）或缓冲含换行/非 `/`，面板即收。
- **过滤**：按已输入前缀匹配 `COMMANDS`（`starts_with`）。
- **导航**：`↑` / `↓` 在匹配项间移动高亮（在顶部/底部夹紧）；`Tab` **补全**当前高亮项到缓冲（`/` + 命令名，**不提交**）；`Enter` **执行**当前高亮项（CC 行为：面板即输入 —— 一个裸 `/` + `Enter` 现在跑 `help`，不再报「未知命令：/」）。
- **状态派生**：面板「是否打开」由**缓冲派生**（`completion_matches()`），不存独立开关 —— 故永不与用户实际输入不同步。高亮行 `menu_selected` 在读取时按匹配数 clamp，输入/删除/回退即归零。
- **抑制**：回合在飞（`is_busy()`）时面板不显示。

### 17.2 渲染

- 面板 = 输入框上方一个**圆角框**（`Block(Rounded)`，`Clear` 先擦底），逐行 `/{name:<9} {中文说明}`；选中行整行反色（`SELECTION_FG`/`SELECTION_BG`）+加粗，未选中行命令名 `ACCENT`、说明 `TEXT_MUTED`。
- 宽度 = `min(输入框宽, 48)`，高度 = `匹配数 + 2`（边框），受输入框上方可用行数约束；可用行 < 3 时不画（键位仍可用）。短面板时窗口随选中行滚动，保证高亮项始终可见。

### 17.3 实现与验证

- **实现**（`crates/lingmiao-tui/src/app.rs`）：`App.menu_selected: usize` + `completion_matches()` / `menu_index(len)` 两个纯助手 + `on_key` 面板分支（`↑/↓/Tab`）+ `Enter` 分支（面板开时执行高亮项）+ `render_completion(body, input)`（`render_body` 在 `render_input` 前调用）。
- **测试**（`app.rs` +5）：`slash_palette_lists_and_filters_commands`（`/` 列全部 / `me` 过滤到 memory / 空格与 busy 关闭）、`slash_palette_tab_completes_the_highlighted_command`、`slash_palette_up_down_move_and_clamp_the_selection`、`slash_palette_enter_runs_the_highlighted_command`、`slash_palette_renders_above_the_input_box`（`TestBackend` 帧含 `/memory` + 中文说明，过滤后只剩 `/help`）。
- **验证**：`cargo fmt --all --check` EXIT=0；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿。
- **computeruse :99 实拍**（真进程 `target/debug/lingmiao`，132×40）：输入 `/` → 面板列 8 条命令（首行 `/help` 高亮）；输入 `me` → 过滤到 `/memory 记忆库统计`；`Tab` → 缓冲补成 `/memory`；`BackSpace`×6 回 `/` + `↓↓` → 高亮移到 `/memory`（灰带）；`Enter` → 执行 `/memory`，转录打印 memory 统计块（archive 65 / observations 76 / knowledge 33 nodes）。

---

## 18. 七条 UI 调整：footer 2 行 + 活动行 ctx + 白板上移 + 桔黄 logo + 英文工具名 + 阶段标签（cli 2026-09-24）（2026-09-24 落地 ✅）

cli 2026-09-24 一次性给出 7 条 UI 调整（「我觉得 CC 的 UI 布局很优秀，照着它的改」），全部落地并 `:99` 真进程实拍验收（`crates/lingmiao-tui`）。**本节取代 §12.1 的 footer 3 行形态、§12.3，并订正 §4（工具名 / 阶段徽标）、§12.13（阶段徽标去除）、§12.17（logo 红）。**

### 18.1 七条与落地

| # | cli 要求 | 落地 |
|---|---------|------|
| ① | 输入框下保留 2 行，取消操作提示 | footer 由 **3 行 → 2 行**；**删除**快捷键提示行（`footer_hint()` 移除） |
| ② | 活动行改成：缓解焦虑提示 + 状态时间 + 同行最右简写上下文占用；CC 运行时会切换词语和闪烁渐变颜色 | 活动行 = `✻ {动词}… {N}s · esc 中断`（左）+ `7.2K/128K`（**右对齐**）；动词每 800ms 轮换（§14.2 既有）+ **颜色渐变**（`activity_color()`，700ms 走 4 色） |
| ③ | 输入框上面放小白板 | 白板由 footer 行 → **输入框上方独立 band**（`render_whiteboard` / `whiteboard_band_height`，上限 `WHITEBOARD_MAX_LINES=5`，空页不显示） |
| ④ | 灵妙的标志换个桔黄色好看的 | `theme::LOGO` 由 `Red` → **暖琥珀桔黄 `#F5A623`**（订正 §12.12/§12.17 的红 logo） |
| ⑤ | 输入框下边那行参考 CC 放点有用的东西 | footer L1 = 状态灯 · 会话 token；L2 = 上下文四数（§15.1） |
| ⑥ | 工具调用的工具名显示英文 | 工具卡 / 运行中工具卡的工具名由中文 → **英文原名**（`read_file` / `search_memory`…）；**删除** `tool_label()`（订正 §4「说人话的中文名」） |
| ⑦ | 每个思考 / 工具调用 / 正文都要有阶段标志，饱和度拉低降低存在感 | **恢复** `stage_badge()`（§12.13 曾按 CC 去除）并加**低饱和**色 `STAGE_TAG`（`#6B7480`）；思考折叠标题 / 展开首行、工具卡、运行中工具卡、回复 各带 `[检索记忆]`/`[正在作答]`/`[沉淀整理]` |

> **与 §12.13 的关系**：§12.13 曾「按 CC 全去徽标」；本轮 ⑦ 明确要「加回、低饱和」，故**反转 §12.13 的徽标去除**。§12.13 其余项（工具结果折 3 行见 §12.16、去 `· N 个工具`、去总结调试行）**仍有效**。

### 18.2 新布局

```
[对话历史区]
∴ [检索记忆] 思考 14s · ctrl+o 展开           ← ⑦ 低饱和阶段标签（思考 / 工具 / 回复各带）
● [正在作答] read_file(Cargo.toml)  0.1ms      ← ⑥ 英文工具名 + ⑦ 标签
  ⎿ [workspace] …                             （§12.16 中灰、折 3 行）
✻ Formulating… 17s · esc 中断      20.5K/128.0K ← ② 动词 + 时间 … 最右 ctx 简写（渐变）
[1] Page 1                                     ← ③ 小白板（输入框上方）
任务：…
╭───────────────────────────╮
│❯ 输入…                    │                  ← 输入框
╰───────────────────────────╯
● 就绪 42.8K↓ 4.7K↑                             ← ① ⑤ footer L1（状态 + 会话 token，无快捷键提示）
上下文 系统 3.3K · lock 83 · 总 28.3K · 其余 24.9K   灵妙 v0.12.0 · model · cwd  ← footer L2（四数 + 右下身份）
```

### 18.3 实现要点（`crates/lingmiao-tui/src`）

- `theme.rs`：`LOGO → Rgb(0xF5,0xA6,0x23)`；新增 `STAGE_TAG = Rgb(0x6B,0x74,0x80)`（低饱和）。
- `app.rs`：
  - `render()`：footer 高度 3 → **2**（矮终端 1）。
  - `render_body()`：垂直三段 `[conv(Min), whiteboard(Length), input(Length)]`；新增 `render_whiteboard` / `whiteboard_band_height` / `whiteboard_content_lines`（空页塌缩）。
  - `render_footer()`：2 行（L1 状态 + 会话 token；L2 四数 + 右下身份）；**删 `footer_hint()`**、**删 `footer_suggestion_line()` 与 `whiteboard_suggestion()`**（白板上移）。
  - 活动行：右侧 `ctx_short()`（`7.2K/128K`）+ `activity_color()`（700ms 渐变）。
  - 阶段标签：`stage_badge()` 去 `#[allow(dead_code)]`、新增 `stage_tag_span()`；接入 `push_tool_card` / 运行中工具卡 / `thinking_lines`（折叠标题 + 展开首行）/ `push_answer`。
  - 工具名英文：`push_tool_card` / 运行中卡直接用 `tool` 原名；**删 `tool_label()`**。
- `lib.rs`：启动横幅不变（LOGO 换色即生效）。

### 18.4 验证

- `cargo fmt --all` 无差异；`clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-tui` **96 passed / 0 failed**（workspace 全绿）。
- **computeruse :99 真进程实拍**（VTE 渲染器 + `GDK_BACKEND=x11`，真 DeepSeek 回合「读取 Cargo.toml，用一句话说明这个包是做什么的」）：
  - idle：橙黄 logo + 启动横幅；白板 band 在输入框上方；footer **2 行**（`● 就绪 0↓ 0↑` / `上下文 ——` + 右下身份）；无快捷键提示行。
  - 回合中：`∴ [检索记忆] 思考 16s · ctrl+o 展开`；`✻ Formulating… 17s · esc 中断`（动词随时间轮换：Shimmying / Cooked / Formulating / Hustling…）。
  - 工具卡：`● [检索记忆] search_observations(audit)  0.1ms` + `⎿ …`（**绿点 + 低饱和标签 + 英文名**）。
  - 完成：`● [正在作答] 这是一个 Rust 语言编写的 AI 编程助手引擎（灵妙 lingmiao）…`（**回复带标签**）；footer L2 `上下文 系统 3.3K · lock 83 · 总 28.3K · 其余 24.9K`。
  - 截图 `/tmp/computerse/ling_idle-*.png` / `ling_mid-*.png` / `ling_done2-*.png` / `ling_cancel-*.png`。

## 19. 对话区微调：白板→上下文 band · 活动行带状态 · 阶段徽标去 `[]` · 工具卡恒显 `()` · 活动行只显总输入 tokens（cli 2026-09-24）（2026-09-24 落地 ✅）

### 19.1 cli 反馈

- **上一轮四点**（承接 §18）：① 工具调用标题被整行染绿；② **取消小白板**；③ 原小白板位置改放**上下文**；④ 状态改放**活动行**、删掉下方原状态行。
- **本轮三点**：① 阶段显示的 `[]` 去掉，同时要体现出是**阶段**；② 即使**没有参数**也要有 `()`，体现出是函数调用；③ `128K` 又来了——去掉，**15 个字符内体现出总 LLM 输入长度**即可。

### 19.2 落地

| # | 项 | 变化 |
|---|----|------|
| 上① | 工具标题整行绿 | `wrap_line` 逐段保留各自颜色（旧实现换行后整行套用第一段样式 → 80 列下标题全绿）。新增测试 `wrapped_line_preserves_per_span_styles` |
| 上② | 小白板取消 | 删 `render_whiteboard` + `whiteboard_band_height` + 输入框上方 band |
| 上③ | 上下文进 band | 新增 `render_context_band` / `context_band_height`（原 footer L2 的 `系统 / lock / 总 / 其余` 上移到输入框上方；首轮注入前折叠为 0 行） |
| 上④ | 状态进活动行 | 活动行 = `✻ {动词}… {状态} Ns · esc 中断`（`activity_status()` 取当前阶段）；footer 降为**单行**身份行 |
| 本① | 阶段徽标去 `[]` | `stage_badge()`：`[检索记忆]` → **`丨检索记忆`**（低饱和 `STAGE_TAG` 灰不变；`丨` 竖条标记「这是一段阶段归属」，非正文） |
| 本② | 无参也显 `()` | 工具卡头部恒渲染 `name(arg)`；无捕获参数时渲染 **`name()`**（如 `memory_kinds()`），读作函数调用 |
| 本③ | 去掉 `128K` | `ctx_short()` 只回**总 LLM 输入长度**（provider 实测 input_tokens），**≤15 列**（原始值放得下就用原始 `26186 tokens`，放不下退化为 `13.3K tokens`）；不再出现 `/128.0K` |
| 附 | 流式回复补徽标 | 渲染实时流式回复的路径此前漏挂阶段徽标（§12.13 去徽标时残留），现与已提交回复一致，**实时回复也带 `丨正在作答`** |

### 19.3 验证

- `cargo fmt -p lingmiao-tui` 无差异；`clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-tui` **98 passed / 0 failed**（+1 新测试 `tool_card_always_shows_call_parens`）。
- **computeruse :99 真进程实拍**（VTE 渲染器 + `GDK_BACKEND=x11`，真 DeepSeek 回合「调用 memory_kinds 工具（不要参数），说明记忆类型」）：
  - 工具卡：`● 丨检索记忆 memory_kinds()  0.2ms`、`● 丨正在作答 read_file(rules.md)  0.3ms`（**丨 徽标 + 无参 `()` + 有参 `(rules.md)`**）。
  - 思考：`∴ 丨正在作答 思考 49s · ctrl+o 展开`。
  - 流式回复：`● 丨正在作答 已调用 memory_kinds，实际返回如下:`（**实时回复带徽标**）。
  - 活动行右端：`✻ Musing… 沉淀整理 26s · esc 中断   26186 tokens`（**仅总输入长度，无 `/128.0K`**）。
  - 截图 `/tmp/computerse/cu99-before-*.png`（旧 `[检索记忆]`）、`ling-mid-*.png`、`ling-done-*.png`。

### 19.4 与 §8.4 / §18 的关系

- **§8.4 颜色=信号**不变：阶段徽标仍是低饱和灰（`STAGE_TAG` `#6B7480`），只是**去掉了方括号、改用 `丨` 竖条**（§18 ⑦ 的「加回徽标」继续有效）。
- **§18 ② 的活动行 ctx 简写**由 `{in}/{window}` 改为**只显 `{in} tokens`**（§18 ② 的 `/128.0K` 形态作废）。
- 上下文四数（系统 / lock / 总 / 其余）**位置**由 footer L2 上移到输入框上方 band（§18 ⑤ / §15.1 的 footer 形态作废，数据口径不变）。


## §20 阶段徽标指针改竖条（`丨`，无空格）+ 思考折叠尾部 3→4 行

**（cli 2026-09-24）** 两条微调：

1. **阶段指针形态定案**：§19 把阶段徽标从 `[检索记忆]` 改成 `▸ 检索记忆`（`▸` + 空格 + 名称），本轮 cli 拍板用**左侧竖条 `丨` 且不加空格** → `stage_badge()` 输出 `丨检索记忆 / 丨正在作答 / 丨沉淀整理`（`丨` 仍是低饱和 `STAGE_TAG` 灰，标记「这是一段阶段归属」，非正文）。徽标本身保留（§18 ⑦ / constraint「阶段徽标保留决定」继续有效），只换指针形态。
2. **思考折叠尾部行数 3 → 4**：`THINKING_TAIL_LINES` 由 `3` 改为 `4`——折叠态 `∴ 思考 {N}s · ctrl+o 展开` 其下的**末尾实时推理行**多留一行（灰、正体、缩进 2 列）。

**验证**：`cargo fmt` 无差异 / `clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0 / `cargo test -p lingmiao-tui` 全绿；computeruse :99 真进程实拍核对（工具卡 `● 丨正在作答 read_file(...)`、思考折叠末 4 行）。

---

## §21 错误必达 UI + 不假设上下文窗口（cli 2026-09-27）

**（cli 2026-09-27）** 两条原则 → 三组改动。

### 21.1 健壮性 —— 错误必须看得见

cli：「系统要健壮，即使未来没有发现的错误，也应该在 UI 上看得出来」。

- **根因（2026-09-27「无回答」bug）**：工作阶段 fault（如 HTTP 400）只写进 `App.error` 而**无人渲染**，且 `run_turn` 仍以 `Ok(report)` 收尾 → 用户只看到一句 `✻ done`，错误被静默吞。
- **`on_turn_done` 兜底**：回合以 `Ok` 收尾时，若 `self.error` 有值（任何阶段 fault），以**红色详情块**打进对话（新增 `push_error_detail`：多行按首行 `✖` + 缩进续行）。任何写入 `self.error` 的路径（现在或将来）都因此可见 —— 这就是「即使未来没发现的错误也看得见」的总机制。
- **`spawn_turn` 加 `catch_unwind`**：回合内 panic 不再让任务静默死掉、UI 永久卡 busy，而是转成 `LingmiaoError` 正常上报。
- **单测**：`stage_fault_is_visible_even_when_the_turn_ends_ok`。
- **:99 真进程验证**：`DEEPSEEK_BASE_URL` 指向死地址（`http://127.0.0.1:1`）触发真实调用失败 → 界面红字 `✖ 沉淀阶段失败：request failed: error sending request for url (http://127.0.0.1:1/chat/completions)`（此前完全静默）。

### 21.2 不假设任何上下文窗口数字

cli：「不能假定模型的上下文窗口，API 对窗口信息都不准确……不要假设是 128k，不能假设任何数字」。

- **删除 `ctx_line_color()` 的 `ctx_tokens / context_window` → 70% 黄 / 90% 红阈值配色**：上下文行（输入框上方单个数字，cli 2026-09-27 起不再有 `总 T · 余 R`）**恒为中性灰**（`TEXT_MUTED`），只报 provider 实测 token，不再有窗口 / 占比 / 警报。§8.4「颜色=信号」对该行**不再启用**（无可靠阈值）。
- **从 `ModelSpec` / `Client::context_window()` / 内置 `models.json` 移除 `context_window`**；`ModelCfg` 仍**消费**该键（`#[allow(dead_code)]`）但不使用 —— 避免用户 `models.json` 里的该键经 `extra` 漏进请求体。
- §8.4 阈值配色、§12.6 / §15.1 / §18 / §19 里的「窗口来源」口径随之下线（本已不显示数字，本轮把**颜色信号**也去掉）。

### 21.3 grep 根因收口（2026-09-27「无回答」根因）

- `GrepTool` 的 `SKIP_DIRS` 补 `.memory` / `.cache/lingmiao`（内部状态 / 记忆目录永不被搜）。
- 命中行加 **500 字符**单行上限 + **64KB** 结果总量上限，防 JSONL 超长行（单行可达数百 KB，×200 命中撑爆输入）。
- 单测 `grep_skips_internal_state_dirs` / `grep_bounds_a_pathological_line`。

### 21.4 验证

- `cargo fmt` 无差异；`clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿（lingmiao-tui 104）。
- computeruse :99 真进程：见 §21.1（错误可见）+ 启动 / 输入无回归。

---

*本文档随讨论更新；UI 细节可调整，架构性决策见 [`architecture.md`](./architecture.md)。*

## §22 反查历史搜索 ctrl+r（cli 2026-09-27，对标 CC `history:search`）

**（cli 2026-09-27）** 研读 CC 源码（`src/keybindings/defaultBindings.ts`：`'ctrl+r': 'history:search'` → `HistorySearchDialog`）后补齐的键盘能力。

CC 的 `ctrl+r` 打开一个 **Search prompts** 浮层：列出已提交 prompt，输入即过滤（子串 + 模糊），↑↓ 选择，Enter 填回输入框，Esc 取消。灵妙照此实现一个 ratatui 版浮层：

- **触发**：`ctrl+r`（空闲态）打开，过滤词**预填当前草稿**（对齐 CC 的 `initialQuery={input}`）；回合进行中不打开（此时输入框用于编辑队列）。
- **过滤**：对 `history`（oldest→newest）做**大小写不敏感子串**匹配，结果**新→旧**排列（CC picker 最新在顶）。
- **按键**：输入字符 → 追加过滤词并复位高亮；`Backspace` → 删一字；`↑/↓` → 移动高亮；`Enter` → 把选中项填回编辑器并关闭；`Esc` → 关闭、编辑器不变。
- **渲染**：复用 `/` 命令补全浮层的槽位与圆角边框，头行 `❯ {过滤词}  搜索历史 · ↑↓ 选择 · Enter 填入 · Esc 取消`，下方列出匹配（选中行反色、整行填充），内容超出单行则折叠换行符为 `⏎`、按宽截断。
- **实现**：`App` 增 `search_open / search_query / search_selected` 三字段 + `search_matches()`（纯函数、新→旧）/ `search_key()` / `close_search()` / `take_search_selection()`；`render_search()` 浮层；`/help` 键位行加 `⌃R 搜索历史`。
- **单测**：`ctrl_r_opens_history_search_and_filters_newest_first` / `ctrl_r_enter_fills_the_editor_and_esc_cancels` / `history_search_renders_the_filter_and_matches`。
- **验证**：`cargo fmt` 无差异；`clippy -p lingmiao-tui --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿（lingmiao-tui 107）。

---

## §23 翻页卡顿根治 + 右下角单数字 + 定位提示文案（cli 2026-09-27）

**（cli 2026-09-27）** 三条 UI 反馈：「UI 翻页很卡」/「不需要第 N 轮，只显示一个数字，是当前轮次的上下文」/「回到底部的提示写错了，应该是 ctrl+end」。

### 23.1 翻页卡 —— 根因是「每帧重排整份 transcript」

- **根因（实测，非推断）**：`render_conversation` 每帧都 `history_lines()` → `transcript_lines()` → 对**每个** `Item::Thinking` 调 `thinking_lines()`，而后者对**整段 reasoning** 做 `wrap_cjk`（即使折叠视图只显示末尾 6 行）；随后又对**所有行**再跑一遍 `wrap_line`（每行 `spans→String` 分配 + `display_width`）。
- **实测曲线**（临时 example 打点，140×45 TestBackend，dev profile）：
  - 10 个已提交思考块、每块 ~110K 字符（合计 1.1M 字符，≈ 真实会话的 885K reasoning 字符）：1 块 **51ms** → 3 块 **186ms** → 8 块 **506ms** → 空闲每帧 **562ms**。
  - 滚轮 30 次（每次 3 行）= **13.55s**，即 **451ms / 次翻页**。
  - 3000 行已提交普通行仅 **12.85ms**/帧 → 瓶颈是大块 reasoning + 全量 `wrap_line`，不是行数。
  - 微基准：`wrap_cjk` 43K 中文字符 = 36ms、216K = 73ms；`markdown_lines`(17K 含代码块) = 186ms；流式思考 54K→20ms / 216K→85ms 每帧。
- **修法**：
  1. **提交态转录缓存**（`App::transcript_cache` / `cache_items` / `cache_width` / `cache_folds` + `ensure_transcript_cache`）：把「已提交 items → 折叠展开 → `wrap_line`」的结果**只算一次**，仅在新 item 入队、宽度变化、或 `ctrl+o` 折叠切换时失效/增量追加；每帧只 clone 可见窗口那几十行。
  2. **流式思考只折尾部窗口**（`tail_window`）：折叠视图最多显示 `THINKING_TAIL_LINES`，故只对末尾 `(TAIL+6)×列宽` 字符折行。`tail_window` 从**字符串尾部**按 UTF-8 首字节倒扫（`& 0xC0 != 0x80`），代价 ≤ 4×max_chars 字节（第一版用 `char_indices().nth(n-max_chars)`，在 648K 字符上要 25ms/帧）。
- **实测收益（同一基准脚本）**：10 块 1.1M 字符 → **5.1ms/帧**（原 506ms）；空闲 **5.0ms**（原 562ms）；滚轮 **5.2ms/次**（原 451ms）；流式 648K 思考 **4.2ms/iter**（原 30ms）；流式回答 24K 字符 **6.9ms/iter**（原 ~30ms）。
- **单测**：`transcript_cache_matches_a_full_rebuild_and_grows_incrementally`（缓存 == 全量重排；增量为追加；宽度/折叠变化会失效重建）、`transcript_cache_covers_a_folded_thinking_block`（60K 字符折叠块只产出折叠行）、`tail_window_returns_a_bounded_suffix_at_a_char_boundary`。

### 23.2 右下角上下文行 → 单个数字

- cli：「不需要第 N 轮，只显示一个数字，是当前轮次的上下文」。
- `ctx_summary()` 由 `总 T · 余 R` 改为**只返回 `fmt_tokens(ctx_tokens)`**（如 `8.9K`）——`ctx_tokens` = 最近一次 `工作阶段` 注入的 provider 实测 `input_tokens`。随之删除只服务于「余量」的 `ctx_section_tokens()` / `section_tokens()` 两个私有方法（`ctx_sections` 数据仍采集，供后续用）。
- 位置、颜色（恒 `TEXT_MUTED`，不假设窗口）与「首轮注入前折叠」不变。
- 单测更新：`ctx_line_shows_after_a_turn` / `context_usage_event_populates_segments` / `ctx_summary_stays_within_20_columns_and_has_no_green` / `render_single_column_into_a_test_backend` 改断言单数字。

### 23.3 「回到底部」提示文案 = `ctrl+end`

- 代码里顶/底本就绑 `Ctrl+Home` / `Ctrl+End`（`Home`/`End` 让给了输入框光标，§②），但上翻提示写的是 `End 回到底部` —— **文案与绑定不符**。
- 改为 `↑ 已上翻 N 行 · ctrl+end 回到底部`（`app.rs`），并同步本文档 §11 / §16.3 的实拍注记。
- 单测：`scrolled_hint_names_ctrl_end`。

### 23.4 验证

- `cargo fmt --all --check` EXIT=0；`cargo clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿（lingmiao-tui 115 / lingmiao-engine 39 / lingmiao-llm 44）。
- **computeruse :99 真进程实拍**：xterm 内跑 `LINGMIAO_CONFIG=… ./target/debug/lingmiao` → 启动提示行显示阶段路由；发一轮真实提问（组织上下文检索 → 工作阶段作答）；`PageUp` ×3 后底部显示 `↑ 已上翻 30 行 · ctrl+end 回到底部`；`ctrl+End` 回到底部。翻页与流式均瞬时响应。

---

## §24 鼠标拖选三调整：拖动只选中（不自动复制）· 释放保留选中 · 右键可拖（cli 2026-09-27）

### 24.1 cli 反馈（三条）

- 「**不需要马上复制**，让用户自己选择…用户如果需要自己按快捷键复制」——拖动只负责**选中**，写剪贴板必须是显式按键。
- 「**也不要顶掉**」——松开鼠标不能清掉刚选中的高亮。
- 「**右键拖动也要能选择**」——在捕获鼠标的 transcript 里右键本无其它含义，左/右应等价。
- 附：「**左下角弹出的复制提示顶掉了选择**」——旧版把 `已复制 N 字符` 画在对话区**底部行**，而那正是拖选最常结束的地方，等于用提示盖掉了用户刚做的选择。

### 24.2 做法（`crates/lingmiao-tui/src/app.rs` + `lib.rs`）

| 项 | 旧 | 新 |
|---|---|---|
| 鼠标职责 | `on_mouse` 返回 `MouseAction::Copy(text)`，松开即复制 | `on_mouse` 返回值去掉，**无外部副作用**；`MouseAction` 枚举整体删除 |
| 复制触发 | 松开鼠标 | 显式按键 `ctrl+y`（tmux copy-mode 的 copy 键）+ `shift+insert` 别名 → 新增 `Action::Copy(String)`；未选中时按键**惰性**（不落到编辑） |
| 松开行为 | 复制并清空 `selection` | **保留** `selection`；仅「按下→原地松开」（未移动）才算空选择并清空 |
| 按键 | 仅左键 `Down/Left`、`Drag/Left`、`Up/Left` | 左**或**右：`Down/Drag/Up(Left\|Right)`；面板外按下丢弃旧选择 |
| Esc | 取消回合 / 清输入 | **先丢选择**（不需要的拖选逃生口），再次 Esc 才是原语义 |
| 复制提示位置 | 画在对话区底部行（盖住选择） | 移到 **footer 同一行**左对齐（`render_footer`，与右对齐身份行共行）；宽度不够则不画。`pane_area` 不含 footer，故永不遮挡选择 |
| 文案 | `mouse: 滚轮翻页 · 左键拖动选中对话文字，松开自动复制到剪贴板` | `mouse: 滚轮翻页 · 左/右键拖动选中对话文字，再按 ⌃Y 复制`；键位行加 `⌃Y 复制选中` |

- 新增 `App::selection_text_opt()`（有选择且非空白才返回文本）与 `App::set_copy_note()`（run loop 写剪贴板成功后置提示，状态仍留在 `App`）。

### 24.3 测试（`app.rs`）

- 新增：`mouse_drag_selects_without_copying`、`ctrl_y_copies_the_live_selection_without_clearing_it`、`shift_insert_is_an_alias_for_ctrl_y`、`right_drag_also_selects`、`copy_without_a_selection_is_inert`、`esc_drops_the_selection_before_the_input`、`bare_click_leaves_no_highlight_behind`、`selection_stays_highlighted_after_release`、`copy_note_renders_on_the_footer_not_over_the_pane`。
- 改写：`mouse_drag_selects_and_copies_transcript_text` / `empty_drag_copies_nothing` 被上面取代；`selection_tracks_absolute_lines_…`、跨行选择、点击面板外、滚轮翻页等断言改读 `selection_text_opt()` / `on_mouse` 无返回值。

### 24.4 验证

- `cargo fmt --all --check` EXIT=0；`cargo clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test --workspace` 全绿（lingmiao-tui 134 / lingmiao-engine 39 / lingmiao-llm 44 / lingmiao-memory 24 + 其余）。
- **computeruse :99 真进程实拍**（xterm + `xdotool`）：
  - `/help` 键位行含 `⌃Y 复制选中`，mouse 行含 `左/右键拖动…再按 ⌃Y 复制`；
  - 左键拖选 → 高亮保留、无提示；`ctrl+y` → footer 左显 `已复制 9 字符`，Windows 剪贴板 = `lp`（所见即所得）；
  - **右键**拖选 `/memory` → `ctrl+y` → footer `已复制 10 字符`，剪贴板 = `mory`；
  - 滚轮翻页、输入、斜杠菜单无回归。

---

## §25 上下文超限 bug 簇修复：raw 400 落盘 · ctx 口径订正 · 分类收紧 · 图片 base64 去重复注入（cli 2026-09-28）

### 25.1 cli 反馈与诊断结论

- 「都修」——把 2026-09-27/28 诊断出的四个问题一次修掉（工作阶段此前只诊断未改码）。
- 右下角 `31.5M` 的真相（代码 + 日志实锤）：`StageAgent` 的 `tokens` 是 **工作阶段工具循环里每一次 LLM 往返 input_tokens 的累加**（`add_usage`），图片 base64 秒级重发 + 全量 msgs 每轮重发让它涨到荒谬量级；**「= 最近一次」的旧口径是错的**（用户「是累积的」判断正确）。
- 400 超限疑误报：组织上下文阶段 `input=3117`、沉淀阶段 `input=12777` 也报「上下文超限」——量级不可能超窗口。

### 25.2 四修（代码）

| # | 问题 | 修法 | 落点 |
|---|---|---|---|
| ① | raw 400 body 未落盘 | `http_error` 增 `tracing::error!`，写 status/category/group/model + **原始 body**（截 2000 字符，`truncate_chars`） | `crates/lingmiao-llm/src/client.rs` |
| ② | 右下角 ctx 行口径 | `StageAgent` 另记 `last_input_tokens`（最后一次往返），`tokens` JSON 增 `input_tokens_last`；UI `ctx_tokens` 优先取它，缺省回退旧累加值 | `crates/lingmiao-engine/src/stage_agent.rs` + `crates/lingmiao-tui/src/app.rs` |
| ③ | 分类过宽 | `CTX_OVERFLOW` 收紧为**仅 413 / 明确超限短语**（`maximum context length` / `context length is` / `context_length_exceeded` / `reduce the length of the messages` / `too many tokens` / `context`+`exceed\|overflow`）；其余含 context 的 400 → `BAD_REQUEST` | `crates/lingmiao-llm/src/error_classify.rs` |
| ④ | read_file 图片 base64 双份注入 | 新增 `image::strip_data_url`（把 marker 的 `data_url` 换成短注记）；stage agent 喂模型的 tool 文本与 TUI 预览都用**紧凑 marker**，像素只走多模态附件 | `crates/lingmiao-core/src/image.rs` + `stage_agent.rs` |

- ①的目的：下次再报超限时，日志里能直接看到 provider 的真实 400 body（此前只有分类后的截断文案）。
- ④的量化收益：一次 `read_file` 图片，后续每轮往返不再重发 MB 级 base64。

### 25.3 测试

- 新增 `error_classify::tests::context_word_alone_is_not_overflow`（`context` 单词 400 → BAD_REQUEST；`maximum context length is N tokens` / `context_length_exceeded` → CTX_OVERFLOW）。
- 新增 `image::tests::strip_data_url_removes_base64_but_keeps_the_path`。
- 新增 `app::tests::ctx_line_prefers_the_last_round_trip_over_the_stage_sum`（31.5M 累加 vs 12.8K 最后一次 → 行显 12.8K，session meter 仍记 31.5M）。
- 改写 `stage_agent` 的 `vision_gate_*` 两条：断言 tool 文本**不含 base64**、路径/说明保留。

### 25.4 验证

- `cargo fmt --all` EXIT=0；`cargo clippy --workspace --all-targets -D warnings` EXIT=0；`cargo test -p lingmiao-core -p lingmiao-llm -p lingmiao-engine -p lingmiao-tui` 全绿（lingmiao-core 25 / lingmiao-llm 45 / lingmiao-engine 39 / lingmiao-tui 135）。
- ⚠ 待真机复核：`cargo build` 通过后需 computeruse :99 真进程实拍一轮（发一条带图片的提问，核对右下角数字为万级而非 M 级；并制造一次 400 确认日志出现 `llm http error (raw provider body)`）。

---

## §26 logo 配色改为输入框提示同色（cli 2026-09-28 落地 ✅）

### 26.1 cli 反馈

- **「把 logo 的颜色改成跟输入框提示这里一样的颜色」**——启动横幅左侧的品牌 mark（`▟▀▙` / `▜▄▛`）由暖琥珀桔黄改成**输入框 placeholder 的同一灰**。
- 同轮附带的「**输入框的功能跟 cc 对齐**」为**大改造**（见 §26.3 待办），本轮先落可验收的一项并给出对齐清单。

### 26.2 logo 改色（本次落地）

| 项 | 旧 | 新 | 落点 |
|---|---|---|---|
| `theme::LOGO` | `Rgb(0xF5,0xA6,0x23)` 桔黄（§18 ④，订正 §12.12 红） | **`= TEXT_MUTED`（`#AAAAAA`，与输入框提示同色）** | `crates/lingmiao-tui/src/theme.rs` |

> **已订正（§29，2026-09-28 同日）**：cli 随后要求「logo 颜色改成跟**活动行**一样的颜色」，`LOGO` 现为 **`= ACCENT`（`#D77757`）**；本节的「占位灰」取值作废，令牌别名机制与守卫断言的做法沿用。

- **做法**：`LOGO` 保留为**独立令牌**、值**别名到 `TEXT_MUTED`**（不是删令牌直写灰），这样品牌色日后要再分化不必改调用点；令牌注释与 `app.rs` 的 `push_startup_banner` 文档同步订正。
- **语义**：启动横幅是**对话区留痕**（不是可交互 affordance），故品牌 mark 与占位提示同灰、名字仍用 [`TEXT`] 白——横幅整体读作「历史留痕」而非彩色品牌 splash。
- **守卫**：`header_uses_theme_tokens` 增断言 `LOGO == TEXT_MUTED`（「logo 颜色 = 输入框占位灰」）。
- **验证**：`cargo fmt --all` ✅；`cargo clippy --workspace --all-targets -- -D warnings` ✅；`cargo test --workspace` 全绿（lingmiao-tui 135）。`computeruse :99` 真进程实拍：启动横幅 mark 像素取色为 **`#AAAAAA`**（旧 `#F5A623` 的桔黄像素已从整窗消失），`❯` 提示符仍为 `#D77757`。

### 26.3 输入框功能对齐 CC —— 现状核查与待办（未落地）

对照 CC 2.1.270 输入框的字符串表（`claude.exe` 内取证）与灵妙现状：

| CC 能力 | CC 取证 | 灵妙现状 | 差距 |
|---|---|---|---|
| 空闲提示行 `? for shortcuts` / `/ for commands` / `@ for file paths` / `! for shell mode` | ✅ `"\? for shortcuts"` `"/ for commands"` `"@ for file paths"` `"! for shell mode"`（`bashBorder` 边框变色） | 仅 `/` 补全 + 空闲 tip 一条 | 缺 `?` `@` `!` 三种模式前缀 |
| **`!` shell 模式**（输入框直跑命令） | ✅ `"! for shell mode"`，模式切换时**输入框边框变色**（`bashBorder`） | ❌ 无；跑命令须让模型调 `bash` 工具 | **缺整块**：本地直执 + 边框信号 |
| **`@` 文件提及**（补全路径，注入文件内容） | ✅ `"@ for file paths"` | ❌ 无 `@` 补全 | 缺整块 |
| `?` 快捷键总览 | ✅ `"? for shortcuts"` | 有 `/help`（语义近似，非 `?`） | 键位差异 |
| Shift+Enter 换行 | ✅ `"Shift+Enter for newlines"`（提示安装终端绑定） | ✅ `Shift/Alt+Enter` + `Ctrl+J` | ✅ 已对齐 |
| Enter 排队 | ✅ `"Hit Enter to queue up additional messages while Claude is working."` | ✅ Enter 排队 + `ctrl+x ctrl+s` 立即发送 | ✅ 已对齐 |
| `ctrl+r` 历史搜索 | ✅ `historySearch:*` | ✅ §22 | ✅ 已对齐 |
| `ctrl+x` 抽行 / `ctrl+x ctrl+e` 外部编辑器 | ✅ `"ctrl+x to delete"` `"ctrl+o to expand"` `ctrl+e 外部编辑器` | 折叠有、`ctrl+x` 用作 send-now 前缀 | 部分差异 |
| 多行 / 光标 / 撤销 / 粘贴 | ✅ | ✅ §12.8 + bracketed paste | ✅ 已对齐 |

**结论**：输入框的**行编辑体验已对齐**（§12.8/§22/§24）；真正的缺口是 CC 的**四种输入前缀模态**（`/` `@` `!` `?`）——其中 `!`（本地 shell）与 `@`（文件提及）是**未开工的整块功能**，涉及：
1. 键位与模式状态机（前缀进入/退出、`Esc` 退出模态）；
2. `!`：本地直跑命令 + 结果渲染（不经过 LLM）+ **输入框边框变色**（新增令牌，如 `SHELL_BORDER`）；
3. `@`：路径补全面板（复用 §17 补全骨架）+ 选中后把文件内容作为附件注入（复用 §25 ④ 的 `strip_data_url` 紧凑 marker 思路）。
4. `?`：快捷键总览浮层（复用 `render_search` / `render_completion` 的浮动面板骨架）。

> 该四项改造量级为**一整个特性**，建议单开一轮实现并在 `:99` 真机验收，不在 §26 里做半成品。

## §27 折叠行数回调（工具 3 / 思考 8）· 活动行动态标不稳定修复 + 去 CC 花星 emoji（cli 2026-09-28）

本轮四条，落在 `crates/lingmiao-tui/src/{app.rs,theme.rs}`。

### 27.1 cli 反馈

1. 「控件显示行数改为 3」——非思考类控件（工具返回值）折叠窗口回到 **3 行**。
2. 「思考改为 8」——思考折叠窗口提到 **8 行**。
3. 「再多看看 CC 还有哪些 UI 元素没有对齐」——在 §26.3 的清单上继续核查（见 27.4）。
4. 「橙色的动态行不太稳定修复一下，再去掉里边 claude 那几个花/星星 emoji」——活动行的**前导动态标记**抖动，且要求去掉 CC 的星/花字形。

### 27.2 折叠行数（本次落地）

| 常量 | 旧（§26 前） | 新 | 落点 |
|---|---|---|---|
| `TOOL_OUTPUT_HEAD_LINES` | 4 | **3** | `app.rs` |
| `TOOL_OUTPUT_COLLAPSE_LINES` | 4 | **3**（与 head **同步**，避免「还有 0 行」假提示） | `app.rs` |
| `THINKING_TAIL_LINES` | 6 | **8** | `app.rs` |

- 思考是**唯一**比其它控件显更多的窗口（cli 2026-09-21「完全收起来看着像卡住」的诉求，长思考时最需要活着的感觉）。
- 测试：`long_tool_output_is_collapsed` 增断言 `TOOL_OUTPUT_HEAD_LINES == 3` 且 `COLLAPSE == HEAD`；`thinking_collapsed_shows_header_and_live_tail` 增断言 `THINKING_TAIL_LINES == 8`，并把夹具加长到 600 个数（尾部 8 行才是严格子集）。

### 27.3 活动行动态标：CC 星形族 → 旋转弧（根因取证）

**根因不是逻辑，是字形覆盖**——`cc_spinner()` 以 120ms 步进本身没问题，问题在它循环的字形在**本机终端字体里不存在**：

```
$ fc-list :charset=2722 family | grep -ci 'Noto Sans Mono'   # ✢ → 0
$ fc-list :charset=2733 family | grep -ci 'Noto Sans Mono'   # ✳ → 0
$ fc-list :charset=273B family | grep -ci 'Noto Sans Mono'   # ✻ → 0
$ fc-list :charset=273D family | grep -ci 'Noto Sans Mono'   # ✽ → 0
```
- 四个星形字形**只在 DejaVu Sans Mono 有**，Noto Sans Mono（本机默认等宽）**一个都没有**；`·`(U+00B7) 与 `✽`(U+273D) 的 East-Asian Width 还是 **Ambiguous**（部分 locale 下按 2 列算）。
- 于是终端从**比例字体 fallback** 里取替补字形：六个「不同」的帧渲染成**几乎一样的星号**、且**前进宽度每次不同** → 视觉上就是「橙色动态行不稳定」。
- :99 快速截图连拍实测：`✶`/`✻`/`✽` 三帧像素完全一致（同一张位图），字形没有真正切换。

**修法**：换成一族**每一帧都是 1 列宽、且两个等宽字体都覆盖**的旋转弧 `◜ ◝ ◞ ◟`（U+25DC/25DD/25DE/25DF，EAW 全为 Narrow；`fc-list :charset=` 对每个码点都命中 Noto Sans Mono + DejaVu Sans Mono）。同时**尾部标记**改用同族的 `◜`（`TAIL_MARK`），替掉原来的 `✻`——尾部读作「转盘停在这一帧」。

| 令牌 | 旧 | 新 |
|---|---|---|
| `theme::STAR_SPINNER` | `['·','✢','✳','✶','✻','✽']` | 删除，改 `theme::ACTIVITY_SPINNER = ['◜','◝','◞','◟']` |
| `theme::TAIL_MARK` | （无，尾行硬写 `✻`） | **`'◜'`**（= 弧的第 0 帧） |
| 尾行 / 活动行 / 相关文档 | `✻ {动词} …` | `◜ {动词} …` |

- 测试：`activity_spinner_cycles_through_the_arc_frames` 断言循环、**每帧 `display_width == 1`**、且不含 `·|✽|✢|✳|✻|✶`；`tail_mark == ACTIVITY_SPINNER[0]`。
- **验证**（`:99` Xvfb + xterm 真进程）：连拍 24 帧活动行首格像素，位图哈希出现 **3 种不同帧**（`◜ ◝ ◟` 均被捕捉到），而修复前是**同一张星号位图**；尾行实拍为 `◜ Formulating for 17.5s · done 01:51:22`。

### 27.4 CC UI 对齐——本轮继续核查（仍未落地）

在 §26.3 输入框清单之外，再从 `claude.exe` 2.1.270 字符串表取证，补充的差距：

| CC 能力 | CC 取证 | 灵妙现状 | 差距 |
|---|---|---|---|
| `shift+tab` 权限模式循环（auto-accept / plan / auto） | ✅ `"shift+tab"` ×15、`"shift+tab, plan, auto"`、`"auto-accept edits"` | ❌ 无（灵妙无权限模式层，Q2 已砍模式层） | 设计上有意不做 |
| 双击 `esc` rewind（回滚对话/代码） | ✅ `"Double-tap esc to rewind the conversation to a previous point in time"` | ❌ 无 | 缺整块（对话回滚） |
| `ctrl+t` 任务/子代理面板 · `/tasks` | ✅ `"ctrl+t"`、`"/tasks to see subagents"` | ❌ 无 | 缺（灵妙无子代理） |
| `ctrl+_` undo · `ctrl+z` undo | ✅ `"chat:undo" ctrl+_`、`"undo ctrl+z"` | ✅ `⌃Z` 撤销输入 | 已对齐（CC 的 `ctrl+_` 是另映射） |
| `alt+p` 切模型 · `alt+o` fast mode · `alt+t` thinking toggle | ✅ `chat:modelPicker alt+p` 等 | `/model` 命令；无 alt 快捷键 | 键位差异 |
| `ctrl+g` 外部编辑器 · `meta+j` | ✅ `chat:externalEditor ctrl+g` | ❌ 无 | 缺（`$EDITOR` 外跳） |
| `/statusline` 自定义状态行 | ✅ `"Use /statusline to set up a custom status line"` | ❌ 无 | 缺 |
| 粘贴大图「再粘贴一次展开」 | ✅ `"paste again to expand"` | bracketed paste 有，无展开提示 | 部分差异 |
| `? for shortcuts` 快捷键总览浮层 | ✅ `"? for shortcuts"` | 仅 `/help` | 键位差异（§26.3 已记） |

**结论**：真正**未对齐且值得做**的是 `!`（本地 shell 直跑 + 边框变色）、`@`（文件路径补全）、`?`（快捷键浮层）、双击 `esc` rewind、`ctrl+g` 外跳编辑器；`shift+tab` 权限模式 / `ctrl+t` 子代理面板属 CC 特有模式层，灵妙 Q2 已决策不做。仍建议**单开一轮**逐项落地并在 `:99` 验收。

### 27.5 验证

- `cargo fmt --all` ✅；`cargo clippy --workspace --all-targets -- -D warnings` ✅。
- `cargo test --workspace` 全绿：lingmiao-core 25 · lingmiao-memory 39 · lingmiao-llm 45 · lingmiao-engine 38 · lingmiao-tools 81 · **lingmiao-tui 135** · lingmiao 1。
- `computeruse :99` 真进程实拍：折叠行数（工具卡 `… 还有 N 行 · ctrl+o 展开`）、思考尾部 8 行、活动行 `◜ … ◝ …` 三帧轮转、尾行 `◜ {动词} for Ns · done HH:MM:SS` 逐项核对通过。

## §28 退出键改「连按两次 Ctrl+C」+ 默认 win/linux 复制粘贴（cli 2026-09-28）

cli 反馈：

> 「把退出快捷键改一下，参考 CC 按一下 ctrl+c 我本来想复制直接就退出去了，这个组合键要按两次，第一次按下后给提示。默认支持 win 和 linux 的复制粘贴。」

### 28.1 CC 取证（`claude.exe` 2.1.270 字符串表）

| 项 | CC 原文 / 实现 |
|---|---|
| 待退提示 | `press ctrl+c or q again to exit` / `Press Ctrl-C again to exit` |
| 双击窗口 | `function zO(t,c,o,s=D)` … `var D=800` —— 距上次 ≤**800ms** 则**执行**（退出），否则 arm 并起 800ms 定时器 |
| Global 键位 | `ctrl+c: app:interrupt`、`ctrl+d: app:exit`（`ctrl+c` 不可重绑，`Cannot be rebound`） |
| 复制选中 | `Scroll` 上下文 `ctrl+shift+c` / `cmd+c: selection:copy`（**不是**裸 `ctrl+c`） |
| 粘贴 | `Chat` 上下文 `ctrl+v: chat:imagePaste`（Windows/WSL 为 `alt+v`）；文本粘贴走终端 bracketed paste |

结论：CC 的裸 `ctrl+c` 语义是 **interrupt（先中断，空闲时 arm 退出）**，复制选中另有键位（`ctrl+shift+c`）。灵妙的映射是「**有拖选 → 复制**，无拖选 → **arm 退出**」，两者互斥、复制优先；窗口按 cli 要求取 CC 的 **800ms**。

### 28.2 落地：Ctrl+C 三态（本次落地）

| 状态 | 条件 | 行为 |
|---|---|---|
| ① 关闭历史搜索 | `ctrl+r` 浮层打开 | 关浮层（CC `historySearch:cancel`），不 arm、不退出 |
| ② 复制选中 | 有应用内拖选 | `Action::Copy`，**只复制**：不退出、不 arm、不清选中 —— 这就是用户「本想复制」的那条路 |
| ③ arm / 退出 | **无**选中 | 首按 arm + 页脚提示 `再按一次 ⌃C 退出`；800ms 内再按 → `Action::Quit` |

**② 与 ③ 互斥，且 ② 优先**：只要还有高亮，`⌃C` 就一直是「复制」。这是刻意的 —— 若复制也 arm 退出，两次「再复制一次」就会把会话关掉；若复制改成 arm，用户又得先研究「怎么把高亮消掉」（正是这次要修的发现性问题）。因此**有高亮时退出需要先 `Esc` 丢掉选择**（`Esc` 本就是选择的逃生口，§24），`/help` 与页脚提示都写明了这一点。**灵妙绝不用退出静默换掉一次复制。**

- 常量 `EXIT_ARM_SECS = 0.8`（对齐 CC 的 `D=800`）、`EXIT_ARM_HINT = "再按一次 ⌃C 退出"`。
- **任一其它键 disarm**（`on_key` 里 `ctrl+c` 分支之后统一 `self.exit_armed = None`）：第二次 Ctrl+C 必须是紧接着的一下，打字/翻页都算「我不是要退出」。
- **到期也 disarm**：`render()` 每帧调 `expire_exit_arm()`（跑循环 ~10 帧/秒，不按键也刷新），提示不会滞留。
- 提示画在 **footer 左段**（与 `已复制 N 字符` 同一槽位），优先级高于复制提示 —— 待退不能被别的提示遮住；用 `WARN` 黄色以示「待确认」。

### 28.3 落地：Ctrl+V 粘贴（win / linux / macOS）

- 新增 `Action::Paste`（无载荷）：`on_key` 只产生意图，**读剪贴板是 I/O，由跑循环做**，再回灌 `App::paste()`（`App` 保持纯函数）。
- 新增 `clipboard::paste()` + `run_capture()`：按与 `copy()` 相同的通道顺序读回 —— Windows/WSL `powershell.exe -NoProfile -Command "[Console]::OutputEncoding=[Text.Encoding]::UTF8; [Console]::Out.Write((Get-Clipboard -Raw))"`（与 `clip.exe` 读的是**同一块**宿主剪贴板，且强制 UTF-8 让 CJK 不变乱码。⚠ 用 `Out.Write` 而非裸 `Get-Clipboard -Raw` —— 后者经 PowerShell 输出格式化器会补 `\r\n`，见 §34）→ macOS `pbpaste` → Wayland `wl-paste --no-newline` → X11 `xclip -selection clipboard -o` → `xsel --clipboard --output`。
- 键位：`ctrl+v`（并接受 `cmd/super+v`）、`shift+insert`（X11/WSL 经典粘贴键）。
- **补上 `EnableBracketedPaste` / `DisableBracketedPaste`**：`ratatui::init()` 不开启 bracketed paste，而 `lib.rs` 的 `TermEvent::Paste` 分支从 §6 痛点⑤ 起就存在 —— 未开模式时该分支是**死代码**，粘贴会被终端当**裸键**逐字送入（多行粘贴每到换行就发一轮）。这是「默认支持粘贴」的必要一环。
- `shift+insert` 原来是 `⌃Y` 的**复制**别名（§24），本轮改为**粘贴**（它本就是 X11 的粘贴键）；复制保留 `⌃Y` 与「有选中时的 `⌃C`」两条。

### 28.4 连带文案

- `App::push_help()` 键位行：新增独立一行 `⌃C 无选中时连按两次退出（第一次给提示）；有拖选时 ⌃C 只复制（先 Esc 丢选择再按两次才退出）`，同一行补 `⌃V 粘贴`；mouse 行改为「再按 ⌃Y 复制（有选中时 ⌃C 亦可复制）」。
- `crates/lingmiao-core/assets/help.json` 的 `commands` 主题：补齐 `Ctrl+C / Ctrl+Y / Ctrl+V / Ctrl+O / Ctrl+R / Shift+Insert` 与选择说明。
- `run.sh` 启动提示改为「连按两次 Ctrl+C 退出」。
- **订正旧节**：§11 / §6 痛点⑤ 的实现说明里「剪贴板因 Ctrl+C 已绑定『退出』（Q3）」一句已被本轮取代 —— `⌃C` 不再无条件退出：**有拖选即复制**、**无拖选才 arm 退出**。

### 28.5 测试（`lingmiao-tui`，132 → **143**）

- 新增：`ctrl_c_arms_then_quits_on_the_second_press`、`ctrl_c_arm_expires_so_a_later_press_only_re_arms`（回填 `exit_armed` 到 900ms 前，不 sleep）、`any_other_key_disarms_the_pending_exit`、`ctrl_c_with_a_selection_only_copies_never_quits`（含「两次复制也不退出」「`Esc` 丢选择后恢复 arm→退出」）、`ctrl_c_closes_an_open_history_search`、`ctrl_c_hint_renders_on_the_footer`、`ctrl_v_pastes_from_the_clipboard`、`shift_insert_pastes_and_ctrl_y_is_the_copy_key`；`clipboard` 增 `run_capture_returns_none_when_the_helper_is_missing`、`run_capture_returns_stdout_of_a_successful_helper`。
- 删除：`ctrl_c_quits`（单按即退的旧语义）、`shift_insert_is_an_alias_for_ctrl_y`（键位已改）。

### 28.6 验证（`:99` Xvfb + xterm 真进程，`claude.exe` 同款 xdotool 操作）

- ✅ 单按 `ctrl+c` + 等 2.5s：窗口仍在（**未退出**），页脚黄色 `再按一次 ⌃C 退出`
- ✅ 提示 800ms 后自动消失（截图确认页脚恢复）
- ✅ 间隔紧连两按 `ctrl+c` → 进程退出、窗口消失
- ✅ 鼠标左键拖选横幅文字 → 页脚高亮 → `ctrl+c` → 宿主剪贴板得到 `lingmiao`，**窗口仍在**（有选中时 ⌃C 只复制），且连按两次也不退出
- ✅ 该场景下 `Esc` 丢选择后再连按两次 `ctrl+c` → 退出（选择是「退出」的显式闸门）
- ✅ `ctrl+v` → 输入框出现剪贴板文本 `粘贴测试中文PASS`（CJK 无乱码）
- ✅ `/help` 键位行显示新文案
- `cargo fmt` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 全绿：lingmiao-core 25 · lingmiao-memory 38 · lingmiao-llm 45 · lingmiao-engine 39 · lingmiao-tools 81 · **lingmiao-tui 143** · lingmiao 1。
- 版本：**v0.12.13**（`Cargo.toml` 的 `version` 已同步；UI 显示 0.12.13，`:99` 启动横幅实测）。

## §29 logo 改活动行同色 + 代码改动红绿对比（cli 2026-09-28）

cli 要求：

> 「把 logo 颜色改成跟活动行一样的颜色。另外参照 CC 实现代码改动时的红绿对比格式显示样式，包括写入的时候也是。」

### 29.1 CC 取证（`claude.exe` 2.1.270 字符串表）

| 项 | CC 原文 / 实现 |
|---|---|
| 活动行取色 | CC 的 `rubric`/状态行由主题令牌绘制；灵妙侧此前已把活动行定为品牌橙（[`ACCENT`]） |
| diff 主题令牌 | `diffAdded:"rgb(105,219,124)"` · `diffRemoved:"rgb(255,168,180)"` · `diffAddedDimmed` / `diffRemovedDimmed` · `diffAddedWord:"rgb(47,157,68)"` / `diffRemovedWord:"rgb(209,69,75)"`；**ANSI 主题**映射为 `diffAdded:"ansi:green"` / `diffRemoved:"ansi:red"` / `diffAddedWord:"ansi:greenBright"` / `diffRemovedWord:"ansi:redBright"` |
| diff 数据结构 | `structuredPatch: [{oldStart, oldLines, newStart, newLines, lines:[{type:"+"/"-"/" ", content}]}]`，由 `NAe({filePath, oldContent, newContent, convertTabs:true})` 生成；`Edit` / `Write` 的 **tool_use_result** 都带它 |
| 计数渲染 | `Dg({added, removed, bold})` → `+N` 用 `diffAddedWord`、`-M` 用 `diffRemovedWord`，中间一个空格；为 0 的一侧不画 |
| 行颜色 | `N(type, theme)`：`"+"→addLine`、`"-"→deleteLine`、`" "→背景`；`he()` 把 `diffAdded`/`diffRemoved`（必要时 `*Dimmed`）映射到 `addLine`/`deleteLine` |
| 模型侧正文 | `Write` → `File created successfully at: X` / `The file X has been updated successfully.`；`Edit` → `The file X has been updated successfully.` —— **短句**，patch 只在 UI |

结论：CC 的「红绿对比」= **UI 专用的 `structuredPatch`**（模型侧只有短句），颜色取 **ANSI green/red**（其 ANSI 主题）/ 固定 RGB（其 truecolor 主题）。

### 29.2 落地①：LOGO = 活动行同色（`theme.rs`）

- `theme::LOGO` 由 `= TEXT_MUTED` 改为 **`= ACCENT`**（CC 品牌橙 `#D77757`）——即**活动行**（`◜ {动词}… Ns · esc 中断`）与 `❯` 提示符的同一份墨。保留 `LOGO` 独立令牌，值走别名，日后要分化不必改调用点。
- 守卫测试：`header_uses_theme_tokens` 断言 `LOGO == ACCENT`，并在取色断言里要求帧内出现 `ACCENT`。
- `:99` 实测（xterm + 启动横幅截图取色）：logo 像素为 **`#D77757`**，与活动行同值（此前为 `#AAAAAA`）。

### 29.3 落地②：代码改动红绿 diff（引擎 + 工具 + TUI 三层）

**新增纯函数模块 `crates/lingmiao-tools/src/diff.rs`**（零依赖、可单测）：

- `unified_diff(old, new) -> FileDiff{lines:[{kind,text}], added, removed}`：
  - 先**裁掉公共前后缀**（每次编辑都保留文件主体），再对剩余中间段做**精确 LCS 行 diff**（`LCS_CELL_BUDGET = 250k` 格）；超过预算则退化为「全删 + 全加」（仍合法，只是不最小）。
  - 单 hunk 输出：`@@ -a,b +c,d @@` + 3 行上下文（`CONTEXT = 3`，git 默认）；纯新建/清空时起点按 git 约定写 **`0`**（`@@ -0,0 +1,3 @@`）。
  - **计数是完整改动量**（`+N -M`），显示行数另有硬顶（`MAX_DIFF_LINES = 200`，超出折叠为一行说明）；单行字符也设顶（`MAX_LINE_CHARS = 240`），防止一行压缩包刷爆卡片。
- `FileDiff::stat()` → `+2 -1` / `+3` / `-2`；`to_json_lines()` → 事件载荷。

**`ToolOutput` 增 `diff: Vec<DiffLine>`**（`tool.rs`）：只有 `edit` / `write_file` 填，其余工具为空；`ToolOutput::with_diff()` 挂载。**模型侧正文不变**（仍是 `Wrote N bytes to X` / `Edited X (1 replacement)`）——与 CC 的「patch 只在 UI」一致。

**`file_tools.rs`**：

- `write_file`：写前读一次旧内容（`GuardedWrite` 已强制「先读后写」）→ `unified_diff(old, new)`；全新文件即纯新增。
- `edit`：对 `current → updated` 求 diff（即这次替换的真实行改动）。
- 二者都 `ToolOutput::ok(短句).with_diff(diff.lines)`。

**事件层**：`Event::ToolCalled` 增 **`diff: Value`**（`[{"kind":"add"|"remove"|"context"|"hunk","text":"…"}]`），`to_json()` 一并序列化；`lingmiao-engine/stage_agent.rs` 把 `ToolOutput.diff` 映射进该字段。

**TUI（`app.rs` + `theme.rs`）**：

- 新增令牌 `DIFF_ADD = Green` / `DIFF_REMOVE = Red` / `DIFF_CONTEXT = TEXT_MUTED` —— 取 CC 的 **ANSI** 主题映射（灵妙是 terminal-native、不绘背景，固定 RGB 只适配暗色底）。widget 层仍无裸 `Color::`。
- `Item::ToolOutput` 增 `diff: Vec<Line>`；`push_tool_card` 在 **header 后、`⎿` 结果前**渲染 diff：`  +text` 绿、`  -text` 红、`  text` 灰上下文、`  @@…@@` 灰；长行按同一 CJK 感知宽度折行、续行缩进。
- header 增 CC 的 **`+N -M`** 统计（`+` 绿、`-` 红），即 `edit(f.rs) +1 -1  3.1ms`。
- diff **恒显**（其长度已由数据源封顶），不参与 `ctrl+o` 的 3 行折叠——折叠只作用于 `⎿` 结果体，原语义不变。
- `parse_diff()` 丢弃未知 `kind`（向前兼容），载荷缺失即空 diff（其余工具的卡片形状完全不变）。

### 29.4 测试

- `lingmiao-tools`（81 → **92**）：`diff` 模块 11 条（相同文本无 diff、尾换行不算改、单行替换、新建纯加、清空纯删、上下文裁到 3 行、两处改动保留中段上下文、超大改写封顶但计数完整、单行字符封顶、JSON 载荷、`@@` 起点 `0`）；`edit_and_write_return_a_display_diff`（模型侧仍是短句 + diff 三类行齐备 + 全新文件纯加 + 同内容重写无 diff + 读工具无 diff）。
- `lingmiao-tui`（143 → **147**）：`file_mutation_card_paints_the_diff_green_and_red`（`+`/`-`/上下文/`@@` 各自取色 + header 统计红绿）、`write_file_card_also_shows_a_diff`、`non_file_tools_show_no_diff`、`parse_diff_ignores_unknown_kinds_and_bad_payloads`；`header_uses_theme_tokens` 改断言 `LOGO == ACCENT`；令牌测试补 `DIFF_*`。

### 29.5 验证（`:99` Xvfb + xterm 真进程 + 真实 LLM 回合）

- ✅ `edit` 实拍：header `edit(scratch_diff_demo.txt) +1 -1  9.0ms` → `@@ -1,2 +1,2 @@` / `-hello A`（红）/ `+hello B`（绿）/ `  keep line`（灰）→ `⎿ Edited … (1 replacement)`
- ✅ `write_file` 实拍：header `write_file(scratch_diff_demo.txt) +3 -2  7.6ms` → 两行红 + 三行绿 → `⎿ Wrote 29 bytes to …`
- ✅ 截图像素统计（裁到卡片区域）：绿色像素 624 / 红色 778，header 统计位 `+3` 绿 43 / `-2` 红 119 —— 红绿确实上屏
- ✅ logo 取色 `#D77757` == 活动行 `#D77757`
- `cargo fmt` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 全绿：lingmiao-core 25 · lingmiao-memory 39 · lingmiao-llm 45 · lingmiao-engine 38 · lingmiao-tools **92** · lingmiao-tui **147** · lingmiao 1。
- 版本：**v0.12.14**（`Cargo.toml` 同步；UI 横幅实测 0.12.14）。

---

## §30 右下角数字改「当前 LLM 调用的输入 tokens」（cli 2026-09-28）

### 30.1 cli 反馈

> 「上下文显示那里还是不对，你显示当前 llm 的输入 tokens 就行」

### 30.2 诊断（先查再动：代码 + `:99` 真回合 + 日志对账）

| 现象 | 实证 |
|---|---|
| **只在工作阶段结束才刷新** | `app.rs` 的 `ctx_tokens` 只在 `Event::StageResultReported{stage=="工作阶段"}` 分支里赋值。回合进行中——组织上下文正在调用、工作阶段正在流式——行上仍是**上一轮**的数字。`:99` 实拍：回合 +4s（组织上下文阶段中）显 `9.3K`，正是上一轮 工作阶段 的 `input_tokens_last`；+16s（工作阶段结束）才跳到 `9.5K`（本轮 工作阶段 的）。 |
| **只认工作阶段** | `stage != "工作阶段"` 一律不更新：组织上下文检索自己的调用、沉淀阶段的调用从不上屏。 |
| **`llm_response` 是死变体** | `Event::LlmResponse`（M0 就有、原版 `stage_agent.py:453` 的 `self.ql.events.push(resp)` 一比一）在 Rust 版**从未被 push** —— 内层循环对 UI 完全沉默，所以「每次往返」的 usage 根本没进总线。 |

### 30.3 修法：每次 LLM 往返都上报（原版语义）

- `stage_agent.rs`：循环内 `add_usage(&mut total, &resp.usage)` 之后新增
  `self.bus.push(round_trip_event(stage, &resp))` —— 补齐原版的 `events.push(resp)`。
  新增纯函数 `round_trip_event()`：**只带 usage**（`input_tokens` / `output_tokens` /
  `total_tokens` / `finish_reason` / `tool_count`），正文与工具载荷置空 —— 那些已经
  由 `LlmDelta` 逐块流过、并由 `record_turn` 全量入档，再抄一份只会让长 C 循环的
  事件日志按往返次数成倍膨胀。
- `app.rs`：新增 `Event::LlmResponse { usage, .. }` 分支 —— `usage.input_tokens > 0`
  时 `ctx_tokens = usage.input_tokens`（**任何阶段**、**每次往返**都更新）。
- `StageResultReported` 里的 `input_tokens_last` 保留为**兜底**（老载荷 / 传输层没报
  usage 的阶段），语义不变。

### 30.4 语义

右下角那一个数字 = **「刚刚这次 LLM 调用送进去多少 tokens」**（provider 实测
`usage.input_tokens`，即 `deepseek` 的 `prompt_tokens`）。组织上下文检索的调用、工作阶段工具循环的
每一次往返、沉淀阶段的调用，都各报一次，行上数字随之实时跳动；转轮次不累计、不假设
窗口、无标签、恒中性灰（承 §21.2 / §23.2）。

### 30.5 测试

- `lingmiao-engine`（39 → **40**）：`round_trip_event_carries_the_usage_of_one_call`
  （线上名 `llm_response`、`usage.input_tokens` 原样带出、`tool_count` 保留、正文/
  工具载荷确为空）。
- `lingmiao-tui`（147 → **148**）：`ctx_line_follows_every_llm_round_trip_of_any_stage`
  —— 组织上下文的调用即上屏（旧代码忽略非工作阶段）→ 工作阶段首往返 `9.3K` → 同一工具循环的后续
  往返 `9.5K` 覆盖 → 沉淀阶段 `8.9K` → `input_tokens=0`（provider 未报）保持原值；
  并断言会话累计 `tokens_in` 不被逐往返事件影响。

### 30.6 验证（`:99` Xvfb + xterm 真进程 + 真实 LLM 回合）

真回合「只回答两个字：你好」，右侧数字逐帧实拍（裁图像素读值）：

| 时刻 | 行上数字 | 对照日志 `llm_response` |
|---|---|---|
| +2s | `3.1K` | B 首次往返 `input_tokens=3121` |
| +4s | `18.1K` | B 第 2 次往返 `18086` |
| +8s | `28.5K` | B 第 3 次往返 `28452` |
| +10s | `9.6K` | C 往返 `9606` |
| +12s | `4.8K` | 沉淀阶段第 1 次往返 `4790` |
| +14s | `4.8K` | 沉淀阶段第 2 次往返 `18859`（该帧为结束后稳态） |

—— 每个数字都对得上「刚发生的那次调用」，且**在回合进行中**就跳变（旧实现全程只会显
示上一轮的值）。日志同轮共 **6** 条 `llm_response`（此前为 0）。

`cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings` /
`cargo test --workspace` 全绿。版本 **v0.12.15**（`Cargo.toml` 同步；UI 横幅实测 0.12.15）。

---

## §31 工具结果回灌上限：修「沉淀阶段 HTTP 400 [上下文超限]」（cli 2026-09-28）

### 31.1 cli 反馈

> （cli 附图，未随仓库分发）—— 末行红字 `✗沉淀阶段失败：HTTP 400 [上下文超限] —— 长程任务常见：上下文累积超限。建议新开对话/精简上下文，或缩短本轮工具结果。` 「这个问题解决一下」

### 31.2 诊断（先查再动：日志 raw body + DB 实测 + 原版对照）

截图那一轮的日志 `.cache/lingmiao/logs/lingmiao-20260928-054725.jsonl` 直接把因果钉死：

| 时刻 | 事实 |
|---|---|
| 05:52:16 | 沉淀阶段往返**成功**，`input_tokens=62719` |
| 05:52:17 | 模型调 `search_knowledge` + `search_observations` + **`list_archive(limit=6)`** |
| 05:52:22 | 下一次请求的 raw 400 body：`requested 1622701 tokens (max 1048576)` → `context_overflow` |

真凶 = **`list_archive`**。本地 `.memory/context_record.db` 最近 6 条 turn 的 pretty-JSON 合计
**4,365,055 字符 ≈ 145 万 token**（单条 `turn-388a5679f9ed` 的 `tool_calls` 1,438,784 字符、
`reasoning` 2,071,863 字符）。

根因是**投影丢失**：`#1 archive` 的 `Turn` 是审计/回放用的全量结构（`system_prompt` /
`context_prefix` / `full_messages` / `tool_calls` / `reasoning` 全列），而 `lingmiao-tools` 的
`list_archive` / `search_archive` 用 `to_output(&rows)` **原样序列化整行**后回灌模型。同批
`search_memory` 无此问题——它有 `SNIPPET_CHARS = 240` 截断，所以「同样大小的记忆库，
分层工具炸、聚合工具不炸」。

**原版对照**（原实现 → `tools/builtin/registration.py`）：Python 版每个记忆投影
都硬截断，`list_archive` / `search_archive` / `list_observations` 一律
`f"[{at[:16]}] {user_msg[:300]}"`、`search_knowledge` 用 `summary[:300]`。Rust 移植把「紧凑投影」
退化成「整行序列化」——这是**偏离原版**带来的回归。

**兜底缺口**：`stage_agent.rs` 的 `RESULT_PREVIEW_CHARS = 2000` 只截 **UI 预览**；喂给模型的
`out_content` 此前**无任何上限**，任何自身上限的工具（bash/grep 另有 64 KiB cap）都会重演。

### 31.3 修法（两层：工具层投影 + 引擎层兜底）

| # | 层 | 修法 | 落点 |
|---|---|---|---|
| ① | 工具层（治本） | 记忆只读工具一律返回**投影**，不返回存储行：新增 `turn_view` / `observation_view` / `node_view` + `RECORD_BODY_CHARS = 300`（对齐原版 `[:300]`）。`turn_view` 只留 `id/at/user_msg/assistant/summary/tokens_total`——`full_messages` / `tool_calls` / `reasoning` / `system_prompt` / `context_prefix` **一律不出**；id 与时间保留，模型仍可精确指认某条记录 | `crates/lingmiao-tools/src/memory_tools.rs`（`list_archive` / `search_archive` / `list_observations` / `search_observations` / `search_knowledge`） |
| ② | 引擎层（兜底） | 单条工具结果回灌模型前加**字符上限** `TOOL_FEED_MAX_CHARS = 64 KiB`（与 `BASH_OUTPUT_CAP` / `GREP_OUTPUT_CAP` 同量级）。截断时附显式提示：结果被截断 **+ 如何重取**（`head_limit` / `offset` / 更窄关键词），避免模型把半份结果当完整结论 | `crates/lingmiao-engine/src/stage_agent.rs`（新纯函数 `cap_tool_feed`，在 `resolve_tool_media` 之后、`Message::tool` 之前） |

- ①是治本：让工具**本就不该吐的东西**不吐。②是兜底：任何**将来**新增/忘记自限的工具都不会
  再把一条结果放大成整轮预算。
- 图片路径不受②影响：`cap_tool_feed` 作用在 `resolve_tool_media` **之后**，像素仍走多模态附件，
  被截的只是文本副本（与 §25 ④ 同向）。

### 31.4 量化收益（本地真实记忆库实测）

| | 修复前 | 修复后 | 倍数 |
|---|---|---|---|
| `list_archive(limit=6)` 回灌字符 | **4,365,055** (≈145 万 token) | **3,340** (≈1.1 千 token) | **≈1307×** |

### 31.5 测试

- `lingmiao-tools`（92 → **94**）：
  - `archive_projections_stay_bounded` —— 造 6 条「真档规模」turn（单条 ~4.2 MB：`system_prompt`
    120K + `context_prefix` 120K + `full_messages` 500K + `tool_calls` 1.4M + `reasoning` 2M），
    断言 `list_archive` / `search_archive` 输出 **< 20,000 字符**、含 `id`、且**不含**
    `system_prompt` / `full_messages` / `tool_calls` / `reasoning` 四个列名。
  - `memory_projections_keep_ids_and_bound_bodies` —— 投影**仍然有用**：`obs-` id 保留、超长
    body 被截到 `RECORD_BODY_CHARS` 并以 `…` 标记（不是静默变短）。
- `lingmiao-engine`（40 → **41**）：`tool_feed_cap_bounds_one_result_and_tells_the_model` —— 短结果
  原样通过；2× 上限的输入被截到上限附近，且带上「已截断 / 重新调用」提示。

### 31.6 验证（`:99` Xvfb + xterm 真进程 + 真实 LLM 回合）

真进程 `灵妙 v0.12.15 · deepseek-v4-flash-vision-exp`（项目根 `~/ai/lingmiao`）跑两轮：

| 轮 | 指令 | 结果 |
|---|---|---|
| 1 | 「回顾我们最近几轮在灵妙 TUI 上做过的改动，列出要点」 | 组织上下文 → 工作阶段（23 次工具调用）→ **沉淀阶段 ok=true**（`input_tokens_last=21331`）。日志 `context_overflow` 计数 **0** |
| 2 | 「调用 list_archive limit=6 把最近 6 轮对话列出来」 | 沉淀阶段**实际调用了 `list_archive(limit=6)`**（日志 `tool_called` 现形），随后 `RESULT ok=True`（`input_tokens_last=24743`，整段累加 109,564）；**未复发 400**，`context_overflow` 计数仍 **0** |

第 2 轮是**靶向复现**：同一工具、同一参数，正是截图那轮崩溃的调用——修复后同一路径通过。

`cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings` /
`cargo test --workspace` 全绿。版本 **v0.12.16**。

---

## §32 阶段名规范化：去 Python 老术语 → 组织上下文 / 工作阶段 / 沉淀阶段（cli 2026-09-28）

> cli：「改一下阶段名 组织上下文 工作阶段 沉淀阶段」+「1、并入沉淀阶段 2、可以（旧键兼容）」。

### 32.1 拍板

| 旧名（Python 老术语） | 新名 |
|---|---|
| `B-上下文选择` | **组织上下文** |
| `C-对话` | **工作阶段** |
| `总结流程` | **沉淀阶段** |
| `I-知识图谱更新` | **并入 `沉淀阶段`**（不再单列阶段名） |

- 一律**纯中文、去字母前缀**（彻底清掉 Python 时代 `字母-中文` 混写的 A–J 残留）。
- `沉淀阶段` 正好等于 TUI 已有的进度标签「沉淀中」（`app.rs::human_stage`），上下一致。
- 第 4 段原 `I-知识图谱更新`（纯算法、无 LLM、无 `stages.json` 行）**并入沉淀阶段**：`STAGE_I_KG`
  归并进 `STAGE_CONSOLIDATE`，`stage_i()` 的 `stage_started`/`stage_result_reported` 上报
  `沉淀阶段` —— 管线收敛为 **三段**，`mg_updated` 事件不变。

### 32.2 关键：旧键兼容（唯一破坏点）

`config.json` 的 `stages` 路由与外部覆盖目录（`LINGMIAO_CONFIG_DIR`）里的阶段键，改名后会**精确查表失配
→ 静默回退默认模型、不报任何错**（最坏：用户以为在跑 Kimi，其实跑默认 DeepSeek）。

- 新增别名表 `config.rs::LEGACY_STAGE_ALIASES` + `canonical_stage()`（`B-上下文选择`/`C-对话`/`总结流程`
  → 新名；`I-知识图谱更新` → `沉淀阶段`）。
- 所有按名读取都过它：`Config::prompt/stage/core_lock/has_pipeline`、`UserConfig::stage`、
  `build_stage_clients`。老用户零感知。

### 32.3 落点（实测 189 处 / 26 文件）

`config.rs` 的 `STAGE_*` 常量、`stages/prompts/locks/help.json` 的键、`engine.rs` 的 `STAGE_{B,C,SUMMARY}`
别名与管线注释、`userconfig.rs` 路由键、`meta_tools.rs` 管线图、`events/errors/error_classify/日志` 的
`stage` 字样、`app.rs` 的 `human_stage`/`stage_index` + 测试、docs×4 / README / rules.md。
另清 `prompts.json`/`help.json` 里 B/C 阶段称呼与「沉淀阶段阶段」「总结阶段」两处批量替换残渣。

### 32.4 验证（真跑）

- `cargo fmt` / `clippy --workspace --all-targets -D warnings` / `cargo test --workspace` 全绿
  （lingmiao-core 26 · lingmiao-engine 41 · lingmiao-llm 45 · lingmiao-memory 38 · lingmiao-tools 94 · lingmiao-tui 148；含新增
  `legacy_stage_names_resolve_to_canonical`）。
- **`:99` 真进程 + 真实 LLM**：问答「管线有哪几个阶段」→ `meta_show(pipeline,graph)` 实测返回
  **三段** `["组织上下文","工作阶段","沉淀阶段"]`（截图 `stage-rename-d.png`）。
- **旧键兼容真进程实测**：`LINGMIAO_CONFIG=/tmp/legacy-cfg.json`（内含 `"C-对话": {group: kimi,…}`）
  → 启动日志 `config.json stage routing active routes=工作阶段=kimi/kimi-for-coding`，UI 身份行显示
  `kimi-for-coding`、启动行「阶段模型路由：工作阶段=kimi/kimi-for-coding」（截图 `legacy-cfg.png`）
  —— 旧键**确实路由到新阶段**，未静默回退。

版本 **v0.12.17**。

---

## §33 小白板移到 ctx 行左槽（与 token 数字同行、左对齐、留 gap）（cli 2026-09-28）

### 33.1 cli 反馈

- 「小白板显示在与上下文数字同一行，左对齐，留一点 gap，不要完全顶着上下文那里」。
- 即 §12.23「小白板可以取消了」撤掉整条 band 之后，白板回到 UI —— 但只占 token 数字那一行的**左半**。

### 33.2 落地（`crates/lingmiao-tui/src/app.rs`）

- 新增 `App::whiteboard_summary()`：把 `set_whiteboard` 送来的页内容压成**一行** —— `[N] 标题` +
  「第一个非空内容行」（` · ` 连接）。空页 / 缺页（占位 `WHITEBOARD_EMPTY`）返回空串，**不**渲染
  占位文字（空白比假内容安静）。
- 新增 `App::ctx_left_budget(pad_width, number_width)`：左槽可用列 = `行宽 − 数字宽 − CTX_LINE_GAP`，
  纯函数、可单测；数字宽超出时返回 0（不出现负预算）。
- `CTX_LINE_GAP = 2`：数字与白板之间**恒留 2 列**，白板超长走 `truncate`（按显示列，CJK=2）截断。
- `render_ctx_line` 改成**两个槽**：左槽 `Alignment::Left` 画白板摘要，右槽 `Alignment::Right` 画
  token 数字（语义不变，承 §30 —— 仍是「刚才那次 LLM 调用的 provider 实测 input_tokens」）。两者同色
  [`TEXT_MUTED`]，widget 层仍不写裸 `Color::`。
- `ctx_line_height()` 一并放宽：只要有白板摘要、或已有 token 数字，该行就显示（此前只认后者）。

### 33.3 测试

- `lingmiao-tui`（148 → **152**）：
  `whiteboard_summary_is_one_line_and_skips_the_placeholder`（占位/空页 → 空串；`[N] 标题 · 首个非空内容行`；
  跳空行）、`ctx_line_keeps_a_gap_before_the_token_number`（预算与 gap，含饱和）、
  `ctx_row_shows_whiteboard_left_and_tokens_right`（同行渲染：左列偏移 1、数字在右、中间只有空格）、
  `ctx_row_hidden_when_there_is_neither_note_nor_tokens`（两者皆无才折叠）。
  另订正 `render_single_column_into_a_test_backend` 的旧断言（它断言「白板 band 彻底移除」，现改为断言
  白板摘要**在 ctx 行上**，`撤侧栏` 老 band 仍不出现）。

### 33.4 验证（`:99` Xvfb + xterm 真进程 + 真实 LLM）

- 起真实进程（`lingmiao-run`，`ctxrow.sh`），输入 → 跑完整回合，实拍 `ctxrow-done.png`：底栏上方一行
  左显 `[23] 部署-<服务器> · 部署：<host>:<port>`，右显 `10.4K`，中间留白。
- **靶向回归**（`target.sh`）：输入「调用 list_archive 参数 limit=6」——正是 §31 崩溃那轮的同一工具
  同一参数 —— 日志实测 `"tool":"list_archive"` 被真实调用、`ok:false` 计数 0、raw 400 计数 0。

版本 **v0.12.18**。

## §34 输入框粘贴修复（cli 2026-09-29）

cli 反馈：

> 「输入框粘贴不好用」

先复现再定位。`:99` + xterm 真进程 + 真实进程操作（`xdotool`，与 §28 同一套），三条粘贴路径逐一实拍：

| # | 现象（修复前实拍） | 根因 |
|---|---|---|
| ① | `ctrl+v` 单行文本，输入框**多出空行**：`single-line-no-newline` 下光标停在第二行 | Windows 读取器把 `Get-Clipboard -Raw` 放在**管道末尾**，PowerShell 的输出格式化器给字符串补了 `\r\n`；`App::paste` 再把 `\r\n` 归一成 `\n` → 凭空一行 |
| ② | 剪贴板为空时 `ctrl+v` 仍插入**一个换行**（输入框从 1 行变 2 行） | 同上：空剪贴板经该管道实测正好读回 `"\r\n"` |
| ③ | **回合进行中**（`思考中`）`ctrl+v` **毫无反应**，`shift+insert` 同样 | `App::paste` 开头 `if self.is_busy() { return; }` —— 而回合中这个框是编辑**排队消息**的（§「排队」），打字本来就能进，粘贴却被静默拒绝，用户重按几次仍无反应 |

三条都与会话忙闲无关：①② 是**每次粘贴**都发生，③ 只在忙时发生（而「一边等一边贴下一句话」正是最常见的用法）。

### 34.1 修复

| 层 | 改动 |
|---|---|
| `clipboard.rs` | 读取命令改 `[Console]::Out.Write((Get-Clipboard -Raw))`（常量 `POWERSHELL_READ_ARGS`）—— 逐字节原样输出，不加行终止符；保留 `OutputEncoding=UTF8`（CJK 不变乱码） |
| `clipboard.rs` | 新增 `normalize_paste()`：`\r\n`/`\r` → `\n`，**去掉尾部换行**，其余原样（内部空行是用户正文，保留）。两条粘贴路径（终端 bracketed paste + `⌃V` 读剪贴板）共用，契约只此一处 |
| `app.rs` | `App::paste` 去掉 `is_busy()` 早退，改调 `normalize_paste`；空串直接忽略。回合中粘贴与打字同权（Enter 仍照旧把该行送进队列） |

「尾部换行去掉」同时解决①与②：`"one line\n"`（「我复制了整行」）不再留空行，空剪贴板的 `"\r\n"` 归零成空串、**什么都不插**。

### 34.2 测试（`lingmiao-tui` 152 → **154**）

- 新增 `clipboard::normalize_paste_owns_the_line_ending_contract`：CRLF/CR→LF、尾部换行剥离、空剪贴板 `"\r\n"`→`""`、**内部空行保留**。
- 新增 `clipboard::windows_reader_uses_out_write_not_a_bare_pipeline`：钉住命令形状（含 `[Console]::Out.Write(` / `Get-Clipboard -Raw` / UTF8），Windows 行为只能在 WSL 上跑，契约在任何机器上可验。
- 改 `app::paste_inserts_multiline_and_normalises_crlf`：新增「尾部换行不留空行」「空粘贴不插入」「**回合中粘贴落进输入框**（且 Enter 进队列）」断言。
- 改 `app::ctrl_v_pastes_from_the_clipboard`：忙时断言由「被忽略」改为「落进输入框」。

### 34.3 验证（`:99` Xvfb + xterm 真进程）

修复后逐条重拍：

- ✅ 单行 `ctrl+v` → 框内**只有一行** `single-line-no-newline`，无多余空行
- ✅ 20 行多行 `ctrl+v` → 20 行原样，末尾无空行，框内滚动到末行
- ✅ `shift+insert`（PRIMARY，内容末尾带 `\n`）→ 只贴到 `TRAILING-NEWLINE-LINE`，**不再有第二行**
- ✅ 空剪贴板 `ctrl+v` → 输入框**保持空**（不再插入换行）
- ✅ 回合进行中 `ctrl+v` → 输入框出现 `MIDTURN-PASTE-OK`（修复前此处为空）
- ✅ 300KB / 4000 行大块 `shift+insert` → 全量落入、进程不卡、滚动到末行
- ✅ 400 字符超长单行 `ctrl+v` → 折行 4 屏、`:END` 完整

`cargo fmt` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 全绿：
lingmiao-core 26 · lingmiao-memory 38 · lingmiao-llm 45 · lingmiao-engine 41 · lingmiao-tools 94 · **lingmiao-tui 154** · lingmiao 1。

版本 **v0.12.19**。

## §35 首帧 UI 错乱根因：MCP 子进程 stderr 污染 alternate screen（cli 2026-09-28）

cli 反馈（附截图，未随仓库分发）：

> 「刚启动的时候有个 bug，UI 是乱的，拖动一下窗口大小会变好」

截图内容：TUI 首帧上叠着一整屏 node 报错堆栈 —— `throw err;` / `Error: Cannot find module '<HOME>/.npm-global/lib/node_modules/tui-mcp/src/server.js'` / `code: 'MODULE_NOT_FOUND'` / `Node.js v22.23.2`，把品牌块和输入框挤散。

### 35.1 复现（PTY，逐字节取证）

用 `pty.fork()` 起一个真实 TTY 运行 `target/debug/lingmiao`，抓原始字节流（`.cache/lingmiao/tmp/pty_raw.bin`）：

```
灵妙 (Rust) v0.12.19 — deepseek/deepseek-v4-flash-vision-exp — logs: …
ESC[?1049h  ESC[?1000h ESC[?1002h ESC[?1003h ESC[?1015h ESC[?1006h ESC[?2004h
node:internal/modules/cjs/loader:1433\n  throw err;\n
Error: Cannot find module '<HOME>/.npm-global/lib/node_modules/tui-mcp/src/server.js'\n …
Node.js v22.23.2\n
```

`ESC[?1049h` = ratatui 已切入 alternate screen、TUI 已接管终端 —— **紧接着**这段 node 堆栈就写进了同一个终端。

### 35.2 机制链（四处合起来才是完整根因）

| # | 环节 | 事实 |
|---|---|---|
| ① | 时序 | `main.rs` 的 `engine.spawn_mcp_attach()` 是后台任务；`lingmiao_tui::run()` 的 `ratatui::init()` 先把终端切进 alternate screen。MCP 子进程与首帧同时发生。 |
| ② | 触发 | `crates/lingmiao-core/assets/mcp.json` 的 `tui-mcp` 指向 `<HOME>/.npm-global/lib/node_modules/tui-mcp/src/server.js` —— 该路径本机**不存在**（node 实际在 `~/.nvm/versions/node/v22.23.2`），node 启动即 `MODULE_NOT_FOUND` 退出。 |
| ③ | 漏点 | `crates/lingmiao-tools/src/mcp/client.rs` 用 `TokioChildProcess::new(…)`；rmcp 3.4 的 `TokioChildProcessBuilder::new` 默认 **`stderr: Stdio::inherit()`**（`rmcp-3.4.0/src/transport/child_process.rs`）→ 子进程 stderr **直通 TUI 终端**。 |
| ④ | 为何「拖一下就好」 | ratatui 增量重绘只刷自己 diff 出的脏单元格，外部写进来的字符它不知道，**不会被清掉**；只有 resize 触发的全量重绘才把整屏重画一遍 → 观感＝首帧乱、拖窗口即好。 |

同项目其他子进程都已接管输出（`file_tools` bash / `verify_tools` / `ripgrep` → piped，`clipboard` / `computer_use` → null），**唯独 MCP client 把 stderr 留给了终端**。

### 35.3 修复

`crates/lingmiao-tools/src/mcp/client.rs`：改用 builder 显式接管 stderr，并把它转成 `tracing::warn!` 后台逐行转发 —— 终端不再被污染，诊断信息也**不丢**（落进 `.cache/lingmiao/logs/*.log`）：

```rust
let (transport, stderr) = TokioChildProcess::builder(cmd)
    .stderr(std::process::Stdio::piped())
    .spawn()?;
if let Some(stderr) = stderr {
    tokio::spawn(async move { /* BufReader::lines → tracing::warn!("mcp server stderr: {line}") */ });
}
```

配套：`lingmiao-tools/Cargo.toml` 的 tokio 加 `io-util`（`AsyncBufReadExt::lines`）；`client.rs` 去掉不再需要的 `ConfigureCommandExt` 导入（原 `configure` 闭包被显式 `cmd.args/envs` 取代）。

> 注：这只修「污染终端」这一层。`tui-mcp` 路径本身仍是死的 —— 启动日志现会明确记 `mcp server stderr: Node.js v22.23.2` 等，便于定位；是否需要修 `mcp.json` 指向另议（当前 `rules.md` §6 明确 **不用 tui-mcp 作验收手段**）。

### 35.4 验证

- **PTY 字节流**（与 §35.1 同一脚本重跑）：修复前 5 个关键串各命中 1 次、raw 997 字节；修复后 **全部 0 次**、raw 212 字节（只剩品牌行 + 终端模式序列）。
- **`:99` 真实终端视觉**（`rules.md` §6 口径：computeruse 视觉模仿）：`Xvfb :99` + `xterm 132x42` 起真实进程，启动后 0.7s / 2.7s 各实拍一张 —— 首帧即为正常版式（左上品牌块 / 底部 ctx 行 + 输入框，无任何 node 堆栈）。
- 诊断不丢：同日日志可见 `mcp server stderr: …` / `mcp server stderr: Node.js v22.23.2`。
- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` 全绿（lingmiao-core 26 · lingmiao-memory 38 · lingmiao-llm 45 · lingmiao-engine 41 · lingmiao-tools 94 · lingmiao-tui 154 · lingmiao 1）。

版本 **v0.12.20**。

---

## §36 UI 四项优化（cli 2026-09-28 ②③④ + 小白板单行长度）

> cli：
> ①「针对目前单行小白板的最大长度做提示词优化，原来的小白板内容太长了显示不出来」
> ②「不光是启动时候，运行中也会有UI突然变乱」
> ③「UI上的阶段名还没改」
> ④「整个UI左下角那个位置看着空空的，加点元素，平常用来显示状态，阶段」

### 36.1 ① 单行小白板的「最大长度」= 提示词问题，不是截断问题

先查再动（实测 27 页）：ctx 行左槽 = `pad.width − 数字宽 − CTX_LINE_GAP(2)`，132 列终端下约 **120 列**；而
`whiteboard_summary()`（`[N] 标题 · 首条非空内容行`）最长的一页只有 78 列 —— **根本没有被截断**。
真正的原因是**内容本身长、且与标题重复**：page 24 的首行内容就是
`--- Page 24: 400超限根因+v0.12.18 ---`（`whiteboard_append` 从 `whiteboard_read` 结果里抄回来的分隔线），
于是单行摘要变成 `[24] 400超限根因+v0.12.18 · --- Page 24: 400超限根因+v0.12.18 ---`。

两处修：

| 层 | 落点 | 做法 |
|---|---|---|
| **提示词**（主） | `crates/lingmiao-core/assets/prompts.json` C 段「# 小白板 → 写作要求」 | 新增「**页首两行就是 UI 单行摘要**」条目：标题 ≤ 12 字、正文首行 ≤ 20 字、首行**不要重复标题**、**不要写 `--- Page N: … ---` 分隔线**、不要用旧称（`B-上下文选择`/`C-对话`/`总结流程`/`I-知识图谱更新`） |
| **渲染**（兜底） | `app.rs::whiteboard_summary` | 新增 `whiteboard_line_is_echo`：跳过「回声行」（`--- Page N: <标题> ---` 分隔线、或与标题逐字相同的一行），继续找第一条**有信息**的内容行 |

老页面（AI 已写入的长标题）由渲染层兜住；新页面由提示词约束源头。

### 36.2 ② 运行中 UI 变乱：panic hook 泄漏到 alternate screen

v0.12.20（§35）只修了**启动**那条（MCP 子进程 stderr 继承终端）。运行期还有一条同类通路：

- 全仓**没有自定义 panic hook**；`ratatui::init()` 装的 hook 只做 `restore()`（关 raw mode / 退出
  alternate screen），之后**默认 hook 照旧把 panic 打到 stderr** —— 而 stderr 就是 TUI 的终端。
- 运行期有 4 个后台 `tokio::spawn`（`engine.rs` MCP attach、`client.rs` ×2 LLM 流、`lib.rs` 回合任务）；
  `spawn_turn` 有 `catch_unwind`，**其余没有**。任一后台任务 panic → ①信息写进 alternate screen，
  ②`restore()` 把 TUI 踢回普通屏幕（进程还活着，UI 继续在普通屏幕上画）——**正是「突然变乱」**。

修法（`crates/lingmiao-tui/src/lib.rs`）：

```rust
pub fn install_panic_hook() { /* 替代 ratatui 的：写日志 + 记进 PANIC_MSG/PANIC_SEEN，不动终端 */ }
// run_loop：
if let Some(msg) = take_panic() { terminal.clear()?; app.push_error_detail(&format!("后台任务 panic：{msg}")); }
```

- 终端**不再被 panic 写脏**，也不再被 `restore()` 踢出 alternate screen；
- 信息不丢：`tracing::error!` 进 `.cache/lingmiao/logs/`，并作为一条错误块上转录（沿用 cli
  2026-09-27「即使未来没有发现的错误，也应该在UI上看得出来」）；
- 每帧检查 `take_panic()`（一次性），命中即 `terminal.clear()` **全量重绘** —— 增量重绘清不掉的外部
  写入（§35 的「拖窗口才变好」机制）就此被主动清掉；
- UI 线程本身的 panic 仍会让会话结束，但走 `run()` 自己的 `catch_unwind` + 收尾，终端的
  restore 仍然只发生一次。

### 36.2b 窄终端竖排乱码：committed / live 用了两个宽度

cli 附图（未随仓库分发，另一进程、窄窗）里工具卡是「一个词一行、竖着排」——
`bash(whi` / `ch` / `xdotool` / `scrot` …。根因**不是**渲染坏了，是**两个 wrap 宽度**：

- `render_conversation` 用 `wrap_width = pad.width` 展平 **committed** 缓存；
- 同一帧的 **live** 块却用 `inner_w = max(8, pad.width)`（`thinking_lines` / 活动行 / running card）；
- 窄窗下二者差 2 列，同一个回合的已提交半和流式半**各按各的宽折行** → 视觉上就是一列一个词。

修：`wrap_width = inner_w`，committed 与 live 强制同宽（新增测试
`narrow_pane_wraps_committed_and_live_blocks_at_the_same_width`，24 列下断言
`cache_width == 22` 且卡片头行仍有完整单词）。

> 关于 resize：`terminal.autoresize()?` 本身已足够 —— ratatui 的 `Terminal::resize`
> 在几何真的变化时会**一并 `clear()`**（`terminal.rs::resize` 末行为 `self.clear()`），
> 下一帧即全量重绘。所以运行期「拖窗口后变好」在这个版本里是既有行为，无需再加 clear；
> 真正缺的是上面这个**同宽**约束，以及 §36.2 的 panic 通路。

### 36.3 ③④ 阶段名 + 左下角状态槽

- **现状**（实测）：UI 里用户可见的阶段字样是**去黑话动词**——`human_stage()`（组织上下文→检索中 /
  工作阶段→作答中 / 沉淀阶段→沉淀中）、`PROGRESS_STAGES`、`stage_badge()`（`丨检索中/丨作答中/丨沉淀中`）。
  即 v0.12.17 的规范名（组织上下文/工作阶段/沉淀阶段）**没进 UI**。cli ③ 指的就是这里。
  本轮同时清掉 `help.json` `topics/logs` 里的旧术语（「总结上报 / 知识图谱更新」）。
- **左下角**（④）：footer 是**一行**——右半是身份行 `灵妙 vX · model · cwd`（§12.1），**左半一直是空的**，
  只被两个瞬时内容借用（`再按一次 ⌃C 退出`、`已复制 N 字符`）。本轮把左槽常驻为
  `footer_status()`：运行中 `作答中 · 工作阶段`（沿用既有动词 + 规范阶段名，橙色），空闲 `待命`（灰）；
  与身份行放不下时自动让位，退出提示与复制提示仍优先覆盖。

### 36.4 验证

- **panic 契约（PTY 逐字节，真二进制）**：`examples/panic_probe`（本修复）vs `examples/panic_control`
  （复刻修复前的 `ratatui::init()` + 默认 hook）：修复后 `thread '` / `panicked` 在终端输出里各 **0 次**、
  **不进 alternate screen**（`ESC[?1049h` 0 次）；对照例各 1 次、进 alt screen **1 次** —— 即修复前的
  文本确实写在 alt screen 里（且会 `ESC[?1049l` 提前退出）。
- **:99 Xvfb + xterm 132x42 真进程实拍**（rules.md §6 computeruse 视觉口径）：启动首帧左槽即 `待命`；
  发一句真回合，运行中左槽变 `作答中 · 工作阶段`（橙），进入沉淀阶段变 `沉淀中 · 沉淀阶段`，
  与 activity 行同步跳动。
- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace`
  全绿（lingmiao-core 26 · lingmiao-engine 41 · lingmiao-llm 45 · lingmiao-memory 38 · lingmiao-tools 94 · **lingmiao-tui 158** ·
  lingmiao 1 · doctests）。新增测试：`whiteboard_summary_skips_a_header_echo_line`、
  `footer_shows_status_and_stage_then_stands_by`、`narrow_pane_wraps_committed_and_live_blocks_at_the_same_width`、
  `a_recorded_panic_is_reported_exactly_once`（lingmiao-tui 154 → **158**）。

版本 **v0.12.21**。

## §37 未提交输入（队列 + 草稿）落盘恢复（cli 2026-09-28）

cli：「刚才队列里的提示词还能看到吗？进程突然被杀死了」——答案是「看不到」，而且根因**两处**：队列
（`App::queue`，mid-turn Enter 入队）和输入框草稿都只活在内存里，全 crate 没有任何落盘；唯一持久化用户
文本的地方是 `turns.user_msg`（`record_turn` 在回合**结束后**写），所以被 kill 时还在排队的消息磁盘上
零留痕。

### 37.1 落盘形状

新增 `crates/lingmiao-tui/src/session_input.rs`：`SessionInput { queue: Vec<String>, draft: String }`，
写到 **易失**缓存目录 `Paths::tmp_dir` = `.cache/lingmiao/tmp/session-input.json`（rules.md §3：
`.cache/` 是可删的运行时产物，**不是**持久记忆资产；恢复丢失的草稿不值得污染 `.memory/`）。

```rust
let mut last_input = app.unsubmitted();
loop {
    /* …select! 处理键盘 / 事件 / 回合结束… */
    let input = app.unsubmitted();            // 队列 + 当前输入框文本
    if input != last_input {                  // 10 帧/秒的循环里按需写盘
        let _ = session_input::save(&tmp_dir, &input);  // 临时文件 + rename，原子
        last_input = input;
    }
}
```

- 写盘落在**循环尾部**——所有分支的唯一汇合点，未来新增键处理不会漏掉写盘（旧教训：分散在各处理函数里
  的副作用总会被后来的改动绕过）。
- `save` 先写 `.tmp` 再 `rename`：这次要防的就是「写到一半被杀」，绝不能留下一个 parse 不了的 JSON。
- 空快照（闲着、空框、空队列）**删文件**而不是写 `{}`：否则一个早该作废的草稿会在下次启动时复活。
- 启动时 `session_input::load` 读回，先 `restore_unsubmitted` 再补一条 notice——
  `已恢复上次未提交的输入：N 条排队消息已放回队列` / `（输入框草稿）`。凭空出现在输入框里的文字必须
  有解释，这是本项目一贯的「不静默」原则（§28 / §36）。

### 37.2 验证（真进程）

:99 Xvfb + xterm 132x42 跑**真二进制**（`tmp_dir` 落在临时工程目录 `/tmp/lmrec`）：

1. 发一个真回合 → mid-turn 依次 Enter 两条（`queued A/B`）→ 再打半句草稿留在输入框；
   实拍 UI 两条 `❯` 排队条 + 底部 `draft: 半句草稿还没发`，`session-input.json` 同步出现三份文本。
2. `kill -9 <lingmiao pid>`（**先核对 pid ≠ 宿主 lingmiao、≠ 本 shell**，obs-841fc19b0a6b 的教训）；
   JSON 文件仍在。
3. 同一目录重启二进制 → 实拍首帧即显示 `· 已恢复上次未提交的输入：2 条排队消息已放回队列`，
   两条排队条回来、输入框回来那半句草稿。

单测（lingmiao-tui 158 → **168**，含 `session_input` 4 项）：`unsubmitted_captures_both_the_queue_and_the_live_draft`、
`nothing_un_submitted_snapshots_as_empty`、`restore_puts_the_queue_back_in_order_and_refills_the_box`、
`a_restored_draft_that_the_user_submits_leaves_no_stale_snapshot`。

版本 **v0.12.22**。

## §38 运行中 bash 卡片「竖排」根因：list_hanging 全行扫描（cli 2026-09-28）

cli 附图（未随仓库分发，另一进程、v0.12.20）：

> 「现在还是存在运行中 bash 命令布局乱了的情况」

截图是一张**运行中的工具卡**，被渲染成「一个词一行、竖着排」：

```
                             中
                             bash(whi
                             ch
                             xdotool
                             scrot
                             Xvfb
                             ffmpeg
                             xterm
                             2>&1;
                             echo
                             "---
                             DISPLA)
                             4s
```

### 38.1 先查再动：这不是 §36.2b 那条

§36.2b（v0.12.21）修的是「committed 与 live 用了两个宽度」（`pad.width` vs `max(8, pad.width)`），
修完 `wrap_width = inner_w`，两者同宽。本轮**实测复跑那条路径不再复现**（:99 / :101 上 24/26 张连拍
全部正常，`narrow_pane_wraps_committed_and_live_blocks_at_the_same_width` 也继续绿）。

真正的线索是**行数**：数一数截图那 13 行 —— 第 2 行起每一行都缩进到**第 70 列**，且每行只放得下
`width − 70` 列。78 列的窗里那就是 **8 列**body：一个短词一行。所以问题不是「宽度不一致」，而是
**hanging（悬挂缩进）被算成了 70**。

### 38.2 根因（实测复现，非推断）

`crates/lingmiao-tui/src/app.rs::list_hanging` 的旧实现对**整行**扫描 `(•|-|*|+) ` 或 `N. ` 形状的子串：

```rust
while i < chars.len() {                       // ← 扫到行尾
    if matches!(chars[i], '•'|'-'|'*'|'+') && chars.get(i+1) == Some(&' ') {
        return Some(display_width(&chars[..=i+1].iter().collect::<String>()));
    }
    ...
}
```

而工具卡头部正好含 `- `：`bash(which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo "--- DISPLAY")` 里
`--- ` 的**第三个连字符**加空格匹配上了「无序列表标记」。于是：

- 渲染行 = `⠋ 丨作答中 bash(which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo "--- DISPLA)  4s`（81 列）
- 旧 `list_hanging` = **70**（= `⠋ 丨作答中 bash(which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo "---` 的显示宽度）
- `wrap_cells` 于是给出 `cap = 78 − 70 = 8`，续行缩进 70 → **13 行竖排**

同一个 bug 也解释为什么它「运行中」才刺眼：**已提交**的卡片（`● … 1.5ms`）与**运行中**的卡片
（`⠋ … 4s`）头部同形，都被这 70 列缩进切碎，只是运行中的那张还在逐帧重绘（spinner），更显混乱。
且参数越长越容易踩到（`which xdotool scrot Xvfb ffmpeg xterm` 这种多词命令遍地都是）。

### 38.3 修法：标记只在「列表项能开始的地方」才算

`list_hanging` 改为先剥掉**渲染 chrome**，再只看行首：

| 步骤 | 说明 |
|---|---|
| 前导字形 | `● ` / `∴ ` / `❯ ` / `◜ ` / 运行卡 spinner（braille `SPINNER` 族，每 100 ms 换一个） |
| 阶段标签 | `丨检索中 ` / `丨作答中 ` / `丨沉淀中 ` |
| 嵌套缩进 | 行首空格（markdown 每层 2 列） |
| **然后** | 行首是 `• `/`- `/`* `/`+ `/`N. ` 才算列表项；否则 `None` |

新增 `strip_item_chrome(&str) -> (&str, usize)` 承担前三步（返回剩余文本 + 已消费列数），
`list_hanging` 只在其结果上判标记。这样：

- 工具卡头部（`bash(… "--- DISPLAY")`）→ `None` → 不缩进，卡片恢复整宽折行；
- 真正的 markdown 列表项（`● 丨作答中 • Rust 的所有权`）→ 仍挂在条目文本下（`Some(13)`），§12.9 的
  「列表悬挂缩进」语义不变。

### 38.4 验证

- **纯函数复现 + 对照**（离线镜像 `wrap_paragraph`/`wrap_cells` 规则）：
  - 旧 `list_hanging(card_header)` = **70**、78 列下折成 **14** 行（首行只到第 3 列）；
  - 新 = **None**、折成 **2** 行（首行用满 78 列）；
  - `● 丨作答中 • Rust 的所有权` = `Some(13)`（列表项未被误伤）。
- **新增单测** `tool_card_header_never_hangs_on_a_dash_in_its_argument`（lingmiao-tui 168 → **169**）：
  断言 live / committed 两种卡片头 `list_hanging` 均为 `None`、78 列下首行 > 40 列、每行 ≤ 78。
- **真进程视觉**（rules.md §6 口径）：:101 Xvfb（无 WM，xterm 几何生效）78x34 起真二进制，发真回合让
  模型跑 `which xdotool scrot Xvfb ffmpeg xterm 2>&1; echo "--- DISPLAY"`，逐 2s 连拍 26 张 ——
  卡片、`⎿` 结果、`· ctrl+o 展开` 折行全部正常，无竖排、无右侧窄列。
- `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace` 全绿（lingmiao-core 26 · lingmiao-memory 38 · lingmiao-llm 45 · lingmiao-engine 42 ·
  lingmiao-tools 94 · **lingmiao-tui 169** · lingmiao 1）。

版本 **v0.12.23**。
