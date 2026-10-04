//! 换模型（MiniLM → multilingual-e5-small）后的**向量空间迁移**黑盒探针。
//!
//! 在**只读副本**上跑生产路径（`default_embedder()` → 内嵌解压 / 本地加载），
//! 观察 `open_default_zone` 记录的「旧后端 → 一次性 rebuild → 新后端」，再用中文
//! 查询验证召回质量（旧英文模型对短中文串会坍缩成同一向量）。
//!
//! 跑法：`LINGMIAO_MEMORY=<副本目录> cargo run -p lingmiao-tools --example e5_migrate_probe`

use lingmiao_core::Paths;
use lingmiao_memory::{Memory, Zone, default_embedder};

const QUERIES: [&str; 6] = [
    "记忆合并",
    "编排框架",
    "层配额",
    "并发",
    "你好",
    "单二进制分发",
];

fn line(c: char) {
    println!("{}", c.to_string().repeat(78));
}

fn main() {
    let root = std::env::var("LINGMIAO_MEMORY").unwrap_or_else(|_| ".memory".to_string());
    let paths = Paths::at(std::path::Path::new(&root));

    let embedder = default_embedder().expect("load production embedder");
    println!("embedder backend = {}", embedder.backend());

    let mem = Memory::open(&paths, Zone::Chat, Some(embedder)).expect("open chat zone");
    println!(
        "迁移前 recorded backend = {:?} / stale = {}",
        mem.observations.recorded_embedder_backend(),
        mem.embedder_backend_stale()
    );

    if mem.embedder_backend_stale() {
        let t = std::time::Instant::now();
        let rep = mem.rebuild_embeddings().expect("rebuild");
        println!(
            "已重建：observations {} / nodes {} / turns {}（{:.1}s）",
            rep.observations,
            rep.knowledge_nodes,
            rep.archive_turns,
            t.elapsed().as_secs_f64()
        );
    }
    println!(
        "迁移后 recorded backend = {:?} / stale = {}",
        mem.observations.recorded_embedder_backend(),
        mem.embedder_backend_stale()
    );

    line('=');
    println!("新向量空间下的中文语义召回 top-5");
    line('=');
    for q in QUERIES {
        line('-');
        println!("查询 {q:?}");
        if let Ok(rows) = mem.observations.search_semantic(q, 30) {
            println!("  #2 observations（{} 命中）", rows.len());
            for s in rows.iter().take(5) {
                println!(
                    "     {:.4}  {:<14} {:<10} {}",
                    s.score,
                    s.observation.id,
                    s.observation.kind,
                    s.observation
                        .content
                        .chars()
                        .take(40)
                        .collect::<String>()
                        .replace('\n', " ")
                );
            }
        }
        if let Ok(rows) = mem.knowledge.search_semantic(q, 30) {
            println!("  #3 knowledge（{} 命中）", rows.len());
            for (n, s) in rows.iter().take(3) {
                println!(
                    "     {:.4}  {:<14} {:<10} {}",
                    s,
                    n.id,
                    n.kind,
                    n.name.chars().take(34).collect::<String>()
                );
            }
        }
    }
}
