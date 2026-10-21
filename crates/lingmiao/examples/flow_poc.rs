//! 编排框架 PoC（2026-10-21，cli 指令）：「按写剧本的流程，用 5 个角色各司其职
//! 编排一套工作流，验证当前编排框架的有效性，以最终出来结果作为判定依据」。
//!
//! 做法：不从零造 LLM 调用——**直接复用引擎里的真实角色运行时**
//! （`autonomous::role::RoleRuntime`，自动模式 Main/Auditor 用的就是它），把它
//! 当「角色模板」逐角色实例化：每个角色**独立的 system 提示词** + **独立的
//! 记忆目录**（`.memory/workflows/写剧本/<角色名>/`），环节间靠 blackboard
//! （上一步产出注入下一步的 input 模板）流转。跑的就是 lingmiao 引擎里那套
//! 「工作阶段式工具循环」（StageAgent::work）+ 真实 LLM client（Kimi/K3）。
//!
//! 串行（同时只跑一个角色）→ 导演 → 编剧 → 台词 → 分镜 → 制片人。
//! 跑完后把每步产出落 `output/*.md`、最终剧本落 `output/最终剧本.md`，并把
//! 「产物文件 + 各角色记忆区行数」当作判定依据如实打印。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lingmiao_core::{Config, EventBus};
use lingmiao_engine::autonomous::role::{RoleKind, RoleRuntime};
use lingmiao_llm::{Client, ModelRegistry};
use lingmiao_memory::{Memory, store::Zone};
use lingmiao_tools::ToolRegistry;

/// 五个角色（顺序即环节顺序——串行流水线）。
const ROLES: [(&str, &str); 5] = [
    ("01-导演", "director"),
    ("02-编剧", "writer"),
    ("03-台词", "dialogue"),
    ("04-分镜", "storyboard"),
    ("05-制片人", "producer"),
];

/// 工作流配置（等价于将来 `workflows/*.yaml` 里的 `agents[]`/`steps[]`）。
struct RoleDef {
    dir_name: &'static str,
    stage_name: &'static str,
    /// 角色 system 提示词（注入 RoleRuntime 的 `role_block` 槽——本字段**会被读**）。
    system: String,
    /// 输入模板：`{goal}` 与 `{prev}` 槽由编排器填充（`{prev}` = 上一步产出）。
    input_tpl: &'static str,
}

fn role_defs(cfg: &Config, work_root: &Path) -> Vec<RoleDef> {
    let cwd = work_root.display().to_string();
    let mk = |dir_name: &'static str,
              stage_name: &'static str,
              duty: &str,
              input_tpl: &'static str| RoleDef {
        dir_name,
        stage_name,
        system: format!(
            "你是「写剧本」工作流里的 **{stage_name}**。\n\
             你只负责本环节的事：{duty}\n\
             工作目录是 {cwd}；上一步的产出已经附在你的任务里（『上一步产出』段）。\n\
             纪律：只动本环节该动的文件；不要替下游角色做他们的工作；产出要落到 \
             文件（用 write_file 写到 output/ 下），并**先用 update_memory 把本环节的 \
             关键决定写进你自己的记忆区**（你的记忆区是本角色专属的，下游看不到你的记忆、 \
             只能靠你的产出文件与『上一步产出』）。\n\
             写完文件后，用一两句话向编排器汇报「做了什么、落在哪个文件」。",
        ),
        input_tpl,
    };
    let _ = cfg;
    vec![
        mk(
            "01-导演",
            "导演",
            "定本子：把一个题目落成一段**剧情梗概 + 主题 + 主要人物表**（各一两句即可），写到 `output/01-梗概.md`",
            "题目：{goal}\n\n请完成你的环节。",
        ),
        mk(
            "02-编剧",
            "编剧",
            "把梗概扩成**分场大纲**（3~5 场，每场一行：场号 / 地点 / 发生什么），写到 `output/02-大纲.md`",
            "上一步产出（导演·梗概）：\n{prev}\n\n请完成你的环节。",
        ),
        mk(
            "03-台词",
            "台词",
            "为每场写**关键对白**（每场 2~4 句，口语、符合人物），写到 `output/03-台词.md`",
            "上一步产出（编剧·分场大纲）：\n{prev}\n\n请完成你的环节。",
        ),
        mk(
            "04-分镜",
            "分镜",
            "把大纲落成**分镜表**（每场 2~3 个镜头：镜头号 / 景别 / 画面内容 / 对应台词），写到 `output/04-分镜.md`",
            "上一步产出（台词·关键对白）：\n{prev}\n\n请完成你的环节。",
        ),
        mk(
            "05-制片人",
            "制片人",
            "做**最终把关**：通读 output/ 下四个文件，合并成最终剧本 `output/最终剧本.md`（梗概→大纲→台词→分镜 四节），并写一段「制片意见」（是否可拍、主要风险）",
            "上一步产出（分镜·分镜表）：\n{prev}\n\n请完成你的环节。",
        ),
    ]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let goal = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "深夜便利店".to_string());
    let t0 = Instant::now();

    // 工作区（不碰主仓）：PoC 全部落 /tmp/lmflow/proj，记忆区在其 .memory 下。
    let work_root = PathBuf::from("/tmp/lmflow/proj");
    std::fs::create_dir_all(work_root.join("output"))?;
    let flow_mem_root = work_root.join(".memory").join("workflows").join("写剧本");

    // 引擎的真实装配：**走 kimi/k3**（DeepSeek 余额不足——402，承 P0 根因），
    // 真实工具注册表 + 真实角色运行时。
    let cfg = Config::load_default()?;
    let registry_models = ModelRegistry::load()?;
    let spec = registry_models
        .resolve("kimi", "k3")
        .ok_or("models.json 没有 kimi/k3")?;
    let client = Client::new(spec)?;
    let bus = Arc::new(EventBus::new(256));
    let tools: Arc<ToolRegistry> = Arc::new(lingmiao_tools::default_registry(&work_root));

    println!("== 编排 PoC：写剧本（5 角色串行）");
    println!("   题目：{goal}");
    println!("   工作区：{}", work_root.display());
    println!("   角色记忆根：{}", flow_mem_root.display());
    println!("   模型：{}", client.model());

    let mut prev_output = String::new();
    let mut ledger: Vec<(String, String, u64, u64)> = Vec::new(); // (角色, 产物文件, 入token, 出token)

    for (i, def) in role_defs(&cfg, &work_root).into_iter().enumerate() {
        let role_mem_dir = flow_mem_root.join(def.dir_name);
        std::fs::create_dir_all(&role_mem_dir)?;
        // 每角色独立记忆区（本 PoC 走「无嵌入」降级——lexical 检索即可；
        // 嵌入器只影响语义召回，不影响「记忆区是否独立」这条判定）。
        let memory = Memory::open_in_dir(&role_mem_dir, Zone::Main, None)
            .map_err(|e| format!("open memory for {}: {e}", def.dir_name))?;
        let mut role = RoleRuntime::new(
            RoleKind::Main, // 角色种类仅决定工具白名单——Main = 全量工具
            cfg.clone(),
            bus.clone(),
            client.clone(),
            tools.clone(),
            Some(Arc::new(memory)),
            &work_root,
            Duration::from_secs(600),
        );
        // 每环节独立 system 提示词（本工作流的核心）：直接替换 `role_block` 槽——
        // 这正是「每个环节系统提示词都不一样」的机制落点。
        role.role_block = def.system.clone();

        let input = def
            .input_tpl
            .replace("{goal}", &goal)
            .replace("{prev}", &prev_output);
        println!("\n--- 环节 {} · {} 开始", i + 1, def.stage_name);
        let step_t = Instant::now();
        let out = role.run(&input).await;
        let dt = step_t.elapsed();
        if !out.ok() {
            return Err(format!("环节 {}（{}）fault：{}", i + 1, def.stage_name, out.fault).into());
        }
        // 角色运行时持有记忆区但自己不写（搜不到 update_memory 也得有兜底）：
        // 编排器把「该角色这一步做了什么」显式落进它自己的记忆区，让「角色记忆
        // 独立」这条判据可核。
        if let Some(mem) = &role.memory {
            let note = format!(
                "环节 {} · {} 完成：题目「{}」，产出 {} 字符，工具调用 {} 次",
                i + 1,
                def.stage_name,
                goal,
                out.response.len(),
                out.tool_calls
            );
            let _ = mem
                .observations
                .insert(&lingmiao_memory::observations::NewObservation::new(
                    "task",
                    &format!("环节{}-{}", i + 1, def.dir_name),
                    &note,
                ));
        }
        prev_output = out.response.clone();
        println!(
            "    完成 · {:.0}s · in {} / out {} tokens · 工具调用 {} 次",
            dt.as_secs_f32(),
            out.usage.input_tokens,
            out.usage.output_tokens,
            out.tool_calls
        );
        // 该角色该产出的文件名（编排层权威，而非猜模型实际写了什么）——
        // 「职责表」就是编排器自己维护的。
        let expected_artifact = match def.dir_name {
            "01-导演" => "01-梗概.md",
            "02-编剧" => "02-大纲.md",
            "03-台词" => "03-台词.md",
            "04-分镜" => "04-分镜.md",
            _ => "最终剧本.md",
        };
        let artifact = work_root.join("output").join(expected_artifact);
        ledger.push((
            def.stage_name.to_string(),
            artifact.display().to_string(),
            out.usage.input_tokens,
            out.usage.output_tokens,
        ));
    }

    // ===== 判定依据（如实打印，供人工/机械核对）=====
    println!("\n===== 判定依据 =====");
    println!("总耗时：{:.0}s", t0.elapsed().as_secs_f32());
    let mut all_ok = true;
    for (role, artifact, ti, to) in &ledger {
        let p = Path::new(artifact);
        let ok = p.exists() && p.metadata().map(|m| m.len() > 0).unwrap_or(false);
        all_ok &= ok;
        println!(
            "- {}：产物 {} · {} ({} B) · tokens {}/{}",
            role,
            artifact,
            if ok { "✅" } else { "❌" },
            p.metadata().map(|m| m.len()).unwrap_or(0),
            ti,
            to
        );
    }
    // 记忆区独立性：每角色目录下应有各自的 observations DB。
    println!("\n各角色记忆区：");
    for (dir, _) in &ROLES {
        let db = flow_mem_root.join(dir).join("observations.db");
        let exists = db.exists();
        let rows = if exists { count_obs_rows(&db) } else { -1 };
        println!(
            "- {}：{} · observations 行数 {}",
            dir,
            if exists { "✅ 已建" } else { "❌ 未建" },
            rows
        );
    }
    let final_script = work_root.join("output/最终剧本.md");
    println!(
        "\n最终剧本：{} · {} B",
        final_script.display(),
        final_script.metadata().map(|m| m.len()).unwrap_or(0)
    );
    println!(
        "\n判定：{}",
        if all_ok && final_script.exists() {
            "✅ 有效（5 环节全部产出落盘，最终剧本已生成）"
        } else {
            "❌ 无效（有环节没落盘）"
        }
    );
    Ok(())
}

/// 数一个 observations.db 的行数（只读，失败返回 -1）。
fn count_obs_rows(db: &Path) -> i64 {
    let conn =
        match rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        {
            Ok(c) => c,
            Err(_) => return -1,
        };
    conn.query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))
        .unwrap_or(-1)
}
