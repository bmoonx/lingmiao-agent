//! 记忆召回的**黑盒探针** —— 真实驱动记忆召回的全部函数，观察实际行为。
//!
//! 覆盖三块（cli 2026-09-30 要求「测试记忆召回的 7 个函数，两种方式各 2，
//! 3 库，外加合并函数」）：
//!
//! 1. **底层召回 6 路**（`lingmiao-memory`，只读副本上真实执行；两种方式各
//!    2、3 库）：
//!    - #2 observations：`search`（关键词）+ `search_semantic`（语义）
//!    - #3 knowledge  ：`search_keyword`（关键词）+ `search_semantic`（语义）
//!    - #1 archive    ：`search`（关键词）+ `search_semantic`（语义）
//! 2. **工具层 7 个函数**（`lingmiao-tools`，经真实 `ToolRegistry::execute`）：
//!    `search_memory` / `search_observations` / `search_knowledge` /
//!    `search_archive` / `list_observations` / `list_archive` /
//!    `search_external_memory`
//! 3. **合并函数**（`collect_hits` → `dedup_hits` → `rank_and_truncate`，经
//!    `search_memory` 间接观察；这是跨层合并去重取 top-N 的唯一入口）。
//!
//! 真实 embedder（fastembed all-MiniLM-L6-v2）+ 真实库（**只读副本**，绝不
//! 触碰活跃 `.memory/`，避免与正在运行的会话争写）。
//!
//! 跑法：
//! `cargo run -p lingmiao-tools --example recall_probe [库目录] [外部项目目录]`

use std::sync::Arc;

use lingmiao_memory::{Embedder, FastEmbedder, Memory, Zone};
use lingmiao_tools::{ToolRegistry, memory_tools};

fn line(c: char) {
    println!("{}", c.to_string().repeat(76));
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".cache/lingmiao/tmp/recall_probe_memory".to_string());
    let external_dir = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "xuanchuan".to_string());

    // ── 真实语义 embedder（与生产同一向量空间） ──────────────────
    let embedder: Arc<dyn Embedder> = match FastEmbedder::new() {
        Ok(e) => Arc::new(e),
        Err(err) => {
            eprintln!("fastembed 加载失败：{err}");
            std::process::exit(1);
        }
    };
    println!("embedder backend = {}", embedder.backend());

    let mem = match Memory::open_in_dir(std::path::Path::new(&dir), Zone::Chat, Some(embedder)) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            eprintln!("打开库 {dir} 失败：{e}");
            std::process::exit(1);
        }
    };

    let query = "记忆合并";
    println!("库目录 = {dir}");
    println!("查询词 = {query:?}");

    // ══ 1. 底层召回 6 路（两种方式 × 3 库） ══════════════════════
    line('=');
    println!("[1] 底层召回 —— 两种方式各 2，3 库");
    line('=');

    // #2 observations
    let mut ok_checks = 0usize;
    let mut total_checks = 0usize;

    total_checks += 1;
    match mem.observations.search(query, 30, "") {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#2 observations · 关键词 search        → {} 命中",
                rows.len()
            );
            for r in rows.iter().take(3) {
                println!(
                    "     {:<10} score={:.4}  {}",
                    r.observation.id, r.score, r.observation.name
                );
            }
        }
        Err(e) => println!("#2 observations · 关键词 search        → ERR {e}"),
    }

    total_checks += 1;
    match mem.observations.search_semantic(query, 30) {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#2 observations · 语义 search_semantic → {} 命中",
                rows.len()
            );
            for r in rows.iter().take(3) {
                println!(
                    "     {:<10} cos={:.4}  {}",
                    r.observation.id, r.score, r.observation.name
                );
            }
        }
        Err(e) => println!("#2 observations · 语义 search_semantic → ERR {e}"),
    }

    // #3 knowledge
    total_checks += 1;
    match mem.knowledge.search_keyword(query, 10, "") {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#3 knowledge    · 关键词 search_keyword → {} 命中",
                rows.len()
            );
            for n in rows.iter().take(3) {
                println!("     {:<10} {}", n.id, n.name);
            }
        }
        Err(e) => println!("#3 knowledge    · 关键词 search_keyword → ERR {e}"),
    }

    total_checks += 1;
    match mem.knowledge.search_semantic(query, 30) {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#3 knowledge    · 语义 search_semantic → {} 命中",
                rows.len()
            );
            for (n, s) in rows.iter().take(3) {
                println!("     {:<10} cos={:.4}  {}", n.id, s, n.name);
            }
        }
        Err(e) => println!("#3 knowledge    · 语义 search_semantic → ERR {e}"),
    }

    // #1 archive
    total_checks += 1;
    match mem.archive.search(query, 10) {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#1 archive      · 关键词 search         → {} 命中",
                rows.len()
            );
            for t in rows.iter().take(3) {
                println!(
                    "     {:<14} {}",
                    t.id,
                    t.user_msg.chars().take(30).collect::<String>()
                );
            }
        }
        Err(e) => println!("#1 archive      · 关键词 search         → ERR {e}"),
    }

    total_checks += 1;
    match mem.archive.search_semantic(query, 30) {
        Ok(rows) => {
            ok_checks += 1;
            println!(
                "#1 archive      · 语义 search_semantic → {} 命中",
                rows.len()
            );
            for r in rows.iter().take(3) {
                println!(
                    "     {:<14} cos={:.4}  {}",
                    r.turn.id,
                    r.score,
                    r.turn.user_msg.chars().take(24).collect::<String>()
                );
            }
        }
        Err(e) => println!("#1 archive      · 语义 search_semantic → ERR {e}"),
    }
    println!("→ 底层 6 路：{ok_checks}/{total_checks} 成功");

    // ══ 2. 工具层 7 个函数（经真实 registry） ════════════════════
    line('=');
    println!("[2] 工具层 7 个函数");
    line('=');

    let mut reg = ToolRegistry::new();
    memory_tools::register(&mut reg, mem.clone(), None); // None = 无 LLM 精排，按分排序

    let calls: [(&str, serde_json::Value); 7] = [
        ("search_memory", serde_json::json!({"keyword": query})),
        (
            "search_observations",
            serde_json::json!({"keyword": "记忆"}),
        ),
        ("search_knowledge", serde_json::json!({"keyword": "记忆"})),
        ("search_archive", serde_json::json!({"keyword": "记忆"})),
        ("list_observations", serde_json::json!({"limit": 3})),
        ("list_archive", serde_json::json!({"limit": 3})),
        (
            "search_external_memory",
            serde_json::json!({"project_dir": external_dir, "keyword": "记忆"}),
        ),
    ];

    let mut tool_ok = 0usize;
    for (name, args) in calls {
        match reg.execute(name, args).await {
            Ok(out) => {
                tool_ok += 1;
                let flag = if out.is_error { "ERR" } else { "ok" };
                println!(
                    "{:<22} → {flag}  {} 字符  | {}",
                    name,
                    out.content.chars().count(),
                    out.content
                        .replace('\n', " ")
                        .chars()
                        .take(90)
                        .collect::<String>()
                );
            }
            Err(e) => println!("{:<22} → TOOL ERR {e}", name),
        }
    }
    println!("→ 工具层：{tool_ok}/7 成功");

    // ══ 3. 合并函数（collect_hits → dedup_hits → rank_and_truncate）═
    line('=');
    println!("[3] 合并函数 —— 跨层合并/去重/top-N（经 search_memory 观察）");
    line('=');

    for term in ["记忆合并", "编排框架"] {
        match reg
            .execute(
                "search_memory",
                serde_json::json!({"keyword": term, "limit": 10}),
            )
            .await
        {
            Ok(out) => {
                let v: serde_json::Value =
                    serde_json::from_str(&out.content).unwrap_or(serde_json::Value::Null);
                let results = v.get("results").and_then(|r| r.as_array());
                match results {
                    Some(rows) => {
                        let mut layers: std::collections::BTreeMap<String, usize> =
                            std::collections::BTreeMap::new();
                        for r in rows {
                            let l = r.get("layer").and_then(|x| x.as_str()).unwrap_or("?");
                            *layers.entry(l.to_string()).or_default() += 1;
                        }
                        println!(
                            "合并查询 {term:?} → {} 条 top-N，layer 分布 {:?}",
                            rows.len(),
                            layers
                        );
                        // 去重校验：id 不重复
                        let ids: Vec<&str> = rows
                            .iter()
                            .filter_map(|r| r.get("id").and_then(|x| x.as_str()))
                            .collect();
                        let uniq: std::collections::BTreeSet<&&str> = ids.iter().collect();
                        println!(
                            "   去重校验：{} 条 / 唯一 {} 条 {}",
                            ids.len(),
                            uniq.len(),
                            if ids.len() == uniq.len() {
                                "✅"
                            } else {
                                "❌ 有重复"
                            }
                        );
                        for (i, r) in rows.iter().enumerate() {
                            println!(
                                "   {:>2}. {:<13} {:<16} score={:.4}  {}",
                                i + 1,
                                r.get("layer").and_then(|x| x.as_str()).unwrap_or("?"),
                                r.get("id").and_then(|x| x.as_str()).unwrap_or("?"),
                                r.get("score").and_then(|x| x.as_f64()).unwrap_or(0.0),
                                r.get("title")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(34)
                                    .collect::<String>()
                            );
                        }
                    }
                    None => println!("合并查询 {term:?} → 无 results 字段：{}", out.content),
                }
            }
            Err(e) => println!("合并查询 {term:?} → TOOL ERR {e}"),
        }
    }

    line('=');
    println!("[4] 合并函数（obs_ids 合并 / 跨层连接）—— 副本库上真实写入");
    line('=');

    // D 项 2026-10-05 新功能：`update_knowledge` 把本回合 `update_memory` 写的
    // 观测挂到节点 `obs_ids`，**合并而非覆盖**（`knowledge::merge_id_lists`）。
    // 副本库可写，不触碰活跃 `.memory/`。
    let tag = format!("probe-{}", std::process::id());
    let mut real_ids: Vec<String> = Vec::new();
    for suffix in ["a", "b"] {
        match reg
            .execute(
                "update_memory",
                serde_json::json!({"kind":"fact","name":format!("{tag}-{suffix}"),"content":"探针观测"}),
            )
            .await
        {
            Ok(o) => {
                let id = o.content.trim().rsplit(' ').next().unwrap_or("").to_string();
                if id.starts_with("obs-") {
                    real_ids.push(id);
                }
            }
            Err(e) => println!("update_memory ERR {e}"),
        }
    }
    println!(
        "update_memory ×2        → 写入观测 {} 条 {:?}",
        real_ids.len(),
        real_ids
    );
    let node1 = reg
        .execute(
            "update_knowledge",
            serde_json::json!({"kind":"fact","name":format!("{tag}-node"),"summary":"探针节点","content":"内容 v1"}),
        )
        .await;
    println!(
        "update_knowledge (v1)   → err={}",
        node1.map(|o| o.is_error).unwrap_or(true)
    );
    // 第二次 upsert 同名节点：obs_ids 应为**合并**结果（不丢第一轮挂上的 id）。
    let node2 = reg
        .execute(
            "update_knowledge",
            serde_json::json!({"kind":"fact","name":format!("{tag}-node"),"summary":"探针节点","content":"内容 v2"}),
        )
        .await;
    println!(
        "update_knowledge (v2 同名) → err={}",
        node2.map(|o| o.is_error).unwrap_or(true)
    );

    let found = mem
        .knowledge
        .search_keyword(&format!("{tag}-node"), 5, "")
        .map(|rows| rows.into_iter().find(|n| n.name == format!("{tag}-node")));
    let node_id = match &found {
        Ok(Some(n)) => {
            let ids: Vec<String> = serde_json::from_str(&n.obs_ids).unwrap_or_default();
            println!(
                "节点 {:<18} obs_ids = {} 条 {}",
                n.name,
                ids.len(),
                if ids.len() == 2 {
                    "✅ 合并保留（未覆盖）"
                } else {
                    "❌ 被覆盖"
                }
            );
            n.id.clone()
        }
        Ok(None) => {
            println!("节点未找到（探针写入失败）");
            String::new()
        }
        Err(e) => {
            println!("查节点 ERR {e}");
            String::new()
        }
    };

    // link_entries：跨层连接（观测 ↔ 知识节点），用真实 id，建完后回读一跳邻居。
    if !real_ids.is_empty() && !node_id.is_empty() {
        let link = reg
            .execute(
                "link_entries",
                serde_json::json!({
                    "from": format!("observations:{}", real_ids[0]),
                    "to": [format!("knowledge:{node_id}")],
                    "relation": "probe"
                }),
            )
            .await;
        match link {
            Ok(o) => println!(
                "link_entries            → {}  {}",
                if o.is_error { "ERR" } else { "ok" },
                o.content
                    .replace('\n', " ")
                    .chars()
                    .take(90)
                    .collect::<String>()
            ),
            Err(e) => println!("link_entries → TOOL ERR {e}"),
        }
        match mem.knowledge.links_touching("knowledge", &node_id) {
            Ok(links) => println!(
                "回读 links_touching     → {} 条连接 {}",
                links.len(),
                if links.is_empty() {
                    "❌"
                } else {
                    "✅ 两端可见"
                }
            ),
            Err(e) => println!("links_touching ERR {e}"),
        }
    } else {
        println!("link_entries            → 跳过（真实 id 不可得）");
    }

    line('=');
    println!("探针结束：底层 {ok_checks}/{total_checks} · 工具层 {tool_ok}/7");
}
