//! RRF 融合 + e5 `query:`/`passage:` 前缀的**生产路径**黑盒探针。
//!
//! 只用**只读副本库**（`cp -a .memory <副本>` 而来），绝不触碰活跃 `.memory/`。
//! 与 `recall_algo_probe.rs`（手工实现 5 种策略做对照）不同，本探针跑的是**真实
//! 工具**：`lingmiao-tools` 的 `search_memory`，即用户与模型实际调用的那条路。
//!
//! 一次跑完验证三件事：
//! ① **向量空间迁移**：副本记录的后端若是旧的（MiniLM / 无前缀 e5），
//!    `embedder_backend_stale()` 必须为真，`rebuild_embeddings()` 之后转假；
//! ② **前缀生效**：重建后 recorded == `EMBEDDER_BACKEND`（带 `-prefixed`）；
//! ③ **召回质量**：每个查询打印 top-N 的 layer / score / title，人工核对是否
//!    为真相关条目（旧口径下短中文查询的 top 全是 `turn-xxx` 噪声）。
//!
//! 跑法：
//! `cargo run --release -p lingmiao-tools --example rrf_prefix_probe -- <副本库目录>`
//!
//! 副本准备（在项目根）：
//! ```sh
//! rm -rf .cache/lingmiao/tmp/rrf-verify && mkdir -p .cache/lingmiao/tmp/rrf-verify
//! cp -a .memory/. .cache/lingmiao/tmp/rrf-verify/
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use lingmiao_memory::{Memory, Zone};
use lingmiao_tools::ToolRegistry;
use serde_json::Value;

const QUERIES: [&str; 6] = [
    "记忆合并",
    "编排框架",
    "层配额",
    "单二进制分发",
    "召回算法",
    "并发",
];

fn line(c: char) {
    println!("{}", c.to_string().repeat(78));
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            eprintln!("用法: rrf_prefix_probe <副本库目录>");
            std::process::exit(1);
        });
    if !dir.join("observations.db").exists() {
        eprintln!(
            "{} 下没有 observations.db —— 传一个 .memory 副本目录",
            dir.display()
        );
        std::process::exit(1);
    }
    if dir.starts_with("/mnt/c/ai/lingmiao/.memory") {
        eprintln!("拒绝对活跃 .memory 运行：这是只读副本探针，请先 cp 一份");
        std::process::exit(1);
    }

    line('=');
    println!("RRF + e5 前缀 · 生产路径探针");
    println!("库 = {}", dir.display());
    line('=');

    let embedder = lingmiao_memory::default_embedder().expect("load embedder");
    println!("当前后端 = {}", embedder.backend());

    // 副本目录本身就是 chat zone 的 .memory 根（Zone::Chat 无子目录）。
    let mem = Memory::open_in_dir(&dir, Zone::Chat, Some(embedder)).expect("open copy");
    let recorded_before = mem.observations.recorded_embedder_backend();
    println!("记录后端(迁移前) = {recorded_before:?}");
    println!("stale(迁移前)     = {}", mem.embedder_backend_stale());

    if mem.embedder_backend_stale() {
        let t = std::time::Instant::now();
        let r = mem.rebuild_embeddings().expect("rebuild");
        println!(
            "重建完成：{} observations / {} nodes / {} turns，{:.1}s",
            r.observations,
            r.knowledge_nodes,
            r.archive_turns,
            t.elapsed().as_secs_f64()
        );
    }
    println!(
        "记录后端(迁移后) = {:?}",
        mem.observations.recorded_embedder_backend()
    );
    println!("stale(迁移后)     = {}", mem.embedder_backend_stale());
    assert!(
        !mem.embedder_backend_stale(),
        "迁移后必须不再是 stale（否则每次启动都会重灌）"
    );

    let mem = Arc::new(mem);
    let mut reg = ToolRegistry::new();
    lingmiao_tools::memory_tools::register(&mut reg, mem.clone(), None);

    let mut layer_tally: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for q in QUERIES {
        let out = futures_lite_block_on(reg.execute(
            "search_memory",
            serde_json::json!({"keyword": q, "limit": 8}),
        ));
        line('-');
        println!("查询 {q:?}");
        match out {
            Ok(o) if !o.is_error => match serde_json::from_str::<Value>(&o.content) {
                Ok(v) => {
                    let results = v["results"].as_array().cloned().unwrap_or_default();
                    for h in &results {
                        let layer = h["layer"].as_str().unwrap_or("?").to_string();
                        *layer_tally.entry(layer.clone()).or_insert(0) += 1;
                        println!(
                            "  {:<14} {:.4}  {}  |  {}",
                            layer,
                            h["score"].as_f64().unwrap_or(0.0),
                            h["id"].as_str().unwrap_or("?"),
                            h["title"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .take(48)
                                .collect::<String>(),
                        );
                    }
                    // 「turn 转写回声」——正文就是另一个回合 id 的观测，是旧口径下
                    // 刷屏 top-N 的主要噪声形状，统计它在 top-8 里占几条。
                    let echoes = results
                        .iter()
                        .filter(|h| {
                            h["title"]
                                .as_str()
                                .is_some_and(|t| t.starts_with("turn-") || t.starts_with("obs-"))
                        })
                        .count();
                    if echoes > 0 {
                        println!("  ↳ top-8 内转写回声 {echoes} 条（旧 MiniLM 口径曾占满 top-10）");
                    }
                }
                Err(_) => println!("  (非 JSON) {}", o.content),
            },
            Ok(o) => println!("  [error] {}", o.content),
            Err(e) => println!("  [tool error] {e}"),
        }
    }

    line('=');
    println!("六查询 × top-8 的层分布：");
    for (layer, n) in &layer_tally {
        println!("  {layer:<14} {n}");
    }
    line('=');
    println!("完成");
}

/// 最小 block_on —— 探针不想为一条 async 调用引入 tokio 依赖。
fn futures_lite_block_on<F: std::future::Future>(f: F) -> F::Output {
    // lingmiao-tools 依赖 tokio（runtime feature 由测试用），这里自建一个
    // current-thread runtime，避免依赖 futures-lite。
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}
