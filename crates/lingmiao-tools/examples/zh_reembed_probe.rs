//! 在**只读副本**上用中文 embedder（bge-small-zh-v1.5）重建全部向量，再跑
//! 真实召回，量化「语义召回退化」到底是不是换模型的解药。
//!
//! 跑法：`cargo run -p lingmiao-tools --example zh_reembed_probe [副本库目录]`

use std::sync::Arc;

use lingmiao_memory::{Embedder, FastEmbedder, Memory, Zone};

const ZH_DIR: &str = "/root/.cache/lingmiao-models/bge-small-zh-v1.5-onnx";
const QUERIES: [&str; 6] = [
    "记忆合并",
    "编排框架",
    "层配额",
    "并发",
    "你好",
    "GitHub 改名",
];

fn line(c: char) {
    println!("{}", c.to_string().repeat(78));
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".cache/lingmiao/tmp/recall_probe_zh".to_string());

    let zh: Arc<dyn Embedder> =
        Arc::new(FastEmbedder::from_dir(std::path::Path::new(ZH_DIR)).expect("bge-small-zh"));
    println!("embedder = {}", zh.backend());

    let mem =
        Memory::open_in_dir(std::path::Path::new(&dir), Zone::Chat, Some(zh)).expect("open copy");
    // ① 先看清副本记录的后端（= MiniLM，本地库写的），
    // ② 重建 → 记录刷新为中文模型。
    println!(
        "重建前 recorded backend = {:?}",
        mem.observations.recorded_embedder_backend()
    );
    let rep = mem.rebuild_embeddings().expect("rebuild");
    println!(
        "已重建：observations {} / nodes {} / turns {}",
        rep.observations, rep.knowledge_nodes, rep.archive_turns
    );
    println!(
        "重建后 recorded backend = {:?}",
        mem.observations.recorded_embedder_backend()
    );

    line('=');
    println!("各层语义召回 top-6（中文模型向量 + 中文查询）");
    line('=');
    for q in QUERIES {
        line('-');
        println!("查询 {q:?}");
        match mem.observations.search_semantic(q, 30) {
            Ok(rows) => {
                println!("  #2 observations（{} 命中，展示 top-6）", rows.len());
                for s in rows.iter().take(6) {
                    println!(
                        "     {:.4}  {:<14} {:<8} {}",
                        s.score,
                        s.observation.id,
                        s.observation.kind,
                        s.observation
                            .content
                            .chars()
                            .take(44)
                            .collect::<String>()
                            .replace('\n', " ")
                    );
                }
            }
            Err(e) => println!("  obs ERR {e}"),
        }
        if let Ok(rows) = mem.knowledge.search_semantic(q, 30) {
            println!("  #3 knowledge（{} 命中，展示 top-4）", rows.len());
            for (n, s) in rows.iter().take(4) {
                println!(
                    "     {:.4}  {:<14} {:<10} {}",
                    s,
                    n.id,
                    n.kind,
                    n.name.chars().take(40).collect::<String>()
                );
            }
        }
        if let Ok(rows) = mem.archive.search_semantic(q, 30) {
            println!("  #1 archive（{} 命中，展示 top-3）", rows.len());
            for s in rows.iter().take(3) {
                println!(
                    "     {:.4}  {:<14} {}",
                    s.score,
                    s.turn.id,
                    s.turn.user_msg.chars().take(40).collect::<String>()
                );
            }
        }
    }
}
