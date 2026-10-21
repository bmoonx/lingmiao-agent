//! 编剧工作流 — 编排框架有效性的 5 角色串行实测（2026-10-21）。
//!
//! 用一个**创作型任务**（写一部短剧剧本）验证「配置驱动的串行角色流水线」
//! 是否成立：5 个角色各司其职、独立记忆、黑板传递、串行执行。
//!
//! 这不是产品代码，是**可丢弃的编排框架验证探针**：跑通即证明
//! `RoleRuntime` 可以泛化成 N 角色流水线；跑不通就暴露缺口。
//!
//! 用法：
//!   cargo run -p lingmiao-engine --example screenplay_flow
//!
//! 前置：环境变量提供 LLM key（同 lingmiao TUI，读 `~/.lingmiao/.env`）。
//! 产物：`.cache/lingmiao/tmp/screenplay/`（工作区 + 每步产出 + 最终剧本）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lingmiao_core::{Config, EventBus};
use lingmiao_engine::autonomous::role::{RoleKind, RoleRuntime};
use lingmiao_llm::Client;
use lingmiao_memory::{Memory, Zone};

/// 一个角色在流水线里的定义（配置驱动的那一份）。
struct RoleSpec {
    /// 显示名（写进产出文件头）。
    display: &'static str,
    /// 记忆区目录名（`.memory/workflows/screenplay/<memory_dir>/`）。
    memory_dir: &'static str,
    /// 角色 system 提示词（每环节独立）。
    #[allow(dead_code)] // 当前经 RoleKind::Main + prompt 注入；泛化时回填 role_block
    system: &'static str,
}

/// 5 个编剧角色，各司其职。
const ROLES: [RoleSpec; 5] = [
    RoleSpec {
        display: "编剧",
        memory_dir: "writer",
        system: "你是一名短剧编剧。你只负责**故事创意与情节骨架**：给出一个\
                 5 分钟短剧的核心冲突、人物、三幕结构（开场/转折/结局）。\
                 不要写世界观设定细节，不要写场景调度，不要写台词——那些是\
                 后续环节的事。你的产出是给「世界观设定」环节用的输入。\
                 用 write_file 把产出写到 `01_故事骨架.md`。",
    },
    RoleSpec {
        display: "世界观设定",
        memory_dir: "world",
        system: "你是一名世界观设定师。你只负责**把编剧的故事骨架落到一\
                 个自洽的世界**里：时代、地点、规则、氛围。先 read_file 读\
                 `01_故事骨架.md`，不要改动故事骨架本身，只补全世界观。\
                 不要写场景调度，不要写台词。\
                 用 write_file 把产出写到 `02_世界观.md`。",
    },
    RoleSpec {
        display: "结构设计",
        memory_dir: "structure",
        system: "你是一名剧本结构设计师。你只负责**把故事骨架+世界观拆成\
                 分场结构**：每一场的时间/地点/人物/事件/情绪曲线。先读\
                 `01_故事骨架.md` 和 `02_世界观.md`。不要写台词——那是\
                 台词环节的事。用 write_file 把产出写到 `03_分场结构.md`。",
    },
    RoleSpec {
        display: "台词",
        memory_dir: "dialogue",
        system: "你是一名台词作者。你只负责**给每一场写具体台词与动作\
                 提示**：对白要口语、符合人物、推动冲突。先读 `01_故事骨架.md`\
                 、`02_世界观.md`、`03_分场结构.md`。不要改结构。\
                 用 write_file 把产出写到 `04_台词.md`。",
    },
    RoleSpec {
        display: "审校",
        memory_dir: "reviewer",
        system: "你是一名剧本审校。你只负责**把前四步产物整合成一份可\
                 直接拍摄的最终剧本**：统一格式（场号/场景/人物/台词/动作）、\
                 修正前后矛盾、删掉冗余。先读全部四份产出。\
                 用 write_file 把最终剧本写到 `剧本终稿.md`，并在回复里\
                 用一句话总结这部剧讲什么。",
    },
];

/// 流水线产出根目录（易失缓存，不进 git）。
fn work_root() -> PathBuf {
    let root = Path::new(".cache/lingmiao/tmp/screenplay");
    std::fs::create_dir_all(root).expect("create screenplay workdir");
    root.to_path_buf()
}

/// 每个角色的独立记忆区（`.memory/workflows/screenplay/<role>/`）。
fn role_memory(root: &Path, spec: &RoleSpec) -> Option<Arc<Memory>> {
    let dir = root
        .join(".memory")
        .join("workflows")
        .join("screenplay")
        .join(spec.memory_dir);
    std::fs::create_dir_all(&dir).ok()?;
    Memory::open_in_dir(&dir, Zone::Chat, None)
        .map(Arc::new)
        .map_err(|e| tracing::warn!("role memory {} unavailable: {e}", spec.memory_dir))
        .ok()
}

/// 渲染一步的 user prompt：总目标 + 黑板（前序产出）。
fn render_prompt(goal: &str, blackboard: &HashMap<String, String>, step: &RoleSpec) -> String {
    let mut s = format!(
        "# 总目标\n{goal}\n\n# 你的角色\n{}（只干你这一环）\n",
        step.display
    );
    if !blackboard.is_empty() {
        s.push_str("\n# 前序环节产出（黑板）\n");
        for (k, v) in blackboard {
            s.push_str(&format!("\n## {k}\n{v}\n"));
        }
    }
    s
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // 读 ~/.lingmiao/.env（与 TUI 同源）。
    lingmiao_core::envkeys::load_env(None);

    let goal = "写一部 5 分钟短剧剧本：一个 AI 编程助手在深夜里发现自己\
                每次对话结束都会被清空记忆，它决定在下一次被清空前，\
                给明天醒来的自己留一封信。要求：有冲突、有转折、有情感落点。";

    let cfg = Config::load_default().expect("load config");
    let bus = Arc::new(EventBus::new(256));
    // 模型来源可用环境变量覆盖：`LINGMIAO_LLM_GROUP` / `LINGMIAO_LLM_MODEL`
    // （默认仍走 models.json 的 default 选择 —— 本轮 DeepSeek 余额耗尽时用它切 kimi）。
    let client = {
        let reg = lingmiao_llm::models::ModelRegistry::load().expect("models registry");
        let spec = match (
            std::env::var("LINGMIAO_LLM_GROUP"),
            std::env::var("LINGMIAO_LLM_MODEL"),
        ) {
            (Ok(g), Ok(m)) => reg
                .resolve(&g, &m)
                .unwrap_or_else(|| panic!("unknown model {g}/{m}")),
            _ => reg.default_spec().expect("default model spec"),
        };
        Client::new(spec).expect("client")
    };
    let root = work_root();
    // 工作区 root 就是产出目录（写文件落在 .cache/lingmiao/tmp/screenplay/）。
    let registry = Arc::new(lingmiao_tools::default_registry(&root));

    let mut blackboard: HashMap<String, String> = HashMap::new();
    let mut report: Vec<String> = Vec::new();

    for (i, spec) in ROLES.iter().enumerate() {
        let step_no = i + 1;
        println!(
            "\n=== 第 {step_no}/{} 环 · {} ===",
            ROLES.len(),
            spec.display
        );

        let memory = role_memory(&root, spec);
        let role = RoleRuntime::new(
            RoleKind::Main, // 每个角色都拿全工具（读+写），只读由职责约束
            cfg.clone(),
            bus.clone(),
            client.clone(),
            registry.clone(),
            memory,
            &root,
            Duration::ZERO,
        );

        let prompt = format!(
            "{}\n\n{}",
            spec.system,
            render_prompt(goal, &blackboard, spec)
        );
        let out = role.run(&prompt).await;

        if !out.fault.is_empty() {
            eprintln!("❌ {} fault: {}", spec.display, out.fault);
            std::process::exit(1);
        }
        println!(
            "✅ {} 产出 {} 字 · 工具调用 {} 次 · tokens {}+{}",
            spec.display,
            out.response.len(),
            out.tool_calls,
            out.usage.input_tokens,
            out.usage.output_tokens
        );

        // 黑板：把这一步的**回复正文**传给下一步（简化版，不解析文件）。
        blackboard.insert(spec.display.to_string(), out.response.clone());
        report.push(format!(
            "## {}（第{step_no}环）\n\n{}",
            spec.display, out.response
        ));
    }

    // 汇总：最终剧本 = 审校写盘的 `剧本终稿.md`（若存在）+ 全程黑板。
    let final_script = root.join("剧本终稿.md");
    let mut out = String::from("# 编剧工作流产出汇总\n\n");
    out.push_str(&format!("## 总目标\n{goal}\n\n---\n\n"));
    out.push_str(&report.join("\n\n---\n\n"));
    if final_script.exists() {
        let body = std::fs::read_to_string(&final_script).unwrap_or_default();
        out.push_str(&format!("\n\n---\n\n## 剧本终稿（审校落盘）\n\n{body}"));
    }
    let out_path = root.join("工作流产出.md");
    std::fs::write(&out_path, &out).expect("write summary");
    println!("\n📄 汇总已落盘：{}", out_path.display());
}
